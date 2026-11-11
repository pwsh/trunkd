// SPDX-License-Identifier: GPL-3.0-or-later
//! The single uplink worker thread: drain the queue, POST, spool, retry.
//!
//! # Staying live while Keyup is slow or down
//!
//! One POST can take up to [`UplinkConfig::timeout`], and the worker is a
//! single thread. Three things keep a slow or unreachable Keyup from costing
//! calls:
//!
//! * **The circuit.** A live call that fails with a transport error, a
//!   timeout, a 408 or a 5xx opens it; while it is open, live calls go
//!   straight to the spool without a request, so they no longer wait behind a
//!   timeout each. The next drain pass is the probe: any call Keyup accepts
//!   closes it again, and live calls go direct once more.
//! * **The overflow spool.** A call the submit queue has no room for is
//!   spooled on the submitting thread ([`Overflow`]) instead of being dropped.
//! * **No head-of-line blocking.** The drain moves past a spooled call that
//!   fails while Keyup is accepting others, retries it at the back of later
//!   passes, and after [`QUARANTINE_AFTER`] such failures with the same answer
//!   moves it to `<spool>/quarantine/` ([`Spool::quarantine`]).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use trunkd_call::{encode_audio, AudioFormat, FinishedCall};
use trunkd_core::chan::BoundedReceiver;
use trunkd_core::{Metric, Metrics};

use crate::spool::{call_from_sidecar, Spool, SpoolEntry};
use crate::UplinkConfig;

/// Longest the worker waits on an empty queue before looking at its timers.
///
/// The worker owes the spool a drain pass on a timer, so it cannot block on
/// `recv` forever; it waits with a timeout instead, which still wakes the
/// moment a call arrives. It used to nap 25 ms between polls (~40 wakeups a
/// second while idle); the wait is now also cut short by the next drain
/// (review 2026-09-22 §3.4, T-C3).
const IDLE_POLL: Duration = Duration::from_millis(500);

/// Connect timeout, separate from (and never longer than) the whole-request
/// [`UplinkConfig::timeout`]: an unreachable host is known within seconds,
/// while a large call over a slow link still gets the full budget to upload.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

/// A spooled call is quarantined after this many failed drain attempts with
/// the same answer, each made while Keyup was accepting other calls.
pub(crate) const QUARANTINE_AFTER: u32 = 5;

/// A drain pass that is getting calls through still gives up after this many
/// failures in a row: past that the endpoint itself is the likelier problem.
const DRAIN_MAX_CONSECUTIVE_FAILURES: u32 = 3;

/// Every counter the uplink publishes, resolved once at startup.
#[derive(Clone)]
pub(crate) struct UplinkMetrics {
    pub uploaded: Metric,
    pub duplicates: Metric,
    pub failed: Metric,
    pub spooled: Metric,
    pub spool_bytes: Metric,
    pub spool_evicted: Metric,
    pub disk_free_bytes: Metric,
    pub queue_dropped: Metric,
    pub spooled_on_pressure: Metric,
    pub quarantined: Metric,
    pub circuit_open: Metric,
}

impl UplinkMetrics {
    pub(crate) fn new(metrics: &Metrics) -> Self {
        UplinkMetrics {
            uploaded: metrics.counter("uplink.uploaded"),
            duplicates: metrics.counter("uplink.duplicates"),
            failed: metrics.counter("uplink.failed"),
            spooled: metrics.counter("uplink.spooled"),
            spool_bytes: metrics.gauge("uplink.spool_bytes"),
            spool_evicted: metrics.counter("uplink.spool_evicted"),
            disk_free_bytes: metrics.gauge("uplink.disk_free_bytes"),
            queue_dropped: metrics.counter("uplink.queue_dropped"),
            spooled_on_pressure: metrics.counter("uplink.spooled_on_pressure"),
            quarantined: metrics.counter("uplink.spool_quarantined"),
            circuit_open: metrics.gauge("uplink.circuit_open"),
        }
    }
}

/// What the server said about one call.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Attempt {
    /// Stored. `Call imported successfully.`
    Uploaded,
    /// Already had it. `duplicate call rejected` — a success, not an error.
    Duplicate,
    /// One of Keyup's documented refusals of the call itself
    /// (`incomplete call data: …`, a backlog push's `no_call_time`): the same
    /// bytes will never be accepted. Logged and dropped — the only failure
    /// that deletes a call.
    Permanent(String),
    /// `invalid key` / HTTP 401 / 403. A configuration error; the call is
    /// spooled and the uplink backs off rather than hammering the endpoint.
    Auth(String),
    /// Transport failure, timeout, 408 or 5xx: Keyup is down, hung or
    /// overloaded. Spool and retry; a live one opens the circuit.
    Retry(String),
    /// Any other answer — a 404/405 from a proxy mid-deploy, 409, 413, 422, a
    /// 3xx — whose meaning for *this* call nobody documented. Not known to be
    /// permanent, so it is spooled and retried like [`Attempt::Retry`]; a
    /// spooled call that keeps getting it while others go through is
    /// quarantined, not deleted.
    Rejected(String),
    /// Keyup's flow control said no for now (`429` queue full or `507` disk
    /// full) and named a `Retry-After`. Spool, and pause uploads that long.
    Throttled(Duration),
    /// Uploads are held (`spool_only`). Not an answer from anyone: the call
    /// was spooled (live) or left where it was (drain) without a request.
    Held,
}

/// Bounds for a server-supplied `Retry-After`; used when it is absent or silly.
const THROTTLE_MIN: Duration = Duration::from_secs(5);
const THROTTLE_MAX: Duration = Duration::from_secs(600);
const THROTTLE_DEFAULT: Duration = Duration::from_secs(30);

/// Map an HTTP status and body onto an [`Attempt`].
#[cfg(test)]
pub(crate) fn classify(status: u16, body: &str) -> Attempt {
    classify_with_retry_after(status, body, None)
}

