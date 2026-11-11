// SPDX-License-Identifier: GPL-3.0-or-later
//! Site observations: what the control channel says the site *is*.
//!
//! [`SiteInventory`] folds every control-channel [`P25Event`] into the site's
//! identity (WACN, system, RFSS, site, LRA, NAC), the services it advertises
//! (`SYS_SVC_BCAST`, opcode 56), its band plan, its secondary control
//! channels, the traffic frequencies it actually grants, and its neighbours.
//! Every `observations_interval_s` this thread renders that as JSON and POSTs
//! it to Keyup's `/api/recorder/observations`, then resets the window counters
//! so the next report describes the next interval rather than all of history.
//!
//! The inventory is not a transcript: the band plan, the neighbour table and
//! the secondary control channels are published only after the identical
//! decode has been heard twice, are rejected outright when the frequencies
//! they name cannot be real, and expire after an hour without a repeat — one
//! CRC-passing corrupt block on a weak site would otherwise be reported for
//! the life of the site. The rules and their constants live in
//! [`trunkd_p25::inventory`]; `docs/keyup.md` states them for operators.
//!
//! The fold is cheap but the POST is not, and neither belongs on the DSP
//! thread: control events cross a bounded queue to get here, and the HTTP call
//! (10 s timeout) blocks nothing but this thread.
//!
//! Keyup grows the endpoint in a parallel branch, so a 404 is expected for now
//! and logged at debug; a 5xx or a connection failure is a warning. Either way
//! the window still resets — observations are a periodic snapshot, not a queue,
//! and re-sending a stale window later would misreport it.
//!
//! # The other two jobs
//!
//! Since every signaling event already arrives here, this thread also runs the
//! non-voice fold that `docs/NON-VOICE-DATA.md` §5 asked for:
//!
//! * **Radio events** — the registration/affiliation/data family, batched and
//!   POSTed to `/api/recorder/events`. Unlike observations these *are* a queue:
//!   a failed batch is spooled to disk and retried oldest-first, because a
//!   registration missed is a radio the archive never learns about.
//! * **The raw event log** — one JSON line per non-voice event under
//!   `[recorder] event_log_dir`, rotated and gzipped daily.
//!
//! Both are deliberately here rather than on the DSP thread, and both are
//! bounded: the queue in front of them drops oldest under pressure (counted as
//! `nonvoice_dropped`), the event spool has a byte cap *and* a free-space floor
//! (both enforced on the drain timer, so a disk filled by something else is
//! noticed), and the log has a retention window.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crossbeam_channel::TryRecvError;
use serde_json::{json, Value};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use trunkd_core::chan::BoundedReceiver;
use trunkd_p25::inventory::{AdjacentSite, InventorySnapshot, ObservedFrequency, TrafficFrequency};
use trunkd_p25::message::ServiceClass;
use trunkd_p25::payload::{is_standard_vendor, MFID_HARRIS};
use trunkd_p25::SiteInventory;
use trunkd_uplink::events::{
    classify_events, events_body, EventAttempt, EventBatcher, EventSite, EventSpool,
    EventUplinkConfig,
};
use trunkd_uplink::UplinkHold;

#[cfg(test)]
use trunkd_p25::inventory::BandEntry;
#[cfg(test)]
use trunkd_p25::message::{AdjacentSiteFlags, SystemServices};

use crate::pipeline::eventlog::EventLog;
use crate::pipeline::failrun::FailureRun;
use crate::pipeline::nonvoice::{is_constant_broadcast, log_line, Channel, NonVoice};
use crate::pipeline::{ObsMsg, Shared};

/// Timeout for one observations POST, connect through response.
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);

/// How often the panel's copy of each site's observation document is rebuilt.
///
/// Independent of `observations_interval_s`: the operator sets that for Keyup,
/// and a panel that only redrew every five minutes would be useless. Rebuilding
/// is a snapshot and a `serde_json` render of a few hundred fields, which is
/// nothing next to the fold this thread already does per TSBK.
const DETAIL_INTERVAL: Duration = Duration::from_secs(2);

/// An unknown `(mfid, opcode)` seen no more than this many times in the window
/// is counted as noise rather than as a message kind worth chasing: one
/// CRC-passing corrupt block names a vendor and an opcode as confidently as a
/// real message does.
const NOISE_MAX: u64 = 2;

/// How many unknown kinds the detail document lists by count.
const TOP_UNKNOWN: usize = 6;

/// How long the tuner a site's control-channel stream is taken from may go
/// quiet before another tuner's stream replaces it. A locked control channel
/// delivers ~26 TSBKs a second, so three seconds of nothing means that tuner
/// has lost lock or stopped, not a lull.
const CONTROL_SOURCE_TAKEOVER: Duration = Duration::from_secs(3);

/// Which tuner a site's control-channel events are folded from.
///
/// Every tuner serving a site runs its own engine on the same control channel
/// (an auto front hunts and locks it as well as the control front), so with
/// two tuners each TSBK reaches this thread twice — once per engine, stamped by
/// each tuner's own stream clock. Folding both doubled the event log, every
/// census and rate in the observation, and the radio events sent to Keyup,
/// which cannot merge the copies because their times differ. One tuner's
/// stream is kept: the first to deliver, until it is quiet for
/// [`CONTROL_SOURCE_TAKEOVER`], when the next tuner that delivers takes over.
#[derive(Debug, Default)]
struct ControlSource {
    /// The tuner currently kept, and when it last delivered.
    current: Option<(usize, Instant)>,
}

/// What [`ControlSource::admit`] decided about one event.
#[derive(Debug, PartialEq, Eq)]
enum Admit {
    /// From the kept tuner.
    Keep,
    /// From a tuner that has just become the kept one; `from` is the tuner it
    /// replaced (`None` for the first event of the run).
    Switched { from: Option<usize> },
    /// A copy from another tuner while the kept one is live.
    Drop,
}

impl ControlSource {
    fn admit(&mut self, tuner: usize, now: Instant) -> Admit {
        match self.current {
            Some((kept, _)) if kept == tuner => {
                self.current = Some((tuner, now));
                Admit::Keep
            }
            Some((_, last)) if now.saturating_duration_since(last) < CONTROL_SOURCE_TAKEOVER => {
                Admit::Drop
            }
            previous => {
                self.current = Some((tuner, now));
                Admit::Switched {
                    from: previous.map(|(t, _)| t),
                }
            }
        }
    }
}

/// How the observations thread reaches Keyup.
#[derive(Clone, Debug)]
pub(crate) struct ObservationsConfig {
    /// Full endpoint URL. Empty disables posting (the fold still runs, so
    /// `--check`-style introspection and the tests can see it).
    pub(crate) url: String,
    /// Keyup API token, sent as `Authorization: Bearer`.
    pub(crate) api_key: String,
    /// Reporting interval.
    pub(crate) interval: Duration,
    /// Where and whether per-radio events are delivered. An empty
    /// [`EventUplinkConfig::url`] disables the POST; `radio_events` disables
    /// the fold entirely.
    pub(crate) events: EventUplinkConfig,
    /// Whether radio events are built and delivered at all.
    pub(crate) radio_events: bool,
    /// The runtime uplink hold. While it is set, observations are skipped
    /// outright (a snapshot nobody received is worth nothing later) and radio
    /// events go to the event spool instead of the network.
    pub(crate) hold: std::sync::Arc<UplinkHold>,
    /// Directory for the raw event log; `None` disables it.
    pub(crate) event_log_dir: Option<PathBuf>,
    /// Days of event log to keep.
    pub(crate) event_log_keep_days: u16,
    /// The shared event-log state block, so the newest line written is
    /// reported on the status POST and the panel. `None` in tests that do not
    /// care; the recorder always supplies it.
    pub(crate) event_log_state: Option<std::sync::Arc<crate::pipeline::eventarchive::ArchiveState>>,
    /// This recorder's stable id, stamped on every observation and event batch
    /// so a receiver can tell two recorders' views of one site apart.
    pub(crate) instance_id: String,
    /// Where the panel's copy of each site's observation document is published.
    /// `None` in tests that do not read it; the recorder always supplies one.
    pub(crate) details: Option<Arc<SiteDetails>>,
}

