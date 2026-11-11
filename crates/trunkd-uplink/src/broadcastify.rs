// SPDX-License-Identifier: GPL-3.0-or-later
//! Direct delivery of a finished call to **Broadcastify Calls**, on the uplink
//! thread, immediately before the same call is pushed to Keyup.
//!
//! ```text
//!  uplink worker thread
//!  ┌───────────────────────────────────────────────────────────────┐
//!  │ encode MP3 ─▶ claim (keyup, 1.5 s) ─▶ register (BCFY, 4 s)    │
//!  │                                          └▶ PUT mp3 (6 s)     │
//!  │                          ─▶ outcome ─▶ POST call to Keyup     │
//!  └───────────────────────────────────────────────────────────────┘
//! ```
//!
//! Three properties matter and are what the shape above is for:
//!
//! * **Never on a DSP thread.** Everything here is HTTP with a timeout, run on
//!   the same worker that already spends seconds POSTing to Keyup. Worst case
//!   it adds 1.5 + 4 + 6 ≈ 12 s to one call's delivery; typical is ~1 s.
//! * **Decided once.** The verdict is written onto the [`FinishedCall`], which
//!   means it is also written into the spool sidecar. A call that waits out a
//!   Keyup outage on disk is re-POSTed with the verdict it already has, and is
//!   never registered with Broadcastify twice.
//! * **Keyup owns the switch.** The configuration arrives on the desired-state
//!   poll as `recorder.broadcastify` and is applied by `revision`, with no
//!   restart. `[uplink] direct_broadcastify = false` is the recorder-side veto:
//!   an operator who will not hold Broadcastify credentials on a given box sets
//!   it and every block keyup sends is ignored.
//!
//! The API key is never logged and never stored in an outcome: the panel and
//! the logs get `api_key_set` and the first six characters, and every response
//! string is passed through [`redact`] on the way in.
//!
//! # The Broadcastify Calls contract
//!
//! Stage 1 is an RFC 1867 multipart POST to the endpoint keyup names, whose
//! body is one line, `<code> <message>`:
//!
//! | line | outcome |
//! |---|---|
//! | `0 https://…/audio/…` | PUT the MP3 there, then `uploaded` |
//! | `1 SKIPPED---ALREADY-RECEIVED-THIS-CALL` | `duplicate` |
//! | `1 REJECTED…`, `100 NO-API-KEY-SPECIFIED`, an HTTP error, a failed PUT | `failed` |
//!
//! Stage 0 is local: a call carrying less than `min_speech_s` seconds of audio
//! above -40 dBFS (default 0.25 s — the floor Keyup classifies with) never
//! reaches the claim, let alone the wire, and is `skipped` with `no speech
//! (0.04 s)`. A mic click is not a call.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use serde_json::{json, Value};
use trunkd_call::{AudioFormat, BroadcastifyOutcome, CallKind, FinishedCall};
use trunkd_core::{Metric, Metrics};

/// Timeout for the claim POST to keyup. Short on purpose: keyup being slow must
/// not delay the call, and an unanswered claim is treated as granted.
pub const CLAIM_TIMEOUT: Duration = Duration::from_secs(1);
/// The extra half-second the claim gets; kept separate so the constant above
/// stays a whole number of seconds for `Duration::from_secs`.
const CLAIM_TIMEOUT_EXTRA: Duration = Duration::from_millis(500);
/// Timeout for the stage-1 register POST.
pub const REGISTER_TIMEOUT: Duration = Duration::from_secs(4);
/// Timeout for the stage-2 audio PUT.
pub const PUT_TIMEOUT: Duration = Duration::from_secs(6);

/// Longest response string kept in an outcome (and therefore pushed to Keyup).
const MAX_RESPONSE_LEN: usize = 500;

/// The safety margin subtracted from `skew_limit_s` before registering: a call
/// this close to the window's edge would spend its remaining budget in the
/// request itself.
const SKEW_MARGIN_S: f64 = 1.0;

/// The six characters of an API key that may be shown.
const KEY_PREFIX_LEN: usize = 6;

// ---------------------------------------------------------------------------
// The block keyup pushes
// ---------------------------------------------------------------------------

/// `recorder.broadcastify` from the desired-state document: everything needed
/// to register a call, plus the `revision` that says whether it has changed.
#[derive(Clone, Debug, PartialEq)]
pub struct BroadcastifyDirect {
    /// Whether keyup wants this recorder uploading at all.
    pub enabled: bool,
    /// The Broadcastify Calls endpoint (stage 1).
    pub endpoint_url: String,
    /// The Broadcastify API key. Never logged, never echoed.
    pub api_key: String,
    /// The Broadcastify system id, sent verbatim as `systemId`.
    pub system_id: String,
    /// Wire encoding, sent as `enc`. Only `mp3` is produced by this recorder.
    pub encoding: String,
    /// Talkgroups to upload, or `None` for "every group call".
    pub talkgroups: Option<Vec<u32>>,
    /// How stale a call may be, in seconds, before registering is pointless.
    pub skew_limit_s: f64,
    /// The least [`FinishedCall::speech_s`] a call must carry to be worth a
    /// register and a PUT. Defaults to 0.25 s when keyup does not say.
    pub min_speech_s: f32,
    /// Whether to ask keyup for a claim before registering.
    pub claim: bool,
    /// Where to ask: absolute, or a path resolved against the keyup base.
    pub claim_url: String,
    /// keyup's opaque revision of this block. A different one is applied; the
    /// same one is a redelivery and changes nothing.
    pub revision: String,
}

impl BroadcastifyDirect {
    /// Parse one `recorder.broadcastify` object.
    ///
    /// Strict where being wrong would put traffic on the wire (the endpoint and
    /// the key must be usable), forgiving everywhere else: a missing
    /// `skew_limit_s` takes the default, an unparseable talkgroup entry is
    /// dropped rather than failing the block.
    pub fn from_json(v: &Value) -> Result<BroadcastifyDirect, String> {
        let obj = v.as_object().ok_or("expected an object or null")?;
        let str_field = |k: &str| {
            obj.get(k)
                .and_then(Value::as_str)
                .map(str::trim)
                .unwrap_or_default()
                .to_owned()
        };
        let endpoint_url = str_field("endpoint_url");
        if endpoint_url.is_empty() {
            return Err("endpoint_url is required".to_owned());
        }
        if !(endpoint_url.starts_with("http://") || endpoint_url.starts_with("https://")) {
            return Err("endpoint_url must be an http(s) URL".to_owned());
        }
        let api_key = str_field("api_key");
        if api_key.is_empty() {
            return Err("api_key is required".to_owned());
        }
        let talkgroups = match obj.get("talkgroups") {
            None | Some(Value::Null) => None,
            Some(Value::Array(a)) => Some(
                a.iter()
                    .filter_map(Value::as_u64)
                    .map(|v| v as u32)
                    .collect(),
            ),
            Some(_) => return Err("talkgroups must be an array or null".to_owned()),
        };
        let encoding = match str_field("encoding").as_str() {
            "" => "mp3".to_owned(),
            other => other.to_owned(),
        };
        Ok(BroadcastifyDirect {
            enabled: obj.get("enabled").and_then(Value::as_bool).unwrap_or(false),
            endpoint_url,
            api_key,
            system_id: str_field("system_id"),
            encoding,
            talkgroups,
            skew_limit_s: obj
                .get("skew_limit_s")
                .and_then(Value::as_f64)
                .filter(|v| *v > 0.0)
                .unwrap_or(15.0),
            // The same floor Keyup classifies with (0.25 s of audio above
            // -40 dBFS). Absent means the default rather than "no gate": a
            // Keyup that has never heard of the field still wants mic clicks
            // kept off the feed.
            min_speech_s: obj
                .get("min_speech_s")
                .and_then(Value::as_f64)
                .filter(|v| v.is_finite() && *v >= 0.0)
                .unwrap_or(0.25) as f32,
            claim: obj.get("claim").and_then(Value::as_bool).unwrap_or(false),
            claim_url: match str_field("claim_url").as_str() {
                "" => "/api/recorder/broadcastify/claim".to_owned(),
                other => other.to_owned(),
            },
            revision: str_field("revision"),
        })
    }