/// The one place an answer from the call-upload endpoint becomes a decision.
///
/// Keyup answers with literal bodies (`app.api.call_upload`, docs/RECORDERS.md),
/// and only the answers it documents as final may cost a call:
///
/// | status | body | [`Attempt`] | the call |
/// |---|---|---|---|
/// | 429, 507 | any | `Throttled` (`Retry-After`, clamped 5–600 s) | spooled, uploads pause |
/// | 401, 403 | any | `Auth` | spooled, uploads back off |
/// | any | `invalid key` | `Auth` | spooled, uploads back off |
/// | 200, 400 | `incomplete call data: …` | `Permanent` | **deleted** |
/// | 400 | JSON `"error": "no_call_time"` | `Permanent` | **deleted** |
/// | 2xx | `duplicate call rejected` | `Duplicate` | deleted (Keyup has it) |
/// | 2xx | anything else | `Uploaded` | deleted (Keyup has it) |
/// | 408, 5xx | any | `Retry` | spooled; a live one opens the circuit |
/// | anything else (other 4xx, 3xx) | any | `Rejected` | spooled; quarantined if it keeps failing alone |
///
/// Transport errors and timeouts never reach this function; they are `Retry`.
pub(crate) fn classify_with_retry_after(
    status: u16,
    body: &str,
    retry_after_secs: Option<u64>,
) -> Attempt {
    let b = body.trim();
    if status == 429 || status == 507 {
        let wait = retry_after_secs
            .map(Duration::from_secs)
            .unwrap_or(THROTTLE_DEFAULT)
            .clamp(THROTTLE_MIN, THROTTLE_MAX);
        return Attempt::Throttled(wait);
    }
    if status == 401 || status == 403 || b.eq_ignore_ascii_case("invalid key") {
        return Attempt::Auth(b.to_owned());
    }
    // Sent with 200 by the sdrtrunk-probe path and 400 for a missing audio
    // part; either way the call as posted will never be accepted. Only
    // Keyup's own statuses: the same words behind a proxy's 404 are not its.
    if matches!(status, 200 | 400)
        && (b.starts_with("incomplete call data") || json_error(b) == Some("no_call_time"))
    {
        return Attempt::Permanent(b.to_owned());
    }
    match status {
        200..=299 if b.contains("duplicate call rejected") => Attempt::Duplicate,
        200..=299 => Attempt::Uploaded,
        408 | 500..=599 => Attempt::Retry(format!("HTTP {status}: {b}")),
        _ => Attempt::Rejected(format!("HTTP {status}: {b}")),
    }
}

/// The `"error"` of a JSON refusal body, when it is one.
fn json_error(body: &str) -> Option<&'static str> {
    if !body.starts_with('{') {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    match v.get("error")?.as_str()? {
        "no_call_time" => Some("no_call_time"),
        _ => None,
    }
}

/// What "the same answer" means when counting a spooled call's failures: the
/// status for an HTTP answer, one bucket for every transport failure (whose
/// wording varies from one timeout to the next).
fn answer_signature(msg: &str) -> &str {
    match msg.strip_prefix("HTTP ") {
        Some(rest) => rest.split(':').next().unwrap_or(rest),
        None => "transport",
    }
}

/// A spooled call's failed drain attempts, remembered by the worker.
///
/// In memory only: a restart gives every entry a fresh count, which costs a
/// poisoned entry at most [`QUARANTINE_AFTER`] more attempts.
#[derive(Debug)]
struct EntryFailures {
    /// [`answer_signature`] of the last failure.
    signature: String,
    /// Failures with that answer while Keyup was accepting other calls.
    count: u32,
}

/// Spools a call the submit queue has no room for, on the submitting thread.
///
/// The queue is deliberately small (it holds decoded PCM), and a worker stuck
/// in one slow POST lets it fill. Dropping the overflow was the old answer;
/// the spool is durable and bounded, so the call goes there instead and the
/// next drain uploads it. Costs the submitting thread one encode and two file
/// writes, only while the worker is behind. The cap and the floor are enforced
/// by the worker's next sweep, not here: a sweep walks the whole spool.
///
/// An overflowed call skips its live Broadcastify attempt (the drain never
/// makes one); Broadcastify is a live feed and the call is no longer live.
#[derive(Clone)]
pub(crate) struct Overflow {
    pub spool: Spool,
    pub format: AudioFormat,
    pub metrics: UplinkMetrics,
}

impl Overflow {
    /// Spool `call`; false when it could not be written (and is lost).
    pub(crate) fn store(&self, call: &FinishedCall, why: &str) -> bool {
        let audio = if call.pcm.is_empty() {
            None
        } else {
            match encode_audio(&call.pcm, self.format) {
                Ok((bytes, _, _)) => Some(bytes),
                Err(e) => {
                    tracing::error!(call = %call.audio_stem(), error = %e, "audio encode failed");
                    self.metrics.failed.inc();
                    None
                }
            }
        };
        match self
            .spool
            .store(call, audio.as_deref(), self.format.extension())
        {
            Ok(bytes) => {
                self.metrics.spooled.inc();
                self.metrics.spooled_on_pressure.inc();
                tracing::warn!(call = %call.audio_stem(), bytes, reason = why, "call spooled for retry");
                true
            }
            Err(e) => {
                self.metrics.failed.inc();
                self.metrics.queue_dropped.inc();
                tracing::error!(call = %call.audio_stem(), error = %e, "could not spool an overflowing call; it is lost");
                false
            }
        }
    }
}

pub(crate) struct Worker {
    cfg: UplinkConfig,
    agent: ureq::Agent,
    /// Direct-to-Broadcastify delivery, run for a freshly finished call just
    /// before it is POSTed to Keyup. Built for every run; it decides nothing
    /// until keyup pushes a block.
    broadcastify: crate::broadcastify::BroadcastifyUploader,
    spool: Spool,
    metrics: UplinkMetrics,
    /// Set while an `invalid key` backoff is in force.
    auth_backoff_until: Option<Instant>,
    /// Set while Keyup's flow control has asked us to wait (`429`/`507`).
    throttle_until: Option<Instant>,
    /// The key error is logged once per backoff window, not once per call.
    auth_logged: bool,
    /// True once the producer side is gone; the loop finishes and exits.
    disconnected: bool,
    /// Open while Keyup is failing: live calls go straight to the spool.
    circuit_open: bool,
    /// Calls Keyup has accepted (stored or duplicate) since start, live or
    /// drained — the "Keyup is taking calls" evidence a quarantine needs.
    accepted: u64,
    /// [`Worker::accepted`] at the end of the previous drain pass.
    accepted_at_last_pass: u64,
    /// Spooled calls whose last drain attempt failed, by sidecar path. They
    /// are retried after every other entry.
    failures: HashMap<PathBuf, EntryFailures>,
}

