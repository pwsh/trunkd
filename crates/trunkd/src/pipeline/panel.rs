// SPDX-License-Identifier: GPL-3.0-or-later
//! The local control panel: a small embedded HTTP server on the recorder box.
//!
//! trunkd normally runs headless on a machine that is not keyup, which makes
//! "what is this instance actually doing right now?" a question you can only
//! answer by reading a log over SSH. This module answers it in a browser: one
//! page, one status endpoint, one redacted config dump, and two mutations —
//! asking a single tuner to restart cleanly, and holding or releasing the
//! uplink.
//!
//! # Read-mostly, deliberately
//!
//! Config editing stays where it is: TOML on disk (restart to apply) and
//! keyup's desired-state (apply by restarting one `DeviceUnit`). The panel does
//! not write config, because two writers of the same state is how a recorder
//! ends up running something nobody can point at. The two things it *can* do
//! are both runtime switches rather than config:
//!
//! * **restart a tuner** — reuses the desired-state generation counter, so it
//!   is the same clean rebuild keyup's apply path takes, spends no restart
//!   budget, and changes no parameters.
//! * **hold or release the uplink** (`POST /api/uplink`) — the same shared
//!   [`UplinkHold`](trunkd_uplink::UplinkHold) `[uplink] spool_only` seeds and
//!   keyup's desired state can push. Most recent writer wins, and the status
//!   body names it, so an operator can always see whether the box or keyup put
//!   the recorder where it is.
//!
//! # Shape
//!
//! Plain threads, no async runtime, matching the rest of the recorder (the
//! supervisor's own docs call its threads "deliberately plain"). The accept
//! loop runs on the `trunkd-http` thread with its own stop flag; each
//! connection is read and answered on a short-lived thread of its own, under
//! the deadlines and the connection cap in [`panelhttp`](super::panelhttp),
//! which also explains why that transport replaced `tiny_http`'s server (whose
//! request and response types the routes still use). Routing itself is
//! serialized — one route runs at a time, exactly as when the panel had one
//! thread — so no handler has to think about another; only the socket I/O
//! runs in parallel, which is the part a slow client can stall.
//!
//! Every request's `Host` must name this panel (see [`OriginPolicy`]) — the
//! DNS-rebinding guard for reads — and every mutation's `Origin`, when a
//! browser sends one, must be the panel's own page. A route that takes a JSON
//! body takes only `Content-Type: application/json`.
//!
//! Nothing here is load-bearing. A bind failure logs an error and ends the
//! thread; the recorder carries on recording, which is the whole point of it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use serde_json::{json, Value};
use tiny_http::{Header, Method, Response};

use trunkd_supervisor::{ComponentState, UnitStatus};

use crate::config::{Config, Station};
use crate::pipeline::control::{
    gather_counters, gather_tuner_status, status_body, uplink_report, DesiredState,
    RecorderControl, TunerDescriptor, MAX_TUNERS,
};
use crate::pipeline::panelhttp::{self, Conn, Limits, Request};
use crate::pipeline::Shared;

/// The single-file page served at `/`. Embedded rather than read from disk so a
/// deployed binary is the whole panel — no asset directory to install, and
/// nothing to fetch from a CDN on a box with no route out.
const INDEX_HTML: &str = include_str!("panel.html");

/// Everything the panel reads. All of it is either `Arc`-backed or a cheap
/// clone taken once at startup, so no handler ever blocks a DSP thread.
pub(crate) struct PanelState {
    /// Metrics registry and the site table.
    pub shared: Shared,
    /// Static per-tuner facts, the same set the keyup status POST reports.
    pub descriptors: Vec<TunerDescriptor>,
    /// The desired-state registry: read for applied config, written only by the
    /// restart route (a generation bump).
    pub desired: Arc<DesiredState>,
    /// Read-only views of the supervised device units.
    pub units: Vec<UnitStatus>,
    /// Accepted bearer tokens; empty means "reads open, mutations loopback".
    pub tokens: Vec<String>,
    /// Which browser `Origin`s may drive a mutation: the panel's own.
    pub origins: OriginPolicy,
    /// This instance's resolved id.
    pub instance_id: String,
    /// The operator's `[station]` block, reported verbatim as keyup gets it.
    pub station: Station,
    /// The loaded config with its secrets already replaced, serialized once at
    /// startup: config is immutable for the life of the *run*, so re-deriving
    /// it per request would only add a way to leak a key later.
    pub config_json: Value,
    /// The same redacted config, still typed. The device routes answer from the
    /// file on disk when there is one; this is what they fall back to when the
    /// config came from the environment and there is no file to read.
    pub config: Arc<Config>,
    /// The `trunkd.toml` this process loaded, when it loaded one at all. `None`
    /// for an env-only or in-process run, and the whole reason the config
    /// routes can answer "not writable" instead of guessing at a path.
    pub config_path: Option<std::path::PathBuf>,
    /// Set by `POST /api/config/reload`: ends the run so the CLI can build the
    /// pipeline again from the re-read file, in the same process.
    pub reload: Arc<AtomicBool>,
    /// Whether these routes have written the config file since this run's
    /// config was loaded — i.e. whether what is on disk is ahead of what is
    /// recording. Cleared by a reload, because the flag lives for one run.
    pub restart_pending: Arc<AtomicBool>,
    /// How the discover route finds attached hardware. Injected so a test can
    /// assert the route's shape without an SDR plugged into the build machine;
    /// in production it is [`trunkd_sdr::enumerate_all`], which asks SoapySDR
    /// to *list* devices and never opens one.
    pub enumerate: Arc<dyn Fn() -> Vec<trunkd_sdr::DeviceDescriptor> + Send + Sync>,
    /// The recorder-wide control block: the uplink hold, and the desired-state
    /// version keyup last set it from.
    pub recorder: Arc<RecorderControl>,
    /// The configured call-upload endpoint, reported alongside the hold.
    pub upload_url: String,
    /// When the recorder started, for `uptime_s`.
    pub started: Instant,
    /// The recorder link, in the slot keyup can rebuild it in.
    pub link: std::sync::Arc<crate::pipeline::link::LinkSlot>,
    /// The event-log archive's live state, reported exactly as keyup gets it.
    pub event_log: Arc<crate::pipeline::eventarchive::ArchiveState>,
    /// Build, dependency, library and host provenance — everything `/about`
    /// and `/api/about` serve. See [`crate::about`].
    pub about: Arc<crate::about::About>,
    /// The keyup push channel's live state, reported inside `uplink.stream`.
    pub stream: Arc<crate::pipeline::stream::StreamState>,
    /// The live-audio fan-out, when `[server] live_audio` built one. `None`
    /// is the default and is what the monitor routes answer 404 from.
    pub monitor: Option<Arc<crate::pipeline::monitor::MonitorHub>>,
    /// Per-site observation documents, republished by the observations thread.
    /// The panel reads the last one written and never touches a live
    /// `SiteInventory`: the fold runs per TSBK and answers to nobody's HTTP
    /// request. See [`crate::pipeline::observations::SiteDetails`].
    pub site_details: Arc<crate::pipeline::observations::SiteDetails>,
}

impl PanelState {
    /// Snapshot the whole instance as the panel's `/api/status` body.
    ///
    /// The tuner objects are built by the *same* builders the keyup status POST
    /// uses ([`status_body`], [`gather_tuner_status`], [`gather_counters`]), so
    /// the panel can never drift into showing a different truth than keyup; the
    /// panel-only extras (uptime, supervisor health, per-site detail, and the
    /// per-tuner `pending` flag) are added on top.
    fn status(&self) -> Value {
        // The LRRP location gate counts its rejections in process-global
        // atomics (it is reached from free functions on the observations
        // thread, with no registry in hand), so they are copied into the
        // registry at the moment the registry is read.
        super::lrrp_gate::publish_metrics(&self.shared.metrics);
        let snap = self.shared.metrics.snapshot();
        let now_ms = trunkd_core::Timestamp::now().unix_millis();
        let tuners: Vec<_> = self
            .descriptors
            .iter()
            .take(MAX_TUNERS)
            .map(|d| gather_tuner_status(&snap, d, self.desired.applied(&d.id).as_ref(), now_ms))
            .collect();
        let counters = gather_counters(&snap);
        let uplink = uplink_report(&counters, &self.recorder, &self.upload_url, &self.stream);
        let mut body = status_body(
            &self.instance_id,
            &self.station,
            &counters,
            &tuners,
            &uplink,
            self.recorder.applied(),
            &self.event_log.report(),
        );
        crate::pipeline::control::insert_link(&mut body, &self.link);
        let obj = body
            .as_object_mut()
            .expect("status_body returns an object literal");

        obj.insert(
            "uptime_s".to_owned(),
            json!(self.started.elapsed().as_secs()),
        );
        obj.insert(
            "supervisor".to_owned(),
            Value::Array(
                self.units
                    .iter()
                    .map(|u| {
                        let state = u.state();
                        let mut unit = json!({
                            "name": u.name(),
                            "state": state_str(state),
                            "restart_count": u.restart_count(),
                            "heartbeat_age_ms": u.heartbeat_age_ms(),
                        });
                        // Why it is down and when it will be tried again —
                        // only while it is, so a healthy unit does not carry
                        // a stale error from an earlier restart.
                        if matches!(
                            state,
                            ComponentState::Restarting
                                | ComponentState::Retrying
                                | ComponentState::Failed
                        ) {
                            unit["next_attempt_ms"] = json!(u.next_attempt_ms());
                            unit["last_error"] = json!(u.last_error());
                        }
                        unit
                    })
                    .collect(),
            ),
        );
        obj.insert("sites".to_owned(), Value::Array(self.sites(&snap)));
        // Direct-to-Broadcastify: whether keyup has turned it on, which
        // revision is in force, and how the calls since boot have gone. The key
        // is never here — only `api_key_set` and its first six characters.
        obj.insert(
            "broadcastify".to_owned(),
            self.recorder.broadcastify().status_json(),
        );
        // How many people are listening to this recorder right now. Always
        // present (0 when the monitor is off) so the page can tell "nobody is
        // listening" from "this build cannot be listened to" by reading
        // `monitor.enabled` rather than guessing from an absent key.
        obj.insert("monitor".to_owned(), self.monitor_json());
        obj.insert(
            "monitor_streams".to_owned(),
            json!(self.monitor.as_ref().map_or(0, |m| m.streams())),
        );
        // Two facts the main page needs to raise the "saved, not yet applied"
        // banner without a second request: where the file is, and whether the
        // panel has written it since this run started.
        obj.insert("config_path".to_owned(), config_path_json(self));
        obj.insert(
            "restart_pending".to_owned(),
            json!(self.restart_pending.load(Ordering::Relaxed)),
        );
        // The recorder-wide settings as they are right now, with the source of
        // each: the panel shows what is in effect, not only what keyup asked
        // for. The link is displayed, never edited here — keyup is the editor
        // for it, and two writers of one setting is how a recorder ends up
        // disagreeing with itself.
        obj.insert("recorder".to_owned(), self.recorder.effective_json());
        // The private calls this recorder is on, or has just been on. A group
        // call shows up as a talkgroup on the sites table; a unit-to-unit call
        // has no talkgroup to show up under, so without this the panel could
        // say only how many there had been.
        obj.insert("private_calls".to_owned(), self.private_calls_json());
        // Everything on the air and everything just off it, so the page can
        // answer "what is coming through right now" without an operator
        // reading the log. The private-call list above is unchanged: it is a
        // different question (which radios called which) and keeps its shape.
        obj.insert("calls".to_owned(), self.calls_json());
        // The conventional channels table: every `[[channel]]` this recorder
        // runs, flattened off the tuner entries (where the keyup POST carries
        // them) with the tuner's key added as `device`. Always present, empty
        // on a P25-only recorder, which the page reads as "no table at all".
        obj.insert("channels".to_owned(), channels_json(&tuners));

        // Whether a staged config has yet to be confirmed by the running unit —
        // the same "pending" keyup shows, computed here so the panel does not
        // have to ask keyup what it thinks of a tuner sitting a metre away.
        if let Some(Value::Array(tuners)) = obj.get_mut("tuners") {
            for t in tuners.iter_mut() {
                let pending = t
                    .get("tuner_key")
                    .and_then(Value::as_str)
                    .map(|k| self.desired.is_pending(k))
                    .unwrap_or(false);
                if let Some(map) = t.as_object_mut() {
                    map.insert("pending".to_owned(), json!(pending));
                }
            }
        }
        body
    }

    /// The live monitor's own state: whether this build is serving audio at
    /// all, how many streams are open, and the ceiling.
    fn monitor_json(&self) -> Value {
        match &self.monitor {
            Some(hub) => json!({
                "enabled": true,
                "streams": hub.streams(),
                "max_streams": crate::pipeline::monitor::MAX_STREAMS,
                "bitrate_kbps": crate::pipeline::monitor::BITRATE_KBPS,
                "dropped_frames": hub.dropped(),
            }),
            None => json!({
                "enabled": false,
                "streams": 0,
                "max_streams": crate::pipeline::monitor::MAX_STREAMS,
            }),
        }
    }

    /// The private-call window as JSON, newest first: `A -> B`, the channel,
    /// and whether the call is still on the air.
    fn private_calls_json(&self) -> Value {
        Value::Array(
            self.shared
                .private_calls
                .rows()
                .into_iter()
                .map(|r| {
                    json!({
                        "source": r.source,
                        "target": r.target,
                        "frequency_hz": r.frequency_hz,
                        "started_at_ms": r.started_at_ms,
                        "ended_at_ms": r.ended_at_ms,
                        "active": r.ended_at_ms.is_none(),
                    })
                })
                .collect(),
        )
    }

    /// The Activity view's two lists: the calls on the air, and the last few
    /// that have finished (newest first) — plus the call thread's two health
    /// counters: messages its queue had to evict, and calls it closed itself
    /// because their end was among them.
    fn calls_json(&self) -> Value {
        let metrics = &self.shared.metrics;
        json!({
            "active": Value::Array(
                self.shared.calls.active().iter().map(call_row_json).collect(),
            ),
            "recent": Value::Array(
                self.shared.calls.recent().iter().map(call_row_json).collect(),
            ),
            "queue_dropped": metrics.counter(super::calls::QUEUE_DROPPED).get(),
            "orphaned": metrics.counter(super::calls::ORPHANED).get(),
        })
    }

    /// Just the `uplink` object, for the route that toggles it: the same shape
    /// `/api/status` and the keyup status POST carry.
    fn uplink_json(&self) -> Value {
        let snap = self.shared.metrics.snapshot();
        let counters = gather_counters(&snap);
        uplink_report(&counters, &self.recorder, &self.upload_url, &self.stream).to_json()
    }

    /// Per-site detail from the `site.N.*` gauges the DSP threads publish.
    fn sites(&self, snap: &[trunkd_core::MetricSnapshot]) -> Vec<Value> {
        let get = |name: &str| snap.iter().find(|m| m.name == name).map(|m| m.value);
        let num = |name: &str| get(name).unwrap_or(0);
        let float = |name: &str| get(name).map(f64::from_bits).unwrap_or(0.0);
        self.shared
            .sites
            .iter()
            .enumerate()
            .map(|(i, site)| {
                let p = &site.metric_prefix;
                json!({
                    "index": i,
                    "label": site.display,
                    "control_frequency_hz": num(&format!("{p}.control_frequency_hz")),
                    "nac": num(&format!("{p}.nac")),
                    "tsbk_rate": float(&format!("{p}.tsbk_rate")),
                    "active_calls": num(&format!("{p}.active_calls")),
                    "pool_in_use": num(&format!("{p}.pool_in_use")),
                    "pool_size": num(&format!("{p}.pool_size")),
                    "control_frequency_error_hz": float(&format!("{p}.control_frequency_error_hz")),
                    "calls_started": num(&format!("{p}.engine_calls_started")),
                    "calls_ended": num(&format!("{p}.engine_calls_ended")),
                    // Calls granted on a Phase 2 (TDMA) channel and refused:
                    // traffic this site carries that the recorder cannot
                    // archive yet.
                    "calls_tdma_refused": num(&format!(
                        "{p}.{}",
                        crate::pipeline::device::SITE_TDMA_REFUSED
                    )),
                    "sync_losses": num(&format!("{p}.sync_losses")),
                    // The one line of the site's profile worth showing before
                    // it is expanded: `null` until the first observation
                    // document exists or while the site has never announced its
                    // services. `/api/sites/{i}/detail` has the rest.
                    "services_available": self
                        .site_details
                        .services_available(i)
                        .flatten(),
                })
            })
            .collect()
    }
}

/// The config file path as JSON, or `null` when there is none.
///
/// Forward slashes even on Windows: this string is read by a browser, where a
/// native path is a pile of escapes, and every consumer of it either displays
/// it or hands it back to a human. One spelling, so `/api/status` and
/// `/api/devices/discover` cannot disagree about the same file.
/// One Activity row as JSON. The same shape for an active and a finished call:
/// the end fields are null while it is on the air, so the page's two tables
/// share one renderer and a row does not change shape under it.
fn call_row_json(r: &crate::pipeline::CallRow) -> Value {
    json!({
        "id": r.id,
        "site": r.site,
        "kind": r.kind,
        "talkgroup": r.talkgroup,
        "alias": r.alias,
        "source": r.source,
        "target": r.target,
        "frequency_hz": r.frequency_hz,
        "encrypted": r.encrypted,
        "started_at_ms": r.started_at_ms,
        "ended_at_ms": r.ended_at_ms,
        "active": r.ended_at_ms.is_none(),
        "voice_frames": r.voice_frames,
        "reason": r.reason,
        "outcome": r.outcome,
    })
}

/// Every tuner's conventional channels as one list, in channel order, each
/// entry the tuner POST's channel object plus the `device` serving it.
fn channels_json(tuners: &[crate::pipeline::control::TunerStatus]) -> Value {
    let mut rows: Vec<(usize, Value)> = tuners
        .iter()
        .flat_map(|t| {
            t.channels.iter().map(|c| {
                let mut v = c.to_json();
                if let Some(map) = v.as_object_mut() {
                    map.insert("device".to_owned(), json!(t.tuner_key));
                }
                (c.index, v)
            })
        })
        .collect();
    rows.sort_by_key(|(i, _)| *i);
    Value::Array(rows.into_iter().map(|(_, v)| v).collect())
}

fn config_path_json(state: &PanelState) -> Value {
    match &state.config_path {
        Some(p) => json!(p.display().to_string().replace('\\', "/")),
        None => Value::Null,
    }
}

/// The wire name of a supervised unit's lifecycle state.
fn state_str(s: ComponentState) -> &'static str {
    match s {
        ComponentState::Running => "running",
        ComponentState::Degraded => "degraded",
        ComponentState::Restarting => "restarting",
        ComponentState::Retrying => "retrying",
        ComponentState::Failed => "failed",
    }
}

/// The loaded config as JSON with every secret replaced.
///
/// A panel that dumps its own config is only safe if the dump cannot hand out
/// the upload key or the panel's own tokens, so redaction happens on a clone of
/// the typed config *before* it is ever serialized — there is no code path that
/// serializes the real one. Empty secrets stay empty: `"***"` where nothing is
/// set would be a lie about how the recorder is configured.
pub(crate) fn redacted_config(cfg: &Config) -> Value {
    serde_json::to_value(redact(cfg)).unwrap_or_else(|e| json!({ "error": e.to_string() }))
}