    /// The first characters of the API key — the most that is ever shown.
    pub fn api_key_prefix(&self) -> String {
        self.api_key.chars().take(KEY_PREFIX_LEN).collect()
    }
}

// ---------------------------------------------------------------------------
// Shared state: the held block, the counters, the last thing that happened
// ---------------------------------------------------------------------------

/// The counters the panel reports, resolved once.
#[derive(Clone)]
struct Counters {
    uploaded: Metric,
    duplicate: Metric,
    failed: Metric,
    late: Metric,
    claim_denied: Metric,
    skipped: Metric,
    /// The subset of `skipped` refused by the speech gate, counted separately
    /// because "the feed is quiet because clicks are being dropped" and "the
    /// feed is quiet because keyup turned the talkgroup off" look identical
    /// from one `skipped` number.
    skipped_no_speech: Metric,
}

impl Counters {
    fn new(m: &Metrics) -> Self {
        Counters {
            uploaded: m.counter("broadcastify.uploaded"),
            duplicate: m.counter("broadcastify.duplicate"),
            failed: m.counter("broadcastify.failed"),
            late: m.counter("broadcastify.late"),
            claim_denied: m.counter("broadcastify.claim_denied"),
            skipped: m.counter("broadcastify.skipped"),
            skipped_no_speech: m.counter("broadcastify.skipped_no_speech"),
        }
    }

    fn for_outcome(&self, outcome: &str) -> Option<&Metric> {
        match outcome {
            "uploaded" => Some(&self.uploaded),
            "duplicate" => Some(&self.duplicate),
            "failed" => Some(&self.failed),
            "late" => Some(&self.late),
            "claim_denied" => Some(&self.claim_denied),
            "skipped" => Some(&self.skipped),
            _ => None,
        }
    }
}

/// The block keyup last pushed, the counters, and the last response — shared by
/// the control thread (which applies pushes), the uplink worker (which uses
/// them) and the panel (which reports them).
///
/// Built for every run whether or not the feature is ever turned on, because
/// the panel reports `enabled: false` either way and because the control thread
/// needs somewhere to put a block that arrives at any moment.
pub struct BroadcastifyState {
    /// The recorder-side master switch (`[uplink] direct_broadcastify`). When
    /// it is false every block keyup sends is refused and nothing is held.
    allowed: AtomicBool,
    block: RwLock<Option<BroadcastifyDirect>>,
    counters: Counters,
    last: Mutex<Last>,
}

#[derive(Default)]
struct Last {
    response: Option<String>,
    at_ms: Option<i64>,
}

impl std::fmt::Debug for BroadcastifyState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BroadcastifyState")
            .field("allowed", &self.allowed.load(Ordering::Relaxed))
            .field("revision", &self.revision())
            .finish_non_exhaustive()
    }
}

impl BroadcastifyState {
    /// A state block. `allowed` is `[uplink] direct_broadcastify`.
    pub fn new(allowed: bool, metrics: &Metrics) -> Arc<BroadcastifyState> {
        Arc::new(BroadcastifyState {
            allowed: AtomicBool::new(allowed),
            block: RwLock::new(None),
            counters: Counters::new(metrics),
            last: Mutex::new(Last::default()),
        })
    }

    /// Apply what keyup pushed: `None` drops whatever is held, `Some` replaces
    /// it when the revision differs. Returns whether anything moved.
    ///
    /// A block that arrives while `[uplink] direct_broadcastify = false` is
    /// refused whole — the operator, not keyup, has the last word on whether
    /// this box holds Broadcastify credentials at all.
    pub fn apply(&self, next: Option<BroadcastifyDirect>) -> bool {
        let mut held = self.block.write().unwrap_or_else(|p| p.into_inner());
        let next = match next {
            Some(b) if !self.allowed.load(Ordering::Relaxed) => {
                if held.is_some() {
                    *held = None;
                    tracing::warn!(
                        revision = %b.revision,
                        "keyup pushed a broadcastify block but [uplink] direct_broadcastify \
                         is false; dropping it"
                    );
                    return true;
                }
                tracing::debug!(
                    revision = %b.revision,
                    "ignoring keyup's broadcastify block: [uplink] direct_broadcastify is false"
                );
                return false;
            }
            other => other,
        };
        match (&*held, &next) {
            (None, None) => false,
            (Some(a), Some(b)) if a.revision == b.revision && !b.revision.is_empty() => false,
            _ => {
                match &next {
                    Some(b) => tracing::info!(
                        revision = %b.revision,
                        enabled = b.enabled,
                        endpoint = %b.endpoint_url,
                        system_id = %b.system_id,
                        api_key_set = !b.api_key.is_empty(),
                        api_key_prefix = %b.api_key_prefix(),
                        talkgroups = b.talkgroups.as_ref().map_or(-1, |t| t.len() as i64),
                        claim = b.claim,
                        skew_limit_s = b.skew_limit_s,
                        "applied keyup's broadcastify block"
                    ),
                    None => tracing::info!("keyup turned direct broadcastify off"),
                }
                *held = next;
                true
            }
        }
    }

    /// A copy of the block in force, or `None` when the feature is off.
    pub fn snapshot(&self) -> Option<BroadcastifyDirect> {
        self.block
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
            .filter(|b| b.enabled)
    }

    /// The revision in force, whether or not it is enabled.
    pub fn revision(&self) -> Option<String> {
        self.block
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
            .map(|b| b.revision.clone())
    }

    /// Whether a call would be offered to Broadcastify right now.
    pub fn enabled(&self) -> bool {
        self.snapshot().is_some()
    }

    /// Count an outcome and remember it as the last thing that happened.
    pub fn record(&self, outcome: &BroadcastifyOutcome, at_ms: i64) {
        if let Some(c) = self.counters.for_outcome(&outcome.outcome) {
            c.inc();
        }
        if is_no_speech(outcome) {
            self.counters.skipped_no_speech.inc();
        }
        let mut last = self.last.lock().unwrap_or_else(|p| p.into_inner());
        last.response = Some(match &outcome.response {
            Some(r) => format!("{}: {r}", outcome.outcome),
            None => outcome.outcome.clone(),
        });
        last.at_ms = Some(at_ms);
    }