impl Worker {
    pub(crate) fn new(cfg: UplinkConfig, spool: Spool, metrics: UplinkMetrics) -> Self {
        let broadcastify = crate::broadcastify::BroadcastifyUploader {
            state: cfg.broadcastify.clone(),
            http: Box::new(crate::broadcastify::Agents::new()),
            instance_id: cfg.instance_id.clone(),
            keyup_token: cfg.api_key.clone(),
            keyup_url: cfg.url.clone(),
            format: cfg.audio_format,
        };
        let agent = ureq::Agent::new_with_config(
            ureq::Agent::config_builder()
                .timeout_global(Some(cfg.timeout))
                .timeout_connect(Some(CONNECT_TIMEOUT.min(cfg.timeout)))
                // The contract is carried in the body of a 401 and a 4xx, so
                // statuses must arrive as responses, not as errors.
                .http_status_as_error(false)
                .user_agent("trunkd")
                .build(),
        );
        Worker {
            cfg,
            agent,
            broadcastify,
            spool,
            metrics,
            throttle_until: None,
            auth_backoff_until: None,
            auth_logged: false,
            disconnected: false,
            circuit_open: false,
            accepted: 0,
            accepted_at_last_pass: 0,
            failures: HashMap::new(),
        }
    }

    /// Run until the queue's sender is dropped.
    pub(crate) fn run(mut self, rx: BoundedReceiver<FinishedCall>) {
        self.sweep_spool();
        let mut next_drain = Instant::now() + self.cfg.drain_interval;
        loop {
            if Instant::now() >= next_drain {
                self.drain(&rx);
                // On the timer, not only after a store: the volume can fill up
                // from something that is not this process, and a spool that is
                // never written to would otherwise never be re-examined.
                self.sweep_spool();
                next_drain = Instant::now() + self.cfg.drain_interval;
            }
            let wait = next_drain
                .saturating_duration_since(Instant::now())
                .min(IDLE_POLL);
            match rx.recv_wait(wait) {
                Ok(call) => self.handle(call),
                Err(trunkd_core::chan::RecvTimeoutError::Timeout) => {
                    if self.disconnected {
                        break;
                    }
                }
                Err(trunkd_core::chan::RecvTimeoutError::Disconnected) => break,
            }
        }
        tracing::debug!("uplink worker stopped");
    }

    /// Encode, offer to Broadcastify, upload to Keyup, and spool one freshly
    /// finished call.
    fn handle(&mut self, mut call: FinishedCall) {
        let format = self.cfg.audio_format;
        let audio = if call.pcm.is_empty() {
            // Encrypted (metadata-only) calls have no audio by design.
            None
        } else {
            match encode_audio(&call.pcm, format) {
                Ok((bytes, _, _)) => Some(bytes),
                Err(e) => {
                    // Nothing to retry: the same samples would fail again. Keep
                    // the metadata, which is still worth having.
                    tracing::error!(call = %call.audio_stem(), error = %e, "audio encode failed");
                    self.metrics.failed.inc();
                    None
                }
            }
        };
        // Broadcastify first, and exactly once: the verdict rides along on the
        // Keyup push below, and is written into the sidecar if that push has to
        // be spooled — so a retry days later never re-registers the call.
        // Bounded by the module's own timeouts (~12 s worst case), on this
        // thread, never on a DSP one.
        if call.broadcastify.is_none() {
            let now_ms = trunkd_core::Timestamp::now().unix_millis();
            call.broadcastify =
                self.broadcastify
                    .attempt(&call, audio.as_deref(), self.cfg.hold.is_held(), now_ms);
        }
        self.deliver(&call, audio.as_deref(), true);
    }

    /// Upload one call, spooling it if that is the right answer.
    ///
    /// `spool_on_failure` is false when the call *came from* the spool: it is
    /// already on disk and must not be written twice.
    fn deliver(
        &mut self,
        call: &FinishedCall,
        audio: Option<&[u8]>,
        spool_on_failure: bool,
    ) -> Attempt {
        // Uploads held (`spool_only`, or a runtime hold from keyup or the
        // panel): the call goes to disk without a socket being opened. Checked
        // before every other gate, because a held uplink is not a failure and
        // must not start an auth backoff or a throttle window it would then
        // have to wait out after the hold is released.
        if self.cfg.hold.is_held() {
            if spool_on_failure {
                self.spool_call(call, audio, "uploads held");
            }
            return Attempt::Held;
        }
        if self.in_auth_backoff() {
            if spool_on_failure {
                self.spool_call(call, audio, "backing off after an invalid key");
            }
            return Attempt::Auth("backing off".to_owned());
        }
        if let Some(until) = self.throttle_until {
            let now = Instant::now();
            if until > now {
                if spool_on_failure {
                    self.spool_call(call, audio, "Keyup flow control: waiting");
                }
                return Attempt::Throttled(until - now);
            }
            self.throttle_until = None;
        }
        // Keyup is failing: a live call would only wait out another timeout
        // before landing in the spool anyway. The drain is the probe.
        if self.circuit_open && spool_on_failure {
            self.metrics.spooled_on_pressure.inc();
            self.spool_call(call, audio, "Keyup is failing; spooled without an attempt");
            return Attempt::Retry("Keyup is failing".to_owned());
        }

        let attempt = self.post(call, audio);
        match &attempt {
            Attempt::Uploaded => {
                self.accepted += 1;
                self.close_circuit();
                self.metrics.uploaded.inc();
                // Debug, as is the duplicate below: the call thread's "call
                // ended" line already records every call at info, the status
                // line carries the upload counts, and a failure warns. The
                // success lines (these two and Broadcastify's) were 18 % of the
                // laptop's log text on 2026-09-18 (review 2026-09-22 §3.5,
                // T-C3).
                tracing::debug!(
                    call = %call.audio_stem(),
                    talkgroup = call.talkgroup,
                    duration_s = call.duration_s(),
                    "call uploaded"
                );
            }
            Attempt::Duplicate => {
                self.accepted += 1;
                self.close_circuit();
                self.metrics.duplicates.inc();
                tracing::debug!(call = %call.audio_stem(), "duplicate call rejected by Keyup");
            }
            Attempt::Permanent(msg) => {
                // Keyup read the call and answered: it is up.
                self.close_circuit();
                self.metrics.failed.inc();
                tracing::error!(call = %call.audio_stem(), reason = %msg, "call rejected permanently");
            }
            Attempt::Auth(msg) => {
                self.begin_auth_backoff(msg);
                if spool_on_failure {
                    self.spool_call(call, audio, msg);
                }
            }
            Attempt::Retry(msg) => {
                if spool_on_failure {
                    // Whether a *spooled* call's failure means Keyup is down
                    // is the drain's call; it knows what else got through.
                    self.open_circuit(msg);
                    self.spool_call(call, audio, msg);
                } else {
                    tracing::warn!(call = %call.audio_stem(), reason = %msg, "spooled call still failing");
                }
            }
            Attempt::Rejected(msg) => {
                if spool_on_failure {
                    self.spool_call(call, audio, msg);
                } else {
                    tracing::warn!(call = %call.audio_stem(), reason = %msg, "spooled call refused again");
                }
            }
            Attempt::Throttled(wait) => {
                if self.throttle_until.is_none_or(|u| u <= Instant::now()) {
                    tracing::warn!(
                        wait_s = wait.as_secs(),
                        "Keyup flow control refused the upload; pausing uploads"
                    );
                }
                self.throttle_until = Some(Instant::now() + *wait);
                if spool_on_failure {
                    self.spool_call(call, audio, "Keyup flow control: queue full");
                }
            }
            Attempt::Held => {}
        }
        attempt
    }