/// Derive the observations endpoint from the configured call-upload URL.
///
/// The two live on the same host and the operator configures one URL, so the
/// endpoint is the upload URL with its path swapped. A URL that is not the
/// upload route is left alone but for appending the route, which keeps a
/// hand-written override working.
pub(crate) fn observations_url(call_upload_url: &str) -> String {
    const UPLOAD: &str = "/api/call-upload";
    const OBSERVATIONS: &str = "/api/recorder/observations";
    let trimmed = call_upload_url.trim_end_matches('/');
    match trimmed.rfind(UPLOAD) {
        Some(at) => format!(
            "{}{OBSERVATIONS}{}",
            &trimmed[..at],
            &trimmed[at + UPLOAD.len()..]
        ),
        None => format!("{trimmed}{OBSERVATIONS}"),
    }
}

/// Per-site state: the inventory, the non-voice fold, and the batch of radio
/// events waiting to be sent for that site.
struct SiteAux {
    inv: SiteInventory,
    nonvoice: NonVoice,
    batcher: EventBatcher,
    /// Highest stream time seen, for dating the snapshot.
    latest_ms: u64,
    /// This site's run of failed observation POSTs, and of failed radio-event
    /// POSTs: an outage is logged when it starts, while it lasts (every ten
    /// minutes) and when it ends, not per attempt.
    observation_failures: FailureRun,
    event_failures: FailureRun,
    /// The one tuner whose control-channel stream is folded.
    control_source: ControlSource,
    /// Control-channel events dropped as another tuner's copy.
    control_duplicates: trunkd_core::Metric,
}

/// The panel's read-only copy of each site's observation document.
///
/// The inventories themselves are owned outright by this thread — the fold
/// runs per TSBK and must never wait on an HTTP handler — so the panel is
/// handed a *rendered* document instead of a handle to the live state. This
/// thread refreshes each slot every [`DETAIL_INTERVAL`]; the panel thread takes
/// the mutex only long enough to clone the last document written, and a panel
/// that blocked forever could not stall the fold for longer than one clone.
#[derive(Debug)]
pub(crate) struct SiteDetails {
    slots: Vec<Mutex<Option<Value>>>,
}

impl SiteDetails {
    /// One slot per configured site, all empty until the first refresh.
    pub(crate) fn new(sites: usize) -> Arc<Self> {
        Arc::new(SiteDetails {
            slots: (0..sites).map(|_| Mutex::new(None)).collect(),
        })
    }

    /// The last document built for one site, or `None` for an unknown index or
    /// a site whose first refresh has not run yet.
    pub(crate) fn get(&self, site: usize) -> Option<Value> {
        lock(self.slots.get(site)?).clone()
    }

    /// Whether `site` is a site at all — the route's 404 test.
    pub(crate) fn has(&self, site: usize) -> bool {
        site < self.slots.len()
    }

    /// How many services the site advertises as available: `Some(None)` when no
    /// system service broadcast has been heard yet, `None` when there is no
    /// document. Read without cloning the document, because `/api/status`
    /// summarises every site on every two-second poll.
    pub(crate) fn services_available(&self, site: usize) -> Option<Option<usize>> {
        let doc = lock(self.slots.get(site)?);
        let doc = doc.as_ref()?;
        Some(
            doc.get("services")
                .and_then(|s| s.get("available"))
                .and_then(|a| a.as_array())
                .map(Vec::len),
        )
    }

    /// Publish one site's document. The observations thread is the only writer
    /// in a running recorder; the panel's tests use it to seed a site.
    pub(crate) fn publish(&self, site: usize, doc: Value) {
        if let Some(slot) = self.slots.get(site) {
            *lock(slot) = Some(doc);
        }
    }
}

/// A poisoned detail mutex is not a reason to stop reporting: the only thing
/// under it is a document that is about to be overwritten anyway.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// One site this recorder decodes, as the detail document names it — so the
/// panel can mark a neighbour that is not just a neighbour.
#[derive(Clone, Debug)]
pub(crate) struct SiteIdentity {
    pub(crate) system_id: Option<u16>,
    pub(crate) rfss: Option<u8>,
    pub(crate) site: Option<u8>,
    pub(crate) label: String,
}

/// Everything the events path needs, resolved once at startup.
struct EventsOut {
    cfg: EventUplinkConfig,
    /// The shared hold, read on every batch and every drain pass.
    hold: std::sync::Arc<UplinkHold>,
    spool: Option<EventSpool>,
    /// Counter of batches Keyup accepted.
    sent: trunkd_core::Metric,
    /// Counter of batches written to the spool instead.
    spooled: trunkd_core::Metric,
    /// The "route not deployed" warning is logged once, not once per batch.
    not_deployed_logged: bool,
    /// Monotonic tie-breaker for two batches spooled in the same millisecond.
    seq: u64,
    last_drain: Instant,
}