    /// The panel's `broadcastify` object.
    pub fn status_json(&self) -> Value {
        let held = self.block.read().unwrap_or_else(|p| p.into_inner());
        let last = self.last.lock().unwrap_or_else(|p| p.into_inner());
        json!({
            "enabled": held.as_ref().is_some_and(|b| b.enabled),
            "allowed": self.allowed.load(Ordering::Relaxed),
            "revision": held.as_ref().map(|b| b.revision.clone()),
            // Never the key itself: the most an operator (or a screenshot of
            // the panel) ever sees is that one is set, and its first six
            // characters, which is enough to tell two keys apart.
            "api_key_set": held.as_ref().is_some_and(|b| !b.api_key.is_empty()),
            "api_key_prefix": held.as_ref().map(BroadcastifyDirect::api_key_prefix),
            "uploaded": self.counters.uploaded.get(),
            "duplicate": self.counters.duplicate.get(),
            "failed": self.counters.failed.get(),
            "late": self.counters.late.get(),
            "claim_denied": self.counters.claim_denied.get(),
            "skipped": self.counters.skipped.get(),
            "skipped_no_speech": self.counters.skipped_no_speech.get(),
            "last_response": last.response.clone(),
            "last_at": last.at_ms,
        })
    }
}

// ---------------------------------------------------------------------------
// The wire, behind a trait so the decisions can be tested without a socket
// ---------------------------------------------------------------------------

/// One HTTP reply: everything the decisions below look at.
#[derive(Clone, Debug)]
pub struct HttpReply {
    /// Status code.
    pub status: u16,
    /// Body, already read to a string.
    pub body: String,
}

impl HttpReply {
    /// Whether the status is 2xx.
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// The three requests this module makes. A trait so the outcome logic can be
/// exercised with no network at all; [`Agents`] is the real one.
pub trait BroadcastifyHttp: Send + Sync {
    /// The claim POST to keyup: JSON, Bearer token, [`CLAIM_TIMEOUT`].
    fn post_claim(&self, url: &str, bearer: &str, body: &str) -> Result<HttpReply, String>;
    /// The stage-1 register POST to Broadcastify, [`REGISTER_TIMEOUT`].
    fn post_form(&self, url: &str, content_type: &str, body: &[u8]) -> Result<HttpReply, String>;
    /// The stage-2 audio PUT, [`PUT_TIMEOUT`].
    fn put_audio(&self, url: &str, body: &[u8]) -> Result<HttpReply, String>;
}

/// The real HTTP: one `ureq` agent per stage, because the three stages have
/// three different timeouts and `ureq`'s timeout is agent-wide.
pub struct Agents {
    claim: ureq::Agent,
    register: ureq::Agent,
    put: ureq::Agent,
}

impl Default for Agents {
    fn default() -> Self {
        Agents::new()
    }
}

impl Agents {
    /// Build the three agents.
    pub fn new() -> Agents {
        let agent = |timeout: Duration| {
            ureq::Agent::new_with_config(
                ureq::Agent::config_builder()
                    .timeout_global(Some(timeout))
                    // Broadcastify puts the contract in the body of a 200 and
                    // an error's status is what decides the rest, so statuses
                    // must arrive as replies rather than as errors.
                    .http_status_as_error(false)
                    .user_agent("trunkd")
                    .build(),
            )
        };
        Agents {
            claim: agent(CLAIM_TIMEOUT + CLAIM_TIMEOUT_EXTRA),
            register: agent(REGISTER_TIMEOUT),
            put: agent(PUT_TIMEOUT),
        }
    }
}

/// Read one `ureq` result into an [`HttpReply`].
fn reply(r: Result<ureq::http::Response<ureq::Body>, ureq::Error>) -> Result<HttpReply, String> {
    match r {
        Ok(mut resp) => {
            let status = resp.status().as_u16();
            let body = resp.body_mut().read_to_string().unwrap_or_default();
            Ok(HttpReply { status, body })
        }
        Err(e) => Err(e.to_string()),
    }
}

impl BroadcastifyHttp for Agents {
    fn post_claim(&self, url: &str, bearer: &str, body: &str) -> Result<HttpReply, String> {
        reply(
            self.claim
                .post(url)
                .header("Authorization", &format!("Bearer {bearer}"))
                .header("Content-Type", "application/json")
                .send(body),
        )
    }

    fn post_form(&self, url: &str, content_type: &str, body: &[u8]) -> Result<HttpReply, String> {
        reply(
            self.register
                .post(url)
                .header("Content-Type", content_type)
                .send(body),
        )
    }

    fn put_audio(&self, url: &str, body: &[u8]) -> Result<HttpReply, String> {
        reply(
            self.put
                .put(url)
                .header("Content-Type", "audio/mpeg")
                .send(body),
        )
    }
}

// ---------------------------------------------------------------------------
// The decisions
// ---------------------------------------------------------------------------

/// Why a call is not offered to Broadcastify at all, or `None` when it is.
///
/// Encrypted calls have no audio, unit-to-unit calls have no talkgroup to file
/// under, and a `talkgroups` list is an allow-list.
pub fn ineligible(
    cfg: &BroadcastifyDirect,
    call: &FinishedCall,
    audio: Option<&[u8]>,
    format: AudioFormat,
) -> Option<String> {
    if call.encrypted {
        return Some("encrypted call".to_owned());
    }
    if call.call_kind == CallKind::Unit {
        return Some("unit-to-unit call".to_owned());
    }
    if let Some(list) = &cfg.talkgroups {
        if !list.contains(&call.talkgroup) {
            return Some(format!("talkgroup {} is not in the list", call.talkgroup));
        }
    }
    if !matches!(format, AudioFormat::Mp3 { .. }) {
        return Some("call audio is not mp3".to_owned());
    }
    if let None | Some([]) = audio {
        return Some("no call audio".to_owned());
    }
    // Last, because it is the only check that looks at the audio itself: a mic
    // click, or a carrier that opened and closed with nobody talking, carries
    // no frame above -40 dBFS and is not worth a register, a PUT, or a slot in
    // somebody's feed. Measured at call assembly (`speech_seconds`), so this
    // costs a float compare here.
    if call.speech_s < cfg.min_speech_s {
        return Some(format!("{NO_SPEECH}({:.2} s)", call.speech_s));
    }
    None
}

/// The prefix every speech-gate refusal's response carries, which is also how
/// [`is_no_speech`] tells one from the other reasons a call is `skipped`.
const NO_SPEECH: &str = "no speech ";

/// Whether an outcome is the speech gate's own `skipped`.
fn is_no_speech(outcome: &BroadcastifyOutcome) -> bool {
    outcome.outcome == "skipped"
        && outcome
            .response
            .as_deref()
            .is_some_and(|r| r.starts_with(NO_SPEECH))
}

/// A frequency in hertz as the MHz decimal string Broadcastify's `freq` takes:
/// `851012500` → `851.0125`.
///
/// Integer arithmetic on purpose — a float division would print 851.0124999
/// often enough to matter.
pub fn freq_mhz(hz: u64) -> String {
    let s = format!("{}.{:06}", hz / 1_000_000, hz % 1_000_000);
    let s = s.trim_end_matches('0');
    s.trim_end_matches('.').to_owned()
}

/// The stage-1 form fields, in the order they go on the wire.
pub fn register_fields(cfg: &BroadcastifyDirect, call: &FinishedCall) -> Vec<(String, String)> {
    vec![
        ("apiKey".to_owned(), cfg.api_key.clone()),
        ("systemId".to_owned(), cfg.system_id.clone()),
        (
            "callDuration".to_owned(),
            format!("{:.1}", call.duration_s()),
        ),
        (
            "ts".to_owned(),
            call.started_at_ms.div_euclid(1000).to_string(),
        ),
        // The lead talkgroup: `FinishedCall::talkgroup` is already the
        // supergroup id for a patched call, which is what the rest of the
        // pipeline files the call under.
        ("tg".to_owned(), call.talkgroup.to_string()),
        ("src".to_owned(), call.source.unwrap_or(0).to_string()),
        ("freq".to_owned(), freq_mhz(call.frequency_hz)),
        (
            "enc".to_owned(),
            match cfg.encoding.trim() {
                "" => "mp3".to_owned(),
                e => e.to_owned(),
            },
        ),
    ]
}

/// What a stage-1 response line means.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stage1 {
    /// `0 <url>`: PUT the audio there.
    Upload {
        /// The parsed code (0).
        code: i64,
        /// Where to PUT the MP3.
        url: String,
    },
    /// Broadcastify already has this call.
    Duplicate {
        /// The parsed code.
        code: i64,
    },
    /// Anything else. The line is kept as the outcome's response.
    Failed {
        /// The parsed code, when the line began with one.
        code: Option<i64>,
    },
}