/// A clone of `cfg` with every secret replaced, still typed.
///
/// The panel holds this rather than the real config: the device routes need to
/// read `[[device]]`/`[[site]]` structurally, and the cheapest way to guarantee
/// they cannot serve a key by accident is to never hand them one.
///
/// The secrets are every field that can authenticate something: the upload
/// key, the panel's own tokens, the recorder link's shared HMAC key (anyone
/// holding it can tell every peer "record this frequency"), and any
/// `[device.settings]` entry whose name says it is a credential — the same
/// name rule the About page applies ([`crate::about::redact::is_secret_name`]),
/// because a free-form driver map is exactly where a field nobody declared
/// turns up.
pub(crate) fn redact(cfg: &Config) -> Config {
    let mut cfg = cfg.clone();
    redact_str(&mut cfg.uplink.api_key);
    for t in cfg.server.tokens.iter_mut() {
        redact_str(t);
    }
    redact_str(&mut cfg.link.key);
    for d in cfg.device.iter_mut() {
        redact_settings(&mut d.settings);
    }
    cfg
}

/// What a set secret reads as anywhere the panel shows config.
const REDACTED: &str = "***";

/// Replace a set secret with [`REDACTED`]; an unset one stays empty.
fn redact_str(s: &mut String) {
    if !s.is_empty() {
        *s = REDACTED.to_owned();
    }
}

/// Redact every secret-named entry of a `[device.settings]` map.
fn redact_settings(settings: &mut std::collections::BTreeMap<String, String>) {
    for (name, value) in settings.iter_mut() {
        if crate::about::redact::is_secret_name(name) {
            redact_str(value);
        }
    }
}

// ---------------------------------------------------------------------------
// The thread
// ---------------------------------------------------------------------------

/// Bind `addr` and serve until `stop` is set.
///
/// A failed bind is logged and returns: the panel is an observability
/// convenience, and a recorder that refuses to record because port 8080 was
/// taken would be a worse recorder than one with no panel at all.
pub(crate) fn run(addr: &str, mut state: PanelState, stop: Arc<AtomicBool>) {
    let listener = match std::net::TcpListener::bind(addr) {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(bind = addr, error = %e, "control panel could not bind; not serving");
            return;
        }
    };
    let bound = listener.local_addr().ok();
    // The port actually bound, which is the one a browser's `Origin` names
    // (`:0` in the config would otherwise match nothing).
    if let Some(a) = bound {
        state.origins.port = a.port();
    }
    tracing::info!(
        bind = %bound.map(|a| a.to_string()).unwrap_or_else(|| addr.to_owned()),
        auth = if tokens_configured(&state.tokens) { "bearer-token" } else { "loopback-mutations" },
        "local control panel listening"
    );
    serve(listener, Arc::new(state), stop, Limits::PANEL);
}

/// The accept loop, split out so a test can drive it over an ephemeral port.
///
/// Returns when `stop` is set. A connection already being answered finishes
/// on its own thread (each is bounded by the transport's deadlines), and a
/// live stream ends at its next frame.
pub(crate) fn serve(
    listener: std::net::TcpListener,
    state: Arc<PanelState>,
    stop: Arc<AtomicBool>,
    limits: Limits,
) {
    // One route at a time, as when the panel was one thread: the handlers
    // share a config file and a desired-state registry, and none of them is
    // slow once its body has been read (which the transport does first).
    let routing = Arc::new(std::sync::Mutex::new(()));
    let handler = Arc::new(move |rq: Request, conn: &mut Conn| handle(rq, conn, &state, &routing));
    panelhttp::serve(listener, limits, stop, handler);
}

/// Route, authorise and answer one request. Never panics out: a handler
/// fault would otherwise cost the connection its answer.
fn handle(mut rq: Request, conn: &mut Conn, state: &PanelState, routing: &std::sync::Mutex<()>) {
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // Before anything else, the page included: a request that names some
        // other host is a browser that was pointed here by a DNS name that is
        // not this panel's.
        if let Some(denial) = check_host(&rq, state) {
            conn.respond(denial);
            return;
        }
        let path = rq.url().split('?').next().unwrap_or("/").to_owned();
        let method = rq.method().clone();
        // The one route that cannot be a `Response<Cursor<Vec<u8>>>`: a live
        // audio stream has no length and no end. It answers on this
        // connection's own thread, outside the routing lock.
        if method == Method::Get && path == "/api/monitor/stream" {
            monitor_stream(&rq, state, conn);
            return;
        }
        let response = {
            let _one_at_a_time = routing.lock().unwrap_or_else(|p| p.into_inner());
            route(&method, &path, &mut rq, state)
        };
        conn.respond(response);
    }));
    if outcome.is_err() {
        tracing::error!("control panel handler panicked; the panel stays up");
        conn.respond(json_response(500, &json!({ "error": "internal error" })));
    }
}

/// The routing table proper.
fn route(
    method: &Method,
    path: &str,
    rq: &mut Request,
    state: &PanelState,
) -> Response<std::io::Cursor<Vec<u8>>> {
    // The page itself is always served: it carries no data, and an operator who
    // cannot even load the login-less shell has no way to discover that a token
    // is what they are missing.
    if path == "/" || path == "/index.html" {
        return if method == &Method::Get {
            html_response(INDEX_HTML)
        } else {
            method_not_allowed("GET")
        };
    }

    // The About page, like `/`, is a data-free shell that fetches its own
    // content from an authorised route.
    if path == "/about" {
        return if method == &Method::Get {
            html_response(crate::about::ABOUT_HTML)
        } else {
            method_not_allowed("GET")
        };
    }

    if !path.starts_with("/api/") {
        return json_response(404, &json!({ "error": "no such route" }));
    }
    if let Some(denial) = check_origin(method, rq, state) {
        return denial;
    }
    if let Some(denial) = authorize(method, rq, state) {
        return denial;
    }

    match path {
        "/api/status" => match method {
            Method::Get => json_response(200, &state.status()),
            _ => method_not_allowed("GET"),
        },
        "/api/monitor/status" => match method {
            Method::Get => json_response(200, &state.monitor_json()),
            _ => method_not_allowed("GET"),
        },
        // `/api/monitor/stream` never reaches here: it is intercepted in
        // `handle`, because it answers from a thread of its own with a body
        // that does not end. This arm exists so a POST to it says 405 rather
        // than "no such route".
        "/api/monitor/stream" => method_not_allowed("GET"),
        "/api/about" => match method {
            Method::Get => json_response(200, &state.about.json()),
            _ => method_not_allowed("GET"),
        },
        "/api/config" => match method {
            Method::Get => json_response(200, &state.config_json),
            _ => method_not_allowed("GET"),
        },
        "/api/devices/discover" => match method {
            Method::Get => devices::discover(state),
            _ => method_not_allowed("GET"),
        },
        "/api/config/devices" => match method {
            Method::Get => devices::list(state),
            _ => method_not_allowed("GET"),
        },
        "/api/config/reload" => match method {
            Method::Post => devices::reload(state),
            _ => method_not_allowed("POST"),
        },
        "/api/uplink" => match method {
            Method::Get => json_response(200, &state.uplink_json()),
            Method::Post => set_hold(rq, state),
            _ => method_not_allowed("GET, POST"),
        },

        _ => {
            if let Some(site) = site_detail_target(path) {
                return match method {
                    Method::Get => site_detail(site, state),
                    _ => method_not_allowed("GET"),
                };
            }
            if let Some(id) = device_target(path) {
                return match method {
                    Method::Put => devices::put(&id, rq, state),
                    Method::Delete => devices::delete(&id, state),
                    _ => method_not_allowed("PUT, DELETE"),
                };
            }
            match restart_target(path) {
                Some(key) => match method {
                    Method::Post => restart(&key, state),
                    _ => method_not_allowed("POST"),
                },
                None => json_response(404, &json!({ "error": "no such route" })),
            }
        }
    }
}

/// The site index in `GET /api/sites/{index}/detail`.
fn site_detail_target(path: &str) -> Option<usize> {
    path.strip_prefix("/api/sites/")?
        .strip_suffix("/detail")?
        .parse()
        .ok()
}

/// `GET /api/sites/{index}/detail`: the site's observation document, as the
/// archive gets it, plus what only a local operator asks for.
///
/// Answered from the observations thread's last published copy — never from the
/// live inventory. Reads under the same rule as `/api/status`; a 404 for an
/// index that is not a configured site, and a 503 in the first couple of
/// seconds of a run, before that thread has published anything.
fn site_detail(site: usize, state: &PanelState) -> Response<std::io::Cursor<Vec<u8>>> {
    if !state.site_details.has(site) {
        return json_response(
            404,
            &json!({ "error": format!("no site with index {site}") }),
        );
    }
    match state.site_details.get(site) {
        Some(doc) => json_response(200, &doc),
        None => json_response(
            503,
            &json!({ "error": "no observation snapshot for this site yet" }),
        ),
    }
}

/// The device id in `/api/config/devices/{id}`, percent-decoded.
fn device_target(path: &str) -> Option<String> {
    let id = path.strip_prefix("/api/config/devices/")?;
    if id.is_empty() || id.contains('/') {
        return None;
    }
    Some(percent_decode(id))
}

/// `POST /api/tuners/{key}/restart`: bump the tuner's desired-state generation.
///
/// That is the whole mutation. The running `DeviceUnit` notices its generation
/// moved, asks the supervisor for a clean rebuild and returns; the rebuild reads
/// the same `pending` config it was already built with, so this restarts a
/// tuner without changing anything about it. A tuner that is *not* running —
/// failing to open and waiting out its retry backoff — is re-armed by the same
/// bump: the supervisor watches the generation while it waits, retries at once
/// and starts the unit's restart budget over.
fn restart(key: &str, state: &PanelState) -> Response<std::io::Cursor<Vec<u8>>> {
    if state.desired.request_restart(key) {
        tracing::info!(tuner = key, "control panel requested a tuner restart");
        json_response(200, &json!({ "restarting": true }))
    } else {
        json_response(404, &json!({ "error": format!("no tuner named {key:?}") }))
    }
}

/// `POST /api/uplink` with `{"spool_only": true|false}`: hold or release
/// uploads, and answer with the new state.
///
/// The panel is the last-resort control: it is the one that still works when
/// keyup is the thing that is unreachable, which is exactly when an operator
/// needs to stop a recorder hammering a dead endpoint — or to release a hold
/// somebody left on. It writes the same shared flag keyup writes, and stamps
/// itself as the source, so the next status POST tells keyup who moved it.
fn set_hold(rq: &mut Request, state: &PanelState) -> Response<std::io::Cursor<Vec<u8>>> {
    if let Some(refusal) = require_json(rq) {
        return refusal;
    }
    let body = match read_body(rq) {
        Ok(b) => b,
        Err(e) => {
            return json_response(400, &json!({ "error": format!("unreadable body: {e}") }));
        }
    };
    let want = serde_json::from_str::<Value>(&body)
        .ok()
        .and_then(|v| v.get("spool_only").and_then(Value::as_bool));
    let Some(want) = want else {
        return json_response(
            400,
            &json!({ "error": "expected a JSON body of {\"spool_only\": true|false}" }),
        );
    };
    if state.recorder.set_hold_from_panel(want) {
        tracing::warn!(
            spool_only = want,
            "control panel {} the uplink",
            if want { "held" } else { "released" }
        );
    }
    json_response(200, &state.uplink_json())
}

/// Refuse a JSON-bodied mutation whose `Content-Type` is not
/// `application/json` (parameters such as `charset` are fine).
///
/// A cross-site page can send `text/plain`, a form encoding or no type at all
/// without a CORS preflight; `application/json` it cannot. The `Origin` check
/// already stops a browser that says where it is from, and this one stops the
/// request that is shaped to avoid the question. The panel's own page, curl
/// with `-H 'Content-Type: application/json'` and PowerShell's
/// `-ContentType application/json` all send it.
fn require_json(rq: &Request) -> Option<Response<std::io::Cursor<Vec<u8>>>> {
    let ct = rq.header("Content-Type").unwrap_or_default();
    let media = ct.split(';').next().unwrap_or_default().trim();
    if media.eq_ignore_ascii_case("application/json") {
        return None;
    }
    Some(json_response(
        415,
        &json!({
            "error": format!(
                "this route takes a JSON body with Content-Type: application/json, not {:?}",
                if ct.is_empty() { "(none)" } else { ct }
            )
        }),
    ))
}

/// Read a request body, refusing anything absurd for a two-field JSON object.
fn read_body(rq: &mut Request) -> std::io::Result<String> {
    /// A generous ceiling for `{"spool_only": false}`; anything larger is not
    /// a request this route understands.
    const MAX_BODY: u64 = 8 * 1024;
    read_body_limit(rq, MAX_BODY)
}

/// Read at most `max` bytes of request body.
fn read_body_limit(rq: &mut Request, max: u64) -> std::io::Result<String> {
    let mut buf = String::new();
    let mut reader = std::io::Read::take(rq.as_reader(), max);
    std::io::Read::read_to_string(&mut reader, &mut buf)?;
    Ok(buf)
}

/// The tuner key in `/api/tuners/{key}/restart`, percent-decoded.
fn restart_target(path: &str) -> Option<String> {
    let rest = path.strip_prefix("/api/tuners/")?;
    let key = rest.strip_suffix("/restart")?;
    if key.is_empty() || key.contains('/') {
        return None;
    }
    Some(percent_decode(key))
}

/// Decode `%XX` escapes; anything malformed is left verbatim (it will simply
/// fail to match a tuner key, which is the right answer for a bad URL).
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ---------------------------------------------------------------------------
// Device setup
// ---------------------------------------------------------------------------

/// The routes that let an operator set devices up from the panel instead of
/// from an SSH session and a text editor.
///
/// # The one rule
///
/// These are the only handlers in the panel that write to disk, and the rule
/// they all follow is *validate, then write, then ask for a reload* — never
/// write something the recorder could not start on. Each mutation parses the
/// live file, edits the document, proves the result loads through the same path
/// [`Config::load`] takes, and only then replaces the file (keeping a `.bak`).
/// A rejected request has changed nothing at all.
///
/// # Why a save is not an apply
///
/// Adding or removing a `[[device]]` changes how many tuners the pipeline has,
/// and every per-device array in the running recorder was sized at boot (see
/// [`crate::pipeline::devicecfg`]). So a save sets `restart_pending` and the
/// operator applies it with `POST /api/config/reload`, which tears the pipeline
/// down cleanly and builds it again in the same process. Gain, ppm and band
/// changes keyup can push stay live and need none of this.
mod devices {
    use super::*;

    use std::path::Path;

    use crate::config::{Backend, Device};
    use crate::pipeline::devicecfg::{self, DeviceSpec};

    /// A body big enough for a device with a long `notes` and a site list, and
    /// small enough that nothing here is a memory question.
    const MAX_BODY: u64 = 64 * 1024;

    type Resp = Response<std::io::Cursor<Vec<u8>>>;