/// Run the observations thread until the event channel closes.
pub(crate) fn run(rx: BoundedReceiver<ObsMsg>, shared: Shared, cfg: ObservationsConfig) {
    let mut sites: Vec<SiteAux> = shared
        .sites
        .iter()
        .enumerate()
        .map(|(i, s)| SiteAux {
            inv: SiteInventory::new(s.control_frequencies.clone()),
            nonvoice: NonVoice::new(),
            batcher: EventBatcher::new(cfg.events.max_batch_age),
            latest_ms: 0,
            observation_failures: FailureRun::default(),
            event_failures: FailureRun::default(),
            control_source: ControlSource::default(),
            control_duplicates: shared
                .metrics
                .counter(&format!("site.{i}.control_duplicates")),
        })
        .collect();
    let mut last_post = Instant::now();
    // Far enough back that the first pass through the loop publishes a
    // document: an operator who opens the panel at startup should not be told
    // the site is unknown for two seconds.
    let mut last_detail = Instant::now()
        .checked_sub(DETAIL_INTERVAL)
        .unwrap_or_else(Instant::now);
    let posted = shared.metrics.counter("observations.posted");
    let failed = shared.metrics.counter("observations.failed");
    let dropped_log = shared.metrics.counter("event_log.dropped");
    let log_written = shared.metrics.gauge("event_log.written");

    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(HTTP_TIMEOUT))
        .build()
        .into();

    // A log directory that cannot be created is a configuration problem worth
    // saying out loud, but not one worth refusing to record over.
    let mut event_log = cfg.event_log_dir.as_ref().and_then(|dir| {
        match EventLog::open(dir, cfg.event_log_keep_days) {
            Ok(log) => Some(log),
            Err(e) => {
                tracing::warn!(dir = %dir.display(), error = %e, "event log disabled");
                None
            }
        }
    });
    // Prune once at startup so a recorder that was off for a month does not
    // wait for its first midnight to drop what it should not still be keeping.
    if let Some(log) = &event_log {
        log.prune();
    }

    let mut events = cfg.radio_events.then(|| EventsOut {
        spool: match EventSpool::open(&cfg.events.spool_dir, cfg.events.spool_cap_bytes) {
            Ok(sp) => Some(sp.with_min_free_bytes(cfg.events.spool_min_free_bytes)),
            Err(e) => {
                tracing::warn!(
                    dir = %cfg.events.spool_dir.display(),
                    error = %e,
                    "radio-event spool disabled; failed batches will be dropped"
                );
                None
            }
        },
        cfg: cfg.events.clone(),
        hold: cfg.hold.clone(),
        sent: shared.metrics.counter("radio_events.sent"),
        spooled: shared.metrics.counter("radio_events.spooled"),
        not_deployed_logged: false,
        seq: 0,
        last_drain: Instant::now(),
    });

    loop {
        // Polling rather than blocking is what keeps the reporting interval
        // honest on a quiet site, where incoming events alone would never wake
        // this thread often enough to notice the deadline.
        match rx.try_recv() {
            Ok(msg) => observe(
                &mut sites,
                &shared,
                msg,
                event_log.as_mut(),
                events.as_mut(),
                &dropped_log,
                cfg.event_log_state.as_deref(),
            ),
            Err(TryRecvError::Empty) => {
                // Idle is the right moment to make the log durable and to
                // republish its size: neither costs anything when there is no
                // event waiting behind them.
                if let Some(log) = event_log.as_mut() {
                    log.flush();
                    log_written.set(log.written());
                    // Once a minute at most: finishes a day nobody wrote to
                    // after midnight (compressed, so the archiver ships it).
                    log.tick();
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(TryRecvError::Disconnected) => break,
        }
        maybe_post(
            &agent,
            &cfg,
            &shared,
            &mut sites,
            &mut last_post,
            &posted,
            &failed,
        );
        refresh_details(&cfg, &shared, &sites, &mut last_detail);
        if let Some(out) = events.as_mut() {
            flush_due_batches(&agent, &shared, &mut sites, out);
            drain_spool(&agent, out);
        }
    }

    // One last report on the way out, so a short run still says what it heard.
    for (i, aux) in sites.iter_mut().enumerate() {
        post_one(&agent, &cfg, &shared, i, aux, &posted, &failed);
    }
    if let Some(out) = events.as_mut() {
        for (i, aux) in sites.iter_mut().enumerate() {
            send_batch(&agent, &shared, i, aux, out, true);
        }
        drain_spool(&agent, out);
    }
    if let Some(log) = event_log.as_mut() {
        log.flush();
        log_written.set(log.written());
    }
}

/// Fold one event into its site: inventory, non-voice counters, the radio-event
/// batch, and the raw log.
fn observe(
    sites: &mut [SiteAux],
    shared: &Shared,
    msg: ObsMsg,
    event_log: Option<&mut EventLog>,
    events: Option<&mut EventsOut>,
    dropped_log: &trunkd_core::Metric,
    log_state: Option<&crate::pipeline::eventarchive::ArchiveState>,
) {
    let Some(aux) = sites.get_mut(msg.site) else {
        return;
    };

    // One control-channel stream per site (see `ControlSource`). A clock
    // resync is about the tuner that sent it, so it is never someone's copy.
    if msg.traffic_hz == 0 && !matches!(msg.event, trunkd_p25::P25Event::ClockResync { .. }) {
        match aux.control_source.admit(msg.tuner, Instant::now()) {
            Admit::Keep => {}
            Admit::Switched { from: None } => {
                tracing::debug!(
                    site = msg.site,
                    tuner = msg.tuner,
                    "control-channel stream from this tuner"
                );
            }
            Admit::Switched { from: Some(from) } => {
                tracing::info!(
                    site = %shared.sites[msg.site].site_label,
                    from,
                    to = msg.tuner,
                    "control-channel stream now taken from another tuner: the previous one went quiet"
                );
            }
            Admit::Drop => {
                aux.control_duplicates.inc();
                return;
            }
        }
    }

    aux.latest_ms = aux.latest_ms.max(msg.at_ms);

    let channel = if msg.traffic_hz == 0 {
        Channel::Control
    } else {
        Channel::Traffic(msg.traffic_hz)
    };

    let radio_event = match channel {
        Channel::Control => {
            aux.inv.observe(msg.control_hz, &msg.event, msg.at_ms);
            aux.nonvoice.observe_control(&msg.event, msg.at_ms)
        }
        Channel::Traffic(_) => aux.nonvoice.observe_traffic(&msg.event, msg.at_ms),
    };
    if let Some(ev) = radio_event {
        if events.is_some() {
            // `at_ms` throughout this thread is already wall-clock: each device
            // mapped it through its own epoch before enqueueing.
            let at = rfc3339(ev.at_ms as i64);
            aux.batcher.push(ev.to_json(at));
        }
    }

    if let Some(log) = event_log {
        let wall = msg.at_ms as i64;
        if let Some(line) = log_line(&msg.event, channel, &rfc3339(wall)) {
            let mut line = line;
            // Which site, so one directory can hold a multi-site recorder's log
            // and still be separable. Additive to the documented line shape.
            if shared.sites.len() > 1 {
                line["site"] = json!(shared.sites[msg.site].site_label);
            }
            let repeatable = is_constant_broadcast(&msg.event);
            match log.write_record(wall, line, repeatable) {
                // One relaxed store per line, on the observations thread: the
                // cheapest possible answer to "is the event log still being
                // written, and when did it stop?".
                Ok(()) => {
                    if let Some(state) = log_state {
                        state.note_written(wall);
                    }
                }
                Err(e) => {
                    dropped_log.inc();
                    tracing::debug!(error = %e, "event log write failed");
                }
            }
        }
    }
}

/// Send any site's batch that has filled up or aged out.
fn flush_due_batches(
    agent: &ureq::Agent,
    shared: &Shared,
    sites: &mut [SiteAux],
    out: &mut EventsOut,
) {
    for (i, aux) in sites.iter_mut().enumerate() {
        if aux.batcher.is_due() {
            send_batch(agent, shared, i, aux, out, false);
        }
    }
}

/// POST one site's pending batch, spooling it if that fails.
///
/// `force` sends a partial batch regardless of size or age — the shutdown path,
/// where waiting for the next deadline means losing the events.
fn send_batch(
    agent: &ureq::Agent,
    shared: &Shared,
    site: usize,
    aux: &mut SiteAux,
    out: &mut EventsOut,
    force: bool,
) {
    if aux.batcher.is_empty() || (!force && !aux.batcher.is_due()) {
        return;
    }
    let batch = aux.batcher.take();
    let spec = &shared.sites[site];
    let snapshot = aux.inv.snapshot(aux.latest_ms);
    let body = events_body(
        &EventSite {
            system_label: spec.system_label.clone(),
            site_label: spec.site_label.clone(),
            wacn: snapshot.wacn,
            system_id: snapshot.system_id,
            nac: snapshot.nac,
            rfss: snapshot.rfss,
            site: snapshot.site,
        },
        &batch,
        &out.cfg.instance,
    )
    .to_string();

    if out.cfg.url.is_empty() {
        tracing::debug!(site = %spec.display, "radio-events endpoint disabled; not posting");
        return;
    }
    // Uploads held: the batch goes to the spool without a socket being opened,
    // and the drain pass delivers it once the hold is released.
    if out.hold.is_held() {
        tracing::debug!(site = %spec.display, events = batch.len(), "uploads held; spooling radio events");
        spool_batch(out, &body, batch.len() as u64);
        return;
    }

    match post_events(agent, &out.cfg, &body) {
        EventAttempt::Stored(n) => {
            out.sent.add(batch.len() as u64);
            tracing::debug!(site = %spec.display, events = batch.len(), stored = n, "radio events posted");
            if let Some((attempts, lasted)) = aux.event_failures.succeeded(Instant::now()) {
                tracing::info!(
                    site = %spec.display,
                    failed_attempts = attempts,
                    lasted_s = lasted.as_secs(),
                    "radio-event POST working again"
                );
            }
        }
        EventAttempt::NotDeployed => {
            if !out.not_deployed_logged {
                out.not_deployed_logged = true;
                tracing::warn!(
                    site = %spec.display,
                    url = %out.cfg.url,
                    "Keyup has no /api/recorder/events route yet (404); \
                     radio events are being spooled until it does"
                );
            }
            spool_batch(out, &body, batch.len() as u64);
        }
        EventAttempt::Retry(e) => {
            if let Some(attempts) = aux.event_failures.failed(Instant::now()) {
                tracing::warn!(
                    site = %spec.display,
                    error = %e,
                    attempts,
                    "radio-event POST failed; spooling"
                );
            } else {
                tracing::debug!(site = %spec.display, error = %e, "radio-event POST still failing; spooling");
            }
            spool_batch(out, &body, batch.len() as u64);
        }
    }
}

/// Write one undelivered batch to the spool and re-enforce its cap.
fn spool_batch(out: &mut EventsOut, body: &str, events: u64) {
    let Some(spool) = &out.spool else {
        return;
    };
    let now = trunkd_core::Timestamp::now().unix_millis();
    out.seq = out.seq.wrapping_add(1);
    match spool.store(body, now, out.seq) {
        Ok(_) => {
            out.spooled.add(events);
            sweep_spool(spool);
        }
        Err(e) => tracing::warn!(error = %e, "could not spool a radio-event batch"),
    }
}

/// Enforce the event spool's byte cap and free-space floor.
///
/// Called after every spooled batch *and* on the drain timer: the volume can
/// fill up from anything on the box, and a recorder that is delivering its
/// events happily would otherwise never look.
fn sweep_spool(spool: &EventSpool) {
    let sweep = spool.enforce_limits();
    if sweep.evicted() > 0 {
        tracing::warn!(
            cap_evicted = sweep.cap_evicted,
            floor_evicted = sweep.floor_evicted,
            free_bytes = sweep.free_bytes.unwrap_or(0),
            "radio-event spool over its limits; oldest batches dropped"
        );
    }
}

/// Retry the spool oldest-first, on the drain timer.
///
/// One batch per pass: a recorder catching up after an outage must not spend
/// its whole loop on the backlog while live events queue behind it.
fn drain_spool(agent: &ureq::Agent, out: &mut EventsOut) {
    if out.cfg.url.is_empty()
        || out.hold.is_held()
        || out.last_drain.elapsed() < out.cfg.drain_interval
    {
        return;
    }
    out.last_drain = Instant::now();
    let Some(spool) = out.spool.clone() else {
        return;
    };
    // On the timer, whether or not there is anything to retry: this is the only
    // pass that notices a volume filled by something other than trunkd.
    sweep_spool(&spool);
    let Some(entry) = spool.entries().into_iter().next() else {
        return;
    };
    let Ok(body) = std::fs::read_to_string(&entry.path) else {
        // Unreadable: dropping it is better than retrying it forever.
        spool.remove(&entry);
        return;
    };
    if let EventAttempt::Stored(n) = post_events(agent, &out.cfg, &body) {
        spool.remove(&entry);
        tracing::debug!(stored = n, "spooled radio-event batch delivered");
    }
}

/// One events POST, classified.
fn post_events(agent: &ureq::Agent, cfg: &EventUplinkConfig, body: &str) -> EventAttempt {
    match agent
        .post(&cfg.url)
        .header("Authorization", &format!("Bearer {}", cfg.api_key))
        .header("Content-Type", "application/json")
        .send(body)
    {
        Ok(mut resp) => {
            let status = resp.status().as_u16();
            let text = resp.body_mut().read_to_string().unwrap_or_default();
            classify_events(status, &text)
        }
        Err(ureq::Error::StatusCode(code)) => classify_events(code, ""),
        Err(e) => EventAttempt::Retry(e.to_string()),
    }
}

#[allow(clippy::too_many_arguments)]
fn maybe_post(
    agent: &ureq::Agent,
    cfg: &ObservationsConfig,
    shared: &Shared,
    sites: &mut [SiteAux],
    last_post: &mut Instant,
    posted: &trunkd_core::Metric,
    failed: &trunkd_core::Metric,
) -> bool {
    if cfg.interval.is_zero() || last_post.elapsed() < cfg.interval {
        return false;
    }
    *last_post = Instant::now();
    for (i, aux) in sites.iter_mut().enumerate() {
        post_one(agent, cfg, shared, i, aux, posted, failed);
    }
    true
}

fn post_one(
    agent: &ureq::Agent,
    cfg: &ObservationsConfig,
    shared: &Shared,
    site: usize,
    aux: &mut SiteAux,
    posted: &trunkd_core::Metric,
    failed: &trunkd_core::Metric,
) {
    let snapshot = aux.inv.snapshot(aux.latest_ms);
    // Nothing heard at all: an empty report tells Keyup nothing it does not
    // already know from the recorder's config.
    if snapshot.nac.is_none() && snapshot.control.is_none() && snapshot.traffic.is_empty() {
        aux.inv.reset_window();
        aux.nonvoice.reset_window();
        return;
    }
    let spec = &shared.sites[site];
    let body = observation_body(
        &snapshot,
        &spec.system_label,
        &spec.site_label,
        cfg.interval.as_secs(),
        |ms| rfc3339(ms as i64),
        rfc3339(trunkd_core::Timestamp::now().unix_millis()),
        aux.nonvoice.stats_json(),
        &cfg.instance_id,
    );
    aux.inv.reset_window();
    aux.nonvoice.reset_window();

    if cfg.url.is_empty() {
        tracing::debug!(site = %spec.display, "observations endpoint disabled; not posting");
        return;
    }
    // Uploads held: dropped rather than spooled. An observation is a periodic
    // snapshot of a window that has already been closed and reset above; a
    // stack of them delivered hours later would only misdate the site's history.
    if cfg.hold.is_held() {
        tracing::debug!(site = %spec.display, "uploads held; skipping the observation");
        return;
    }

    match agent
        .post(&cfg.url)
        .header("Authorization", &format!("Bearer {}", cfg.api_key))
        .header("Content-Type", "application/json")
        .send(body.to_string())
    {
        Ok(mut resp) => {
            posted.inc();
            let status = resp.status().as_u16();
            // Drained so the socket closes after the reply, not during it.
            let _ = resp.body_mut().read_to_string();
            tracing::debug!(site = %spec.display, status, "site observations posted");
            if let Some((attempts, lasted)) = aux.observation_failures.succeeded(Instant::now()) {
                tracing::info!(
                    site = %spec.display,
                    failed_attempts = attempts,
                    lasted_s = lasted.as_secs(),
                    "site observations POST working again"
                );
            }
        }
        // Keyup grows this endpoint in a parallel branch; until it lands a 404
        // is the expected answer and not worth a warning.
        Err(ureq::Error::StatusCode(404)) => {
            failed.inc();
            tracing::debug!(site = %spec.display, "observations endpoint not deployed yet (404)");
        }
        Err(e) => {
            failed.inc();
            if let Some(attempts) = aux.observation_failures.failed(Instant::now()) {
                tracing::warn!(
                    site = %spec.display,
                    error = %e,
                    attempts,
                    "site observations POST failed"
                );
            } else {
                tracing::debug!(site = %spec.display, error = %e, "site observations POST still failing");
            }
        }
    }
}

/// Rebuild the panel's copy of every site's observation document.
///
/// Read-only over the inventories: the window counters are *not* reset here —
/// that belongs to the report Keyup actually receives, and a panel poll must
/// never move the boundary of a window somebody else is being told about.
fn refresh_details(
    cfg: &ObservationsConfig,
    shared: &Shared,
    sites: &[SiteAux],
    last: &mut Instant,
) {
    let Some(details) = cfg.details.as_ref() else {
        return;
    };
    if last.elapsed() < DETAIL_INTERVAL {
        return;
    }
    *last = Instant::now();

    // Every site this recorder decodes, so a neighbour announcement can be
    // marked as "one of ours" rather than as an unknown.
    let peers: Vec<SiteIdentity> = sites
        .iter()
        .enumerate()
        .map(|(i, aux)| {
            let snap = aux.inv.snapshot(aux.latest_ms);
            SiteIdentity {
                system_id: snap.system_id,
                rfss: snap.rfss,
                site: snap.site,
                label: shared.sites[i].display.clone(),
            }
        })
        .collect();

    let now = rfc3339(trunkd_core::Timestamp::now().unix_millis());
    for (i, aux) in sites.iter().enumerate() {
        let spec = &shared.sites[i];
        let locked_hz = shared
            .metrics
            .gauge(&format!("{}.control_frequency_hz", spec.metric_prefix))
            .get();
        details.publish(
            i,
            detail_body(
                &aux.inv.snapshot(aux.latest_ms),
                &spec.system_label,
                &spec.site_label,
                cfg.interval.as_secs(),
                now.clone(),
                aux.nonvoice.stats_json(),
                &cfg.instance_id,
                locked_hz,
                &peers,
            ),
        );
    }
}

/// The panel's per-site document: the observation body Keyup receives, plus the
/// handful of things only a local operator cares about.
///
/// One renderer, deliberately: the panel's expandable site profile and the
/// archive's observation must not be able to disagree about what the site said.
#[allow(clippy::too_many_arguments)]
pub(crate) fn detail_body(
    s: &InventorySnapshot,
    system_label: &str,
    site_label: &str,
    window_s: u64,
    observed_at: String,
    nonvoice: Value,
    instance: &str,
    locked_hz: u64,
    peers: &[SiteIdentity],
) -> Value {
    let mut doc = observation_body(
        s,
        system_label,
        site_label,
        window_s,
        |ms| rfc3339(ms as i64),
        observed_at,
        nonvoice,
        instance,
    );
    let kinds = message_kinds(doc.get("message_mix").unwrap_or(&Value::Null));
    let unknowns = unknowns(s, &kinds, peers);
    if let Some(o) = doc.as_object_mut() {
        o.insert("site_label".to_owned(), json!(site_label));
        o.insert("system_label".to_owned(), json!(system_label));
        // What the *tuner* is doing, which the observation body has no reason
        // to carry: a site can have announced a control channel it is no longer
        // being decoded on.
        o.insert("locked".to_owned(), json!(locked_hz != 0));
        o.insert(
            "control_locked_hz".to_owned(),
            match locked_hz {
                0 => Value::Null,
                hz => json!(hz),
            },
        );
        o.insert(
            "recorder_sites".to_owned(),
            json!(peers
                .iter()
                .map(|p| json!({
                    "system_id": p.system_id,
                    "rfss": p.rfss,
                    "site": p.site,
                    "label": p.label,
                }))
                .collect::<Vec<_>>()),
        );
        o.insert("message_kinds".to_owned(), kinds.to_json());
        o.insert("unknowns".to_owned(), json!(unknowns));
    }
    doc
}

/// The message census split into what the decoder names and what it does not.
#[derive(Debug, Default, PartialEq, Eq)]
struct MessageKinds {
    /// Distinct `(mfid, opcode)` pairs the p25 crate names.
    known: usize,
    /// Distinct unnamed pairs seen more than [`NOISE_MAX`] times — the ones
    /// worth a look.
    unknown: usize,
    /// Distinct unnamed pairs seen at most [`NOISE_MAX`] times.
    noise: usize,
    /// The recurring unknowns, busiest first.
    top: Vec<(u8, u8, u64)>,
}

impl MessageKinds {
    fn to_json(&self) -> Value {
        json!({
            "known": self.known,
            "unknown": self.unknown,
            "noise": self.noise,
            "top_unknown": self.top.iter().map(|(mfid, opcode, count)| json!({
                "mfid": mfid,
                "opcode": opcode,
                "count": count,
            })).collect::<Vec<_>>(),
        })
    }
}

/// Split `message_mix` — `{"<mfid>:<opcode>": n}`, with the AMBT census under
/// `ambt:` keys — into known, unknown and noise.
///
/// The three counts are *kinds*, not messages: "eleven kinds named, two not"
/// is the question an operator opening a site profile is asking, and one
/// chatty grant opcode would drown the answer if messages were counted.
fn message_kinds(mix: &Value) -> MessageKinds {
    let mut out = MessageKinds::default();
    let Some(mix) = mix.as_object() else {
        return out;
    };
    for (key, count) in mix {
        let Some((mfid, opcode)) = parse_kind(key) else {
            continue;
        };
        let count = count.as_u64().unwrap_or(0);
        if is_named_kind(mfid, opcode) {
            out.known += 1;
        } else if count <= NOISE_MAX {
            out.noise += 1;
        } else {
            out.unknown += 1;
            out.top.push((mfid, opcode, count));
        }
    }
    // Busiest first; the pair breaks ties so the list is stable between polls.
    out.top
        .sort_by(|a, b| b.2.cmp(&a.2).then(a.0.cmp(&b.0)).then(a.1.cmp(&b.1)));
    out.top.truncate(TOP_UNKNOWN);
    out
}

/// `"<mfid>:<opcode>"`, or the same with the `ambt:` prefix a multi-block
/// message carries.
fn parse_kind(key: &str) -> Option<(u8, u8)> {
    let key = key.strip_prefix("ambt:").unwrap_or(key);
    let (mfid, opcode) = key.split_once(':')?;
    Some((mfid.parse().ok()?, opcode.parse().ok()?))
}

/// Whether [`trunkd_p25`] names this `(mfid, opcode)` — the standard opcode
/// table under MFID 0/1, plus the one L3Harris opcode it decodes by value.
///
/// The rest of the Harris table is *catalogued* (`harris_osp_name` gives
/// `HARRIS OSP 15` for every opcode) rather than understood, which is exactly
/// what the unknown column is for.
fn is_named_kind(mfid: u8, opcode: u8) -> bool {
    is_standard_vendor(mfid) || (mfid == MFID_HARRIS && opcode == 0x30)
}

/// What this site is showing that the recorder cannot account for.
///
/// Deliberately prose: these are the lines an operator reads before deciding
/// whether a site needs another tuner, another control frequency in the config,
/// or a look at `docs/NON-VOICE-DATA.md`.
fn unknowns(s: &InventorySnapshot, kinds: &MessageKinds, peers: &[SiteIdentity]) -> Vec<String> {
    let mut out = Vec::new();

    for a in &s.adjacent_sites {
        let ours = peers.iter().any(|p| {
            p.system_id == Some(a.system_id) && p.rfss == Some(a.rfss) && p.site == Some(a.site)
        });
        if !ours {
            out.push(format!(
                "neighbour {:03X}/{}-{} ({}) is not a site this recorder decodes",
                a.system_id,
                a.rfss,
                a.site,
                a.control_hz
                    .map(|hz| format!("{:.4} MHz", hz as f64 / 1e6))
                    .unwrap_or_else(|| "frequency unknown".to_owned()),
            ));
        }
    }

    for c in &s.secondary_control {
        if c.hz.is_none() {
            out.push(format!(
                "secondary control channel {} has no resolvable frequency",
                c.channel_label.clone().unwrap_or_else(|| "?".to_owned()),
            ));
        }
    }

    if s.system_services.is_none() {
        out.push("services never heard (no system service broadcast yet)".to_owned());
    }

    if kinds.unknown > 0 {
        out.push(format!(
            "{} recurring message kind{} the decoder does not name",
            kinds.unknown,
            if kinds.unknown == 1 { "" } else { "s" },
        ));
    }

    // A band an announcement referred to whose identifier update has never been
    // heard: every channel number on that band is unresolvable until it is.
    let heard: Vec<u8> = s.bands.iter().map(|b| b.id).collect();
    let mut missing: Vec<u8> = s
        .control
        .iter()
        .chain(s.secondary_control.iter())
        .filter_map(|c| c.band)
        .filter(|b| !heard.contains(b))
        .collect();
    missing.sort_unstable();
    missing.dedup();
    for band in missing {
        out.push(format!(
            "band {band} is referenced on air but its identifier update has never been heard"
        ));
    }
    if s.bands.is_empty() {
        out.push("no band plan heard".to_owned());
    }

    out
}

/// A fully populated snapshot: every optional field set, one of each list.
///
/// Module-level rather than test-module-level so the panel's tests can seed a
/// site profile from exactly the snapshot the observation tests assert on —
/// two views of one document, built from one fixture.
#[cfg(test)]
pub(crate) fn sample_snapshot() -> InventorySnapshot {
    InventorySnapshot {
        nac: Some(0x293),
        wacn: Some(0xBEE00),
        system_id: Some(0x1F5),
        rfss: Some(1),
        site: Some(7),
        lra: Some(3),
        system_services: Some((
            SystemServices::from_bits(0x0C_2C68),
            SystemServices::from_bits(0x0C_2C68),
            1,
        )),
        control: Some(ObservedFrequency {
            hz: Some(851_050_000),
            band: Some(1),
            channel: Some(638),
            channel_label: Some("1-638".into()),
            service_class: Some(ServiceClass::from_byte(0x21)),
            first_seen_ms: 0,
            last_seen_ms: 60_000,
            announcements: 12,
        }),
        secondary_control: vec![ObservedFrequency {
            hz: Some(852_137_500),
            band: Some(1),
            channel: Some(700),
            channel_label: Some("1-700".into()),
            service_class: Some(ServiceClass::from_byte(0x24)),
            first_seen_ms: 100,
            last_seen_ms: 50_000,
            announcements: 4,
        }],
        configured_control: vec![851_050_000, 852_137_500],
        bands: vec![BandEntry {
            id: 1,
            base_hz: 851_006_250,
            spacing_hz: 6_250,
            offset_hz: -45_000_000,
            bandwidth_hz: 12_500,
            tdma: false,
            slots_per_carrier: 1,
            first_seen_ms: 0,
            last_seen_ms: 60_000,
        }],
        traffic: vec![TrafficFrequency {
            hz: 852_825_000,
            channel: Some("1-810".into()),
            grants: 9,
            calls: 7,
            tdma_refused: 2,
            first_seen_ms: 1_000,
            last_seen_ms: 59_000,
        }],
        adjacent_sites: vec![AdjacentSite {
            system_id: 0x1F5,
            rfss: 1,
            site: 9,
            lra: 3,
            control_hz: Some(853_112_500),
            channel: "1-460".into(),
            flags: AdjacentSiteFlags {
                conventional: false,
                site_failure: false,
                valid_information: true,
                active_rfss_connection: true,
            },
            service_class: ServiceClass::from_byte(0x21),
            first_seen_ms: 500,
            last_seen_ms: 58_000,
            announcements: 6,
        }],
        tsbk_window: 900,
        tsbk_failed_window: 12,
        window_ms: 60_000,
    }
}

/// Render an [`InventorySnapshot`] as the observations request body.
///
/// `wall` maps the snapshot's stream-time milliseconds to an RFC 3339 instant;
/// splitting it out is what lets this be tested without a clock or a device.
///
/// `nonvoice` is [`NonVoice::stats_json`] for the same window: the message
/// histogram, unknown-vendor exemplars, link-control mix, data-channel counters
/// and low-speed-data count are merged in at the top level, so Keyup sees one
/// document rather than two.
#[allow(clippy::too_many_arguments)]
pub(crate) fn observation_body(
    s: &InventorySnapshot,
    system_label: &str,
    site_label: &str,
    window_s: u64,
    wall: impl Fn(u64) -> String,
    observed_at: String,
    nonvoice: Value,
    instance: &str,
) -> Value {
    let mut doc = json!({
        "recorder": "trunkd",
        // Which recorder said it. `recorder` names the software; several boxes
        // run it and hear the same site, and only this tells their reports
        // apart. The same id the tuner-status POST sends as `instance_id`.
        "instance": instance,
        "recorder_version": env!("CARGO_PKG_VERSION"),
        "observed_at": observed_at,
        "window_s": window_s,
        "system": {
            "label": system_label,
            "wacn": s.wacn,
            "system_id": s.system_id,
            "nac": s.nac,
        },
        "site": {
            "label": site_label,
            "rfss": s.rfss,
            "site": s.site,
            "lra": s.lra,
        },
        // What the site says it can do (opcode 56). `null` until a system
        // service broadcast has been heard, so absent never reads as "offers
        // nothing".
        "services": s.system_services.map(|(available, supported, priority)| json!({
            "available": available.names(),
            "supported": supported.names(),
            "available_raw": format!("0x{:06x}", available.raw),
            "supported_raw": format!("0x{:06x}", supported.raw),
            "request_priority": priority,
        })),
        "control": s.control.as_ref().map(|c| json!({
            "frequency_hz": c.hz,
            "channel": c.channel_label,
            "tsbk_rate": tsbk_rate(s),
        })),
        "secondary_control": s
            .secondary_control
            .iter()
            .map(secondary_json)
            .collect::<Vec<_>>(),
        "configured_control": s.configured_control,
        "bands": s.bands.iter().map(|b| json!({
            "id": b.id,
            "base_hz": b.base_hz,
            "spacing_hz": b.spacing_hz,
            "offset_hz": b.offset_hz,
            "bandwidth_hz": b.bandwidth_hz,
            "tdma": b.tdma,
        })).collect::<Vec<_>>(),
        "traffic": s.traffic.iter().map(|t| traffic_json(t, &wall)).collect::<Vec<_>>(),
        // Calls granted on a Phase 2 (TDMA) channel in this window and not
        // recorded — the Phase 1 decoder refuses them. The per-frequency split
        // is on each `traffic` entry; this is the site's total, so "how much
        // Phase 2 traffic does this site carry" is one field.
        "tdma_calls_refused": s.traffic.iter().map(|t| t.tdma_refused).sum::<u64>(),
        "adjacent_sites": s
            .adjacent_sites
            .iter()
            .map(adjacent_json)
            .collect::<Vec<_>>(),
    });
    if let (Some(doc), Value::Object(extra)) = (doc.as_object_mut(), nonvoice) {
        doc.extend(extra);
    }
    doc
}

/// TSBKs per second over the reported window.
fn tsbk_rate(s: &InventorySnapshot) -> f64 {
    if s.window_ms == 0 {
        return 0.0;
    }
    s.tsbk_window as f64 * 1000.0 / s.window_ms as f64
}

fn secondary_json(c: &ObservedFrequency) -> Value {
    json!({
        "frequency_hz": c.hz,
        "channel": c.channel_label,
        "service_class": c.service_class.map(service_class_names).unwrap_or_default(),
    })
}

fn traffic_json(t: &TrafficFrequency, wall: impl Fn(u64) -> String) -> Value {
    json!({
        "frequency_hz": t.hz,
        "channel": t.channel,
        "grants": t.grants,
        "calls": t.calls,
        "tdma_refused": t.tdma_refused,
        "first_seen": wall(t.first_seen_ms),
        "last_seen": wall(t.last_seen_ms),
    })
}

fn adjacent_json(a: &AdjacentSite) -> Value {
    json!({
        "system_id": a.system_id,
        "rfss": a.rfss,
        "site": a.site,
        "lra": a.lra,
        "control_frequency_hz": a.control_hz,
        "channel": a.channel,
        "flags": {
            "conventional": a.flags.conventional,
            "failure": a.flags.site_failure,
            "valid": a.flags.valid_information,
            "rfss_connected": a.flags.active_rfss_connection,
        },
    })
}

/// The set service-class bits, by name. The raw byte stays in the decoded
/// type; the wire form is the readable list Keyup can index.
fn service_class_names(sc: ServiceClass) -> Vec<&'static str> {
    let mut names = Vec::new();
    for (set, name) in [
        (sc.composite, "composite"),
        (sc.no_service_requests, "no_service_requests"),
        (sc.backup_control, "backup_control"),
        (sc.data, "data"),
        (sc.voice, "voice"),
        (sc.registration, "registration"),
        (sc.authentication, "authentication"),
    ] {
        if set {
            names.push(name);
        }
    }
    names
}