/// Parse the first line of a stage-1 response.
pub fn parse_stage1(line: &str) -> Stage1 {
    let line = line.trim();
    let (code, rest) = match line.split_once(char::is_whitespace) {
        Some((c, rest)) => (c.parse::<i64>().ok(), rest.trim()),
        None => (line.parse::<i64>().ok(), ""),
    };
    if line.contains("SKIPPED---ALREADY-RECEIVED-THIS-CALL") {
        return Stage1::Duplicate {
            code: code.unwrap_or(1),
        };
    }
    match code {
        Some(0) if !rest.is_empty() => Stage1::Upload {
            code: 0,
            url: rest.to_owned(),
        },
        other => Stage1::Failed { code: other },
    }
}

/// Broadcastify's id for a stored call: the last path segment of the upload
/// URL, with any query string dropped.
pub fn remote_id(url: &str) -> Option<String> {
    let no_query = url.split(['?', '#']).next().unwrap_or(url);
    // Only the *path* has segments: a bare origin has no id in it, and
    // returning its hostname would be a plausible-looking lie.
    let path = match no_query.split_once("://") {
        Some((_, rest)) => rest.split_once('/').map(|(_, p)| p)?,
        None => no_query,
    };
    path.trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// The first line of `text`, capped at [`MAX_RESPONSE_LEN`] characters, with
/// `api_key` replaced wherever it appears.
///
/// Every string that reaches an outcome (and therefore Keyup, the logs and the
/// panel) goes through here: a server that echoes the request must not be the
/// way a key escapes the box.
pub fn redact(text: &str, api_key: &str) -> String {
    let first = text.lines().next().unwrap_or("").trim();
    let cleaned = if api_key.is_empty() {
        first.to_owned()
    } else {
        first.replace(api_key, "***")
    };
    cleaned.chars().take(MAX_RESPONSE_LEN).collect()
}

/// Resolve keyup's `claim_url` against the configured call-upload URL: an
/// absolute URL is used as it is, a path is hung off the same scheme and host.
pub fn claim_endpoint(call_upload_url: &str, claim_url: &str) -> String {
    if claim_url.starts_with("http://") || claim_url.starts_with("https://") {
        return claim_url.to_owned();
    }
    let path = if claim_url.starts_with('/') {
        claim_url.to_owned()
    } else {
        format!("/{claim_url}")
    };
    match call_upload_url.split_once("://") {
        Some((scheme, rest)) => {
            let host = rest.split('/').next().unwrap_or(rest);
            format!("{scheme}://{host}{path}")
        }
        None => path,
    }
}

/// keyup's answer to a claim.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Claim {
    /// This recorder may upload the call.
    Granted,
    /// Another instance holds it.
    Denied {
        /// Who does.
        holder: String,
    },
    /// keyup could not be reached or did not answer usefully. Treated as
    /// granted — Broadcastify deduplicates anyway — and noted on the outcome.
    Unreachable {
        /// What went wrong, short.
        reason: String,
    },
}

/// Parse a claim reply.
fn parse_claim(reply: &HttpReply) -> Claim {
    if !reply.ok() {
        return Claim::Unreachable {
            reason: format!("HTTP {}", reply.status),
        };
    }
    let Ok(v) = serde_json::from_str::<Value>(&reply.body) else {
        return Claim::Unreachable {
            reason: "unparseable claim answer".to_owned(),
        };
    };
    match v.get("granted").and_then(Value::as_bool) {
        Some(true) => Claim::Granted,
        Some(false) => Claim::Denied {
            holder: v
                .get("holder")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        },
        None => Claim::Unreachable {
            reason: "claim answer had no `granted`".to_owned(),
        },
    }
}

// ---------------------------------------------------------------------------
// The uploader
// ---------------------------------------------------------------------------

/// Everything the uplink worker needs to offer a call to Broadcastify.
pub struct BroadcastifyUploader {
    /// The block keyup pushed, the counters, the last response.
    pub state: Arc<BroadcastifyState>,
    /// How the requests are made.
    pub http: Box<dyn BroadcastifyHttp>,
    /// This recorder's id, sent on the claim.
    pub instance_id: String,
    /// The keyup upload token, sent as the claim's Bearer.
    pub keyup_token: String,
    /// The configured call-upload URL, which a relative `claim_url` hangs off.
    pub keyup_url: String,
    /// How the uplink encodes call audio; only MP3 is uploadable.
    pub format: AudioFormat,
}

impl BroadcastifyUploader {
    /// Decide, and act, for one finished call.
    ///
    /// `None` means the feature is off and the Keyup push must carry no
    /// `broadcastify*` field at all. Anything else is the verdict, already
    /// counted, ready to be attached to the call.
    ///
    /// `held` is the uplink hold (`spool_only`): a recorder an operator has
    /// deliberately taken off the air does not upload to Broadcastify either,
    /// and says so rather than staying silent about it.
    pub fn attempt(
        &self,
        call: &FinishedCall,
        audio: Option<&[u8]>,
        held: bool,
        now_ms: i64,
    ) -> Option<BroadcastifyOutcome> {
        let cfg = self.state.snapshot()?;
        let outcome = self.decide(&cfg, call, audio, held, now_ms);
        self.state.record(&outcome, now_ms);
        match outcome.outcome.as_str() {
            // Debug like Keyup's own success line (worker.rs): per call, and
            // counted on the status line; a refusal still warns.
            "uploaded" => tracing::debug!(
                call = %call.audio_stem(),
                remote_id = outcome.remote_id.as_deref().unwrap_or(""),
                "call uploaded to broadcastify"
            ),
            "failed" => tracing::warn!(
                call = %call.audio_stem(),
                response = outcome.response.as_deref().unwrap_or(""),
                "broadcastify refused the call"
            ),
            other => tracing::debug!(
                call = %call.audio_stem(),
                outcome = other,
                response = outcome.response.as_deref().unwrap_or(""),
                "broadcastify outcome"
            ),
        }
        Some(outcome)
    }