    /// One attached board, as the discover route reports it.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(super) struct Hardware {
        /// Stable key for this board: `<backend>:<canonical args>`. Derived
        /// rather than taken from the descriptor id because a Soapy id carries
        /// the driver's `label=` too, which changes with the driver version and
        /// would make the key move under the UI.
        pub key: String,
        /// `soapy` or `hackrf` — the `backend` a `[[device]]` for it needs.
        pub backend: String,
        /// Driver module name (`airspy`, `sdrplay`, `rtlsdr`, `hackrf`).
        pub driver: String,
        /// Board serial, when the backend reports one.
        pub serial: Option<String>,
        /// What the driver calls it, for the operator to recognise.
        pub label: String,
        /// The verbatim args the backend enumerated.
        pub args: String,
    }

    /// Turn a backend descriptor into the panel's view of a board.
    pub(super) fn describe(d: &trunkd_sdr::DeviceDescriptor) -> Hardware {
        let (backend, driver) = match d.driver.split_once(':') {
            Some(("soapy", drv)) => ("soapy", drv.to_owned()),
            _ => ("hackrf", "hackrf".to_owned()),
        };
        let serial = arg_value(&d.args, "serial");
        let key = format!(
            "{backend}:{}",
            trunkd_sdr::soapy_args(None, Some(&driver), serial.as_deref())
        );
        Hardware {
            key,
            backend: backend.to_owned(),
            driver,
            serial,
            label: d.label.clone(),
            args: d.args.clone(),
        }
    }

    /// The value of `name=` in a comma-separated Soapy args string.
    fn arg_value(args: &str, name: &str) -> Option<String> {
        args.split(',').find_map(|kv| {
            let (k, v) = kv.split_once('=')?;
            (k.trim() == name).then(|| v.trim().to_owned())
        })
    }

    /// The driver a configured device names: its Soapy `driver`, or the backend
    /// itself for a HackRF.
    fn device_driver(d: &Device) -> Option<String> {
        match d.backend {
            Backend::Hackrf => Some("hackrf".to_owned()),
            Backend::Soapy => d
                .driver
                .clone()
                .or_else(|| arg_value(&d.effective_soapy_args(), "driver")),
            Backend::Iqfile => None,
        }
    }

    /// Which attached board each configured device is, by index into `hw`.
    ///
    /// Three rules in order of how much they can be trusted:
    ///
    /// 1. **Serial** — the only identifier that survives a USB port change, so
    ///    it wins whenever the device has one. Compared case-insensitively:
    ///    Soapy reports an Airspy serial in lower case and `hackrf_info` in
    ///    upper, and an operator who pasted one into the file should not have
    ///    the panel tell them the board is missing.
    /// 2. **Exact Soapy args** — for a device pinned with an explicit `soapy`
    ///    string, which is the escape hatch for hardware trunkd has no field
    ///    for; nothing but a literal comparison can be right there.
    /// 3. **Driver, only when it is unambiguous** — one configured device of
    ///    that driver and exactly one such board attached. With two Airspys and
    ///    no serials there is no honest answer, so both are reported absent
    ///    rather than matched arbitrarily.
    pub(super) fn match_configured(devices: &[Device], hw: &[Hardware]) -> Vec<Option<usize>> {
        devices
            .iter()
            .map(|d| {
                if let Some(serial) = d.serial.as_deref().filter(|s| !s.is_empty()) {
                    return hw.iter().position(|h| {
                        h.serial
                            .as_deref()
                            .is_some_and(|s| s.eq_ignore_ascii_case(serial))
                    });
                }
                if d.backend == Backend::Soapy {
                    let args = d.effective_soapy_args();
                    if let Some(i) = hw.iter().position(|h| h.args == args) {
                        return Some(i);
                    }
                }
                let driver = device_driver(d)?;
                let configured_with = devices
                    .iter()
                    .filter(|o| device_driver(o).as_deref() == Some(driver.as_str()))
                    .count();
                let boards: Vec<usize> = hw
                    .iter()
                    .enumerate()
                    .filter(|(_, h)| h.driver == driver)
                    .map(|(i, _)| i)
                    .collect();
                (configured_with == 1 && boards.len() == 1).then(|| boards[0])
            })
            .collect()
    }

    /// Whether the supervised unit for `id` is up.
    ///
    /// The supervisor has no `stopped` state — a unit that exists is one the
    /// recorder started — so what is excluded is `failed`, a tuner that gave
    /// up, and `retrying`, one that keeps failing (it holds no board open,
    /// which is what "running" answers for here). `restarting` counts as
    /// running: it is coming back, and reporting it as gone would have the
    /// panel flicker on every rebuild.
    fn unit_running(state: &PanelState, id: &str) -> bool {
        let name = format!("device:{id}");
        state.units.iter().any(|u| {
            u.name() == name
                && !matches!(u.state(), ComponentState::Failed | ComponentState::Retrying)
        })
    }

    // -----------------------------------------------------------------------
    // Reading
    // -----------------------------------------------------------------------

    /// `GET /api/devices/discover`: what is plugged in, what is configured, and
    /// which is which.
    ///
    /// Enumeration is a *listing*, not an open: SoapySDR's `enumerateStrArgs`
    /// asks each module what it can see and touches no device, so this is safe
    /// to call while the recorder is running on those same boards.
    pub(super) fn discover(state: &PanelState) -> Resp {
        let hw: Vec<Hardware> = (state.enumerate)().iter().map(describe).collect();
        // Lenient on purpose: discover is the page an operator opens *because*
        // something is wrong, so a file that no longer parses must not stop it
        // from showing what hardware is attached.
        let cfg = read_config(state).unwrap_or_else(|_| (*state.config).clone());
        let matched = match_configured(&cfg.device, &hw);

        let hardware: Vec<Value> = hw
            .iter()
            .enumerate()
            .map(|(i, h)| {
                let configured_id = matched
                    .iter()
                    .position(|m| *m == Some(i))
                    .map(|d| cfg.device[d].id.clone());
                json!({
                    "key": h.key,
                    "backend": h.backend,
                    "driver": h.driver,
                    "serial": h.serial,
                    "label": h.label,
                    "args": h.args,
                    "configured_id": configured_id,
                    "running": configured_id.as_deref().is_some_and(|id| unit_running(state, id)),
                    "suggested": devicecfg::suggested_json(&h.driver, h.serial.as_deref()),
                })
            })
            .collect();

        let configured: Vec<Value> = cfg
            .device
            .iter()
            .zip(&matched)
            .map(|(d, m)| {
                // A board this process holds open does not enumerate (the
                // Airspy and SDRplay drivers read the serial by opening the
                // device), so a running device is present by definition.
                let running = unit_running(state, &d.id);
                json!({
                    "id": d.id,
                    "present": m.is_some() || running,
                    "running": running,
                    "sites": sites_of(&cfg, &d.id),
                })
            })
            .collect();

        json_response(
            200,
            &json!({
                "config_path": config_path_json(state),
                "writable": state.config_path.is_some(),
                "restart_pending": state.restart_pending.load(Ordering::Relaxed),
                "hardware": hardware,
                "configured": configured,
            }),
        )
    }

    /// `GET /api/config/devices`: the `[[device]]` and `[[site]]` tables as
    /// they are *on disk*, which after a save is one step ahead of what is
    /// recording. Re-read per request so the editor never shows a stale form.
    pub(super) fn list(state: &PanelState) -> Resp {
        let cfg = match read_config(state) {
            Ok(c) => c,
            Err(r) => return r,
        };
        let devices: Vec<Value> = cfg
            .device
            .iter()
            .map(|d| {
                // Read from the file, so redacted here: a secret-named driver
                // setting is shown as `***`, and `put` puts the real value
                // back when the editor returns the placeholder untouched.
                let mut d = d.clone();
                redact_settings(&mut d.settings);
                // The serde serialization, so every key is spelled exactly as
                // the TOML spells it and a PUT can hand the same names back.
                let mut v = serde_json::to_value(&d).unwrap_or_else(|_| json!({}));
                if let Some(o) = v.as_object_mut() {
                    o.insert("sites".to_owned(), json!(sites_of(&cfg, &d.id)));
                }
                v
            })
            .collect();
        let sites: Vec<Value> = cfg
            .site
            .iter()
            .map(|s| {
                json!({
                    "system": s.system,
                    "site": s.site,
                    "enabled": s.enabled,
                    "devices": s.device_ids(),
                    "control_frequencies": s.control_frequencies,
                    "traffic_pool_size": s.traffic_pool_size,
                })
            })
            .collect();
        json_response(200, &json!({ "devices": devices, "sites": sites }))
    }

    /// The `"System/Site"` names of every site `id` serves.
    fn sites_of(cfg: &Config, id: &str) -> Vec<String> {
        cfg.site
            .iter()
            .filter(|s| s.device_ids().contains(&id))
            .map(|s| format!("{}/{}", s.system, s.site))
            .collect()
    }

    // -----------------------------------------------------------------------
    // Writing
    // -----------------------------------------------------------------------

    /// `PUT /api/config/devices/{id}`: create or edit one `[[device]]`, and
    /// optionally set which sites it serves.
    ///
    /// The body is an object of `[[device]]` keys by their TOML names. Fields
    /// it does not mention keep whatever the file says and a field set to
    /// `null` is removed, so a form that only edits gain cannot wipe the notes
    /// the operator typed in by hand. `sites`, when present, is the *complete*
    /// membership list; omitting it leaves membership alone.
    pub(super) fn put(id: &str, rq: &mut Request, state: &PanelState) -> Resp {
        let path = match writable(state) {
            Ok(p) => p,
            Err(r) => return r,
        };
        if let Some(refusal) = require_json(rq) {
            return refusal;
        }
        let body = match read_body_limit(rq, MAX_BODY) {
            Ok(b) => b,
            Err(e) => return bad_request(format!("unreadable body: {e}")),
        };
        let parsed: Value = match serde_json::from_str(&body) {
            Ok(v) => v,
            Err(e) => return bad_request(format!("body is not JSON: {e}")),
        };
        let Some(obj) = parsed.as_object() else {
            return bad_request("expected a JSON object of device fields".to_owned());
        };

        // `sites` is the route's own key, not a device field; validate its
        // shape before anything is edited.
        let sites: Option<Vec<String>> = match obj.get("sites") {
            None => None,
            Some(Value::Array(items)) => {
                let mut out = Vec::with_capacity(items.len());
                for it in items {
                    match it.as_str() {
                        Some(s) => out.push(s.to_owned()),
                        None => {
                            return bad_request(
                                "sites must be a list of \"System/Site\" strings".to_owned(),
                            )
                        }
                    }
                }
                Some(out)
            }
            Some(_) => {
                return bad_request("sites must be a list of \"System/Site\" strings".to_owned())
            }
        };

        let mut obj = obj.clone();
        if let Err(r) = keep_redacted_settings(id, &mut obj, state) {
            return r;
        }
        let spec = match DeviceSpec::from_json(id, &obj) {
            Ok(s) => s,
            Err(e) => return bad_request(e),
        };

        let mut doc = match read_document(&path) {
            Ok(d) => d,
            Err(r) => return r,
        };
        if let Err(e) = devicecfg::upsert_device(&mut doc, &spec) {
            return bad_request(e);
        }
        if let Some(sites) = &sites {
            if let Err(e) = devicecfg::set_device_sites(&mut doc, id, sites) {
                // The only failure is a site name the file does not have.
                return json_response(404, &json!({ "error": e }));
            }
        }
        if let Err(e) = devicecfg::validate_document(&doc) {
            return json_response(422, &json!({ "error": e }));
        }
        if let Err(r) = commit(&path, &doc.to_string(), state) {
            return r;
        }
        tracing::info!(device = id, path = %path.display(), "control panel wrote a device");
        json_response(
            200,
            &json!({ "ok": true, "restart_required": true, "id": id }),
        )
    }

    /// Swap a `***` the editor handed back for a secret-named setting with the
    /// value on disk.
    ///
    /// `list` redacts those settings, and the editor sends the whole settings
    /// map on every save, so without this editing a device's gain would
    /// overwrite its credential with the placeholder. A placeholder with no
    /// value on disk to stand for is refused rather than written.
    fn keep_redacted_settings(
        id: &str,
        obj: &mut serde_json::Map<String, Value>,
        state: &PanelState,
    ) -> Result<(), Resp> {
        let Some(Value::Object(settings)) = obj.get_mut("settings") else {
            return Ok(());
        };
        let placeholders: Vec<String> = settings
            .iter()
            .filter(|(k, v)| {
                crate::about::redact::is_secret_name(k) && v.as_str() == Some(REDACTED)
            })
            .map(|(k, _)| k.clone())
            .collect();
        if placeholders.is_empty() {
            return Ok(());
        }
        let on_disk = read_config(state)?;
        let current = on_disk.device.iter().find(|d| d.id == id);
        for k in placeholders {
            match current.and_then(|d| d.settings.get(&k)) {
                Some(real) => {
                    settings.insert(k, Value::String(real.clone()));
                }
                None => {
                    return Err(bad_request(format!(
                        "settings.{k}: {REDACTED:?} is the redaction placeholder, \
                         not a value; send the real value, or null to remove it"
                    )))
                }
            }
        }
        Ok(())
    }

    /// `DELETE /api/config/devices/{id}`: drop a `[[device]]`.
    ///
    /// Refused with 409 while a site or channel still names it. Removing the
    /// membership first is the operator's decision to make, not the panel's:
    /// silently pulling a tuner out from under a site is how a recorder comes
    /// back deaf on one system and nobody knows why.
    pub(super) fn delete(id: &str, state: &PanelState) -> Resp {
        let path = match writable(state) {
            Ok(p) => p,
            Err(r) => return r,
        };
        let mut doc = match read_document(&path) {
            Ok(d) => d,
            Err(r) => return r,
        };
        let refs = devicecfg::references_to(&doc, id);
        if !refs.is_empty() {
            return json_response(
                409,
                &json!({
                    "error": format!("device '{id}' is still used by {}", refs.join(", ")),
                    "referenced_by": refs,
                }),
            );
        }
        if let Err(e) = devicecfg::remove_device(&mut doc, id) {
            return json_response(404, &json!({ "error": e }));
        }
        if let Err(e) = devicecfg::validate_document(&doc) {
            return json_response(422, &json!({ "error": e }));
        }
        if let Err(r) = commit(&path, &doc.to_string(), state) {
            return r;
        }
        tracing::info!(device = id, path = %path.display(), "control panel removed a device");
        json_response(200, &json!({ "ok": true, "restart_required": true }))
    }

    /// `POST /api/config/reload`: apply what is on disk.
    ///
    /// The file is loaded and validated *first*. A recorder must never be taken
    /// down by a config it is about to fail to start on, and the operator who
    /// hand-edited the file in another window while the panel was open is
    /// exactly who this protects. Only once it is known good does the flag go
    /// up; the run then ends through the ordinary teardown and the CLI builds
    /// the pipeline again, same process, same PID.
    pub(super) fn reload(state: &PanelState) -> Resp {
        let path = match writable(state) {
            Ok(p) => p,
            Err(r) => return r,
        };
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => {
                return json_response(422, &json!({ "error": format!("{}: {e}", path.display()) }))
            }
        };
        if let Err(e) = Config::from_toml_str(&text) {
            return json_response(422, &json!({ "error": e.to_string() }));
        }
        tracing::warn!(path = %path.display(), "control panel requested a configuration reload");
        state.reload.store(true, Ordering::Relaxed);
        json_response(200, &json!({ "ok": true }))
    }

    // -----------------------------------------------------------------------
    // Shared plumbing
    // -----------------------------------------------------------------------

    /// The config file, or the 503 that says there is not one.
    fn writable(state: &PanelState) -> Result<std::path::PathBuf, Resp> {
        state.config_path.clone().ok_or_else(|| {
            json_response(
                503,
                &json!({
                    "error": "this recorder was configured from the environment, not a file, \
                              so there is nothing for the panel to write"
                }),
            )
        })
    }

    /// Parse the live file into an editable document.
    fn read_document(path: &Path) -> Result<toml_edit::DocumentMut, Resp> {
        let text = std::fs::read_to_string(path).map_err(|e| {
            json_response(422, &json!({ "error": format!("{}: {e}", path.display()) }))
        })?;
        text.parse::<toml_edit::DocumentMut>().map_err(|e| {
            json_response(422, &json!({ "error": format!("{}: {e}", path.display()) }))
        })
    }

    /// The config as it is on disk right now, or the running one when this
    /// process was configured without a file.
    fn read_config(state: &PanelState) -> Result<Config, Resp> {
        let Some(path) = &state.config_path else {
            return Ok((*state.config).clone());
        };
        let text = std::fs::read_to_string(path).map_err(|e| {
            json_response(422, &json!({ "error": format!("{}: {e}", path.display()) }))
        })?;
        Config::from_toml_str(&text)
            .map_err(|e| json_response(422, &json!({ "error": e.to_string() })))
    }

    /// Write the edited document and raise the "saved, not yet applied" flag.
    fn commit(path: &Path, text: &str, state: &PanelState) -> Result<(), Resp> {
        devicecfg::write_atomic(path, text).map_err(|e| {
            tracing::error!(path = %path.display(), error = %e, "control panel could not write the config");
            json_response(
                500,
                &json!({ "error": format!("could not write {}: {e}", path.display()) }),
            )
        })?;
        state.restart_pending.store(true, Ordering::Relaxed);
        Ok(())
    }

    fn bad_request(msg: String) -> Resp {
        json_response(400, &json!({ "error": msg }))
    }
}

// ---------------------------------------------------------------------------
// Auth
// ---------------------------------------------------------------------------

/// Why an `/api/*` request was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Denial {
    /// Tokens are configured and this request did not present a good one.
    NeedsToken,
    /// No tokens are configured, so a mutation must come from loopback.
    LoopbackOnly,
}

/// The auth rule, as a pure function of what the request presented.
///
/// Two modes, and which one is in force is simply whether `server.tokens` is
/// empty:
///
/// * **tokens configured** — every API request, read or write, must present a
///   matching `Authorization: Bearer <token>`. A loopback peer is *not* a
///   substitute: once an operator has set tokens, they have said what the rule
///   is.
/// * **no tokens** — reads are open, and mutations are refused unless the peer
///   is on loopback. Exposing a read-only panel on the LAN with no token is a
///   defensible choice; exposing an unauthenticated restart button is not.
///
/// An unknown peer address (no `remote_addr`) is treated as not-loopback: the
/// safe answer when we cannot tell who is asking.
///
/// A blank configured token does not count as one. The config loader already
/// drops them with a warning ([`crate::config::Server::drop_blank_tokens`]);
/// this is the second line, for a `Config` built any other way — `tokens =
/// [""]` must never mean "every request with an empty bearer is in".
fn auth_check(
    mutation: bool,
    presented: Option<&str>,
    peer: Option<std::net::IpAddr>,
    tokens: &[String],
) -> Result<(), Denial> {
    if tokens_configured(tokens) {
        return match presented {
            Some(t) if token_accepted(t, tokens) => Ok(()),
            _ => Err(Denial::NeedsToken),
        };
    }
    if !mutation || peer.is_some_and(|ip| ip.is_loopback()) {
        Ok(())
    } else {
        Err(Denial::LoopbackOnly)
    }
}

/// Apply [`auth_check`] to a live request, returning the denial response when
/// it refuses. Every method other than GET counts as a mutation.
fn authorize(
    method: &Method,
    rq: &Request,
    state: &PanelState,
) -> Option<Response<std::io::Cursor<Vec<u8>>>> {
    let presented = bearer(rq);
    let outcome = auth_check(
        method != &Method::Get,
        presented.as_deref(),
        rq.remote_addr().map(|a| a.ip()),
        &state.tokens,
    );
    match outcome {
        Ok(()) => None,
        Err(Denial::NeedsToken) => Some(json_response(
            401,
            &json!({
                "error": "server.tokens is set: every /api/ request needs \
                          Authorization: Bearer <token>"
            }),
        )),
        Err(Denial::LoopbackOnly) => Some(json_response(
            403,
            &json!({
                "error": "server.tokens is empty: mutations are accepted from loopback only. \
                          Set server.tokens to allow them from the network."
            }),
        )),
    }
}

/// Whether `presented` is one of the configured tokens.
///
/// Plain equality over a handful of strings. A constant-time compare would be
/// theatre here: the list is tiny and local, the endpoint is rate-limited by
/// nothing more exotic than a thread doing real work, and a timing oracle over
/// a LAN against a string compare is not the attack this panel loses to.
///
/// Blank never matches, on either side.
fn token_accepted(presented: &str, tokens: &[String]) -> bool {
    let presented = presented.trim();
    !presented.is_empty() && tokens.iter().any(|t| t.trim() == presented)
}

/// Whether any real (non-blank) token is configured: the switch between the
/// two auth modes.
fn tokens_configured(tokens: &[String]) -> bool {
    tokens.iter().any(|t| !t.trim().is_empty())
}

/// Which browser origins may send the panel a mutation, and which `Host`
/// names may ask it anything.
///
/// Loopback-only mutations stop the LAN, not the browser on the recorder box
/// itself: any page open there can `fetch(…, {method: "POST", mode:
/// "no-cors"})` to `127.0.0.1:<port>` without a preflight, and a DNS-rebinding
/// page can do the same under a name of its own. Browsers put an `Origin` on
/// every such request, so a mutation carrying one must come from the panel's
/// own page:
///
/// * `http://` + a loopback host (`localhost`, `127.0.0.1`, `[::1]`) + the
///   panel's port;
/// * `http://` + the bind host + the panel's port — or, for a wildcard bind,
///   any IP literal on the panel's port (whatever answers on that port at that
///   address *is* the panel);
/// * exactly the origin of `[server] public_url`, which is how a panel behind a
///   reverse proxy, or reached by a DNS name, is allowed.
///
/// * `http://` + this machine's own hostname (or `<hostname>.local`) + the
///   panel's port: a name only this box's own resolver or LAN hands out, and
///   so not one a rebinding page on the internet can use.
///
/// A different port on localhost is a different site. A request with no
/// `Origin` at all (curl, scripts, the Windows health monitor) is not a
/// browser being steered and is left to the token/loopback rule.
///
/// # `Host`, on every request
///
/// A rebinding page does not need a mutation to do harm: it can *read*
/// `/api/status` and `/api/config`, because once its own name resolves to
/// 127.0.0.1 the browser treats the panel as same-origin. What gives it away
/// is the `Host` header, which carries the attacker's name. So every request
/// must name the panel by one of the same names ([`allows_host`]): loopback,
/// the bind host, any IP literal on a wildcard bind, this machine's hostname,
/// or `public_url`'s host. The port is not compared: the browser has already
/// connected to *this* socket, so the port in `Host` cannot be forged into
/// anything, while a reverse proxy or a port-forward may well rewrite it. (It
/// matters for `Origin`, where another port is another site that could be
/// sending the request.) A request with no `Host` at all is HTTP/1.0 — the
/// transport refuses an HTTP/1.1 request without one — and so no browser.
///
/// [`allows_host`]: OriginPolicy::allows_host
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct OriginPolicy {
    /// The bind host, lowercased (brackets kept for IPv6); `None` for a
    /// wildcard bind.
    bind_host: Option<String>,
    /// The panel's listening port. [`run`] overwrites it with the port it
    /// actually bound.
    pub port: u16,
    /// The origin `public_url` names, when one is set.
    public: Option<(String, String, u16)>,
    /// This machine's hostname, lowercased, and its `.local` form.
    hostnames: Vec<String>,
}