    fn open_circuit(&mut self, reason: &str) {
        if !self.circuit_open {
            tracing::warn!(
                reason = %reason,
                "Keyup is failing; live calls go straight to the spool until a drain gets one through"
            );
            self.circuit_open = true;
            self.metrics.circuit_open.set(1);
        }
    }

    fn close_circuit(&mut self) {
        if self.circuit_open {
            tracing::info!("Keyup is answering again; live calls go direct");
            self.circuit_open = false;
            self.metrics.circuit_open.set(0);
        }
    }

    /// One POST. Transport failures are retryable by definition.
    fn post(&self, call: &FinishedCall, audio: Option<&[u8]>) -> Attempt {
        let (content_type, body) = crate::multipart::build_multipart_with_priority(
            call,
            &self.cfg.api_key,
            self.cfg.system_id,
            audio,
            self.cfg.audio_format,
            &self.cfg.priority,
        );
        match self
            .agent
            .post(&self.cfg.url)
            .header("Content-Type", &content_type)
            .send(&body[..])
        {
            Ok(mut resp) => {
                let status = resp.status().as_u16();
                let retry_after = resp
                    .headers()
                    .get("Retry-After")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.trim().parse::<u64>().ok());
                let body = resp.body_mut().read_to_string().unwrap_or_default();
                classify_with_retry_after(status, &body, retry_after)
            }
            Err(e) => Attempt::Retry(e.to_string()),
        }
    }

    fn spool_call(&mut self, call: &FinishedCall, audio: Option<&[u8]>, reason: &str) {
        match self
            .spool
            .store(call, audio, self.cfg.audio_format.extension())
        {
            Ok(bytes) => {
                self.metrics.spooled.inc();
                tracing::warn!(
                    call = %call.audio_stem(),
                    bytes,
                    reason = %reason,
                    "call spooled for retry"
                );
            }
            Err(e) => {
                self.metrics.failed.inc();
                tracing::error!(call = %call.audio_stem(), error = %e, "could not spool call");
            }
        }
        self.sweep_spool();
    }

    /// Retry spooled calls, oldest first.
    ///
    /// A pass that has got nothing through stops at the first failure — the
    /// server is still down, and walking the whole spool into a dead endpoint
    /// helps nobody; if that failure was a transport error or a 5xx the
    /// circuit opens. Once Keyup is taking calls, a failing entry no longer
    /// stops the pass: it is skipped (and retried after every other entry on
    /// later passes) until [`QUARANTINE_AFTER`] failures move it aside. Live
    /// calls arriving mid-drain are handled first: a fresh call must never
    /// wait behind a backlog.
    fn drain(&mut self, rx: &BoundedReceiver<FinishedCall>) {
        // Nothing to drain into while uploads are held; the backlog waits for
        // the release, which is exactly what the hold is for.
        if self.cfg.hold.is_held() || self.in_auth_backoff() {
            return;
        }
        let mut entries = self.spool.entries();
        let present: HashSet<&Path> = entries.iter().map(|e| e.json.as_path()).collect();
        self.failures.retain(|p, _| present.contains(p.as_path()));
        if entries.is_empty() {
            // Nothing to probe with: let the next live call be the probe.
            self.close_circuit();
            self.accepted_at_last_pass = self.accepted;
            return;
        }
        // Stable, so each group stays oldest-first: entries that have not
        // failed yet, then the ones that have.
        entries.sort_by_key(|e| self.failures.contains_key(&e.json));
        tracing::debug!(pending = entries.len(), "draining spool");

        let accepted_before = self.accepted_at_last_pass;
        let mut failed: Vec<(SpoolEntry, String)> = Vec::new();
        let mut consecutive = 0u32;
        for entry in entries {
            // Live calls take priority over the backlog.
            loop {
                match rx.try_recv() {
                    Ok(call) => self.handle(call),
                    Err(crossbeam_channel::TryRecvError::Empty) => break,
                    Err(crossbeam_channel::TryRecvError::Disconnected) => {
                        self.disconnected = true;
                        break;
                    }
                }
            }

            let Ok(text) = std::fs::read_to_string(&entry.json) else {
                // Vanished (evicted, or swept up by an operator) — nothing to do.
                continue;
            };
            let Some(call) = call_from_sidecar(&text) else {
                tracing::error!(path = %entry.json.display(), "unreadable spool sidecar; deleting");
                self.spool.remove(&entry);
                continue;
            };
            let audio = entry
                .audio
                .as_ref()
                .and_then(|p| std::fs::read(p).ok())
                .filter(|b| !b.is_empty());

            let (msg, keyup_down) = match self.deliver(&call, audio.as_deref(), false) {
                Attempt::Uploaded | Attempt::Duplicate | Attempt::Permanent(_) => {
                    self.spool.remove(&entry);
                    self.failures.remove(&entry.json);
                    consecutive = 0;
                    continue;
                }
                Attempt::Auth(_) | Attempt::Throttled(_) | Attempt::Held => break,
                Attempt::Retry(msg) => (msg, true),
                Attempt::Rejected(msg) => (msg, false),
            };
            consecutive += 1;
            let taking_calls = self.accepted > accepted_before;
            if !taking_calls && keyup_down {
                self.open_circuit(&msg);
            }
            failed.push((entry, msg));
            if !taking_calls || consecutive >= DRAIN_MAX_CONSECUTIVE_FAILURES {
                break;
            }
        }

        // Counted only when Keyup took some other call since the last pass:
        // then the refusal is about this call, not about Keyup being down.
        let taking_calls = self.accepted > accepted_before;
        for (entry, msg) in failed {
            self.note_failure(entry, &msg, taking_calls);
        }
        self.accepted_at_last_pass = self.accepted;
        self.spool.prune_empty_dirs();
    }

    /// Record one failed drain attempt for `entry`, and quarantine it once it
    /// has failed [`QUARANTINE_AFTER`] times with the same answer while Keyup
    /// was taking other calls.
    fn note_failure(&mut self, entry: SpoolEntry, msg: &str, counts: bool) {
        let signature = answer_signature(msg);
        let rec = self
            .failures
            .entry(entry.json.clone())
            .or_insert_with(|| EntryFailures {
                signature: signature.to_owned(),
                count: 0,
            });
        if rec.signature != signature {
            rec.signature = signature.to_owned();
            rec.count = 0;
        }
        if counts {
            rec.count += 1;
        }
        if rec.count < QUARANTINE_AFTER {
            return;
        }
        let attempts = rec.count;
        self.failures.remove(&entry.json);
        match self.spool.quarantine(&entry) {
            Ok(dest) => {
                self.metrics.quarantined.inc();
                tracing::warn!(
                    call = %entry.json.display(),
                    attempts,
                    reason = %msg,
                    to = %dest.display(),
                    "spooled call keeps failing while Keyup takes others; quarantined"
                );
            }
            Err(e) => tracing::error!(
                call = %entry.json.display(),
                error = %e,
                "could not quarantine a failing spooled call; it stays in the spool"
            ),
        }
    }

    fn in_auth_backoff(&mut self) -> bool {
        match self.auth_backoff_until {
            Some(until) if Instant::now() < until => true,
            Some(_) => {
                self.auth_backoff_until = None;
                self.auth_logged = false;
                false
            }
            None => false,
        }
    }

    fn begin_auth_backoff(&mut self, msg: &str) {
        if !self.auth_logged {
            tracing::error!(
                url = %self.cfg.url,
                reason = %msg,
                backoff_s = self.cfg.auth_backoff.as_secs(),
                "Keyup rejected the API key; calls are being spooled"
            );
            self.auth_logged = true;
        }
        self.auth_backoff_until = Some(Instant::now() + self.cfg.auth_backoff);
    }

    /// Enforce both storage limits and publish what they saw.
    ///
    /// Cheap enough to run after every store *and* on the drain timer: one
    /// directory walk (which the drain already does) plus one `statvfs`.
    fn sweep_spool(&self) {
        let sweep = self.spool.enforce_limits();
        if sweep.cap_evicted > 0 {
            self.metrics.spool_evicted.add(sweep.cap_evicted);
            tracing::warn!(
                evicted = sweep.cap_evicted,
                cap_bytes = self.cfg.spool_cap_bytes,
                "spool over cap; dropped the oldest calls"
            );
        }
        if sweep.floor_evicted > 0 {
            self.metrics.spool_evicted.add(sweep.floor_evicted);
            tracing::warn!(
                evicted = sweep.floor_evicted,
                free_bytes = sweep.free_bytes.unwrap_or(0),
                min_free_bytes = self.cfg.spool_min_free_bytes,
                "volume below its free-space floor; dropped the oldest calls"
            );
        }
        self.metrics.spool_bytes.set(sweep.bytes);
        // 0 is "not measured": the gauge would otherwise claim a full disk on a
        // platform that cannot answer the question.
        self.metrics
            .disk_free_bytes
            .set(sweep.free_bytes.unwrap_or(0));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    use trunkd_call::CallBuilder;
    use trunkd_core::Metrics;

    use crate::hold::{HoldSource, UplinkHold};
    use crate::{KeyupUplink, Spool, UplinkConfig};

    /// How a [`CountingServer`] answers: given the request's index (0-based)
    /// and its text (headers and multipart body, lossily decoded), a status
    /// and a body.
    type Responder = Box<dyn Fn(u64, &str) -> (u16, String) + Send>;

    /// A one-thread HTTP server that counts the call-upload requests it saw and
    /// answers every one of them with Keyup's success body (or whatever its
    /// [`Responder`] says).
    ///
    /// The point of the counter is the negative assertion: a held uplink must
    /// not merely *fail* to upload, it must never open the socket at all.
    struct CountingServer {
        url: String,
        seen: Arc<AtomicU64>,
        stop: Arc<std::sync::atomic::AtomicBool>,
    }

    impl CountingServer {
        fn start() -> CountingServer {
            Self::start_with(Box::new(|_, _| (200, "Call imported successfully.".into())))
        }

        fn start_with(respond: Responder) -> CountingServer {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
            let port = listener.local_addr().expect("addr").port();
            listener
                .set_nonblocking(true)
                .expect("the accept loop polls so it can be stopped");
            let seen = Arc::new(AtomicU64::new(0));
            let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let (s, st) = (seen.clone(), stop.clone());
            std::thread::spawn(move || {
                while !st.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((mut sock, _)) => {
                            let n = s.fetch_add(1, Ordering::Relaxed);
                            let _ = sock.set_nonblocking(false);
                            let request = read_request(&mut sock).unwrap_or_default();
                            let (status, body) = respond(n, &String::from_utf8_lossy(&request));
                            let _ = sock.write_all(
                                format!(
                                    "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                                    body.len()
                                )
                                .as_bytes(),
                            );
                            let _ = sock.flush();
                        }
                        Err(_) => std::thread::sleep(Duration::from_millis(5)),
                    }
                }
            });
            CountingServer {
                url: format!("http://127.0.0.1:{port}/api/call-upload"),
                seen,
                stop,
            }
        }

        fn seen(&self) -> u64 {
            self.seen.load(Ordering::Relaxed)
        }
    }

    impl Drop for CountingServer {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
        }
    }

    /// Consume one request, headers and declared body, so the client never sees
    /// a reply land before its upload finished. Returns all of it.
    fn read_request(sock: &mut std::net::TcpStream) -> std::io::Result<Vec<u8>> {
        let mut buf = Vec::new();
        let mut byte = [0u8; 1];
        while !buf.ends_with(b"\r\n\r\n") {
            if sock.read(&mut byte)? == 0 {
                return Ok(buf);
            }
            buf.push(byte[0]);
        }
        let head = String::from_utf8_lossy(&buf).to_ascii_lowercase();
        let len = head
            .lines()
            .find_map(|l| l.strip_prefix("content-length:"))
            .and_then(|v| v.trim().parse::<usize>().ok())
            .unwrap_or(0);
        let mut body = vec![0u8; len];
        sock.read_exact(&mut body)?;
        buf.extend_from_slice(&body);
        Ok(buf)
    }

    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let mut p = std::env::temp_dir();
            p.push(format!(
                "trunkd-hold-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).expect("temp dir");
            TempDir(p)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn sample_call() -> FinishedCall {
        let at = 1_788_012_000_123i64;
        let mut b = CallBuilder::new("Example Network", "Site 1", at, 852_137_500, 52101);
        b.talker(9_000_011, at, true, None);
        b.push_pcm(&[7i16; 8000]);
        b.finish(at + 1000)
    }

    /// Poll until `f` holds or the deadline passes; returns whether it held.
    fn wait_for(mut f: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if f() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        f()
    }

    #[test]
    fn a_held_uplink_spools_without_opening_a_socket_and_drains_on_release() {
        let server = CountingServer::start();
        let dir = TempDir::new("spools");
        let metrics = Metrics::new();
        let hold = Arc::new(UplinkHold::new(true));
        let uplink = KeyupUplink::start(
            UplinkConfig {
                url: server.url.clone(),
                spool_dir: dir.0.clone(),
                // Fast enough that the release is observed inside the test,
                // slow enough that the held pass is not a race.
                drain_interval: Duration::from_millis(100),
                audio_format: trunkd_call::AudioFormat::Wav,
                hold: hold.clone(),
                ..UplinkConfig::default()
            },
            &metrics,
        )
        .expect("uplink starts");

        uplink.submit(sample_call());
        let spool = Spool::open(dir.0.clone(), 1 << 30).expect("open");
        assert!(
            wait_for(|| spool.entries().len() == 1),
            "a held call is written to the spool"
        );
        // Give the drain timer several passes to prove it stays away.
        std::thread::sleep(Duration::from_millis(400));
        assert_eq!(server.seen(), 0, "and no request is attempted while held");
        assert_eq!(uplink.stats().uploaded, 0);

        hold.set(false, HoldSource::Panel);
        assert!(
            wait_for(|| spool.entries().is_empty()),
            "releasing the hold drains the spool"
        );
        assert!(server.seen() >= 1, "which means the socket is opened again");
        uplink.shutdown();
        assert_eq!(metrics.counter("uplink.uploaded").get(), 1);
    }

    #[test]
    fn a_drain_uploads_the_calls_and_leaves_the_event_spool_alone() {
        // The radio-event spool is `<spool>/events`; its batches are JSON
        // objects too. A drain once posted each one as a talkgroup-0 call,
        // Keyup refused it, and the batch was deleted as a permanent failure.
        let server = CountingServer::start();
        let dir = TempDir::new("drain-events");
        let events =
            crate::events::EventSpool::open(dir.0.join("events"), 1 << 30).expect("event spool");
        let body = r#"{"site":{"system":"S"},"events":[{"kind":"affiliation"}]}"#;
        events.store(body, 1_788_012_000_500, 0).expect("batch");
        events.store(body, 1_788_012_001_500, 1).expect("batch");
        let batches: Vec<_> = events.entries().into_iter().map(|e| e.path).collect();
        assert_eq!(batches.len(), 2);

        let metrics = Metrics::new();
        let hold = Arc::new(UplinkHold::new(true));
        let uplink = KeyupUplink::start(
            UplinkConfig {
                url: server.url.clone(),
                spool_dir: dir.0.clone(),
                drain_interval: Duration::from_millis(100),
                audio_format: trunkd_call::AudioFormat::Wav,
                hold: hold.clone(),
                ..UplinkConfig::default()
            },
            &metrics,
        )
        .expect("uplink starts");
        uplink.submit(sample_call());
        let spool = Spool::open(dir.0.clone(), 1 << 30).expect("open");
        assert!(wait_for(|| spool.entries().len() == 1), "the call spools");

        hold.set(false, HoldSource::Panel);
        assert!(wait_for(|| spool.entries().is_empty()), "the call drains");
        // Several more drain passes: none of them may reach the event spool.
        std::thread::sleep(Duration::from_millis(400));
        uplink.shutdown();
        assert_eq!(server.seen(), 1, "exactly one upload: the call");
        assert_eq!(metrics.counter("uplink.uploaded").get(), 1);
        assert!(
            batches.iter().all(|p| p.is_file()),
            "every event batch is still on disk"
        );
        assert_eq!(events.entries().len(), 2);
    }

    /// The variant, without the message: what the table test compares.
    fn kind(a: &Attempt) -> &'static str {
        match a {
            Attempt::Uploaded => "uploaded",
            Attempt::Duplicate => "duplicate",
            Attempt::Permanent(_) => "permanent",
            Attempt::Auth(_) => "auth",
            Attempt::Retry(_) => "retry",
            Attempt::Rejected(_) => "rejected",
            Attempt::Throttled(_) => "throttled",
            Attempt::Held => "held",
        }
    }

    #[test]
    fn only_keyups_documented_refusals_delete_a_call() {
        // (status, body, verdict). `permanent`, `uploaded` and `duplicate` are
        // the only verdicts that let a call leave the spool.
        let table: &[(u16, &str, &str)] = &[
            (200, "Call imported successfully.", "uploaded"),
            (200, "ok", "uploaded"),
            (201, "", "uploaded"),
            (200, "duplicate call rejected", "duplicate"),
            // Documented as final: the call as posted can never be accepted.
            (200, "incomplete call data: no talkgroup", "permanent"),
            (400, "incomplete call data: no audio", "permanent"),
            (400, "incomplete call data: no system", "permanent"),
            (
                400,
                "incomplete call data: a unit call has no talkgroup",
                "permanent",
            ),
            (
                400,
                r#"{"error": "no_call_time", "class": "backlog"}"#,
                "permanent",
            ),
            // Keyup's words behind a status Keyup does not use for them.
            (404, "incomplete call data: no audio", "rejected"),
            (422, "incomplete call data: no audio", "rejected"),
            // Misconfiguration: spool and back off.
            (401, "invalid key", "auth"),
            (401, "", "auth"),
            (200, "invalid key", "auth"),
            (403, "CSRF check failed", "auth"),
            // Flow control.
            (429, "queue full, retry later", "throttled"),
            (507, "insufficient storage", "throttled"),
            // Keyup down, hung or overloaded.
            (408, "", "retry"),
            (500, "import failed: OperationalError", "retry"),
            (502, "Bad Gateway", "retry"),
            (503, "down for maintenance", "retry"),
            (504, "", "retry"),
            // Undocumented: kept and retried, never deleted.
            (400, "Bad Request", "rejected"),
            (400, r#"{"error": "bad_priority"}"#, "rejected"),
            (404, "<html>Not Found</html>", "rejected"),
            (405, "Method Not Allowed", "rejected"),
            (409, "", "rejected"),
            (413, "audio file too large", "rejected"),
            (413, "<html>Request Entity Too Large</html>", "rejected"),
            (422, "Unprocessable Entity", "rejected"),
            (302, "", "rejected"),
        ];
        for &(status, body, want) in table {
            assert_eq!(
                kind(&classify(status, body)),
                want,
                "HTTP {status} {body:?}"
            );
        }
    }

    #[test]
    fn a_throttle_honours_retry_after_within_bounds() {
        assert_eq!(
            classify_with_retry_after(429, r#"{"error":"queue_full"}"#, Some(45)),
            Attempt::Throttled(Duration::from_secs(45))
        );
        assert_eq!(
            classify_with_retry_after(429, "", None),
            Attempt::Throttled(THROTTLE_DEFAULT)
        );
        assert_eq!(
            classify_with_retry_after(507, "", Some(100_000)),
            Attempt::Throttled(THROTTLE_MAX)
        );
        assert_eq!(
            classify_with_retry_after(429, "", Some(1)),
            Attempt::Throttled(THROTTLE_MIN)
        );
    }

    #[test]
    fn the_same_answer_means_the_same_status() {
        assert_eq!(answer_signature("HTTP 422: nope"), "422");
        assert_eq!(answer_signature("HTTP 422: something else"), "422");
        assert_eq!(answer_signature("timeout: global"), "transport");
        assert_eq!(answer_signature("connection refused"), "transport");
    }

    /// An uplink against `server` with a fast drain and WAV audio (no MP3
    /// encoder in the loop), not held.
    fn uplink_for(server: &CountingServer, dir: &TempDir, metrics: &Metrics) -> KeyupUplink {
        KeyupUplink::start(
            UplinkConfig {
                url: server.url.clone(),
                spool_dir: dir.0.clone(),
                drain_interval: Duration::from_millis(100),
                timeout: Duration::from_secs(5),
                audio_format: trunkd_call::AudioFormat::Wav,
                ..UplinkConfig::default()
            },
            metrics,
        )
        .expect("uplink starts")
    }

    fn call_at(at: i64, talkgroup: u32) -> FinishedCall {
        let mut b = CallBuilder::new("Example Network", "Site 1", at, 852_137_500, talkgroup);
        b.talker(9_000_011, at, false, None);
        b.push_pcm(&[7i16; 8000]);
        b.finish(at + 1000)
    }

    #[test]
    fn a_call_the_full_queue_cannot_take_is_spooled_not_dropped() {
        // Keyup hangs on the first upload; the worker is stuck in it.
        let server = CountingServer::start_with(Box::new(|n, _| {
            if n == 0 {
                std::thread::sleep(Duration::from_millis(1500));
            }
            (200, "Call imported successfully.".into())
        }));
        let dir = TempDir::new("overflow");
        let metrics = Metrics::new();
        let uplink = KeyupUplink::start(
            UplinkConfig {
                url: server.url.clone(),
                spool_dir: dir.0.clone(),
                queue_capacity: 1,
                drain_interval: Duration::from_millis(100),
                audio_format: trunkd_call::AudioFormat::Wav,
                ..UplinkConfig::default()
            },
            &metrics,
        )
        .expect("uplink starts");

        let at = 1_788_012_000_123i64;
        assert_eq!(uplink.submit(call_at(at, 52101)), crate::Submitted::Queued);
        assert!(wait_for(|| server.seen() == 1), "the worker is in the POST");
        assert_eq!(
            uplink.submit(call_at(at + 10_000, 52101)),
            crate::Submitted::Queued
        );
        // The queue (one deep) is full: this one goes to disk, right here.
        let overflow = call_at(at + 20_000, 52101);
        let stem = overflow.audio_stem();
        assert_eq!(uplink.submit(overflow), crate::Submitted::Spooled);
        let spool = Spool::open(dir.0.clone(), 1 << 30).expect("open");
        let on_disk = spool.entries();
        assert_eq!(on_disk.len(), 1);
        assert!(on_disk[0].json.to_string_lossy().contains(&stem));
        assert!(on_disk[0].audio.is_some(), "with its audio");
        let stats = uplink.stats();
        assert_eq!(stats.spooled_on_pressure, 1);
        assert_eq!(stats.queue_dropped, 0, "nothing was lost");

        // And once Keyup answers, all three arrive.
        assert!(wait_for(|| metrics.counter("uplink.uploaded").get() == 3));
        assert!(wait_for(|| spool.entries().is_empty()));
        uplink.shutdown();
    }

    #[test]
    fn a_503_then_200_uploads_the_call_exactly_once() {
        let server = CountingServer::start_with(Box::new(|n, _| {
            if n == 0 {
                (503, "down for maintenance".into())
            } else {
                (200, "Call imported successfully.".into())
            }
        }));
        let dir = TempDir::new("503-200");
        let metrics = Metrics::new();
        let uplink = uplink_for(&server, &dir, &metrics);
        uplink.submit(call_at(1_788_012_000_123, 52101));
        let spool = Spool::open(dir.0.clone(), 1 << 30).expect("open");
        assert!(wait_for(|| metrics.counter("uplink.uploaded").get() == 1));
        assert!(
            wait_for(|| spool.entries().is_empty()),
            "the drain removed it"
        );
        // More drain passes: nothing is sent twice.
        std::thread::sleep(Duration::from_millis(400));
        let stats = uplink.stats();
        uplink.shutdown();
        assert_eq!(server.seen(), 2, "the refused POST and the drained one");
        assert_eq!(stats.uploaded, 1);
        assert_eq!(stats.duplicates, 0);
        assert_eq!(stats.spooled, 1);
        assert!(
            !stats.circuit_open,
            "the drain's success closed the circuit"
        );
    }

    #[test]
    fn while_keyup_is_failing_live_calls_go_straight_to_the_spool() {
        let server = CountingServer::start_with(Box::new(|_, _| (503, "down".into())));
        let dir = TempDir::new("circuit");
        let metrics = Metrics::new();
        let uplink = KeyupUplink::start(
            UplinkConfig {
                url: server.url.clone(),
                spool_dir: dir.0.clone(),
                // No drain inside the test: only live calls reach the server.
                drain_interval: Duration::from_secs(600),
                audio_format: trunkd_call::AudioFormat::Wav,
                ..UplinkConfig::default()
            },
            &metrics,
        )
        .expect("uplink starts");
        let at = 1_788_012_000_123i64;
        uplink.submit(call_at(at, 52101));
        assert!(wait_for(|| uplink.stats().spooled == 1));
        assert!(uplink.stats().circuit_open);
        for i in 1..4 {
            uplink.submit(call_at(at + i * 10_000, 52101));
        }
        assert!(wait_for(|| uplink.stats().spooled == 4));
        let stats = uplink.stats();
        uplink.shutdown();
        assert_eq!(server.seen(), 1, "only the first call waited on Keyup");
        assert_eq!(stats.spooled_on_pressure, 3);
    }

    #[test]
    fn a_404_from_a_proxy_keeps_the_call() {
        let server =
            CountingServer::start_with(Box::new(|_, _| (404, "<html>Not Found</html>".into())));
        let dir = TempDir::new("404");
        let metrics = Metrics::new();
        let uplink = uplink_for(&server, &dir, &metrics);
        uplink.submit(call_at(1_788_012_000_123, 52101));
        let spool = Spool::open(dir.0.clone(), 1 << 30).expect("open");
        assert!(
            wait_for(|| spool.entries().len() == 1),
            "spooled, not deleted"
        );
        // Many drain passes into the same 404.
        assert!(wait_for(|| server.seen() >= 5));
        let stats = uplink.stats();
        uplink.shutdown();
        assert_eq!(spool.entries().len(), 1, "still there");
        assert_eq!(stats.failed, 0);
        assert_eq!(
            stats.quarantined, 0,
            "Keyup took nothing else, so the call is not the problem"
        );
        assert!(spool.quarantined().is_empty());
    }

    #[test]
    fn a_poisoned_entry_is_quarantined_and_the_next_one_drains() {
        // Keyup refuses one talkgroup with an answer nobody documented, and
        // takes everything else.
        let server = CountingServer::start_with(Box::new(|_, req| {
            if req.contains("name=\"talkgroup\"\r\n\r\n666\r\n") {
                (422, "Unprocessable Entity".into())
            } else {
                (200, "Call imported successfully.".into())
            }
        }));
        let dir = TempDir::new("poison");
        let spool = Spool::open(dir.0.clone(), 1 << 30).expect("open");
        // Oldest first: the poisoned call is at the head of the drain.
        let wav = vec![1u8; 64];
        let poisoned = call_at(1_788_012_000_123, 666);
        spool.store(&poisoned, Some(&wav), "wav").expect("store");
        spool
            .store(&call_at(1_788_098_400_123, 52101), Some(&wav), "wav")
            .expect("store");
        assert_eq!(spool.entries().len(), 2);

        let metrics = Metrics::new();
        let uplink = uplink_for(&server, &dir, &metrics);
        assert!(
            wait_for(|| metrics.counter("uplink.uploaded").get() >= 1),
            "the entry behind the poisoned one drains"
        );
        // Live traffic keeps going through, which is the evidence that the
        // refusals are about that one call.
        let mut at = 1_788_200_000_000i64;
        let quarantined = wait_for(|| {
            at += 10_000;
            uplink.submit(call_at(at, 52101));
            std::thread::sleep(Duration::from_millis(60));
            uplink.stats().quarantined == 1
        });
        assert!(quarantined, "quarantined after {QUARANTINE_AFTER} refusals");
        let stats = uplink.stats();
        uplink.shutdown();

        assert!(spool.entries().is_empty(), "the drain is clear");
        let q = spool.quarantined();
        assert_eq!(q.len(), 1, "moved, not deleted");
        assert!(q[0].json.starts_with(dir.0.join(crate::QUARANTINE_DIR)));
        assert!(q[0].json.to_string_lossy().contains(&poisoned.audio_stem()));
        assert!(q[0].audio.is_some(), "with its audio");
        assert_eq!(stats.failed, 0, "a refusal is not a deletion");
    }

    #[test]
    fn a_duplicate_answer_still_empties_the_spool() {
        let server = CountingServer::start_with(Box::new(|n, _| {
            if n == 0 {
                (503, "down".into())
            } else {
                (200, "duplicate call rejected".into())
            }
        }));
        let dir = TempDir::new("dup");
        let metrics = Metrics::new();
        let uplink = uplink_for(&server, &dir, &metrics);
        uplink.submit(call_at(1_788_012_000_123, 52101));
        assert!(wait_for(|| metrics.counter("uplink.duplicates").get() == 1));
        let spool = Spool::open(dir.0.clone(), 1 << 30).expect("open");
        assert!(wait_for(|| spool.entries().is_empty()));
        let stats = uplink.stats();
        uplink.shutdown();
        assert_eq!(stats.uploaded, 0);
        assert!(spool.quarantined().is_empty());
    }
}