    fn decide(
        &self,
        cfg: &BroadcastifyDirect,
        call: &FinishedCall,
        audio: Option<&[u8]>,
        held: bool,
        now_ms: i64,
    ) -> BroadcastifyOutcome {
        if held {
            return BroadcastifyOutcome::with_response("skipped", "uploads held");
        }
        if let Some(reason) = ineligible(cfg, call, audio, self.format) {
            return BroadcastifyOutcome::with_response("skipped", reason);
        }
        // Checked before any traffic: a call this stale will be refused, and
        // the seconds spent finding that out are seconds the *next* call waits.
        let age_s = (now_ms - call.ended_at_ms) as f64 / 1000.0;
        if age_s > cfg.skew_limit_s - SKEW_MARGIN_S {
            return BroadcastifyOutcome::with_response(
                "late",
                format!(
                    "call ended {age_s:.1}s ago; skew limit is {:.1}s",
                    cfg.skew_limit_s
                ),
            );
        }
        let mut note = None;
        if cfg.claim {
            match self.ask_claim(cfg, call) {
                Claim::Granted => {}
                Claim::Denied { holder } => {
                    let mut out = BroadcastifyOutcome::with_response(
                        "claim_denied",
                        format!("claim held by {holder}"),
                    );
                    out.holder = Some(holder);
                    return out;
                }
                // keyup is down and the call is live: uploading a duplicate is
                // cheap (Broadcastify deduplicates) and losing the call is not.
                Claim::Unreachable { reason } => note = Some(reason),
            }
        }
        let audio = audio.unwrap_or_default();
        self.register_and_put(cfg, call, audio, note)
    }

    fn ask_claim(&self, cfg: &BroadcastifyDirect, call: &FinishedCall) -> Claim {
        let url = claim_endpoint(&self.keyup_url, &cfg.claim_url);
        let body = json!({
            "instance_id": self.instance_id,
            "tgid": call.talkgroup,
            "ts": call.started_at_ms.div_euclid(1000),
            "duration_s": call.duration_s(),
        })
        .to_string();
        match self.http.post_claim(&url, &self.keyup_token, &body) {
            Ok(reply) => parse_claim(&reply),
            Err(e) => Claim::Unreachable { reason: e },
        }
    }

    fn register_and_put(
        &self,
        cfg: &BroadcastifyDirect,
        call: &FinishedCall,
        audio: &[u8],
        note: Option<String>,
    ) -> BroadcastifyOutcome {
        let fields = register_fields(cfg, call);
        let refs: Vec<(&str, &str)> = fields
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        let (content_type, body) = crate::multipart::build_form(&refs, None);
        let reply = match self.http.post_form(&cfg.endpoint_url, &content_type, &body) {
            Ok(r) => r,
            Err(e) => return failed(None, redact(&e, &cfg.api_key), note),
        };
        let line = redact(&reply.body, &cfg.api_key);
        if !reply.ok() {
            let text = match line.is_empty() {
                true => format!("HTTP {}", reply.status),
                false => format!("HTTP {}: {line}", reply.status),
            };
            return failed(None, text, note);
        }
        match parse_stage1(&line) {
            Stage1::Duplicate { code } => {
                let mut out =
                    BroadcastifyOutcome::with_response("duplicate", with_note(line, note));
                out.code = Some(code);
                out
            }
            Stage1::Failed { code } => failed(code, line, note),
            Stage1::Upload { code, url } => match self.http.put_audio(&url, audio) {
                Ok(r) if r.ok() => {
                    let mut out = BroadcastifyOutcome::new("uploaded");
                    out.code = Some(code);
                    out.remote_id = remote_id(&url);
                    out.response = note;
                    out
                }
                Ok(r) => failed(Some(code), format!("PUT: HTTP {}", r.status), note),
                Err(e) => failed(
                    Some(code),
                    format!("PUT: {}", redact(&e, &cfg.api_key)),
                    note,
                ),
            },
        }
    }
}

/// A `failed` outcome carrying the raw line (already redacted) and, when keyup
/// could not be asked for a claim, that fact too.
fn failed(code: Option<i64>, response: String, note: Option<String>) -> BroadcastifyOutcome {
    let mut out = BroadcastifyOutcome::with_response("failed", with_note(response, note));
    out.code = code;
    out
}