impl OriginPolicy {
    /// The policy for a `[server]` block.
    pub(crate) fn new(server: &crate::config::Server) -> OriginPolicy {
        let bind = server.bind.trim();
        let (host, port) = match bind.rsplit_once(':') {
            Some((h, p)) if !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) => {
                (h, p.parse().unwrap_or(0))
            }
            _ => (bind, 80),
        };
        let bind_host = match host {
            "0.0.0.0" | "[::]" | "::" | "" => None,
            h => Some(h.to_ascii_lowercase()),
        };
        let public = server
            .public_url
            .as_deref()
            .map(str::trim)
            .filter(|u| !u.is_empty())
            .and_then(parse_origin);
        let hostnames = crate::about::host::hostname()
            .map(|h| h.trim().to_ascii_lowercase())
            .filter(|h| !h.is_empty() && !h.contains(['/', ':', '@', ' ']))
            .map(|h| vec![format!("{h}.local"), h])
            .unwrap_or_default();
        OriginPolicy {
            bind_host,
            port,
            public,
            hostnames,
        }
    }

    /// Whether `host` (lowercased; brackets kept for IPv6) is a name this
    /// panel answers to directly — everything but `public_url`, whose origin
    /// is matched whole.
    fn local_name(&self, host: &str) -> bool {
        if matches!(host, "localhost" | "127.0.0.1" | "[::1]") {
            return true;
        }
        if self.hostnames.iter().any(|h| h == host) {
            return true;
        }
        match &self.bind_host {
            Some(bound) => bound == host,
            None => is_ip_literal(host),
        }
    }

    /// Whether a request's `Host` header names this panel (see the type docs).
    pub(crate) fn allows_host(&self, host: &str) -> bool {
        let host = host.trim();
        // An authority and nothing else: `name`, `name:port`, `[v6]:port`.
        if host.is_empty() || host.contains(['/', '?', '#', '@', ' ']) {
            return false;
        }
        let Some((_, name, _)) = parse_origin(&format!("http://{host}")) else {
            return false;
        };
        self.local_name(&name) || self.public.as_ref().is_some_and(|(_, h, _)| *h == name)
    }

    /// Whether a request carrying this `Origin` header may mutate.
    pub(crate) fn allows(&self, origin: &str) -> bool {
        let Some(o) = parse_origin(origin.trim()) else {
            // `null` (a sandboxed frame, a `file://` page) and anything
            // unparseable are not the panel.
            return false;
        };
        if self.public.as_ref() == Some(&o) {
            return true;
        }
        let (scheme, host, port) = o;
        scheme == "http" && port == self.port && self.local_name(&host)
    }
}

/// `scheme://host[:port][/path]` → `(scheme, host, port)`, lowercased, with the
/// scheme's default port filled in. `None` for anything else.
fn parse_origin(s: &str) -> Option<(String, String, u16)> {
    let (scheme, rest) = s.split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    let authority = rest.split(['/', '?', '#']).next()?;
    // Userinfo has no place in an origin; refuse rather than guess.
    if authority.is_empty() || authority.contains('@') {
        return None;
    }
    let default = match scheme.as_str() {
        "http" => 80,
        "https" => 443,
        _ => return None,
    };
    // `[v6]:port`, `[v6]`, `host:port` or `host`.
    let (host, port) = if authority.starts_with('[') {
        let end = authority.find(']')?;
        let (h, tail) = authority.split_at(end + 1);
        match tail.strip_prefix(':') {
            Some(p) => (h, p.parse().ok()?),
            None if tail.is_empty() => (h, default),
            None => return None,
        }
    } else {
        match authority.rsplit_once(':') {
            Some((h, p)) => (h, p.parse().ok()?),
            None => (authority, default),
        }
    };
    if host.is_empty() {
        return None;
    }
    Some((scheme, host.to_ascii_lowercase(), port))
}

/// An IPv4 literal, or a bracketed IPv6 one — never a DNS name, which is what
/// a rebinding attack needs.
fn is_ip_literal(host: &str) -> bool {
    host.parse::<std::net::Ipv4Addr>().is_ok()
        || host
            .strip_prefix('[')
            .and_then(|h| h.strip_suffix(']'))
            .is_some_and(|h| h.parse::<std::net::Ipv6Addr>().is_ok())
}

/// Refuse a mutation whose `Origin` is not the panel's own (see
/// [`OriginPolicy`]). Reads are not checked: they change nothing, and the
/// secrets they could carry are redacted.
fn check_origin(
    method: &Method,
    rq: &Request,
    state: &PanelState,
) -> Option<Response<std::io::Cursor<Vec<u8>>>> {
    if matches!(method, Method::Get | Method::Head | Method::Options) {
        return None;
    }
    let origin = rq
        .headers()
        .iter()
        .find(|h| h.field.equiv("Origin"))
        .map(|h| h.value.as_str().to_owned())?;
    if state.origins.allows(&origin) {
        return None;
    }
    tracing::warn!(origin = %origin, "control panel refused a mutation from a foreign origin");
    Some(json_response(
        403,
        &json!({
            "error": format!(
                "Origin {origin} is not this panel: mutations are accepted only from the \
                 panel's own page (its bind address, localhost, or [server] public_url)"
            )
        }),
    ))
}

/// Refuse a request whose `Host` does not name this panel (see
/// [`OriginPolicy`]). 421 Misdirected Request: the request reached a server
/// that is not the one its name is for.
fn check_host(rq: &Request, state: &PanelState) -> Option<Response<std::io::Cursor<Vec<u8>>>> {
    let host = rq.header("Host")?;
    if state.origins.allows_host(host) {
        return None;
    }
    tracing::warn!(host = %host, "control panel refused a request for a foreign Host");
    Some(json_response(
        421,
        &json!({
            "error": format!(
                "Host {host} is not this panel: use its address, localhost, this machine's \
                 name, or set [server] public_url to the name you reach it by"
            )
        }),
    ))
}

/// The bearer token on a request, if any.
fn bearer(rq: &Request) -> Option<String> {
    rq.headers()
        .iter()
        .find(|h| h.field.equiv("Authorization"))
        .and_then(|h| {
            let v = h.value.as_str();
            let (scheme, token) = v.split_once(' ')?;
            scheme
                .eq_ignore_ascii_case("Bearer")
                .then(|| token.trim().to_owned())
        })
}

// ---------------------------------------------------------------------------
// The live audio monitor
// ---------------------------------------------------------------------------

/// `GET /api/monitor/stream[?talkgroup=N][&token=...]` — decoded voice, live.
///
/// This is the only route that is not a buffer with a length: it is a body
/// that ends when the listener closes the tab. It is answered on its
/// connection's own thread (every connection has one), outside the routing
/// lock, and gives its in-flight slot back once it is streaming — the hub's
/// [`MAX_STREAMS`](crate::pipeline::monitor::MAX_STREAMS) is its cap.
///
/// # HTTP/1.1 only
///
/// Chunked transfer is the only way to frame a body with no length, and it is
/// HTTP/1.1's. An HTTP/1.0 request gets a 400 saying so before a subscription
/// is taken — the old server instead buffered the endless body trying to
/// learn its length, growing without bound and pinning a stream slot. Every
/// HTTP/1.1 client must accept chunked, so `TE` needs no inspection.
///
/// # Bounded
///
/// Nothing here queues: the hub drops frames for a listener more than
/// two seconds behind, the reader re-encodes one tick at a time, and each
/// chunk is written as it is produced under the transport's per-chunk
/// deadline, so a stalled listener ends its stream (and frees its slot)
/// within that deadline of the socket filling.
///
/// # Why a token may ride the query string
///
/// A browser `<audio>` element cannot be given an `Authorization` header — its
/// `src` is a bare URL fetched by the media stack — so a panel with
/// `[server] tokens` set would otherwise be able to *show* the monitor and
/// never play it. The token is therefore also accepted as `?token=`, for this
/// one read-scope route. That is a real (if small) widening: query strings turn
/// up in proxy logs in a way headers do not. It is bounded to a route that is
/// off by default, reads nothing but audio, and is bound to loopback unless the
/// operator moved it — and the header still works for anything that can send
/// one.
fn monitor_stream(rq: &Request, state: &PanelState, conn: &mut Conn) {
    use crate::pipeline::monitor::{self, Filter};

    let query = rq
        .url()
        .split_once('?')
        .map(|(_, q)| q.to_owned())
        .unwrap_or_default();

    // Read scope, same rule as `/api/status`: with tokens set one must be
    // presented; with none set, reads are open.
    let presented = bearer(rq).or_else(|| query_param(&query, "token"));
    if auth_check(
        false,
        presented.as_deref(),
        rq.remote_addr().map(|a| a.ip()),
        &state.tokens,
    )
    .is_err()
    {
        conn.respond(json_response(
            401,
            &json!({
                "error": "server.tokens is set: the monitor stream needs \
                          Authorization: Bearer <token> or ?token=<token>"
            }),
        ));
        return;
    }

    let Some(hub) = state.monitor.clone() else {
        conn.respond(json_response(
            404,
            &json!({
                "error": "live audio is off",
                "detail": "set [server] live_audio = true and restart to serve decoded voice",
            }),
        ));
        return;
    };

    if !rq.is_http11() {
        conn.respond(json_response(
            400,
            &json!({
                "error": "the live stream needs HTTP/1.1: it is sent with chunked transfer \
                          encoding, which HTTP/1.0 does not have",
            }),
        ));
        return;
    }

    let filter = match query_param(&query, "talkgroup") {
        None => Filter::All,
        Some(raw) => match raw.trim().parse::<u32>() {
            Ok(tg) => Filter::Talkgroup(tg),
            Err(_) => {
                conn.respond(json_response(
                    400,
                    &json!({ "error": format!("talkgroup must be a number, not {raw:?}") }),
                ));
                return;
            }
        },
    };

    let Some(sub) = hub.subscribe(filter) else {
        conn.respond(json_response(
            503,
            &json!({
                "error": "too many monitor streams",
                "max_streams": monitor::MAX_STREAMS,
            }),
        ));
        return;
    };
    let mut reader = match monitor::reader(sub) {
        Ok(r) => r,
        Err(e) => {
            conn.respond(json_response(
                500,
                &json!({ "error": format!("could not open an encoder: {e}") }),
            ));
            return;
        }
    };

    let headers = [
        content_type("audio/mpeg"),
        header("Cache-Control", "no-store"),
        // The media stack must not try to seek in something that has no end;
        // saying so up front avoids a range request the recorder would have
        // to refuse.
        header("Accept-Ranges", "none"),
    ];
    // Ends on the listener's disconnect (a write fails), a stalled listener (a
    // chunk misses its deadline) or the recorder stopping; the reader is then
    // dropped and its subscription with it.
    if let Err(e) = conn.stream(&headers, &mut reader) {
        tracing::debug!(error = %e, "monitor stream ended");
    }
}

/// One `name=value` from a query string, percent-decoded. `None` when absent.
fn query_param(query: &str, name: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == name).then(|| percent_decode(&v.replace('+', " ")))
    })
}

// ---------------------------------------------------------------------------
// Responses
// ---------------------------------------------------------------------------

/// A JSON response with `status` and `body`.
fn json_response(status: u16, body: &Value) -> Response<std::io::Cursor<Vec<u8>>> {
    Response::from_string(body.to_string())
        .with_status_code(status)
        .with_header(content_type("application/json; charset=utf-8"))
}

/// The embedded page.
fn html_response(html: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    Response::from_string(html.to_owned())
        .with_status_code(200)
        .with_header(content_type("text/html; charset=utf-8"))
}

/// 405 naming the method the route does take.
fn method_not_allowed(allow: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    let mut r = json_response(
        405,
        &json!({ "error": format!("this route takes {allow}") }),
    );
    if let Ok(h) = Header::from_bytes(&b"Allow"[..], allow.as_bytes()) {
        r = r.with_header(h);
    }
    r
}

/// A header from two static-ish strings. Panics only on a malformed constant.
fn header(field: &str, value: &str) -> Header {
    Header::from_bytes(field.as_bytes(), value.as_bytes())
        .expect("a static header name and value are valid")
}