/// Unix milliseconds as an RFC 3339 UTC instant.
pub(crate) fn rfc3339(unix_ms: i64) -> String {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(unix_ms) * 1_000_000)
        .ok()
        .and_then(|t| t.format(&Rfc3339).ok())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    // -- the event-log sink across tuner rebuilds ---------------------------

    /// 2026-09-04T05:28:50Z — the second the live event log stopped, which is
    /// the run these tests are about.
    const AT_MS: i64 = 1_788_499_730_000;

    /// A directory that removes itself; the event log needs a real one.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let mut p = std::env::temp_dir();
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            p.push(format!("trunkd-{tag}-{}-{nanos}", std::process::id()));
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

    fn one_site() -> Shared {
        Shared {
            private_calls: Default::default(),
            calls: Default::default(),
            skip: trunkd_core::SkipList::empty(),
            replay_clock: Default::default(),
            metrics: trunkd_core::Metrics::new(),
            sites: Arc::new(vec![crate::pipeline::SiteSpec {
                system_label: "Example".into(),
                site_label: "Central".into(),
                control_frequencies: vec![851_050_000],
                metric_prefix: "site.0".into(),
                display: "Example/Central".into(),
            }]),
        }
    }

    /// A control-channel event the raw log writes a line for.
    fn control_msg(at_ms: i64) -> ObsMsg {
        let mut b = trunkd_p25::bits::BitBuf::new();
        for _ in 0..96 {
            b.push(false);
        }
        b.set(0, true);
        b.set_u64(2, 6, 58); // a site broadcast: cheap, and always logged
        ObsMsg {
            site: 0,
            tuner: 0,
            control_hz: 851_050_000,
            traffic_hz: 0,
            at_ms: at_ms as u64,
            event: trunkd_p25::P25Event::Tsbk {
                tsbk: trunkd_p25::message::parse_tsbk(&b, 0),
                corrected: 0,
                crc_corrected: 0,
            },
        }
    }

    fn log_only_config(
        dir: &std::path::Path,
        state: &Arc<crate::pipeline::eventarchive::ArchiveState>,
    ) -> ObservationsConfig {
        ObservationsConfig {
            url: String::new(),
            api_key: String::new(),
            interval: Duration::from_secs(3600),
            events: EventUplinkConfig::default(),
            radio_events: false,
            hold: Arc::new(UplinkHold::new(false)),
            event_log_dir: Some(dir.to_path_buf()),
            event_log_keep_days: 0,
            event_log_state: Some(state.clone()),
            instance_id: "rec-01".into(),
            details: None,
        }
    }

    /// The panel's copy of a site's profile is published by this thread, from
    /// the same inventory the observation is built from — and publishing it
    /// must not close the window the *next* observation reports on.
    #[test]
    fn the_thread_publishes_each_site_profile_for_the_panel() {
        let dir = TempDir::new("obs-detail");
        let (tx, rx) = trunkd_core::chan::bounded::<ObsMsg>(64);
        let state = crate::pipeline::eventarchive::ArchiveState::new(false);
        let mut cfg = log_only_config(&dir.0, &state);
        let details = SiteDetails::new(1);
        cfg.details = Some(details.clone());
        let shared = one_site();
        shared
            .metrics
            .gauge("site.0.control_frequency_hz")
            .set(851_050_000);
        let thread = std::thread::spawn(move || run(rx, shared, cfg));
        tx.send(control_msg(AT_MS));
        drop(tx);
        thread.join().expect("observations thread");

        let doc = details.get(0).expect("a published profile");
        assert_eq!(doc["site_label"], "Central");
        assert_eq!(doc["system_label"], "Example");
        assert_eq!(doc["locked"], true);
        assert_eq!(doc["control_locked_hz"], 851_050_000_u64);
        assert_eq!(doc["control"]["frequency_hz"], 851_050_000_u64);
        assert!(
            doc["unknowns"].is_array(),
            "the prose list is always present"
        );
        assert_eq!(details.services_available(0), Some(None), "never heard");
        assert!(details.has(0) && !details.has(1));
    }

    /// Known, unknown and noise are *kinds*, not messages, and the AMBT census
    /// shares the histogram with the TSBK one.
    #[test]
    fn the_message_census_splits_into_named_unnamed_and_noise() {
        let kinds = message_kinds(&json!({
            "0:56": 900,          // named by the standard table
            "1:60": 4,            // MFID 1 is the standard table too
            "164:48": 30,         // the one Harris opcode the decoder parses
            "164:15": 40,         // catalogued, not understood: recurring
            "164:2": 2,           // seen twice: noise
            "7:9": 1,             // an unknown vendor, once: noise
            "ambt:0:44": 6,       // a multi-block standard message
            "nonsense": 3,
        }));
        assert_eq!(kinds.known, 4);
        assert_eq!(kinds.unknown, 1);
        assert_eq!(kinds.noise, 2);
        assert_eq!(kinds.top, vec![(164, 15, 40)]);
    }

    /// The unknown list is what an operator reads before deciding a site needs
    /// something — every line of it is a gap, not a statistic.
    #[test]
    fn the_unknown_list_names_every_gap_the_snapshot_shows() {
        let mut snap = sample_snapshot();
        snap.system_services = None;
        snap.secondary_control[0].hz = None;
        snap.bands.clear();
        let kinds = message_kinds(&json!({ "164:15": 40 }));
        let peers = [SiteIdentity {
            system_id: Some(0x1F5),
            rfss: Some(1),
            site: Some(7),
            label: "Example/Central".into(),
        }];
        let out = unknowns(&snap, &kinds, &peers);
        let has = |needle: &str| out.iter().any(|u| u.contains(needle));
        assert!(has("1F5/1-9"), "the neighbour nobody decodes: {out:?}");
        assert!(has("no resolvable frequency"), "{out:?}");
        assert!(has("services never heard"), "{out:?}");
        assert!(has("recurring message kind"), "{out:?}");
        assert!(has("band 1 is referenced"), "{out:?}");
        assert!(has("no band plan heard"), "{out:?}");

        // A site that has heard everything has nothing to report, and the
        // neighbour it decodes for itself is not a gap.
        let mut whole = sample_snapshot();
        whole.adjacent_sites[0].site = 7;
        assert_eq!(
            unknowns(&whole, &MessageKinds::default(), &peers),
            Vec::<String>::new()
        );
    }

    /// The event-log sink belongs to the observations thread, not to any one
    /// `DeviceUnit`, so it survives every way a tuner can be rebuilt: the
    /// startup build from keyup's prefetched desired state, and any number of
    /// in-place restarts after it.
    ///
    /// The regression this pins: the recorder shares one `BoundedSender`
    /// behind an `Arc` and hands the supervisor's device factory a *clone* on
    /// every build. If a rebuild were ever given its own channel — or if the
    /// receiver ended when a torn-down unit dropped its handle — the log would
    /// stop at the first restart and never come back.
    #[test]
    fn the_event_log_survives_every_tuner_rebuild() {
        let dir = TempDir::new("obs-rebuild");
        let (tx, rx) = trunkd_core::chan::bounded::<ObsMsg>(64);
        let tx = Arc::new(tx);
        let state = crate::pipeline::eventarchive::ArchiveState::new(false);
        let cfg = log_only_config(&dir.0, &state);
        let shared = one_site();
        let thread = std::thread::spawn(move || run(rx, shared, cfg));

        // Build 0 is the tuner built at startup from keyup's prefetched
        // desired state; 1 and 2 are in-place restarts applying a later push.
        // Each build takes its own handle and drops it when the unit is torn
        // down, exactly as `spawn_devices`' factory closure does.
        for build in 0..3i64 {
            let unit_tx = tx.clone();
            assert_eq!(
                unit_tx.send(control_msg(AT_MS + build)),
                trunkd_core::chan::SendOutcome::Sent
            );
            drop(unit_tx);
        }

        // Only the recorder's own handle ends the thread.
        drop(tx);
        thread.join().expect("observations thread");

        let body = std::fs::read_to_string(dir.0.join("2026-09-04.jsonl")).expect("the day's log");
        assert_eq!(
            body.lines().count(),
            3,
            "every build's events reach the log: {body}"
        );
        assert_eq!(
            state.last_written_ms(),
            AT_MS + 2,
            "the report names the newest line written"
        );
        assert!(state
            .report()
            .last_written_at
            .is_some_and(|at| at.starts_with("2026-09-04T05:28:50")));
    }

    /// Two tuners on one site each decode the same control channel. Only one
    /// tuner's copy is folded, so the log gets each event once; the other
    /// copies are counted, not written. A traffic-channel event and a clock
    /// resync are never anybody's copy.
    #[test]
    fn a_second_tuners_copy_of_the_control_channel_is_not_folded() {
        let dir = TempDir::new("obs-twotuner");
        let (tx, rx) = trunkd_core::chan::bounded::<ObsMsg>(64);
        let state = crate::pipeline::eventarchive::ArchiveState::new(false);
        let cfg = log_only_config(&dir.0, &state);
        let shared = one_site();
        let duplicates = shared.metrics.counter("site.0.control_duplicates");
        let thread = std::thread::spawn(move || run(rx, shared, cfg));

        for i in 0..5i64 {
            // Tuner 0 first, then tuner 1's copy of the same block, stamped
            // by its own clock 600 ms later.
            tx.send(control_msg(AT_MS + i * 1_000));
            let mut copy = control_msg(AT_MS + i * 1_000 + 600);
            copy.tuner = 1;
            tx.send(copy);
        }
        let mut resync = control_msg(AT_MS + 9_000);
        resync.tuner = 1;
        resync.event = trunkd_p25::P25Event::ClockResync {
            correction_ms: -1_600,
        };
        tx.send(resync);
        drop(tx);
        thread.join().expect("observations thread");

        let body = std::fs::read_to_string(dir.0.join("2026-09-04.jsonl")).expect("the day's log");
        assert_eq!(
            body.lines().count(),
            6,
            "five control events once each, plus tuner 1's own clock resync: {body}"
        );
        assert!(body.contains("CLOCK_RESYNC"), "{body}");
        assert_eq!(duplicates.get(), 5, "each of tuner 1's copies counted");
    }

    /// The kept tuner keeps the stream while it delivers; another tuner takes
    /// over only once it has been quiet for the takeover time.
    #[test]
    fn the_control_source_moves_only_when_the_kept_tuner_goes_quiet() {
        let t0 = Instant::now();
        let mut src = ControlSource::default();
        assert_eq!(
            src.admit(1, t0),
            Admit::Switched { from: None },
            "first to deliver"
        );
        assert_eq!(src.admit(0, t0 + Duration::from_millis(600)), Admit::Drop);
        assert_eq!(src.admit(1, t0 + Duration::from_secs(2)), Admit::Keep);
        // Tuner 1 is still live 2.9 s after its last event...
        assert_eq!(src.admit(0, t0 + Duration::from_millis(4_900)), Admit::Drop);
        // ...and quiet at 3 s, so tuner 0 takes over.
        assert_eq!(
            src.admit(0, t0 + Duration::from_secs(5)),
            Admit::Switched { from: Some(1) }
        );
        assert_eq!(src.admit(1, t0 + Duration::from_millis(5_100)), Admit::Drop);
        assert_eq!(src.admit(0, t0 + Duration::from_millis(5_200)), Admit::Keep);
    }

    /// With no state block (the shape tests and any embedder that does not
    /// want one) the log still writes; the field simply has nothing to say.
    #[test]
    fn the_event_log_needs_no_state_block_to_write() {
        let dir = TempDir::new("obs-nostate");
        let (tx, rx) = trunkd_core::chan::bounded::<ObsMsg>(8);
        let state = crate::pipeline::eventarchive::ArchiveState::new(false);
        let mut cfg = log_only_config(&dir.0, &state);
        cfg.event_log_state = None;
        let shared = one_site();
        let thread = std::thread::spawn(move || run(rx, shared, cfg));
        tx.send(control_msg(AT_MS));
        drop(tx);
        thread.join().expect("observations thread");
        assert_eq!(
            std::fs::read_to_string(dir.0.join("2026-09-04.jsonl"))
                .expect("the day's log")
                .lines()
                .count(),
            1
        );
        assert_eq!(state.last_written_ms(), 0);
        assert_eq!(state.report().last_written_at, None);
    }

    /// The whole `SYS_SVC_BCAST` path, from wire bytes to the posted document,
    /// rather than from a hand-built snapshot: a real 0:56 block off a test capture's
    /// control channel is folded by the same [`SiteInventory`] the observations
    /// thread owns, and its bitmap has to come out under the names Keyup reads.
    ///
    /// The 2026-09-17 three-site comparison (§7) found `services` null in all
    /// 492 windows; the recorder half of that path is what this pins.
    #[test]
    fn a_system_service_broadcast_reaches_the_observation_document() {
        // `b800040c2c680c2c6801aa30` — one of ~3,900 an hour on a busy site, taken
        // verbatim from the raw event log.
        let mut b = trunkd_p25::bits::BitBuf::new();
        for _ in 0..96 {
            b.push(false);
        }
        for (i, byte) in "b800040c2c680c2c6801aa30"
            .as_bytes()
            .chunks(2)
            .map(|p| u8::from_str_radix(std::str::from_utf8(p).expect("ascii"), 16).expect("hex"))
            .enumerate()
        {
            b.set_u64(i * 8, 8, u64::from(byte));
        }
        let event = trunkd_p25::P25Event::Tsbk {
            tsbk: trunkd_p25::message::parse_tsbk(&b, 0),
            corrected: 0,
            crc_corrected: 0,
        };

        let mut inv = SiteInventory::new(vec![851_787_500]);
        inv.observe(851_787_500, &event, 1_000);
        let body = observation_body(
            &inv.snapshot(2_000),
            "Example Network",
            "Central",
            300,
            |ms| rfc3339(ms as i64),
            "2026-09-17T00:00:00Z".into(),
            json!({}),
            "rec-01",
        );

        assert_eq!(body["services"]["available_raw"], "0x0c2c68");
        assert_eq!(body["services"]["supported_raw"], "0x0c2c68");
        assert_eq!(body["services"]["request_priority"], 1);
        assert_eq!(
            body["services"]["available"],
            json!([
                "group_voice",
                "individual_voice",
                "individual_data",
                "unit_registration",
                "group_affiliation",
                "user_status",
                "user_message",
                "user_status_query"
            ])
        );
        assert_eq!(body["services"]["supported"], body["services"]["available"]);
    }

    #[test]
    fn the_observations_endpoint_replaces_the_upload_route() {
        assert_eq!(
            observations_url("http://192.0.2.60/api/call-upload"),
            "http://192.0.2.60/api/recorder/observations"
        );
        assert_eq!(
            observations_url("http://192.0.2.60/api/call-upload/"),
            "http://192.0.2.60/api/recorder/observations"
        );
    }

    #[test]
    fn a_bare_host_gets_the_route_appended() {
        assert_eq!(
            observations_url("http://192.0.2.60"),
            "http://192.0.2.60/api/recorder/observations"
        );
    }

    #[test]
    fn service_class_bits_become_names() {
        let sc = ServiceClass::from_byte(0x21);
        assert_eq!(service_class_names(sc), vec!["composite", "voice"]);
        assert!(service_class_names(ServiceClass::from_byte(0)).is_empty());
    }

    #[test]
    fn rfc3339_renders_utc_with_a_z() {
        assert_eq!(rfc3339(1_700_000_000_123), "2023-11-14T22:13:20.123Z");
    }

    use super::sample_snapshot as snapshot;

    #[test]
    fn the_body_carries_every_documented_field() {
        let body = observation_body(
            &snapshot(),
            "Example Network",
            "Central",
            300,
            |ms| format!("t+{ms}"),
            "2026-08-31T00:00:00Z".into(),
            serde_json::json!({}),
            "rec-01",
        );

        assert_eq!(body["recorder"], "trunkd");
        // Which box heard it, alongside which software: two recorders on one
        // site are the case this field exists for.
        assert_eq!(body["instance"], "rec-01");
        assert_eq!(body["recorder_version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(body["observed_at"], "2026-08-31T00:00:00Z");
        assert_eq!(body["window_s"], 300);

        assert_eq!(body["system"]["label"], "Example Network");
        assert_eq!(body["system"]["wacn"], 0xBEE00);
        assert_eq!(body["system"]["system_id"], 0x1F5);
        assert_eq!(body["system"]["nac"], 0x293);

        assert_eq!(body["site"]["label"], "Central");
        assert_eq!(body["site"]["rfss"], 1);
        assert_eq!(body["site"]["site"], 7);
        assert_eq!(body["site"]["lra"], 3);

        // The system service broadcast (opcode 56), decoded.
        assert_eq!(body["services"]["request_priority"], 1);
        assert_eq!(body["services"]["available_raw"], "0x0c2c68");
        assert_eq!(body["services"]["supported"][0], "group_voice");

        assert_eq!(body["control"]["frequency_hz"], 851_050_000u64);
        assert_eq!(body["control"]["channel"], "1-638");
        // 900 TSBKs in a 60 s window.
        assert_eq!(body["control"]["tsbk_rate"], 15.0);

        assert_eq!(body["secondary_control"][0]["frequency_hz"], 852_137_500u64);
        assert_eq!(
            body["secondary_control"][0]["service_class"],
            json!(["backup_control", "voice"])
        );

        assert_eq!(
            body["configured_control"],
            json!([851_050_000, 852_137_500])
        );

        assert_eq!(body["bands"][0]["id"], 1);
        assert_eq!(body["bands"][0]["base_hz"], 851_006_250u64);
        assert_eq!(body["bands"][0]["spacing_hz"], 6_250);
        assert_eq!(body["bands"][0]["offset_hz"], -45_000_000);
        assert_eq!(body["bands"][0]["bandwidth_hz"], 12_500);
        assert_eq!(body["bands"][0]["tdma"], false);

        assert_eq!(body["traffic"][0]["frequency_hz"], 852_825_000u64);
        assert_eq!(body["traffic"][0]["channel"], "1-810");
        assert_eq!(body["traffic"][0]["grants"], 9);
        assert_eq!(body["traffic"][0]["calls"], 7);
        assert_eq!(body["traffic"][0]["tdma_refused"], 2);
        assert_eq!(body["tdma_calls_refused"], 2);
        assert_eq!(body["traffic"][0]["first_seen"], "t+1000");
        assert_eq!(body["traffic"][0]["last_seen"], "t+59000");

        let adj = &body["adjacent_sites"][0];
        assert_eq!(adj["system_id"], 0x1F5);
        assert_eq!(adj["rfss"], 1);
        assert_eq!(adj["site"], 9);
        assert_eq!(adj["lra"], 3);
        assert_eq!(adj["control_frequency_hz"], 853_112_500u64);
        assert_eq!(adj["channel"], "1-460");
        assert_eq!(adj["flags"]["conventional"], false);
        assert_eq!(adj["flags"]["failure"], false);
        assert_eq!(adj["flags"]["valid"], true);
        assert_eq!(adj["flags"]["rfss_connected"], true);
    }

    #[test]
    fn an_unheard_band_plan_reports_a_null_frequency_rather_than_a_wrong_one() {
        let mut s = snapshot();
        s.control.as_mut().expect("control").hz = None;
        let body = observation_body(
            &s,
            "S",
            "T",
            300,
            |ms| format!("t+{ms}"),
            String::new(),
            serde_json::json!({}),
            "rec-01",
        );
        assert!(body["control"]["frequency_hz"].is_null());
    }

    #[test]
    fn an_empty_window_reports_a_zero_rate_rather_than_dividing_by_it() {
        let mut s = snapshot();
        s.window_ms = 0;
        s.tsbk_window = 0;
        let body = observation_body(
            &s,
            "S",
            "T",
            300,
            |ms| format!("t+{ms}"),
            String::new(),
            serde_json::json!({}),
            "rec-01",
        );
        assert_eq!(body["control"]["tsbk_rate"], 0.0);
    }
}