/// Append "keyup could not be asked" to a response, when that is what happened.
fn with_note(response: String, note: Option<String>) -> String {
    match note {
        Some(n) => {
            let joined = format!("{response} (claim unreachable: {n})");
            joined.chars().take(MAX_RESPONSE_LEN).collect()
        }
        None => response,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;
    use trunkd_call::CallBuilder;

    /// One recorded request, so a test can assert what went on the wire.
    #[derive(Clone, Debug, PartialEq)]
    enum Req {
        Claim(String, String, String),
        Form(String, Vec<u8>),
        Put(String, usize),
    }

    /// A scripted HTTP seam: canned replies, and a log of what was asked.
    struct Fake {
        claim: Option<Result<HttpReply, String>>,
        form: Option<Result<HttpReply, String>>,
        put: Option<Result<HttpReply, String>>,
        seen: StdMutex<Vec<Req>>,
    }

    impl Fake {
        fn new() -> Fake {
            Fake {
                claim: None,
                form: None,
                put: None,
                seen: StdMutex::new(Vec::new()),
            }
        }
        fn with_form(mut self, status: u16, body: &str) -> Fake {
            self.form = Some(Ok(HttpReply {
                status,
                body: body.to_owned(),
            }));
            self
        }
        fn with_put(mut self, status: u16) -> Fake {
            self.put = Some(Ok(HttpReply {
                status,
                body: String::new(),
            }));
            self
        }
        fn with_claim(mut self, status: u16, body: &str) -> Fake {
            self.claim = Some(Ok(HttpReply {
                status,
                body: body.to_owned(),
            }));
            self
        }
        fn log(&self, r: Req) {
            self.seen.lock().expect("lock").push(r);
        }
    }

    impl BroadcastifyHttp for Arc<Fake> {
        fn post_claim(&self, url: &str, bearer: &str, body: &str) -> Result<HttpReply, String> {
            (**self).log(Req::Claim(
                url.to_owned(),
                bearer.to_owned(),
                body.to_owned(),
            ));
            self.claim
                .clone()
                .unwrap_or_else(|| Err("no claim scripted".to_owned()))
        }
        fn post_form(
            &self,
            url: &str,
            _content_type: &str,
            body: &[u8],
        ) -> Result<HttpReply, String> {
            (**self).log(Req::Form(url.to_owned(), body.to_vec()));
            self.form
                .clone()
                .unwrap_or_else(|| Err("no form scripted".to_owned()))
        }
        fn put_audio(&self, url: &str, body: &[u8]) -> Result<HttpReply, String> {
            (**self).log(Req::Put(url.to_owned(), body.len()));
            self.put
                .clone()
                .unwrap_or_else(|| Err("no put scripted".to_owned()))
        }
    }

    fn block() -> BroadcastifyDirect {
        BroadcastifyDirect {
            enabled: true,
            endpoint_url: "https://api.broadcastify.com/call-upload".into(),
            api_key: "bcfy-secret-key".into(),
            system_id: "6551".into(),
            encoding: "mp3".into(),
            talkgroups: None,
            skew_limit_s: 15.0,
            min_speech_s: 0.25,
            claim: false,
            claim_url: "/api/recorder/broadcastify/claim".into(),
            revision: "a1b2c3d4e5f6".into(),
        }
    }

    /// A two-second group call on 851.0125 MHz, started at a known second,
    /// carrying two seconds of audio well above the speech floor.
    fn sample_call() -> FinishedCall {
        let at = 1_788_012_000_000i64;
        let mut b = CallBuilder::new("Example Network", "Site 1", at, 851_012_500, 52401);
        b.talker(9_000_011, at, false, None);
        b.push_pcm(&loud(16_000));
        b.finish(at + 2000)
    }

    /// `n` samples of a full-throated square wave: ±4000 is about -18 dBFS,
    /// which is what a radio actually sounds like when somebody is talking.
    fn loud(n: usize) -> Vec<i16> {
        (0..n)
            .map(|i| if i % 2 == 0 { 4000 } else { -4000 })
            .collect()
    }

    /// The uploader plus the seam it talks to, so a test can read back what
    /// went on the wire without downcasting a trait object.
    struct Rig {
        up: BroadcastifyUploader,
        http: Arc<Fake>,
    }

    impl Rig {
        fn seen(&self) -> Vec<Req> {
            self.http.seen.lock().expect("lock").clone()
        }
    }

    fn uploader(state: Arc<BroadcastifyState>, http: Fake) -> Rig {
        let http = Arc::new(http);
        let up = BroadcastifyUploader {
            state,
            http: Box::new(http.clone()),
            instance_id: "rec-north-01".into(),
            keyup_token: "kup_test".into(),
            keyup_url: "http://192.0.2.60/api/call-upload".into(),
            format: AudioFormat::default(),
        };
        Rig { up, http }
    }

    fn state_with(block: Option<BroadcastifyDirect>) -> Arc<BroadcastifyState> {
        let s = BroadcastifyState::new(true, &Metrics::new());
        s.apply(block);
        s
    }

    /// Split a multipart body into its `(name, value)` text parts.
    fn form_fields(body: &[u8]) -> Vec<(String, String)> {
        let text = String::from_utf8_lossy(body).into_owned();
        let mut out = Vec::new();
        for chunk in text.split("\r\n--") {
            let Some((head, value)) = chunk.split_once("\r\n\r\n") else {
                continue;
            };
            let Some(name) = head
                .split("name=\"")
                .nth(1)
                .and_then(|s| s.split('"').next())
            else {
                continue;
            };
            out.push((
                name.to_owned(),
                value.trim_end_matches("\r\n").trim_end_matches("--").into(),
            ));
        }
        out
    }

    #[test]
    fn the_register_form_carries_exactly_the_broadcastify_fields() {
        let fields = register_fields(&block(), &sample_call());
        assert_eq!(
            fields,
            vec![
                ("apiKey".to_owned(), "bcfy-secret-key".to_owned()),
                ("systemId".to_owned(), "6551".to_owned()),
                ("callDuration".to_owned(), "2.0".to_owned()),
                ("ts".to_owned(), "1788012000".to_owned()),
                ("tg".to_owned(), "52401".to_owned()),
                ("src".to_owned(), "9000011".to_owned()),
                ("freq".to_owned(), "851.0125".to_owned()),
                ("enc".to_owned(), "mp3".to_owned()),
            ]
        );
    }

    #[test]
    fn a_megahertz_string_is_exact_at_the_edges() {
        assert_eq!(freq_mhz(851_012_500), "851.0125");
        assert_eq!(freq_mhz(852_137_500), "852.1375");
        assert_eq!(freq_mhz(851_000_000), "851");
        assert_eq!(freq_mhz(154_445_000), "154.445");
    }

    #[test]
    fn response_lines_map_onto_outcomes() {
        assert_eq!(
            parse_stage1("0 https://audio.broadcastify.com/upload/abc123?sig=x"),
            Stage1::Upload {
                code: 0,
                url: "https://audio.broadcastify.com/upload/abc123?sig=x".into()
            }
        );
        assert_eq!(
            parse_stage1("1 SKIPPED---ALREADY-RECEIVED-THIS-CALL"),
            Stage1::Duplicate { code: 1 }
        );
        assert_eq!(
            parse_stage1("1 REJECTED---SYSTEM-NOT-FOUND"),
            Stage1::Failed { code: Some(1) }
        );
        assert_eq!(
            parse_stage1("100 NO-API-KEY-SPECIFIED"),
            Stage1::Failed { code: Some(100) }
        );
        assert_eq!(parse_stage1("nonsense"), Stage1::Failed { code: None });
        // A bare `0` with nowhere to PUT is a failure, not an upload.
        assert_eq!(parse_stage1("0"), Stage1::Failed { code: Some(0) });
    }

    #[test]
    fn the_remote_id_is_the_last_path_segment_without_a_query() {
        assert_eq!(
            remote_id("https://audio.broadcastify.com/upload/abc123?sig=xyz"),
            Some("abc123".to_owned())
        );
        assert_eq!(
            remote_id("https://audio.broadcastify.com/upload/abc123"),
            Some("abc123".to_owned())
        );
        assert_eq!(remote_id("https://audio.broadcastify.com/"), None);
    }

    #[test]
    fn a_key_never_reaches_a_stored_string() {
        let line = redact(
            "1 REJECTED key=bcfy-secret-key\nsecond line",
            "bcfy-secret-key",
        );
        assert_eq!(line, "1 REJECTED key=***");
        assert_eq!(redact(&"x".repeat(900), "k").chars().count(), 500);
    }

    #[test]
    fn an_encrypted_or_unit_call_and_a_filtered_talkgroup_are_skipped() {
        let cfg = block();
        let at = 1_788_012_000_000i64;

        let mut b = CallBuilder::new("S", "1", at, 851_012_500, 52401);
        b.set_encryption(Some("AES-256".into()), Some(42));
        let enc = b.finish(at + 1000);
        assert_eq!(
            ineligible(&cfg, &enc, None, AudioFormat::default()).as_deref(),
            Some("encrypted call")
        );

        let mut b = CallBuilder::new("S", "1", at, 851_012_500, 0);
        b.set_unit_call(9_000_007);
        b.push_pcm(&[1i16; 8000]);
        let unit = b.finish(at + 1000);
        assert_eq!(
            ineligible(&cfg, &unit, Some(b"mp3"), AudioFormat::default()).as_deref(),
            Some("unit-to-unit call")
        );

        let filtered = BroadcastifyDirect {
            talkgroups: Some(vec![52402, 52403]),
            ..block()
        };
        assert_eq!(
            ineligible(
                &filtered,
                &sample_call(),
                Some(b"mp3"),
                AudioFormat::default()
            )
            .as_deref(),
            Some("talkgroup 52401 is not in the list")
        );
        // The same call, on the list, is eligible.
        let allowed = BroadcastifyDirect {
            talkgroups: Some(vec![52401]),
            ..block()
        };
        assert_eq!(
            ineligible(
                &allowed,
                &sample_call(),
                Some(b"mp3"),
                AudioFormat::default()
            ),
            None
        );
        // WAV is not an upload format for this endpoint.
        assert_eq!(
            ineligible(&cfg, &sample_call(), Some(b"RIFF"), AudioFormat::Wav).as_deref(),
            Some("call audio is not mp3")
        );
    }

    #[test]
    fn a_call_registers_then_puts_and_reports_its_remote_id() {
        let http = Fake::new()
            .with_form(200, "0 https://audio.broadcastify.com/upload/abc123?sig=x")
            .with_put(200);
        let rig = uploader(state_with(Some(block())), http);
        let call = sample_call();
        let out = rig
            .up
            .attempt(&call, Some(b"ID3-MP3-BYTES"), false, call.ended_at_ms + 500)
            .expect("the feature is on");

        assert_eq!(out.outcome, "uploaded");
        assert_eq!(out.code, Some(0));
        assert_eq!(out.remote_id.as_deref(), Some("abc123"));
        assert_eq!(out.response, None);
        assert_eq!(rig.up.state.status_json()["uploaded"], 1);

        // And the body it registered with is the documented field set.
        let seen = rig.seen();
        let Req::Form(url, body) = &seen[0] else {
            panic!("expected a form POST first, got {seen:?}");
        };
        assert_eq!(url, "https://api.broadcastify.com/call-upload");
        assert_eq!(
            form_fields(body),
            register_fields(&block(), &call),
            "the wire body is the documented field set"
        );
        assert_eq!(
            seen[1],
            Req::Put(
                "https://audio.broadcastify.com/upload/abc123?sig=x".to_owned(),
                13
            )
        );
    }

    #[test]
    fn a_duplicate_and_a_rejection_are_told_apart() {
        let rig = uploader(
            state_with(Some(block())),
            Fake::new().with_form(200, "1 SKIPPED---ALREADY-RECEIVED-THIS-CALL"),
        );
        let call = sample_call();
        let out = rig
            .up
            .attempt(&call, Some(b"MP3"), false, call.ended_at_ms)
            .expect("on");
        assert_eq!(out.outcome, "duplicate");
        assert_eq!(out.code, Some(1));
        assert_eq!(
            out.response.as_deref(),
            Some("1 SKIPPED---ALREADY-RECEIVED-THIS-CALL")
        );

        let rig = uploader(
            state_with(Some(block())),
            Fake::new().with_form(200, "100 NO-API-KEY-SPECIFIED"),
        );
        let out = rig
            .up
            .attempt(&call, Some(b"MP3"), false, call.ended_at_ms)
            .expect("on");
        assert_eq!(out.outcome, "failed");
        assert_eq!(out.code, Some(100));
        assert_eq!(out.response.as_deref(), Some("100 NO-API-KEY-SPECIFIED"));
        assert_eq!(rig.up.state.status_json()["failed"], 1);
    }

    #[test]
    fn a_failed_put_fails_the_call_rather_than_claiming_an_upload() {
        let rig = uploader(
            state_with(Some(block())),
            Fake::new()
                .with_form(200, "0 https://audio.broadcastify.com/upload/abc123")
                .with_put(500),
        );
        let call = sample_call();
        let out = rig
            .up
            .attempt(&call, Some(b"MP3"), false, call.ended_at_ms)
            .expect("on");
        assert_eq!(out.outcome, "failed");
        assert_eq!(out.response.as_deref(), Some("PUT: HTTP 500"));
        assert_eq!(out.remote_id, None);
    }

    #[test]
    fn a_stale_call_is_late_and_opens_no_socket() {
        let rig = uploader(state_with(Some(block())), Fake::new());
        let call = sample_call();
        // 14.1 s after the call ended, against a 15 s limit and a 1 s margin.
        let out = rig
            .up
            .attempt(&call, Some(b"MP3"), false, call.ended_at_ms + 14_100)
            .expect("on");
        assert_eq!(out.outcome, "late");
        assert!(
            rig.seen().is_empty(),
            "nothing goes on the wire for a late call"
        );
        // A second inside the margin still goes.
        let rig = uploader(
            state_with(Some(block())),
            Fake::new().with_form(200, "1 REJECTED"),
        );
        let out = rig
            .up
            .attempt(&call, Some(b"MP3"), false, call.ended_at_ms + 13_000)
            .expect("on");
        assert_eq!(out.outcome, "failed");
    }

    #[test]
    fn a_denied_claim_records_the_holder_and_sends_nothing_to_broadcastify() {
        let cfg = BroadcastifyDirect {
            claim: true,
            ..block()
        };
        let rig = uploader(
            state_with(Some(cfg)),
            Fake::new().with_claim(
                200,
                r#"{"granted":false,"holder":"rec-south-02","claim_id":7}"#,
            ),
        );
        let call = sample_call();
        let out = rig
            .up
            .attempt(&call, Some(b"MP3"), false, call.ended_at_ms)
            .expect("on");
        assert_eq!(out.outcome, "claim_denied");
        assert_eq!(out.holder.as_deref(), Some("rec-south-02"));

        let seen = rig.seen();
        assert_eq!(seen.len(), 1, "only the claim: {seen:?}");
        let Req::Claim(url, bearer, body) = &seen[0] else {
            panic!("expected a claim");
        };
        assert_eq!(url, "http://192.0.2.60/api/recorder/broadcastify/claim");
        assert_eq!(bearer, "kup_test");
        let v: Value = serde_json::from_str(body).expect("json");
        assert_eq!(v["instance_id"], "rec-north-01");
        assert_eq!(v["tgid"], 52401);
        assert_eq!(v["ts"], 1_788_012_000i64);
        assert_eq!(v["duration_s"], 2.0);
    }

    #[test]
    fn an_unreachable_keyup_uploads_anyway_and_says_so() {
        let cfg = BroadcastifyDirect {
            claim: true,
            ..block()
        };
        let rig = uploader(
            state_with(Some(cfg)),
            // No claim scripted: the seam answers with a transport error.
            Fake::new()
                .with_form(200, "0 https://audio.broadcastify.com/upload/zz9")
                .with_put(200),
        );
        let call = sample_call();
        let out = rig
            .up
            .attempt(&call, Some(b"MP3"), false, call.ended_at_ms)
            .expect("on");
        assert_eq!(out.outcome, "uploaded");
        assert_eq!(out.remote_id.as_deref(), Some("zz9"));
        assert_eq!(out.response.as_deref(), Some("no claim scripted"));
    }

    #[test]
    fn a_held_uplink_uploads_nothing() {
        let rig = uploader(state_with(Some(block())), Fake::new());
        let call = sample_call();
        let out = rig
            .up
            .attempt(&call, Some(b"MP3"), true, call.ended_at_ms)
            .expect("on");
        assert_eq!(out.outcome, "skipped");
        assert_eq!(out.response.as_deref(), Some("uploads held"));
    }

    #[test]
    fn the_feature_being_off_decides_nothing_at_all() {
        let rig = uploader(state_with(None), Fake::new());
        let call = sample_call();
        assert_eq!(
            rig.up.attempt(&call, Some(b"MP3"), false, call.ended_at_ms),
            None
        );

        // Present but disabled is the same silence.
        let rig = uploader(
            state_with(Some(BroadcastifyDirect {
                enabled: false,
                ..block()
            })),
            Fake::new(),
        );
        assert_eq!(
            rig.up.attempt(&call, Some(b"MP3"), false, call.ended_at_ms),
            None
        );
    }

    #[test]
    fn a_block_applies_on_a_new_revision_and_a_redelivery_changes_nothing() {
        let state = BroadcastifyState::new(true, &Metrics::new());
        assert!(state.apply(Some(block())), "first push applies");
        assert_eq!(state.revision().as_deref(), Some("a1b2c3d4e5f6"));
        assert!(!state.apply(Some(block())), "the same revision is a no-op");

        let next = BroadcastifyDirect {
            revision: "ffffffffffff".into(),
            talkgroups: Some(vec![52401]),
            ..block()
        };
        assert!(state.apply(Some(next)), "a new revision applies");
        assert_eq!(
            state.snapshot().expect("held").talkgroups,
            Some(vec![52401])
        );

        assert!(state.apply(None), "null drops the block");
        assert!(!state.enabled());
        assert_eq!(state.revision(), None);
        assert!(!state.apply(None), "and dropping twice changes nothing");
    }

    #[test]
    fn the_operators_switch_beats_keyups() {
        let state = BroadcastifyState::new(false, &Metrics::new());
        assert!(!state.apply(Some(block())), "refused whole");
        assert!(!state.enabled());
        assert_eq!(state.status_json()["allowed"], false);
    }

    #[test]
    fn the_block_parses_and_a_useless_one_is_refused() {
        let v: Value = serde_json::from_str(
            r#"{"enabled":true,"endpoint_url":"https://api.broadcastify.com/call-upload",
                "api_key":"k","system_id":"6551","encoding":"mp3","talkgroups":[52401,52402],
                "skew_limit_s":15.0,"claim":true,
                "claim_url":"/api/recorder/broadcastify/claim","revision":"a1b2c3d4e5f6"}"#,
        )
        .expect("json");
        let b = BroadcastifyDirect::from_json(&v).expect("parses");
        assert!(b.enabled);
        assert_eq!(b.system_id, "6551");
        assert_eq!(b.talkgroups, Some(vec![52401, 52402]));
        assert_eq!(b.skew_limit_s, 15.0);
        assert!(b.claim);
        assert_eq!(b.revision, "a1b2c3d4e5f6");

        // `talkgroups: null` is "every group call", not "no talkgroups".
        let v: Value = serde_json::from_str(
            r#"{"enabled":true,"endpoint_url":"https://x/y","api_key":"k","talkgroups":null}"#,
        )
        .expect("json");
        let b = BroadcastifyDirect::from_json(&v).expect("parses");
        assert_eq!(b.talkgroups, None);
        assert_eq!(b.skew_limit_s, 15.0, "the default skew limit");

        for bad in [
            r#"{"enabled":true,"api_key":"k"}"#,
            r#"{"enabled":true,"endpoint_url":"ftp://x/y","api_key":"k"}"#,
            r#"{"enabled":true,"endpoint_url":"https://x/y"}"#,
        ] {
            let v: Value = serde_json::from_str(bad).expect("json");
            assert!(
                BroadcastifyDirect::from_json(&v).is_err(),
                "should refuse {bad}"
            );
        }
    }

    #[test]
    fn a_click_is_refused_before_any_traffic_and_a_talk_is_not() {
        let cfg = block();
        let at = 1_788_012_000_000i64;

        // 30 ms of click in an otherwise silent half-second: 0.04 s of speech,
        // below the 0.25 s floor.
        let mut pcm = vec![0i16; 4000];
        pcm[..240].copy_from_slice(&loud(240));
        let mut b = CallBuilder::new("S", "1", at, 851_012_500, 52401);
        b.talker(9_000_011, at, false, None);
        b.push_pcm(&pcm);
        let click = b.finish(at + 500);
        assert_eq!(
            ineligible(&cfg, &click, Some(b"mp3"), AudioFormat::default()).as_deref(),
            Some("no speech (0.04 s)")
        );

        // Two seconds of somebody talking passes.
        assert_eq!(
            ineligible(&cfg, &sample_call(), Some(b"mp3"), AudioFormat::default()),
            None
        );

        // And the refusal never opens a socket: no claim, no register, no PUT.
        let rig = uploader(
            state_with(Some(BroadcastifyDirect {
                claim: true,
                ..block()
            })),
            Fake::new(),
        );
        let out = rig
            .up
            .attempt(&click, Some(b"mp3"), false, at + 1000)
            .expect("the feature is on");
        assert_eq!(out.outcome, "skipped");
        assert_eq!(out.response.as_deref(), Some("no speech (0.04 s)"));
        assert!(rig.seen().is_empty(), "sent {:?}", rig.seen());
        assert_eq!(rig.up.state.status_json()["skipped"], 1);
        assert_eq!(rig.up.state.status_json()["skipped_no_speech"], 1);

        // A talkgroup that is simply not on the list is `skipped` too, but it
        // is not the speech gate and must not move that counter.
        let held = BroadcastifyDirect {
            talkgroups: Some(vec![52402]),
            ..block()
        };
        let rig = uploader(state_with(Some(held)), Fake::new());
        rig.up
            .attempt(&sample_call(), Some(b"mp3"), false, at + 1000)
            .expect("the feature is on");
        assert_eq!(rig.up.state.status_json()["skipped"], 1);
        assert_eq!(rig.up.state.status_json()["skipped_no_speech"], 0);
    }

    #[test]
    fn keyup_can_move_the_speech_floor_and_its_absence_is_the_default() {
        let v: Value = serde_json::from_str(
            r#"{"enabled":true,"endpoint_url":"https://x/y","api_key":"k","min_speech_s":1.5}"#,
        )
        .expect("json");
        assert_eq!(
            BroadcastifyDirect::from_json(&v)
                .expect("parses")
                .min_speech_s,
            1.5
        );

        // Absent, null and negative all take the default: a Keyup that has
        // never heard of the field still gets the gate.
        for text in [
            r#"{"enabled":true,"endpoint_url":"https://x/y","api_key":"k"}"#,
            r#"{"enabled":true,"endpoint_url":"https://x/y","api_key":"k","min_speech_s":null}"#,
            r#"{"enabled":true,"endpoint_url":"https://x/y","api_key":"k","min_speech_s":-1}"#,
        ] {
            let v: Value = serde_json::from_str(text).expect("json");
            assert_eq!(
                BroadcastifyDirect::from_json(&v)
                    .expect("parses")
                    .min_speech_s,
                0.25,
                "for {text}"
            );
        }

        // Zero is a real value — "upload everything" — not a missing field.
        let v: Value = serde_json::from_str(
            r#"{"enabled":true,"endpoint_url":"https://x/y","api_key":"k","min_speech_s":0}"#,
        )
        .expect("json");
        let cfg = BroadcastifyDirect::from_json(&v).expect("parses");
        assert_eq!(cfg.min_speech_s, 0.0);
        let silent = CallBuilder::new("S", "1", 1_788_012_000_000, 851_012_500, 52401)
            .finish(1_788_012_000_500);
        assert_eq!(
            ineligible(&cfg, &silent, Some(b"mp3"), AudioFormat::default()),
            None,
            "a zero floor lets even silence through"
        );
    }

    #[test]
    fn a_relative_claim_url_hangs_off_the_keyup_host() {
        assert_eq!(
            claim_endpoint("http://192.0.2.60/api/call-upload", "/api/recorder/x"),
            "http://192.0.2.60/api/recorder/x"
        );
        assert_eq!(
            claim_endpoint("https://keyup.example/api/call-upload", "api/recorder/x"),
            "https://keyup.example/api/recorder/x"
        );
        assert_eq!(
            claim_endpoint("http://192.0.2.60/api/call-upload", "https://other/claim"),
            "https://other/claim"
        );
    }
}