fn content_type(value: &str) -> Header {
    Header::from_bytes(&b"Content-Type"[..], value.as_bytes())
        .expect("a static content type is a valid header")
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::AtomicBool;
    use std::time::Duration;

    use crate::config::{Backend, Device};
    use crate::pipeline::SiteSpec;

    /// One `[[device]]`, parsed from TOML so the test never has to keep up with
    /// the struct's field list.
    fn dev(id: &str) -> Device {
        toml::from_str(&format!(
            "id = \"{id}\"\nbackend = \"hackrf\"\nsample_rate = 20000000\n\
             center_frequency = 855500000\n"
        ))
        .expect("a valid device")
    }

    /// A panel bound to an ephemeral port, serving on its own thread.
    struct Panel {
        base: String,
        stop: Arc<AtomicBool>,
        desired: Arc<DesiredState>,
        recorder: Arc<RecorderControl>,
        reload: Arc<AtomicBool>,
        restart_pending: Arc<AtomicBool>,
        /// The same shared block the running panel reads, so a test can put a
        /// private call in it and see it come back out of `/api/status`.
        shared: Shared,
        /// The scratch directory holding this panel's config file, removed on
        /// drop. `None` for a panel with no file, which is the env-only case.
        dir: Option<std::path::PathBuf>,
        /// The same per-site document slots the running panel reads, so a test
        /// can publish an observation document the way the observations thread
        /// does and see it come back out of the detail route.
        details: Arc<crate::pipeline::observations::SiteDetails>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    /// A config file with two devices, one site and a secret, for the routes
    /// that read and rewrite it.
    const CONFIG_FILE: &str = r#"# hand written
[server]
enabled = true

[uplink]
api_key = 'kup_live_secret'

[[device]]
id = "airspy0"
backend = "soapy"
driver = "airspy"
serial = "00000000DEADBEEF"
sample_rate = 10000000
center_frequency = 851500000

[[device]]
id = "hackrf-0"
backend = "hackrf"
sample_rate = 20000000
center_frequency = 851500000

[[site]]
system = "Example Network"
site = "Central"
devices = ["hackrf-0"]
control_frequencies = [851050000]
"#;

    /// One descriptor per fake board, in the shape the backends produce.
    fn fake_hardware() -> Vec<trunkd_sdr::DeviceDescriptor> {
        vec![
            trunkd_sdr::DeviceDescriptor {
                id: trunkd_core::DeviceId::new(
                    "soapy:driver=airspy,label=AirSpy One,serial=00000000deadbeef",
                ),
                driver: "soapy:airspy".into(),
                label: "AirSpy One [00000000deadbeef]".into(),
                args: "driver=airspy,label=AirSpy One,serial=00000000deadbeef".into(),
            },
            trunkd_sdr::DeviceDescriptor {
                id: trunkd_core::DeviceId::new("hackrf:0000000000000000deadbeef00000003"),
                driver: "hackrf".into(),
                label: "HackRF One (0000000000000000deadbeef00000003)".into(),
                args: "serial=0000000000000000deadbeef00000003".into(),
            },
        ]
    }

    impl Panel {
        fn start(tokens: Vec<String>) -> Panel {
            Panel::build(tokens, false, None, Vec::new())
        }

        /// A panel whose one tuner also records conventional channels.
        fn with_channels(channels: Vec<crate::pipeline::control::ChannelDescriptor>) -> Panel {
            Panel::build(Vec::new(), false, None, channels)
        }

        /// A panel with the live audio monitor switched on, and the hub kept so
        /// the test can publish into it.
        fn with_monitor() -> (Panel, Arc<crate::pipeline::monitor::MonitorHub>) {
            let hub = crate::pipeline::monitor::MonitorHub::new();
            (
                Panel::build(Vec::new(), false, Some(Arc::clone(&hub)), Vec::new()),
                hub,
            )
        }

        /// A panel whose config came from a real file, so the write routes have
        /// something to write.
        fn with_config(tokens: Vec<String>) -> Panel {
            Panel::build(tokens, true, None, Vec::new())
        }

        fn build(
            tokens: Vec<String>,
            with_file: bool,
            monitor: Option<Arc<crate::pipeline::monitor::MonitorHub>>,
            channels: Vec<crate::pipeline::control::ChannelDescriptor>,
        ) -> Panel {
            let metrics = trunkd_core::Metrics::new();
            // Enough of a site for the sites[] block to have real numbers.
            metrics
                .gauge("site.0.control_frequency_hz")
                .set(851_050_000);
            metrics.gauge("site.0.nac").set(0x2f1);
            metrics.gauge("site.0.tsbk_rate").set_f64(17.5);
            metrics.gauge("site.0.active_calls").set(2);
            metrics.counter("calls.started").add(7);
            metrics.counter("site.0.calls_tdma_refused").add(3);
            metrics.counter("calls.tdma_refused").add(3);
            let shared = Shared {
                private_calls: Default::default(),
                calls: Default::default(),
                skip: trunkd_core::SkipList::empty(),
                replay_clock: Default::default(),
                metrics,
                sites: Arc::new(vec![SiteSpec {
                    system_label: "Example".into(),
                    site_label: "Central".into(),
                    control_frequencies: vec![851_050_000],
                    metric_prefix: "site.0".into(),
                    display: "Example/Central".into(),
                }]),
            };
            let devices = vec![dev("hackrf-0")];
            let desired = DesiredState::from_config(&Config {
                device: devices.clone(),
                ..Config::default()
            });
            let mut cfg = Config::default();
            cfg.uplink.api_key = "kup_supersecret".into();
            cfg.uplink.url = "http://keyup.example/api/call-upload".into();
            cfg.link.key = "link-hmac-supersecret".into();
            cfg.server.tokens = tokens.clone();
            let recorder =
                RecorderControl::for_test(Arc::new(trunkd_uplink::UplinkHold::new(false)));

            // A per-test scratch config, so the write routes exercise a real
            // file (and a real `.bak`) rather than a mock.
            let dir = with_file.then(|| {
                let d = std::env::temp_dir().join(format!(
                    "trunkd-panel-{}-{:?}",
                    std::process::id(),
                    std::thread::current().id()
                ));
                let _ = std::fs::remove_dir_all(&d);
                std::fs::create_dir_all(&d).expect("a scratch dir");
                std::fs::write(d.join("trunkd.toml"), CONFIG_FILE).expect("the config file");
                d
            });
            let config_path = dir.as_ref().map(|d| d.join("trunkd.toml"));
            let reload = Arc::new(AtomicBool::new(false));
            let restart_pending = Arc::new(AtomicBool::new(false));
            // The devices the panel's *running* config has, matching the file
            // so the fallback and the on-disk answers can be compared.
            let running = match &config_path {
                Some(p) => Config::load(p).expect("the scratch config loads"),
                None => Config {
                    device: devices.clone(),
                    ..cfg.clone()
                },
            };
            let kept = shared.clone();
            // One slot, matching the one site above; empty until a test seeds
            // it, exactly as it is before the observations thread's first pass.
            let site_details = crate::pipeline::observations::SiteDetails::new(1);
            let state = PanelState {
                link: crate::pipeline::link::LinkSlot::disabled(),
                shared,
                descriptors: vec![TunerDescriptor {
                    id: "hackrf-0".into(),
                    serial: None,
                    backend: Backend::Hackrf,
                    driver: None,
                    sample_rate: 20_000_000,
                    site_label: Some("Central".into()),
                    site_metric_prefix: Some("site.0".into()),
                    description: Default::default(),
                    channels,
                }],
                desired: desired.clone(),
                units: Vec::new(),
                tokens,
                origins: OriginPolicy::new(&cfg.server),
                instance_id: "panel-test".into(),
                station: Station::default(),
                config_json: redacted_config(&cfg),
                config: Arc::new(redact(&running)),
                config_path: config_path.clone(),
                reload: reload.clone(),
                restart_pending: restart_pending.clone(),
                enumerate: Arc::new(fake_hardware),
                recorder: recorder.clone(),
                upload_url: cfg.uplink.url.clone(),
                started: Instant::now(),
                event_log: crate::pipeline::eventarchive::ArchiveState::new(false),
                about: crate::about::About::new(&cfg),
                stream: crate::pipeline::stream::StreamState::new(false),
                // Off unless a test asked for it: that is the default, and
                // the state the monitor routes must 404 from.
                monitor,
                site_details: site_details.clone(),
            };
            let listener =
                std::net::TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral port");
            let bound = listener.local_addr().expect("an ip listener");
            let base = format!("http://{bound}");
            // What `run` does once it has bound.
            let mut state = state;
            state.origins.port = bound.port();
            let stop = Arc::new(AtomicBool::new(false));
            let thread = {
                let stop = stop.clone();
                std::thread::spawn(move || serve(listener, Arc::new(state), stop, Limits::PANEL))
            };
            Panel {
                base,
                stop,
                desired,
                recorder,
                reload,
                restart_pending,
                dir,
                shared: kept,
                details: site_details,
                thread: Some(thread),
            }
        }

        /// The observations thread's slot for site 0, for seeding a profile.
        fn site_details(&self) -> Arc<crate::pipeline::observations::SiteDetails> {
            self.details.clone()
        }

        /// The config file's current text.
        fn config_text(&self) -> String {
            let dir = self.dir.as_ref().expect("a panel with a config file");
            std::fs::read_to_string(dir.join("trunkd.toml")).expect("the config file")
        }

        fn agent() -> ureq::Agent {
            ureq::Agent::config_builder()
                .timeout_global(Some(Duration::from_secs(5)))
                // A 401 or a 404 is an answer here, not a transport failure.
                .http_status_as_error(false)
                .build()
                .into()
        }

        fn get(&self, path: &str, token: Option<&str>) -> (u16, Value) {
            let mut rq = Panel::agent().get(format!("{}{path}", self.base));
            if let Some(t) = token {
                rq = rq.header("Authorization", &format!("Bearer {t}"));
            }
            let mut resp = rq.call().expect("the panel answers");
            let status = resp.status().as_u16();
            let body = resp.body_mut().read_to_string().expect("a body");
            (status, serde_json::from_str(&body).unwrap_or(Value::Null))
        }

        fn post_json(&self, path: &str, token: Option<&str>, body: Value) -> (u16, Value) {
            let mut rq = Panel::agent().post(format!("{}{path}", self.base));
            if let Some(t) = token {
                rq = rq.header("Authorization", &format!("Bearer {t}"));
            }
            let mut resp = rq
                .header("Content-Type", "application/json")
                .send(body.to_string())
                .expect("the panel answers");
            let status = resp.status().as_u16();
            let text = resp.body_mut().read_to_string().expect("a body");
            (status, serde_json::from_str(&text).unwrap_or(Value::Null))
        }

        fn put_json(&self, path: &str, token: Option<&str>, body: Value) -> (u16, Value) {
            let mut rq = Panel::agent().put(format!("{}{path}", self.base));
            if let Some(t) = token {
                rq = rq.header("Authorization", &format!("Bearer {t}"));
            }
            let mut resp = rq
                .header("Content-Type", "application/json")
                .send(body.to_string())
                .expect("the panel answers");
            let status = resp.status().as_u16();
            let text = resp.body_mut().read_to_string().expect("a body");
            (status, serde_json::from_str(&text).unwrap_or(Value::Null))
        }

        fn delete(&self, path: &str, token: Option<&str>) -> (u16, Value) {
            let mut rq = Panel::agent().delete(format!("{}{path}", self.base));
            if let Some(t) = token {
                rq = rq.header("Authorization", &format!("Bearer {t}"));
            }
            let mut resp = rq.call().expect("the panel answers");
            let status = resp.status().as_u16();
            let text = resp.body_mut().read_to_string().expect("a body");
            (status, serde_json::from_str(&text).unwrap_or(Value::Null))
        }

        /// An empty-bodied POST carrying an `Origin`, as a browser sends one.
        fn post_from(&self, path: &str, origin: &str, body: Value) -> (u16, Value) {
            let mut resp = Panel::agent()
                .post(format!("{}{path}", self.base))
                .header("Origin", origin)
                .header("Content-Type", "text/plain")
                .send(body.to_string())
                .expect("the panel answers");
            let status = resp.status().as_u16();
            let text = resp.body_mut().read_to_string().expect("a body");
            (status, serde_json::from_str(&text).unwrap_or(Value::Null))
        }

        /// A PUT carrying an `Origin`.
        fn put_from(&self, path: &str, origin: &str, body: Value) -> (u16, Value) {
            let mut resp = Panel::agent()
                .put(format!("{}{path}", self.base))
                .header("Origin", origin)
                .header("Content-Type", "application/json")
                .send(body.to_string())
                .expect("the panel answers");
            let status = resp.status().as_u16();
            let text = resp.body_mut().read_to_string().expect("a body");
            (status, serde_json::from_str(&text).unwrap_or(Value::Null))
        }

        /// The panel's own origin, as its page would send it.
        fn own_origin(&self) -> String {
            self.base.clone()
        }

        /// The `host:port` the panel listens on.
        fn authority(&self) -> &str {
            self.base.trim_start_matches("http://")
        }

        /// Send `request` verbatim on a fresh connection and read the whole
        /// answer (the panel always closes after one).
        fn raw(&self, request: &str) -> String {
            use std::io::{Read as _, Write as _};
            let mut s = std::net::TcpStream::connect(self.authority()).expect("connect");
            s.set_read_timeout(Some(Duration::from_secs(5)))
                .expect("a read timeout");
            s.write_all(request.as_bytes()).expect("send");
            let mut out = Vec::new();
            let _ = s.read_to_end(&mut out);
            String::from_utf8_lossy(&out).into_owned()
        }

        fn post(&self, path: &str, token: Option<&str>) -> (u16, Value) {
            let mut rq = Panel::agent().post(format!("{}{path}", self.base));
            if let Some(t) = token {
                rq = rq.header("Authorization", &format!("Bearer {t}"));
            }
            let mut resp = rq.send_empty().expect("the panel answers");
            let status = resp.status().as_u16();
            let body = resp.body_mut().read_to_string().expect("a body");
            (status, serde_json::from_str(&body).unwrap_or(Value::Null))
        }
    }

    impl Drop for Panel {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
            if let Some(d) = &self.dir {
                let _ = std::fs::remove_dir_all(d);
            }
        }
    }

    #[test]
    fn the_page_is_served_without_a_token_even_when_tokens_are_set() {
        let panel = Panel::start(vec!["s3cret".into()]);
        let mut resp = Panel::agent()
            .get(format!("{}/", panel.base))
            .call()
            .expect("the panel answers");
        assert_eq!(resp.status().as_u16(), 200);
        let body = resp.body_mut().read_to_string().expect("a body");
        assert!(body.contains("/api/status"), "the page drives the API");
    }

    /// The monitor is off in the default config the panel tests run with, and
    /// "off" has to be an answer the page can read rather than a hang: a 404
    /// with a detail saying which setting turns it on.
    #[test]
    fn the_monitor_is_off_and_says_so() {
        let panel = Panel::start(Vec::new());
        let (status, body) = panel.get("/api/monitor/status", None);
        assert_eq!(status, 200);
        assert_eq!(body["enabled"], false);
        assert_eq!(body["streams"], 0);
        assert_eq!(body["max_streams"], crate::pipeline::monitor::MAX_STREAMS);

        let (status, body) = panel.get("/api/monitor/stream", None);
        assert_eq!(status, 404, "a disabled monitor must refuse, not stream");
        assert!(
            body["detail"]
                .as_str()
                .unwrap_or_default()
                .contains("live_audio"),
            "the 404 does not name the setting: {body}"
        );

        // And the status body carries the same fact, so the page needs one
        // request rather than two to decide whether to show the card.
        let (_, body) = panel.get("/api/status", None);
        assert_eq!(body["monitor"]["enabled"], false);
        assert_eq!(body["monitor_streams"], 0);
    }

    /// With tokens set the stream is a read like any other: no token, no audio.
    /// The header and the `?token=` form are both accepted, because an
    /// `<audio>` element can only send the second.
    #[test]
    fn the_monitor_stream_needs_a_token_when_one_is_set() {
        let panel = Panel::start(vec!["s3cret".into()]);
        let (status, _) = panel.get("/api/monitor/stream", None);
        assert_eq!(status, 401);
        // Authorised, but still disabled: the 404 is what proves the token was
        // accepted, since an unauthorised request never gets that far.
        let (status, _) = panel.get("/api/monitor/stream?token=s3cret", None);
        assert_eq!(status, 404);
        let (status, _) = panel.get("/api/monitor/stream", Some("s3cret"));
        assert_eq!(status, 404);
    }

    /// The one route that is not a buffer with a length: a real listener, a
    /// real socket, real MP3 frames, and a slot that comes back when they
    /// leave. Nothing here touches an SDR — the frames are published straight
    /// into the hub, exactly as the call thread would.
    #[test]
    fn the_monitor_streams_live_audio_and_frees_its_slot() {
        use std::io::Read as _;

        let (panel, hub) = Panel::with_monitor();
        let (_, body) = panel.get("/api/monitor/status", None);
        assert_eq!(body["enabled"], true);

        // A talker, until the test says stop.
        let stop = Arc::new(AtomicBool::new(false));
        let feeder = {
            let hub = Arc::clone(&hub);
            let stop = stop.clone();
            std::thread::spawn(move || {
                let meta = crate::pipeline::monitor::CallAudioMeta::new(
                    1,
                    Some(52101),
                    trunkd_p25::event::CallKind::Group,
                    1234,
                    Some("hackrf-0"),
                    851_050_000,
                );
                // A 1 kHz tone at 8 kHz, one 20 ms frame at a time.
                let mut phase = 0usize;
                while !stop.load(Ordering::Relaxed) {
                    let frame: Vec<i16> = (0..160)
                        .map(|i| {
                            let t = (phase + i) as f64 / 8000.0;
                            (8000.0 * (2.0 * std::f64::consts::PI * 1000.0 * t).sin()) as i16
                        })
                        .collect();
                    phase += 160;
                    hub.publish(&meta, &frame);
                    std::thread::sleep(Duration::from_millis(20));
                }
            })
        };

        let mut resp = Panel::agent()
            .get(format!("{}/api/monitor/stream?talkgroup=52101", panel.base))
            .call()
            .expect("the panel answers");
        assert_eq!(resp.status().as_u16(), 200);
        assert_eq!(
            resp.headers()
                .get("Content-Type")
                .and_then(|h| h.to_str().ok()),
            Some("audio/mpeg")
        );
        // No Content-Length: a live stream has no length, and saying one would
        // be the difference between playing and hanging.
        assert!(resp.headers().get("Content-Length").is_none());

        let mut got = Vec::new();
        let mut reader = resp.body_mut().as_reader();
        let mut chunk = [0u8; 256];
        while got.len() < 512 {
            let n = reader.read(&mut chunk).expect("the stream reads");
            assert!(n > 0, "the stream ended while a call was on the air");
            got.extend_from_slice(&chunk[..n]);
        }
        assert_eq!(got[0], 0xFF, "no mpeg frame sync: {:02X?}", &got[..4]);
        assert_eq!(got[1] & 0xE0, 0xE0, "no mpeg frame sync");
        assert_eq!(hub.streams(), 1, "the stream is not counted");

        // Hanging up frees the slot: the write fails, the reader drops, the
        // subscription goes with it.
        drop(reader);
        drop(resp);
        stop.store(true, Ordering::Relaxed);
        feeder.join().expect("the feeder thread");

        let freed = std::time::Instant::now();
        while hub.streams() != 0 {
            assert!(
                freed.elapsed() < Duration::from_secs(5),
                "the stream slot was never freed"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// None of the ways a client can stall a connection stalls the panel:
    /// a live stream with a request pipelined behind it (which used to hang
    /// the only request thread in `respond`), the page requested over and
    /// over on one connection by a client that reads nothing, and a request
    /// head that never finishes. `/api/status` still answers at once.
    #[test]
    fn a_stalled_client_does_not_block_the_status_route() {
        use std::io::Write as _;

        let (panel, hub) = Panel::with_monitor();
        let host = panel.authority().to_owned();
        let connect = || std::net::TcpStream::connect(&host).expect("connect");

        let mut pipelined = connect();
        pipelined
            .write_all(
                format!(
                    "GET /api/monitor/stream HTTP/1.1\r\nHost: {host}\r\n\r\n\
                     GET / HTTP/1.1\r\nHost: {host}\r\n\r\n"
                )
                .as_bytes(),
            )
            .expect("send");
        let mut greedy = connect();
        let page = format!("GET / HTTP/1.1\r\nHost: {host}\r\n\r\n");
        greedy.write_all(page.repeat(200).as_bytes()).expect("send");
        let mut trickle = connect();
        trickle
            .write_all(b"GET /api/status HTTP/1.1\r\nHost: ")
            .expect("send");

        // The stream is up (so the pipelined request really is queued behind
        // an endless response) before anyone else asks.
        let up = Instant::now();
        while hub.streams() == 0 {
            assert!(
                up.elapsed() < Duration::from_secs(5),
                "the stream never started"
            );
            std::thread::sleep(Duration::from_millis(10));
        }

        for _ in 0..3 {
            let asked = Instant::now();
            let (status, body) = panel.get("/api/status", None);
            assert_eq!(status, 200);
            assert!(body.get("uptime_s").is_some());
            assert!(
                asked.elapsed() < Duration::from_secs(2),
                "status took {:?} behind stalled clients",
                asked.elapsed()
            );
        }
        drop((pipelined, greedy, trickle));
    }

    /// HTTP/1.0 has no chunked encoding, and the old server answered one by
    /// buffering the endless stream to learn its length. Now it is a prompt
    /// 400 that says why, and no stream slot is taken.
    #[test]
    fn an_http10_stream_request_gets_a_prompt_refusal() {
        let (panel, hub) = Panel::with_monitor();
        let asked = Instant::now();
        let answer = panel.raw("GET /api/monitor/stream HTTP/1.0\r\n\r\n");
        assert!(answer.starts_with("HTTP/1.1 400 "), "{answer}");
        assert!(
            answer.contains("HTTP/1.1"),
            "the refusal says what is needed"
        );
        assert!(asked.elapsed() < Duration::from_secs(2));
        assert_eq!(hub.streams(), 0, "a refused request took a stream slot");
        // With a Host it is the same answer.
        let answer = panel.raw(&format!(
            "GET /api/monitor/stream HTTP/1.0\r\nHost: {}\r\n\r\n",
            panel.authority()
        ));
        assert!(answer.starts_with("HTTP/1.1 400 "), "{answer}");
    }

    /// A DNS-rebinding page reads with its own name in `Host`; the panel
    /// answers only to names that are its own — on reads and the page too.
    #[test]
    fn a_foreign_host_is_refused_even_on_a_read() {
        let panel = Panel::start(Vec::new());
        let port = panel.base.rsplit(':').next().expect("a port").to_owned();
        for path in ["/api/status", "/api/config", "/", "/api/monitor/stream"] {
            let answer = panel.raw(&format!(
                "GET {path} HTTP/1.1\r\nHost: rebind.evil.example:{port}\r\n\r\n"
            ));
            assert!(answer.starts_with("HTTP/1.1 421 "), "{path}: {answer}");
            assert!(!answer.contains("uptime_s"), "{path} leaked the body");
        }
        for host in [
            format!("127.0.0.1:{port}"),
            format!("localhost:{port}"),
            format!("LOCALHOST:{port}"),
        ] {
            let answer = panel.raw(&format!("GET /api/status HTTP/1.1\r\nHost: {host}\r\n\r\n"));
            assert!(answer.starts_with("HTTP/1.1 200 "), "{host}: {answer}");
        }
    }

    #[test]
    fn the_host_rule_follows_the_bind_and_the_public_url() {
        let server = |bind: &str, public: Option<&str>| crate::config::Server {
            bind: bind.into(),
            public_url: public.map(str::to_owned),
            ..crate::config::Server::default()
        };

        let p = OriginPolicy::new(&server("127.0.0.1:8095", None));
        for ok in [
            "127.0.0.1:8095",
            "127.0.0.1",
            "localhost:8095",
            "[::1]:8095",
            "Localhost",
        ] {
            assert!(p.allows_host(ok), "{ok}");
        }
        for bad in [
            "192.0.2.30:8095",
            "evil.example:8095",
            "localhost.evil.example",
            "",
            "127.0.0.1/x",
            "user@127.0.0.1",
            "[::1",
        ] {
            assert!(!p.allows_host(bad), "{bad:?}");
        }

        // A LAN bind adds its own address; a wildcard any IP literal, never a
        // name — which is all a rebinding page has.
        let p = OriginPolicy::new(&server("192.0.2.30:8095", None));
        assert!(p.allows_host("192.0.2.30:8095"));
        assert!(!p.allows_host("192.0.2.31:8095"));
        let p = OriginPolicy::new(&server("0.0.0.0:8095", None));
        assert!(p.allows_host("192.0.2.30:8095"));
        assert!(p.allows_host("[fe80::1]:8095"));
        assert!(!p.allows_host("recorder.lan:8095"));

        // public_url's host, by whatever port a proxy forwards.
        let p = OriginPolicy::new(&server(
            "127.0.0.1:8095",
            Some("https://Recorder.example/panel/"),
        ));
        assert!(p.allows_host("recorder.example"));
        assert!(p.allows_host("recorder.example:443"));
        assert!(p.allows_host("recorder.example:8095"));
        assert!(!p.allows_host("recorder.example.evil.example"));

        // This machine's own name, bare or mDNS, agrees with the Origin rule.
        if let Some(name) = crate::about::host::hostname() {
            let name = name.to_ascii_lowercase();
            if !name.contains(['/', ':', '@', ' ']) {
                assert!(p.allows_host(&format!("{name}:8095")), "{name}");
                assert!(p.allows_host(&format!("{name}.local")), "{name}");
                assert!(p.allows(&format!("http://{name}:8095")), "{name}");
            }
        }
    }

    /// Nothing may frame the panel: every answer says so, refusals included.
    #[test]
    fn every_answer_forbids_framing() {
        let panel = Panel::start(vec!["s3cret".into()]);
        for path in ["/", "/about", "/api/status", "/no/such/route"] {
            let resp = Panel::agent()
                .get(format!("{}{path}", panel.base))
                .call()
                .expect("the panel answers");
            let h = |name: &str| {
                resp.headers()
                    .get(name)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_owned)
            };
            assert_eq!(h("X-Frame-Options").as_deref(), Some("DENY"), "{path}");
            assert_eq!(
                h("Content-Security-Policy").as_deref(),
                Some("frame-ancestors 'none'"),
                "{path}"
            );
        }
        // And on the stream's refusals, which take their own path.
        let answer = panel.raw(&format!(
            "GET /api/monitor/stream HTTP/1.1\r\nHost: {}\r\n\r\n",
            panel.authority()
        ));
        assert!(answer.starts_with("HTTP/1.1 401 "), "{answer}");
        assert!(answer.contains("X-Frame-Options: DENY\r\n"), "{answer}");
    }

    /// A JSON body must say it is JSON: `text/plain`, a form encoding, or no
    /// type at all is what a cross-site page can send without a preflight.
    #[test]
    fn a_json_route_refuses_any_other_content_type() {
        let panel = Panel::with_config(Vec::new());
        let before = panel.config_text();
        let host = panel.authority().to_owned();
        let send = |method: &str, path: &str, ctype: Option<&str>, body: &str| {
            let ct = ctype
                .map(|c| format!("Content-Type: {c}\r\n"))
                .unwrap_or_default();
            panel.raw(&format!(
                "{method} {path} HTTP/1.1\r\nHost: {host}\r\n{ct}Content-Length: {}\r\n\r\n{body}",
                body.len()
            ))
        };
        let hold = r#"{"spool_only": true}"#;
        for ctype in [
            Some("text/plain"),
            Some("application/x-www-form-urlencoded"),
            Some("multipart/form-data; boundary=x"),
            None,
        ] {
            let answer = send("POST", "/api/uplink", ctype, hold);
            assert!(answer.starts_with("HTTP/1.1 415 "), "{ctype:?}: {answer}");
            let answer = send(
                "PUT",
                "/api/config/devices/airspy0",
                ctype,
                r#"{"ppm": 9.0}"#,
            );
            assert!(answer.starts_with("HTTP/1.1 415 "), "{ctype:?}: {answer}");
        }
        assert!(
            !panel.recorder.hold().is_held(),
            "a refused hold changed the hold"
        );
        assert_eq!(panel.config_text(), before, "a refused PUT wrote the file");

        // JSON, with or without parameters, in any case.
        let answer = send(
            "POST",
            "/api/uplink",
            Some("Application/JSON; charset=utf-8"),
            hold,
        );
        assert!(answer.starts_with("HTTP/1.1 200 "), "{answer}");
        assert!(panel.recorder.hold().is_held());
    }

    #[test]
    fn status_reports_the_whole_instance() {
        let panel = Panel::start(Vec::new());
        let (status, body) = panel.get("/api/status", None);
        assert_eq!(status, 200);
        for key in [
            "instance_id",
            "recorder_version",
            "uptime_s",
            "counters",
            "tuners",
            "supervisor",
            "sites",
        ] {
            assert!(
                body.get(key).is_some(),
                "status body is missing {key}: {body}"
            );
        }
        assert_eq!(body["instance_id"], "panel-test");
        let tuner = &body["tuners"][0];
        assert_eq!(tuner["tuner_key"], "hackrf-0");
        assert_eq!(tuner["pending"], false);
        // The site block comes straight off the gauges the DSP threads publish.
        assert_eq!(body["sites"][0]["control_frequency_hz"], 851_050_000_u64);
        assert_eq!(body["sites"][0]["nac"], 0x2f1);
        assert_eq!(body["counters"]["calls_started"], 7);
        // Phase 2 calls refused, per site and recorder-wide.
        assert_eq!(body["sites"][0]["calls_tdma_refused"], 3);
        assert_eq!(body["counters"]["calls_tdma_refused"], 3);
        // The skip list is reported on the recorder block whether or not
        // anything is on it: "nothing is skipped" is the answer for most
        // recorders and has to be tellable from "this build cannot skip".
        assert_eq!(body["recorder"]["skip_talkgroups"]["count"], 0);
        assert_eq!(body["recorder"]["skip_talkgroups"]["revision"], json!(null));
        assert_eq!(body["recorder"]["skip_talkgroups"]["local"], json!([]));
        // Private calls are their own list: nothing has happened here, so it is
        // present and empty rather than absent.
        assert_eq!(body["private_calls"], json!([]));
    }

    /// A P25-only recorder: `channels` is present and empty, which the page
    /// reads as "no table" — the section ships hidden and the renderer keeps it
    /// hidden for an empty list, so the panel looks exactly as it did before
    /// channels existed.
    #[test]
    fn a_recorder_without_channels_reports_none_and_the_page_hides_the_table() {
        let panel = Panel::start(Vec::new());
        let (status, body) = panel.get("/api/status", None);
        assert_eq!(status, 200);
        assert_eq!(body["channels"], json!([]));
        assert!(
            body["tuners"][0].get("channels").is_none(),
            "a P25-only tuner entry carries no channels field: {body}"
        );
        assert!(INDEX_HTML.contains(r#"<section id="convsec" hidden>"#));
        assert!(INDEX_HTML.contains(r#"$("convsec").hidden = rows.length === 0;"#));
        assert!(INDEX_HTML.contains("renderChannels(s.channels || []);"));
    }

    fn example_channel(
        index: usize,
        name: &str,
        tone: Option<&str>,
    ) -> crate::pipeline::control::ChannelDescriptor {
        crate::pipeline::control::ChannelDescriptor {
            index,
            name: name.into(),
            system: "Example County".into(),
            frequency_hz: 154_000_000 + index as u64 * 25_000,
            tone: tone.map(str::to_owned),
            talkgroup: 154_000 + index as u32 * 25,
            metric_prefix: format!("channel.{index}"),
        }
    }

    /// The channels table comes off the `channel.{i}.*` gauges, flattened off
    /// the tuner entries with the tuner's key as `device`, in channel order.
    #[test]
    fn status_lists_the_conventional_channels_from_their_gauges() {
        let panel = Panel::with_channels(vec![
            example_channel(1, "Fire Dispatch", Some("D445N")),
            example_channel(0, "Public Works", Some("167.9")),
        ]);
        let m = &panel.shared.metrics;
        let now = trunkd_core::Timestamp::now().unix_millis();
        m.gauge("device.hackrf-0.stream_beat_ms").set(now as u64);
        m.gauge("channel.0.state").set(2);
        m.gauge("channel.0.open").set(1);
        m.gauge("channel.0.snr_db").set_f64(21.5);
        m.gauge("channel.0.calls").set(4);
        m.gauge("channel.0.tone_mismatch").set(1);
        m.gauge("channel.0.false_opens").set(2);
        m.gauge("channel.0.last_call_end_ms").set(1_788_499_700_000);
        m.gauge("channel.1.snr_db").set_f64(0.5);

        let (status, body) = panel.get("/api/status", None);
        assert_eq!(status, 200);
        let rows = body["channels"].as_array().expect("a channels array");
        assert_eq!(rows.len(), 2);
        let pw = &rows[0];
        assert_eq!(pw["index"], 0);
        assert_eq!(pw["name"], "Public Works");
        assert_eq!(pw["system"], "Example County");
        assert_eq!(pw["device"], "hackrf-0");
        assert_eq!(pw["frequency_hz"], 154_000_000_u64);
        assert_eq!(pw["tone"], "167.9");
        assert_eq!(pw["state"], "open");
        assert_eq!(pw["open"], true);
        assert_eq!(pw["snr_db"], 21.5);
        assert_eq!(pw["calls"], 4);
        assert_eq!(pw["tone_mismatch"], 1);
        assert_eq!(pw["false_opens"], 2);
        assert_eq!(pw["last_call_end_ms"], 1_788_499_700_000_i64);
        let fd = &rows[1];
        assert_eq!(fd["name"], "Fire Dispatch");
        assert_eq!(fd["tone"], "D445N");
        assert_eq!(fd["state"], "closed");
        assert!(fd.get("last_call_end_ms").is_none(), "{fd}");
        // The same objects ride on the tuner entry, as keyup gets them.
        assert_eq!(
            body["tuners"][0]["channels"].as_array().map(Vec::len),
            Some(2)
        );
    }

    /// Seed site 0's slot with a real observation document, the way the
    /// observations thread publishes one: the fixture the observation tests
    /// assert on, through the same renderer.
    fn seed_site_detail(panel: &Panel) {
        panel.site_details().publish(
            0,
            crate::pipeline::observations::detail_body(
                &crate::pipeline::observations::sample_snapshot(),
                "Example Network",
                "Central",
                300,
                "2026-09-17T00:00:00Z".into(),
                json!({ "message_mix": { "0:56": 900, "164:15": 40, "164:2": 1 } }),
                "panel-test",
                851_050_000,
                &[crate::pipeline::observations::SiteIdentity {
                    system_id: Some(0x1F5),
                    rfss: Some(1),
                    site: Some(7),
                    label: "Example/Central".into(),
                }],
            ),
        );
    }

    /// An index that is not a configured site is a 404, not an empty profile:
    /// the panel's chevron builds the URL from the row, and a silently empty
    /// answer would hide a renumbered site rather than report it.
    #[test]
    fn a_site_detail_for_an_unknown_index_is_a_404() {
        let panel = Panel::start(Vec::new());
        let (status, body) = panel.get("/api/sites/7/detail", None);
        assert_eq!(status, 404);
        assert!(
            body["error"].as_str().unwrap_or_default().contains('7'),
            "the error names the index asked for, got {body}"
        );
        // Not a site index at all.
        assert_eq!(panel.get("/api/sites/x/detail", None).0, 404);
    }

    /// Before the observations thread's first pass there is no document, and
    /// the route says so rather than inventing one.
    #[test]
    fn a_site_with_no_observation_yet_is_a_503() {
        let panel = Panel::start(Vec::new());
        assert_eq!(panel.get("/api/sites/0/detail", None).0, 503);
    }

    /// The detail route is the observation document Keyup receives, plus the
    /// five things only a local operator asks for.
    #[test]
    fn a_site_detail_is_the_observation_document_plus_the_panel_extras() {
        let panel = Panel::start(Vec::new());
        seed_site_detail(&panel);
        let (status, body) = panel.get("/api/sites/0/detail", None);
        assert_eq!(status, 200);

        // One renderer: the observation document's own fields, unchanged.
        assert_eq!(body["recorder"], "trunkd");
        assert_eq!(body["system"]["wacn"], 0xBEE00);
        assert_eq!(body["system"]["nac"], 0x293);
        assert_eq!(body["site"]["rfss"], 1);
        assert_eq!(body["site"]["lra"], 3);
        assert_eq!(body["control"]["frequency_hz"], 851_050_000_u64);
        assert_eq!(
            body["services"]["available"].as_array().map(Vec::len),
            Some(8)
        );
        assert_eq!(
            body["secondary_control"][0]["frequency_hz"],
            852_137_500_u64
        );
        assert_eq!(body["bands"][0]["id"], 1);
        assert_eq!(body["traffic"][0]["grants"], 9);
        assert_eq!(body["adjacent_sites"][0]["site"], 9);

        // The panel's own additions.
        assert_eq!(body["site_label"], "Central");
        assert_eq!(body["system_label"], "Example Network");
        assert_eq!(body["locked"], true);
        assert_eq!(body["control_locked_hz"], 851_050_000_u64);
        assert_eq!(body["recorder_sites"][0]["label"], "Example/Central");

        // 0:56 is named, 164:15 recurs unnamed, 164:2 was seen once.
        assert_eq!(body["message_kinds"]["known"], 1);
        assert_eq!(body["message_kinds"]["unknown"], 1);
        assert_eq!(body["message_kinds"]["noise"], 1);
        assert_eq!(body["message_kinds"]["top_unknown"][0]["mfid"], 164);
        assert_eq!(body["message_kinds"]["top_unknown"][0]["opcode"], 15);
        assert_eq!(body["message_kinds"]["top_unknown"][0]["count"], 40);

        // The neighbour in the fixture is 1F5/1-9; this recorder decodes
        // 1F5/1-7, so the neighbour is one it cannot hear for itself.
        let unknowns: Vec<&str> = body["unknowns"]
            .as_array()
            .expect("a list of unknowns")
            .iter()
            .map(|v| v.as_str().unwrap_or_default())
            .collect();
        assert!(
            unknowns.iter().any(|u| u.contains("1F5/1-9")),
            "the un-decoded neighbour is listed, got {unknowns:?}"
        );
        assert!(
            unknowns
                .iter()
                .any(|u| u.contains("recurring message kind")),
            "the recurring unknown kind is listed, got {unknowns:?}"
        );
    }

    /// The Sites row carries the one-line services summary, so the table says
    /// something about the profile before anyone expands it.
    #[test]
    fn status_summarises_the_services_a_site_advertises() {
        let panel = Panel::start(Vec::new());
        assert_eq!(
            panel.get("/api/status", None).1["sites"][0]["services_available"],
            json!(null),
            "no observation document yet"
        );
        seed_site_detail(&panel);
        assert_eq!(
            panel.get("/api/status", None).1["sites"][0]["services_available"],
            8
        );
    }

    /// The detail route reads like `/api/status`: a token, when one is set.
    #[test]
    fn a_site_detail_needs_the_read_token() {
        let panel = Panel::start(vec!["s3cret".into()]);
        seed_site_detail(&panel);
        assert_eq!(panel.get("/api/sites/0/detail", None).0, 401);
        assert_eq!(panel.get("/api/sites/0/detail", Some("s3cret")).0, 200);
    }

    /// The panel's private-call list is what the call thread put in the shared
    /// block, rendered as the pair, the channel and whether it is still up.
    #[test]
    fn status_shows_the_private_calls_the_call_thread_reported() {
        let panel = Panel::start(Vec::new());
        panel
            .shared
            .private_calls
            .started(1, 9_000_001, 9_000_007, 851_125_000, 1_788_012_000_000);
        panel
            .shared
            .private_calls
            .started(2, 9_000_007, 9_000_001, 851_125_000, 1_788_012_005_000);
        panel.shared.private_calls.ended(1, 1_788_012_004_000);

        let (_, body) = panel.get("/api/status", None);
        let rows = body["private_calls"].as_array().expect("a list");
        assert_eq!(rows.len(), 2);
        // Newest first: the second half of the conversation, still on the air.
        assert_eq!(rows[0]["source"], 9_000_007);
        assert_eq!(rows[0]["target"], 9_000_001);
        assert_eq!(rows[0]["active"], true);
        assert_eq!(rows[0]["ended_at_ms"], Value::Null);
        assert_eq!(rows[1]["source"], 9_000_001);
        assert_eq!(rows[1]["frequency_hz"], 851_125_000u64);
        assert_eq!(rows[1]["active"], false);
        assert_eq!(rows[1]["ended_at_ms"], 1_788_012_004_000i64);
    }

    /// Nothing decoded yet: both lists are present and empty, so the page can
    /// render the Activity tables without testing for an absent key.
    #[test]
    fn status_calls_are_two_empty_lists_before_anything_is_heard() {
        let panel = Panel::start(Vec::new());
        let (_, body) = panel.get("/api/status", None);
        assert_eq!(body["calls"]["active"], json!([]));
        assert_eq!(body["calls"]["recent"], json!([]));
        assert_eq!(body["calls"]["queue_dropped"], 0);
        assert_eq!(body["calls"]["orphaned"], 0);
    }

    /// The call thread's health counters reach `/api/status`: messages the
    /// DSP → call queue evicted, and the calls closed as orphaned because of it.
    #[test]
    fn status_reports_call_queue_drops_and_orphaned_calls() {
        let panel = Panel::start(Vec::new());
        let metrics = &panel.shared.metrics;
        metrics
            .counter(crate::pipeline::calls::QUEUE_DROPPED)
            .add(12);
        metrics.counter(crate::pipeline::calls::ORPHANED).add(1);
        let (_, body) = panel.get("/api/status", None);
        assert_eq!(body["calls"]["queue_dropped"], 12);
        assert_eq!(body["calls"]["orphaned"], 1);
    }

    /// A call on the air shows up as active with no end fields, and the alias
    /// and encryption the call thread reported after the grant are on it.
    #[test]
    fn status_shows_a_call_that_is_still_on_the_air() {
        let panel = Panel::start(Vec::new());
        panel.shared.calls.started(crate::pipeline::CallStart {
            id: 7,
            site: "County/Simulcast".into(),
            kind: "group",
            talkgroup: Some(41_601),
            source: 9_000_001,
            target: None,
            frequency_hz: 851_125_000,
            started_at_ms: 1_788_012_000_000,
        });
        panel.shared.calls.talker(7, 9_000_020, Some("UNIT_A1"));
        panel.shared.calls.encrypted(7);

        let (_, body) = panel.get("/api/status", None);
        let active = body["calls"]["active"].as_array().expect("a list");
        assert_eq!(active.len(), 1);
        let r = &active[0];
        assert_eq!(r["talkgroup"], 41_601);
        assert_eq!(r["kind"], "group");
        assert_eq!(r["site"], "County/Simulcast");
        assert_eq!(r["alias"], "UNIT_A1");
        assert_eq!(r["source"], 9_000_020, "the latest talker, not the grant's");
        assert_eq!(r["encrypted"], true);
        assert_eq!(r["active"], true);
        assert_eq!(r["ended_at_ms"], Value::Null);
        assert_eq!(r["outcome"], Value::Null);
        assert_eq!(body["calls"]["recent"], json!([]));
    }

    /// Ending a call moves it off `active` and onto `recent`, newest first,
    /// carrying the duration, the frame count, the teardown reason and what
    /// this recorder did with it.
    #[test]
    fn status_shows_ended_calls_newest_first_with_their_outcome() {
        let panel = Panel::start(Vec::new());
        for (id, tg, at) in [
            (1u64, 41_601u32, 1_788_012_000_000i64),
            (2, 50_061, 1_788_012_010_000),
        ] {
            panel.shared.calls.started(crate::pipeline::CallStart {
                id,
                site: "County/Simulcast".into(),
                kind: "group",
                talkgroup: Some(tg),
                source: 9_000_001,
                target: None,
                frequency_hz: 851_125_000,
                started_at_ms: at,
            });
        }
        panel
            .shared
            .calls
            .ended(1, 1_788_012_004_000, 180, "terminator");
        panel.shared.calls.outcome(1, "delivered");
        panel
            .shared
            .calls
            .ended(2, 1_788_012_012_000, 0, "inactivity");
        panel.shared.calls.outcome(2, "no voice");

        let (_, body) = panel.get("/api/status", None);
        assert_eq!(body["calls"]["active"], json!([]));
        let recent = body["calls"]["recent"].as_array().expect("a list");
        assert_eq!(recent.len(), 2);
        // Newest first: the second call, which ended last.
        assert_eq!(recent[0]["talkgroup"], 50_061);
        assert_eq!(recent[0]["outcome"], "no voice");
        assert_eq!(recent[0]["reason"], "inactivity");
        assert_eq!(recent[0]["voice_frames"], 0);
        assert_eq!(recent[1]["talkgroup"], 41_601);
        assert_eq!(recent[1]["active"], false);
        assert_eq!(recent[1]["ended_at_ms"], 1_788_012_004_000i64);
        assert_eq!(recent[1]["voice_frames"], 180);
        assert_eq!(recent[1]["outcome"], "delivered");
    }

    #[test]
    fn status_shows_the_bands_in_force_because_it_reuses_the_keyup_builders() {
        // The panel gets every keyup status field for free; this pins that for
        // `bands`, which is the field an operator will look at to check that a
        // band push actually took.
        let panel = Panel::start(Vec::new());
        assert!(
            panel.get("/api/status", None).1["tuners"][0]
                .get("bands")
                .is_none(),
            "unrestricted at boot, so no bands field"
        );

        // Push a restriction and let the "rebuilt" unit confirm it opened.
        let desired = crate::pipeline::control::DesiredTuner {
            version: 1,
            role: None,
            site: None,
            site_id: None,
            center_frequency: None,
            bands: Some(vec!["856000000-866000000".to_owned()]),
            enabled: true,
            spectrum: false,
            ..Default::default()
        };
        panel.desired.apply_desired("hackrf-0", &desired);
        let built = panel.desired.take_for_build("hackrf-0");
        panel
            .desired
            .confirm_applied("hackrf-0", &built.config, built.version);

        let (status, body) = panel.get("/api/status", None);
        assert_eq!(status, 200);
        assert_eq!(body["tuners"][0]["bands"][0], "856000000-866000000");
    }

    #[test]
    fn status_says_which_tuner_fields_keyup_owns_rather_than_the_file() {
        // The page marks the rows keyup set, so the body has to carry them.
        // Empty before any push — and present, not absent, because "no
        // overrides" and "a recorder too old to say" must not look alike.
        let panel = Panel::start(Vec::new());
        let before = panel.get("/api/status", None).1;
        assert_eq!(before["tuners"][0]["overrides"], json!([]));
        assert!(before["tuners"][0].get("file_role").is_none());

        // The file leaves this tuner on the default role; keyup pushes
        // `control`. Without the marker the operator sees a control tuner they
        // never asked for and no way to tell where it came from.
        let desired = crate::pipeline::control::DesiredTuner {
            version: 3,
            role: Some(crate::config::DeviceRole::Control),
            enabled: true,
            ..Default::default()
        };
        panel.desired.apply_desired("hackrf-0", &desired);
        let built = panel.desired.take_for_build("hackrf-0");
        panel
            .desired
            .confirm_applied("hackrf-0", &built.config, built.version);

        let (status, body) = panel.get("/api/status", None);
        assert_eq!(status, 200);
        assert_eq!(body["tuners"][0]["role"], "control");
        assert_eq!(body["tuners"][0]["overrides"], json!(["role"]));
        assert_eq!(body["tuners"][0]["file_role"], "auto");
    }

    #[test]
    fn the_about_page_and_its_api_are_served() {
        let panel = Panel::start(vec!["s3cret".into()]);

        // The page is a shell with no data in it, so it is served like `/`:
        // an operator who cannot load it has no way to discover that a token
        // is what they are missing.
        let mut resp = Panel::agent()
            .get(format!("{}/about", panel.base))
            .call()
            .expect("the panel answers");
        assert_eq!(resp.status().as_u16(), 200);
        assert!(resp
            .body_mut()
            .read_to_string()
            .expect("a body")
            .contains("/api/about"));

        // The data behind it answers to the same auth rule as every other API
        // route, and carries every documented section.
        assert_eq!(panel.get("/api/about", None).0, 401);
        let (status, body) = panel.get("/api/about", Some("s3cret"));
        assert_eq!(status, 200);
        for key in [
            "schema_version",
            "build",
            "dependencies",
            "libraries",
            "process",
            "config",
        ] {
            assert!(body.get(key).is_some(), "missing {key}: {body}");
        }
        assert_eq!(body["schema_version"], crate::about::SCHEMA_VERSION);
        assert_eq!(body["process"]["pid"], std::process::id());
        assert!(body["dependencies"]["count"]
            .as_u64()
            .is_some_and(|c| c > 0));

        // The config the panel was built with has a live upload key in it.
        let text = body.to_string();
        assert!(
            !text.contains("kup_supersecret") && !text.contains("s3cret"),
            "a secret reached /api/about: {text}"
        );

        assert_eq!(panel.post("/api/about", Some("s3cret")).0, 405);
    }

    #[test]
    fn the_config_dump_carries_no_secret() {
        let panel = Panel::start(vec!["s3cret".into()]);
        let (status, body) = panel.get("/api/config", Some("s3cret"));
        assert_eq!(status, 200);
        assert_eq!(body["uplink"]["api_key"], "***");
        assert_eq!(body["server"]["tokens"][0], "***");
        let text = body.to_string();
        assert!(
            !text.contains("kup_supersecret") && !text.contains("s3cret"),
            "a secret survived serialization: {text}"
        );
    }

    #[test]
    fn the_config_dump_redacts_the_link_key_too() {
        // The recorder link's HMAC key authenticates "record this frequency"
        // datagrams; it was once served in clear text here.
        let panel = Panel::start(Vec::new());
        let (status, body) = panel.get("/api/config", None);
        assert_eq!(status, 200);
        assert_eq!(body["link"]["key"], "***");
        let text = body.to_string();
        assert!(
            !text.contains("link-hmac-supersecret"),
            "the link key survived: {text}"
        );
    }

    #[test]
    fn redaction_covers_every_secret_the_config_can_hold() {
        let mut cfg = Config::default();
        cfg.uplink.api_key = "kup_live_x".into();
        cfg.server.tokens = vec!["tok-one".into()];
        cfg.link.key = "link-shared-secret-value".into();
        cfg.device = vec![toml::from_str(
            "id = \"airspy-0\"\nbackend = \"soapy\"\nsample_rate = 10000000\n\
             [settings]\napi_token = \"driver-token-value\"\nbiastee = \"false\"\n",
        )
        .expect("a valid device")];

        let typed = redact(&cfg);
        assert_eq!(typed.uplink.api_key, "***");
        assert_eq!(typed.server.tokens, vec!["***".to_owned()]);
        assert_eq!(typed.link.key, "***");
        assert_eq!(typed.device[0].settings["api_token"], "***");
        assert_eq!(
            typed.device[0].settings["biastee"], "false",
            "an ordinary driver setting is left alone"
        );

        let json = redacted_config(&cfg).to_string();
        for secret in [
            "kup_live_x",
            "tok-one",
            "link-shared-secret-value",
            "driver-token-value",
        ] {
            assert!(!json.contains(secret), "{secret:?} survived: {json}");
        }
    }

    #[test]
    fn a_secret_driver_setting_is_redacted_in_the_editor_and_survives_a_save() {
        let panel = Panel::with_config(Vec::new());
        let (status, body) = panel.put_json(
            "/api/config/devices/airspy0",
            None,
            json!({ "settings": { "api_token": "drv-secret", "biastee": "false" } }),
        );
        assert_eq!(status, 200, "{body}");

        // The editor's read shows the name, not the value.
        let listed = panel.get("/api/config/devices", None).1;
        let settings = listed["devices"][0]["settings"].clone();
        assert_eq!(settings["api_token"], "***");
        assert_eq!(settings["biastee"], "false");
        assert!(!listed.to_string().contains("drv-secret"));

        // The editor sends the whole map back on every save; the placeholder
        // stands for the value on disk and must not overwrite it.
        let (status, body) = panel.put_json(
            "/api/config/devices/airspy0",
            None,
            json!({ "ppm": 2.5, "settings": settings }),
        );
        assert_eq!(status, 200, "{body}");
        let text = panel.config_text();
        assert!(text.contains("drv-secret"), "the secret was lost:\n{text}");
        assert!(
            !text.contains("***"),
            "the placeholder was written:\n{text}"
        );

        // A placeholder with nothing on disk behind it is refused.
        let before = panel.config_text();
        let (status, body) = panel.put_json(
            "/api/config/devices/hackrf-0",
            None,
            json!({ "settings": { "password": "***" } }),
        );
        assert_eq!(status, 400, "{body}");
        assert_eq!(panel.config_text(), before, "nothing was written");
    }

    #[test]
    fn an_empty_token_never_authenticates() {
        let lan: std::net::IpAddr = "192.168.1.20".parse().expect("an address");
        // `tokens = [""]` alone is no token at all: loopback mode, not a mode
        // in which an empty bearer opens everything.
        let blank = ["".to_owned()];
        assert_eq!(
            auth_check(true, Some(""), Some(lan), &blank),
            Err(Denial::LoopbackOnly)
        );
        assert_eq!(
            auth_check(true, Some("   "), Some(lan), &["  ".to_owned()]),
            Err(Denial::LoopbackOnly)
        );
        // Beside a real token, the blank one matches nothing.
        let mixed = ["".to_owned(), "s3cret".to_owned()];
        assert_eq!(
            auth_check(false, Some(""), Some(lan), &mixed),
            Err(Denial::NeedsToken)
        );
        assert_eq!(auth_check(true, Some("s3cret"), Some(lan), &mixed), Ok(()));

        // Over the wire, `Authorization: Bearer ` with nothing after it.
        let panel = Panel::start(mixed.to_vec());
        assert_eq!(panel.get("/api/status", Some("")).0, 401);
        assert_eq!(panel.get("/api/config", Some("")).0, 401);
        assert_eq!(panel.post("/api/tuners/hackrf-0/restart", Some("")).0, 401);
        assert_eq!(panel.get("/api/status", Some("s3cret")).0, 200);
    }

    #[test]
    fn a_mutation_from_a_foreign_origin_is_refused() {
        let panel = Panel::start(Vec::new());
        let generation = panel.desired.generation_of("hackrf-0");

        // Another page on this machine, a rebinding name, and a sandboxed
        // frame: none of them is the panel.
        let port = panel.base.rsplit(':').next().expect("a port").to_owned();
        for origin in [
            "http://evil.example".to_owned(),
            format!("http://evil.example:{port}"),
            "http://localhost:3000".to_owned(),
            format!("https://127.0.0.1:{port}"),
            "null".to_owned(),
        ] {
            let (status, body) =
                panel.post_from("/api/tuners/hackrf-0/restart", &origin, json!({}));
            assert_eq!(status, 403, "{origin}: {body}");
            let (status, _) =
                panel.post_from("/api/uplink", &origin, json!({ "spool_only": true }));
            assert_eq!(status, 403, "{origin}");
        }
        assert_eq!(panel.desired.generation_of("hackrf-0"), generation);
        assert!(!panel.recorder.hold().is_held(), "the hold was not touched");

        // The panel's own page, by address or by name, is fine.
        let own = panel.own_origin();
        assert_eq!(
            panel
                .post_from("/api/tuners/hackrf-0/restart", &own, json!({}))
                .0,
            200
        );
        let by_name = format!("http://localhost:{port}");
        assert_eq!(
            panel
                .post_from("/api/tuners/hackrf-0/restart", &by_name, json!({}))
                .0,
            200
        );
        // No Origin at all — curl, the Windows health monitor — is not a
        // browser being steered.
        assert_eq!(panel.post("/api/tuners/hackrf-0/restart", None).0, 200);
        // And a read is never origin-checked.
        assert_eq!(panel.get("/api/status", None).0, 200);
    }

    #[test]
    fn a_config_write_from_a_foreign_origin_changes_nothing() {
        let panel = Panel::with_config(Vec::new());
        let before = panel.config_text();
        let (status, _) = panel.put_from(
            "/api/config/devices/airspy0",
            "http://evil.example",
            json!({ "ppm": 9.0 }),
        );
        assert_eq!(status, 403);
        assert_eq!(panel.config_text(), before);
        let (status, _) = panel.post_from("/api/config/reload", "http://evil.example", json!({}));
        assert_eq!(status, 403);
        assert!(!panel.reload.load(Ordering::Relaxed));

        let own = panel.own_origin();
        assert_eq!(
            panel
                .put_from("/api/config/devices/airspy0", &own, json!({ "ppm": 9.0 }))
                .0,
            200
        );
    }

    #[test]
    fn the_origin_policy_follows_the_bind_and_the_public_url() {
        let server = |bind: &str, public: Option<&str>| crate::config::Server {
            bind: bind.into(),
            public_url: public.map(str::to_owned),
            ..crate::config::Server::default()
        };

        // A loopback bind: loopback names on the panel's port only.
        let p = OriginPolicy::new(&server("127.0.0.1:8095", None));
        assert!(p.allows("http://127.0.0.1:8095"));
        assert!(p.allows("http://LOCALHOST:8095"));
        assert!(p.allows("http://[::1]:8095"));
        assert!(!p.allows("http://127.0.0.1:8096"));
        assert!(!p.allows("http://192.168.1.5:8095"));
        assert!(!p.allows("http://attacker.example:8095"));
        assert!(!p.allows("null"));
        assert!(!p.allows("garbage"));

        // A LAN bind: its own address too.
        let p = OriginPolicy::new(&server("192.168.1.5:8095", None));
        assert!(p.allows("http://192.168.1.5:8095"));
        assert!(!p.allows("http://192.168.1.6:8095"));

        // A wildcard bind: any IP literal on the port, never a DNS name —
        // which is the one thing a rebinding page has.
        let p = OriginPolicy::new(&server("0.0.0.0:8095", None));
        assert!(p.allows("http://192.0.2.30:8095"));
        assert!(p.allows("http://[fe80::1]:8095"));
        assert!(!p.allows("http://recorder.lan:8095"));
        assert!(!p.allows("http://192.0.2.30:80"));

        // public_url: exactly its origin, scheme and default port included.
        let p = OriginPolicy::new(&server(
            "127.0.0.1:8095",
            Some("https://Recorder.example/panel/"),
        ));
        assert!(p.allows("https://recorder.example"));
        assert!(p.allows("https://recorder.example:443"));
        assert!(!p.allows("http://recorder.example"));
        assert!(!p.allows("https://recorder.example:8443"));
        let p = OriginPolicy::new(&server("0.0.0.0:8095", Some("http://recorder.lan:8095")));
        assert!(p.allows("http://recorder.lan:8095"));
    }

    #[test]
    fn an_unset_secret_stays_unset_rather_than_becoming_stars() {
        let json = redacted_config(&Config::default());
        assert_eq!(json["uplink"]["api_key"], "");
        assert_eq!(json["link"]["key"], "");
        assert_eq!(json["server"]["tokens"], json!([]));
    }

    #[test]
    fn a_restart_bumps_the_generation_and_changes_nothing_else() {
        let panel = Panel::start(Vec::new());
        let before = panel.desired.generation_of("hackrf-0");
        let applied_before = panel.desired.applied("hackrf-0").expect("a known tuner");

        let (status, body) = panel.post("/api/tuners/hackrf-0/restart", None);
        assert_eq!(status, 200);
        assert_eq!(body["restarting"], true);

        assert_eq!(
            panel.desired.generation_of("hackrf-0"),
            before + 1,
            "the restart is a generation bump"
        );
        let applied_after = panel.desired.applied("hackrf-0").expect("a known tuner");
        assert_eq!(
            applied_after.version, applied_before.version,
            "a restart is not a config change: the applied version must not move"
        );
        assert_eq!(applied_after.config, applied_before.config);
        assert!(
            !panel.desired.is_pending("hackrf-0"),
            "a restart must not raise keyup's pending badge"
        );
    }

    #[test]
    fn an_unknown_tuner_is_a_404() {
        let panel = Panel::start(Vec::new());
        let (status, body) = panel.post("/api/tuners/nope/restart", None);
        assert_eq!(status, 404);
        assert!(body["error"].as_str().expect("an error").contains("nope"));
    }

    #[test]
    fn unknown_routes_and_methods_are_distinguished() {
        let panel = Panel::start(Vec::new());
        assert_eq!(panel.get("/api/nothing", None).0, 404);
        assert_eq!(panel.get("/nothing", None).0, 404);
        assert_eq!(
            panel.post("/api/status", None).0,
            405,
            "status is a GET route"
        );
        assert_eq!(
            panel.get("/api/tuners/hackrf-0/restart", None).0,
            405,
            "restart is a POST route"
        );
    }

    #[test]
    fn a_token_is_required_on_every_api_route_once_one_is_configured() {
        let panel = Panel::start(vec!["s3cret".into(), "other".into()]);
        assert_eq!(panel.get("/api/status", None).0, 401, "no bearer");
        assert_eq!(
            panel.get("/api/status", Some("wrong")).0,
            401,
            "wrong token"
        );
        assert_eq!(panel.get("/api/status", Some("s3cret")).0, 200);
        assert_eq!(panel.get("/api/status", Some("other")).0, 200, "any token");
        assert_eq!(
            panel.post("/api/tuners/hackrf-0/restart", None).0,
            401,
            "a loopback peer is not a substitute for a configured token"
        );
        assert_eq!(
            panel.post("/api/tuners/hackrf-0/restart", Some("s3cret")).0,
            200
        );
    }

    #[test]
    fn without_tokens_reads_are_open_and_loopback_may_mutate() {
        let panel = Panel::start(Vec::new());
        assert_eq!(panel.get("/api/status", None).0, 200);
        assert_eq!(panel.get("/api/config", None).0, 200);
        assert_eq!(panel.post("/api/tuners/hackrf-0/restart", None).0, 200);
    }

    #[test]
    fn a_non_loopback_peer_cannot_mutate_without_a_token() {
        // A test cannot dial in from the LAN, so exercise the rule itself with
        // the peer address a LAN client would arrive with.
        let lan: std::net::IpAddr = "192.168.1.20".parse().expect("an address");
        let local: std::net::IpAddr = "127.0.0.1".parse().expect("an address");
        let none: &[String] = &[];

        assert_eq!(
            auth_check(false, None, Some(lan), none),
            Ok(()),
            "reads open"
        );
        assert_eq!(
            auth_check(true, None, Some(lan), none),
            Err(Denial::LoopbackOnly),
            "a LAN restart with no token configured is refused"
        );
        assert_eq!(auth_check(true, None, Some(local), none), Ok(()));
        assert_eq!(
            auth_check(true, None, None, none),
            Err(Denial::LoopbackOnly),
            "an unknown peer is not loopback"
        );

        // With tokens set, the peer address stops mattering in either direction.
        let tokens = ["s3cret".to_owned()];
        assert_eq!(
            auth_check(false, None, Some(local), &tokens),
            Err(Denial::NeedsToken)
        );
        assert_eq!(auth_check(true, Some("s3cret"), Some(lan), &tokens), Ok(()));
    }

    #[test]
    fn the_status_body_reports_the_uplink_hold() {
        let panel = Panel::start(Vec::new());
        let (status, body) = panel.get("/api/status", None);
        assert_eq!(status, 200);
        let u = &body["uplink"];
        assert_eq!(u["spool_only"], false);
        assert_eq!(u["source"], "config");
        assert_eq!(u["spooled_calls"], 0);
        assert_eq!(u["spool_bytes"], 0);
        assert_eq!(u["url"], "http://keyup.example/api/call-upload");
        // Spooled-on-pressure and quarantine are always present, so "none"
        // reads as 0 rather than as a recorder too old to say.
        assert_eq!(u["spooled_on_pressure"], 0);
        assert_eq!(u["quarantined"], 0);
        assert_eq!(u["circuit_open"], false);
        assert!(
            body.get("recorder_applied").is_none(),
            "nothing echoed until keyup pushes a recorder block"
        );
        // The push channel rides inside the same object, always present so a
        // GUI can tell "off" from "this recorder is too old to say".
        assert_eq!(
            u["stream"],
            json!({
                "enabled": false,
                "connected": false,
                "since": Value::Null,
                "reconnects": 0,
                "last_notify_at": Value::Null,
                "last_error": Value::Null,
            }),
            "uplink.stream is reported verbatim"
        );
    }

    #[test]
    fn posting_the_uplink_route_toggles_the_hold_both_ways() {
        let panel = Panel::start(Vec::new());
        let (status, body) = panel.post_json("/api/uplink", None, json!({ "spool_only": true }));
        assert_eq!(status, 200);
        assert_eq!(body["spool_only"], true);
        assert_eq!(body["source"], "panel", "the panel names itself");
        assert!(panel.recorder.hold().is_held(), "the shared flag moved");
        // And the status body agrees, because both read the same builder.
        assert_eq!(
            panel.get("/api/status", None).1["uplink"]["spool_only"],
            true
        );

        let (status, body) = panel.post_json("/api/uplink", None, json!({ "spool_only": false }));
        assert_eq!(status, 200);
        assert_eq!(body["spool_only"], false);
        assert!(!panel.recorder.hold().is_held());
    }

    #[test]
    fn the_uplink_route_refuses_a_body_it_cannot_read() {
        let panel = Panel::start(Vec::new());
        assert_eq!(
            panel
                .post_json("/api/uplink", None, json!({ "hold": true }))
                .0,
            400,
            "the field has to be spool_only"
        );
        assert_eq!(
            panel
                .post_json("/api/uplink", None, json!({ "spool_only": "yes" }))
                .0,
            400,
            "and it has to be a boolean"
        );
        assert!(
            !panel.recorder.hold().is_held(),
            "a refused request changes nothing"
        );
        // A GET reads it without changing it.
        assert_eq!(panel.get("/api/uplink", None).0, 200);
    }

    #[test]
    fn holding_the_uplink_is_a_mutation_and_answers_to_the_same_auth_rule() {
        // Over the wire: with tokens set, the route needs one like every other.
        let panel = Panel::start(vec!["s3cret".into()]);
        assert_eq!(
            panel
                .post_json("/api/uplink", None, json!({ "spool_only": true }))
                .0,
            401
        );
        assert!(!panel.recorder.hold().is_held());
        assert_eq!(
            panel
                .post_json("/api/uplink", Some("s3cret"), json!({ "spool_only": true }))
                .0,
            200
        );

        // And with no tokens, it is loopback-only — the rule a test cannot dial
        // in to exercise, so the peer address is fed to it directly.
        let lan: std::net::IpAddr = "192.168.1.20".parse().expect("an address");
        let none: &[String] = &[];
        assert_eq!(
            auth_check(true, None, Some(lan), none),
            Err(Denial::LoopbackOnly),
            "a LAN hold with no token configured is refused"
        );
    }

    #[test]
    fn a_percent_encoded_tuner_key_is_decoded() {
        assert_eq!(
            restart_target("/api/tuners/hack%20rf/restart").as_deref(),
            Some("hack rf")
        );
        assert_eq!(restart_target("/api/tuners//restart"), None);
        assert_eq!(restart_target("/api/tuners/a/b/restart"), None);
        assert_eq!(restart_target("/api/status"), None);
    }

    // -----------------------------------------------------------------------
    // Device setup
    // -----------------------------------------------------------------------

    #[test]
    fn discover_reports_the_attached_boards_and_which_device_each_one_is() {
        let panel = Panel::with_config(Vec::new());
        let (status, body) = panel.get("/api/devices/discover", None);
        assert_eq!(status, 200);
        assert!(body["writable"].as_bool().expect("writable"));
        assert_eq!(body["restart_pending"], false);
        // One spelling of the path, forward slashes, in both bodies that carry
        // it: a browser reading two different strings for one file is a bug
        // waiting to be reported as "the panel cannot find my config".
        let path = body["config_path"].as_str().expect("a path");
        assert!(path.ends_with("trunkd.toml"), "{path}");
        assert!(
            !path.contains('\\'),
            "a native path reached the wire: {path}"
        );
        assert_eq!(panel.get("/api/status", None).1["config_path"], path);

        let hw = body["hardware"].as_array().expect("a hardware array");
        assert_eq!(hw.len(), 2);

        // The Airspy: keyed on the canonical args, not the driver's `label=`,
        // and matched to the configured device by serial despite the case
        // difference between the file and what Soapy reports.
        let airspy = &hw[0];
        assert_eq!(airspy["key"], "soapy:driver=airspy,serial=00000000deadbeef");
        assert_eq!(airspy["backend"], "soapy");
        assert_eq!(airspy["driver"], "airspy");
        assert_eq!(airspy["serial"], "00000000deadbeef");
        assert_eq!(airspy["label"], "AirSpy One [00000000deadbeef]");
        assert_eq!(airspy["configured_id"], "airspy0");
        assert_eq!(airspy["running"], false, "no supervised units in a test");
        assert_eq!(airspy["suggested"]["sample_rate"], 10_000_000);
        assert_eq!(airspy["suggested"]["gains"]["LNA"], 12.0);

        // The HackRF has no serial in the file, so it matches on being the one
        // board of its driver.
        let hackrf = &hw[1];
        assert_eq!(hackrf["backend"], "hackrf");
        assert_eq!(hackrf["configured_id"], "hackrf-0");
        assert_eq!(hackrf["suggested"]["backend"], "hackrf");

        let configured = body["configured"].as_array().expect("configured");
        assert_eq!(configured.len(), 2);
        assert_eq!(configured[0]["id"], "airspy0");
        assert_eq!(configured[0]["present"], true);
        assert_eq!(configured[0]["sites"], json!([]), "not in any site yet");
        assert_eq!(configured[1]["id"], "hackrf-0");
        assert_eq!(configured[1]["sites"], json!(["Example Network/Central"]));
    }

    #[test]
    fn the_device_list_is_the_file_on_disk_with_its_toml_field_names() {
        let panel = Panel::with_config(Vec::new());
        let (status, body) = panel.get("/api/config/devices", None);
        assert_eq!(status, 200);
        let devices = body["devices"].as_array().expect("devices");
        assert_eq!(devices.len(), 2);
        // Serde field names, so a PUT can hand the same keys straight back.
        assert_eq!(devices[0]["id"], "airspy0");
        assert_eq!(devices[0]["backend"], "soapy");
        assert_eq!(devices[0]["sample_rate"], 10_000_000);
        assert_eq!(devices[0]["center_frequency"], 851_500_000);
        assert_eq!(devices[0]["sites"], json!([]));
        assert_eq!(devices[1]["sites"], json!(["Example Network/Central"]));

        let sites = body["sites"].as_array().expect("sites");
        assert_eq!(sites.len(), 1);
        assert_eq!(sites[0]["system"], "Example Network");
        assert_eq!(sites[0]["site"], "Central");
        assert_eq!(sites[0]["enabled"], true);
        assert_eq!(sites[0]["devices"], json!(["hackrf-0"]));
        assert_eq!(sites[0]["control_frequencies"], json!([851_050_000]));
        assert_eq!(sites[0]["traffic_pool_size"], 20);
    }

    #[test]
    fn a_put_writes_the_file_keeps_the_secret_and_raises_restart_pending() {
        let panel = Panel::with_config(Vec::new());
        let (status, body) = panel.put_json(
            "/api/config/devices/airspy0",
            None,
            json!({ "ppm": 1.5, "sites": ["Example Network/Central"] }),
        );
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["ok"], true);
        assert_eq!(body["restart_required"], true);
        assert_eq!(body["id"], "airspy0");

        let text = panel.config_text();
        assert!(text.contains("ppm = 1.5"), "{text}");
        assert!(
            text.contains(r#"devices = ["hackrf-0", "airspy0"]"#),
            "{text}"
        );
        assert!(
            text.contains("api_key = 'kup_live_secret'"),
            "the secret was rewritten:\n{text}"
        );
        assert!(text.contains("# hand written"), "the comment went:\n{text}");
        // Untouched fields survive a partial edit.
        assert!(text.contains(r#"serial = "00000000DEADBEEF""#));

        assert!(
            panel.restart_pending.load(Ordering::Relaxed),
            "a write must raise the banner"
        );
        assert_eq!(panel.get("/api/status", None).1["restart_pending"], true);
        assert_eq!(
            panel.get("/api/devices/discover", None).1["restart_pending"],
            true
        );
        // And the read route sees the new file, not the running config.
        let listed = panel.get("/api/config/devices", None).1;
        assert_eq!(listed["devices"][0]["ppm"], 1.5);
        assert_eq!(
            listed["devices"][0]["sites"],
            json!(["Example Network/Central"])
        );
    }

    #[test]
    fn a_put_can_add_a_device_that_was_not_in_the_file() {
        let panel = Panel::with_config(Vec::new());
        let (status, body) = panel.put_json(
            "/api/config/devices/rsp1",
            None,
            json!({
                "backend": "soapy",
                "driver": "sdrplay",
                "sample_rate": 8_000_000,
                "bandwidth_hz": 8_000_000,
                "center_frequency": 851_500_000,
                "gains": { "IFGR": 40, "RFGR": 3 },
                "settings": { "rfnotch_ctrl": "false" },
            }),
        );
        assert_eq!(status, 200, "{body}");
        let text = panel.config_text();
        assert!(text.contains("[device.gains]"), "{text}");
        assert!(text.contains("[device.settings]"), "{text}");
        let listed = panel.get("/api/config/devices", None).1;
        assert_eq!(listed["devices"].as_array().expect("devices").len(), 3);
        assert_eq!(listed["devices"][2]["id"], "rsp1");
    }

    #[test]
    fn a_put_refuses_a_field_that_is_not_a_device_field_and_writes_nothing() {
        let panel = Panel::with_config(Vec::new());
        let before = panel.config_text();
        let (status, body) = panel.put_json(
            "/api/config/devices/airspy0",
            None,
            json!({ "sample_rte": 10_000_000 }),
        );
        assert_eq!(status, 400);
        assert!(body["error"]
            .as_str()
            .expect("an error")
            .contains("sample_rte"));
        assert_eq!(panel.config_text(), before, "a refusal changed the file");
        assert!(!panel.restart_pending.load(Ordering::Relaxed));

        // Malformed JSON is the same answer.
        assert_eq!(
            panel
                .put_json("/api/config/devices/airspy0", None, json!([1, 2]))
                .0,
            400
        );
    }

    #[test]
    fn a_put_that_would_not_start_is_refused_with_the_startup_message() {
        let panel = Panel::with_config(Vec::new());
        let before = panel.config_text();
        // Move the site's tuner where it cannot hear the control channel.
        let (status, body) = panel.put_json(
            "/api/config/devices/hackrf-0",
            None,
            json!({ "center_frequency": 155_000_000 }),
        );
        assert_eq!(status, 422, "{body}");
        let err = body["error"].as_str().expect("an error");
        assert!(
            err.contains("851050000"),
            "not the validate() message: {err}"
        );
        assert_eq!(panel.config_text(), before);
        assert!(!panel.restart_pending.load(Ordering::Relaxed));
    }

    #[test]
    fn a_put_naming_a_site_the_file_does_not_have_is_a_404() {
        let panel = Panel::with_config(Vec::new());
        let before = panel.config_text();
        let (status, body) = panel.put_json(
            "/api/config/devices/airspy0",
            None,
            json!({ "sites": ["Nowhere/Nothing"] }),
        );
        assert_eq!(status, 404);
        assert!(body["error"]
            .as_str()
            .expect("an error")
            .contains("Nowhere/Nothing"));
        assert_eq!(panel.config_text(), before);
    }

    #[test]
    fn a_delete_is_refused_while_a_site_still_uses_the_device() {
        let panel = Panel::with_config(Vec::new());
        let before = panel.config_text();
        let (status, body) = panel.delete("/api/config/devices/hackrf-0", None);
        assert_eq!(status, 409);
        assert_eq!(
            body["referenced_by"],
            json!(["site 'Example Network/Central'"])
        );
        assert_eq!(panel.config_text(), before);
        assert!(!panel.restart_pending.load(Ordering::Relaxed));
    }

    #[test]
    fn a_delete_of_an_unreferenced_device_writes_the_file_and_keeps_a_backup() {
        let panel = Panel::with_config(Vec::new());
        let (status, body) = panel.delete("/api/config/devices/airspy0", None);
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["restart_required"], true);
        let text = panel.config_text();
        assert!(!text.contains("airspy0"), "{text}");
        assert!(text.contains("api_key = 'kup_live_secret'"));
        assert!(panel.restart_pending.load(Ordering::Relaxed));

        let bak = panel
            .dir
            .as_ref()
            .expect("a config dir")
            .join("trunkd.toml.bak");
        assert_eq!(
            std::fs::read_to_string(bak).expect("the backup"),
            CONFIG_FILE,
            "the previous version has to be recoverable"
        );

        assert_eq!(
            panel.delete("/api/config/devices/airspy0", None).0,
            404,
            "and it is gone"
        );
    }

    #[test]
    fn reload_validates_the_file_before_it_takes_the_recorder_down() {
        let panel = Panel::with_config(Vec::new());
        let path = panel
            .dir
            .as_ref()
            .expect("a config dir")
            .join("trunkd.toml");

        // A file that will not parse: refused, and the flag stays down.
        std::fs::write(&path, "[[device]\nbroken").expect("write");
        let (status, body) = panel.post("/api/config/reload", None);
        assert_eq!(status, 422);
        assert!(!body["error"].as_str().expect("an error").is_empty());
        assert!(
            !panel.reload.load(Ordering::Relaxed),
            "a bad file must never take the recorder down"
        );

        // A file that parses but would not start: same answer.
        std::fs::write(
            &path,
            CONFIG_FILE.replace(
                "center_frequency = 851500000",
                "center_frequency = 155000000",
            ),
        )
        .expect("write");
        assert_eq!(panel.post("/api/config/reload", None).0, 422);
        assert!(!panel.reload.load(Ordering::Relaxed));

        // And a good one goes through.
        std::fs::write(&path, CONFIG_FILE).expect("write");
        let (status, body) = panel.post("/api/config/reload", None);
        assert_eq!(status, 200);
        assert_eq!(body["ok"], true);
        assert!(panel.reload.load(Ordering::Relaxed));
    }

    #[test]
    fn without_a_config_file_the_write_routes_say_so_and_the_reads_still_work() {
        // The env-only recorder: there is nothing on disk to edit.
        let panel = Panel::start(Vec::new());
        for (status, body) in [
            panel.put_json("/api/config/devices/x", None, json!({ "ppm": 1.0 })),
            panel.delete("/api/config/devices/x", None),
            panel.post("/api/config/reload", None),
        ] {
            assert_eq!(status, 503, "{body}");
            assert!(body["error"]
                .as_str()
                .expect("an error")
                .contains("environment"));
        }

        // Reads fall back to the running config rather than failing.
        let (status, body) = panel.get("/api/devices/discover", None);
        assert_eq!(status, 200);
        assert_eq!(body["config_path"], Value::Null);
        assert_eq!(body["writable"], false);
        assert_eq!(body["configured"][0]["id"], "hackrf-0");
        assert_eq!(panel.get("/api/config/devices", None).0, 200);
        assert_eq!(panel.get("/api/status", None).1["config_path"], Value::Null);
    }

    #[test]
    fn the_device_write_routes_answer_to_the_same_auth_rule() {
        let panel = Panel::with_config(vec!["s3cret".into()]);
        assert_eq!(panel.get("/api/devices/discover", None).0, 401);
        assert_eq!(panel.get("/api/devices/discover", Some("s3cret")).0, 200);
        assert_eq!(
            panel
                .put_json("/api/config/devices/airspy0", None, json!({ "ppm": 1.0 }))
                .0,
            401,
            "a loopback peer is not a substitute for a configured token"
        );
        assert_eq!(panel.delete("/api/config/devices/airspy0", None).0, 401);
        assert_eq!(panel.post("/api/config/reload", None).0, 401);
        assert_eq!(
            panel
                .put_json(
                    "/api/config/devices/airspy0",
                    Some("s3cret"),
                    json!({ "ppm": 1.0 })
                )
                .0,
            200
        );
        // Nothing leaked on the way through.
        assert!(!panel.config_text().contains("***"));
    }

    #[test]
    fn the_device_routes_take_the_methods_they_document_and_no_others() {
        let panel = Panel::with_config(Vec::new());
        assert_eq!(panel.post("/api/devices/discover", None).0, 405);
        assert_eq!(panel.post("/api/config/devices", None).0, 405);
        assert_eq!(panel.get("/api/config/reload", None).0, 405);
        assert_eq!(panel.get("/api/config/devices/airspy0", None).0, 405);
        assert_eq!(panel.get("/api/config/devices/", None).0, 404);
    }

    #[test]
    fn a_device_id_is_taken_from_the_path_and_percent_decoded() {
        assert_eq!(
            device_target("/api/config/devices/air%20spy").as_deref(),
            Some("air spy")
        );
        assert_eq!(device_target("/api/config/devices/"), None);
        assert_eq!(device_target("/api/config/devices/a/b"), None);
        assert_eq!(device_target("/api/config/devices"), None);
    }

    #[test]
    fn hardware_matching_refuses_to_guess_between_two_identical_boards() {
        use crate::config::Backend;

        let board = |serial: &str| trunkd_sdr::DeviceDescriptor {
            id: trunkd_core::DeviceId::new(format!("soapy:driver=airspy,serial={serial}")),
            driver: "soapy:airspy".into(),
            label: format!("AirSpy [{serial}]"),
            args: format!("driver=airspy,serial={serial}"),
        };
        let hw: Vec<_> = ["aaaa", "bbbb"]
            .map(|s| devices::describe(&board(s)))
            .to_vec();

        // Two configured Airspys with no serials: neither can be identified,
        // and inventing an order would put the wrong board on the wrong mast.
        let mut a = dev("a");
        a.backend = Backend::Soapy;
        a.driver = Some("airspy".into());
        a.serial = None;
        let mut b = a.clone();
        b.id = "b".into();
        assert_eq!(
            devices::match_configured(&[a.clone(), b], &hw),
            vec![None, None]
        );

        // One of them: unambiguous, so the driver rule fires.
        assert_eq!(
            devices::match_configured(&[a.clone()], &hw[..1]),
            vec![Some(0)]
        );

        // A serial pins it regardless of order or case.
        let mut with_serial = a.clone();
        with_serial.serial = Some("BBBB".into());
        assert_eq!(
            devices::match_configured(&[with_serial], &hw),
            vec![Some(1)]
        );

        // A serial that is not attached is absent, not "probably that one".
        let mut missing = a;
        missing.serial = Some("cccc".into());
        assert_eq!(devices::match_configured(&[missing], &hw), vec![None]);
    }

    #[test]
    fn a_token_matches_only_by_exact_equality_over_the_configured_list() {
        assert!(token_accepted("a", &["b".into(), "a".into()]));
        assert!(!token_accepted("", &["b".into()]));
        assert!(!token_accepted("a", &[]));
    }
}
