// SPDX-License-Identifier: GPL-3.0-or-later
//! The keyup control loop: status up, desired-state down, apply by restart.
//!
//! This is the recorder half of keyup's tuner-management contract (W2). It runs
//! on its own thread, a sibling of the observations thread, and does two things
//! on independent cadences:
//!
//! * **Status up** (~15 s): gather each tuner's reported fields from W1's
//!   per-tuner metrics and config and POST them to
//!   `{keyup}/api/recorder/tuners/status`. A status snapshot is not a queue —
//!   staleness is fine — so a failed POST is logged and simply retried next
//!   cycle; nothing is spooled.
//! * **Desired-state down** (~10 s): GET
//!   `{keyup}/api/recorder/desired-state?instance=<id>`, diff each tuner's
//!   desired `config` against what is applied, and when a tuner's `version` is
//!   newer *and* its config differs, apply it by asking the supervisor to
//!   restart that one `DeviceUnit` with the new parameters — never rebuilding
//!   the whole process.
//!
//! # Layering
//!
//! TOML is hardware and boot defaults; keyup's desired-state is a runtime
//! override applied on top and never written back to the TOML. The
//! [`DesiredState`] registry holds, per tuner, the override currently in effect
//! (what the running `DeviceUnit` was built with) and the override to build the
//! next one with. The device factory in `recorder.rs` reads the registry on
//! every (re)build, so a restart is what makes an override take effect, and the
//! *applied version* only advances once that restart has happened — which is
//! exactly what clears keyup's "pending" badge.
//!
//! # Apply by restart
//!
//! The supervisor rebuilds a `DeviceUnit` from its factory on every restart
//! (its recovery primitive). To apply a config change we bump a per-tuner
//! generation counter; the `DeviceUnit` notices its generation moved, calls
//! [`SupervisionContext::request_restart`](trunkd_supervisor::SupervisionContext::request_restart)
//! (a clean rebuild, not a fault), and returns; the supervisor's next
//! `factory()` call reads the new override out of the registry. `enabled = false`
//! builds the unit in an idle state so the tuner stops recording, and the same
//! rebuild drops it out of the cross-tuner routing table, so grants go to a
//! sibling that can record them rather than to a unit that is idling.
//!
//! Nothing here can crash the recorder: keyup being unreachable, or returning a
//! malformed or oversized response, leaves every tuner on its last-applied state
//! (or the TOML defaults) with a log line and no more.
//!
//! # Bands, and why a bad push cannot mute a recorder
//!
//! `bands` restricts which downlink ranges a tuner records — an antenna and a
//! bandpass may only cover some of the capture, possibly in disjoint pieces. It
//! is the one overridable field that can, in principle, be pushed to a value
//! that records nothing, so it gets four nets, in order:
//!
//! 1. **Parse.** Caps ([`MAX_BANDS`], [`MAX_BAND_LEN`]) and every entry must
//!    resolve via [`crate::config::resolve_band`]. Anything else drops the whole
//!    tuner entry with one warn — never a partly applied list.
//! 2. **Apply.** A merged config whose bands do not overlap the tuner's usable
//!    capture at all is refused ([`ApplyDecision::NoOp`]) with a warn: nothing is
//!    staged, no restart happens, and the applied version does not move, so
//!    keyup keeps showing the change as pending rather than as done.
//! 3. **Two-phase confirm.** Anything that gets past both still only takes
//!    effect through a rebuild, and a unit whose stream never opens never calls
//!    [`DesiredState::confirm_applied`] — same net that catches a bad center.
//! 4. **Control is not band-gated.** Control acquisition is bounded by the
//!    capture span alone; the engine consults `bands` only when deciding which
//!    *traffic grants* it records. So no bands push can stop a recorder from
//!    following control, reporting status, or hearing this loop again.
//!
//! Grants that end up outside every tuner's ranges are counted
//! (`coordinator.unrouted_grants.*`), which is how an over-narrow (but legal)
//! partition shows up on a dashboard rather than as silence.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use trunkd_core::chan::BoundedReceiver;
use trunkd_sdr::GainRequest;
use trunkd_uplink::{HoldSource, UplinkHold};

use crate::config::{truncate_chars, Backend, Config, Device, DeviceRole, Site, Station};
use crate::pipeline::eventarchive::{ArchiveSource, EventLogReport};
use crate::pipeline::failrun::FailureRun;
use crate::pipeline::link::LinkSlot;
use crate::pipeline::spectrum::{spectrum_body, SpectrumFrame};
use crate::pipeline::Shared;

/// Timeout for one control-loop HTTP call, connect through response.
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);

/// Timeout for one spectrum POST. Spectrum is a live display posted every
/// cycle while it is on, and a lost frame costs nothing, so a slow Keyup must
/// not hold the loop — and the desired-state GET a push is waiting on — for
/// the full [`HTTP_TIMEOUT`] per frame.
const SPECTRUM_TIMEOUT: Duration = Duration::from_secs(2);

/// keyup caps a report at 64 tuners; we never POST more, and never parse more
/// desired tuners than this either.
pub(crate) const MAX_TUNERS: usize = 64;

/// keyup caps a `tuner_key` at 64 characters.
pub(crate) const MAX_TUNER_KEY_LEN: usize = 64;

/// Refuse to parse a desired-state body larger than this; a legitimate one for
/// 64 tuners is a few kilobytes.
const MAX_RESPONSE_BYTES: usize = 512 * 1024;

// ---------------------------------------------------------------------------
// URL derivation (mirrors observations_url / events_url)
// ---------------------------------------------------------------------------

/// Derive the desired-state endpoint from the configured call-upload URL.
pub(crate) fn desired_state_url(call_upload_url: &str) -> String {
    swap_route(call_upload_url, "/api/recorder/desired-state")
}

/// Derive the tuner-status endpoint from the configured call-upload URL.
pub(crate) fn tuner_status_url(call_upload_url: &str) -> String {
    swap_route(call_upload_url, "/api/recorder/tuners/status")
}

/// Derive the spectrum endpoint from the configured call-upload URL (W4).
pub(crate) fn spectrum_url(call_upload_url: &str) -> String {
    swap_route(call_upload_url, "/api/recorder/spectrum")
}

/// Derive the push-channel endpoint from the configured call-upload URL (W6).
///
/// The same route swap as its siblings, plus the scheme change a WebSocket
/// needs: `http` → `ws`, `https` → `wss`. An operator therefore configures one
/// URL and gets the push channel on the same host, with TLS exactly where the
/// uploads already have it.
pub(crate) fn stream_url(call_upload_url: &str) -> String {
    let http = swap_route(call_upload_url, "/api/recorder/stream");
    match http.split_once("://") {
        Some(("https", rest)) => format!("wss://{rest}"),
        Some(("http", rest)) => format!("ws://{rest}"),
        // Already a ws/wss URL, or something we do not recognise: leave it
        // alone rather than mangling an operator's override.
        _ => http,
    }
}

/// Swap the upload path for `route`, or append it to a hand-written override.
fn swap_route(call_upload_url: &str, route: &str) -> String {
    const UPLOAD: &str = "/api/call-upload";
    let trimmed = call_upload_url.trim_end_matches('/');
    match trimmed.rfind(UPLOAD) {
        Some(at) => format!("{}{route}{}", &trimmed[..at], &trimmed[at + UPLOAD.len()..]),
        None => format!("{trimmed}{route}"),
    }
}

// ---------------------------------------------------------------------------
// Role <-> wire string
// ---------------------------------------------------------------------------

/// The wire name for a [`DeviceRole`] (matches the TOML `role` values).
pub(crate) fn role_str(role: DeviceRole) -> &'static str {
    match role {
        DeviceRole::Auto => "auto",
        DeviceRole::Control => "control",
        DeviceRole::Voice => "voice",
        DeviceRole::Data => "data",
    }
}

/// Parse a wire role name; unknown names yield `None` (ignored, not applied).
pub(crate) fn role_from_str(s: &str) -> Option<DeviceRole> {
    match s.trim().to_ascii_lowercase().as_str() {
        "auto" => Some(DeviceRole::Auto),
        "control" => Some(DeviceRole::Control),
        "voice" => Some(DeviceRole::Voice),
        "data" => Some(DeviceRole::Data),
        _ => None,
    }
}

/// The wire name for a [`Backend`].
pub(crate) fn backend_str(backend: Backend) -> &'static str {
    match backend {
        Backend::Soapy => "soapy",
        Backend::Iqfile => "iqfile",
        Backend::Hackrf => "hackrf",
    }
}

// ---------------------------------------------------------------------------
// Applied / desired config and the decision logic
// ---------------------------------------------------------------------------

/// The runtime-overridable parameters of one tuner: the fields keyup's
/// desired-state can change and that a `DeviceUnit` restart applies.
///
/// `spectrum` is deliberately not here: it is a live flag (W4) that toggles the
/// DSP thread's spectrum tap in place via an atomic, so a spectrum-only change
/// must never force a restart.
#[derive(Clone, Debug, PartialEq, Default)]
pub(crate) struct AppliedConfig {
    /// Engine role; `None` keeps the TOML role.
    pub role: Option<DeviceRole>,
    /// Advisory site label focus; `None` keeps the TOML value.
    pub site: Option<String>,
    /// keyup's numeric site identifier, echoed back distinctly so keyup can
    /// match on it; `None` when keyup did not send one.
    pub site_id: Option<String>,
    /// Tune frequency in Hz; `None` keeps the TOML center.
    pub center_frequency: Option<u64>,
    /// Downlink band labels this tuner records for (`vhf`/`uhf`/`700`/`800`/
    /// `900`, or an explicit `"lo-hi"` Hz range), possibly several disjoint
    /// ones. `None` keeps the TOML list; `Some([])` is "no restriction", which
    /// is a real instruction and distinct from absent.
    pub bands: Option<Vec<String>>,
    /// Whether the tuner records at all.
    pub enabled: bool,
    /// Backend-generic overall gain in dB (Soapy).
    pub gain: Option<f64>,
    /// Per-element gains by the driver's own element names (Soapy).
    pub gains: Option<BTreeMap<String, f64>>,
    /// Hardware AGC (Soapy).
    pub agc: Option<bool>,
    /// Static frequency correction in ppm.
    pub ppm: Option<f64>,
    /// Whether the closed-loop frequency correction runs.
    pub auto_ppm: Option<bool>,
    /// Ceiling on the total automatic correction, in ppm.
    pub auto_ppm_limit: Option<f64>,
    /// Analog filter bandwidth in Hz (Soapy).
    pub bandwidth_hz: Option<u32>,
    /// RX antenna/port name (Soapy).
    pub rx_port: Option<String>,
    /// Driver settings written with `writeSetting` (Soapy).
    pub settings: Option<BTreeMap<String, String>>,
    /// HackRF LNA (IF) gain in dB.
    pub lna_db: Option<u32>,
    /// HackRF VGA (baseband) gain in dB.
    pub vga_db: Option<u32>,
    /// HackRF front-end +14 dB amp.
    pub amp: Option<bool>,
    /// Whether SNDCP data-channel grants are followed. Applies to *every* site
    /// this tuner serves, because it is a property of what the tuner does with
    /// its channelizer rather than of one site's channel plan.
    pub data_follow: Option<bool>,
    /// Whether this tuner accepts grants a *peer* recorder heard over the
    /// `[link]` (`[[device]] remote_grants`). `None` keeps the role default.
    pub remote_grants: Option<bool>,
    /// The control candidate list of the tuner's primary site (the first
    /// `[[site]]` it serves), in Hz.
    pub control_frequencies: Option<Vec<u64>>,
    /// Complex sample rate in samples/sec.
    pub sample_rate: Option<u32>,
}

impl AppliedConfig {
    /// The effective sample rate, falling back to `toml` for a config that has
    /// never carried one (an entry built for an unknown tuner).
    pub(crate) fn rate_or(&self, toml: u32) -> u32 {
        self.sample_rate.unwrap_or(toml)
    }
}

/// One tuner's desired config as parsed from keyup, with its own version.
#[derive(Clone, Debug, PartialEq, Default)]
pub(crate) struct DesiredTuner {
    /// Per-tuner version; echoed back as `applied_version` once in effect.
    pub version: i64,
    /// Desired role, when a recognised one was given.
    pub role: Option<DeviceRole>,
    /// Desired site label focus.
    pub site: Option<String>,
    /// Desired numeric site id (kept distinct from the label).
    pub site_id: Option<String>,
    /// Desired tune frequency in Hz.
    pub center_frequency: Option<u64>,
    /// Desired band restriction; `None` when keyup did not send one (inherit),
    /// `Some([])` to lift the restriction.
    pub bands: Option<Vec<String>>,
    /// Desired enabled state (defaults to true when absent).
    pub enabled: bool,
    /// Desired spectrum/FFT flag (reported, and toggles the live tap; W4).
    pub spectrum: bool,
    /// Desired overall gain in dB (Soapy).
    pub gain: Option<f64>,
    /// Desired per-element gains (Soapy).
    pub gains: Option<BTreeMap<String, f64>>,
    /// Desired hardware AGC (Soapy).
    pub agc: Option<bool>,
    /// Desired static frequency correction in ppm.
    pub ppm: Option<f64>,
    /// Desired closed-loop correction on/off.
    pub auto_ppm: Option<bool>,
    /// Desired ceiling on the automatic correction, in ppm.
    pub auto_ppm_limit: Option<f64>,
    /// Desired analog filter bandwidth in Hz (Soapy).
    pub bandwidth_hz: Option<u32>,
    /// Desired RX antenna/port name (Soapy).
    pub rx_port: Option<String>,
    /// Desired driver settings (Soapy).
    pub settings: Option<BTreeMap<String, String>>,
    /// Desired HackRF LNA gain in dB.
    pub lna_db: Option<u32>,
    /// Desired HackRF VGA gain in dB.
    pub vga_db: Option<u32>,
    /// Desired HackRF front-end amp.
    pub amp: Option<bool>,
    /// Desired data-channel follow, for every site this tuner serves.
    pub data_follow: Option<bool>,
    /// Desired acceptance of grants heard by a peer recorder.
    pub remote_grants: Option<bool>,
    /// Desired control candidate list for the tuner's primary site.
    pub control_frequencies: Option<Vec<u64>>,
    /// Desired complex sample rate in samples/sec.
    pub sample_rate: Option<u32>,
}

/// The outcome of comparing a desired tuner config against the applied one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ApplyDecision {
    /// Newer version and a changed field the backend cannot move on a running
    /// stream: restart the `DeviceUnit`.
    Restart,
    /// Newer version and every changed field is one the backend applies in
    /// place (gain, AGC, ppm on Soapy): hand the change to the running stream
    /// and advance the applied version without a restart.
    Runtime,
    /// Newer version, and every field that changed is one the recorder only
    /// echoes back ([`ECHO_ONLY_FIELDS`]): record the new config and version in
    /// place. Nothing a `DeviceUnit` was built from moved, so a restart would
    /// buy a stream gap and nothing else.
    Recorded,
    /// Newer version but the config is identical: advance the applied version
    /// only (clears keyup's pending badge without a needless restart).
    VersionOnly,
    /// Not newer: nothing to do.
    NoOp,
}

// ---------------------------------------------------------------------------
// The field table: what is overridable, and what applying it costs
// ---------------------------------------------------------------------------

/// Fields every backend accepts, in the order the contract documents them.
const COMMON_FIELDS: &[&str] = &[
    "role",
    "site",
    "site_id",
    "center_frequency",
    "bands",
    "enabled",
    "ppm",
    "auto_ppm",
    "auto_ppm_limit",
    "data_follow",
    "remote_grants",
    "control_frequencies",
    "sample_rate",
];

/// Soapy-only fields.
const SOAPY_FIELDS: &[&str] = &[
    "gain",
    "gains",
    "agc",
    "bandwidth_hz",
    "rx_port",
    "settings",
];

/// HackRF-only fields.
const HACKRF_FIELDS: &[&str] = &["lna_db", "vga_db", "amp"];

/// Every field keyup may override on a tuner with this backend.
///
/// Backend-specific rather than one flat list because the answer is what a GUI
/// draws: offering `lna_db` for an Airspy, or `rx_port` for a HackRF, would be
/// offering a control that does nothing.
pub(crate) fn overridable_fields(backend: Backend) -> Vec<&'static str> {
    let mut out = COMMON_FIELDS.to_vec();
    match backend {
        Backend::Soapy => out.extend_from_slice(SOAPY_FIELDS),
        Backend::Hackrf => out.extend_from_slice(HACKRF_FIELDS),
        Backend::Iqfile => {}
    }
    out
}

/// Fields the recorder only carries and reports back: nothing a `DeviceUnit`
/// is built from reads them, so a change to one costs neither a restart nor a
/// mailbox hop — just a new applied config and version.
///
/// `site_id` is keyup's numeric identifier for the site a tuner is pointed at.
/// The TOML has no place to write it, so it is seeded `None` at boot and every
/// keyup push differs from the boot state in exactly this field. Classifying it
/// as a restart is what made a recorder rebuild each tuner ~1 s after start,
/// every start, for a config otherwise identical to the one already running.
pub(crate) const ECHO_ONLY_FIELDS: &[&str] = &["site_id"];

/// The fields this backend can change on a *running* stream.
///
/// Soapy owns its device handle on the reader thread and can call
/// `setGain`/`setGainElement`/`setGainMode`/`setFrequencyCorrection` between
/// reads, so those four cost nothing but a mailbox hop. The HackRF backend
/// drives a `hackrf_transfer` child it can only reconfigure by respawning —
/// which is a stream gap either way — so its gains go through the ordinary
/// restart path rather than pretending to be live. A file has no gain at all.
pub(crate) fn runtime_fields(backend: Backend) -> &'static [&'static str] {
    match backend {
        Backend::Soapy => &["gain", "gains", "agc", "ppm"],
        Backend::Hackrf | Backend::Iqfile => &[],
    }
}

/// The overridable fields that cost this backend a restart.
pub(crate) fn restart_fields(backend: Backend) -> Vec<&'static str> {
    let runtime = runtime_fields(backend);
    overridable_fields(backend)
        .into_iter()
        .filter(|f| !runtime.contains(f))
        .collect()
}

/// The names of the fields that differ between two configs.
///
/// Pure and total, and the single source of truth for "did anything change?":
/// [`decide`] classifies exactly this list, so the runtime-vs-restart table and
/// the never-restart-for-nothing rule cannot drift apart.
pub(crate) fn changed_fields(a: &AppliedConfig, b: &AppliedConfig) -> Vec<&'static str> {
    let mut out = Vec::new();
    let mut diff = |name: &'static str, differs: bool| {
        if differs {
            out.push(name);
        }
    };
    diff("role", a.role != b.role);
    diff("site", a.site != b.site);
    diff("site_id", a.site_id != b.site_id);
    diff("center_frequency", a.center_frequency != b.center_frequency);
    diff("bands", a.bands != b.bands);
    diff("enabled", a.enabled != b.enabled);
    diff("gain", a.gain != b.gain);
    diff("gains", a.gains != b.gains);
    diff("agc", a.agc != b.agc);
    diff("ppm", a.ppm != b.ppm);
    diff("auto_ppm", a.auto_ppm != b.auto_ppm);
    diff("auto_ppm_limit", a.auto_ppm_limit != b.auto_ppm_limit);
    diff("bandwidth_hz", a.bandwidth_hz != b.bandwidth_hz);
    diff("rx_port", a.rx_port != b.rx_port);
    diff("settings", a.settings != b.settings);
    diff("lna_db", a.lna_db != b.lna_db);
    diff("vga_db", a.vga_db != b.vga_db);
    diff("amp", a.amp != b.amp);
    diff("data_follow", a.data_follow != b.data_follow);
    diff("remote_grants", a.remote_grants != b.remote_grants);
    diff(
        "control_frequencies",
        a.control_frequencies != b.control_frequencies,
    );
    diff("sample_rate", a.sample_rate != b.sample_rate);
    out
}

/// The names of the fields whose effective value comes from keyup's desired
/// state rather than the TOML file.
///
/// This is what lets an operator reading the local panel tell "the file says
/// this" from "keyup says this": a tuner the file leaves on `role = "auto"`
/// that keyup has pushed to `control` looks exactly like one the file names
/// `control`, and until this list existed nothing on the page said which it
/// was.
///
/// A plain diff against the seed is the whole answer, because the seed *is* the
/// file's view of the tuner and [`merge`] only ever lays keyup's values over
/// it: a field that differs from the seed differs because keyup sent it, and a
/// field keyup sent that matches the file is not an override worth flagging.
/// `site_id` needs no special case either — the file cannot hold one, so the
/// seed's is always `None` and any value at all differs.
///
/// `spectrum` is passed separately because it lives outside [`AppliedConfig`]
/// (a live flag, not a restart parameter) and has no TOML field at all, so it
/// is an override exactly when it is on.
pub(crate) fn override_fields(
    seed: &AppliedConfig,
    current: &AppliedConfig,
    spectrum: bool,
) -> Vec<&'static str> {
    let mut out = changed_fields(seed, current);
    if spectrum {
        out.push("spectrum");
    }
    out
}

/// Decide what to do with a desired config given what is applied.
///
/// Pure and total, so the version/diff/apply-cost policy is table-testable:
/// anything not newer is a no-op; a newer version with an identical config is a
/// version-only bump; a newer version that moved only echo-only fields is
/// recorded in place; a newer version whose every changed field is one this
/// backend moves in place (or echoes) is applied at runtime; anything else
/// restarts.
pub(crate) fn decide(
    applied_version: i64,
    applied: &AppliedConfig,
    desired_version: i64,
    merged: &AppliedConfig,
    backend: Backend,
) -> ApplyDecision {
    if desired_version <= applied_version {
        return ApplyDecision::NoOp;
    }
    let changed = changed_fields(applied, merged);
    if changed.is_empty() {
        return ApplyDecision::VersionOnly;
    }
    let echo_only = |f: &&str| ECHO_ONLY_FIELDS.contains(f);
    if changed.iter().all(echo_only) {
        return ApplyDecision::Recorded;
    }
    let runtime = runtime_fields(backend);
    if changed.iter().all(|f| runtime.contains(f) || echo_only(f)) {
        ApplyDecision::Runtime
    } else {
        ApplyDecision::Restart
    }
}

/// The runtime request that carries `merged`'s live-changeable fields to the
/// reader thread. Only the fields that actually moved are sent, so a gain push
/// does not also re-assert a ppm the loop has since corrected.
fn runtime_request(applied: &AppliedConfig, merged: &AppliedConfig) -> GainRequest {
    let changed = changed_fields(applied, merged);
    GainRequest {
        gain: changed.contains(&"gain").then_some(merged.gain).flatten(),
        gains: changed
            .contains(&"gains")
            .then(|| merged.gains.clone())
            .flatten()
            .unwrap_or_default(),
        agc: changed.contains(&"agc").then_some(merged.agc).flatten(),
        ppm: changed.contains(&"ppm").then_some(merged.ppm).flatten(),
    }
}

/// Merge a desired config over the applied one: fields keyup did not specify are
/// inherited so a partial desired-state never wipes a TOML value.
fn merge(applied: &AppliedConfig, desired: &DesiredTuner) -> AppliedConfig {
    AppliedConfig {
        role: desired.role.or(applied.role),
        site: desired.site.clone().or_else(|| applied.site.clone()),
        site_id: desired.site_id.clone().or_else(|| applied.site_id.clone()),
        center_frequency: desired.center_frequency.or(applied.center_frequency),
        // `Some([])` is a real instruction ("record the whole capture"), so the
        // inherit test is on the Option, never on emptiness. The same rule
        // governs `gains`, `settings` and `control_frequencies` below.
        bands: desired.bands.clone().or_else(|| applied.bands.clone()),
        enabled: desired.enabled,
        gain: desired.gain.or(applied.gain),
        gains: desired.gains.clone().or_else(|| applied.gains.clone()),
        agc: desired.agc.or(applied.agc),
        ppm: desired.ppm.or(applied.ppm),
        auto_ppm: desired.auto_ppm.or(applied.auto_ppm),
        auto_ppm_limit: desired.auto_ppm_limit.or(applied.auto_ppm_limit),
        bandwidth_hz: desired.bandwidth_hz.or(applied.bandwidth_hz),
        rx_port: desired.rx_port.clone().or_else(|| applied.rx_port.clone()),
        settings: desired
            .settings
            .clone()
            .or_else(|| applied.settings.clone()),
        lna_db: desired.lna_db.or(applied.lna_db),
        vga_db: desired.vga_db.or(applied.vga_db),
        amp: desired.amp.or(applied.amp),
        data_follow: desired.data_follow.or(applied.data_follow),
        remote_grants: desired.remote_grants.or(applied.remote_grants),
        control_frequencies: desired
            .control_frequencies
            .clone()
            .or_else(|| applied.control_frequencies.clone()),
        sample_rate: desired.sample_rate.or(applied.sample_rate),
    }
}

/// Lay an applied config over a TOML `[[device]]`, in place: this is what makes
/// a rebuilt `DeviceUnit` run keyup's parameters instead of the file's.
///
/// Every field is `Some`-gated, so an override keyup never sent leaves the TOML
/// value alone. The collection fields are gated on the `Option`, not on
/// emptiness — an empty `bands` list is keyup saying "record the whole
/// capture", and an empty `gains` map is "no per-element gains", both of which
/// must be able to clear a value written in the TOML.
pub(crate) fn apply_override(config: &AppliedConfig, device: &mut Device) {
    if let Some(center) = config.center_frequency {
        device.center_frequency = Some(center);
    }
    if let Some(role) = config.role {
        device.role = role;
    }
    if config.site.is_some() {
        device.site = config.site.clone();
    }
    if let Some(bands) = &config.bands {
        device.bands = bands.clone();
    }
    if config.gain.is_some() {
        device.gain = config.gain;
    }
    if let Some(gains) = &config.gains {
        device.gains = gains.clone();
    }
    if config.agc.is_some() {
        device.agc = config.agc;
    }
    if let Some(ppm) = config.ppm {
        device.ppm = ppm;
    }
    if let Some(auto_ppm) = config.auto_ppm {
        device.auto_ppm = auto_ppm;
    }
    if let Some(limit) = config.auto_ppm_limit {
        device.auto_ppm_limit = limit;
    }
    if config.bandwidth_hz.is_some() {
        device.bandwidth_hz = config.bandwidth_hz;
    }
    if config.rx_port.is_some() {
        device.rx_port = config.rx_port.clone();
    }
    if let Some(settings) = &config.settings {
        device.settings = settings.clone();
    }
    if config.lna_db.is_some() {
        device.lna_db = config.lna_db;
    }
    if config.vga_db.is_some() {
        device.vga_db = config.vga_db;
    }
    if config.amp.is_some() {
        device.amp = config.amp;
    }
    if let Some(rate) = config.sample_rate {
        device.sample_rate = rate;
    }
    if config.remote_grants.is_some() {
        device.remote_grants = config.remote_grants;
    }
}

/// Lay the site-scoped half of an applied config over a loaded [`Config`], in
/// place, for the tuner `device_id`.
///
/// `data_follow` is a property of the tuner, so it is applied to **every** site
/// this tuner serves; `control_frequencies` names one channel list, so it
/// replaces that of the tuner's *primary* site — the first `[[site]]` in the
/// file that this tuner serves. A tuner that serves no site changes nothing.
pub(crate) fn apply_site_override(config: &AppliedConfig, device_id: &str, cfg: &mut Config) {
    let serves = |s: &Site| s.device_ids().contains(&device_id);
    if let Some(follow) = config.data_follow {
        for site in cfg.site.iter_mut().filter(|s| serves(s)) {
            site.data_follow = follow;
        }
    }
    if let Some(controls) = &config.control_frequencies {
        if let Some(site) = cfg.site.iter_mut().find(|s| serves(s)) {
            site.control_frequencies = controls.clone();
        }
    }
}

/// Whether a merged config's bands would leave the tuner able to record
/// nothing: it has a usable capture span, but no band it was given overlaps it.
///
/// This is the apply-time sanity net on a pushed `bands` list. A tuner whose
/// capture is unusable for other reasons (no center, an off-grid rate) records
/// nothing either way, and rejecting that here would only mask the real fault,
/// so the check is deliberately "did the *bands* do this?".
fn bands_would_record_nothing(merged: &AppliedConfig, sample_rate: u32) -> bool {
    let Some(bands) = &merged.bands else {
        return false;
    };
    if bands.is_empty() {
        return false;
    }
    let rate = merged.rate_or(sample_rate);
    crate::config::usable_span_hz(merged.center_frequency, rate).is_some()
        && crate::config::band_ranges(merged.center_frequency, rate, bands).is_empty()
}

/// The rejection a [`bands_would_record_nothing`] refusal reports to keyup:
/// the field that moved the capture off the bands, and why the entry was not
/// applied.
///
/// Named after `bands` when the push carried them, otherwise after the
/// `center_frequency` that moved the capture away from inherited bands.
fn no_overlap_rejection(desired: &DesiredTuner, merged: &AppliedConfig) -> (String, String) {
    let field = if desired.bands.is_none() && desired.center_frequency.is_some() {
        "center_frequency"
    } else {
        "bands"
    };
    let reason = format!(
        "no band in {:?} overlaps this tuner's capture at center {} Hz; the entry was not \
         applied and the tuner keeps its running config",
        merged.bands.as_deref().unwrap_or_default(),
        merged.center_frequency.unwrap_or(0),
    );
    (field.to_owned(), reason)
}

/// Which refused fields of one desired-state version have already been
/// logged at warn.
///
/// A refused entry is not acted on, so keyup keeps sending it and the control
/// loop sees it again on every poll (≈8,600 times a day). The refusal is news
/// once: `warn_first!` logs it at warn the first time for its (scope,
/// version, field) — the scope being the tuner or the recorder that owns this
/// log — and at debug after. The rejection itself keeps riding the status
/// POST, so keyup shows it until a newer version supersedes it. Holds one
/// version's fields at a time, so it never grows.
#[derive(Debug, Default)]
pub(crate) struct RejectionLog {
    version: Option<i64>,
    fields: std::collections::BTreeSet<String>,
}

impl RejectionLog {
    /// True the first time `field` is refused in `version`; a different
    /// version starts the record over.
    pub(crate) fn first(&mut self, version: i64, field: &str) -> bool {
        if self.version != Some(version) {
            self.version = Some(version);
            self.fields.clear();
        }
        self.fields.insert(field.to_owned())
    }
}

/// `tracing::warn!` when `$first` holds, `tracing::debug!` otherwise — the
/// level of a callsite is fixed at compile time, so the choice is two
/// callsites. Used with [`RejectionLog::first`].
macro_rules! warn_first {
    ($first:expr, $($arg:tt)+) => {
        if $first {
            tracing::warn!($($arg)+)
        } else {
            tracing::debug!($($arg)+)
        }
    };
}

// ---------------------------------------------------------------------------
// Validation of a pushed config, and the rejections it reports back
// ---------------------------------------------------------------------------

/// One RX gain element a backend exposes, with the range it accepts.
///
/// Learned at open (the Soapy driver's own answer) and reported on every status
/// POST, so a GUI offers the element names and limits of the board in front of
/// the operator rather than a guess. Empty for `hackrf` and `iqfile`, which
/// have no named elements.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct GainElement {
    pub name: String,
    pub min: f64,
    pub max: f64,
    pub step: f64,
}

impl GainElement {
    fn to_json(&self) -> Value {
        json!({ "name": self.name, "min": self.min, "max": self.max, "step": self.step })
    }
}

/// Largest static frequency correction accepted, in ppm either way. Real
/// oscillator error is single digits; past this the operator has typed Hz into
/// a ppm box, and applying it would walk the tuner off the band entirely.
pub(crate) const MAX_PPM: f64 = 200.0;

/// The P25 channel grid: a control frequency that is not a multiple of this is
/// not a channel any IDEN can describe.
pub(crate) const CHANNEL_GRID_HZ: u64 = 12_500;

/// What validation needs to know about the tuner a config is aimed at.
pub(crate) struct ValidateCtx<'a> {
    /// The tuner's backend, which decides which gain rules apply.
    pub backend: Backend,
    /// The gain elements the open reported; empty when unknown or not a Soapy
    /// device, in which case per-element gains are passed through unchecked
    /// (the driver is then the one that refuses them, loudly, at open).
    pub gain_elements: &'a [GainElement],
    /// The center frequency the merged config would run at.
    pub center_frequency: Option<u64>,
    /// The sample rate currently applied, before any pushed change.
    pub sample_rate: u32,
}

/// Whether a sample rate can drive the channelizer: `rate / 12500` must be an
/// even integer of at least 4. The same rule the DSP layer enforces, checked
/// here so a pushed rate is refused with a reason instead of opening a tuner
/// that cannot decode.
pub(crate) fn sample_rate_ok(rate: u32) -> bool {
    let bins = u64::from(rate) / CHANNEL_GRID_HZ;
    u64::from(rate).is_multiple_of(CHANNEL_GRID_HZ) && bins >= 4 && bins.is_multiple_of(2)
}

/// Validate a desired config against the tuner it is aimed at.
///
/// Rejected fields are cleared from `d` (so they are never applied) and
/// returned as `(field, reason)` pairs for the status report. The version is
/// deliberately *not* touched: it is still acknowledged, because a keyup that
/// never sees its version applied retries forever, and the operator needs to
/// see the reason rather than a spinner.
pub(crate) fn validate_tuner(d: &mut DesiredTuner, ctx: &ValidateCtx) -> Vec<(String, String)> {
    let mut rejected: Vec<(String, String)> = Vec::new();
    let mut reject = |field: &str, reason: String| rejected.push((field.to_owned(), reason));

    // The rate first: everything frequency-shaped below is judged against the
    // span the tuner would actually have.
    if let Some(rate) = d.sample_rate {
        if !sample_rate_ok(rate) {
            reject(
                "sample_rate",
                format!(
                    "{rate} does not drive the channelizer: sample_rate / {CHANNEL_GRID_HZ} \
                     must be an even whole number of at least 4"
                ),
            );
            d.sample_rate = None;
        }
    }
    let rate = d.sample_rate.unwrap_or(ctx.sample_rate);
    let center = d.center_frequency.or(ctx.center_frequency);

    if let Some(ppm) = d.ppm {
        if ppm.is_nan() || ppm.abs() > MAX_PPM {
            reject("ppm", format!("must be within +/-{MAX_PPM} ppm, got {ppm}"));
            d.ppm = None;
        }
    }
    if let Some(limit) = d.auto_ppm_limit {
        if limit.is_nan() || limit <= 0.0 || limit > crate::config::MAX_AUTO_PPM_LIMIT {
            reject(
                "auto_ppm_limit",
                format!(
                    "must be > 0 and <= {} ppm, got {limit}",
                    crate::config::MAX_AUTO_PPM_LIMIT
                ),
            );
            d.auto_ppm_limit = None;
        }
    }
    if let Some(bw) = d.bandwidth_hz {
        if bw == 0 {
            reject("bandwidth_hz", "must be > 0".to_owned());
            d.bandwidth_hz = None;
        }
    }

    // Soapy gains are judged against what the open reported this board has.
    if let Some(gain) = d.gain {
        let (lo, hi) = overall_gain_range(ctx.gain_elements);
        if gain.is_nan() || (!ctx.gain_elements.is_empty() && (gain < lo || gain > hi)) {
            reject(
                "gain",
                format!("must be within the device's {lo}..={hi} dB gain range, got {gain}"),
            );
            d.gain = None;
        }
    }
    if let Some(gains) = &d.gains {
        if !ctx.gain_elements.is_empty() {
            if let Some(reason) = element_gain_problem(gains, ctx.gain_elements) {
                reject("gains", reason);
                d.gains = None;
            }
        }
    }

    // HackRF gains land on hardware steps or not at all — the same rule the
    // TOML is validated by, kept in one place in `config`.
    if ctx.backend == Backend::Hackrf {
        if let Some(lna) = d.lna_db {
            if let Err(reason) = crate::config::check_hackrf_lna(lna) {
                reject("lna_db", reason);
                d.lna_db = None;
            }
        }
        if let Some(vga) = d.vga_db {
            if let Err(reason) = crate::config::check_hackrf_vga(vga) {
                reject("vga_db", reason);
                d.vga_db = None;
            }
        }
    }

    // Control candidates must be real channels the tuner can reach: a
    // candidate outside the usable span can never be decoded, and one off the
    // 12.5 kHz grid is not a P25 channel at all.
    if let Some(controls) = &d.control_frequencies {
        if let Some(reason) = control_frequency_problem(controls, center, rate) {
            reject("control_frequencies", reason);
            d.control_frequencies = None;
        }
    }
    rejected
}

/// The overall-gain range a device accepts: the sum of its elements' ranges,
/// which is what a Soapy driver's distributed `setGain` spreads across them.
/// `(0, 0)` when the elements are unknown, and the caller then checks nothing.
fn overall_gain_range(elements: &[GainElement]) -> (f64, f64) {
    (
        elements.iter().map(|e| e.min).sum(),
        elements.iter().map(|e| e.max).sum(),
    )
}

/// The first thing wrong with a per-element gain map, or `None`.
fn element_gain_problem(gains: &BTreeMap<String, f64>, elements: &[GainElement]) -> Option<String> {
    for (name, value) in gains {
        let Some(e) = elements.iter().find(|e| e.name == *name) else {
            let known: Vec<&str> = elements.iter().map(|e| e.name.as_str()).collect();
            return Some(format!(
                "'{name}' is not a gain element of this device (it has: {})",
                known.join(", ")
            ));
        };
        if value.is_nan() || *value < e.min || *value > e.max {
            return Some(format!(
                "'{name}' must be within {}..={} dB, got {value}",
                e.min, e.max
            ));
        }
    }
    None
}

/// The first thing wrong with a control candidate list, or `None`.
fn control_frequency_problem(
    controls: &[u64],
    center: Option<u64>,
    sample_rate: u32,
) -> Option<String> {
    if controls.is_empty() {
        return Some("must name at least one control frequency".to_owned());
    }
    for &f in controls {
        if !f.is_multiple_of(CHANNEL_GRID_HZ) {
            return Some(format!(
                "{f} Hz is not a multiple of the {CHANNEL_GRID_HZ} Hz channel grid"
            ));
        }
    }
    // No usable span means the tuner cannot reach anything; that is a fault of
    // the center/rate, not of this list, so it is not reported against it.
    let (lo, hi) = crate::config::usable_span_hz(center, sample_rate)?;
    for &f in controls {
        if f < lo || f > hi {
            return Some(format!(
                "{f} Hz is outside this tuner's usable span {lo}..={hi} Hz"
            ));
        }
    }
    None
}

// ---------------------------------------------------------------------------
// The shared registry
// ---------------------------------------------------------------------------

/// One config version paired with the parameters it carries.
#[derive(Clone, Debug)]
struct Versioned {
    config: AppliedConfig,
    version: i64,
}

/// Per-tuner registry entry.
///
/// Two-phase by design: `current` is what a `DeviceUnit` has *confirmed* is
/// running (and is the only thing the status report echoes), while `pending` is
/// what the next build should use. `current` only advances when a unit calls
/// [`DesiredState::confirm_applied`] after its stream opens — so a config that
/// never ran (a bad center that fails to open) never advances the applied
/// version, and keyup's pending badge stays until the change actually takes.
struct TunerEntry {
    /// The TOML's own view of this tuner, exactly as it was seeded at boot and
    /// never written again. Kept so the status report can say which of the
    /// effective values are keyup's rather than the file's
    /// ([`override_fields`]), and what the file asks for where they differ.
    seed: AppliedConfig,
    /// The confirmed-running config and the version in effect (what status
    /// echoes).
    current: Versioned,
    /// What the next (re)build should use.
    pending: Versioned,
    /// Bumped whenever a restart is needed; the `DeviceUnit` compares the value
    /// it was built at and restarts when it moves.
    generation: u64,
    /// The tuner's boot sample rate, from the TOML. The applied config carries
    /// the effective one; this is the fallback for an entry that has never had
    /// one, and the base the `bands` overlap check falls back to.
    sample_rate: u32,
    /// The tuner's backend, which decides which fields are overridable at all
    /// and which of them can be applied without a restart.
    backend: Backend,
    /// The gain elements the backend reported when it opened; empty until a
    /// stream has come up (or forever, for `hackrf`/`iqfile`). Published so a
    /// GUI can offer the real element names and limits, and used to validate a
    /// pushed `gains` map.
    gain_elements: Vec<GainElement>,
    /// Why the last push's rejected fields were rejected, reported alongside
    /// `applied_version`. Replaced wholesale by each newer version keyup sends
    /// — acted on or refused as a whole — so it always describes the newest
    /// one, and a refused version keeps being reported until a newer one
    /// supersedes it.
    rejected: Vec<(String, String)>,
    /// Which of those refusals have been logged at warn (once per version and
    /// field; a refused version comes back on every poll).
    rejections_logged: RejectionLog,
    /// The runtime mailbox: gain/AGC/ppm changes the running `DeviceUnit`
    /// picks up off the hot path and hands to its stream. Lives in the registry
    /// so it survives a rebuild, exactly as `spectrum_flag` does.
    runtime: Arc<Mutex<Option<GainRequest>>>,
    /// Latest desired spectrum flag (reported in the status body).
    spectrum: bool,
    /// Set while the tuner's unit is failing — typically failing to open — and
    /// being retried; cleared the moment a build opens (or settles idle). While
    /// it is set, nothing is running to confirm a change in place, so every
    /// newer desired version takes the rebuild path (see
    /// [`DesiredState::apply_desired`]).
    fault: Option<TunerFault>,
    /// The live spectrum on/off flag the DSP thread reads once per block (W4).
    /// Shared with the `DeviceUnit`'s `SpectrumTap`; flipped here, never a
    /// restart. Persists across device rebuilds because it lives in the registry.
    spectrum_flag: Arc<AtomicBool>,
}

/// The runtime override registry shared by the control thread (writer), the
/// device factory (reader at build time), each `DeviceUnit` (restart watcher)
/// and the status reporter.
///
/// TOML seeds it; keyup layers on top; nothing here is ever written back to the
/// TOML.
pub(crate) struct DesiredState {
    tuners: Mutex<HashMap<String, TunerEntry>>,
}

/// What the factory needs to build a `DeviceUnit`: the override to build with,
/// the generation it corresponds to, and the version the unit will confirm once
/// its stream opens.
pub(crate) struct BuiltTuner {
    pub config: AppliedConfig,
    pub generation: u64,
    pub version: i64,
}

impl DesiredState {
    /// Seed one entry per device from the TOML config (the boot defaults).
    ///
    /// Every overridable field is seeded `Some`, empty collections and all: the
    /// TOML value *is* what is in effect until keyup says otherwise, so the
    /// applied config is a complete description of the running tuner and the
    /// status report can be built from it alone. `data_follow` and
    /// `control_frequencies` come from the tuner's primary site — the first
    /// `[[site]]` in the file that this tuner serves.
    pub(crate) fn from_config(cfg: &Config) -> Arc<DesiredState> {
        let mut tuners = HashMap::new();
        for d in &cfg.device {
            let primary = cfg
                .site
                .iter()
                .find(|s| s.device_ids().contains(&d.id.as_str()));
            let config = AppliedConfig {
                role: Some(d.role),
                site: d.site.clone(),
                site_id: None,
                center_frequency: d.center_frequency,
                bands: Some(d.bands.clone()),
                enabled: true,
                gain: d.gain,
                gains: Some(d.gains.clone()),
                agc: d.agc,
                ppm: Some(d.ppm),
                auto_ppm: Some(d.auto_ppm),
                auto_ppm_limit: Some(d.auto_ppm_limit),
                bandwidth_hz: d.bandwidth_hz,
                rx_port: d.rx_port.clone(),
                settings: Some(d.settings.clone()),
                lna_db: d.lna_db,
                vga_db: d.vga_db,
                amp: d.amp,
                data_follow: primary.map(|s| s.data_follow),
                remote_grants: Some(d.accepts_remote_grants()),
                control_frequencies: primary.map(|s| s.control_frequencies.clone()),
                sample_rate: Some(d.sample_rate),
            };
            tuners.insert(
                d.id.clone(),
                TunerEntry {
                    seed: config.clone(),
                    current: Versioned {
                        config: config.clone(),
                        version: 0,
                    },
                    pending: Versioned { config, version: 0 },
                    generation: 0,
                    sample_rate: d.sample_rate,
                    backend: d.backend,
                    gain_elements: Vec::new(),
                    rejected: Vec::new(),
                    rejections_logged: RejectionLog::default(),
                    runtime: Arc::new(Mutex::new(None)),
                    spectrum: false,
                    fault: None,
                    spectrum_flag: Arc::new(AtomicBool::new(false)),
                },
            );
        }
        Arc::new(DesiredState {
            tuners: Mutex::new(tuners),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, TunerEntry>> {
        self.tuners
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The restart generation for a tuner (0 for an unknown id).
    pub(crate) fn generation_of(&self, id: &str) -> u64 {
        self.lock().get(id).map(|e| e.generation).unwrap_or(0)
    }

    /// The override to build a `DeviceUnit` with, plus the generation and the
    /// version the unit will confirm once its stream opens.
    ///
    /// Deliberately does **not** advance `current`: the applied version only
    /// moves on [`DesiredState::confirm_applied`], so a build that fails to open
    /// never clears keyup's pending badge (two-phase adopt).
    pub(crate) fn take_for_build(&self, id: &str) -> BuiltTuner {
        let map = self.lock();
        match map.get(id) {
            Some(entry) => BuiltTuner {
                config: entry.pending.config.clone(),
                generation: entry.generation,
                version: entry.pending.version,
            },
            None => BuiltTuner {
                config: AppliedConfig {
                    enabled: true,
                    ..AppliedConfig::default()
                },
                generation: 0,
                version: 0,
            },
        }
    }

    /// Confirm that a `DeviceUnit` opened successfully with `config`/`version`,
    /// advancing the in-effect (`current`) view. This is the second phase: it is
    /// what actually clears keyup's pending badge, and it runs only after the
    /// stream is up (or the tuner has settled into its disabled/idle state).
    pub(crate) fn confirm_applied(&self, id: &str, config: &AppliedConfig, version: i64) {
        let mut map = self.lock();
        if let Some(entry) = map.get_mut(id) {
            // It opened: whatever was failing before has stopped failing.
            entry.fault = None;
            // Never move the version backwards (a later apply may already have
            // advanced it in a VersionOnly bump).
            if version >= entry.current.version {
                entry.current = Versioned {
                    config: config.clone(),
                    version,
                };
            }
        }
    }

    /// Apply a desired tuner config, returning the decision taken.
    ///
    /// Fields that fail validation are dropped from the push and recorded as
    /// rejections; the rest are merged and applied. A push whose every field was
    /// rejected therefore lands as a `VersionOnly` bump: keyup's version is
    /// acknowledged (so it stops resending) and the reasons ride back on the
    /// next status POST.
    ///
    /// Two things make every newer version take the rebuild path, whatever it
    /// changed: a build still waiting to confirm an earlier version, and a unit
    /// that is failing ([`TunerFault`]). In both cases nothing is running that
    /// could confirm a change in place, so the in-place decisions would report
    /// a version applied that no tuner ever opened with; a rebuild confirms
    /// only once the stream is up. It also moves the restart generation, which
    /// is what re-arms a unit waiting out its retry backoff — a Keyup revert
    /// is tried at once, not up to ten minutes later.
    pub(crate) fn apply_desired(&self, id: &str, desired: &DesiredTuner) -> ApplyDecision {
        self.apply_desired_inner(id, desired, false)
    }

    fn apply_desired_inner(
        &self,
        id: &str,
        desired: &DesiredTuner,
        nothing_running: bool,
    ) -> ApplyDecision {
        let mut map = self.lock();
        let Some(entry) = map.get_mut(id) else {
            return ApplyDecision::NoOp;
        };
        // Nothing below this line is worth doing for a version already applied
        // *or already staged* — a staged version is applied by the rebuild (or
        // the retry) that is already coming, and restarting again for each
        // redelivery would only interrupt it. Re-running validation on a
        // redelivery would churn the reported rejections for no reason.
        if desired.version <= entry.current.version.max(entry.pending.version) {
            return ApplyDecision::NoOp;
        }
        let nothing_running = nothing_running
            || entry.fault.is_some()
            || entry.pending.version > entry.current.version;
        let mut desired = desired.clone();
        let ctx = ValidateCtx {
            backend: entry.backend,
            gain_elements: &entry.gain_elements,
            center_frequency: entry.current.config.center_frequency,
            sample_rate: entry.current.config.rate_or(entry.sample_rate),
        };
        let mut rejected = validate_tuner(&mut desired, &ctx);
        for (field, reason) in &rejected {
            warn_first!(
                entry.rejections_logged.first(desired.version, field),
                tuner = %id,
                version = desired.version,
                field = %field,
                reason = %reason,
                "desired-state field rejected; the rest of the entry still applies"
            );
        }
        let merged = merge(&entry.current.config, &desired);
        // A bands push that leaves this tuner unable to reach anything is
        // refused outright rather than staged: applying it would rebuild the
        // unit into a front that decodes control fine but records not one call,
        // and the applied version would advance so keyup's badge cleared —
        // a remote operator would have quietly muted the box. Ignoring it keeps
        // the running config and leaves keyup showing the change as pending,
        // with the reason among the rejections. Keyup keeps sending a version
        // it sees pending, so this runs on every poll until a newer one: the
        // warning is logged once per version, and debug after.
        if bands_would_record_nothing(&merged, entry.sample_rate) {
            let (field, reason) = no_overlap_rejection(&desired, &merged);
            warn_first!(
                entry.rejections_logged.first(desired.version, &field),
                tuner = %id,
                version = desired.version,
                field = %field,
                bands = ?merged.bands,
                center_hz = merged.center_frequency.unwrap_or(0),
                "desired bands leave no overlap with this tuner's capture; ignoring the entry"
            );
            rejected.push((field, reason));
            entry.rejected = rejected;
            return ApplyDecision::NoOp;
        }
        entry.rejected = rejected;
        let decision = match decide(
            entry.current.version,
            &entry.current.config,
            desired.version,
            &merged,
            entry.backend,
        ) {
            ApplyDecision::NoOp => ApplyDecision::NoOp,
            _ if nothing_running => ApplyDecision::Restart,
            d => d,
        };
        match decision {
            ApplyDecision::Restart => {
                // Same warning the TOML gets at load: a control front publishes
                // every grant regardless of its bands, so bands there change
                // nothing. Warned, not refused — it is a pointless setting, not
                // a dangerous one.
                if merged.role == Some(DeviceRole::Control)
                    && merged.bands.as_ref().is_some_and(|b| !b.is_empty())
                {
                    tracing::warn!(
                        tuner = %id,
                        "desired bands on a 'control' tuner have no effect: a control \
                         front publishes every grant; set bands on the recording tuners"
                    );
                }
                entry.pending = Versioned {
                    config: merged,
                    version: desired.version,
                };
                entry.generation += 1;
                entry.spectrum = desired.spectrum;
                entry
                    .spectrum_flag
                    .store(desired.spectrum, Ordering::Relaxed);
            }
            ApplyDecision::Runtime => {
                // Every changed field is one the backend moves in place, so the
                // change goes in the mailbox and the running unit applies it on
                // its next off-hot-path pass. `current` advances here rather
                // than through the two-phase confirm: there is no rebuild to
                // wait for, and the unit has no way to fail the request that
                // would not already be a stream fault.
                let request = runtime_request(&entry.current.config, &merged);
                {
                    let mut slot = entry
                        .runtime
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    match slot.as_mut() {
                        Some(existing) => existing.merge(request),
                        None => *slot = Some(request),
                    }
                }
                entry.current = Versioned {
                    config: merged.clone(),
                    version: desired.version,
                };
                entry.pending = Versioned {
                    config: merged,
                    version: desired.version,
                };
                entry.spectrum = desired.spectrum;
                entry
                    .spectrum_flag
                    .store(desired.spectrum, Ordering::Relaxed);
            }
            ApplyDecision::Recorded => {
                // Only echo-only fields moved, so what the running unit was
                // built from is already right. Record the merged config and the
                // version in both views — `current` directly, because there is
                // no rebuild to two-phase-confirm — and leave the generation
                // alone so no `DeviceUnit` sees a restart.
                tracing::info!(
                    tuner = %id,
                    version = desired.version,
                    fields = ?changed_fields(&entry.current.config, &merged),
                    "desired-state matches the running config; recorded version \
                     without restarting"
                );
                entry.current = Versioned {
                    config: merged.clone(),
                    version: desired.version,
                };
                entry.pending = Versioned {
                    config: merged,
                    version: desired.version,
                };
                entry.spectrum = desired.spectrum;
                entry
                    .spectrum_flag
                    .store(desired.spectrum, Ordering::Relaxed);
            }
            ApplyDecision::VersionOnly => {
                // Config unchanged (identical to what is running): advance the
                // version in place so the next status POST echoes it and keyup's
                // pending badge clears — no restart needed. Reset `pending` to
                // the current config so a previously-set-but-superseded change
                // cannot cause one spurious restart.
                entry.current.version = desired.version;
                entry.pending = Versioned {
                    config: entry.current.config.clone(),
                    version: desired.version,
                };
                entry.spectrum = desired.spectrum;
                entry
                    .spectrum_flag
                    .store(desired.spectrum, Ordering::Relaxed);
            }
            ApplyDecision::NoOp => {}
        }
        decision
    }

    /// Apply a desired tuner config **before any `DeviceUnit` has been built**.
    ///
    /// The config is staged exactly as a restart would stage it, and the
    /// restart generation is put back to zero: the very first build carries
    /// it, and the poll loop's first pass — seeing a version already staged —
    /// leaves the tuner alone instead of restarting it a second after its
    /// first lock. It is **not** marked applied here: like any other change it
    /// is confirmed by the build that opens with it. (It used to be marked
    /// applied at once, so a config that could not open was reported applied
    /// after every restart, keyup's pending badge cleared, and the same failure
    /// repeated on every start with nothing on keyup to say so.)
    ///
    /// Returns the fields the seed moved off the TOML, or `None` when nothing
    /// was seeded (unknown tuner, stale version, or an entry that was refused).
    pub(crate) fn seed_applied(
        &self,
        id: &str,
        desired: &DesiredTuner,
    ) -> Option<Vec<&'static str>> {
        let before = self.lock().get(id)?.current.config.clone();
        if matches!(
            self.apply_desired_inner(id, desired, true),
            ApplyDecision::NoOp
        ) {
            return None;
        }
        let mut map = self.lock();
        let entry = map.get_mut(id)?;
        entry.generation = 0;
        Some(changed_fields(&before, &entry.pending.config))
    }

    /// A tuner entry the parser dropped whole ([`EntryRefusal`]): report why
    /// to keyup, and log it.
    ///
    /// Only a version newer than anything applied or staged counts — the same
    /// gate [`DesiredState::apply_desired`] applies — and it then replaces the
    /// reported rejections, exactly as a refused bands push does. Nothing else
    /// moves: the tuner stays on what it runs and keyup keeps showing the
    /// version pending, with the reason. Keyup resends it on every poll, so the
    /// warning is logged once per version and field, and at debug after.
    pub(crate) fn refuse_entry(&self, refusal: &EntryRefusal) {
        let mut map = self.lock();
        let Some(entry) = map.get_mut(refusal.tuner.as_str()) else {
            tracing::debug!(tuner = %refusal.tuner, "desired-state names an unknown tuner; ignoring");
            return;
        };
        if refusal.version <= entry.current.version.max(entry.pending.version) {
            return;
        }
        warn_first!(
            entry
                .rejections_logged
                .first(refusal.version, refusal.field),
            tuner = %refusal.tuner,
            version = refusal.version,
            field = refusal.field,
            reason = refusal.reason,
            "desired-state field is unusable; ignoring this tuner's entry"
        );
        entry.rejected = vec![(refusal.field.to_owned(), refusal.reason.to_owned())];
    }

    /// Record that a tuner's unit failed and when it will be tried again —
    /// the supervisor's failure hook. Cleared by the next
    /// [`DesiredState::confirm_applied`].
    pub(crate) fn note_failure(&self, id: &str, report: &trunkd_supervisor::FailureReport<'_>) {
        if let Some(entry) = self.lock().get_mut(id) {
            entry.fault = Some(TunerFault {
                error: truncate_chars(report.error, MAX_FAULT_ERROR_LEN),
                failures: report.failures,
                retrying: report.retrying,
                next_attempt_ms: report.next_attempt_ms,
                version: entry.pending.version,
            });
        }
    }

    /// What the supervisor watches while this tuner's unit waits to be
    /// retried: the restart generation, which the panel's restart button and
    /// every newer desired version of a failing tuner move. A move cuts the
    /// wait short and starts the unit's budget over.
    pub(crate) fn rearm_token(&self, id: &str) -> u64 {
        self.generation_of(id)
    }

    /// Record the gain elements a backend reported when it opened, so the next
    /// status POST carries them and the next push is validated against them.
    pub(crate) fn set_gain_elements(&self, id: &str, elements: Vec<GainElement>) {
        if let Some(entry) = self.lock().get_mut(id) {
            entry.gain_elements = elements;
        }
    }

    /// The runtime mailbox for a tuner, shared with its `DeviceUnit`. An
    /// unknown id gets a fresh, never-filled one so no caller special-cases it.
    pub(crate) fn runtime_slot(&self, id: &str) -> Arc<Mutex<Option<GainRequest>>> {
        self.lock()
            .get(id)
            .map(|e| e.runtime.clone())
            .unwrap_or_else(|| Arc::new(Mutex::new(None)))
    }

    /// The applied (in-effect) view of a tuner for the status report.
    pub(crate) fn applied(&self, id: &str) -> Option<AppliedSnapshot> {
        self.lock().get(id).map(|e| AppliedSnapshot {
            config: e.current.config.clone(),
            version: e.current.version,
            spectrum: e.spectrum,
            gain_elements: e.gain_elements.clone(),
            rejected: e.rejected.clone(),
            backend: e.backend,
            overrides: override_fields(&e.seed, &e.current.config, e.spectrum),
            file_role: e.seed.role.unwrap_or(DeviceRole::Auto),
            fault: e.fault.clone(),
        })
    }

    /// Ask one tuner to restart with exactly the config it already has.
    ///
    /// This is the local control panel's "restart this tuner" button, and it is
    /// deliberately *not* a config change: `pending` is left untouched, so the
    /// rebuilt `DeviceUnit` comes up on the same parameters and the applied
    /// version never moves (keyup's pending badge is unaffected). Only the
    /// generation is bumped, which is what
    /// [`TunerControl::restart_due`] watches; the unit then calls
    /// `request_restart`, so the rebuild is clean and spends no restart budget.
    ///
    /// Returns false for an unknown tuner, which is how the panel answers 404.
    pub(crate) fn request_restart(&self, id: &str) -> bool {
        let mut map = self.lock();
        match map.get_mut(id) {
            Some(entry) => {
                entry.generation += 1;
                true
            }
            None => false,
        }
    }

    /// Whether a tuner has a config staged that the running `DeviceUnit` has
    /// not confirmed yet — keyup's "pending" badge, computed locally so the
    /// control panel can show the same thing without asking keyup.
    pub(crate) fn is_pending(&self, id: &str) -> bool {
        self.lock()
            .get(id)
            .map(|e| e.pending.version > e.current.version)
            .unwrap_or(false)
    }

    /// The live spectrum on/off flag for a tuner, shared with its DSP-thread
    /// `SpectrumTap` (W4). An unknown id gets a fresh, never-flipped `false`
    /// flag, so a caller never has to special-case it.
    pub(crate) fn spectrum_flag(&self, id: &str) -> Arc<AtomicBool> {
        self.lock()
            .get(id)
            .map(|e| e.spectrum_flag.clone())
            .unwrap_or_else(|| Arc::new(AtomicBool::new(false)))
    }
}

/// Handed to a `DeviceUnit` so it can honour a live config change: idle when
/// disabled, and restart itself (through the supervisor) when its restart
/// generation moves. `None` on the plain fast path (e.g. tests) — the unit then
/// always records and never self-restarts.
pub(crate) struct TunerControl {
    /// The shared registry to watch.
    pub desired: Arc<DesiredState>,
    /// This device's id, its key in the registry.
    pub device_id: String,
    /// The generation this unit was built at; a restart is due when it moves.
    pub built_generation: u64,
    /// Whether this tuner is enabled; when false the unit idles.
    pub enabled: bool,
    /// The config this unit was built with, confirmed once its stream opens.
    pub config: AppliedConfig,
    /// The version this unit will confirm as applied once its stream opens.
    pub version: i64,
    /// Gain/AGC/ppm changes to hand the running stream without a restart.
    /// Drained on the unit's off-hot-path 1 Hz pass, never per block.
    pub runtime: Arc<Mutex<Option<GainRequest>>>,
}

impl TunerControl {
    /// Whether the registry's generation has moved past what this unit was built
    /// at — i.e. a new config must be applied by restarting.
    pub(crate) fn restart_due(&self) -> bool {
        self.desired.generation_of(&self.device_id) != self.built_generation
    }

    /// Advance the applied version now that this unit is up (streaming, or
    /// settled into its disabled/idle state). The second phase of the two-phase
    /// adopt — what clears keyup's pending badge.
    pub(crate) fn confirm_applied(&self) {
        self.desired
            .confirm_applied(&self.device_id, &self.config, self.version);
    }

    /// Take whatever runtime change is waiting, if any. One uncontended mutex
    /// acquisition per second on the DSP thread, and nothing at all in the
    /// ordinary case where the slot is empty.
    pub(crate) fn take_runtime(&self) -> Option<GainRequest> {
        self.runtime
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
    }

    /// Publish the gain elements this unit's backend reported at open.
    pub(crate) fn report_gain_elements(&self, elements: Vec<GainElement>) {
        self.desired.set_gain_elements(&self.device_id, elements);
    }
}

/// A read-only view of one tuner's applied state, for the status report.
pub(crate) struct AppliedSnapshot {
    pub config: AppliedConfig,
    pub version: i64,
    pub spectrum: bool,
    /// The gain elements the backend reported at open (empty until then).
    pub gain_elements: Vec<GainElement>,
    /// `(field, reason)` for every field of the newest push that was refused.
    pub rejected: Vec<(String, String)>,
    /// The tuner's backend, which decides its capability table.
    pub backend: Backend,
    /// The TOML field names whose effective value came from keyup rather than
    /// the file ([`override_fields`]).
    pub overrides: Vec<&'static str>,
    /// The role the *file* asks for, whether or not keyup has overridden it.
    pub file_role: DeviceRole,
    /// Set while the tuner's unit is failing and being retried.
    pub fault: Option<TunerFault>,
}

/// Longest error text a status report carries for a failing tuner.
const MAX_FAULT_ERROR_LEN: usize = 300;

/// A tuner whose unit is failing — most often, cannot open its device — and is
/// being retried. Reported as `fault` on the tuner's status entry.
///
/// The applied version stays where the last successful open left it, so keyup
/// keeps showing a change that has not opened as pending; `version` says which
/// version the failing build was trying to run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TunerFault {
    /// The last failure, as the unit reported it.
    pub error: String,
    /// Consecutive failures since it last ran, or was re-armed.
    pub failures: u32,
    /// Whether the fast restart budget is spent and it is on the slow
    /// (up to ten-minute) retry.
    pub retrying: bool,
    /// When the next attempt is due, unix ms.
    pub next_attempt_ms: Option<u64>,
    /// The desired-state version the failing build carries.
    pub version: i64,
}

impl TunerFault {
    pub(crate) fn to_json(&self) -> Value {
        json!({
            "state": if self.retrying { "retrying" } else { "restarting" },
            "error": self.error,
            "failures": self.failures,
            "next_attempt_ms": self.next_attempt_ms,
            "version": self.version,
        })
    }
}

// ---------------------------------------------------------------------------
// Desired-state parsing (defensive)
// ---------------------------------------------------------------------------

/// A parsed desired-state document.
#[derive(Debug, PartialEq)]
pub(crate) struct DesiredSnapshot {
    /// Recorder-level version: keyup's cheap "anything changed?" check.
    pub version: i64,
    /// Recorder-wide settings, versioned independently of the tuners. Absent on
    /// a keyup that predates the block.
    pub recorder: Option<DesiredRecorder>,
    /// Desired config per tuner key, capped at [`MAX_TUNERS`].
    pub tuners: Vec<(String, DesiredTuner)>,
    /// Tuner entries dropped whole because a field could not be understood
    /// (see [`parse_tuner`]). Not logged by the parser: the caller logs each
    /// once per version and reports it to keyup
    /// ([`DesiredState::refuse_entry`]), since keyup resends the entry on
    /// every poll until a newer version replaces it.
    pub refused: Vec<EntryRefusal>,
}

/// A tuner entry the parser dropped whole, and the field that caused it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct EntryRefusal {
    /// The tuner key the entry was for.
    pub tuner: String,
    /// The entry's own version.
    pub version: i64,
    /// The field that could not be understood.
    pub field: &'static str,
    /// Why.
    pub reason: &'static str,
}

/// The optional top-level `"recorder"` object of a desired-state document.
///
/// ```json
/// {
///   "recorder": {
///     "version": 7,
///     "spool_only": true,
///     "event_log_archive": true,
///     "link": { "enabled": true, "bind": "0.0.0.0:7345", "peers": ["192.0.2.31:7345"],
///               "key": "…", "hello_interval_s": 10, "accept_remote_grants": true,
///               "publish_grants": true, "defer_to_claims": false },
///     "broadcastify": { "enabled": true, "endpoint_url": "https://api.broadcastify.com/call-upload",
///                       "api_key": "…", "system_id": "6551", "encoding": "mp3",
///                       "talkgroups": [52401], "skew_limit_s": 15.0, "claim": true,
///                       "claim_url": "/api/recorder/broadcastify/claim",
///                       "revision": "a1b2c3d4e5f6" },
///     "skip_talkgroups": { "tgids": [1786, 1787], "revision": "a1b2c3d4e5f6" }
///   },
///   "tuners": { … }
/// }
/// ```
///
/// Versioned on its own counter so keyup can push a recorder-wide change
/// without touching any tuner's version, and so a recorder that has already
/// applied version 7 ignores a redelivery of it. Every setting is a tri-state:
/// absent or `null` means "keyup is not managing this", which leaves whatever
/// the config or the panel last set alone.
// Not `Eq`: the broadcastify block carries a float skew limit.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct DesiredRecorder {
    /// Version of the recorder-level block.
    pub version: i64,
    /// Desired hold state, or `None` for "not managed".
    pub spool_only: Option<bool>,
    /// Desired event-log archive upload, or `None` for "not managed".
    pub event_log_archive: Option<bool>,
    /// Desired `[link]` settings, or `None` for "not managed". Each field of it
    /// is independently tri-state for the same reason.
    pub link: Option<DesiredLink>,
    /// Direct-to-Broadcastify delivery, which is tri-state one level deeper
    /// than the rest: the outer `None` is "keyup did not mention it", the inner
    /// `None` is keyup's explicit `null` — "the feature is off, drop whatever
    /// you are holding" — and a block is the configuration to apply, keyed on
    /// its own `revision` rather than on this block's version.
    pub broadcastify: Option<Option<trunkd_uplink::BroadcastifyDirect>>,
    /// Talkgroups to skip entirely — no tuner, no recording, no upload —
    /// because the operator turned Record, Transcribe and every export off for
    /// them. Tri-state one level deeper, exactly like `broadcastify`: the outer
    /// `None` is "keyup did not mention it" (keep the list being held), the
    /// inner `None` is keyup's explicit `null` ("drop the list you are
    /// holding"), and a block is the list to apply, keyed on its own
    /// `revision`. The local `[recorder] skip_talkgroups` is unioned with it
    /// and is never dropped by either.
    pub skip_talkgroups: Option<Option<trunkd_core::SkipSpec>>,
    /// `(field, reason)` for every recorder-level field of this push that was
    /// refused. The rest of the block still applies.
    pub rejected: Vec<(String, String)>,
}

/// The optional `"link"` object inside the desired `recorder` block: the whole
/// of the TOML `[link]` section, every field independently managed.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct DesiredLink {
    pub enabled: Option<bool>,
    pub bind: Option<String>,
    pub peers: Option<Vec<String>>,
    pub key: Option<String>,
    pub hello_interval_s: Option<u64>,
    pub accept_remote_grants: Option<bool>,
    pub publish_grants: Option<bool>,
    pub defer_to_claims: Option<bool>,
}

impl DesiredLink {
    /// Whether keyup sent nothing usable at all (every field unmanaged).
    fn is_empty(&self) -> bool {
        *self == DesiredLink::default()
    }

    /// Lay this over a `[link]` block, returning the effective config: a field
    /// keyup did not send keeps the value the TOML (or the last push) gave it.
    pub(crate) fn merge_over(&self, base: &crate::config::Link) -> crate::config::Link {
        crate::config::Link {
            enabled: self.enabled.unwrap_or(base.enabled),
            bind: self.bind.clone().unwrap_or_else(|| base.bind.clone()),
            peers: self.peers.clone().unwrap_or_else(|| base.peers.clone()),
            key: self.key.clone().unwrap_or_else(|| base.key.clone()),
            hello_interval_s: self.hello_interval_s.unwrap_or(base.hello_interval_s),
            accept_remote_grants: self
                .accept_remote_grants
                .unwrap_or(base.accept_remote_grants),
            publish_grants: self.publish_grants.unwrap_or(base.publish_grants),
            defer_to_claims: self.defer_to_claims.unwrap_or(base.defer_to_claims),
            // Not part of the desired state: the grant ack/retry tuning is a
            // property of this box's network, set in its TOML.
            grant_retries: base.grant_retries,
            grant_retry_ms: base.grant_retry_ms,
        }
    }
}

/// Most peers a pushed link config may name. Far beyond any real deployment;
/// the cap exists so a malformed push cannot make this recorder broadcast to
/// thousands of addresses.
const MAX_LINK_PEERS: usize = 16;

/// Longest `host:port` accepted.
const MAX_ADDR_LEN: usize = 128;

/// Longest link key accepted.
const MAX_LINK_KEY_LEN: usize = 256;

/// Widest hello interval accepted, in seconds. A peer is declared dead after
/// three of these, so a minute is already a three-minute liveness window.
const MAX_HELLO_INTERVAL_S: u64 = 60;

/// Parse the optional top-level `"recorder"` block.
///
/// Absent, `null`, or not an object: `None`, which is what an older keyup
/// sends and what a keyup that does not manage this recorder sends. A field
/// that is present but malformed is dropped and recorded in `rejected`, so the
/// rest of the block still applies and the operator sees why the one field did
/// not.
fn parse_recorder(v: Option<&Value>) -> Option<DesiredRecorder> {
    let obj = v?.as_object()?;
    let mut rejected: Vec<(String, String)> = Vec::new();
    let spool_only = managed_bool(obj.get("spool_only"), "spool_only", &mut rejected);
    let event_log_archive = managed_bool(
        obj.get("event_log_archive"),
        "event_log_archive",
        &mut rejected,
    );
    let link = match obj.get("link") {
        None | Some(Value::Null) => None,
        Some(Value::Object(m)) => {
            let parsed = parse_desired_link(m, &mut rejected);
            (!parsed.is_empty()).then_some(parsed)
        }
        Some(_) => {
            rejected.push(("link".to_owned(), "expected an object or null".to_owned()));
            None
        }
    };
    // One level deeper than its siblings: `null` is a *value* here ("the
    // feature is off"), so only an absent key means "not managed".
    let broadcastify = match obj.get("broadcastify") {
        None => None,
        Some(Value::Null) => Some(None),
        Some(v) => match trunkd_uplink::BroadcastifyDirect::from_json(v) {
            Ok(b) => Some(Some(b)),
            Err(reason) => {
                rejected.push(("broadcastify".to_owned(), reason));
                None
            }
        },
    };
    // Same shape as `broadcastify`: `null` is the value "drop it", so only an
    // absent key means "keyup is not managing the skip list".
    let skip_talkgroups = match obj.get("skip_talkgroups") {
        None => None,
        Some(Value::Null) => Some(None),
        Some(v) => match parse_skip_talkgroups(v) {
            Ok(spec) => Some(Some(spec)),
            Err(reason) => {
                rejected.push(("skip_talkgroups".to_owned(), reason));
                None
            }
        },
    };
    Some(DesiredRecorder {
        version: obj.get("version").and_then(Value::as_i64).unwrap_or(0),
        spool_only,
        event_log_archive,
        link,
        broadcastify,
        skip_talkgroups,
        rejected,
    })
}

/// Most talkgroups a pushed skip list may name. A P25 system has 65 536
/// talkgroups in total; the cap is only here so a malformed push cannot make
/// this recorder hold an unbounded list.
const MAX_SKIP_TALKGROUPS: usize = 4096;

/// Parse `recorder.skip_talkgroups`: `{"tgids": [...], "revision": "..."}`.
///
/// Refused whole rather than field by field — half a skip list is a recorder
/// that records some of what the operator turned off, which is the one outcome
/// the feature exists to prevent. Talkgroup ids are 16-bit on P25, so anything
/// outside `1..=65535` is a mistake worth reporting rather than truncating.
fn parse_skip_talkgroups(v: &Value) -> Result<trunkd_core::SkipSpec, String> {
    let obj = v.as_object().ok_or("expected an object or null")?;
    let list = match obj.get("tgids") {
        None | Some(Value::Null) => return Err("tgids is required".to_owned()),
        Some(Value::Array(a)) => a,
        Some(_) => return Err("tgids must be an array of talkgroup numbers".to_owned()),
    };
    if list.len() > MAX_SKIP_TALKGROUPS {
        return Err(format!("at most {MAX_SKIP_TALKGROUPS} talkgroups"));
    }
    let mut tgids = Vec::with_capacity(list.len());
    for entry in list {
        match entry.as_u64() {
            Some(t) if (1..=u64::from(u16::MAX)).contains(&t) => tgids.push(t as u32),
            _ => return Err("every talkgroup must be a number in 1..=65535".to_owned()),
        }
    }
    Ok(trunkd_core::SkipSpec {
        tgids,
        revision: obj
            .get("revision")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_owned(),
    })
}

/// A tri-state boolean field: absent/`null` is "not managed", a boolean is the
/// value, anything else is refused with a reason.
fn managed_bool(
    v: Option<&Value>,
    field: &str,
    rejected: &mut Vec<(String, String)>,
) -> Option<bool> {
    match v {
        None | Some(Value::Null) => None,
        Some(Value::Bool(b)) => Some(*b),
        Some(_) => {
            rejected.push((field.to_owned(), "expected true, false or null".to_owned()));
            None
        }
    }
}

/// Parse the desired `link` object, refusing field by field.
fn parse_desired_link(
    m: &serde_json::Map<String, Value>,
    rejected: &mut Vec<(String, String)>,
) -> DesiredLink {
    let mut out = DesiredLink {
        enabled: managed_bool(m.get("enabled"), "link.enabled", rejected),
        accept_remote_grants: managed_bool(
            m.get("accept_remote_grants"),
            "link.accept_remote_grants",
            rejected,
        ),
        publish_grants: managed_bool(m.get("publish_grants"), "link.publish_grants", rejected),
        defer_to_claims: managed_bool(m.get("defer_to_claims"), "link.defer_to_claims", rejected),
        ..DesiredLink::default()
    };
    match m.get("bind") {
        None | Some(Value::Null) => {}
        Some(Value::String(b)) => match addr_problem(b) {
            None => out.bind = Some(b.trim().to_owned()),
            Some(why) => rejected.push(("link.bind".to_owned(), why)),
        },
        Some(_) => rejected.push(("link.bind".to_owned(), "expected a string".to_owned())),
    }
    match m.get("peers") {
        None | Some(Value::Null) => {}
        Some(Value::Array(list)) if list.len() > MAX_LINK_PEERS => rejected.push((
            "link.peers".to_owned(),
            format!("at most {MAX_LINK_PEERS} peers"),
        )),
        Some(Value::Array(list)) => {
            let mut peers = Vec::with_capacity(list.len());
            let mut bad = None;
            for entry in list {
                match entry.as_str() {
                    Some(a) => match addr_problem(a) {
                        None => peers.push(a.trim().to_owned()),
                        Some(why) => bad = Some(why),
                    },
                    None => bad = Some("every peer must be a \"host:port\" string".to_owned()),
                }
                if bad.is_some() {
                    break;
                }
            }
            // A bad entry fails the whole list, exactly as a tuner's `bands`
            // does: half a peer list is a link that silently talks to some of
            // the boxes it was told to.
            match bad {
                None => out.peers = Some(peers),
                Some(why) => rejected.push(("link.peers".to_owned(), why)),
            }
        }
        Some(_) => rejected.push((
            "link.peers".to_owned(),
            "expected an array of \"host:port\" strings".to_owned(),
        )),
    }
    match m.get("key") {
        None | Some(Value::Null) => {}
        Some(Value::String(k)) if k.trim().len() < trunkd_link::MIN_KEY_LEN => rejected.push((
            "link.key".to_owned(),
            format!("must be at least {} characters", trunkd_link::MIN_KEY_LEN),
        )),
        Some(Value::String(k)) if k.trim().len() > MAX_LINK_KEY_LEN => rejected.push((
            "link.key".to_owned(),
            format!("must be at most {MAX_LINK_KEY_LEN} characters"),
        )),
        Some(Value::String(k)) => out.key = Some(k.trim().to_owned()),
        Some(_) => rejected.push(("link.key".to_owned(), "expected a string".to_owned())),
    }
    match m.get("hello_interval_s") {
        None | Some(Value::Null) => {}
        Some(v) => match v.as_u64() {
            Some(n) if (1..=MAX_HELLO_INTERVAL_S).contains(&n) => out.hello_interval_s = Some(n),
            _ => rejected.push((
                "link.hello_interval_s".to_owned(),
                format!("must be a whole number of seconds in 1..={MAX_HELLO_INTERVAL_S}"),
            )),
        },
    }
    out
}

/// The first thing wrong with a `host:port`, or `None`.
///
/// Deliberately syntactic: a name this recorder cannot resolve today may
/// resolve tomorrow, and refusing it here would make keyup unable to configure
/// a peer that is merely down.
fn addr_problem(addr: &str) -> Option<String> {
    let addr = addr.trim();
    if addr.is_empty() || addr.len() > MAX_ADDR_LEN {
        return Some(format!(
            "must be a \"host:port\" of at most {MAX_ADDR_LEN} characters"
        ));
    }
    let Some((host, port)) = addr.rsplit_once(':') else {
        return Some("must be \"host:port\"".to_owned());
    };
    if host.is_empty() {
        return Some("must name a host before the port".to_owned());
    }
    match port.parse::<u16>() {
        Ok(p) if p > 0 => None,
        _ => Some(format!("'{port}' is not a port number in 1..=65535")),
    }
}

// ---------------------------------------------------------------------------
// Recorder-level control: the uplink hold, the event-log archive, the link
// ---------------------------------------------------------------------------

/// The recorder-wide half of the control plane: the settings keyup manages for
/// the whole box rather than for one tuner, plus the desired-state version they
/// were last set from.
///
/// Shared by the control thread (which applies keyup's pushes), the local panel
/// (which can toggle the hold), and the uplink/observations threads (which read
/// it). The hold and the archive flag are atomics, so the panel toggling one
/// cannot block the thread that is about to read it; the link settings sit
/// behind the [`LinkSlot`]'s own lock, and nothing here is ever touched from a
/// DSP thread.
pub(crate) struct RecorderControl {
    hold: Arc<UplinkHold>,
    /// The event-log archive's live state; its `archive` flag is what the
    /// archive thread gates its uploads on.
    archive: Arc<crate::pipeline::eventarchive::ArchiveState>,
    /// The recorder link, swappable: a keyup link push rebuilds it in place.
    link: Arc<LinkSlot>,
    /// Direct-to-Broadcastify delivery: the block keyup last pushed, the
    /// counters and the last response. Read by the uplink worker on every call
    /// and by the panel; written only here.
    broadcastify: Arc<trunkd_uplink::BroadcastifyState>,
    /// Talkgroups this recorder refuses to record. The very `Arc` every site
    /// engine holds: applying keyup's push here is what makes the next grant on
    /// a DSP thread be refused, with no restart and nothing rebuilt.
    skip: Arc<trunkd_core::SkipList>,
    /// Last recorder-level version applied from keyup; [`NO_VERSION`] until one
    /// has been.
    applied_version: AtomicI64,
    /// The `spool_only` value that version carried, echoed back to keyup so it
    /// can tell "applied" from "still pending".
    applied_hold: AtomicBool,
    /// The rest of what that version carried, echoed the same way.
    applied_rest: Mutex<AppliedRecorderRest>,
    /// Which refused recorder-level fields have been logged at warn: keyup
    /// sends the block on every poll, and a refusal is news once per version.
    rejections_logged: Mutex<RejectionLog>,
    /// Whether keyup, rather than the TOML, last set the link config.
    link_from_keyup: AtomicBool,
    /// The top-level `version` of the newest desired-state document this
    /// recorder has fetched and run through the apply path; [`NO_VERSION`]
    /// until one has been. Reported to keyup as `desired_seen` on the push
    /// channel's hello, which is how the server decides whether to tell this
    /// recorder to fetch again the moment it connects.
    desired_seen: AtomicI64,
}

impl std::fmt::Debug for RecorderControl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecorderControl")
            .field("applied_version", &self.applied_version)
            .field("applied_hold", &self.applied_hold)
            .finish_non_exhaustive()
    }
}

/// The non-atomic half of what keyup last pushed, as applied.
#[derive(Clone, Debug, Default)]
struct AppliedRecorderRest {
    event_log_archive: bool,
    link: crate::config::Link,
    rejected: Vec<(String, String)>,
}

/// The `recorder_applied` echo of the status POST.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RecorderApplied {
    pub version: i64,
    pub spool_only: bool,
    pub event_log_archive: bool,
    /// The link settings in effect. The key itself is never echoed.
    pub link: crate::config::Link,
    /// Recorder-level fields of that push that were refused, with reasons.
    pub rejected: Vec<(String, String)>,
}

impl RecorderApplied {
    /// The wire object.
    pub(crate) fn to_json(&self) -> Value {
        let mut obj = serde_json::Map::new();
        obj.insert("version".to_owned(), json!(self.version));
        obj.insert("spool_only".to_owned(), json!(self.spool_only));
        obj.insert(
            "event_log_archive".to_owned(),
            json!(self.event_log_archive),
        );
        obj.insert("link".to_owned(), applied_link_json(&self.link));
        if !self.rejected.is_empty() {
            let mut r = serde_json::Map::new();
            for (field, reason) in &self.rejected {
                r.insert(field.clone(), Value::String(reason.clone()));
            }
            obj.insert("rejected".to_owned(), Value::Object(r));
        }
        Value::Object(obj)
    }
}

/// The applied `[link]` block as keyup sees it: every field except the key,
/// which is replaced by whether one is set and a short fingerprint of it.
pub(crate) fn applied_link_json(link: &crate::config::Link) -> Value {
    json!({
        "enabled": link.enabled,
        "bind": link.bind,
        "peers": link.peers,
        "hello_interval_s": link.hello_interval_s,
        "accept_remote_grants": link.accept_remote_grants,
        "publish_grants": link.publish_grants,
        "defer_to_claims": link.defer_to_claims,
        // The key never leaves the box. The fingerprint is enough for keyup to
        // show that two peers hold the same secret, and useless to anyone who
        // wants to speak the protocol.
        "key_set": !link.key.trim().is_empty(),
        "key_fingerprint": crate::pipeline::link::key_fingerprint(&link.key),
    })
}

/// What the recorder-level settings cost to apply, reported as
/// `capabilities.recorder` so a GUI can say so before the operator presses
/// apply.
///
/// Nothing here restarts a tuner or the process: the hold and the archive flag
/// are live atomics, and a link change rebuilds the link's own sockets and
/// threads while every tuner keeps recording.
pub(crate) fn recorder_capabilities_json() -> Value {
    json!({ "restart_on": Vec::<&str>::new() })
}

/// "keyup has never pushed a recorder block", distinct from version 0.
const NO_VERSION: i64 = i64::MIN;

impl RecorderControl {
    /// A control block over the recorder-wide state keyup manages.
    pub(crate) fn new(
        hold: Arc<UplinkHold>,
        archive: Arc<crate::pipeline::eventarchive::ArchiveState>,
        link: Arc<LinkSlot>,
        broadcastify: Arc<trunkd_uplink::BroadcastifyState>,
        skip: Arc<trunkd_core::SkipList>,
    ) -> Arc<RecorderControl> {
        Arc::new(RecorderControl {
            hold,
            archive,
            link,
            broadcastify,
            skip,
            applied_version: AtomicI64::new(NO_VERSION),
            applied_hold: AtomicBool::new(false),
            applied_rest: Mutex::new(AppliedRecorderRest::default()),
            rejections_logged: Mutex::new(RejectionLog::default()),
            link_from_keyup: AtomicBool::new(false),
            desired_seen: AtomicI64::new(NO_VERSION),
        })
    }

    /// A control block with a fresh archive state and no link, for tests and
    /// for the panel's own fixtures.
    #[cfg(test)]
    pub(crate) fn for_test(hold: Arc<UplinkHold>) -> Arc<RecorderControl> {
        RecorderControl::new(
            hold,
            crate::pipeline::eventarchive::ArchiveState::new(false),
            crate::pipeline::link::LinkSlot::disabled(),
            trunkd_uplink::BroadcastifyState::new(true, &trunkd_core::Metrics::new()),
            trunkd_core::SkipList::empty(),
        )
    }

    /// The shared hold, for the threads that only read it.
    pub(crate) fn hold(&self) -> &Arc<UplinkHold> {
        &self.hold
    }

    /// The direct-to-Broadcastify state, for the panel and the uplink.
    pub(crate) fn broadcastify(&self) -> &Arc<trunkd_uplink::BroadcastifyState> {
        &self.broadcastify
    }

    /// The skip list. The DSP threads get the same `Arc` through
    /// [`crate::pipeline::Shared`]; the panel reads it through
    /// [`RecorderControl::effective_json`], so this is the tests' handle on it.
    #[cfg(test)]
    pub(crate) fn skip(&self) -> &Arc<trunkd_core::SkipList> {
        &self.skip
    }

    /// Apply one desired recorder block, returning whether anything moved.
    ///
    /// Ignored when the version is not newer than the one already applied — a
    /// redelivery of a version this recorder has acted on must not undo a panel
    /// toggle made since — and when keyup manages nothing at all (an all-`null`
    /// block, which is how it says "your config is yours").
    pub(crate) fn apply_desired(&self, d: &DesiredRecorder) -> bool {
        // Broadcastify first, and outside both gates below: it carries its own
        // `revision`, which is what decides whether it has changed. Keyup can
        // therefore rotate a Broadcastify key without touching the recorder
        // block's version, and a redelivery of the same revision still costs
        // nothing.
        let mut moved = false;
        if let Some(next) = &d.broadcastify {
            moved |= self.broadcastify.apply(next.clone());
        }
        // The skip list is revision-gated in the same way and for the same
        // reason: the operator flips a talkgroup's switches in keyup and expects
        // this recorder to stop tuning it, without a recorder-block version bump
        // and without a restart.
        if let Some(next) = &d.skip_talkgroups {
            if self.skip.apply(next.clone()) {
                moved = true;
                match next {
                    Some(spec) => tracing::info!(
                        revision = %spec.revision,
                        talkgroups = spec.tgids.len(),
                        effective = self.skip.len(),
                        "applied keyup's skip_talkgroups list"
                    ),
                    None => tracing::info!(
                        effective = self.skip.len(),
                        "keyup dropped its skip_talkgroups list; \
                         only [recorder] skip_talkgroups remains"
                    ),
                }
            }
        }
        if d.spool_only.is_none()
            && d.event_log_archive.is_none()
            && d.link.is_none()
            && d.rejected.is_empty()
        {
            return moved;
        }
        let applied = self.applied_version.load(Ordering::Relaxed);
        if applied != NO_VERSION && d.version <= applied {
            return moved;
        }
        let mut wide = false;
        if let Some(want) = d.spool_only {
            wide |= self.hold.set(want, HoldSource::Keyup);
        }
        if let Some(want) = d.event_log_archive {
            wide |= self.archive.set_archive(want, ArchiveSource::Keyup);
        }
        if let Some(link) = &d.link {
            // Merged over what is in effect, so a push that names only
            // `peers` keeps the key and the bind the TOML gave this box.
            let next = link.merge_over(&self.link.config());
            self.link_from_keyup.store(true, Ordering::Relaxed);
            wide |= self.link.set_config(next);
        }
        if wide {
            // A warning because an operator changed what this box does with
            // its calls (hold, archive, link) from somewhere else; the
            // revision-gated blocks above are routine and log at info.
            tracing::warn!(
                version = d.version,
                spool_only = self.hold.is_held(),
                event_log_archive = ?d.event_log_archive,
                link = d.link.is_some(),
                "keyup changed the recorder-wide settings"
            );
        }
        moved |= wide;
        self.applied_version.store(d.version, Ordering::Relaxed);
        self.applied_hold
            .store(self.hold.is_held(), Ordering::Relaxed);
        *self.applied_rest.lock().unwrap_or_else(|p| p.into_inner()) = AppliedRecorderRest {
            event_log_archive: self.archive.enabled(),
            link: self.link.config(),
            rejected: d.rejected.clone(),
        };
        moved
    }

    /// Toggle the hold from the local panel.
    ///
    /// Deliberately does not touch the applied keyup version: `recorder_applied`
    /// says what keyup asked for and got, and a panel toggle is precisely the
    /// case keyup needs to be able to see diverging from that. The next keyup
    /// push with a newer version wins, as the most-recent-writer rule says.
    pub(crate) fn set_hold_from_panel(&self, held: bool) -> bool {
        self.hold.set(held, HoldSource::Panel)
    }

    /// The recorder-wide settings in effect right now, and who last set each.
    ///
    /// Reported by the local panel rather than to keyup: `recorder_applied` is
    /// the echo of keyup's own push, while this is the truth about the box even
    /// when keyup has never pushed anything.
    pub(crate) fn effective_json(&self) -> Value {
        json!({
            "spool_only": self.hold.is_held(),
            "spool_only_source": self.hold.source().as_str(),
            "event_log_archive": self.archive.enabled(),
            "event_log_archive_source": self.archive.source().as_str(),
            "link": applied_link_json(&self.link.config()),
            "link_source": match self.link_from_keyup.load(Ordering::Relaxed) {
                true => "keyup",
                false => "config",
            },
            // Counters, the revision in force, and whether a key is set — never
            // the key itself.
            "broadcastify": self.broadcastify.status_json(),
            // The talkgroups this recorder refuses to tune: how many in total,
            // the revision of keyup's half (`null` when it is managing none),
            // and the local half in full, so an operator can see at a glance
            // which entries are the file's and cannot be taken away by keyup.
            "skip_talkgroups": {
                "count": self.skip.len(),
                "revision": match self.skip.revision() {
                    Some(r) => Value::String(r),
                    None => Value::Null,
                },
                "local": self.skip.local(),
            },
        })
    }

    /// Record the top-level version of a desired-state document that has been
    /// fetched and applied. Monotonic: an out-of-order or stale body cannot
    /// walk the reported high-water mark backwards.
    pub(crate) fn note_desired_seen(&self, version: i64) {
        let mut cur = self.desired_seen.load(Ordering::Relaxed);
        while cur == NO_VERSION || version > cur {
            match self.desired_seen.compare_exchange_weak(
                cur,
                version,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return,
                Err(actual) => cur = actual,
            }
        }
    }

    /// The newest top-level desired version applied, or `None` before any
    /// desired-state document has been fetched.
    pub(crate) fn desired_seen(&self) -> Option<i64> {
        match self.desired_seen.load(Ordering::Relaxed) {
            NO_VERSION => None,
            v => Some(v),
        }
    }

    /// What to echo as `recorder_applied`, or `None` before keyup has pushed
    /// anything.
    pub(crate) fn applied(&self) -> Option<RecorderApplied> {
        let version = match self.applied_version.load(Ordering::Relaxed) {
            NO_VERSION => return None,
            v => v,
        };
        let rest = self
            .applied_rest
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        Some(RecorderApplied {
            version,
            spool_only: self.applied_hold.load(Ordering::Relaxed),
            event_log_archive: rest.event_log_archive,
            link: rest.link,
            rejected: rest.rejected,
        })
    }
}

/// The `"uplink"` object reported on the status POST and by the panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UplinkReport {
    /// Whether uploads are held right now.
    pub spool_only: bool,
    /// Who last set that: `config` | `keyup` | `panel`.
    pub source: &'static str,
    /// Calls sitting in the offline spool.
    pub spooled_calls: u64,
    /// Bytes the spool occupies.
    pub spool_bytes: u64,
    /// Where calls are uploaded when the hold is released.
    pub url: String,
    /// Calls spooled without an upload attempt: the queue was full or Keyup
    /// was failing (the circuit was open). Lifetime total.
    pub spooled_on_pressure: u64,
    /// Spooled calls moved to `<spool>/quarantine/` after failing repeatedly
    /// while Keyup took others. Lifetime total; each one wants a look.
    pub quarantined: u64,
    /// Live calls are going straight to the spool because Keyup is failing.
    pub circuit_open: bool,
    /// The keyup push channel's health (W6).
    pub stream: crate::pipeline::stream::StreamReport,
}

impl UplinkReport {
    /// The wire object.
    pub(crate) fn to_json(&self) -> Value {
        json!({
            "spool_only": self.spool_only,
            "source": self.source,
            "spooled_calls": self.spooled_calls,
            "spool_bytes": self.spool_bytes,
            "url": self.url,
            // Why calls are in the spool although nobody held them, and which
            // ones will not leave it on their own. keyup stores the block
            // verbatim (`UplinkIn` keeps unknown fields).
            "spooled_on_pressure": self.spooled_on_pressure,
            "quarantined": self.quarantined,
            "circuit_open": self.circuit_open,
            // The push channel is reported under `uplink` because it is the
            // same connection's health: one place an operator (or keyup) looks
            // to answer "is this recorder reachable, and how fast does it hear
            // about a change?".
            "stream": self.stream.to_json(),
        })
    }
}

/// Build the `uplink` report from the counters already gathered for the status
/// body, so the two can never disagree about how much is spooled.
pub(crate) fn uplink_report(
    counters: &Counters,
    recorder: &RecorderControl,
    url: &str,
    stream: &crate::pipeline::stream::StreamState,
) -> UplinkReport {
    UplinkReport {
        spool_only: recorder.hold().is_held(),
        source: recorder.hold().source().as_str(),
        spooled_calls: counters.spooled,
        spool_bytes: counters.spool_bytes,
        url: url.to_owned(),
        spooled_on_pressure: counters.spooled_on_pressure,
        quarantined: counters.quarantined,
        circuit_open: counters.circuit_open,
        stream: stream.report(),
    }
}

/// Parse a desired-state JSON body into a snapshot, defensively.
///
/// Never panics and never errors: a malformed top level yields an empty
/// snapshot, a malformed tuner entry is skipped, and entries beyond
/// [`MAX_TUNERS`] are dropped. The caller treats "could not understand it" the
/// same as "keyup said nothing", i.e. keep the last-applied state.
pub(crate) fn parse_desired(body: &str) -> DesiredSnapshot {
    let empty = DesiredSnapshot {
        version: 0,
        recorder: None,
        tuners: Vec::new(),
        refused: Vec::new(),
    };
    if body.len() > MAX_RESPONSE_BYTES {
        tracing::warn!(
            bytes = body.len(),
            "desired-state response is oversized; ignoring it"
        );
        return empty;
    }
    let Ok(doc) = serde_json::from_str::<Value>(body) else {
        tracing::warn!("desired-state response is not valid JSON; ignoring it");
        return empty;
    };
    let version = doc.get("version").and_then(Value::as_i64).unwrap_or(0);
    let recorder = parse_recorder(doc.get("recorder"));
    let mut tuners = Vec::new();
    let mut refused = Vec::new();
    if let Some(map) = doc.get("tuners").and_then(Value::as_object) {
        for (key, entry) in map {
            if tuners.len() >= MAX_TUNERS {
                tracing::warn!(
                    cap = MAX_TUNERS,
                    "desired-state lists more tuners than the cap; ignoring the rest"
                );
                break;
            }
            if key.chars().count() > MAX_TUNER_KEY_LEN {
                tracing::debug!(key, "desired tuner key over the length cap; skipping");
                continue;
            }
            match parse_tuner(entry) {
                Ok(Some(t)) => tuners.push((key.clone(), t)),
                Ok(None) => {}
                Err((field, reason)) => refused.push(EntryRefusal {
                    tuner: key.clone(),
                    version: entry.get("version").and_then(Value::as_i64).unwrap_or(0),
                    field,
                    reason,
                }),
            }
        }
    }
    DesiredSnapshot {
        version,
        recorder,
        tuners,
        refused,
    }
}

/// Most band entries one tuner's desired config may carry.
const MAX_BANDS: usize = 16;

/// Longest single band label accepted (`"858000000-866000000"` is 19).
const MAX_BAND_LEN: usize = 64;

/// Parse a desired `bands` array: `Ok(None)` when keyup said nothing (inherit),
/// `Ok(Some(vec))` for a list it did send (possibly empty = no restriction), and
/// `Err(reason)` for anything malformed.
///
/// A bad entry fails the *whole* list rather than being dropped from it, and the
/// caller then ignores the whole tuner. Partially applying a band list is the
/// one outcome that must not happen: silently dropping the one label an operator
/// mistyped would narrow what the tuner records to something nobody asked for,
/// and it would look applied on keyup.
fn parse_bands(v: Option<&Value>) -> Result<Option<Vec<String>>, &'static str> {
    let Some(v) = v else {
        return Ok(None);
    };
    if v.is_null() {
        return Ok(None);
    }
    let Some(arr) = v.as_array() else {
        return Err("bands is not an array");
    };
    if arr.len() > MAX_BANDS {
        return Err("bands has more entries than the cap");
    }
    let mut out = Vec::with_capacity(arr.len());
    for entry in arr {
        let Some(s) = entry.as_str() else {
            return Err("a bands entry is not a string");
        };
        if s.chars().count() > MAX_BAND_LEN {
            return Err("a bands entry is over the length cap");
        }
        if crate::config::resolve_band(s).is_none() {
            return Err("a bands entry names no known band or Hz range");
        }
        out.push(s.to_owned());
    }
    Ok(Some(out))
}

/// Most entries a desired `gains`, `settings` or `control_frequencies` may
/// carry. Generous for a real radio and small enough that a malformed push
/// cannot allocate anything interesting.
const MAX_MAP_ENTRIES: usize = 32;

/// Longest key or value accepted inside `gains` / `settings` / `rx_port`.
const MAX_KEY_LEN: usize = 64;

/// Parse a JSON object of string -> number, e.g. a `gains` map.
///
/// `Ok(None)` when keyup said nothing (inherit), `Ok(Some(map))` for a map it
/// did send (possibly empty, which clears the TOML's), `Err(reason)` for
/// anything malformed. As with `bands`, one bad entry fails the whole map:
/// silently dropping the element an operator mistyped would leave the tuner on
/// a gain nobody asked for and show as applied.
fn parse_f64_map(v: Option<&Value>) -> Result<Option<BTreeMap<String, f64>>, &'static str> {
    let Some(obj) = optional_object(v)? else {
        return Ok(None);
    };
    if obj.len() > MAX_MAP_ENTRIES {
        return Err("the map has more entries than the cap");
    }
    let mut out = BTreeMap::new();
    for (k, value) in obj {
        if k.chars().count() > MAX_KEY_LEN {
            return Err("a map key is over the length cap");
        }
        let Some(n) = value.as_f64() else {
            return Err("a map value is not a number");
        };
        if !n.is_finite() {
            return Err("a map value is not finite");
        }
        out.insert(k.clone(), n);
    }
    Ok(Some(out))
}

/// Parse a JSON object of string -> string, e.g. a `settings` map.
fn parse_str_map(v: Option<&Value>) -> Result<Option<BTreeMap<String, String>>, &'static str> {
    let Some(obj) = optional_object(v)? else {
        return Ok(None);
    };
    if obj.len() > MAX_MAP_ENTRIES {
        return Err("the map has more entries than the cap");
    }
    let mut out = BTreeMap::new();
    for (k, value) in obj {
        if k.chars().count() > MAX_KEY_LEN {
            return Err("a map key is over the length cap");
        }
        // A driver setting is a string on the wire whichever way it is typed:
        // `biastee = true` and `biastee = "true"` mean the same thing to
        // `writeSetting`, so both are accepted rather than one being a parse
        // error an operator has to discover.
        let text = match value {
            Value::String(t) => t.clone(),
            Value::Bool(b) => b.to_string(),
            Value::Number(n) => n.to_string(),
            _ => return Err("a settings value is not a string, number or bool"),
        };
        if text.chars().count() > MAX_KEY_LEN {
            return Err("a settings value is over the length cap");
        }
        out.insert(k.clone(), text);
    }
    Ok(Some(out))
}

/// `Ok(None)` for absent/null, `Ok(Some(obj))` for an object, `Err` otherwise.
fn optional_object(
    v: Option<&Value>,
) -> Result<Option<&serde_json::Map<String, Value>>, &'static str> {
    match v {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Object(o)) => Ok(Some(o)),
        Some(_) => Err("the value is not an object"),
    }
}

/// Parse a desired `control_frequencies` array in Hz.
fn parse_frequencies(v: Option<&Value>) -> Result<Option<Vec<u64>>, &'static str> {
    let Some(v) = v else { return Ok(None) };
    if v.is_null() {
        return Ok(None);
    }
    let Some(arr) = v.as_array() else {
        return Err("control_frequencies is not an array");
    };
    if arr.len() > MAX_MAP_ENTRIES {
        return Err("control_frequencies has more entries than the cap");
    }
    arr.iter()
        .map(|e| {
            e.as_u64()
                .ok_or("a control frequency is not a whole number of Hz")
        })
        .collect::<Result<Vec<u64>, _>>()
        .map(Some)
}

/// A finite `f64` field, or `None` when absent. A non-finite number is treated
/// as absent rather than rejected: NaN is not a value an operator meant.
fn parse_f64(v: Option<&Value>) -> Option<f64> {
    v.and_then(Value::as_f64).filter(|n| n.is_finite())
}

/// A `u32` field that fits, or `None`.
fn parse_u32(v: Option<&Value>) -> Option<u32> {
    v.and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
}

/// Parse one tuner's desired entry: `Ok(None)` when it carries no `config`
/// object at all, `Err((field, reason))` when a field is present but cannot be
/// understood — the whole entry is then dropped rather than half applied.
fn parse_tuner(entry: &Value) -> Result<Option<DesiredTuner>, (&'static str, &'static str)> {
    let version = entry.get("version").and_then(Value::as_i64).unwrap_or(0);
    let Some(config) = entry.get("config").and_then(Value::as_object) else {
        return Ok(None);
    };
    let role = config
        .get("role")
        .and_then(Value::as_str)
        .and_then(role_from_str);
    // The label and the numeric id are kept distinct: the status body echoes
    // both so keyup can match on whichever it uses.
    let site = config
        .get("site")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let site_id = stringify_scalar(config.get("site_id"));
    let center_frequency = config.get("center_frequency").and_then(Value::as_u64);
    let enabled = config
        .get("enabled")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let spectrum = config
        .get("spectrum")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    // An unusable field drops the tuner's whole entry: the rest of the
    // payload's tuners still apply, and this one stays on what it is running.
    // Every map-shaped field follows the `bands` rule rather than applying
    // half of the entry.
    let bands = parse_bands(config.get("bands")).map_err(|reason| ("bands", reason))?;
    let gains = parse_f64_map(config.get("gains")).map_err(|reason| ("gains", reason))?;
    let settings = parse_str_map(config.get("settings")).map_err(|reason| ("settings", reason))?;
    let control_frequencies = parse_frequencies(config.get("control_frequencies"))
        .map_err(|reason| ("control_frequencies", reason))?;
    Ok(Some(DesiredTuner {
        version,
        role,
        site,
        site_id,
        center_frequency,
        bands,
        enabled,
        spectrum,
        gain: parse_f64(config.get("gain")),
        gains,
        agc: config.get("agc").and_then(Value::as_bool),
        ppm: parse_f64(config.get("ppm")),
        auto_ppm: config.get("auto_ppm").and_then(Value::as_bool),
        auto_ppm_limit: parse_f64(config.get("auto_ppm_limit")),
        bandwidth_hz: parse_u32(config.get("bandwidth_hz")),
        rx_port: config
            .get("rx_port")
            .and_then(Value::as_str)
            .map(|s| truncate_chars(s, MAX_KEY_LEN)),
        settings,
        lna_db: parse_u32(config.get("lna_db")),
        vga_db: parse_u32(config.get("vga_db")),
        amp: config.get("amp").and_then(Value::as_bool),
        data_follow: config.get("data_follow").and_then(Value::as_bool),
        remote_grants: config.get("remote_grants").and_then(Value::as_bool),
        control_frequencies,
        sample_rate: parse_u32(config.get("sample_rate")),
    }))
}

/// A JSON string or number rendered as a plain string; `None` for anything else.
fn stringify_scalar(v: Option<&Value>) -> Option<String> {
    match v {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Number(n)) => Some(n.to_string()),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Status report body (pure)
// ---------------------------------------------------------------------------

/// One tuner's reported status, gathered from W1's metrics and config.
#[derive(Clone, Debug, Default)]
pub(crate) struct TunerStatus {
    pub tuner_key: String,
    pub serial: Option<String>,
    pub backend: &'static str,
    pub center_hz: u64,
    pub span_hz: u64,
    pub role: &'static str,
    pub site: Option<String>,
    pub site_id: Option<String>,
    /// Whether this tuner is switched on at all. Reported because "keyup
    /// disabled it" and "it is broken" look identical from every other field:
    /// a disabled tuner idles with its board open, decodes nothing, writes no
    /// event log and starts no call, and an operator watching the panel needs
    /// to be told which of the two they are looking at.
    pub enabled: bool,
    pub locked: bool,
    pub control_hz: u64,
    pub nac: u32,
    pub tsbk_rate: f64,
    pub pool_in_use: u64,
    pub pool_size: u64,
    pub overflow_per_s: u64,
    /// How many times this tuner's stream clock has been re-anchored since the
    /// process started (see `crate::pipeline::streamclock`). Non-zero means the
    /// host lost samples: every timestamp before the resync was early by
    /// `last_clock_resync_ms`.
    pub clock_resyncs: u64,
    /// The size of the most recent re-anchor, in signed milliseconds — *not* a
    /// timestamp. Positive: the stream clock had fallen behind wall time and
    /// calls were being stamped that much early. 0 when nothing has been
    /// corrected.
    pub last_clock_resync_ms: i64,
    /// The stream clock's averaged disagreement with the host clock right now,
    /// in signed milliseconds (positive: stamps early). The slow trim keeps it
    /// within about ±20 ms; a value stuck well outside that is a clock the
    /// trim cannot follow.
    pub clock_error_ms: i64,
    /// Everything the slow trim has moved this tuner's stamps by since the
    /// stream started, signed. Its slope is the sample clock's rate error
    /// against the host's: 86 ms a day is 1 ppm.
    pub clock_trim_ms: i64,
    /// Blocks that arrived later than the producer should ever be — an
    /// independent count of the sample discontinuities the resync corrects,
    /// visible whether or not the drift ever crossed the resync threshold.
    pub stream_gaps: u64,
    pub calls_active: u64,
    pub applied_version: i64,
    pub spectrum: bool,
    /// The TOML field names whose effective value came from keyup's desired
    /// state rather than the file. Every other reported setting is the file's.
    ///
    /// Reported *always*, empty list included — unlike its optional
    /// neighbours. An absent field and an empty one would otherwise say the
    /// same thing on the wire, and a panel that cannot tell "this tuner runs
    /// exactly what the file says" from "this recorder is too old to tell you"
    /// has to stay silent in both cases, which is the bug this exists to fix.
    pub overrides: Vec<&'static str>,
    /// The role the file asks for, present exactly when `role` is overridden:
    /// what the panel puts next to the effective role so an operator reading
    /// "control" can see the file said "auto". `None` when the role in force
    /// *is* the file's.
    pub file_role: Option<&'static str>,
    /// The band restriction actually in force (applied config, not the TOML).
    /// Empty means unrestricted, and is omitted from the body.
    pub bands: Vec<String>,
    /// Operator-entered hardware description, with `receiver` already defaulted
    /// from the backend.
    pub description: TunerDescription,
    /// The effective tuner settings — what the tuner is actually running, TOML
    /// and keyup overrides already resolved. This is the half of the report a
    /// GUI edits, so every one of them is reported whether keyup set it or the
    /// TOML did.
    pub settings_applied: AppliedSettings,
    /// The gain elements the backend reported at open; empty for `hackrf` and
    /// `iqfile`, and until a Soapy stream has come up.
    pub gain_elements: Vec<GainElement>,
    /// What this tuner can change without a restart, and what it cannot.
    pub capabilities: TunerCapabilities,
    /// `(field, reason)` for every field of the newest desired-state push that
    /// was refused. Empty is the normal case and is omitted from the body.
    pub rejected: Vec<(String, String)>,
    /// Measured wideband level in dBFS; `None` when never measured or stale.
    pub rssi_dbfs: Option<f64>,
    /// How far off frequency the control channel's carrier actually is, in Hz,
    /// for a control-following tuner only; `None` otherwise, when stale, or
    /// while the site is still hunting.
    ///
    /// This is the carrier meter's offset — the number the auto-ppm loop
    /// trusts and corrects on. It used to be the demodulator loop's phase
    /// error instead, which [`trunkd_p25::dsp::carrier`] is explicit is *not*
    /// a carrier measurement: it read 422 Hz on a healthy, locked Windows
    /// tuner and an operator reasonably read that as a board 0.5 ppm out.
    pub freq_error_hz: Option<f64>,
    /// The demodulator loop's own phase-error reading, in Hz — what
    /// `freq_error_hz` used to carry. Kept because it is a real diagnostic of
    /// the *demodulator* (a lock that is working hard shows here), just not of
    /// the carrier. Same liveness and role gating as `freq_error_hz`.
    pub pll_error_hz: Option<f64>,
    /// Control-channel signal-to-noise ratio in dB: this tuner's control
    /// carrier power minus the noise floor of its own capture, both measured
    /// by its own [`trunkd_p25::site::SiteEngine`]. `None` unless this tuner
    /// holds a control lock (and live, and control-following) — SNR without a
    /// signal is a number about noise.
    pub snr_db: Option<f64>,
    /// The noise floor `snr_db` was measured against, in dBFS: the 20th
    /// percentile channelizer bin. Reported alongside so an operator can tell
    /// a strong signal from a quiet band. Present exactly when `snr_db` is.
    pub noise_floor_dbfs: Option<f64>,
    /// Share of control-channel TSBKs that passed CRC over the last stats
    /// window, 0-100. `None` unless locked, and in a window that carried no
    /// TSBK at all. This is the one figure that separates "the bits are
    /// arriving damaged" from "the site is quiet".
    pub tsbk_pass_pct: Option<f64>,
    /// Control-channel framer sync losses per minute over the last stats
    /// window. `None` unless locked. Non-zero means the demodulator keeps
    /// losing its grip on a channel it is nominally holding.
    pub sync_losses_per_min: Option<f64>,
    /// Total automatic frequency correction the tuner is running, in ppm.
    /// `None` when stale or when nothing has been corrected — an operator
    /// copying this into `[[device]] ppm` wants the number that was applied,
    /// and "0" is not one.
    ///
    /// Distinct from `settings_applied.auto_ppm`, which is whether the loop is
    /// *enabled*: one is a switch the operator sets, the other is a measurement
    /// the recorder made.
    pub auto_ppm_applied: Option<f64>,
    /// What this board remembers of its own frequency error, and whether that
    /// memory was applied when the tuner was opened. `None` when the board has
    /// never been measured.
    ///
    /// Reported whether or not it was applied: with `auto_ppm` off the operator
    /// owns `ppm`, and this is the number they would type into it.
    pub ppm_memory: Option<PpmMemoryStatus>,
    /// Set while the tuner's unit is failing and being retried: what failed,
    /// and when it will be tried again. Omitted from the body otherwise.
    pub fault: Option<TunerFault>,
    /// The conventional `[[channel]]`s this tuner records, in config order.
    /// Empty on a P25-only tuner, and then omitted from the body entirely, so
    /// a recorder without channels posts exactly the body it always has.
    pub channels: Vec<ChannelStatus>,
}

/// Most conventional channels reported per tuner. Far above any capture's
/// worth of 12.5 kHz channels a recorder would configure; a cap so the body
/// stays bounded whatever the config says.
pub(crate) const MAX_CHANNELS_PER_TUNER: usize = 64;

/// Longest channel name reported, in characters.
const MAX_CHANNEL_NAME_LEN: usize = 64;

/// One conventional channel's live state, read from the `channel.{i}.*`
/// gauges its device's DSP thread publishes once a second.
///
/// Reported per tuner on the status POST and, flattened with the tuner's key,
/// as the panel's top-level `channels` list. The static half (name,
/// frequency, tone, talkgroup) comes from the config; the rest is measured.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct ChannelStatus {
    /// Position among the enabled NBFM channels: the `i` of `channel.{i}.*`.
    pub index: usize,
    pub name: String,
    pub system: String,
    pub frequency_hz: u64,
    /// Canonical tone (`"167.9"`, `"D445N"`); `None` is carrier squelch.
    pub tone: Option<String>,
    pub talkgroup: u32,
    /// `closed` | `opening` | `open` | `hang` while the tuner streams;
    /// `idle` when it does not (stopped, disabled, or its heartbeat is stale).
    pub state: &'static str,
    /// Channel power over the tracked noise floor, dB. Liveness-gated.
    pub snr_db: Option<f64>,
    /// Channel power after the channel filter, dBFS. Liveness-gated.
    pub power_dbfs: Option<f64>,
    /// The tracked noise floor, dBFS. Liveness-gated.
    pub noise_floor_dbfs: Option<f64>,
    /// Carrier offset measured during the last call, Hz; `None` before the
    /// first call of this run.
    pub offset_hz: Option<f64>,
    /// Calls kept since the tuner (re)started.
    pub calls: u64,
    /// Openings dropped as shorter than `min_call_ms`.
    pub calls_too_short: u64,
    /// Carrier openings dropped because the configured tone never arrived.
    pub tone_mismatch: u64,
    /// Carrier openings vetoed as noise (energy with no FM structure).
    pub false_opens: u64,
    /// When the last call ended, unix ms; `None` before the first.
    pub last_call_end_ms: Option<i64>,
}

impl ChannelStatus {
    /// The wire object. Unmeasured fields are omitted, as on the tuner entry.
    pub(crate) fn to_json(&self) -> Value {
        let mut obj = json!({
            "index": self.index,
            "name": truncate_chars(&self.name, MAX_CHANNEL_NAME_LEN),
            "system": truncate_chars(&self.system, MAX_CHANNEL_NAME_LEN),
            "frequency_hz": self.frequency_hz,
            "tone": self.tone,
            "talkgroup": self.talkgroup,
            "state": self.state,
            "open": matches!(self.state, "open" | "hang"),
            "calls": self.calls,
            "calls_too_short": self.calls_too_short,
            "tone_mismatch": self.tone_mismatch,
            "false_opens": self.false_opens,
        });
        let map = obj
            .as_object_mut()
            .expect("json! of an object literal is an object");
        for (k, v) in [
            ("snr_db", self.snr_db),
            ("power_dbfs", self.power_dbfs),
            ("noise_floor_dbfs", self.noise_floor_dbfs),
            ("offset_hz", self.offset_hz),
        ] {
            if let Some(v) = v {
                map.insert(k.to_owned(), json!(v));
            }
        }
        if let Some(t) = self.last_call_end_ms {
            map.insert("last_call_end_ms".to_owned(), json!(t));
        }
        obj
    }
}

/// `channel.{i}.state` (see `conventional::state_code`) as its wire name.
fn channel_state_str(code: u64) -> &'static str {
    match code {
        1 => "opening",
        2 => "open",
        3 => "hang",
        _ => "closed",
    }
}

/// Read one channel's gauges. `live` is the tuner's stream liveness: a
/// stopped tuner's channels are `idle` and report no measurement, whatever
/// the last published gauges say.
fn gather_channel_status(
    get: &dyn Fn(&str) -> Option<u64>,
    c: &ChannelDescriptor,
    live: bool,
) -> ChannelStatus {
    let p = &c.metric_prefix;
    let num = |name: &str| get(&format!("{p}.{name}")).unwrap_or(0);
    let float = |name: &str| {
        live.then(|| get(&format!("{p}.{name}")).map(f64::from_bits))
            .flatten()
    };
    let last_end = num("last_call_end_ms");
    let calls = num("calls");
    ChannelStatus {
        index: c.index,
        name: c.name.clone(),
        system: c.system.clone(),
        frequency_hz: c.frequency_hz,
        tone: c.tone.clone(),
        talkgroup: c.talkgroup,
        state: if live {
            channel_state_str(num("state"))
        } else {
            "idle"
        },
        snr_db: float("snr_db"),
        power_dbfs: float("power_dbfs"),
        noise_floor_dbfs: float("noise_floor_dbfs"),
        // A measurement of the last call, not of now: kept while the tuner is
        // stopped, absent only before anything has been heard.
        offset_hz: (calls != 0 || last_end != 0)
            .then(|| get(&format!("{p}.offset_hz")).map(f64::from_bits))
            .flatten(),
        calls,
        calls_too_short: num("calls_too_short"),
        tone_mismatch: num("tone_mismatch"),
        false_opens: num("false_opens"),
        last_call_end_ms: (last_end != 0).then_some(last_end as i64),
    }
}

/// One board's persisted frequency correction, as the status report sees it.
///
/// Read back out of the gauges the device unit publishes at open rather than
/// off disk: the status path must not do file I/O per POST, and a gauge is
/// already the recorder's answer to "what is this tuner running".
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PpmMemoryStatus {
    /// The remembered total correction in ppm, `[[device]] ppm` included.
    pub ppm: f64,
    /// When it was measured, RFC 3339 UTC.
    pub measured_at: String,
    /// Whether it was dialed into the backend when this tuner was opened.
    pub applied_at_open: bool,
}

impl PpmMemoryStatus {
    fn to_json(&self) -> Value {
        json!({
            "ppm": self.ppm,
            "measured_at": self.measured_at,
            "applied_at_open": self.applied_at_open,
        })
    }
}

/// The effective, GUI-editable settings of one tuner.
///
/// Built from the applied config, which is seeded from the TOML and layered
/// with keyup's overrides — so this is a complete description of the running
/// tuner and never a partial diff.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct AppliedSettings {
    pub gain: Option<f64>,
    pub gains: BTreeMap<String, f64>,
    pub agc: Option<bool>,
    pub ppm: f64,
    pub auto_ppm: bool,
    pub auto_ppm_limit: f64,
    pub bandwidth_hz: Option<u32>,
    pub rx_port: Option<String>,
    pub settings: BTreeMap<String, String>,
    pub lna_db: Option<u32>,
    pub vga_db: Option<u32>,
    pub amp: Option<bool>,
    pub sample_rate: u32,
    pub data_follow: bool,
    pub remote_grants: bool,
    pub control_frequencies: Vec<u64>,
}

impl AppliedSettings {
    /// Read the effective settings out of an applied config, falling back to
    /// the boot sample rate for an entry that has never carried one.
    fn from_applied(config: &AppliedConfig, toml_rate: u32) -> Self {
        AppliedSettings {
            gain: config.gain,
            gains: config.gains.clone().unwrap_or_default(),
            agc: config.agc,
            ppm: config.ppm.unwrap_or(0.0),
            auto_ppm: config.auto_ppm.unwrap_or(true),
            auto_ppm_limit: config.auto_ppm_limit.unwrap_or_default(),
            bandwidth_hz: config.bandwidth_hz,
            rx_port: config.rx_port.clone(),
            settings: config.settings.clone().unwrap_or_default(),
            lna_db: config.lna_db,
            vga_db: config.vga_db,
            amp: config.amp,
            sample_rate: config.rate_or(toml_rate),
            data_follow: config.data_follow.unwrap_or(false),
            remote_grants: config.remote_grants.unwrap_or(false),
            control_frequencies: config.control_frequencies.clone().unwrap_or_default(),
        }
    }

    /// The wire object, merged into the tuner entry at the top level so a GUI
    /// reads one flat record rather than reaching into a nested settings blob.
    fn write_into(&self, map: &mut serde_json::Map<String, Value>) {
        let mut put = |k: &str, v: Value| {
            map.insert(k.to_owned(), v);
        };
        put("gain", json!(self.gain));
        put("gains", json!(self.gains));
        put("agc", json!(self.agc));
        put("ppm", json!(self.ppm));
        put("auto_ppm", json!(self.auto_ppm));
        put("auto_ppm_limit", json!(self.auto_ppm_limit));
        put("bandwidth_hz", json!(self.bandwidth_hz));
        put("rx_port", json!(self.rx_port));
        put("settings", json!(self.settings));
        put("lna_db", json!(self.lna_db));
        put("vga_db", json!(self.vga_db));
        put("amp", json!(self.amp));
        put("sample_rate", json!(self.sample_rate));
        put("data_follow", json!(self.data_follow));
        put("remote_grants", json!(self.remote_grants));
        put("control_frequencies", json!(self.control_frequencies));
    }
}

/// What a tuner's backend can and cannot do without a restart.
///
/// Reported so a GUI can say "this takes effect now" or "this restarts the
/// tuner" *before* the operator presses apply, rather than leaving them to
/// discover it from a gap in the recording.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct TunerCapabilities {
    /// Gain / AGC change on a running stream.
    pub runtime_gain: bool,
    /// Frequency correction change on a running stream.
    pub runtime_ppm: bool,
    /// The overridable fields whose change restarts the tuner.
    pub restart_on: Vec<&'static str>,
}

impl TunerCapabilities {
    /// The capabilities of `backend`, derived from the one field table.
    pub(crate) fn for_backend(backend: Backend) -> Self {
        let runtime = runtime_fields(backend);
        TunerCapabilities {
            runtime_gain: runtime.contains(&"gain"),
            runtime_ppm: runtime.contains(&"ppm"),
            restart_on: restart_fields(backend),
        }
    }

    fn to_json(&self) -> Value {
        json!({
            "runtime_gain": self.runtime_gain,
            "runtime_ppm": self.runtime_ppm,
            "restart_on": self.restart_on,
        })
    }
}

/// One tuner's operator-entered hardware description (`[[device]]` free text).
///
/// Carried through to the status POST untouched: keyup displays it and cannot
/// override it, because only the operator knows what is on the mast.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct TunerDescription {
    pub receiver: String,
    pub antenna: String,
    pub cable: String,
    pub amplifier: String,
    pub height_m: Option<f64>,
    pub notes: String,
}

impl TunerDescription {
    /// Whether the operator described nothing — the status POST then omits the
    /// whole `description` object.
    fn is_empty(&self) -> bool {
        self == &TunerDescription::default()
    }

    /// The description as JSON, empty fields omitted; `None` when there is
    /// nothing to say.
    fn to_json(&self) -> Option<Value> {
        if self.is_empty() {
            return None;
        }
        let mut o = serde_json::Map::new();
        let mut put = |k: &str, v: &str| {
            if !v.is_empty() {
                o.insert(k.to_owned(), Value::from(v));
            }
        };
        put("receiver", &self.receiver);
        put("antenna", &self.antenna);
        put("cable", &self.cable);
        put("amplifier", &self.amplifier);
        put("notes", &self.notes);
        if let Some(h) = self.height_m {
            o.insert("height_m".to_owned(), json!(h));
        }
        Some(Value::Object(o))
    }
}

/// Recorder-wide counters reported alongside the per-tuner status.
///
/// Deliberately a handful of totals, not a metrics dump: keyup needs to see
/// that a recorder is delivering calls and not filling its disk, and anything
/// finer belongs in the local log or a future `/metrics` scrape.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Counters {
    pub calls_started: u64,
    pub calls_ended: u64,
    /// Calls that ended with too little voice to encode (`min_voice_frames`).
    pub calls_discarded_empty: u64,
    /// Unit-to-unit (private) calls started, of `calls_started`.
    pub unit_calls_started: u64,
    /// Unit-to-unit (private) calls ended, of `calls_ended`.
    pub unit_calls_ended: u64,
    /// Grants refused because the talkgroup is on the skip list: calls this
    /// recorder could have taken and deliberately did not.
    pub calls_skipped_by_policy: u64,
    /// Calls granted on a Phase 2 (TDMA) channel and not followed: traffic
    /// this Phase 1 recorder heard and cannot archive yet.
    pub calls_tdma_refused: u64,
    pub uploaded: u64,
    pub spooled: u64,
    pub failed: u64,
    pub spool_bytes: u64,
    pub disk_free_bytes: u64,
    /// Calls spooled without an upload attempt (queue full, Keyup failing).
    /// Reported in the `uplink` block, not under `counters`.
    pub spooled_on_pressure: u64,
    /// Spooled calls moved to quarantine. Reported in the `uplink` block.
    pub quarantined: u64,
    /// Keyup is failing and live calls go straight to the spool.
    pub circuit_open: bool,
}

/// Read the recorder-wide counters out of a metrics snapshot.
///
/// Delivery publishes the same figures under two prefixes depending on mode —
/// `uplink.*` when calls are POSTed, `spool.*` in `spool_only` — exactly as
/// `recorder::log_status` handles it. Which prefix exists is itself the mode
/// signal: the registry only holds metrics something registered.
pub(crate) fn gather_counters(snap: &[trunkd_core::MetricSnapshot]) -> Counters {
    let num = |name: &str| {
        snap.iter()
            .find(|m| m.name == name)
            .map(|m| m.value)
            .unwrap_or(0)
    };
    let uplink_mode = snap.iter().any(|m| m.name.starts_with("uplink."));
    let (uploaded, spooled, failed, spool_bytes, disk_free_bytes) = if uplink_mode {
        (
            // A duplicate is a call keyup already has: delivered, not lost.
            num("uplink.uploaded") + num("uplink.duplicates"),
            num("uplink.spooled"),
            num("uplink.failed"),
            num("uplink.spool_bytes"),
            num("uplink.disk_free_bytes"),
        )
    } else {
        (
            0,
            num("spool.stored"),
            num("spool.failed"),
            num("spool.bytes"),
            num("spool.disk_free_bytes"),
        )
    };
    // Uplink-only: `spool_only` never uploads, so it has no circuit and no
    // quarantine, and these read 0 there.
    Counters {
        // Call assembly is recorder-wide (one call thread serves every site),
        // so these are single counters rather than a per-site sum.
        calls_started: num("calls.started"),
        calls_ended: num("calls.ended"),
        calls_discarded_empty: num("calls.discarded_empty"),
        unit_calls_started: num("calls.unit_started"),
        unit_calls_ended: num("calls.unit_ended"),
        calls_skipped_by_policy: num("calls.skipped_by_policy"),
        calls_tdma_refused: num(crate::pipeline::device::TDMA_REFUSED),
        uploaded,
        spooled,
        failed,
        spool_bytes,
        disk_free_bytes,
        spooled_on_pressure: num("uplink.spooled_on_pressure"),
        quarantined: num("uplink.spool_quarantined"),
        circuit_open: num("uplink.circuit_open") != 0,
    }
}

/// The display name to report for a tuner whose operator left `receiver` blank:
/// what the backend implies, which beats an empty string on a dashboard.
/// `driver` is the SoapySDR module name from `[[device]]`, which is the only
/// thing that distinguishes one Soapy tuner from another: "SoapySDR" alone
/// tells a dashboard nothing, while "Airspy" is the answer the operator would
/// have typed.
pub(crate) fn default_receiver(backend: Backend, driver: Option<&str>) -> String {
    match backend {
        Backend::Hackrf => "HackRF One".to_owned(),
        Backend::Iqfile => "IQ file".to_owned(),
        Backend::Soapy => match driver.map(str::trim).filter(|d| !d.is_empty()) {
            None => "SoapySDR".to_owned(),
            Some(d) => match d.to_ascii_lowercase().as_str() {
                "airspy" => "Airspy".to_owned(),
                "sdrplay" => "SDRplay".to_owned(),
                "rtlsdr" => "RTL-SDR".to_owned(),
                other => format!("SoapySDR {other}"),
            },
        },
    }
}

/// Render the tuner-status POST body, conforming to the keyup contract and its
/// caps: `instance_id` <= 128 chars (already resolved by the caller),
/// `tuner_key` <= 64 chars, and <= 64 tuners.
#[allow(clippy::too_many_arguments)]
pub(crate) fn status_body(
    instance_id: &str,
    station: &Station,
    counters: &Counters,
    tuners: &[TunerStatus],
    uplink: &UplinkReport,
    recorder_applied: Option<RecorderApplied>,
    event_log: &EventLogReport,
) -> Value {
    let tuners: Vec<Value> = tuners
        .iter()
        .take(MAX_TUNERS)
        .map(|t| {
            let mut obj = json!({
                "tuner_key": truncate_chars(&t.tuner_key, MAX_TUNER_KEY_LEN),
                "serial": t.serial,
                "backend": t.backend,
                "center_hz": t.center_hz,
                "span_hz": t.span_hz,
                "role": t.role,
                "site": t.site,
                "site_id": t.site_id,
                "enabled": t.enabled,
                "locked": t.locked,
                "control_hz": t.control_hz,
                "nac": t.nac,
                "tsbk_rate": t.tsbk_rate,
                "pool_in_use": t.pool_in_use,
                "pool_size": t.pool_size,
                "overflow_per_s": t.overflow_per_s,
                "clock_resyncs": t.clock_resyncs,
                "last_clock_resync_ms": t.last_clock_resync_ms,
                "clock_error_ms": t.clock_error_ms,
                "clock_trim_ms": t.clock_trim_ms,
                "stream_gaps": t.stream_gaps,
                "calls_active": t.calls_active,
                "applied_version": t.applied_version,
                "spectrum": t.spectrum,
            });
            // Self-description and link quality are additive: every one of
            // these is omitted when there is nothing true to say, so a bare
            // recorder posts exactly the body it posted before this wave.
            let map = obj
                .as_object_mut()
                .expect("json! of an object literal is an object");
            // Which of the effective values above are keyup's rather than the
            // file's. Always present, empty array included: absent has to keep
            // meaning "a recorder too old to say", so that a display can mark
            // the overridden fields without ever mislabelling a tuner that is
            // running exactly what its TOML asks for.
            map.insert(
                "overrides".to_owned(),
                Value::Array(t.overrides.iter().copied().map(Value::from).collect()),
            );
            // What the file asks for where keyup has overridden the role — the
            // one field whose file value a reader cannot otherwise recover from
            // this body, and the one an operator is most often surprised by.
            if let Some(r) = t.file_role {
                map.insert("file_role".to_owned(), Value::from(r));
            }
            // The bands in force, so keyup displays what the tuner is actually
            // restricted to rather than what it last pushed. Omitted when the
            // tuner records its whole capture — an empty array and "no bands"
            // say the same thing, and absent is the cheaper of the two.
            if !t.bands.is_empty() {
                map.insert(
                    "bands".to_owned(),
                    Value::Array(
                        t.bands
                            .iter()
                            .take(MAX_BANDS)
                            .map(|b| Value::from(truncate_chars(b, MAX_BAND_LEN)))
                            .collect(),
                    ),
                );
            }
            if let Some(d) = t.description.to_json() {
                map.insert("description".to_owned(), d);
            }
            // The effective settings, flat at the top level, plus what the
            // backend can move without a restart and what the board's own gain
            // elements are: between them, everything a GUI needs to draw the
            // tuner's controls and label them honestly.
            t.settings_applied.write_into(map);
            map.insert(
                "gain_elements".to_owned(),
                Value::Array(t.gain_elements.iter().map(GainElement::to_json).collect()),
            );
            map.insert("capabilities".to_owned(), t.capabilities.to_json());
            // Rejections are omitted when there are none: the normal case is a
            // clean apply, and an empty object on every tuner of every POST
            // would say nothing at a cost.
            if !t.rejected.is_empty() {
                let mut r = serde_json::Map::new();
                for (field, reason) in &t.rejected {
                    r.insert(field.clone(), Value::from(reason.clone()));
                }
                map.insert("rejected".to_owned(), Value::Object(r));
            }
            if let Some(r) = t.rssi_dbfs {
                map.insert("rssi_dbfs".to_owned(), json!(r));
            }
            if let Some(f) = t.freq_error_hz {
                map.insert("freq_error_hz".to_owned(), json!(f));
            }
            if let Some(f) = t.pll_error_hz {
                map.insert("pll_error_hz".to_owned(), json!(f));
            }
            if let Some(s) = t.snr_db {
                map.insert("snr_db".to_owned(), json!(s));
            }
            if let Some(n) = t.noise_floor_dbfs {
                map.insert("noise_floor_dbfs".to_owned(), json!(n));
            }
            if let Some(p) = t.tsbk_pass_pct {
                map.insert("tsbk_pass_pct".to_owned(), json!(p));
            }
            if let Some(s) = t.sync_losses_per_min {
                map.insert("sync_losses_per_min".to_owned(), json!(s));
            }
            if let Some(p) = t.auto_ppm_applied {
                map.insert("auto_ppm_applied".to_owned(), json!(p));
            }
            // Always present, unlike its neighbours: `null` here is a real
            // answer ("this board has never been measured"), and a GUI drawing
            // a ppm control needs to tell that apart from a field this
            // recorder is too old to send.
            map.insert(
                "ppm_memory".to_owned(),
                t.ppm_memory
                    .as_ref()
                    .map_or(Value::Null, PpmMemoryStatus::to_json),
            );
            // A tuner that cannot open (or keeps failing) and is being
            // retried: omitted when it is healthy. `applied_version` above
            // stays at the last version that opened, so a change that has not
            // is still pending on keyup; this says why.
            if let Some(f) = &t.fault {
                map.insert("fault".to_owned(), f.to_json());
            }
            // Conventional channels on this tuner. Omitted (not an empty
            // array) on a P25-only tuner: the field is additive, and a
            // recorder with no `[[channel]]` posts the body it always has.
            if !t.channels.is_empty() {
                map.insert(
                    "channels".to_owned(),
                    Value::Array(
                        t.channels
                            .iter()
                            .take(MAX_CHANNELS_PER_TUNER)
                            .map(ChannelStatus::to_json)
                            .collect(),
                    ),
                );
            }
            obj
        })
        .collect();
    let mut body = json!({
        "instance_id": instance_id,
        // keyup's recorder_version is the recorder SOFTWARE version (a string); the
        // desired-state generation is tracked per-tuner (applied_version), not here.
        "recorder_version": env!("CARGO_PKG_VERSION"),
        "counters": {
            "calls_started": counters.calls_started,
            "calls_ended": counters.calls_ended,
            // Calls that ended carrying no voice, so nothing was encoded for
            // them. Reported so a rising number is visible as what it is — a
            // site whose traffic channels are not being heard — rather than as
            // a silent gap between `calls_ended` and `uploaded`.
            "calls_discarded_empty": counters.calls_discarded_empty,
            // The private-call share of the two above: they compete for the
            // same tuners and are the traffic this recorder used to refuse, so
            // "how much of what we record is unit-to-unit" is one subtraction
            // rather than a guess.
            "unit_calls_started": counters.unit_calls_started,
            "unit_calls_ended": counters.unit_calls_ended,
            // Grants refused outright because the operator turned the talkgroup
            // off in keyup: the traffic this recorder heard, logged and chose
            // not to record. Rising here while `calls_started` is flat is the
            // skip list working, not a recorder going deaf.
            "calls_skipped_by_policy": counters.calls_skipped_by_policy,
            // Calls granted on a Phase 2 (TDMA) channel, which this recorder
            // cannot decode and so does not follow: how much of a site's
            // traffic the archive is missing, once per call (not per grant).
            "calls_tdma_refused": counters.calls_tdma_refused,
            "uploaded": counters.uploaded,
            "spooled": counters.spooled,
            "failed": counters.failed,
            "spool_bytes": counters.spool_bytes,
            "disk_free_bytes": counters.disk_free_bytes,
        },
        "tuners": tuners,
        // Whether uploads are held, who held them, and how much is waiting.
        // keyup needs this to explain a recorder that is up, healthy and
        // delivering nothing — and to offer the release button.
        "uplink": uplink.to_json(),
        // Whether the day's event-log archives are being shipped, and how far
        // behind they are. Reported unconditionally, `archive: false` and all:
        // "the feature is off" is the answer to most of the questions this
        // block gets asked.
        "event_log": event_log.to_json(),
        // What the recorder-level settings cost to apply. Per-tuner
        // capabilities live on each tuner entry; this is the box-wide half.
        "capabilities": { "recorder": recorder_capabilities_json() },
    });
    // The echo of keyup's own recorder-level push, so it can tell a version it
    // sent from a version this recorder has acted on. Omitted entirely until
    // keyup has pushed one: absent says "nothing applied" without claiming a
    // version number nobody issued.
    if let Some(applied) = recorder_applied {
        body.as_object_mut()
            .expect("json! of an object literal is an object")
            .insert("recorder_applied".to_owned(), applied.to_json());
    }
    // An unconfigured `[station]` sends no object at all rather than seven
    // empty strings: keyup can then tell "not described" from "described as
    // nothing" without inspecting every field.
    if !station.is_empty() {
        if let Ok(v) = serde_json::to_value(station) {
            body.as_object_mut()
                .expect("json! of an object literal is an object")
                .insert("station".to_owned(), v);
        }
    }
    body
}

/// The usable span of a capture in Hz, or 0 when the rate cannot drive the
/// channelizer. keyup validates a desired center against `center_hz ± span/2`,
/// so an inaccurate span would let it place a control channel the tuner cannot
/// reach — reporting 0 is the safe answer for an unusable rate.
pub(crate) fn span_hz_for(sample_rate: u32) -> u64 {
    trunkd_p25::dsp::channelizer::usable_half_span_for(sample_rate)
        .map(|half| (half * 2.0) as u64)
        .unwrap_or(0)
}

/// A tuner whose stream heartbeat is older than this is treated as not live: its
/// site gauges are stale, so `locked`/`control_hz` must not be trusted. Two
/// status intervals of slack over the 1 s heartbeat cadence.
const STREAM_STALE_MS: i64 = 30_000;

/// Gather one tuner's reported status from a metrics snapshot and its applied
/// config. Pure (no clock, no network) given `now_ms`, so the field derivation —
/// including the liveness gate on `locked`/`control_hz` and the overflow read —
/// is unit-testable.
pub(crate) fn gather_tuner_status(
    snap: &[trunkd_core::MetricSnapshot],
    d: &TunerDescriptor,
    applied: Option<&AppliedSnapshot>,
    now_ms: i64,
) -> TunerStatus {
    let get = |name: &str| snap.iter().find(|m| m.name == name).map(|m| m.value);
    let num = |name: &str| get(name).unwrap_or(0);

    let role = applied
        .and_then(|a| a.config.role)
        .unwrap_or(DeviceRole::Auto);
    let center_hz = applied.and_then(|a| a.config.center_frequency).unwrap_or(0);
    let enabled = applied.map(|a| a.config.enabled).unwrap_or(true);
    let site = applied
        .and_then(|a| a.config.site.clone())
        .or_else(|| d.site_label.clone());
    let site_id = applied.and_then(|a| a.config.site_id.clone());
    let applied_version = applied.map(|a| a.version).unwrap_or(0);
    let spectrum = applied.map(|a| a.spectrum).unwrap_or(false);
    let bands = applied
        .and_then(|a| a.config.bands.clone())
        .unwrap_or_default();
    // Which of the reported values are keyup's rather than the file's. A tuner
    // with no registry entry has never been overridden by anything, so an empty
    // list is the truth for it too.
    let overrides = applied.map(|a| a.overrides.clone()).unwrap_or_default();
    let file_role = applied
        .filter(|_| overrides.contains(&"role"))
        .map(|a| role_str(a.file_role));
    // The effective settings and the backend's capability table. A tuner with
    // no registry entry (never seeded) reports the TOML rate and its backend's
    // capabilities, which is the truth for it too.
    let settings_applied = match applied {
        Some(a) => AppliedSettings::from_applied(&a.config, d.sample_rate),
        None => AppliedSettings {
            sample_rate: d.sample_rate,
            auto_ppm: true,
            ..AppliedSettings::default()
        },
    };
    let gain_elements = applied.map(|a| a.gain_elements.clone()).unwrap_or_default();
    let rejected = applied.map(|a| a.rejected.clone()).unwrap_or_default();
    let fault = applied.and_then(|a| a.fault.clone());
    let capabilities =
        TunerCapabilities::for_backend(applied.map(|a| a.backend).unwrap_or(d.backend));

    // Liveness: the DSP thread bumps this gauge every second while streaming. A
    // dead or idle tuner stops bumping it, so its persisted site gauges (which
    // linger in the registry after the unit exits) must not be reported as a
    // live lock.
    let beat_ms = num(&format!("device.{}.stream_beat_ms", d.id)) as i64;
    let live = beat_ms != 0 && now_ms.saturating_sub(beat_ms) < STREAM_STALE_MS;

    let (control_hz, nac, site_tsbk_rate, pool_in_use, pool_size, calls_active) =
        match &d.site_metric_prefix {
            Some(p) => (
                num(&format!("{p}.control_frequency_hz")),
                num(&format!("{p}.nac")) as u32,
                get(&format!("{p}.tsbk_rate"))
                    .map(f64::from_bits)
                    .unwrap_or(0.0),
                num(&format!("{p}.pool_in_use")),
                num(&format!("{p}.pool_size")),
                num(&format!("{p}.active_calls")),
            ),
            None => (0, 0, 0.0, 0, 0, 0),
        };

    // Signal quality is measured per *device*, because it is: two tuners on one
    // site share every `site.{index}.*` gauge and the last stats window to be
    // routed overwrites the other's. `device.{id}.site_stats_seen` says whether
    // this device's own copies have ever been written; until they have (a
    // recorder mid-startup, a tuner that has not completed a stats window) the
    // site-keyed values are still the best answer available, so nothing
    // regresses.
    let dev = |name: &str| get(&format!("device.{}.{name}", d.id));
    let dev_f64 = |name: &str| dev(name).map(f64::from_bits);
    let dev_flag = |name: &str| dev(name).is_some_and(|v| v != 0);
    let per_tuner = dev_flag("site_stats_seen");
    let tsbk_rate = if per_tuner {
        dev_f64("tsbk_rate").unwrap_or(0.0)
    } else {
        site_tsbk_rate
    };
    // Liveness-gated like every other measurement below, and for a reason paid
    // for in the field: a tuner keyup disabled at 05:28 went on reporting the
    // 25.2 TSBK/s of its last live second for the next hour, so the recorder
    // looked healthy while it decoded nothing at all. A rate is a measurement
    // over a window that has stopped; when it has, it is 0.
    let tsbk_rate = if live { tsbk_rate } else { 0.0 };

    // Overflow rate is published as a gauge by the DSP thread under the config
    // id (the ring's own counter is keyed by the backend-derived device id,
    // which the control loop does not know), so read it directly here.
    let overflow_per_s = num(&format!("device.{}.overflow_per_s", d.id));

    // Stream-clock health. Cumulative facts about this process, not a
    // measurement over a window, so unlike the gauges above they are reported
    // whether or not the tuner is live right now: "this recorder lost samples
    // and re-anchored" stays true after the tuner that did it went quiet.
    let clock_resyncs = num(&format!("device.{}.clock_resyncs", d.id));
    let last_clock_resync_ms = num(&format!("device.{}.last_clock_resync_ms", d.id)) as i64;
    let clock_error_ms = num(&format!("device.{}.clock_error_ms", d.id)) as i64;
    let clock_trim_ms = num(&format!("device.{}.clock_trim_ms", d.id)) as i64;
    let stream_gaps = num(&format!("device.{}.stream_gaps", d.id));

    // A stale/dead tuner reports no lock and a zero control frequency, whatever
    // the lingering gauges say; a live one reports its real control lock.
    let (locked, control_hz, nac) = if live {
        (enabled && control_hz != 0, control_hz, nac)
    } else {
        (false, 0, 0)
    };

    // Link quality is *measured*, so it obeys the same liveness gate as the
    // lock: a lingering gauge from a tuner that died an hour ago would read as
    // a live signal report, which is exactly the wrong thing to hand a source
    // selector. Omitted (not zeroed) when stale — 0 dBFS is a real, and very
    // loud, level.
    let rssi_dbfs = live
        .then(|| get(&format!("device.{}.rssi_dbfs", d.id)).map(f64::from_bits))
        .flatten();
    // Both frequency figures exist only for the tuner that follows control on
    // this site, and both are measurements, so both are liveness-gated.
    //
    // `freq_error_hz` is the *carrier* offset — how far off frequency the
    // control channel actually is, which is what the auto-ppm loop corrects
    // on and what an operator reaches for when a board will not lock. It is
    // absent while the site is hunting (the meter has no locked channel to
    // measure), which the companion flag says; a lingering value with the flag
    // clear would read as "measured, and spot on".
    //
    // `pll_error_hz` is the demodulator loop's phase error, which used to be
    // reported *as* `freq_error_hz` and is not a carrier measurement at all —
    // it read 422 Hz on a healthy locked tuner. Still worth reporting as a
    // diagnostic of the demodulator, under its own name.
    let control_following = matches!(role, DeviceRole::Auto | DeviceRole::Control);
    let measurable = live && control_following;
    let (freq_error_hz, pll_error_hz) = if !measurable {
        (None, None)
    } else if per_tuner {
        (
            dev_flag("freq_error_valid")
                .then(|| dev_f64("freq_error_hz"))
                .flatten(),
            dev_f64("pll_error_hz"),
        )
    } else {
        match &d.site_metric_prefix {
            Some(p) => (
                (num(&format!("{p}.control_carrier_offset_valid")) != 0)
                    .then(|| get(&format!("{p}.control_carrier_offset_hz")).map(f64::from_bits))
                    .flatten(),
                get(&format!("{p}.control_frequency_error_hz")).map(f64::from_bits),
            ),
            None => (None, None),
        }
    };
    // The four signal-quality figures have no site-keyed predecessor to fall
    // back to: they only ever existed per tuner, so a recorder whose device
    // gauges are not up yet simply omits them. Same liveness gate as
    // `rssi_dbfs` — a lingering SNR from a tuner that died an hour ago is
    // exactly the wrong thing to hand a source selector — plus each figure's
    // own validity flag, so an absent field means "not measured" and never
    // "measured as zero".
    let quality = measurable && per_tuner;
    let (snr_db, noise_floor_dbfs) = if quality && dev_flag("snr_valid") {
        (dev_f64("snr_db"), dev_f64("noise_floor_dbfs"))
    } else {
        (None, None)
    };
    let tsbk_pass_pct = (quality && dev_flag("tsbk_pass_valid"))
        .then(|| dev_f64("tsbk_pass_pct"))
        .flatten();
    // Sync losses are a rate, so zero is a real and welcome reading; it is the
    // *lock* that decides whether there is anything to report at all.
    let sync_losses_per_min = (quality && locked)
        .then(|| dev_f64("sync_losses_per_min"))
        .flatten();
    // The correction the auto-ppm loop has actually applied, gated on liveness
    // like every other measurement. Omitted at zero: an untouched tuner has no
    // correction to report, and a 0.0 on the panel reads as a measurement.
    let auto_ppm_applied = live
        .then(|| get(&format!("device.{}.auto_ppm", d.id)).map(f64::from_bits))
        .flatten()
        .filter(|p| *p != 0.0);

    // Persisted, not measured, so it is *not* liveness-gated: what the board
    // remembers is true whether or not it is streaming right now, and a tuner
    // that just died is exactly when an operator wants to see the number it
    // learned. A zero timestamp means no entry was ever published for it.
    let ppm_memory = match num(&format!("device.{}.ppm_memory_at_ms", d.id)) {
        0 => None,
        at_ms => Some(PpmMemoryStatus {
            ppm: get(&format!("device.{}.ppm_memory_ppm", d.id))
                .map(f64::from_bits)
                .unwrap_or(0.0),
            measured_at: crate::pipeline::observations::rfc3339(at_ms as i64),
            applied_at_open: num(&format!("device.{}.ppm_memory_at_open", d.id)) != 0,
        }),
    };

    // An operator who did not name the receiver gets the backend's own name
    // rather than a blank field on keyup's dashboard.
    let mut description = d.description.clone();
    if description.receiver.is_empty() {
        description.receiver = default_receiver(d.backend, d.driver.as_deref());
    }

    let channels = d
        .channels
        .iter()
        .map(|c| gather_channel_status(&get, c, live && enabled))
        .collect();

    TunerStatus {
        tuner_key: d.id.clone(),
        serial: d.serial.clone(),
        backend: backend_str(d.backend),
        center_hz,
        // The span of the rate actually in force, not the TOML's: keyup
        // validates a desired center against it, so a stale span would let it
        // place a control channel the tuner cannot reach.
        span_hz: span_hz_for(settings_applied.sample_rate),
        role: role_str(role),
        site,
        site_id,
        enabled,
        locked,
        control_hz,
        nac,
        tsbk_rate,
        pool_in_use,
        pool_size,
        overflow_per_s,
        clock_resyncs,
        last_clock_resync_ms,
        clock_error_ms,
        clock_trim_ms,
        stream_gaps,
        calls_active,
        applied_version,
        spectrum,
        overrides,
        file_role,
        bands,
        description,
        settings_applied,
        gain_elements,
        capabilities,
        rejected,
        rssi_dbfs,
        freq_error_hz,
        pll_error_hz,
        snr_db,
        noise_floor_dbfs,
        tsbk_pass_pct,
        sync_losses_per_min,
        auto_ppm_applied,
        ppm_memory,
        fault,
        channels,
    }
}

// ---------------------------------------------------------------------------
// The thread
// ---------------------------------------------------------------------------

/// Static per-tuner facts, resolved once at startup. The mutable parts (center,
/// role, enabled, applied version, spectrum) come from [`DesiredState`].
#[derive(Clone)]
pub(crate) struct TunerDescriptor {
    pub id: String,
    pub serial: Option<String>,
    pub backend: Backend,
    /// SoapySDR driver module name (`soapy` backend only), used to report a
    /// hardware name for a tuner whose `receiver` the operator left blank.
    pub driver: Option<String>,
    pub sample_rate: u32,
    /// Label of the site this tuner reports for, if any.
    pub site_label: Option<String>,
    /// Metric prefix of that site (`site.N`), for the per-site gauges.
    pub site_metric_prefix: Option<String>,
    /// Operator-entered hardware description from `[[device]]`.
    pub description: TunerDescription,
    /// The enabled conventional channels this tuner records. Empty for a
    /// P25-only tuner.
    pub channels: Vec<ChannelDescriptor>,
}

/// Static facts of one conventional channel, for the status reports: what
/// [`ChannelSpec`](crate::pipeline::conventional::ChannelSpec) knows, minus the
/// engine settings.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ChannelDescriptor {
    pub index: usize,
    pub name: String,
    pub system: String,
    pub frequency_hz: u64,
    pub tone: Option<String>,
    pub talkgroup: u32,
    /// `channel.{index}`.
    pub metric_prefix: String,
}

impl From<&crate::pipeline::conventional::ChannelSpec> for ChannelDescriptor {
    fn from(s: &crate::pipeline::conventional::ChannelSpec) -> Self {
        ChannelDescriptor {
            index: s.index,
            name: s.name.clone(),
            system: s.system_label.clone(),
            frequency_hz: s.frequency_hz,
            tone: s.tone.clone(),
            talkgroup: s.talkgroup,
            metric_prefix: s.metric_prefix.clone(),
        }
    }
}

/// How the control thread reaches keyup.
#[derive(Clone)]
pub(crate) struct ControlConfig {
    /// Desired-state endpoint. Empty disables the GET.
    pub desired_url: String,
    /// Tuner-status endpoint. Empty disables the POST.
    pub status_url: String,
    /// Spectrum-frame endpoint (W4). Empty disables the POST (frames still get
    /// drained so the channel never backs up).
    pub spectrum_url: String,
    /// Bearer token (the upload token).
    pub api_key: String,
    /// This instance's stable id (already resolved and capped).
    pub instance_id: String,
    /// Operator-entered `[station]` facts, reported verbatim on every status
    /// POST (already length-capped at config parse).
    pub station: Station,
    /// Desired-state poll cadence.
    pub desired_interval: Duration,
    /// Status report cadence.
    pub status_interval: Duration,
    /// The recorder-wide control block: the uplink hold and the desired-state
    /// version it was last set from.
    pub recorder: Arc<RecorderControl>,
    /// The call-upload endpoint, reported verbatim in the `uplink` object so an
    /// operator can see where a released hold would send calls.
    pub upload_url: String,
    /// The recorder link slot. Only its status JSON is used here — the control
    /// loop neither sends nor receives on it, and a keyup link push is applied
    /// through [`RecorderControl`], which holds the same slot.
    pub link: Arc<LinkSlot>,
    /// The event-log archive's live state, reported in the `event_log` object.
    pub event_log: Arc<crate::pipeline::eventarchive::ArchiveState>,
    /// The per-board ppm memory file, so an explicit `ppm` push can overwrite
    /// what the recorder measured for itself. See [`reset_ppm_memory`].
    pub ppm_memory_path: std::path::PathBuf,
    /// Set by the push-channel thread to mean "fetch the desired state now".
    ///
    /// The loop already wakes every 100 ms, so an atomic flag it swaps on each
    /// pass is the whole mechanism: no condvar, no channel, and a keyup change
    /// takes effect within a tenth of a second instead of waiting out the poll
    /// interval. Taking it is a `swap(false)`, so two notifications that arrive
    /// inside one tick coalesce into one GET — which is fine, because the GET
    /// always returns the newest document either way.
    pub fetch_now: Arc<AtomicBool>,
    /// The push channel's live state, reported in the `uplink.stream` object.
    pub stream: Arc<crate::pipeline::stream::StreamState>,
    /// The control panel, when this recorder serves one, so Keyup's status
    /// page can offer a "listen live" link. `None` with `[server] enabled`
    /// off, and then no `monitor` object is posted at all.
    pub panel: Option<PanelLink>,
}

/// Where this recorder's panel and live monitor are, for the `monitor` object
/// on the status POST.
#[derive(Clone)]
pub(crate) struct PanelLink {
    /// Panel base URL, no trailing slash — `[server] public_url`, or derived
    /// from the bind address.
    pub base_url: String,
    /// `[server] live_audio`: whether the stream route exists at all.
    pub live_audio: bool,
    /// Whether `[server] tokens` is non-empty, so Keyup can say the link needs
    /// one rather than offering a click that 401s.
    pub token_required: bool,
    /// The fan-out hub, for the open-stream count. `None` when live audio is
    /// off (the hub is not built).
    pub hub: Option<Arc<crate::pipeline::monitor::MonitorHub>>,
}

/// Add the `monitor` block to a status body.
///
/// Omitted entirely when there is no panel: absent means "this recorder serves
/// no page", which is a different thing from a page with the live tap off.
pub(crate) fn insert_monitor(body: &mut Value, panel: Option<&PanelLink>) {
    let (Some(map), Some(panel)) = (body.as_object_mut(), panel) else {
        return;
    };
    map.insert(
        "monitor".to_owned(),
        json!({
            "enabled": panel.live_audio,
            "url": panel.base_url,
            "stream_url": format!("{}/api/monitor/stream", panel.base_url),
            "token_required": panel.token_required,
            "streams": panel.hub.as_ref().map_or(0, |h| h.streams()),
            "max_streams": crate::pipeline::monitor::MAX_STREAMS,
        }),
    );
}

/// Add the `link` block to a status body.
///
/// A free function so the panel and the keyup POST cannot drift apart, and so
/// [`status_body`] itself keeps the signature every existing caller uses.
pub(crate) fn insert_link(body: &mut Value, link: &LinkSlot) {
    if let Some(map) = body.as_object_mut() {
        map.insert("link".to_owned(), link.status_json());
    }
}

/// Run the control loop until `stop` is set.
pub(crate) fn run(
    shared: Shared,
    descriptors: Vec<TunerDescriptor>,
    desired: Arc<DesiredState>,
    cfg: ControlConfig,
    spectrum_rx: BoundedReceiver<SpectrumFrame>,
    stop: Arc<AtomicBool>,
) {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(HTTP_TIMEOUT))
        .build()
        .into();
    // Its own agent (and connection pool) so its shorter limit applies only
    // to spectrum.
    let spectrum_agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(SPECTRUM_TIMEOUT))
        .build()
        .into();

    let applied = shared.metrics.counter("control.applied");
    let posted = shared.metrics.counter("control.status_posted");
    let failed = shared.metrics.counter("control.status_failed");
    let spectrum_posted = shared.metrics.counter("control.spectrum_posted");
    let spectrum_failed = shared.metrics.counter("control.spectrum_failed");
    let notified = shared.metrics.counter("control.stream_notified");
    // Keyup being down is logged when it starts, every ten minutes while it
    // lasts, and when it ends — not on every 10 s / 15 s attempt.
    let mut desired_failures = FailureRun::default();
    let mut status_failures = FailureRun::default();

    // Fire both jobs promptly on startup rather than after a full interval.
    let mut next_desired = Instant::now();
    let mut next_status = Instant::now();

    while !stop.load(Ordering::Relaxed) {
        let now = Instant::now();
        // A push from keyup pulls the next GET forward; the interval is then
        // measured from now, so a burst of notifications cannot turn into a
        // burst of polls once the pushes stop.
        let pushed = cfg.fetch_now.swap(false, Ordering::Relaxed);
        if pushed {
            notified.inc();
        }
        let do_desired = pushed || now >= next_desired;
        let do_status = now >= next_status;
        if do_desired {
            next_desired = now + cfg.desired_interval;
        }
        if do_status {
            next_status = now + cfg.status_interval;
        }
        // A per-iteration panic must not silently end the control loop for the
        // life of the process: catch it, log loudly, and carry on next tick.
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            if do_desired {
                poll_desired(
                    &agent,
                    &cfg,
                    &descriptors,
                    &desired,
                    &applied,
                    &mut desired_failures,
                );
            }
            if do_status {
                report_status(
                    &agent,
                    &cfg,
                    &shared,
                    &descriptors,
                    &desired,
                    &posted,
                    &failed,
                    &mut status_failures,
                );
            }
            // Spectrum frames arrive from the DSP threads at a few Hz; drain and
            // POST them every iteration so latency stays low. Ephemeral: a
            // failed POST is dropped, never spooled.
            post_spectrum(
                &spectrum_agent,
                &cfg,
                &spectrum_rx,
                &spectrum_posted,
                &spectrum_failed,
            );
        }));
        if outcome.is_err() {
            tracing::error!("control-loop iteration panicked; continuing next cycle");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Why one desired-state GET produced no body.
enum FetchError {
    /// The route answered 404: a keyup that predates it.
    NotDeployed,
    /// Anything else — connect refused, timeout, 5xx, unreadable body.
    Failed(String),
}

/// GET the desired-state document for this instance and return its body.
///
/// The one place the request is built, so the startup prefetch and the poll
/// loop cannot drift apart on the URL, the query parameter or the auth header.
fn fetch_desired(
    agent: &ureq::Agent,
    base_url: &str,
    api_key: &str,
    instance_id: &str,
) -> Result<String, FetchError> {
    let url = format!("{}?instance={}", base_url, percent_encode(instance_id));
    match agent
        .get(&url)
        .header("Authorization", &format!("Bearer {api_key}"))
        .call()
    {
        Ok(mut resp) => Ok(resp.body_mut().read_to_string().unwrap_or_default()),
        Err(ureq::Error::StatusCode(404)) => Err(FetchError::NotDeployed),
        Err(e) => Err(FetchError::Failed(e.to_string())),
    }
}

// ---------------------------------------------------------------------------
// Startup prefetch
// ---------------------------------------------------------------------------

/// What the startup prefetch needs to reach keyup, exactly once.
pub(crate) struct PrefetchConfig {
    /// Desired-state endpoint. Empty skips the prefetch.
    pub desired_url: String,
    /// Bearer token (the upload token).
    pub api_key: String,
    /// This instance's stable id (already resolved and capped).
    pub instance_id: String,
    /// Ceiling on the whole call, connect through response. Zero skips the
    /// prefetch.
    pub timeout: Duration,
    /// The per-board ppm memory file, so a prefetched explicit `ppm` overwrites
    /// what the recorder measured for itself — the same thing the poll loop
    /// does when that push arrives a few hundred ms later.
    pub ppm_memory_path: std::path::PathBuf,
}

/// Fetch keyup's desired state **once, synchronously, before the first tuner is
/// built**, and seed the registry with it.
///
/// Without this the tuners come up on the TOML, the poll loop's first pass a
/// few hundred ms later finds any field keyup manages differing from it, and
/// the tuner restarts about a second after its first lock — a recording gap at
/// every process start. Seeding first makes the initial build the merged
/// config, so the later poll sees the version it already applied and does
/// nothing.
///
/// Never fatal and never slow: a keyup that is down, slow, or predates the
/// route leaves the registry exactly as the TOML seeded it, at a cost of at
/// most `timeout`, and the normal poll loop then applies changes as it always
/// has.
pub(crate) fn prefetch_desired(
    cfg: &PrefetchConfig,
    descriptors: &[TunerDescriptor],
    desired: &Arc<DesiredState>,
    recorder: &RecorderControl,
) {
    if cfg.desired_url.is_empty() || cfg.timeout.is_zero() {
        tracing::debug!("desired-state prefetch skipped: not configured");
        return;
    }
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(cfg.timeout))
        .build()
        .into();
    let body = match fetch_desired(&agent, &cfg.desired_url, &cfg.api_key, &cfg.instance_id) {
        Ok(body) => body,
        Err(FetchError::NotDeployed) => {
            tracing::info!(
                "desired-state prefetch skipped: endpoint not deployed (404);                  starting from config"
            );
            return;
        }
        Err(e) => {
            let reason = match e {
                FetchError::Failed(e) => e,
                FetchError::NotDeployed => unreachable!(),
            };
            tracing::info!(
                reason = %reason,
                "desired-state prefetch failed; starting from config"
            );
            return;
        }
    };

    let snapshot = parse_desired(&body);
    recorder.note_desired_seen(snapshot.version);
    // Recorder-wide first, exactly as the poll loop orders it: the hold is
    // independent of any tuner and should be in effect before the first call
    // can be recorded.
    if let Some(r) = &snapshot.recorder {
        log_recorder_rejections(recorder, r);
        if recorder.apply_desired(r) {
            tracing::info!(
                version = r.version,
                spool_only = recorder.hold().is_held(),
                event_log_archive = ?r.event_log_archive,
                link = r.link.is_some(),
                "desired-state prefetched: recorder-wide settings"
            );
        }
    }
    let known: std::collections::HashSet<&str> =
        descriptors.iter().map(|d| d.id.as_str()).collect();
    for refusal in &snapshot.refused {
        desired.refuse_entry(refusal);
    }
    let mut seeded = 0usize;
    for (key, tuner) in &snapshot.tuners {
        if !known.contains(key.as_str()) {
            tracing::debug!(tuner = %key, "desired-state names an unknown tuner; ignoring");
            continue;
        }
        let Some(fields) = desired.seed_applied(key, tuner) else {
            continue;
        };
        seeded += 1;
        tracing::info!(
            tuner = %key,
            version = tuner.version,
            fields = ?fields,
            "desired-state prefetched: the tuner is built from keyup's config"
        );
        // The same ppm-memory overwrite the poll loop performs for a push that
        // lands, done here so the first open already uses the operator's number.
        if let Some(ppm) = tuner.ppm {
            if let Some(d) = descriptors.iter().find(|d| d.id == *key) {
                reset_ppm_memory(&cfg.ppm_memory_path, d, ppm);
            }
        }
    }
    if seeded == 0 && snapshot.recorder.is_none() {
        tracing::info!("desired-state prefetch: keyup has nothing for this recorder");
    }
}

/// Say why each refused recorder-level field was refused. One line per field,
/// at warn the first time a version refuses it: an operator who set something
/// in the keyup GUI that this recorder will not take must be able to find out
/// why from the box's own log — once, not on every poll that brings the same
/// block back. The reasons ride the status POST (`recorder_applied.rejected`)
/// until a newer version replaces them.
fn log_recorder_rejections(recorder: &RecorderControl, r: &DesiredRecorder) {
    let mut logged = recorder
        .rejections_logged
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    for (field, reason) in &r.rejected {
        warn_first!(
            logged.first(r.version, field),
            field = %field,
            reason = %reason,
            version = r.version,
            "recorder-level desired-state field rejected; the rest of the block still applies"
        );
    }
}

/// GET the desired state and apply each tuner's config.
fn poll_desired(
    agent: &ureq::Agent,
    cfg: &ControlConfig,
    descriptors: &[TunerDescriptor],
    desired: &Arc<DesiredState>,
    applied: &trunkd_core::Metric,
    failures: &mut FailureRun,
) {
    if cfg.desired_url.is_empty() {
        return;
    }
    let body = match fetch_desired(agent, &cfg.desired_url, &cfg.api_key, &cfg.instance_id) {
        Ok(body) => {
            if let Some((attempts, lasted)) = failures.succeeded(Instant::now()) {
                tracing::info!(
                    failed_attempts = attempts,
                    lasted_s = lasted.as_secs(),
                    "desired-state GET working again"
                );
            }
            body
        }
        // keyup grows this route in a parallel branch; a 404 is expected until
        // it lands and not worth a warning.
        Err(FetchError::NotDeployed) => {
            tracing::debug!("desired-state endpoint not deployed yet (404)");
            return;
        }
        Err(FetchError::Failed(e)) => {
            if let Some(attempts) = failures.failed(Instant::now()) {
                tracing::warn!(
                    error = %e,
                    attempts,
                    "desired-state GET failed; keeping last-applied state"
                );
            } else {
                tracing::debug!(error = %e, "desired-state GET still failing");
            }
            return;
        }
    };

    let snapshot = parse_desired(&body);
    // The high-water mark the push channel reports as `desired_seen`. Recorded
    // before the apply decisions: "we have this document" is true whatever each
    // tuner's diff turns out to be, and keyup uses it only to decide whether to
    // nudge us on the next connect.
    cfg.recorder.note_desired_seen(snapshot.version);
    // Recorder-wide first: it is independent of any tuner, and a hold release
    // should not wait behind a tuner restart decision.
    if let Some(r) = &snapshot.recorder {
        log_recorder_rejections(&cfg.recorder, r);
        // The revision-gated sub-blocks (Broadcastify, skip list) log their
        // own apply at info; the versioned recorder-wide block warns from
        // inside `apply_desired` only when it actually moved. Keyup's
        // Broadcastify talkgroup list grows every time a new talkgroup is
        // discovered, and that used to surface here as a "recorder-wide
        // settings changed" warning at an unchanged version, 25 times a day.
        if cfg.recorder.apply_desired(r) {
            applied.inc();
            tracing::debug!(version = r.version, "desired-state recorder entry applied");
        }
    }
    let known: std::collections::HashSet<&str> =
        descriptors.iter().map(|d| d.id.as_str()).collect();
    for refusal in &snapshot.refused {
        desired.refuse_entry(refusal);
    }
    for (key, tuner) in &snapshot.tuners {
        if !known.contains(key.as_str()) {
            tracing::debug!(tuner = %key, "desired-state names an unknown tuner; ignoring");
            continue;
        }
        let decision = desired.apply_desired(key, tuner);
        match decision {
            ApplyDecision::Restart => {
                applied.inc();
                tracing::info!(
                    tuner = %key,
                    version = tuner.version,
                    center_hz = tuner.center_frequency.unwrap_or(0),
                    role = tuner.role.map(role_str).unwrap_or("(unchanged)"),
                    bands = ?tuner.bands,
                    enabled = tuner.enabled,
                    "applying desired-state by restarting the tuner"
                );
            }
            ApplyDecision::Runtime => {
                applied.inc();
                tracing::info!(
                    tuner = %key,
                    version = tuner.version,
                    gain = ?tuner.gain,
                    agc = ?tuner.agc,
                    ppm = ?tuner.ppm,
                    "applying desired-state on the running tuner (no restart)"
                );
            }
            ApplyDecision::Recorded => {
                // `apply_desired` logged which echo-only fields moved.
                applied.inc();
            }
            ApplyDecision::VersionOnly => {
                tracing::debug!(tuner = %key, version = tuner.version, "desired-state version bump; config unchanged");
            }
            ApplyDecision::NoOp => {}
        }
        // An operator's number wins over the recorder's history. Only on a
        // push that actually landed, and only when it named `ppm` — a gain
        // push that happens to carry no ppm must not wipe a board's memory.
        if !matches!(decision, ApplyDecision::NoOp) {
            if let Some(ppm) = tuner.ppm {
                if let Some(d) = descriptors.iter().find(|d| d.id == *key) {
                    reset_ppm_memory(&cfg.ppm_memory_path, d, ppm);
                }
            }
        }
    }
}

/// Adopt an operator's `ppm` as this board's memory, discarding the measurement
/// history behind whatever was there.
///
/// The stored total *is* the pushed `ppm`, with `config_ppm` equal to it, so
/// the remembered delta is zero: the next open applies the operator's number
/// and nothing else, and the loop starts hunting from there rather than from a
/// correction measured against a value the operator has just overruled.
fn reset_ppm_memory(path: &std::path::Path, d: &TunerDescriptor, ppm: f64) {
    // An IQ file has no board to remember; see `resolve_ppm_memory`.
    if d.backend == Backend::Iqfile {
        return;
    }
    let board = crate::pipeline::ppmmemory::board_key(
        d.backend,
        d.driver.as_deref(),
        d.serial.as_deref(),
        &d.id,
    );
    tracing::info!(
        tuner = %d.id,
        board = %board,
        ppm,
        "operator set ppm explicitly; the board's remembered correction is reset to it"
    );
    crate::pipeline::ppmmemory::record(
        path,
        &board,
        ppm,
        ppm,
        crate::pipeline::ppmmemory::Samples::Reset,
    );
}

/// Build and POST the status report.
#[allow(clippy::too_many_arguments)]
fn report_status(
    agent: &ureq::Agent,
    cfg: &ControlConfig,
    shared: &Shared,
    descriptors: &[TunerDescriptor],
    desired: &Arc<DesiredState>,
    posted: &trunkd_core::Metric,
    failed: &trunkd_core::Metric,
    failures: &mut FailureRun,
) {
    // Offline (`enabled = false`, or no url at all — never merely held), so do
    // not even snapshot the registry.
    if cfg.status_url.is_empty() {
        tracing::debug!("tuner-status endpoint disabled; not posting");
        return;
    }

    let snap = shared.metrics.snapshot();
    let now_ms = trunkd_core::Timestamp::now().unix_millis();
    let tuners: Vec<TunerStatus> = descriptors
        .iter()
        .take(MAX_TUNERS)
        .map(|d| gather_tuner_status(&snap, d, desired.applied(&d.id).as_ref(), now_ms))
        .collect();
    let counters = gather_counters(&snap);
    let uplink = uplink_report(&counters, &cfg.recorder, &cfg.upload_url, &cfg.stream);

    let mut body_json = status_body(
        &cfg.instance_id,
        &cfg.station,
        &counters,
        &tuners,
        &uplink,
        cfg.recorder.applied(),
        &cfg.event_log.report(),
    );
    // The link block, additive: a recorder with no link still posts exactly
    // what it posted before the link existed, minus one `"enabled": false`.
    insert_link(&mut body_json, &cfg.link);
    // Where to listen, for the link Keyup puts on its status page. Additive in
    // the same way: a recorder with no panel posts the body it always did.
    insert_monitor(&mut body_json, cfg.panel.as_ref());
    let body = body_json.to_string();
    match agent
        .post(&cfg.status_url)
        .header("Authorization", &format!("Bearer {}", cfg.api_key))
        .header("Content-Type", "application/json")
        .send(body)
    {
        Ok(mut resp) => {
            posted.inc();
            let status = resp.status().as_u16();
            // Drained for the same reason as the spectrum reply: an unread
            // body is a socket closed mid-write on the proxy's side.
            let _ = resp.body_mut().read_to_string();
            tracing::debug!(status, tuners = tuners.len(), "tuner status posted");
            if let Some((attempts, lasted)) = failures.succeeded(Instant::now()) {
                tracing::info!(
                    failed_attempts = attempts,
                    lasted_s = lasted.as_secs(),
                    "tuner-status POST working again"
                );
            }
        }
        Err(ureq::Error::StatusCode(404)) => {
            failed.inc();
            tracing::debug!("tuner-status endpoint not deployed yet (404)");
        }
        Err(e) => {
            failed.inc();
            if let Some(attempts) = failures.failed(Instant::now()) {
                tracing::warn!(error = %e, attempts, "tuner-status POST failed; retrying next cycle");
            } else {
                tracing::debug!(error = %e, "tuner-status POST still failing");
            }
        }
    }
}

/// Drain the spectrum channel and POST what it holds (W4).
///
/// Coalesces to the newest frame per tuner before posting: spectrum is a live
/// display, not a queue, so a slightly stale frame in the same drain window is
/// worthless. A failed POST is dropped — never spooled, never retried. When the
/// endpoint is disabled (offline) the channel is still drained so the
/// drop-oldest sender never backs up.
fn post_spectrum(
    agent: &ureq::Agent,
    cfg: &ControlConfig,
    rx: &BoundedReceiver<SpectrumFrame>,
    posted: &trunkd_core::Metric,
    failed: &trunkd_core::Metric,
) {
    if cfg.spectrum_url.is_empty() {
        while rx.try_recv().is_ok() {}
        return;
    }
    // Newest frame per tuner (spectrum is ephemeral); bounds POSTs per cycle to
    // the tuner count no matter how many frames queued.
    let mut latest: HashMap<String, SpectrumFrame> = HashMap::new();
    while let Ok(frame) = rx.try_recv() {
        latest.insert(frame.tuner_key.clone(), frame);
    }
    for frame in latest.into_values() {
        let body = spectrum_body(&cfg.instance_id, &frame).to_string();
        match agent
            .post(&cfg.spectrum_url)
            .header("Authorization", &format!("Bearer {}", cfg.api_key))
            .header("Content-Type", "application/json")
            .send(body)
        {
            Ok(mut resp) => {
                // Drain the (tiny) reply. Dropping it unread closes the socket
                // before the server has finished writing, which keyup's edge
                // proxy logs as "aborting with incomplete response" — 2,300
                // warnings a day for a request that succeeded.
                let _ = resp.body_mut().read_to_string();
                posted.inc();
            }
            // keyup grows this route in a parallel branch; a 404 is expected
            // until it lands and not worth a warning.
            Err(ureq::Error::StatusCode(404)) => {
                failed.inc();
                tracing::debug!("spectrum endpoint not deployed yet (404)");
            }
            Err(e) => {
                failed.inc();
                tracing::debug!(error = %e, "spectrum POST failed; dropping the frame");
            }
        }
    }
}

/// Percent-encode everything but the RFC 3986 unreserved set, so an instance id
/// with a space or a slash still forms a valid query.
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A registry seeded from `devices` alone, which is what most of these
    /// tests are about; the site-scoped fields then seed as absent.
    fn state_for(devices: &[Device]) -> Arc<DesiredState> {
        DesiredState::from_config(&Config {
            device: devices.to_vec(),
            ..Config::default()
        })
    }

    fn dev(id: &str, role: DeviceRole, center: u64) -> Device {
        toml::from_str(&format!(
            "id = \"{id}\"\nbackend = \"iqfile\"\npath = \"/dev/null\"\n\
             sample_rate = 20000000\ncenter_frequency = {center}\nrole = \"{}\"\n",
            role_str(role)
        ))
        .expect("device")
    }

    /// The same, with a TOML `bands` list.
    fn dev_with_bands(id: &str, center: u64, bands: &[&str]) -> Device {
        let list = bands
            .iter()
            .map(|b| format!("\"{b}\""))
            .collect::<Vec<_>>()
            .join(", ");
        toml::from_str(&format!(
            "id = \"{id}\"\nbackend = \"iqfile\"\npath = \"/dev/null\"\n\
             sample_rate = 20000000\ncenter_frequency = {center}\nrole = \"voice\"\n\
             bands = [{list}]\n"
        ))
        .expect("device")
    }

    // -- URL derivation -----------------------------------------------------

    #[test]
    fn the_control_endpoints_replace_the_upload_route() {
        assert_eq!(
            desired_state_url("http://192.0.2.60/api/call-upload"),
            "http://192.0.2.60/api/recorder/desired-state"
        );
        assert_eq!(
            tuner_status_url("http://192.0.2.60/api/call-upload/"),
            "http://192.0.2.60/api/recorder/tuners/status"
        );
        assert_eq!(
            desired_state_url("http://192.0.2.60"),
            "http://192.0.2.60/api/recorder/desired-state"
        );
        assert_eq!(
            spectrum_url("http://192.0.2.60/api/call-upload"),
            "http://192.0.2.60/api/recorder/spectrum"
        );
        assert_eq!(
            spectrum_url("http://192.0.2.60"),
            "http://192.0.2.60/api/recorder/spectrum"
        );
    }

    #[test]
    fn a_desired_spectrum_toggle_flips_the_live_flag_without_a_restart() {
        let devices = vec![dev("d0", DeviceRole::Auto, 851_000_000)];
        let state = state_for(&devices);
        let flag = state.spectrum_flag("d0");
        assert!(!flag.load(Ordering::Relaxed), "spectrum off at boot");

        // Same config, newer version, spectrum on -> VersionOnly (no restart)
        // but the live flag flips.
        let on = DesiredTuner {
            version: 1,
            role: Some(DeviceRole::Auto),
            site: None,
            site_id: None,
            center_frequency: Some(851_000_000),
            bands: None,
            enabled: true,
            spectrum: true,
            ..DesiredTuner::default()
        };
        assert_eq!(state.apply_desired("d0", &on), ApplyDecision::VersionOnly);
        assert_eq!(state.generation_of("d0"), 0, "spectrum never restarts");
        assert!(flag.load(Ordering::Relaxed), "flag flipped on");

        // And back off on the next version.
        let off = DesiredTuner {
            version: 2,
            spectrum: false,
            ..on
        };
        assert_eq!(state.apply_desired("d0", &off), ApplyDecision::VersionOnly);
        assert!(!flag.load(Ordering::Relaxed), "flag flipped back off");
        assert_eq!(state.generation_of("d0"), 0, "still no restart");
    }

    #[test]
    fn an_unknown_tuner_gets_an_inert_spectrum_flag() {
        let devices = vec![dev("d0", DeviceRole::Auto, 851_000_000)];
        let state = state_for(&devices);
        assert!(!state.spectrum_flag("ghost").load(Ordering::Relaxed));
    }

    // -- decision logic (table-driven) --------------------------------------

    #[test]
    fn version_and_diff_decisions() {
        let base = AppliedConfig {
            role: Some(DeviceRole::Auto),
            site: None,
            site_id: None,
            center_frequency: Some(851_000_000),
            bands: None,
            enabled: true,
            ..AppliedConfig::default()
        };
        let changed_center = AppliedConfig {
            center_frequency: Some(852_000_000),
            ..base.clone()
        };
        let toggled = AppliedConfig {
            enabled: false,
            ..base.clone()
        };
        struct Case {
            name: &'static str,
            applied_version: i64,
            applied: AppliedConfig,
            desired_version: i64,
            merged: AppliedConfig,
            want: ApplyDecision,
        }
        let cases = [
            Case {
                name: "newer version, changed config -> restart",
                applied_version: 1,
                applied: base.clone(),
                desired_version: 2,
                merged: changed_center.clone(),
                want: ApplyDecision::Restart,
            },
            Case {
                name: "same version -> no-op",
                applied_version: 2,
                applied: base.clone(),
                desired_version: 2,
                merged: changed_center.clone(),
                want: ApplyDecision::NoOp,
            },
            Case {
                name: "older version -> no-op",
                applied_version: 5,
                applied: base.clone(),
                desired_version: 3,
                merged: changed_center.clone(),
                want: ApplyDecision::NoOp,
            },
            Case {
                name: "newer version, identical config -> version only",
                applied_version: 1,
                applied: base.clone(),
                desired_version: 2,
                merged: base.clone(),
                want: ApplyDecision::VersionOnly,
            },
            Case {
                name: "newer version, only site_id moved -> recorded",
                applied_version: 1,
                applied: base.clone(),
                desired_version: 2,
                merged: AppliedConfig {
                    site_id: Some("7".into()),
                    ..base.clone()
                },
                want: ApplyDecision::Recorded,
            },
            Case {
                name: "site_id alongside a real change -> restart",
                applied_version: 1,
                applied: base.clone(),
                desired_version: 2,
                merged: AppliedConfig {
                    site_id: Some("7".into()),
                    ..changed_center.clone()
                },
                want: ApplyDecision::Restart,
            },
            Case {
                name: "enabled toggle, newer version -> restart",
                applied_version: 1,
                applied: base.clone(),
                desired_version: 2,
                merged: toggled.clone(),
                want: ApplyDecision::Restart,
            },
        ];
        for c in cases {
            assert_eq!(
                decide(
                    c.applied_version,
                    &c.applied,
                    c.desired_version,
                    &c.merged,
                    Backend::Iqfile,
                ),
                c.want,
                "{}",
                c.name
            );
        }
    }

    // -- registry apply/restart flow ----------------------------------------

    fn desired(version: i64, center: Option<u64>, enabled: bool) -> DesiredTuner {
        DesiredTuner {
            version,
            role: None,
            site: None,
            site_id: None,
            center_frequency: center,
            bands: None,
            enabled,
            spectrum: false,
            ..DesiredTuner::default()
        }
    }

    /// The live sequence of 2026-09-04, both ways round: keyup pushed
    /// `enabled: false` (version 5), the tuner was rebuilt idle and stopped
    /// decoding — and with it stopped feeding the event log and the
    /// observations uplink, because there was nothing left to feed them. A
    /// later push that switches it back on must restart it into a full,
    /// decoding build with no process restart in between.
    #[test]
    fn a_tuner_disabled_by_keyup_comes_straight_back_when_it_is_re_enabled() {
        let state = state_for(&[dev("hackrf0", DeviceRole::Auto, 855_500_000)]);
        assert!(
            state.take_for_build("hackrf0").config.enabled,
            "a tuner with nothing pushed at it is on"
        );

        // Version 5: off. The decision is a restart, and the build the factory
        // is about to do carries `enabled: false` — which is what idles it.
        let off = desired(5, Some(855_500_000), false);
        assert_eq!(state.apply_desired("hackrf0", &off), ApplyDecision::Restart);
        let built = state.take_for_build("hackrf0");
        assert!(!built.config.enabled);
        assert_eq!(built.version, 5);
        // The idle unit confirms it settled into the state it was asked for,
        // exactly as `DeviceUnit::run` does before idling.
        state.confirm_applied("hackrf0", &built.config, built.version);
        assert!(!state.applied("hackrf0").expect("applied").config.enabled);

        // Version 6: on again. Another restart, and this build streams.
        let on = desired(6, Some(855_500_000), true);
        assert_eq!(state.apply_desired("hackrf0", &on), ApplyDecision::Restart);
        let built = state.take_for_build("hackrf0");
        assert!(built.config.enabled, "re-enabling needs no process restart");
        assert_eq!(built.version, 6);
        // The rest of the config survived the round trip untouched.
        assert_eq!(built.config.center_frequency, Some(855_500_000));
        state.confirm_applied("hackrf0", &built.config, built.version);
        assert!(state.applied("hackrf0").expect("applied").config.enabled);
    }

    #[test]
    fn a_change_is_only_confirmed_after_the_build_reports_it_running() {
        let devices = vec![dev("d0", DeviceRole::Auto, 851_000_000)];
        let state = state_for(&devices);

        // At boot: generation 0, applied version 0, TOML center in effect.
        let built = state.take_for_build("d0");
        assert_eq!(built.generation, 0);
        assert_eq!(state.applied("d0").unwrap().version, 0);
        assert_eq!(built.config.center_frequency, Some(851_000_000));

        // A newer version with a changed center restarts.
        let d = desired(4, Some(852_000_000), true);
        assert_eq!(state.apply_desired("d0", &d), ApplyDecision::Restart);
        // Generation moved; the running unit (built at gen 0) will notice.
        assert_eq!(state.generation_of("d0"), 1);
        // Applied version has NOT advanced: keyup still sees it as pending.
        assert_eq!(state.applied("d0").unwrap().version, 0);

        // The factory rebuilds with the pending override, but the version still
        // does not advance on build alone (two-phase adopt).
        let built = state.take_for_build("d0");
        assert_eq!(built.generation, 1);
        assert_eq!(built.config.center_frequency, Some(852_000_000));
        assert_eq!(built.version, 4);
        assert_eq!(
            state.applied("d0").unwrap().version,
            0,
            "still pending until the stream confirms it opened"
        );

        // Only when the unit confirms its stream opened does the applied version
        // advance and the reported center reflect the new config.
        state.confirm_applied("d0", &built.config, built.version);
        let a = state.applied("d0").unwrap();
        assert_eq!(a.version, 4);
        assert_eq!(a.config.center_frequency, Some(852_000_000));
    }

    fn failure(retrying: bool) -> trunkd_supervisor::FailureReport<'static> {
        trunkd_supervisor::FailureReport {
            name: "device:d0",
            error: "device 'd0': no HackRF boards found",
            failures: 7,
            retrying,
            next_attempt_ms: Some(1_800_000_000_000),
        }
    }

    /// A build that fails to open never advances the applied version; the
    /// failure is reported beside it until a build opens.
    #[test]
    fn a_failed_open_is_reported_and_never_applied() {
        let state = state_for(&[dev("d0", DeviceRole::Auto, 851_000_000)]);
        assert_eq!(
            state.apply_desired("d0", &desired(5, Some(852_000_000), true)),
            ApplyDecision::Restart
        );
        let built = state.take_for_build("d0");
        // The unit fails to open, over and over: no confirm, only failures.
        state.note_failure("d0", &failure(false));
        state.note_failure("d0", &failure(true));

        let a = state.applied("d0").expect("known tuner");
        assert_eq!(a.version, 0, "never opened with v5");
        assert!(state.is_pending("d0"));
        let fault = a.fault.clone().expect("the failure is reported");
        assert!(fault.retrying);
        assert_eq!(fault.version, 5, "the version the failing build carries");
        assert_eq!(fault.next_attempt_ms, Some(1_800_000_000_000));

        // As keyup gets it: applied_version still 0, and why.
        let m = trunkd_core::Metrics::new();
        let t = gather_tuner_status(&m.snapshot(), &descriptor(), Some(&a), 0);
        let body = status_body(
            "rec-01",
            &Station::default(),
            &Counters::default(),
            &[t],
            &no_hold(),
            None,
            &EventLogReport::default(),
        );
        let t = &body["tuners"][0];
        assert_eq!(t["applied_version"], 0);
        assert_eq!(t["fault"]["state"], "retrying");
        assert_eq!(t["fault"]["version"], 5);
        assert_eq!(t["fault"]["failures"], 7);
        assert_eq!(t["fault"]["next_attempt_ms"], 1_800_000_000_000_i64);
        assert!(t["fault"]["error"]
            .as_str()
            .is_some_and(|e| e.contains("no HackRF")));

        // The build that finally opens confirms, and the fault is gone.
        state.confirm_applied("d0", &built.config, built.version);
        let a = state.applied("d0").expect("known tuner");
        assert_eq!(a.version, 5);
        assert!(a.fault.is_none());
        let t = gather_tuner_status(&m.snapshot(), &descriptor(), Some(&a), 0);
        assert!(t.fault.is_none());
    }

    /// Keyup redelivers the desired state every poll. A version already staged
    /// for a build that is still coming (or still retrying) must not restart
    /// it again each time.
    #[test]
    fn a_staged_version_is_not_restaged_by_a_redelivery() {
        let state = state_for(&[dev("d0", DeviceRole::Auto, 851_000_000)]);
        let d = desired(5, Some(852_000_000), true);
        assert_eq!(state.apply_desired("d0", &d), ApplyDecision::Restart);
        let generation = state.generation_of("d0");
        state.note_failure("d0", &failure(true));
        assert_eq!(state.apply_desired("d0", &d), ApplyDecision::NoOp);
        assert_eq!(
            state.generation_of("d0"),
            generation,
            "no re-arm, no restart"
        );
    }

    /// A newer version for a failing tuner re-arms it (the generation the
    /// supervisor watches moves) even when it only reverts to the config that
    /// last ran — which used to be a version-only bump, reported applied at
    /// once while nothing was open at all.
    #[test]
    fn a_new_version_for_a_failing_tuner_rearms_it_and_is_confirmed_only_by_an_open() {
        let state = state_for(&[dev("d0", DeviceRole::Auto, 851_000_000)]);
        // v4 runs.
        state.apply_desired("d0", &desired(4, Some(851_000_000), true));
        let good = state.take_for_build("d0");
        state.confirm_applied("d0", &good.config, good.version);
        assert_eq!(state.applied("d0").unwrap().version, 4);
        // v5 cannot open.
        assert_eq!(
            state.apply_desired("d0", &desired(5, Some(999_000_000), true)),
            ApplyDecision::Restart
        );
        let _bad = state.take_for_build("d0");
        state.note_failure("d0", &failure(true));
        let token = state.rearm_token("d0");

        // Keyup reverts: v6 is v4's config.
        assert_eq!(
            state.apply_desired("d0", &desired(6, Some(851_000_000), true)),
            ApplyDecision::Restart,
            "a rebuild, not an in-place bump: nothing is running to confirm it"
        );
        assert_ne!(
            state.rearm_token("d0"),
            token,
            "the waiting unit is re-armed"
        );
        assert_eq!(state.applied("d0").unwrap().version, 4, "not yet applied");
        let rebuilt = state.take_for_build("d0");
        assert_eq!(rebuilt.config.center_frequency, Some(851_000_000));
        assert_eq!(rebuilt.version, 6);
        state.confirm_applied("d0", &rebuilt.config, rebuilt.version);
        let a = state.applied("d0").unwrap();
        assert_eq!(a.version, 6);
        assert!(a.fault.is_none());
    }

    /// The panel's restart button re-arms a failing tuner the same way.
    #[test]
    fn a_restart_request_rearms_a_failing_tuner() {
        let state = state_for(&[dev("d0", DeviceRole::Auto, 851_000_000)]);
        state.note_failure("d0", &failure(true));
        let token = state.rearm_token("d0");
        assert!(state.request_restart("d0"));
        assert_ne!(state.rearm_token("d0"), token);
        // And still reports the fault until something opens.
        assert!(state.applied("d0").unwrap().fault.is_some());
    }

    #[test]
    fn a_rejected_config_does_not_advance_the_applied_version() {
        // The bad-center scenario: the change is applied to the registry and the
        // unit is rebuilt, but the stream never opens, so confirm is never
        // called and the applied version stays put — keyup keeps showing pending.
        let devices = vec![dev("d0", DeviceRole::Auto, 851_000_000)];
        let state = state_for(&devices);
        state.take_for_build("d0");
        assert_eq!(
            state.apply_desired("d0", &desired(9, Some(1), true)),
            ApplyDecision::Restart
        );
        let built = state.take_for_build("d0");
        assert_eq!(built.version, 9, "the build carries the new version");
        // ... but the stream open fails, so no confirm happens.
        assert_eq!(
            state.applied("d0").unwrap().version,
            0,
            "a config that never ran must not clear pending"
        );
    }

    #[test]
    fn a_version_only_bump_does_not_restart_but_clears_pending() {
        let devices = vec![dev("d0", DeviceRole::Auto, 851_000_000)];
        let state = state_for(&devices);
        state.take_for_build("d0");
        // Same config, newer version, plus a spectrum flag toggle.
        let d = DesiredTuner {
            version: 7,
            role: Some(DeviceRole::Auto),
            site: None,
            site_id: None,
            center_frequency: Some(851_000_000),
            bands: None,
            enabled: true,
            spectrum: true,
            ..DesiredTuner::default()
        };
        assert_eq!(state.apply_desired("d0", &d), ApplyDecision::VersionOnly);
        assert_eq!(state.generation_of("d0"), 0, "no restart");
        let a = state.applied("d0").unwrap();
        assert_eq!(a.version, 7, "applied version advanced in place");
        assert!(a.spectrum, "spectrum flag stored");
    }

    /// The boot regression: keyup's version is newer than the boot state's 0
    /// on the very first poll after a start, but every value it carries is the
    /// one already running. Nothing may restart.
    #[test]
    fn a_push_matching_the_running_config_never_restarts() {
        let devices = vec![dev("d0", DeviceRole::Auto, 851_000_000)];
        let state = state_for(&devices);
        state.take_for_build("d0");

        let d = DesiredTuner {
            version: 4,
            role: Some(DeviceRole::Auto),
            center_frequency: Some(851_000_000),
            enabled: true,
            ..DesiredTuner::default()
        };
        assert_eq!(state.apply_desired("d0", &d), ApplyDecision::VersionOnly);
        assert_eq!(state.generation_of("d0"), 0, "no rebuild was staged");
        assert_eq!(
            state.applied("d0").unwrap().version,
            4,
            "keyup's pending badge still clears"
        );
        assert!(!state.is_pending("d0"), "nothing left staged");
    }

    /// The same poll, but carrying keyup's numeric site id — which the TOML has
    /// no place to hold, so it always differs from the boot state. It is
    /// recorded (and echoed back) without costing a stream gap.
    #[test]
    fn a_site_id_only_push_is_recorded_without_a_restart() {
        let devices = vec![dev("d0", DeviceRole::Auto, 851_000_000)];
        let state = state_for(&devices);
        state.take_for_build("d0");

        let d = DesiredTuner {
            version: 4,
            role: Some(DeviceRole::Auto),
            site_id: Some("7".into()),
            center_frequency: Some(851_000_000),
            enabled: true,
            ..DesiredTuner::default()
        };
        assert_eq!(state.apply_desired("d0", &d), ApplyDecision::Recorded);
        assert_eq!(state.generation_of("d0"), 0, "no rebuild was staged");
        let a = state.applied("d0").unwrap();
        assert_eq!(a.version, 4, "the applied version advanced in place");
        assert_eq!(
            a.config.site_id.as_deref(),
            Some("7"),
            "and the id is echoed back"
        );
        assert!(!state.is_pending("d0"), "nothing left staged");
    }

    /// One real field alongside it still restarts, exactly as before.
    #[test]
    fn a_changed_field_beside_the_site_id_still_restarts() {
        let devices = vec![dev("d0", DeviceRole::Auto, 851_000_000)];
        let state = state_for(&devices);
        state.take_for_build("d0");

        let d = DesiredTuner {
            version: 4,
            role: Some(DeviceRole::Auto),
            site_id: Some("7".into()),
            center_frequency: Some(852_000_000),
            enabled: true,
            ..DesiredTuner::default()
        };
        assert_eq!(state.apply_desired("d0", &d), ApplyDecision::Restart);
        assert_eq!(state.generation_of("d0"), 1, "the unit is asked to rebuild");
        let built = state.take_for_build("d0");
        assert_eq!(built.config.center_frequency, Some(852_000_000));
        assert_eq!(built.version, 4);
    }

    #[test]
    fn a_bare_restart_request_bumps_only_the_generation() {
        let devices = vec![dev("d0", DeviceRole::Auto, 851_000_000)];
        let state = state_for(&devices);
        state.take_for_build("d0");
        state.confirm_applied(
            "d0",
            &AppliedConfig {
                role: Some(DeviceRole::Auto),
                site: None,
                site_id: None,
                center_frequency: Some(851_000_000),
                bands: None,
                enabled: true,
                ..AppliedConfig::default()
            },
            0,
        );

        assert!(state.request_restart("d0"), "a known tuner");
        assert_eq!(state.generation_of("d0"), 1, "the unit is asked to rebuild");
        // The rebuild reads exactly what the running unit was built with, so a
        // panel restart cannot smuggle in a config change.
        let built = state.take_for_build("d0");
        assert_eq!(built.config.center_frequency, Some(851_000_000));
        assert_eq!(built.version, 0, "the applied version does not move");
        assert!(!state.is_pending("d0"), "no config is left staged");

        assert!(!state.request_restart("ghost"), "an unknown tuner says so");
    }

    #[test]
    fn a_restart_request_leaves_a_staged_config_staged() {
        let devices = vec![dev("d0", DeviceRole::Auto, 851_000_000)];
        let state = state_for(&devices);
        state.take_for_build("d0");
        assert_eq!(
            state.apply_desired("d0", &desired(4, Some(852_000_000), true)),
            ApplyDecision::Restart
        );
        assert!(state.is_pending("d0"), "keyup's change is staged");

        assert!(state.request_restart("d0"));
        // The staged change is still what the next build takes: a restart
        // request is a nudge, never a rollback.
        let built = state.take_for_build("d0");
        assert_eq!(built.config.center_frequency, Some(852_000_000));
        assert_eq!(built.version, 4);
    }

    #[test]
    fn an_unknown_tuner_is_ignored() {
        let devices = vec![dev("d0", DeviceRole::Auto, 851_000_000)];
        let state = state_for(&devices);
        assert_eq!(
            state.apply_desired("ghost", &desired(1, Some(852_000_000), true)),
            ApplyDecision::NoOp
        );
    }

    // -- defensive parsing --------------------------------------------------

    #[test]
    fn parses_a_well_formed_desired_state() {
        let body = r#"{
            "version": 12,
            "tuners": {
                "d0": {"version": 3, "config": {
                    "role": "voice", "site": "Central", "site_id": 7,
                    "center_frequency": 852000000, "enabled": true, "spectrum": true
                }},
                "d1": {"version": 1, "config": {"enabled": false}}
            }
        }"#;
        let snap = parse_desired(body);
        assert_eq!(snap.version, 12);
        assert_eq!(snap.tuners.len(), 2);
        let d0 = &snap.tuners.iter().find(|(k, _)| k == "d0").unwrap().1;
        assert_eq!(d0.version, 3);
        assert_eq!(d0.role, Some(DeviceRole::Voice));
        assert_eq!(d0.site.as_deref(), Some("Central"));
        assert_eq!(d0.site_id.as_deref(), Some("7"), "site_id kept distinct");
        assert_eq!(d0.center_frequency, Some(852_000_000));
        assert!(d0.enabled);
        assert!(d0.spectrum);
        // Absent fields default sanely.
        let d1 = &snap.tuners.iter().find(|(k, _)| k == "d1").unwrap().1;
        assert!(!d1.enabled);
        assert_eq!(d1.role, None);
        assert_eq!(d1.center_frequency, None);
        assert!(!d1.spectrum);
    }

    #[test]
    fn the_recorder_block_is_optional_and_tolerated_when_absent() {
        // An older keyup sends no recorder block at all; that must parse to
        // "not managed", not to "release the hold".
        let snap = parse_desired(r#"{"version":3,"tuners":{}}"#);
        assert_eq!(snap.recorder, None);
        // Present but not an object, or `null`: same answer.
        assert_eq!(parse_desired(r#"{"recorder":null}"#).recorder, None);
        assert_eq!(parse_desired(r#"{"recorder":7}"#).recorder, None);
        // Present with a null value: the version is kept, the value is not.
        assert_eq!(
            parse_desired(r#"{"recorder":{"version":4,"spool_only":null}}"#).recorder,
            Some(DesiredRecorder {
                version: 4,
                ..DesiredRecorder::default()
            })
        );
        assert_eq!(
            parse_desired(r#"{"recorder":{"version":4,"spool_only":true}}"#).recorder,
            Some(DesiredRecorder {
                version: 4,
                spool_only: Some(true),
                ..DesiredRecorder::default()
            })
        );
    }

    /// A recorder control whose link slot is real (but never started), so a
    /// pushed `link` can be inspected as it would be applied.
    fn recorder_with_link() -> (Arc<RecorderControl>, Arc<LinkSlot>) {
        let cfg = Config::default();
        let slot = LinkSlot::new(&cfg, &[], trunkd_core::Metrics::new(), "rec-a".to_owned());
        let rc = RecorderControl::new(
            Arc::new(UplinkHold::new(false)),
            crate::pipeline::eventarchive::ArchiveState::new(false),
            slot.clone(),
            trunkd_uplink::BroadcastifyState::new(true, &trunkd_core::Metrics::new()),
            trunkd_core::SkipList::empty(),
        );
        (rc, slot)
    }

    #[test]
    fn the_whole_recorder_block_parses() {
        let snap = parse_desired(
            r#"{"recorder":{"version":9,"spool_only":true,"event_log_archive":true,
                 "link":{"enabled":true,"bind":"0.0.0.0:7345","peers":["192.0.2.31:7345"],
                         "key":"a-sixteen-byte-key-or-longer","hello_interval_s":10,
                         "accept_remote_grants":true,"publish_grants":false,
                         "defer_to_claims":true}}}"#,
        );
        let r = snap.recorder.expect("a recorder block");
        assert_eq!(r.version, 9);
        assert_eq!(r.spool_only, Some(true));
        assert_eq!(r.event_log_archive, Some(true));
        assert!(r.rejected.is_empty(), "{:?}", r.rejected);
        let link = r.link.expect("a link block");
        assert_eq!(link.enabled, Some(true));
        assert_eq!(link.bind.as_deref(), Some("0.0.0.0:7345"));
        assert_eq!(
            link.peers.as_deref(),
            Some(&["192.0.2.31:7345".to_owned()][..])
        );
        assert_eq!(link.key.as_deref(), Some("a-sixteen-byte-key-or-longer"));
        assert_eq!(link.hello_interval_s, Some(10));
        assert_eq!(link.accept_remote_grants, Some(true));
        assert_eq!(link.publish_grants, Some(false));
        assert_eq!(link.defer_to_claims, Some(true));
    }

    #[test]
    fn an_unmanaged_recorder_field_is_null_and_not_a_rejection() {
        // Every field is a tri-state: `null` (or absent) is "keyup is not
        // managing this", which must leave the TOML value alone.
        let r = parse_desired(
            r#"{"recorder":{"version":2,"spool_only":null,"event_log_archive":null,"link":null}}"#,
        )
        .recorder
        .expect("a block");
        assert_eq!(r.spool_only, None);
        assert_eq!(r.event_log_archive, None);
        assert_eq!(r.link, None);
        assert!(r.rejected.is_empty());

        // A partial link block manages only the fields it names.
        let r = parse_desired(r#"{"recorder":{"version":3,"link":{"peers":[]}}}"#)
            .recorder
            .expect("a block");
        let link = r.link.expect("a link block");
        assert_eq!(
            link.peers,
            Some(Vec::new()),
            "an empty peer list is a value"
        );
        assert_eq!(link.enabled, None);
        assert_eq!(link.key, None);
        // Laid over a config, only `peers` moves.
        let base = crate::config::Link {
            enabled: true,
            key: "a-sixteen-byte-key-or-longer".to_owned(),
            peers: vec!["192.0.2.9:7345".to_owned()],
            ..crate::config::Link::default()
        };
        let merged = link.merge_over(&base);
        assert!(merged.peers.is_empty());
        assert_eq!(merged.key, base.key, "the key the TOML gave it survives");
        assert!(merged.enabled);
    }

    #[test]
    fn a_bad_recorder_field_is_rejected_with_a_reason_and_the_rest_applies() {
        let r = parse_desired(
            r#"{"recorder":{"version":4,"spool_only":"yes","event_log_archive":true,
                 "link":{"enabled":true,"bind":"not-an-address","peers":["192.0.2.4:7345","nope"],
                         "key":"short","hello_interval_s":900,"publish_grants":true}}}"#,
        )
        .recorder
        .expect("a block");
        let by_field: HashMap<&str, &str> = r
            .rejected
            .iter()
            .map(|(f, why)| (f.as_str(), why.as_str()))
            .collect();
        assert!(by_field.contains_key("spool_only"), "{by_field:?}");
        assert!(by_field["link.bind"].contains("host:port"), "{by_field:?}");
        assert!(
            by_field.contains_key("link.peers"),
            "a bad entry fails the list"
        );
        assert!(by_field["link.key"].contains("at least"), "{by_field:?}");
        assert!(by_field.contains_key("link.hello_interval_s"));
        assert_eq!(by_field.len(), 5, "{by_field:?}");

        // Everything well-formed still applies.
        assert_eq!(r.event_log_archive, Some(true));
        assert_eq!(r.spool_only, None);
        let link = r.link.expect("the rest of the link block");
        assert_eq!(link.enabled, Some(true));
        assert_eq!(link.publish_grants, Some(true));
        assert_eq!(link.bind, None);
        assert_eq!(link.peers, None);
        assert_eq!(link.key, None);
        assert_eq!(link.hello_interval_s, None);

        // A link that is not an object at all is one rejection, not a crash.
        let r = parse_desired(r#"{"recorder":{"version":5,"link":7}}"#)
            .recorder
            .expect("a block");
        assert_eq!(r.link, None);
        assert_eq!(r.rejected.len(), 1);
        assert_eq!(r.rejected[0].0, "link");
    }

    /// The broadcastify block: parsed, applied by its own revision, and
    /// dropped by an explicit `null` — none of which touches the recorder
    /// block's version gate.
    #[test]
    fn keyup_pushes_the_broadcastify_block_and_takes_it_away_again() {
        let body = |rev: &str| {
            format!(
                r#"{{"recorder":{{"version":9,"broadcastify":{{"enabled":true,
                 "endpoint_url":"https://api.broadcastify.com/call-upload","api_key":"bcfy-secret-key",
                 "system_id":"6551","encoding":"mp3","talkgroups":[52401],"skew_limit_s":15.0,
                 "claim":true,"claim_url":"/api/recorder/broadcastify/claim",
                 "revision":"{rev}"}}}}}}"#
            )
        };
        let d = parse_desired(&body("a1b2c3d4e5f6"))
            .recorder
            .expect("a block");
        let b = d.broadcastify.clone().expect("managed").expect("not null");
        assert_eq!(b.system_id, "6551");
        assert_eq!(b.talkgroups, Some(vec![52401]));
        assert!(b.claim);

        let rc = RecorderControl::for_test(Arc::new(UplinkHold::new(false)));
        assert!(rc.apply_desired(&d), "the first push applies");
        assert!(rc.broadcastify().enabled());
        assert_eq!(
            rc.effective_json()["broadcastify"]["revision"],
            "a1b2c3d4e5f6"
        );
        // The key itself never appears anywhere the panel or a log can see it.
        let shown = rc.effective_json()["broadcastify"].to_string();
        assert!(!shown.contains("bcfy-secret-key"), "{shown}");
        assert_eq!(rc.effective_json()["broadcastify"]["api_key_set"], true);
        assert_eq!(
            rc.effective_json()["broadcastify"]["api_key_prefix"],
            "bcfy-s",
            "six characters, and no more"
        );

        // A redelivery of the same revision changes nothing...
        let same = parse_desired(&body("a1b2c3d4e5f6"))
            .recorder
            .expect("a block");
        assert!(!rc.apply_desired(&same), "same revision, same state");
        // ...and a new one applies even though the recorder version has not
        // moved, because the block is keyed on its own revision.
        let next = parse_desired(&body("ffffffffffff"))
            .recorder
            .expect("a block");
        assert!(rc.apply_desired(&next));
        assert_eq!(
            rc.effective_json()["broadcastify"]["revision"],
            "ffffffffffff"
        );

        // `null` is the feature being turned off: the held block is dropped.
        let off = parse_desired(r#"{"recorder":{"version":9,"broadcastify":null}}"#)
            .recorder
            .expect("a block");
        assert_eq!(off.broadcastify, Some(None));
        assert!(rc.apply_desired(&off));
        assert!(!rc.broadcastify().enabled());
        assert_eq!(rc.effective_json()["broadcastify"]["revision"], Value::Null);

        // An absent key is "keyup is not managing this", which leaves whatever
        // is held alone.
        let unmanaged = parse_desired(r#"{"recorder":{"version":10,"spool_only":true}}"#)
            .recorder
            .expect("a block");
        assert_eq!(unmanaged.broadcastify, None);
    }

    /// The skip list: parsed as a tri-state, applied by its own revision,
    /// dropped by an explicit `null`, and unioned with the operator's own
    /// `[recorder] skip_talkgroups` — which keyup can never take away.
    #[test]
    fn keyup_pushes_a_skip_list_and_takes_it_away_again() {
        let body = |rev: &str, tgids: &str| {
            format!(
                r#"{{"recorder":{{"version":9,"skip_talkgroups":{{"tgids":{tgids},
                 "revision":"{rev}"}}}}}}"#
            )
        };
        let d = parse_desired(&body("a1b2c3d4e5f6", "[1786, 1787]"))
            .recorder
            .expect("a block");
        let spec = d
            .skip_talkgroups
            .clone()
            .expect("managed")
            .expect("not null");
        assert_eq!(spec.tgids, vec![1786, 1787]);
        assert_eq!(spec.revision, "a1b2c3d4e5f6");

        // The recorder holds the operator's own two entries as well.
        let rc = RecorderControl::new(
            Arc::new(UplinkHold::new(false)),
            crate::pipeline::eventarchive::ArchiveState::new(false),
            crate::pipeline::link::LinkSlot::disabled(),
            trunkd_uplink::BroadcastifyState::new(true, &trunkd_core::Metrics::new()),
            trunkd_core::SkipList::new([1001, 1002]),
        );
        assert!(rc.apply_desired(&d), "the first push applies");
        assert!(rc.skip().contains(1786));
        assert!(rc.skip().contains(1001), "the local half survives");
        assert_eq!(rc.skip().len(), 4, "unioned, not replaced");
        let eff = rc.effective_json();
        assert_eq!(eff["skip_talkgroups"]["count"], 4);
        assert_eq!(eff["skip_talkgroups"]["revision"], "a1b2c3d4e5f6");
        assert_eq!(eff["skip_talkgroups"]["local"], json!([1001, 1002]));

        // A redelivery of the revision in force costs nothing...
        let same = parse_desired(&body("a1b2c3d4e5f6", "[9]"))
            .recorder
            .expect("a block");
        assert!(!rc.apply_desired(&same), "same revision, same state");
        assert!(!rc.skip().contains(9));
        // ...and a new revision replaces keyup's half whole, with no move of
        // the recorder block's own version.
        let next = parse_desired(&body("ffffffffffff", "[9]"))
            .recorder
            .expect("a block");
        assert!(rc.apply_desired(&next));
        assert!(rc.skip().contains(9));
        assert!(!rc.skip().contains(1786), "replaced, not merged");
        assert!(rc.skip().contains(1001));

        // `null` drops keyup's half; the file's half is still the operator's.
        let off = parse_desired(r#"{"recorder":{"version":9,"skip_talkgroups":null}}"#)
            .recorder
            .expect("a block");
        assert_eq!(off.skip_talkgroups, Some(None));
        assert!(rc.apply_desired(&off));
        assert!(!rc.skip().contains(9));
        assert!(rc.skip().contains(1001));
        assert_eq!(rc.effective_json()["skip_talkgroups"]["count"], 2);
        assert_eq!(
            rc.effective_json()["skip_talkgroups"]["revision"],
            Value::Null
        );

        // Absent is "keyup is not managing this": whatever is held stays.
        let unmanaged = parse_desired(r#"{"recorder":{"version":10,"spool_only":true}}"#)
            .recorder
            .expect("a block");
        assert_eq!(unmanaged.skip_talkgroups, None);
        assert!(!rc.apply_desired(&unmanaged) || rc.skip().contains(1001));
    }

    /// A skip list is refused whole rather than field by field: half a list is
    /// a recorder that records some of what the operator turned off.
    #[test]
    fn a_malformed_skip_list_is_refused_with_a_reason() {
        let r = parse_desired(
            r#"{"recorder":{"version":4,"event_log_archive":true,
                 "skip_talkgroups":{"tgids":[1786,"nope"],"revision":"r"}}}"#,
        )
        .recorder
        .expect("a block");
        assert_eq!(r.skip_talkgroups, None);
        assert_eq!(r.event_log_archive, Some(true), "the rest still applies");
        let (_, why) = r
            .rejected
            .iter()
            .find(|(f, _)| f == "skip_talkgroups")
            .expect("refused with a reason");
        assert!(why.contains("1..=65535"), "{why}");

        // A talkgroup outside the 16-bit range is the same kind of mistake.
        let r = parse_desired(
            r#"{"recorder":{"version":4,"skip_talkgroups":{"tgids":[70000],"revision":"r"}}}"#,
        )
        .recorder
        .expect("a block");
        assert_eq!(r.skip_talkgroups, None);
        assert_eq!(r.rejected.len(), 1);

        // And so is a list that is not a list at all.
        let r = parse_desired(r#"{"recorder":{"version":4,"skip_talkgroups":{"revision":"r"}}}"#)
            .recorder
            .expect("a block");
        assert_eq!(r.skip_talkgroups, None);
        assert!(r.rejected[0].1.contains("tgids"), "{:?}", r.rejected);
    }

    /// A malformed block is one refusal with a reason, not a crash, and the
    /// rest of the recorder block still applies.
    #[test]
    fn a_broadcastify_block_with_no_endpoint_is_refused_by_itself() {
        let r = parse_desired(
            r#"{"recorder":{"version":4,"event_log_archive":true,
                 "broadcastify":{"enabled":true,"api_key":"k","revision":"r"}}}"#,
        )
        .recorder
        .expect("a block");
        assert_eq!(r.broadcastify, None);
        assert_eq!(r.event_log_archive, Some(true), "the rest still applies");
        let (field, why) = r
            .rejected
            .iter()
            .find(|(f, _)| f == "broadcastify")
            .expect("refused with a reason");
        assert_eq!(field, "broadcastify");
        assert!(why.contains("endpoint_url"), "{why}");
    }

    #[test]
    fn keyup_can_turn_the_event_log_archive_on_and_off() {
        let (rc, _slot) = recorder_with_link();
        assert!(!rc.applied().is_some());

        assert!(rc.apply_desired(&DesiredRecorder {
            version: 1,
            event_log_archive: Some(true),
            ..DesiredRecorder::default()
        }));
        let applied = rc.applied().expect("echoed");
        assert!(applied.event_log_archive);
        assert_eq!(applied.version, 1);
        assert_eq!(
            rc.effective_json()["event_log_archive_source"],
            "keyup",
            "and the panel says who set it"
        );

        // Off again, on a newer version.
        assert!(rc.apply_desired(&DesiredRecorder {
            version: 2,
            event_log_archive: Some(false),
            ..DesiredRecorder::default()
        }));
        assert!(!rc.applied().expect("echoed").event_log_archive);
    }

    #[test]
    fn keyup_can_configure_the_link_and_the_key_is_never_echoed() {
        let (rc, slot) = recorder_with_link();
        assert!(!slot.config().enabled, "the TOML has no link");

        assert!(rc.apply_desired(&DesiredRecorder {
            version: 3,
            link: Some(DesiredLink {
                enabled: Some(true),
                bind: Some("127.0.0.1:0".to_owned()),
                peers: Some(vec!["192.0.2.31:7345".to_owned()]),
                key: Some("a-sixteen-byte-key-or-longer".to_owned()),
                hello_interval_s: Some(5),
                ..DesiredLink::default()
            }),
            ..DesiredRecorder::default()
        }));
        let effective = slot.config();
        assert!(effective.enabled);
        assert_eq!(effective.bind, "127.0.0.1:0");
        assert_eq!(effective.key, "a-sixteen-byte-key-or-longer");

        let applied = rc.applied().expect("echoed");
        let json = applied.to_json();
        assert_eq!(json["link"]["enabled"], true);
        assert_eq!(json["link"]["bind"], "127.0.0.1:0");
        assert_eq!(json["link"]["peers"][0], "192.0.2.31:7345");
        assert_eq!(json["link"]["hello_interval_s"], 5);
        assert_eq!(json["link"]["key_set"], true);
        assert_eq!(
            json["link"]["key_fingerprint"],
            Value::String(
                crate::pipeline::link::key_fingerprint("a-sixteen-byte-key-or-longer")
                    .expect("a fingerprint")
            )
        );
        let rendered = json.to_string();
        assert!(
            !rendered.contains("a-sixteen-byte-key-or-longer"),
            "the key never leaves the box: {rendered}"
        );
        assert!(json.get("rejected").is_none(), "nothing was refused");

        // A push that names only one field keeps the rest of what is in effect.
        assert!(rc.apply_desired(&DesiredRecorder {
            version: 4,
            link: Some(DesiredLink {
                defer_to_claims: Some(true),
                ..DesiredLink::default()
            }),
            ..DesiredRecorder::default()
        }));
        let effective = slot.config();
        assert!(effective.defer_to_claims);
        assert_eq!(effective.key, "a-sixteen-byte-key-or-longer");
        assert_eq!(effective.peers, vec!["192.0.2.31:7345".to_owned()]);
    }

    #[test]
    fn a_recorder_rejection_is_echoed_even_though_nothing_moved() {
        let (rc, _slot) = recorder_with_link();
        assert!(!rc.apply_desired(&DesiredRecorder {
            version: 6,
            rejected: vec![("link.key".to_owned(), "must be at least 16".to_owned())],
            ..DesiredRecorder::default()
        }));
        let applied = rc.applied().expect("the version is still acknowledged");
        assert_eq!(applied.version, 6);
        assert_eq!(
            applied.to_json()["rejected"]["link.key"],
            "must be at least 16"
        );
    }

    #[test]
    fn the_status_body_reports_the_recorder_capabilities() {
        let body = status_body(
            "rec-01",
            &Station::default(),
            &Counters::default(),
            &[],
            &no_hold(),
            None,
            &EventLogReport::default(),
        );
        assert_eq!(
            body["capabilities"]["recorder"]["restart_on"],
            json!([]),
            "no recorder-level setting costs a restart"
        );
    }

    /// `(version, spool_only)` of the echo, the shape these tests read.
    fn applied_pair(rc: &RecorderControl) -> Option<(i64, bool)> {
        rc.applied().map(|a| (a.version, a.spool_only))
    }

    #[test]
    fn the_recorder_hold_applies_only_on_a_newer_version_with_a_value() {
        let hold = Arc::new(UplinkHold::new(false));
        let rc = RecorderControl::for_test(hold.clone());
        assert_eq!(applied_pair(&rc), None);

        // No value: keyup is not managing the hold, so nothing moves and
        // nothing is echoed.
        assert!(!rc.apply_desired(&DesiredRecorder {
            version: 9,
            spool_only: None,
            ..DesiredRecorder::default()
        }));
        assert_eq!(applied_pair(&rc), None);

        // Version 0 is a real version, distinct from "never pushed".
        assert!(rc.apply_desired(&DesiredRecorder {
            version: 0,
            spool_only: Some(true),
            ..DesiredRecorder::default()
        }));
        assert!(hold.is_held());
        assert_eq!(hold.source(), HoldSource::Keyup);
        assert_eq!(applied_pair(&rc), Some((0, true)));

        // A redelivery of the same version must not undo a panel toggle made
        // since — the most-recent-writer rule.
        rc.set_hold_from_panel(false);
        assert!(!rc.apply_desired(&DesiredRecorder {
            version: 0,
            spool_only: Some(true),
            ..DesiredRecorder::default()
        }));
        assert!(!hold.is_held(), "the panel still wins");
        assert_eq!(hold.source(), HoldSource::Panel);
        assert_eq!(
            applied_pair(&rc),
            Some((0, true)),
            "the echo still reports what keyup asked for"
        );

        // A newer version does take it back.
        assert!(rc.apply_desired(&DesiredRecorder {
            version: 1,
            spool_only: Some(true),
            ..DesiredRecorder::default()
        }));
        assert!(hold.is_held());
        assert_eq!(applied_pair(&rc), Some((1, true)));

        // An older one is ignored outright.
        assert!(!rc.apply_desired(&DesiredRecorder {
            version: 0,
            spool_only: Some(false),
            ..DesiredRecorder::default()
        }));
        assert!(hold.is_held());
    }

    #[test]
    fn the_status_body_reports_the_hold_and_echoes_what_keyup_applied() {
        let report = UplinkReport {
            spool_only: true,
            source: "keyup",
            spooled_calls: 12,
            spool_bytes: 4096,
            url: "http://192.0.2.60/api/call-upload".to_owned(),
            spooled_on_pressure: 0,
            quarantined: 0,
            circuit_open: false,
            stream: crate::pipeline::stream::StreamState::new(false).report(),
        };
        let body = status_body(
            "rec-01",
            &Station::default(),
            &Counters::default(),
            &[],
            &report,
            Some(RecorderApplied {
                version: 7,
                spool_only: true,
                event_log_archive: false,
                link: crate::config::Link::default(),
                rejected: Vec::new(),
            }),
            &EventLogReport::default(),
        );
        assert_eq!(body["uplink"]["spool_only"], true);
        assert_eq!(body["uplink"]["source"], "keyup");
        assert_eq!(body["uplink"]["spooled_calls"], 12);
        assert_eq!(body["uplink"]["spool_bytes"], 4096);
        assert_eq!(body["uplink"]["url"], "http://192.0.2.60/api/call-upload");
        assert_eq!(body["recorder_applied"]["version"], 7);
        assert_eq!(body["recorder_applied"]["spool_only"], true);

        // Nothing pushed yet: the echo is absent rather than invented.
        let body = status_body(
            "rec-01",
            &Station::default(),
            &Counters::default(),
            &[],
            &report,
            None,
            &EventLogReport::default(),
        );
        assert!(body.get("recorder_applied").is_none());
        assert!(
            body.get("uplink").is_some(),
            "but the uplink block is always there"
        );
    }

    #[test]
    fn the_uplink_report_reads_its_spool_numbers_from_the_counters() {
        // One source of truth: keyup must never see a `spooled` counter that
        // disagrees with the `uplink` block sitting beside it.
        let rc = RecorderControl::for_test(Arc::new(UplinkHold::new(true)));
        let counters = Counters {
            spooled: 5,
            spool_bytes: 999,
            spooled_on_pressure: 3,
            quarantined: 1,
            circuit_open: true,
            ..Counters::default()
        };
        let stream = crate::pipeline::stream::StreamState::new(true);
        let r = uplink_report(&counters, &rc, "http://keyup/api/call-upload", &stream);
        assert!(r.stream.enabled && !r.stream.connected);
        assert_eq!(r.spooled_calls, 5);
        assert_eq!(r.spool_bytes, 999);
        // Why calls are spooled that nobody held, and which will not drain.
        assert_eq!(r.spooled_on_pressure, 3);
        assert_eq!(r.quarantined, 1);
        assert!(r.circuit_open);
        let wire = r.to_json();
        assert_eq!(wire["spooled_on_pressure"], 3);
        assert_eq!(wire["quarantined"], 1);
        assert_eq!(wire["circuit_open"], true);
        assert!(r.spool_only);
        assert_eq!(r.source, "config");
    }

    #[test]
    fn site_id_is_parsed_distinctly_from_the_label() {
        let body = r#"{"version":1,"tuners":{"d0":{"version":1,"config":{"site_id":42}}}}"#;
        let snap = parse_desired(body);
        // No label given, but the numeric id is captured on its own field.
        assert_eq!(snap.tuners[0].1.site, None);
        assert_eq!(snap.tuners[0].1.site_id.as_deref(), Some("42"));
    }

    #[test]
    fn malformed_json_yields_an_empty_snapshot() {
        assert_eq!(
            parse_desired("not json at all"),
            DesiredSnapshot {
                version: 0,
                recorder: None,
                tuners: Vec::new(),
                refused: Vec::new(),
            }
        );
        // A tuner entry that is not an object is skipped, not fatal.
        let snap = parse_desired(r#"{"version":1,"tuners":{"d0":"bogus","d1":{"config":{}}}}"#);
        assert_eq!(snap.version, 1);
        assert_eq!(snap.tuners.len(), 1);
        assert_eq!(snap.tuners[0].0, "d1");
    }

    #[test]
    fn an_unknown_role_is_dropped_not_applied() {
        let snap = parse_desired(
            r#"{"version":1,"tuners":{"d0":{"version":1,"config":{"role":"nonsense"}}}}"#,
        );
        assert_eq!(snap.tuners[0].1.role, None);
    }

    #[test]
    fn an_oversized_response_is_ignored() {
        let big = format!(
            r#"{{"version":1,"tuners":{{}},"pad":"{}"}}"#,
            "x".repeat(MAX_RESPONSE_BYTES + 1)
        );
        assert_eq!(parse_desired(&big).tuners.len(), 0);
        assert_eq!(parse_desired(&big).version, 0);
    }

    #[test]
    fn more_than_the_cap_of_tuners_is_truncated() {
        let mut entries = String::new();
        for i in 0..(MAX_TUNERS + 10) {
            if i > 0 {
                entries.push(',');
            }
            entries.push_str(&format!(r#""t{i}":{{"version":1,"config":{{}}}}"#));
        }
        let body = format!(r#"{{"version":1,"tuners":{{{entries}}}}}"#);
        assert_eq!(parse_desired(&body).tuners.len(), MAX_TUNERS);
    }

    #[test]
    fn a_key_over_the_length_cap_is_skipped() {
        let long = "k".repeat(MAX_TUNER_KEY_LEN + 1);
        let body =
            format!(r#"{{"version":1,"tuners":{{"{long}":{{"version":1,"config":{{}}}}}}}}"#);
        assert!(parse_desired(&body).tuners.is_empty());
    }

    // -- status body shape + caps -------------------------------------------

    /// An `uplink` report for a recorder that is uploading normally: the
    /// status-body tests are about everything except the hold.
    fn no_hold() -> UplinkReport {
        UplinkReport {
            spool_only: false,
            source: "config",
            spooled_calls: 0,
            spool_bytes: 0,
            url: "http://keyup.example/api/call-upload".to_owned(),
            spooled_on_pressure: 0,
            quarantined: 0,
            circuit_open: false,
            stream: crate::pipeline::stream::StreamState::new(true).report(),
        }
    }

    fn sample_status() -> TunerStatus {
        TunerStatus {
            tuner_key: "ctrl".into(),
            serial: Some("abc123".into()),
            backend: "hackrf",
            center_hz: 855_500_000,
            span_hz: 19_975_000,
            role: "control",
            site: Some("Central".into()),
            site_id: Some("7".into()),
            locked: true,
            control_hz: 851_050_000,
            nac: 0x34,
            tsbk_rate: 15.0,
            pool_in_use: 3,
            pool_size: 20,
            overflow_per_s: 0,
            calls_active: 2,
            applied_version: 4,
            spectrum: false,
            ..TunerStatus::default()
        }
    }

    /// The `monitor` block is what Keyup's "listen live" link is built from:
    /// the panel's base URL, the stream route under it, and whether a token is
    /// needed to open it.
    #[test]
    fn status_body_carries_the_panel_and_its_live_monitor() {
        let mut body = status_body(
            "rec-01",
            &Station::default(),
            &Counters::default(),
            &[sample_status()],
            &no_hold(),
            None,
            &EventLogReport::default(),
        );
        let server = crate::config::Server {
            enabled: true,
            bind: "0.0.0.0:8095".into(),
            live_audio: true,
            ..Default::default()
        };
        let panel = PanelLink {
            // A wildcard bind is reported as loopback: the link works from the
            // recorder's own browser, and Keyup labels it as such.
            base_url: server.public_base_url(),
            live_audio: server.live_audio,
            token_required: !server.tokens.is_empty(),
            hub: None,
        };
        insert_monitor(&mut body, Some(&panel));
        assert_eq!(body["monitor"]["enabled"], true);
        assert_eq!(body["monitor"]["url"], "http://127.0.0.1:8095");
        assert_eq!(
            body["monitor"]["stream_url"],
            "http://127.0.0.1:8095/api/monitor/stream"
        );
        assert_eq!(body["monitor"]["token_required"], false);
        assert_eq!(body["monitor"]["streams"], 0);
        assert_eq!(
            body["monitor"]["max_streams"],
            crate::pipeline::monitor::MAX_STREAMS
        );
    }

    /// No panel, no block at all: absent means "this recorder serves no page",
    /// which Keyup must be able to tell from a page with the tap switched off.
    #[test]
    fn a_recorder_with_no_panel_posts_no_monitor_block() {
        let mut body = status_body(
            "rec-01",
            &Station::default(),
            &Counters::default(),
            &[sample_status()],
            &no_hold(),
            None,
            &EventLogReport::default(),
        );
        insert_monitor(&mut body, None);
        assert!(body.get("monitor").is_none(), "{body}");
    }

    #[test]
    fn status_body_matches_the_contract_shape() {
        let body = status_body(
            "rec-01",
            &Station::default(),
            &Counters::default(),
            &[sample_status()],
            &no_hold(),
            None,
            &EventLogReport::default(),
        );
        assert_eq!(body["instance_id"], "rec-01");
        assert_eq!(body["recorder_version"], env!("CARGO_PKG_VERSION"));
        let t = &body["tuners"][0];
        assert_eq!(t["tuner_key"], "ctrl");
        assert_eq!(t["serial"], "abc123");
        assert_eq!(t["backend"], "hackrf");
        assert_eq!(t["center_hz"], 855_500_000u64);
        assert_eq!(t["span_hz"], 19_975_000u64);
        assert_eq!(t["role"], "control");
        assert_eq!(t["site"], "Central");
        assert_eq!(t["site_id"], "7");
        assert_eq!(t["locked"], true);
        assert_eq!(t["control_hz"], 851_050_000u64);
        assert_eq!(t["nac"], 0x34);
        assert_eq!(t["tsbk_rate"], 15.0);
        assert_eq!(t["pool_in_use"], 3);
        assert_eq!(t["pool_size"], 20);
        assert_eq!(t["overflow_per_s"], 0);
        assert_eq!(t["calls_active"], 2);
        assert_eq!(t["applied_version"], 4);
    }

    /// A P25-only recorder (no `[[channel]]`) gathered from a full set of
    /// live gauges, through the very builders the control loop runs.
    fn p25_only_body() -> Value {
        let m = trunkd_core::Metrics::new();
        let now = 1_788_499_730_000_i64;
        m.gauge("device.ctrl.stream_beat_ms").set(now as u64);
        m.gauge("device.ctrl.overflow_per_s").set(0);
        m.gauge("device.ctrl.rssi_dbfs").set_f64(-24.5);
        m.gauge("device.ctrl.site_stats_seen").set(1);
        m.gauge("device.ctrl.tsbk_rate").set_f64(15.25);
        m.gauge("device.ctrl.snr_valid").set(1);
        m.gauge("device.ctrl.snr_db").set_f64(18.5);
        m.gauge("device.ctrl.noise_floor_dbfs").set_f64(-61.0);
        m.gauge("site.0.control_frequency_hz").set(851_050_000);
        m.gauge("site.0.nac").set(0x34);
        m.gauge("site.0.pool_in_use").set(3);
        m.gauge("site.0.pool_size").set(20);
        m.gauge("site.0.active_calls").set(2);
        let t = gather_tuner_status(&m.snapshot(), &descriptor(), Some(&applied_snapshot()), now);
        status_body(
            "rec-01",
            &Station::default(),
            &Counters::default(),
            &[t],
            &no_hold(),
            None,
            &EventLogReport::default(),
        )
    }

    /// The tuner-status POST of a recorder with no conventional channels, byte
    /// for byte as it was before channels existed (captured on the commit that
    /// precedes them). Keyup ignores unknown fields, but a P25-only recorder
    /// must not even send new ones.
    #[test]
    fn a_p25_only_status_post_is_byte_identical_to_the_one_before_channels() {
        let got = serde_json::to_string(&p25_only_body()).expect("serialises");
        let want = P25_ONLY_STATUS_GOLDEN.replace("@VERSION@", env!("CARGO_PKG_VERSION"));
        assert_eq!(got, want);
    }

    fn channel_descriptor(index: usize, tone: Option<&str>) -> ChannelDescriptor {
        ChannelDescriptor {
            index,
            name: format!("Channel {index}"),
            system: "Example County".into(),
            frequency_hz: 154_000_000 + index as u64 * 25_000,
            tone: tone.map(str::to_owned),
            talkgroup: 154_000 + index as u32 * 25,
            metric_prefix: format!("channel.{index}"),
        }
    }

    /// A tuner carrying conventional channels reports each one off its
    /// `channel.{i}.*` gauges, as an additive `channels` array on its entry.
    #[test]
    fn a_tuner_with_channels_reports_them_from_their_gauges() {
        let m = trunkd_core::Metrics::new();
        let now = 1_788_499_730_000_i64;
        m.gauge("device.ctrl.stream_beat_ms").set(now as u64);
        m.gauge("channel.0.state").set(3);
        m.gauge("channel.0.open").set(1);
        m.gauge("channel.0.snr_db").set_f64(24.25);
        m.gauge("channel.0.power_dbfs").set_f64(-40.0);
        m.gauge("channel.0.noise_floor_dbfs").set_f64(-64.25);
        m.gauge("channel.0.offset_hz").set_f64(-120.5);
        m.gauge("channel.0.calls").set(9);
        m.gauge("channel.0.calls_too_short").set(2);
        m.gauge("channel.0.tone_mismatch").set(3);
        m.gauge("channel.0.false_opens").set(4);
        m.gauge("channel.0.last_call_end_ms").set(1_788_499_725_000);
        m.gauge("channel.1.state").set(1);
        let d = TunerDescriptor {
            channels: vec![
                channel_descriptor(0, Some("167.9")),
                channel_descriptor(1, None),
            ],
            ..descriptor()
        };
        let t = gather_tuner_status(&m.snapshot(), &d, Some(&applied_snapshot()), now);
        let body = status_body(
            "rec-01",
            &Station::default(),
            &Counters::default(),
            &[t],
            &no_hold(),
            None,
            &EventLogReport::default(),
        );
        let ch = &body["tuners"][0]["channels"];
        assert_eq!(
            ch[0],
            json!({
                "index": 0, "name": "Channel 0", "system": "Example County",
                "frequency_hz": 154_000_000_u64, "tone": "167.9", "talkgroup": 154_000,
                "state": "hang", "open": true, "snr_db": 24.25, "power_dbfs": -40.0,
                "noise_floor_dbfs": -64.25, "offset_hz": -120.5, "calls": 9,
                "calls_too_short": 2, "tone_mismatch": 3, "false_opens": 4,
                "last_call_end_ms": 1_788_499_725_000_i64,
            })
        );
        // Carrier squelch, never a call yet: `tone` is null, no offset or
        // last-call time is invented.
        assert_eq!(ch[1]["tone"], Value::Null);
        assert_eq!(ch[1]["state"], "opening");
        assert_eq!(ch[1]["open"], false);
        assert!(ch[1].get("offset_hz").is_none(), "{ch}");
        assert!(ch[1].get("last_call_end_ms").is_none(), "{ch}");
        // Everything else on the body is untouched: removing the field gives
        // back the P25-only document.
        let mut stripped = body.clone();
        stripped["tuners"][0]
            .as_object_mut()
            .expect("a tuner object")
            .remove("channels");
        let mut p25 = p25_only_body();
        // The golden fixture seeds signal-quality gauges this one does not.
        for k in ["rssi_dbfs", "snr_db", "noise_floor_dbfs"] {
            p25["tuners"][0].as_object_mut().expect("tuner").remove(k);
        }
        p25["tuners"][0]["tsbk_rate"] = json!(0.0);
        for k in [
            "control_hz",
            "nac",
            "pool_in_use",
            "pool_size",
            "calls_active",
        ] {
            p25["tuners"][0][k] = json!(0);
        }
        p25["tuners"][0]["locked"] = json!(false);
        assert_eq!(stripped, p25);
    }

    /// A stopped or disabled tuner's channels are `idle` and measure nothing,
    /// whatever its last published gauges say; the counters and the last
    /// call's offset are history, and stay.
    #[test]
    fn a_stale_tuner_reports_its_channels_idle_and_unmeasured() {
        let m = trunkd_core::Metrics::new();
        let now = 1_788_499_730_000_i64;
        m.gauge("device.ctrl.stream_beat_ms")
            .set((now - 60_000) as u64);
        m.gauge("channel.0.state").set(2);
        m.gauge("channel.0.snr_db").set_f64(24.0);
        m.gauge("channel.0.offset_hz").set_f64(80.0);
        m.gauge("channel.0.calls").set(5);
        let d = TunerDescriptor {
            channels: vec![channel_descriptor(0, Some("D445N"))],
            ..descriptor()
        };
        let t = gather_tuner_status(&m.snapshot(), &d, Some(&applied_snapshot()), now);
        let c = &t.channels[0];
        assert_eq!(c.state, "idle");
        assert_eq!(c.snr_db, None);
        assert_eq!(c.power_dbfs, None);
        assert_eq!(c.offset_hz, Some(80.0));
        assert_eq!(c.calls, 5);

        // Live but disabled by keyup: idle too.
        m.gauge("device.ctrl.stream_beat_ms").set(now as u64);
        let mut a = applied_snapshot();
        a.config.enabled = false;
        let t = gather_tuner_status(&m.snapshot(), &d, Some(&a), now);
        assert_eq!(t.channels[0].state, "idle");
    }

    #[test]
    fn the_channels_array_is_capped_and_names_are_truncated() {
        let t = TunerStatus {
            tuner_key: "ctrl".into(),
            channels: (0..MAX_CHANNELS_PER_TUNER + 3)
                .map(|i| ChannelStatus {
                    index: i,
                    name: "n".repeat(MAX_CHANNEL_NAME_LEN + 10),
                    state: "closed",
                    ..ChannelStatus::default()
                })
                .collect(),
            ..TunerStatus::default()
        };
        let body = status_body(
            "i",
            &Station::default(),
            &Counters::default(),
            &[t],
            &no_hold(),
            None,
            &EventLogReport::default(),
        );
        let ch = body["tuners"][0]["channels"].as_array().expect("channels");
        assert_eq!(ch.len(), MAX_CHANNELS_PER_TUNER);
        assert_eq!(
            ch[0]["name"].as_str().map(|n| n.chars().count()),
            Some(MAX_CHANNEL_NAME_LEN)
        );
    }

    const P25_ONLY_STATUS_GOLDEN: &str = r#"{"capabilities":{"recorder":{"restart_on":[]}},"counters":{"calls_discarded_empty":0,"calls_ended":0,"calls_skipped_by_policy":0,"calls_started":0,"calls_tdma_refused":0,"disk_free_bytes":0,"failed":0,"spool_bytes":0,"spooled":0,"unit_calls_ended":0,"unit_calls_started":0,"uploaded":0},"event_log":{"archive":false,"last_archived":null,"last_error":null,"last_written_at":null,"pending":0,"source":"config"},"instance_id":"rec-01","recorder_version":"@VERSION@","tuners":[{"agc":null,"amp":false,"applied_version":4,"auto_ppm":true,"auto_ppm_limit":10.0,"backend":"hackrf","bandwidth_hz":null,"calls_active":2,"capabilities":{"restart_on":["role","site","site_id","center_frequency","bands","enabled","ppm","auto_ppm","auto_ppm_limit","data_follow","remote_grants","control_frequencies","sample_rate","lna_db","vga_db","amp"],"runtime_gain":false,"runtime_ppm":false},"center_hz":855500000,"clock_error_ms":0,"clock_resyncs":0,"clock_trim_ms":0,"control_frequencies":[],"control_hz":851050000,"data_follow":false,"description":{"receiver":"HackRF One"},"enabled":true,"gain":null,"gain_elements":[],"gains":{},"last_clock_resync_ms":0,"lna_db":24,"locked":true,"nac":52,"noise_floor_dbfs":-61.0,"overflow_per_s":0,"overrides":[],"pool_in_use":3,"pool_size":20,"ppm":0.0,"ppm_memory":null,"remote_grants":false,"role":"control","rssi_dbfs":-24.5,"rx_port":null,"sample_rate":20000000,"serial":null,"settings":{},"site":"Central","site_id":"7","snr_db":18.5,"span_hz":19975000,"spectrum":false,"stream_gaps":0,"tsbk_rate":15.25,"tuner_key":"ctrl","vga_db":20}],"uplink":{"circuit_open":false,"quarantined":0,"source":"config","spool_bytes":0,"spool_only":false,"spooled_calls":0,"spooled_on_pressure":0,"stream":{"connected":false,"enabled":true,"last_error":null,"last_notify_at":null,"reconnects":0,"since":null},"url":"http://keyup.example/api/call-upload"}}"#;

    #[test]
    fn recorder_version_is_the_software_version_string() {
        // keyup's contract types recorder_version as a string (the recorder software
        // version), so it must serialize as a string, never an int, or keyup 422s.
        let body = status_body(
            "rec-01",
            &Station::default(),
            &Counters::default(),
            &[],
            &no_hold(),
            None,
            &EventLogReport::default(),
        );
        assert!(body["recorder_version"].is_string());
        assert_eq!(body["recorder_version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(body["tuners"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn status_body_respects_the_tuner_and_key_caps() {
        let mut many = Vec::new();
        for i in 0..(MAX_TUNERS + 5) {
            many.push(TunerStatus {
                tuner_key: format!("tuner-{i}"),
                ..TunerStatus::default()
            });
        }
        let body = status_body(
            "i",
            &Station::default(),
            &Counters::default(),
            &many,
            &no_hold(),
            None,
            &EventLogReport::default(),
        );
        assert_eq!(body["tuners"].as_array().unwrap().len(), MAX_TUNERS);

        let long_key = TunerStatus {
            tuner_key: "z".repeat(MAX_TUNER_KEY_LEN + 20),
            ..TunerStatus::default()
        };
        let body = status_body(
            "i",
            &Station::default(),
            &Counters::default(),
            &[long_key],
            &no_hold(),
            None,
            &EventLogReport::default(),
        );
        assert_eq!(
            body["tuners"][0]["tuner_key"]
                .as_str()
                .unwrap()
                .chars()
                .count(),
            MAX_TUNER_KEY_LEN
        );
    }

    #[test]
    fn instance_id_is_percent_encoded_in_the_query() {
        assert_eq!(percent_encode("rec north/01"), "rec%20north%2F01");
        assert_eq!(percent_encode("plain-id_1.0~"), "plain-id_1.0~");
    }

    #[test]
    fn an_off_grid_rate_reports_a_zero_span() {
        // 3 MHz is not a multiple of the 12.5 kHz bin grid usably; the safe
        // report is a zero span so keyup places nothing.
        assert_eq!(span_hz_for(0), 0);
        assert!(span_hz_for(20_000_000) > 0);
    }

    // -- status gathering: overflow read + liveness gate --------------------

    fn descriptor() -> TunerDescriptor {
        TunerDescriptor {
            id: "ctrl".into(),
            serial: None,
            backend: Backend::Hackrf,
            driver: None,
            sample_rate: 20_000_000,
            site_label: Some("Central".into()),
            site_metric_prefix: Some("site.0".into()),
            description: TunerDescription::default(),
            channels: Vec::new(),
        }
    }

    fn applied_snapshot() -> AppliedSnapshot {
        AppliedSnapshot {
            config: AppliedConfig {
                role: Some(DeviceRole::Control),
                site: Some("Central".into()),
                site_id: Some("7".into()),
                center_frequency: Some(855_500_000),
                bands: None,
                enabled: true,
                lna_db: Some(24),
                vga_db: Some(20),
                amp: Some(false),
                ppm: Some(0.0),
                auto_ppm: Some(true),
                auto_ppm_limit: Some(10.0),
                sample_rate: Some(20_000_000),
                ..AppliedConfig::default()
            },
            version: 4,
            spectrum: false,
            gain_elements: Vec::new(),
            rejected: Vec::new(),
            backend: Backend::Hackrf,
            overrides: Vec::new(),
            file_role: DeviceRole::Control,
            fault: None,
        }
    }

    #[test]
    fn overflow_per_s_is_read_from_the_config_id_gauge() {
        // The ring counter is keyed by the backend device id; the DSP thread
        // republishes the rate under the *config* id, which is what we read.
        let m = trunkd_core::Metrics::new();
        m.gauge("device.ctrl.overflow_per_s").set(4200);
        let now = trunkd_core::Timestamp::now().unix_millis();
        m.gauge("device.ctrl.stream_beat_ms").set(now as u64);
        let d = descriptor();
        let a = applied_snapshot();
        let t = gather_tuner_status(&m.snapshot(), &d, Some(&a), now);
        assert_eq!(t.overflow_per_s, 4200);
    }

    #[test]
    fn the_status_carries_the_stream_clock_health() {
        // The whole point of these three: an operator looking at the panel
        // after a run of early timestamps can see that the recorder noticed,
        // when, and by how much.
        let m = trunkd_core::Metrics::new();
        let now = trunkd_core::Timestamp::now().unix_millis();
        m.gauge("device.ctrl.stream_beat_ms").set(now as u64);
        m.counter("device.ctrl.clock_resyncs").add(2);
        m.gauge("device.ctrl.last_clock_resync_ms").set(11_600);
        m.counter("device.ctrl.stream_gaps").add(7);
        let d = descriptor();
        let a = applied_snapshot();
        let t = gather_tuner_status(&m.snapshot(), &d, Some(&a), now);
        assert_eq!(t.clock_resyncs, 2);
        assert_eq!(t.last_clock_resync_ms, 11_600);
        assert_eq!(t.stream_gaps, 7);

        let body = status_body(
            "rec-01",
            &Station::default(),
            &Counters::default(),
            &[t],
            &no_hold(),
            None,
            &EventLogReport::default(),
        );
        let j = &body["tuners"][0];
        assert_eq!(j["clock_resyncs"], 2);
        assert_eq!(j["last_clock_resync_ms"], 11_600);
        assert_eq!(j["stream_gaps"], 7);
    }

    #[test]
    fn a_backwards_resync_is_reported_as_a_negative_correction() {
        // The gauge holds two's-complement bits; a correction that moved the
        // epoch *back* must not come out of the panel as 18 exabytes.
        let m = trunkd_core::Metrics::new();
        let now = trunkd_core::Timestamp::now().unix_millis();
        m.gauge("device.ctrl.stream_beat_ms").set(now as u64);
        m.gauge("device.ctrl.last_clock_resync_ms")
            .set(-4_000i64 as u64);
        let d = descriptor();
        let a = applied_snapshot();
        let t = gather_tuner_status(&m.snapshot(), &d, Some(&a), now);
        assert_eq!(t.last_clock_resync_ms, -4_000);
    }

    #[test]
    fn the_slow_trim_is_reported_signed() {
        // A fast sample clock is trimmed backwards; both gauges hold
        // two's-complement bits and both must come out negative.
        let m = trunkd_core::Metrics::new();
        let now = trunkd_core::Timestamp::now().unix_millis();
        m.gauge("device.ctrl.stream_beat_ms").set(now as u64);
        m.gauge("device.ctrl.clock_error_ms").set(-12i64 as u64);
        m.gauge("device.ctrl.clock_trim_ms").set(-1_430i64 as u64);
        let d = descriptor();
        let a = applied_snapshot();
        let t = gather_tuner_status(&m.snapshot(), &d, Some(&a), now);
        assert_eq!((t.clock_error_ms, t.clock_trim_ms), (-12, -1_430));
    }

    #[test]
    fn a_live_tuner_with_control_reports_locked() {
        let m = trunkd_core::Metrics::new();
        let now = trunkd_core::Timestamp::now().unix_millis();
        m.gauge("device.ctrl.stream_beat_ms").set(now as u64);
        m.gauge("site.0.control_frequency_hz").set(851_050_000);
        m.gauge("site.0.nac").set(0x34);
        let d = descriptor();
        let a = applied_snapshot();
        let t = gather_tuner_status(&m.snapshot(), &d, Some(&a), now);
        assert!(t.locked, "a live tuner with a control lock is locked");
        assert_eq!(t.control_hz, 851_050_000);
        assert_eq!(t.nac, 0x34);
        assert_eq!(t.site_id.as_deref(), Some("7"));
    }

    #[test]
    fn a_dead_tuner_does_not_report_locked_despite_stale_gauges() {
        // The site gauges linger after the unit dies; a stale heartbeat must
        // zero the reported lock so keyup does not trust a dead tuner.
        let m = trunkd_core::Metrics::new();
        let now = trunkd_core::Timestamp::now().unix_millis();
        // Heartbeat well past the staleness window (or never set).
        m.gauge("device.ctrl.stream_beat_ms")
            .set((now - STREAM_STALE_MS - 1_000) as u64);
        m.gauge("site.0.control_frequency_hz").set(851_050_000);
        m.gauge("site.0.nac").set(0x34);
        let d = descriptor();
        let a = applied_snapshot();
        let t = gather_tuner_status(&m.snapshot(), &d, Some(&a), now);
        assert!(!t.locked, "a dead tuner must not report locked");
        assert_eq!(t.control_hz, 0, "and its stale control frequency is zeroed");
        assert_eq!(t.nac, 0);
        // The applied config it was built with is still reported.
        assert_eq!(t.center_hz, 855_500_000);
        assert_eq!(t.applied_version, 4);
    }

    // -- station self-description, counters, link quality -------------------

    fn sample_station() -> Station {
        Station {
            name: "Example hilltop".into(),
            location: "Springfield".into(),
            latitude: Some(40.1234),
            longitude: Some(-100.5678),
            elevation_m: Some(320.0),
            contact: "N0CALL".into(),
            notes: "mast down until Friday".into(),
        }
    }

    #[test]
    fn a_configured_station_is_reported_verbatim() {
        let body = status_body(
            "rec-01",
            &sample_station(),
            &Counters::default(),
            &[sample_status()],
            &no_hold(),
            None,
            &EventLogReport::default(),
        );
        let st = &body["station"];
        assert_eq!(st["name"], "Example hilltop");
        assert_eq!(st["location"], "Springfield");
        assert_eq!(st["latitude"], 40.1234);
        assert_eq!(st["longitude"], -100.5678);
        assert_eq!(st["elevation_m"], 320.0);
        assert_eq!(st["contact"], "N0CALL");
        assert_eq!(st["notes"], "mast down until Friday");
    }

    #[test]
    fn an_undescribed_station_sends_no_station_object() {
        // Absent, not an object of empty strings: keyup can then tell "not
        // described" from "described as nothing".
        let body = status_body(
            "rec-01",
            &Station::default(),
            &Counters::default(),
            &[sample_status()],
            &no_hold(),
            None,
            &EventLogReport::default(),
        );
        assert!(body.get("station").is_none(), "body was {body}");
    }

    #[test]
    fn a_partly_filled_station_omits_the_fields_left_blank() {
        let station = Station {
            name: "Shack".into(),
            ..Station::default()
        };
        let body = status_body(
            "rec-01",
            &station,
            &Counters::default(),
            &[],
            &no_hold(),
            None,
            &EventLogReport::default(),
        );
        assert_eq!(body["station"]["name"], "Shack");
        assert!(body["station"].get("location").is_none());
        assert!(body["station"].get("latitude").is_none());
    }

    #[test]
    fn counters_come_from_the_uplink_prefix_when_delivering() {
        let m = trunkd_core::Metrics::new();
        m.counter("calls.started").add(12);
        m.counter("calls.ended").add(11);
        m.counter("uplink.uploaded").add(9);
        // A duplicate is a call keyup already has: delivered, not lost.
        m.counter("uplink.duplicates").add(2);
        m.counter("uplink.spooled").add(3);
        m.counter("uplink.failed").add(1);
        m.gauge("uplink.spool_bytes").set(4096);
        m.gauge("uplink.disk_free_bytes").set(123_456);
        m.counter("uplink.spooled_on_pressure").add(2);
        m.counter("uplink.spool_quarantined").add(1);
        m.gauge("uplink.circuit_open").set(1);
        m.counter("calls.discarded_empty").add(1);
        m.counter("calls.unit_started").add(2);
        m.counter("calls.unit_ended").add(2);
        m.counter("calls.skipped_by_policy").add(4);
        m.counter("calls.tdma_refused").add(6);
        let c = gather_counters(&m.snapshot());
        assert_eq!(
            c,
            Counters {
                calls_started: 12,
                calls_ended: 11,
                calls_discarded_empty: 1,
                unit_calls_started: 2,
                unit_calls_ended: 2,
                calls_skipped_by_policy: 4,
                calls_tdma_refused: 6,
                uploaded: 11,
                spooled: 3,
                failed: 1,
                spool_bytes: 4096,
                disk_free_bytes: 123_456,
                spooled_on_pressure: 2,
                quarantined: 1,
                circuit_open: true,
            }
        );
        let body = status_body(
            "rec-01",
            &Station::default(),
            &c,
            &[],
            &no_hold(),
            None,
            &EventLogReport::default(),
        );
        assert_eq!(body["counters"]["calls_started"], 12);
        // The traffic this recorder heard and deliberately did not record.
        assert_eq!(body["counters"]["calls_skipped_by_policy"], 4);
        // Phase 2 calls heard and not followed.
        assert_eq!(body["counters"]["calls_tdma_refused"], 6);
        assert_eq!(body["counters"]["calls_discarded_empty"], 1);
        assert_eq!(body["counters"]["unit_calls_started"], 2);
        assert_eq!(body["counters"]["unit_calls_ended"], 2);
        assert_eq!(body["counters"]["uploaded"], 11);
        assert_eq!(body["counters"]["disk_free_bytes"], 123_456);
    }

    #[test]
    fn counters_fall_back_to_the_spool_prefix_in_spool_only_mode() {
        // Nothing registers `uplink.*` when no uploader runs, so the prefix
        // that exists is the mode signal.
        let m = trunkd_core::Metrics::new();
        m.counter("calls.started").add(5);
        m.counter("spool.stored").add(5);
        m.counter("spool.failed").add(1);
        m.gauge("spool.bytes").set(8192);
        m.gauge("spool.disk_free_bytes").set(999);
        let c = gather_counters(&m.snapshot());
        assert_eq!(c.uploaded, 0, "spool-only delivers nothing");
        assert_eq!(c.spooled, 5);
        assert_eq!(c.failed, 1);
        assert_eq!(c.spool_bytes, 8192);
        assert_eq!(c.disk_free_bytes, 999);
    }

    fn described() -> TunerDescriptor {
        TunerDescriptor {
            description: TunerDescription {
                receiver: String::new(),
                antenna: "Diamond X50 @ 8m".into(),
                cable: "25 m LMR-400".into(),
                amplifier: String::new(),
                height_m: Some(8.0),
                notes: String::new(),
            },
            ..descriptor()
        }
    }

    #[test]
    fn a_blank_receiver_falls_back_to_the_backend_name() {
        let m = trunkd_core::Metrics::new();
        let now = trunkd_core::Timestamp::now().unix_millis();
        m.gauge("device.ctrl.stream_beat_ms").set(now as u64);
        let t = gather_tuner_status(&m.snapshot(), &described(), Some(&applied_snapshot()), now);
        assert_eq!(t.description.receiver, "HackRF One");
        assert_eq!(default_receiver(Backend::Soapy, None), "SoapySDR");
        assert_eq!(default_receiver(Backend::Iqfile, None), "IQ file");
        assert_eq!(default_receiver(Backend::Soapy, Some("airspy")), "Airspy");
        assert_eq!(default_receiver(Backend::Soapy, Some("SDRplay")), "SDRplay");
        assert_eq!(default_receiver(Backend::Soapy, Some("rtlsdr")), "RTL-SDR");
        assert_eq!(
            default_receiver(Backend::Soapy, Some("uhd")),
            "SoapySDR uhd"
        );
        // The driver only names a Soapy tuner; it never overrides another backend.
        assert_eq!(
            default_receiver(Backend::Hackrf, Some("airspy")),
            "HackRF One"
        );
    }

    #[test]
    fn description_and_settings_reach_the_body() {
        let m = trunkd_core::Metrics::new();
        let now = trunkd_core::Timestamp::now().unix_millis();
        m.gauge("device.ctrl.stream_beat_ms").set(now as u64);
        let t = gather_tuner_status(&m.snapshot(), &described(), Some(&applied_snapshot()), now);
        let body = status_body(
            "rec-01",
            &Station::default(),
            &Counters::default(),
            &[t],
            &no_hold(),
            None,
            &EventLogReport::default(),
        );
        let d = &body["tuners"][0]["description"];
        assert_eq!(d["receiver"], "HackRF One");
        assert_eq!(d["antenna"], "Diamond X50 @ 8m");
        assert_eq!(d["cable"], "25 m LMR-400");
        assert_eq!(d["height_m"], 8.0);
        assert!(d.get("amplifier").is_none(), "blank fields are omitted");
        assert!(d.get("notes").is_none());
        // The effective settings are flat on the tuner, so a GUI reads one
        // record: what the tuner is running, whoever set it.
        let t = &body["tuners"][0];
        assert_eq!(t["lna_db"], 24);
        assert_eq!(t["vga_db"], 20);
        assert_eq!(t["amp"], false);
        assert_eq!(t["gain"], Value::Null, "an unset overall gain reports null");
        assert_eq!(t["ppm"], 0.0);
        assert_eq!(t["auto_ppm"], true);
        assert_eq!(t["sample_rate"], 20_000_000);
        assert_eq!(t["gains"], json!({}));
        assert_eq!(t["control_frequencies"], json!([]));
        assert_eq!(t["gain_elements"], json!([]), "a HackRF names no elements");
        // And the capability table says what a change costs, before it is made.
        assert_eq!(t["capabilities"]["runtime_gain"], false);
        assert_eq!(t["capabilities"]["runtime_ppm"], false);
        assert!(t["capabilities"]["restart_on"]
            .as_array()
            .expect("restart_on is a list")
            .contains(&Value::from("lna_db")));
        assert!(t.get("rejected").is_none(), "nothing was rejected");
    }

    #[test]
    fn a_board_with_a_remembered_ppm_reports_it_and_whether_it_was_applied() {
        let m = trunkd_core::Metrics::new();
        let now = trunkd_core::Timestamp::now().unix_millis();
        m.gauge("device.ctrl.ppm_memory_ppm").set_f64(1.25);
        m.gauge("device.ctrl.ppm_memory_at_ms")
            .set(1_700_000_000_123);
        m.gauge("device.ctrl.ppm_memory_at_open").set(1);
        // Deliberately *not* live: what a board remembers is true whether or
        // not it is streaming, and a tuner that just died is exactly when an
        // operator wants the number it learned.
        let t = gather_tuner_status(&m.snapshot(), &descriptor(), Some(&applied_snapshot()), now);
        let mem = t.ppm_memory.clone().expect("the board is remembered");
        assert_eq!(mem.ppm, 1.25);
        assert_eq!(mem.measured_at, "2023-11-14T22:13:20.123Z");
        assert!(mem.applied_at_open);
        assert!(!t.locked, "and the memory does not fake a lock");

        let body = status_body(
            "rec-01",
            &Station::default(),
            &Counters::default(),
            &[t],
            &no_hold(),
            None,
            &EventLogReport::default(),
        );
        assert_eq!(
            body["tuners"][0]["ppm_memory"],
            json!({
                "ppm": 1.25,
                "measured_at": "2023-11-14T22:13:20.123Z",
                "applied_at_open": true,
            })
        );
    }

    #[test]
    fn a_remembered_ppm_that_was_not_applied_still_reports() {
        // `auto_ppm` off: the operator owns `ppm`, so the memory is shown and
        // not used — that number is what they would type into the TOML.
        let m = trunkd_core::Metrics::new();
        let now = trunkd_core::Timestamp::now().unix_millis();
        m.gauge("device.ctrl.ppm_memory_ppm").set_f64(-0.75);
        m.gauge("device.ctrl.ppm_memory_at_ms")
            .set(1_700_000_000_123);
        m.gauge("device.ctrl.ppm_memory_at_open").set(0);
        let t = gather_tuner_status(&m.snapshot(), &descriptor(), Some(&applied_snapshot()), now);
        let mem = t.ppm_memory.clone().expect("remembered but unapplied");
        assert_eq!(mem.ppm, -0.75);
        assert!(!mem.applied_at_open);
    }

    #[test]
    fn a_board_never_measured_reports_a_null_memory_rather_than_omitting_it() {
        let m = trunkd_core::Metrics::new();
        let now = trunkd_core::Timestamp::now().unix_millis();
        let t = gather_tuner_status(&m.snapshot(), &descriptor(), Some(&applied_snapshot()), now);
        assert_eq!(t.ppm_memory, None);
        let body = status_body(
            "rec-01",
            &Station::default(),
            &Counters::default(),
            &[t],
            &no_hold(),
            None,
            &EventLogReport::default(),
        );
        // Null, not absent: a GUI drawing a ppm control has to tell "never
        // measured" apart from "this recorder is too old to say".
        assert_eq!(body["tuners"][0]["ppm_memory"], Value::Null);
    }

    #[test]
    fn an_operator_ppm_push_overwrites_what_the_recorder_measured() {
        let dir = std::env::temp_dir().join(format!("trunkd-ppm-push-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("ppm-memory.json");
        let d = TunerDescriptor {
            serial: Some("0000cafe".into()),
            ..descriptor()
        };
        let board = "hackrf:0000cafe";

        // Two corrections the loop measured for itself.
        crate::pipeline::ppmmemory::record(
            &path,
            board,
            1.25,
            0.0,
            crate::pipeline::ppmmemory::Samples::Bump,
        );
        crate::pipeline::ppmmemory::record(
            &path,
            board,
            1.30,
            0.0,
            crate::pipeline::ppmmemory::Samples::Bump,
        );

        reset_ppm_memory(&path, &d, -3.5);
        let e = crate::pipeline::ppmmemory::read(&path)
            .expect("read")
            .get(board)
            .cloned()
            .expect("kept");
        assert_eq!(e.ppm, -3.5);
        assert_eq!(
            e.config_ppm, -3.5,
            "the operator's number IS the config ppm"
        );
        assert_eq!(e.samples, 0, "the measurement history behind it is gone");
        assert_eq!(
            e.delta(),
            0.0,
            "so the next open applies it and nothing else"
        );

        // An IQ file has no board to remember, so nothing is written for one.
        let file = TunerDescriptor {
            id: "replay".into(),
            serial: None,
            backend: Backend::Iqfile,
            ..descriptor()
        };
        reset_ppm_memory(&path, &file, 9.0);
        assert!(crate::pipeline::ppmmemory::read(&path)
            .expect("read")
            .get("iqfile:replay")
            .is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_undescribed_tuner_sends_no_description_block() {
        // The body a bare recorder posts is exactly what it posted before this
        // wave, plus the counters.
        let body = status_body(
            "rec-01",
            &Station::default(),
            &Counters::default(),
            &[sample_status()],
            &no_hold(),
            None,
            &EventLogReport::default(),
        );
        let t = &body["tuners"][0];
        assert!(t.get("description").is_none());
        assert!(t.get("rssi_dbfs").is_none());
        assert!(t.get("freq_error_hz").is_none());
        assert!(t.get("rejected").is_none());
        // The settings block is always present, because "what is this tuner
        // running?" always has an answer — including "nothing is set", which
        // an absent field could not distinguish from "not reported".
        for key in ["gain", "gains", "agc", "ppm", "auto_ppm", "sample_rate"] {
            assert!(t.get(key).is_some(), "{key} is always reported");
        }
        assert_eq!(t["ppm"], 0.0);
    }

    #[test]
    fn a_live_tuner_reports_measured_rssi_and_frequency_error() {
        // `freq_error_hz` is the carrier offset, which is the measurement:
        // the PLL's phase error rides alongside as `pll_error_hz`, under its
        // own name, because it is not one.
        let m = trunkd_core::Metrics::new();
        let now = trunkd_core::Timestamp::now().unix_millis();
        m.gauge("device.ctrl.stream_beat_ms").set(now as u64);
        m.gauge("device.ctrl.rssi_dbfs").set_f64(-42.5);
        m.gauge("site.0.control_frequency_error_hz").set_f64(422.0);
        m.gauge("site.0.control_carrier_offset_hz").set_f64(-137.25);
        m.gauge("site.0.control_carrier_offset_valid").set(1);
        let t = gather_tuner_status(&m.snapshot(), &descriptor(), Some(&applied_snapshot()), now);
        assert_eq!(t.rssi_dbfs, Some(-42.5));
        assert_eq!(t.freq_error_hz, Some(-137.25));
        assert_eq!(t.pll_error_hz, Some(422.0));
        let body = status_body(
            "rec-01",
            &Station::default(),
            &Counters::default(),
            &[t],
            &no_hold(),
            None,
            &EventLogReport::default(),
        );
        assert_eq!(body["tuners"][0]["rssi_dbfs"], -42.5);
        assert_eq!(body["tuners"][0]["freq_error_hz"], -137.25);
        assert_eq!(body["tuners"][0]["pll_error_hz"], 422.0);
    }

    /// Every gauge a locked, healthy tuner publishes for itself, at values that
    /// cannot be confused with the site-keyed ones the test also sets.
    fn per_tuner_gauges(m: &trunkd_core::Metrics) {
        m.gauge("device.ctrl.site_stats_seen").set(1);
        m.gauge("device.ctrl.tsbk_rate").set_f64(31.5);
        m.gauge("device.ctrl.freq_error_hz").set_f64(-12.0);
        m.gauge("device.ctrl.freq_error_valid").set(1);
        m.gauge("device.ctrl.pll_error_hz").set_f64(88.0);
        m.gauge("device.ctrl.snr_db").set_f64(21.5);
        m.gauge("device.ctrl.snr_valid").set(1);
        m.gauge("device.ctrl.noise_floor_dbfs").set_f64(-71.25);
        m.gauge("device.ctrl.tsbk_pass_pct").set_f64(99.5);
        m.gauge("device.ctrl.tsbk_pass_valid").set(1);
        m.gauge("device.ctrl.sync_losses_per_min").set_f64(1.2);
    }

    /// The whole point of the device-keyed gauges: two tuners on one site share
    /// every `site.{index}.*` handle, so the last stats window routed wins and
    /// the panel drew one tuner's numbers on both rows. A tuner's own gauges
    /// take precedence over the shared ones wherever it has published them.
    #[test]
    fn a_tuners_own_gauges_win_over_the_shared_site_ones() {
        let m = trunkd_core::Metrics::new();
        let now = trunkd_core::Timestamp::now().unix_millis();
        m.gauge("device.ctrl.stream_beat_ms").set(now as u64);
        m.gauge("site.0.control_frequency_hz").set(851_050_000);
        // The other tuner on this site got here last.
        m.gauge("site.0.tsbk_rate").set_f64(4.0);
        m.gauge("site.0.control_frequency_error_hz").set_f64(422.0);
        m.gauge("site.0.control_carrier_offset_hz").set_f64(-137.25);
        m.gauge("site.0.control_carrier_offset_valid").set(1);
        per_tuner_gauges(&m);

        let t = gather_tuner_status(&m.snapshot(), &descriptor(), Some(&applied_snapshot()), now);
        assert_eq!(t.tsbk_rate, 31.5);
        assert_eq!(t.freq_error_hz, Some(-12.0));
        assert_eq!(t.pll_error_hz, Some(88.0));
        assert_eq!(t.snr_db, Some(21.5));
        assert_eq!(t.noise_floor_dbfs, Some(-71.25));
        assert_eq!(t.tsbk_pass_pct, Some(99.5));
        assert_eq!(t.sync_losses_per_min, Some(1.2));

        let body = status_body(
            "rec-01",
            &Station::default(),
            &Counters::default(),
            &[t],
            &no_hold(),
            None,
            &EventLogReport::default(),
        );
        let tuner = &body["tuners"][0];
        assert_eq!(tuner["tsbk_rate"], 31.5);
        assert_eq!(tuner["snr_db"], 21.5);
        assert_eq!(tuner["noise_floor_dbfs"], -71.25);
        assert_eq!(tuner["tsbk_pass_pct"], 99.5);
        assert_eq!(tuner["sync_losses_per_min"], 1.2);
    }

    /// Nothing regresses on a recorder whose tuners have not completed a stats
    /// window yet (or on the shutdown path, where the device gauges stop): the
    /// site-keyed values are still reported, and the four new figures are
    /// simply absent rather than zeroed.
    #[test]
    fn the_site_gauges_are_the_fallback_until_a_tuner_publishes_its_own() {
        let m = trunkd_core::Metrics::new();
        let now = trunkd_core::Timestamp::now().unix_millis();
        m.gauge("device.ctrl.stream_beat_ms").set(now as u64);
        m.gauge("site.0.control_frequency_hz").set(851_050_000);
        m.gauge("site.0.tsbk_rate").set_f64(4.0);
        m.gauge("site.0.control_frequency_error_hz").set_f64(422.0);
        m.gauge("site.0.control_carrier_offset_hz").set_f64(-137.25);
        m.gauge("site.0.control_carrier_offset_valid").set(1);

        let t = gather_tuner_status(&m.snapshot(), &descriptor(), Some(&applied_snapshot()), now);
        assert_eq!(t.tsbk_rate, 4.0);
        assert_eq!(t.freq_error_hz, Some(-137.25));
        assert_eq!(t.pll_error_hz, Some(422.0));
        assert_eq!(t.snr_db, None);
        assert_eq!(t.noise_floor_dbfs, None);
        assert_eq!(t.tsbk_pass_pct, None);
        assert_eq!(t.sync_losses_per_min, None);

        let body = status_body(
            "rec-01",
            &Station::default(),
            &Counters::default(),
            &[t],
            &no_hold(),
            None,
            &EventLogReport::default(),
        );
        for key in [
            "snr_db",
            "noise_floor_dbfs",
            "tsbk_pass_pct",
            "sync_losses_per_min",
        ] {
            assert!(
                body["tuners"][0].get(key).is_none(),
                "{key} is absent, never null, when it was not measured"
            );
        }
    }

    /// A hunting tuner has no channel to measure and a stale tuner's readings
    /// describe a receiver that is no longer listening. Both report nothing
    /// rather than the last number they happened to hold.
    #[test]
    fn signal_quality_needs_a_lock_and_a_live_tuner() {
        let now = trunkd_core::Timestamp::now().unix_millis();

        // Hunting: the flags are what the router clears, so they are what the
        // status path reads.
        let m = trunkd_core::Metrics::new();
        m.gauge("device.ctrl.stream_beat_ms").set(now as u64);
        per_tuner_gauges(&m);
        m.gauge("device.ctrl.snr_valid").set(0);
        m.gauge("device.ctrl.tsbk_pass_valid").set(0);
        m.gauge("device.ctrl.freq_error_valid").set(0);
        let t = gather_tuner_status(&m.snapshot(), &descriptor(), Some(&applied_snapshot()), now);
        assert!(!t.locked, "no control frequency gauge, so no lock");
        assert_eq!(t.snr_db, None);
        assert_eq!(t.noise_floor_dbfs, None);
        assert_eq!(t.tsbk_pass_pct, None);
        assert_eq!(t.sync_losses_per_min, None, "gated on the lock");
        assert_eq!(t.freq_error_hz, None);
        assert_eq!(t.tsbk_rate, 31.5, "the rate itself is still a measurement");

        // Stale: the heartbeat stopped, so every measurement is withdrawn.
        let m = trunkd_core::Metrics::new();
        m.gauge("device.ctrl.stream_beat_ms")
            .set((now - STREAM_STALE_MS - 1) as u64);
        m.gauge("site.0.control_frequency_hz").set(851_050_000);
        per_tuner_gauges(&m);
        let t = gather_tuner_status(&m.snapshot(), &descriptor(), Some(&applied_snapshot()), now);
        assert!(!t.locked);
        assert_eq!(t.snr_db, None);
        assert_eq!(t.noise_floor_dbfs, None);
        assert_eq!(t.tsbk_pass_pct, None);
        assert_eq!(t.sync_losses_per_min, None);
    }

    #[test]
    fn a_hunting_tuner_reports_no_carrier_offset() {
        // The carrier meter has no locked channel to measure while the site is
        // hunting, so it publishes nothing and clears its flag. A lingering
        // 0 Hz reported as a measurement would say "spot on" at exactly the
        // moment the board may be too far off to acquire — which is the
        // question being asked.
        let m = trunkd_core::Metrics::new();
        let now = trunkd_core::Timestamp::now().unix_millis();
        m.gauge("device.ctrl.stream_beat_ms").set(now as u64);
        m.gauge("site.0.control_frequency_error_hz").set_f64(422.0);
        m.gauge("site.0.control_carrier_offset_hz").set_f64(-137.25);
        m.gauge("site.0.control_carrier_offset_valid").set(0);
        let t = gather_tuner_status(&m.snapshot(), &descriptor(), Some(&applied_snapshot()), now);
        assert_eq!(t.freq_error_hz, None);
        assert_eq!(t.pll_error_hz, Some(422.0), "the PLL still has a reading");
        let body = status_body(
            "rec-01",
            &Station::default(),
            &Counters::default(),
            &[t],
            &no_hold(),
            None,
            &EventLogReport::default(),
        );
        assert!(body["tuners"][0].get("freq_error_hz").is_none());
    }

    #[test]
    fn a_stale_tuner_omits_rssi_and_frequency_error() {
        // Both are *measurements*; a gauge left behind by a tuner that died an
        // hour ago would read as a live signal report, which is precisely what
        // a source selector must not be handed. Omitted, not zeroed: 0 dBFS is
        // a real and very loud level.
        let m = trunkd_core::Metrics::new();
        let now = trunkd_core::Timestamp::now().unix_millis();
        m.gauge("device.ctrl.stream_beat_ms")
            .set((now - STREAM_STALE_MS - 1_000) as u64);
        m.gauge("device.ctrl.rssi_dbfs").set_f64(-42.5);
        m.gauge("site.0.control_carrier_offset_hz").set_f64(-137.25);
        m.gauge("site.0.control_carrier_offset_valid").set(1);
        m.gauge("site.0.control_frequency_error_hz").set_f64(422.0);
        let t = gather_tuner_status(&m.snapshot(), &descriptor(), Some(&applied_snapshot()), now);
        assert_eq!(t.rssi_dbfs, None);
        assert_eq!(t.freq_error_hz, None);
        assert_eq!(t.pll_error_hz, None);
        let body = status_body(
            "rec-01",
            &Station::default(),
            &Counters::default(),
            &[t],
            &no_hold(),
            None,
            &EventLogReport::default(),
        );
        assert!(body["tuners"][0].get("rssi_dbfs").is_none());
        assert!(body["tuners"][0].get("freq_error_hz").is_none());
    }

    /// The live bug this pins, in the shape it was found in: keyup pushed
    /// `enabled: false` at 05:28:49, the `DeviceUnit` idled a second later,
    /// and the tuner went on reporting the 25.2 TSBK/s of its last live second
    /// for the next hour — so a recorder that had stopped decoding, stopped
    /// recording and stopped writing its event log looked perfectly healthy.
    /// A rate is a measurement over a window; a closed window reports zero.
    #[test]
    fn a_tuner_that_stopped_decoding_reports_no_tsbk_rate_and_says_it_is_disabled() {
        let m = trunkd_core::Metrics::new();
        let now = trunkd_core::Timestamp::now().unix_millis();
        // The gauges its last live second left behind, site- and device-keyed.
        m.gauge("site.0.tsbk_rate").set_f64(25.2);
        m.gauge("site.0.control_frequency_hz").set(851_787_500);
        m.gauge("device.ctrl.tsbk_rate").set_f64(25.2);
        m.gauge("device.ctrl.site_stats_seen").set(1);
        m.gauge("device.ctrl.stream_beat_ms")
            .set((now - STREAM_STALE_MS - 1_000) as u64);

        let mut applied = applied_snapshot();
        applied.config.enabled = false;
        let t = gather_tuner_status(&m.snapshot(), &descriptor(), Some(&applied), now);
        assert_eq!(t.tsbk_rate, 0.0, "a stale rate is not a rate");
        assert!(!t.locked);
        assert_eq!(t.control_hz, 0);
        assert!(!t.enabled);

        let body = status_body(
            "rec-01",
            &Station::default(),
            &Counters::default(),
            &[t],
            &no_hold(),
            None,
            &EventLogReport::default(),
        );
        assert_eq!(body["tuners"][0]["tsbk_rate"], 0.0);
        // Plainly reported, so "keyup switched it off" and "it is broken" are
        // not the same picture on the panel.
        assert_eq!(body["tuners"][0]["enabled"], false);

        // A live tuner still reports its rate, and says it is enabled.
        m.gauge("device.ctrl.stream_beat_ms").set(now as u64);
        let t = gather_tuner_status(&m.snapshot(), &descriptor(), Some(&applied_snapshot()), now);
        assert_eq!(t.tsbk_rate, 25.2);
        assert!(t.enabled);
    }

    #[test]
    fn a_tuner_that_never_measured_rssi_reports_none() {
        let m = trunkd_core::Metrics::new();
        let now = trunkd_core::Timestamp::now().unix_millis();
        m.gauge("device.ctrl.stream_beat_ms").set(now as u64);
        let t = gather_tuner_status(&m.snapshot(), &descriptor(), Some(&applied_snapshot()), now);
        assert_eq!(t.rssi_dbfs, None);
    }

    #[test]
    fn only_a_control_following_tuner_reports_frequency_error() {
        // Both figures come off the control channel; a voice front does not
        // follow one and has neither number to report.
        let m = trunkd_core::Metrics::new();
        let now = trunkd_core::Timestamp::now().unix_millis();
        m.gauge("device.ctrl.stream_beat_ms").set(now as u64);
        m.gauge("site.0.control_frequency_error_hz").set_f64(12.5);
        m.gauge("site.0.control_carrier_offset_hz").set_f64(12.5);
        m.gauge("site.0.control_carrier_offset_valid").set(1);
        let voice = AppliedSnapshot {
            config: AppliedConfig {
                role: Some(DeviceRole::Voice),
                ..applied_snapshot().config
            },
            ..applied_snapshot()
        };
        let t = gather_tuner_status(&m.snapshot(), &descriptor(), Some(&voice), now);
        assert_eq!(t.freq_error_hz, None);
        assert_eq!(t.pll_error_hz, None);
        // ...but the RSSI of a voice front is still worth having.
        m.gauge("device.ctrl.rssi_dbfs").set_f64(-50.0);
        let t = gather_tuner_status(&m.snapshot(), &descriptor(), Some(&voice), now);
        assert_eq!(t.rssi_dbfs, Some(-50.0));
    }

    #[test]
    fn a_tuner_serving_no_site_reports_no_frequency_error() {
        let m = trunkd_core::Metrics::new();
        let now = trunkd_core::Timestamp::now().unix_millis();
        m.gauge("device.ctrl.stream_beat_ms").set(now as u64);
        let d = TunerDescriptor {
            site_metric_prefix: None,
            ..descriptor()
        };
        let t = gather_tuner_status(&m.snapshot(), &d, Some(&applied_snapshot()), now);
        assert_eq!(t.freq_error_hz, None);
    }

    // -- bands: parse, merge, apply, echo -----------------------------------

    fn desired_bands(version: i64, bands: Option<&[&str]>) -> DesiredTuner {
        DesiredTuner {
            version,
            role: None,
            site: None,
            site_id: None,
            center_frequency: None,
            bands: bands.map(|b| b.iter().map(|s| (*s).to_owned()).collect()),
            enabled: true,
            spectrum: false,
            ..DesiredTuner::default()
        }
    }

    #[test]
    fn a_well_formed_bands_list_is_parsed() {
        let body = r#"{
            "version": 3,
            "tuners": {
                "d0": {"version": 2, "config": {"bands": ["800", "858000000-866000000"]}},
                "d1": {"version": 2, "config": {"bands": []}},
                "d2": {"version": 2, "config": {"center_frequency": 855500000}}
            }
        }"#;
        let snap = parse_desired(body);
        let of = |k: &str| snap.tuners.iter().find(|(n, _)| n == k).unwrap().1.clone();
        assert_eq!(
            of("d0").bands,
            Some(vec!["800".to_owned(), "858000000-866000000".to_owned()])
        );
        // An empty array is an instruction ("no restriction"), not an absence.
        assert_eq!(of("d1").bands, Some(Vec::new()));
        assert_eq!(of("d2").bands, None, "absent means inherit");
    }

    #[test]
    fn an_unusable_bands_list_drops_only_that_tuner() {
        // Every malformation fails the whole list rather than dropping entries:
        // a partially applied band list would silently narrow what the tuner
        // records, and would look applied on keyup.
        let long = "8".repeat(MAX_BAND_LEN + 1);
        let many = (0..=MAX_BANDS)
            .map(|_| "\"800\"".to_owned())
            .collect::<Vec<_>>()
            .join(",");
        let cases = [
            ("unresolvable label", r#"["800","nonsense"]"#.to_owned()),
            ("entry over the length cap", format!(r#"["{long}"]"#)),
            ("more entries than the cap", format!("[{many}]")),
            ("not an array", "\"800\"".to_owned()),
            ("entry not a string", "[800]".to_owned()),
            (
                "reversed explicit range",
                r#"["866000000-856000000"]"#.to_owned(),
            ),
        ];
        for (name, bands) in cases {
            let body = format!(
                r#"{{"version":1,"tuners":{{
                    "d0":{{"version":1,"config":{{"bands":{bands}}}}},
                    "d1":{{"version":1,"config":{{"center_frequency":855500000}}}}
                }}}}"#
            );
            let snap = parse_desired(&body);
            assert_eq!(snap.tuners.len(), 1, "{name}: only d0 is dropped");
            assert_eq!(snap.tuners[0].0, "d1", "{name}: the rest still applies");
        }
    }

    #[test]
    fn absent_bands_inherit_what_is_running() {
        let devices = vec![dev_with_bands("d0", 855_500_000, &["800"])];
        let state = state_for(&devices);
        assert_eq!(
            state.applied("d0").unwrap().config.bands,
            Some(vec!["800".to_owned()]),
            "the TOML list is what is in effect at boot"
        );
        // keyup says nothing about bands: the running list is kept, so the
        // config is identical and only the version moves.
        assert_eq!(
            state.apply_desired("d0", &desired_bands(4, None)),
            ApplyDecision::VersionOnly
        );
        assert_eq!(
            state.take_for_build("d0").config.bands,
            Some(vec!["800".to_owned()])
        );
    }

    #[test]
    fn a_bands_change_restarts_and_an_identical_one_does_not() {
        let devices = vec![dev_with_bands("d0", 855_500_000, &["800"])];
        let state = state_for(&devices);

        // Same list, newer version: a config change it is not.
        assert_eq!(
            state.apply_desired("d0", &desired_bands(1, Some(&["800"]))),
            ApplyDecision::VersionOnly
        );
        assert_eq!(state.generation_of("d0"), 0);

        // A different list is a config change, so it restarts — and the new
        // list is only in effect once the rebuilt unit confirms it opened.
        assert_eq!(
            state.apply_desired("d0", &desired_bands(2, Some(&["858000000-866000000"]))),
            ApplyDecision::Restart
        );
        assert_eq!(state.generation_of("d0"), 1);
        assert_eq!(
            state.applied("d0").unwrap().config.bands,
            Some(vec!["800".to_owned()]),
            "still the old list until the rebuild confirms"
        );
        let built = state.take_for_build("d0");
        assert_eq!(
            built.config.bands,
            Some(vec!["858000000-866000000".to_owned()])
        );
        state.confirm_applied("d0", &built.config, built.version);
        assert_eq!(
            state.applied("d0").unwrap().config.bands,
            Some(vec!["858000000-866000000".to_owned()])
        );
    }

    #[test]
    fn an_empty_bands_array_clears_a_toml_restriction() {
        let devices = vec![dev_with_bands("d0", 855_500_000, &["800"])];
        let state = state_for(&devices);
        assert_eq!(
            state.apply_desired("d0", &desired_bands(5, Some(&[]))),
            ApplyDecision::Restart,
            "clearing a restriction is a config change"
        );
        let built = state.take_for_build("d0");
        assert_eq!(built.config.bands, Some(Vec::new()));
        // And the rebuilt device records its whole usable capture again.
        let mut device = devices[0].clone();
        apply_override(&built.config, &mut device);
        assert!(device.bands.is_empty());
        assert_eq!(
            crate::config::band_ranges(device.center_frequency, device.sample_rate, &device.bands)
                .len(),
            1,
            "one range: the whole usable span"
        );
    }

    #[test]
    fn bands_that_miss_the_capture_are_refused_rather_than_staged() {
        // The brick case. "vhf" parses (it is a known label) but this tuner is
        // an 800 MHz capture, so applying it would rebuild a front that records
        // nothing at all — and would clear keyup's pending badge while doing it.
        let devices = vec![dev_with_bands("d0", 855_500_000, &["800"])];
        let state = state_for(&devices);
        assert_eq!(
            state.apply_desired("d0", &desired_bands(9, Some(&["vhf"]))),
            ApplyDecision::NoOp
        );
        assert_eq!(state.generation_of("d0"), 0, "no restart was staged");
        let applied = state.applied("d0").unwrap();
        assert_eq!(applied.version, 0, "and the version did not advance");
        assert_eq!(
            applied.config.bands,
            Some(vec!["800".to_owned()]),
            "the tuner keeps recording what it was recording"
        );
        // A partly-reachable list is fine: only "no overlap at all" is refused.
        assert_eq!(
            state.apply_desired("d0", &desired_bands(9, Some(&["vhf", "800"]))),
            ApplyDecision::Restart
        );
    }

    // -- a refusal is news once per version ---------------------------------

    /// Every `trunkd` event on one test's thread as `(level, field, message)`,
    /// where `field` is the event's `field` value ("" when it has none).
    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<(tracing::Level, String, String)>>>);

    impl Captured {
        fn count(&self, level: tracing::Level, field: &str) -> usize {
            self.0
                .lock()
                .expect("capture lock")
                .iter()
                .filter(|(l, f, _)| *l == level && f == field)
                .count()
        }

        /// Warnings about a refused `field`.
        fn warns(&self, field: &str) -> usize {
            self.count(tracing::Level::WARN, field)
        }

        /// The same, logged at debug because it was already warned about.
        fn debugs(&self, field: &str) -> usize {
            self.count(tracing::Level::DEBUG, field)
        }
    }

    std::thread_local! {
        /// Where [`CaptureRouter`] puts this thread's events, while a test
        /// is [`capturing`].
        static CAPTURE: std::cell::RefCell<Option<Captured>> =
            const { std::cell::RefCell::new(None) };
    }

    /// The process-wide test subscriber's one layer: each event goes to the
    /// capture of the thread that emitted it, if that thread has one.
    ///
    /// One global subscriber rather than `tracing::subscriber::with_default`
    /// per test: scoped dispatchers on parallel test threads race over each
    /// callsite's cached interest, and a test would now and then see none of
    /// its own events. [`capturing`] also rebuilds that cache once the
    /// subscriber is in place, for the same race around its installation.
    struct CaptureRouter;

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for CaptureRouter {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _: tracing_subscriber::layer::Context<'_, S>,
        ) {
            #[derive(Default)]
            struct Fields {
                field: String,
                message: String,
            }
            impl tracing::field::Visit for Fields {
                fn record_str(&mut self, f: &tracing::field::Field, v: &str) {
                    match f.name() {
                        "field" => self.field = v.to_owned(),
                        "message" => self.message = v.to_owned(),
                        _ => {}
                    }
                }
                fn record_debug(&mut self, f: &tracing::field::Field, v: &dyn std::fmt::Debug) {
                    match f.name() {
                        "field" => self.field = format!("{v:?}"),
                        "message" => self.message = format!("{v:?}"),
                        _ => {}
                    }
                }
            }
            if !event.metadata().target().starts_with("trunkd") {
                return;
            }
            CAPTURE.with(|slot| {
                if let Some(captured) = slot.borrow().as_ref() {
                    let mut fields = Fields::default();
                    event.record(&mut fields);
                    captured.0.lock().expect("capture lock").push((
                        *event.metadata().level(),
                        fields.field,
                        fields.message,
                    ));
                }
            });
        }
    }

    /// Run `f`, capturing every event this thread emits meanwhile.
    fn capturing(f: impl FnOnce()) -> Captured {
        static INSTALL: std::sync::Once = std::sync::Once::new();
        INSTALL.call_once(|| {
            use tracing_subscriber::layer::SubscriberExt;
            tracing::subscriber::set_global_default(
                tracing_subscriber::registry().with(CaptureRouter),
            )
            .expect("no other test installs a global subscriber");
        });
        // A callsite first hit on another test's thread while the subscriber
        // was being installed can keep the "nobody listens" interest it was
        // registered with; recompute every callsite's interest against the
        // subscriber that is now in place.
        tracing::callsite::rebuild_interest_cache();
        let captured = Captured::default();
        CAPTURE.with(|slot| *slot.borrow_mut() = Some(captured.clone()));
        f();
        CAPTURE.with(|slot| *slot.borrow_mut() = None);
        captured
    }

    /// The fields a tuner reports as rejected, sorted.
    fn reported_rejections(state: &DesiredState, id: &str) -> Vec<String> {
        let mut fields: Vec<String> = state
            .applied(id)
            .expect("the tuner exists")
            .rejected
            .into_iter()
            .map(|(field, _)| field)
            .collect();
        fields.sort();
        fields
    }

    /// An entry refused as a whole is never acted on,
    /// so keyup sends it on every poll (≈8,600 times a day) and it used to be
    /// warned about each time. Now: one warning per (tuner, version, field),
    /// debug after, the rejection reported on every status until a newer
    /// version supersedes it — and the version still not applied, so keyup
    /// keeps showing it pending.
    #[test]
    fn a_refused_version_seen_again_warns_once_and_stays_reported() {
        let state = state_for(&[dev_with_bands("d0", 855_500_000, &["800"])]);
        // Refused as a whole (no overlap), with a field validation refuses too.
        let refused = |version| DesiredTuner {
            ppm: Some(5_000.0),
            ..desired_bands(version, Some(&["vhf"]))
        };

        let log = capturing(|| {
            for poll in 0..3 {
                assert_eq!(state.apply_desired("d0", &refused(9)), ApplyDecision::NoOp);
                assert_eq!(
                    reported_rejections(&state, "d0"),
                    vec!["bands", "ppm"],
                    "poll {poll}: both refusals are reported"
                );
            }
        });
        assert_eq!(log.warns("bands"), 1, "the refusal is warned once");
        assert_eq!(
            log.warns("ppm"),
            1,
            "and so is the field validation refused"
        );
        assert_eq!(log.debugs("bands"), 2, "then at debug");
        assert_eq!(log.debugs("ppm"), 2);
        let applied = state.applied("d0").expect("tuner");
        assert_eq!(applied.version, 0, "the refused version is not applied");
        let bands_reason = &applied
            .rejected
            .iter()
            .find(|(f, _)| f == "bands")
            .expect("bands")
            .1;
        assert!(bands_reason.contains("vhf"), "{bands_reason}");

        // A newer version refused the same way is news again, once.
        let log = capturing(|| {
            state.apply_desired("d0", &refused(10));
            state.apply_desired("d0", &refused(10));
        });
        assert_eq!((log.warns("bands"), log.warns("ppm")), (1, 1));

        // A newer version that applies supersedes the rejections.
        assert_eq!(
            state.apply_desired("d0", &desired_bands(11, Some(&["800", "vhf"]))),
            ApplyDecision::Restart
        );
        assert!(reported_rejections(&state, "d0").is_empty());
    }

    /// The same through the poll loop, for all three kinds of refusal: a tuner
    /// entry refused as a whole, a tuner entry the parser cannot understand
    /// (which used to be warned about every poll and never reported), and a
    /// recorder-level field.
    #[test]
    fn polling_a_refused_document_twice_warns_once_per_field_and_keeps_reporting() {
        const BODY: &str = r#"{
            "version": 12,
            "recorder": { "version": 3, "spool_only": "yes" },
            "tuners": {
                "t0": { "version": 5, "config": { "bands": ["vhf"] } },
                "t1": { "version": 2, "config": { "gains": [1, 2] } }
            }
        }"#;
        let stub = StubKeyup::start(Some(BODY));
        let cfg = slow_control_cfg(stub.url(), Arc::new(AtomicBool::new(false)));
        let desired = state_for(&[
            dev_with_bands("t0", 855_500_000, &["800"]),
            dev("t1", DeviceRole::Auto, 851_000_000),
        ]);
        let descriptors = [prefetch_descriptor("t0"), prefetch_descriptor("t1")];
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(5)))
            .build()
            .into();
        let applied = trunkd_core::Metrics::new().counter("control.applied");

        let log = capturing(|| {
            for _ in 0..2 {
                poll_desired(
                    &agent,
                    &cfg,
                    &descriptors,
                    &desired,
                    &applied,
                    &mut FailureRun::default(),
                );
            }
        });
        assert_eq!(stub.served(), 2, "both polls fetched the document");
        for field in ["bands", "gains", "spool_only"] {
            assert_eq!(log.warns(field), 1, "{field} is warned about once");
            assert_eq!(log.debugs(field), 1, "{field} is at debug the second time");
        }

        // Every refusal is still reported after the second poll.
        assert_eq!(reported_rejections(&desired, "t0"), vec!["bands"]);
        assert_eq!(reported_rejections(&desired, "t1"), vec!["gains"]);
        let t1 = desired.applied("t1").expect("t1");
        assert_eq!(t1.version, 0, "the unparseable entry is not applied");
        assert_eq!(
            t1.rejected[0].1, "the value is not an object",
            "with the parser's reason"
        );
        let recorder = cfg.recorder.applied().expect("the recorder block");
        assert_eq!(recorder.version, 3);
        assert_eq!(
            recorder
                .rejected
                .iter()
                .map(|(f, _)| f.as_str())
                .collect::<Vec<_>>(),
            vec!["spool_only"]
        );
        assert_eq!(applied.get(), 0, "nothing was applied to a tuner");
    }

    #[test]
    fn a_center_push_that_moves_the_capture_off_the_bands_is_refused_too() {
        // The same net catches the other direction: bands are inherited, and
        // the merged config is what gets checked.
        let devices = vec![dev_with_bands("d0", 855_500_000, &["800"])];
        let state = state_for(&devices);
        let d = DesiredTuner {
            center_frequency: Some(154_000_000),
            ..desired_bands(3, None)
        };
        assert_eq!(state.apply_desired("d0", &d), ApplyDecision::NoOp);
        assert_eq!(state.generation_of("d0"), 0);
    }

    #[test]
    fn the_build_override_runs_the_applied_bands_not_the_toml_ones() {
        let devices = vec![dev_with_bands("d0", 855_500_000, &["800"])];
        let state = state_for(&devices);
        assert_eq!(
            state.apply_desired("d0", &desired_bands(2, Some(&["856000000-866000000"]))),
            ApplyDecision::Restart
        );
        let built = state.take_for_build("d0");
        let mut device = devices[0].clone();
        apply_override(&built.config, &mut device);
        assert_eq!(device.bands, vec!["856000000-866000000".to_owned()]);
        // The fields keyup did not touch keep their TOML values.
        assert_eq!(device.center_frequency, Some(855_500_000));
        assert_eq!(device.role, DeviceRole::Voice);
        // And the coordinator's ranges follow, clamped to the usable capture:
        // the half of the span the operator asked for, not the whole thing.
        let ranges =
            crate::config::band_ranges(device.center_frequency, device.sample_rate, &device.bands);
        assert_eq!(ranges.len(), 1);
        assert_eq!(ranges[0].0, 856_000_000);
        assert!(ranges[0].1 < 866_000_000, "clamped to the capture edge");
    }

    #[test]
    fn the_status_body_echoes_the_bands_in_force() {
        let m = trunkd_core::Metrics::new();
        let now = trunkd_core::Timestamp::now().unix_millis();
        m.gauge("device.ctrl.stream_beat_ms").set(now as u64);
        let applied = AppliedSnapshot {
            config: AppliedConfig {
                bands: Some(vec!["800".to_owned(), "858000000-866000000".to_owned()]),
                ..applied_snapshot().config
            },
            ..applied_snapshot()
        };
        let t = gather_tuner_status(&m.snapshot(), &descriptor(), Some(&applied), now);
        assert_eq!(t.bands, vec!["800", "858000000-866000000"]);
        let body = status_body(
            "rec-01",
            &Station::default(),
            &Counters::default(),
            &[t],
            &no_hold(),
            None,
            &EventLogReport::default(),
        );
        assert_eq!(body["tuners"][0]["bands"][0], "800");
        assert_eq!(body["tuners"][0]["bands"][1], "858000000-866000000");
    }

    #[test]
    fn an_unrestricted_tuner_sends_no_bands_field() {
        // Empty and absent say the same thing; absent is the cheaper of the two.
        let empty = AppliedSnapshot {
            config: AppliedConfig {
                bands: Some(Vec::new()),
                ..applied_snapshot().config
            },
            ..applied_snapshot()
        };
        let m = trunkd_core::Metrics::new();
        let now = trunkd_core::Timestamp::now().unix_millis();
        let t = gather_tuner_status(&m.snapshot(), &descriptor(), Some(&empty), now);
        assert!(t.bands.is_empty());
        let body = status_body(
            "rec-01",
            &Station::default(),
            &Counters::default(),
            &[t],
            &no_hold(),
            None,
            &EventLogReport::default(),
        );
        assert!(body["tuners"][0].get("bands").is_none(), "body was {body}");
    }

    // -- override provenance ------------------------------------------------

    /// Push a desired config and let the "rebuilt" unit confirm it opened,
    /// which is the only thing that moves the applied view status reports read.
    fn push_and_confirm(state: &DesiredState, id: &str, t: DesiredTuner) {
        state.apply_desired(id, &t);
        let built = state.take_for_build(id);
        state.confirm_applied(id, &built.config, built.version);
    }

    /// The status body for one tuner of `state`, through the real builders.
    fn body_for(state: &DesiredState, id: &str) -> Value {
        let t = gather_tuner_status(
            &trunkd_core::Metrics::new().snapshot(),
            &TunerDescriptor {
                id: id.to_owned(),
                backend: Backend::Iqfile,
                site_metric_prefix: None,
                ..descriptor()
            },
            state.applied(id).as_ref(),
            trunkd_core::Timestamp::now().unix_millis(),
        );
        status_body(
            "rec-01",
            &Station::default(),
            &Counters::default(),
            &[t],
            &no_hold(),
            None,
            &EventLogReport::default(),
        )
    }

    #[test]
    fn the_status_names_the_fields_keyup_took_over_from_the_file() {
        // The bug this exists for: the file names d1 `control` and leaves d0 on
        // the default, keyup pushes d0 to `control`, and both then read
        // "control" with nothing on the page saying which the operator asked
        // for.
        let state = state_for(&[
            dev("d0", DeviceRole::Auto, 855_500_000),
            dev("d1", DeviceRole::Control, 855_500_000),
        ]);
        push_and_confirm(
            &state,
            "d0",
            DesiredTuner {
                version: 3,
                role: Some(DeviceRole::Control),
                enabled: true,
                ..DesiredTuner::default()
            },
        );

        let snap = state.applied("d0").expect("the tuner exists");
        assert_eq!(snap.config.role, Some(DeviceRole::Control));
        assert_eq!(snap.overrides, vec!["role"]);
        assert_eq!(snap.file_role, DeviceRole::Auto, "what the file asks for");

        let body = body_for(&state, "d0");
        let t = &body["tuners"][0];
        assert_eq!(t["role"], "control", "the effective role, as before");
        assert_eq!(t["overrides"], json!(["role"]));
        assert_eq!(t["file_role"], "auto", "and what it replaced");
    }

    #[test]
    fn an_override_that_agrees_with_the_file_is_not_an_override() {
        // keyup re-asserting the role the file already asks for changes
        // nothing an operator needs to be warned about, so flagging it would
        // only teach them to ignore the marker. `site_id` in the same push is
        // still listed: the file has nowhere to write one, so it can only ever
        // have come from keyup.
        let state = state_for(&[dev("d0", DeviceRole::Control, 855_500_000)]);
        push_and_confirm(
            &state,
            "d0",
            DesiredTuner {
                version: 3,
                role: Some(DeviceRole::Control),
                site_id: Some("1".to_owned()),
                enabled: true,
                ..DesiredTuner::default()
            },
        );

        let snap = state.applied("d0").expect("the tuner exists");
        assert_eq!(snap.overrides, vec!["site_id"]);
        let body = body_for(&state, "d0");
        assert_eq!(body["tuners"][0]["overrides"], json!(["site_id"]));
        assert!(
            body["tuners"][0].get("file_role").is_none(),
            "the role in force is the file's, so there is nothing to compare it \
             with: {body}"
        );
    }

    #[test]
    fn an_untouched_tuner_reports_an_empty_override_list() {
        // Empty, not absent: a panel has to be able to tell "this tuner runs
        // the file" from "this recorder is too old to say".
        let state = state_for(&[dev("d0", DeviceRole::Control, 855_500_000)]);
        assert!(state
            .applied("d0")
            .expect("the tuner exists")
            .overrides
            .is_empty());

        let body = body_for(&state, "d0");
        assert_eq!(body["tuners"][0]["overrides"], json!([]));
        assert!(body["tuners"][0].get("file_role").is_none());
    }

    #[test]
    fn a_never_started_tuner_is_not_locked() {
        // No heartbeat gauge at all (unit never opened a stream).
        let m = trunkd_core::Metrics::new();
        m.gauge("site.0.control_frequency_hz").set(851_050_000);
        let now = trunkd_core::Timestamp::now().unix_millis();
        let t = gather_tuner_status(&m.snapshot(), &descriptor(), Some(&applied_snapshot()), now);
        assert!(!t.locked);
        assert_eq!(t.control_hz, 0);
    }

    // -- the full field set: parse, merge, validate, decide -----------------

    /// A Soapy tuner with a real gain-element table, which is what the
    /// validation of `gain`/`gains` is judged against.
    fn soapy_state() -> Arc<DesiredState> {
        let d: Device = toml::from_str(
            "id = \"d0\"\nbackend = \"soapy\"\ndriver = \"airspy\"\n\
             sample_rate = 20000000\ncenter_frequency = 855500000\n",
        )
        .expect("device");
        let state = DesiredState::from_config(&Config {
            device: vec![d],
            site: vec![site_for("d0", &[851_050_000])],
            ..Config::default()
        });
        state.set_gain_elements(
            "d0",
            vec![
                GainElement {
                    name: "LNA".into(),
                    min: 0.0,
                    max: 14.0,
                    step: 1.0,
                },
                GainElement {
                    name: "VGA".into(),
                    min: 0.0,
                    max: 15.0,
                    step: 1.0,
                },
            ],
        );
        state
    }

    fn site_for(device: &str, controls: &[u64]) -> Site {
        let list: Vec<String> = controls.iter().map(u64::to_string).collect();
        toml::from_str(&format!(
            "system = \"Example\"\nsite = \"Central\"\ndevice = \"{device}\"\n\
             control_frequencies = [{}]\n",
            list.join(", ")
        ))
        .expect("site")
    }

    /// A desired-state document naming every overridable field.
    fn every_field_body() -> &'static str {
        r#"{
          "version": 9,
          "tuners": {
            "d0": {
              "version": 3,
              "config": {
                "role": "voice",
                "site": "Central",
                "site_id": 7,
                "center_frequency": 855500000,
                "bands": ["800"],
                "enabled": true,
                "spectrum": true,
                "gain": 12.5,
                "gains": {"LNA": 10.0, "VGA": 8},
                "agc": true,
                "ppm": -1.25,
                "auto_ppm": false,
                "auto_ppm_limit": 12.0,
                "bandwidth_hz": 18000000,
                "rx_port": "Antenna B",
                "settings": {"biastee": "false", "digital_gain": 3, "agc_setpoint": true},
                "lna_db": 24,
                "vga_db": 20,
                "amp": true,
                "data_follow": true,
                "control_frequencies": [851050000, 852137500],
                "sample_rate": 20000000,
                "something_keyup_invented_later": {"nested": [1, 2, 3]}
              }
            }
          }
        }"#
    }

    #[test]
    fn the_parser_accepts_every_field_and_ignores_what_it_does_not_know() {
        let snap = parse_desired(every_field_body());
        assert_eq!(snap.version, 9);
        let (key, t) = &snap.tuners[0];
        assert_eq!(key, "d0");
        assert_eq!(t.version, 3);
        assert_eq!(t.role, Some(DeviceRole::Voice));
        assert_eq!(t.site.as_deref(), Some("Central"));
        assert_eq!(t.site_id.as_deref(), Some("7"));
        assert_eq!(t.center_frequency, Some(855_500_000));
        assert_eq!(t.bands.as_deref(), Some(["800".to_owned()].as_slice()));
        assert!(t.enabled);
        assert!(t.spectrum);
        assert_eq!(t.gain, Some(12.5));
        assert_eq!(
            t.gains,
            Some(BTreeMap::from([
                ("LNA".to_owned(), 10.0),
                ("VGA".to_owned(), 8.0)
            ]))
        );
        assert_eq!(t.agc, Some(true));
        assert_eq!(t.ppm, Some(-1.25));
        assert_eq!(t.auto_ppm, Some(false));
        assert_eq!(t.auto_ppm_limit, Some(12.0));
        assert_eq!(t.bandwidth_hz, Some(18_000_000));
        assert_eq!(t.rx_port.as_deref(), Some("Antenna B"));
        // A driver setting is a string whichever way it was typed: keyup's
        // form fields are not going to agree with TOML about that.
        assert_eq!(
            t.settings,
            Some(BTreeMap::from([
                ("agc_setpoint".to_owned(), "true".to_owned()),
                ("biastee".to_owned(), "false".to_owned()),
                ("digital_gain".to_owned(), "3".to_owned()),
            ]))
        );
        assert_eq!(t.lna_db, Some(24));
        assert_eq!(t.vga_db, Some(20));
        assert_eq!(t.amp, Some(true));
        assert_eq!(t.data_follow, Some(true));
        assert_eq!(
            t.control_frequencies.as_deref(),
            Some([851_050_000u64, 852_137_500].as_slice())
        );
        assert_eq!(t.sample_rate, Some(20_000_000));
        // The unknown field cost nothing: a newer keyup can send fields this
        // recorder has never heard of and the rest still applies.
    }

    #[test]
    fn an_absent_field_inherits_and_an_empty_collection_clears() {
        let applied = AppliedConfig {
            gain: Some(20.0),
            gains: Some(BTreeMap::from([("LNA".to_owned(), 9.0)])),
            ppm: Some(1.5),
            rx_port: Some("Antenna A".to_owned()),
            settings: Some(BTreeMap::from([("biastee".to_owned(), "true".to_owned())])),
            control_frequencies: Some(vec![851_050_000]),
            sample_rate: Some(20_000_000),
            enabled: true,
            ..AppliedConfig::default()
        };
        // A push that mentions only `agc` leaves every other field alone.
        let sparse = DesiredTuner {
            version: 1,
            enabled: true,
            agc: Some(true),
            ..DesiredTuner::default()
        };
        let merged = merge(&applied, &sparse);
        assert_eq!(merged.agc, Some(true));
        assert_eq!(merged.gain, Some(20.0));
        assert_eq!(merged.ppm, Some(1.5));
        assert_eq!(merged.rx_port.as_deref(), Some("Antenna A"));
        assert_eq!(merged.control_frequencies, Some(vec![851_050_000]));

        // An *empty* collection is a real instruction, not an absent field:
        // "no per-element gains", "no driver settings".
        let cleared = DesiredTuner {
            version: 2,
            enabled: true,
            gains: Some(BTreeMap::new()),
            settings: Some(BTreeMap::new()),
            ..DesiredTuner::default()
        };
        let merged = merge(&applied, &cleared);
        assert_eq!(merged.gains, Some(BTreeMap::new()));
        assert_eq!(merged.settings, Some(BTreeMap::new()));
        assert_eq!(merged.gain, Some(20.0), "and nothing else moved");
    }

    #[test]
    fn a_malformed_map_drops_the_tuner_rather_than_half_applying_it() {
        for body in [
            r#"{"tuners":{"d0":{"version":1,"config":{"gains":{"LNA":"loud"}}}}}"#,
            r#"{"tuners":{"d0":{"version":1,"config":{"gains":[1,2]}}}}"#,
            r#"{"tuners":{"d0":{"version":1,"config":{"settings":{"k":[1]}}}}}"#,
            r#"{"tuners":{"d0":{"version":1,"config":{"control_frequencies":["851050000"]}}}}"#,
        ] {
            let snapshot = parse_desired(body);
            assert!(
                snapshot.tuners.is_empty(),
                "a partly-understood {body} must not be applied"
            );
            assert_eq!(
                snapshot
                    .refused
                    .iter()
                    .map(|r| (r.tuner.as_str(), r.version))
                    .collect::<Vec<_>>(),
                vec![("d0", 1)],
                "and the refusal is carried out to be logged and reported"
            );
        }
    }

    // -- validation ---------------------------------------------------------

    fn push(state: &DesiredState, t: DesiredTuner) -> (ApplyDecision, Vec<(String, String)>) {
        let decision = state.apply_desired("d0", &t);
        let snap = state.applied("d0").expect("the tuner exists");
        (decision, snap.rejected)
    }

    #[test]
    fn a_rejected_field_is_not_applied_but_its_version_is_still_acknowledged() {
        let state = soapy_state();
        let (decision, rejected) = push(
            &state,
            DesiredTuner {
                version: 5,
                enabled: true,
                // Every one of these is refused for its own reason.
                ppm: Some(5_000.0),
                gain: Some(90.0),
                gains: Some(BTreeMap::from([("MIX".to_owned(), 3.0)])),
                auto_ppm_limit: Some(0.0),
                sample_rate: Some(20_000_001),
                control_frequencies: Some(vec![851_051_000]),
                ..DesiredTuner::default()
            },
        );
        // Nothing survived, so there is nothing to restart for — but the
        // version moves, which is what stops keyup resending it forever.
        assert_eq!(decision, ApplyDecision::VersionOnly);
        let snap = state.applied("d0").expect("tuner");
        assert_eq!(snap.version, 5, "acknowledged even though nothing applied");
        assert_eq!(snap.config.ppm, Some(0.0), "the running ppm is untouched");
        assert_eq!(snap.config.gain, None);
        assert_eq!(snap.config.sample_rate, Some(20_000_000));
        assert_eq!(state.generation_of("d0"), 0, "and no restart happened");

        let by_field: HashMap<&str, &str> = rejected
            .iter()
            .map(|(f, r)| (f.as_str(), r.as_str()))
            .collect();
        assert_eq!(
            by_field.len(),
            6,
            "one entry per refused field: {rejected:?}"
        );
        assert!(by_field["ppm"].contains("200"));
        assert!(
            by_field["gain"].contains("29"),
            "the sum of the element maxima"
        );
        assert!(
            by_field["gains"].contains("MIX"),
            "named element, not a range"
        );
        assert!(by_field["auto_ppm_limit"].contains("> 0"));
        assert!(by_field["sample_rate"].contains("12500"));
        assert!(by_field["control_frequencies"].contains("channel grid"));
    }

    #[test]
    fn a_rejected_field_does_not_stop_the_rest_of_the_push() {
        let state = soapy_state();
        let (decision, rejected) = push(
            &state,
            DesiredTuner {
                version: 2,
                enabled: true,
                ppm: Some(1_000.0),                    // refused
                rx_port: Some("Antenna B".to_owned()), // applied
                ..DesiredTuner::default()
            },
        );
        assert_eq!(decision, ApplyDecision::Restart, "rx_port needs a restart");
        assert_eq!(rejected.len(), 1);
        assert_eq!(rejected[0].0, "ppm");
        let built = state.take_for_build("d0");
        assert_eq!(built.config.rx_port.as_deref(), Some("Antenna B"));
        assert_eq!(built.config.ppm, Some(0.0), "the refused field stayed put");
    }

    #[test]
    fn control_frequencies_must_be_on_the_grid_and_inside_the_span() {
        let state = soapy_state();
        // On-grid but outside a 20 MHz capture centred on 855.5 MHz.
        let (_, rejected) = push(
            &state,
            DesiredTuner {
                version: 2,
                enabled: true,
                control_frequencies: Some(vec![770_050_000]),
                ..DesiredTuner::default()
            },
        );
        assert_eq!(rejected.len(), 1);
        assert!(rejected[0].1.contains("usable span"), "{rejected:?}");

        // On-grid and inside: applied, and it restarts the tuner (the site's
        // engine is built with the candidate list).
        let (decision, rejected) = push(
            &state,
            DesiredTuner {
                version: 3,
                enabled: true,
                control_frequencies: Some(vec![851_050_000, 852_137_500]),
                ..DesiredTuner::default()
            },
        );
        assert!(rejected.is_empty(), "{rejected:?}");
        assert_eq!(decision, ApplyDecision::Restart);
    }

    #[test]
    fn hackrf_gains_are_judged_by_the_hardware_steps() {
        let devices = vec![toml::from_str::<Device>(
            "id = \"d0\"\nbackend = \"hackrf\"\nsample_rate = 20000000\n\
             center_frequency = 855500000\n",
        )
        .expect("device")];
        let state = state_for(&devices);
        let (_, rejected) = push(
            &state,
            DesiredTuner {
                version: 1,
                enabled: true,
                lna_db: Some(25), // not a multiple of 8
                vga_db: Some(21), // not a multiple of 2
                ..DesiredTuner::default()
            },
        );
        assert_eq!(rejected.len(), 2, "{rejected:?}");

        let (decision, rejected) = push(
            &state,
            DesiredTuner {
                version: 2,
                enabled: true,
                lna_db: Some(24),
                vga_db: Some(20),
                ..DesiredTuner::default()
            },
        );
        assert!(rejected.is_empty());
        // A HackRF cannot change gain on a running child, so this restarts.
        assert_eq!(decision, ApplyDecision::Restart);
    }

    #[test]
    fn the_sample_rate_rule_is_the_channelizers_own() {
        assert!(sample_rate_ok(20_000_000), "1600 bins");
        assert!(sample_rate_ok(2_400_000), "192 bins");
        assert!(sample_rate_ok(50_000), "the floor: 4 bins");
        assert!(!sample_rate_ok(25_000), "2 bins is under the floor");
        assert!(!sample_rate_ok(2_412_500), "193 bins is odd");
        assert!(!sample_rate_ok(20_000_001), "off the 12.5 kHz grid");
        assert!(!sample_rate_ok(0));
    }

    // -- runtime vs restart -------------------------------------------------

    #[test]
    fn the_runtime_restart_table_is_by_backend_and_by_field() {
        let base = AppliedConfig {
            enabled: true,
            gain: Some(10.0),
            ppm: Some(0.0),
            agc: Some(false),
            sample_rate: Some(20_000_000),
            ..AppliedConfig::default()
        };
        let cases: &[(&str, AppliedConfig, ApplyDecision, ApplyDecision)] = &[
            (
                "gain",
                AppliedConfig {
                    gain: Some(12.0),
                    ..base.clone()
                },
                ApplyDecision::Runtime,
                ApplyDecision::Restart,
            ),
            (
                "agc",
                AppliedConfig {
                    agc: Some(true),
                    ..base.clone()
                },
                ApplyDecision::Runtime,
                ApplyDecision::Restart,
            ),
            (
                "ppm",
                AppliedConfig {
                    ppm: Some(1.5),
                    ..base.clone()
                },
                ApplyDecision::Runtime,
                ApplyDecision::Restart,
            ),
            (
                "gains",
                AppliedConfig {
                    gains: Some(BTreeMap::from([("LNA".to_owned(), 9.0)])),
                    ..base.clone()
                },
                ApplyDecision::Runtime,
                ApplyDecision::Restart,
            ),
            (
                "sample_rate",
                AppliedConfig {
                    sample_rate: Some(2_400_000),
                    ..base.clone()
                },
                ApplyDecision::Restart,
                ApplyDecision::Restart,
            ),
            (
                "rx_port",
                AppliedConfig {
                    rx_port: Some("Antenna B".to_owned()),
                    ..base.clone()
                },
                ApplyDecision::Restart,
                ApplyDecision::Restart,
            ),
            (
                "control_frequencies",
                AppliedConfig {
                    control_frequencies: Some(vec![851_050_000]),
                    ..base.clone()
                },
                ApplyDecision::Restart,
                ApplyDecision::Restart,
            ),
            (
                "data_follow",
                AppliedConfig {
                    data_follow: Some(true),
                    ..base.clone()
                },
                ApplyDecision::Restart,
                ApplyDecision::Restart,
            ),
            (
                "nothing",
                base.clone(),
                ApplyDecision::VersionOnly,
                ApplyDecision::VersionOnly,
            ),
        ];
        for (field, merged, soapy, hackrf) in cases {
            assert_eq!(
                decide(1, &base, 2, merged, Backend::Soapy),
                *soapy,
                "soapy: {field}"
            );
            assert_eq!(
                decide(1, &base, 2, merged, Backend::Hackrf),
                *hackrf,
                "hackrf: {field}"
            );
        }
        // A gain change plus one restart field is a restart, not a runtime
        // apply that silently skips the rest.
        let mixed = AppliedConfig {
            gain: Some(12.0),
            rx_port: Some("Antenna B".to_owned()),
            ..base.clone()
        };
        assert_eq!(
            decide(1, &base, 2, &mixed, Backend::Soapy),
            ApplyDecision::Restart
        );
        // And an older version is a no-op whatever changed.
        assert_eq!(
            decide(5, &base, 5, &mixed, Backend::Soapy),
            ApplyDecision::NoOp
        );
    }

    #[test]
    fn a_runtime_change_reaches_the_mailbox_without_a_restart() {
        let state = soapy_state();
        let slot = state.runtime_slot("d0");
        let decision = state.apply_desired(
            "d0",
            &DesiredTuner {
                version: 4,
                enabled: true,
                gain: Some(12.0),
                agc: Some(true),
                ..DesiredTuner::default()
            },
        );
        assert_eq!(decision, ApplyDecision::Runtime);
        assert_eq!(state.generation_of("d0"), 0, "no rebuild");
        assert_eq!(
            state.applied("d0").expect("tuner").version,
            4,
            "and the version advances at once: there is no rebuild to wait for"
        );
        let req = slot
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
            .expect("the change is waiting for the running unit");
        assert_eq!(req.gain, Some(12.0));
        assert_eq!(req.agc, Some(true));
        assert_eq!(
            req.ppm, None,
            "only the fields that actually moved are sent"
        );
    }

    #[test]
    fn the_capability_table_says_what_a_change_costs() {
        let soapy = TunerCapabilities::for_backend(Backend::Soapy);
        assert!(soapy.runtime_gain);
        assert!(soapy.runtime_ppm);
        assert!(soapy.restart_on.contains(&"sample_rate"));
        assert!(soapy.restart_on.contains(&"rx_port"));
        assert!(!soapy.restart_on.contains(&"gain"), "gain is live on soapy");
        assert!(
            !soapy.restart_on.contains(&"lna_db"),
            "a HackRF-only field is not offered for an Airspy"
        );

        let hackrf = TunerCapabilities::for_backend(Backend::Hackrf);
        assert!(!hackrf.runtime_gain, "the child has to be respawned");
        assert!(!hackrf.runtime_ppm);
        assert!(hackrf.restart_on.contains(&"lna_db"));
        assert!(!hackrf.restart_on.contains(&"rx_port"));

        let file = TunerCapabilities::for_backend(Backend::Iqfile);
        assert!(!file.runtime_gain);
        assert!(!file.restart_on.contains(&"gain"), "a file has no gain");
    }

    // -- the site-scoped half -----------------------------------------------

    #[test]
    fn the_site_scoped_fields_reach_the_primary_site_only() {
        let mut cfg = Config {
            device: vec![dev("d0", DeviceRole::Auto, 855_500_000)],
            site: vec![
                site_for("d0", &[851_050_000]),
                site_for("d0", &[852_137_500]),
                site_for("other", &[853_000_000]),
            ],
            ..Config::default()
        };
        let applied = AppliedConfig {
            data_follow: Some(true),
            control_frequencies: Some(vec![851_062_500]),
            enabled: true,
            ..AppliedConfig::default()
        };
        apply_site_override(&applied, "d0", &mut cfg);

        // Control candidates name one channel list, so only the primary site
        // (the first this tuner serves) is rewritten.
        assert_eq!(cfg.site[0].control_frequencies, vec![851_062_500]);
        assert_eq!(cfg.site[1].control_frequencies, vec![852_137_500]);
        assert_eq!(cfg.site[2].control_frequencies, vec![853_000_000]);
        // `data_follow` is a property of what the tuner does with its
        // channelizer, so it applies to every site the tuner serves.
        assert!(cfg.site[0].data_follow);
        assert!(cfg.site[1].data_follow);
        assert!(
            !cfg.site[2].data_follow,
            "and not to a site it does not serve"
        );
    }

    #[test]
    fn the_registry_seeds_the_site_scoped_fields_from_the_primary_site() {
        let state = DesiredState::from_config(&Config {
            device: vec![dev("d0", DeviceRole::Auto, 855_500_000)],
            site: vec![site_for("d0", &[851_050_000, 852_137_500])],
            ..Config::default()
        });
        let snap = state.applied("d0").expect("tuner");
        assert_eq!(
            snap.config.control_frequencies,
            Some(vec![851_050_000, 852_137_500]),
            "the boot value is what is in effect until keyup says otherwise"
        );
        assert_eq!(snap.config.data_follow, Some(false));
        assert_eq!(snap.config.sample_rate, Some(20_000_000));
        assert_eq!(snap.config.ppm, Some(0.0));
    }

    #[test]
    fn an_override_reaches_the_rebuilt_device() {
        let mut device = dev("d0", DeviceRole::Auto, 855_500_000);
        apply_override(
            &AppliedConfig {
                gain: Some(21.0),
                gains: Some(BTreeMap::from([("LNA".to_owned(), 9.0)])),
                agc: Some(true),
                ppm: Some(-1.5),
                auto_ppm: Some(false),
                auto_ppm_limit: Some(12.0),
                bandwidth_hz: Some(18_000_000),
                rx_port: Some("Antenna B".to_owned()),
                settings: Some(BTreeMap::from([("biastee".to_owned(), "false".to_owned())])),
                lna_db: Some(24),
                vga_db: Some(20),
                amp: Some(true),
                sample_rate: Some(2_400_000),
                enabled: true,
                ..AppliedConfig::default()
            },
            &mut device,
        );
        assert_eq!(device.gain, Some(21.0));
        assert_eq!(device.gains.get("LNA"), Some(&9.0));
        assert_eq!(device.agc, Some(true));
        assert_eq!(device.ppm, -1.5);
        assert!(!device.auto_ppm);
        assert_eq!(device.auto_ppm_limit, 12.0);
        assert_eq!(device.bandwidth_hz, Some(18_000_000));
        assert_eq!(device.rx_port.as_deref(), Some("Antenna B"));
        assert_eq!(
            device.settings.get("biastee").map(String::as_str),
            Some("false")
        );
        assert_eq!(device.lna_db, Some(24));
        assert_eq!(device.vga_db, Some(20));
        assert_eq!(device.amp, Some(true));
        assert_eq!(device.sample_rate, 2_400_000);
    }

    #[test]
    fn a_rejection_rides_back_on_the_status_body_beside_the_version() {
        let state = soapy_state();
        state.apply_desired(
            "d0",
            &DesiredTuner {
                version: 6,
                enabled: true,
                ppm: Some(9_999.0),
                ..DesiredTuner::default()
            },
        );
        let m = trunkd_core::Metrics::new();
        let now = trunkd_core::Timestamp::now().unix_millis();
        let d = TunerDescriptor {
            id: "d0".into(),
            backend: Backend::Soapy,
            ..descriptor()
        };
        let t = gather_tuner_status(&m.snapshot(), &d, state.applied("d0").as_ref(), now);
        let body = status_body(
            "rec-01",
            &Station::default(),
            &Counters::default(),
            &[t],
            &no_hold(),
            None,
            &EventLogReport::default(),
        );
        let tuner = &body["tuners"][0];
        assert_eq!(tuner["applied_version"], 6, "the version is acknowledged");
        assert!(tuner["rejected"]["ppm"]
            .as_str()
            .expect("a reason keyup can show an operator")
            .contains("200"));
        // And the element table the operator's GUI draws its controls from.
        assert_eq!(tuner["gain_elements"][0]["name"], "LNA");
        assert_eq!(tuner["gain_elements"][0]["max"], 14.0);
        assert_eq!(tuner["capabilities"]["runtime_gain"], true);
    }

    // -- the event-log block ------------------------------------------------

    #[test]
    fn the_status_body_reports_the_event_log_archive() {
        let body = status_body(
            "rec-01",
            &Station::default(),
            &Counters::default(),
            &[],
            &no_hold(),
            None,
            &EventLogReport {
                archive: true,
                source: ArchiveSource::Keyup,
                pending: 3,
                last_archived: Some("2026-08-30".into()),
                last_error: Some("keyup answered 503".into()),
                last_written_at: Some("2026-09-04T05:28:50.230Z".into()),
            },
        );
        assert_eq!(body["event_log"]["archive"], true);
        assert_eq!(body["event_log"]["pending"], 3);
        assert_eq!(body["event_log"]["last_archived"], "2026-08-30");
        assert_eq!(body["event_log"]["last_error"], "keyup answered 503");
        // When the log last took a line: the field that answers "the recorder
        // has been quiet for an hour — since when?" without an SSH session.
        assert_eq!(
            body["event_log"]["last_written_at"],
            "2026-09-04T05:28:50.230Z"
        );
        // Off is still reported, because "it is off" is the answer to most
        // questions about a day that never arrived.
        let off = status_body(
            "rec-01",
            &Station::default(),
            &Counters::default(),
            &[],
            &no_hold(),
            None,
            &EventLogReport::default(),
        );
        assert_eq!(off["event_log"]["archive"], false);
        assert_eq!(off["event_log"]["last_archived"], Value::Null);
    }

    // -- startup prefetch ---------------------------------------------------

    /// A one-shot keyup stand-in: serves `body` to every GET until dropped, or
    /// accepts and never answers when `body` is `None` (the timeout case).
    struct StubKeyup {
        port: u16,
        stop: Arc<AtomicBool>,
        /// GETs served, so a test can count how many polls actually happened.
        served: Arc<std::sync::atomic::AtomicU64>,
    }

    impl StubKeyup {
        fn start(body: Option<&'static str>) -> StubKeyup {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
            let port = listener.local_addr().expect("addr").port();
            listener.set_nonblocking(true).expect("nonblocking");
            let stop = Arc::new(AtomicBool::new(false));
            let served = Arc::new(std::sync::atomic::AtomicU64::new(0));
            let s = stop.clone();
            let count = served.clone();
            std::thread::spawn(move || {
                // Held so a connection the test never answers stays open for the
                // life of the stub rather than being closed by an early drop.
                let mut hung = Vec::new();
                while !s.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((mut stream, _)) => match body {
                            Some(body) => {
                                use std::io::{Read, Write};
                                // Windows hands an accepted socket the
                                // listener's non-blocking mode (Linux does
                                // not); a WouldBlock read here would answer
                                // before the request arrived.
                                let _ = stream.set_nonblocking(false);
                                let mut buf = [0u8; 2048];
                                let _ = stream.read(&mut buf);
                                // Counted before the answer goes out: the
                                // client returns as soon as it has
                                // Content-Length bytes, so a count taken after
                                // the write can still be pending when the test
                                // reads it (seen under load as 1 of 2 polls).
                                count.fetch_add(1, Ordering::SeqCst);
                                let _ = stream.write_all(
                                    format!(
                                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                                         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                                        body.len()
                                    )
                                    .as_bytes(),
                                );
                                let _ = stream.flush();
                            }
                            None => hung.push(stream),
                        },
                        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(2));
                        }
                        Err(_) => break,
                    }
                }
            });
            StubKeyup { port, stop, served }
        }

        /// GETs answered so far, or being answered: a poll that has returned is
        /// always counted.
        fn served(&self) -> u64 {
            self.served.load(Ordering::SeqCst)
        }

        fn url(&self) -> String {
            format!("http://127.0.0.1:{}/api/recorder/desired-state", self.port)
        }
    }

    impl Drop for StubKeyup {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
        }
    }

    /// A [`ControlConfig`] that polls `url` slowly enough that any GET inside
    /// the test window must have come from a push.
    fn slow_control_cfg(url: String, fetch_now: Arc<AtomicBool>) -> ControlConfig {
        ControlConfig {
            desired_url: url,
            status_url: String::new(),
            spectrum_url: String::new(),
            api_key: "kup_test".into(),
            instance_id: "rec-1".into(),
            station: Station::default(),
            desired_interval: Duration::from_secs(3600),
            status_interval: Duration::from_secs(3600),
            recorder: recorder_control(),
            upload_url: String::new(),
            link: LinkSlot::new(
                &Config::default(),
                &[],
                trunkd_core::Metrics::new(),
                "rec-1".to_owned(),
            ),
            event_log: crate::pipeline::eventarchive::ArchiveState::new(false),
            ppm_memory_path: std::path::PathBuf::from("/nonexistent/trunkd-ppm.json"),
            fetch_now,
            stream: crate::pipeline::stream::StreamState::new(true),
            panel: None,
        }
    }

    /// Wait for `f`, or give up after ten seconds.
    fn until(f: impl Fn() -> bool) -> bool {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            if f() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    #[test]
    fn a_push_pulls_the_desired_state_get_forward() {
        let stub = StubKeyup::start(Some(PREFETCH_BODY));
        let fetch_now = Arc::new(AtomicBool::new(false));
        let cfg = slow_control_cfg(stub.url(), fetch_now.clone());
        let recorder = cfg.recorder.clone();
        let shared = Shared {
            private_calls: Default::default(),
            calls: Default::default(),
            skip: trunkd_core::SkipList::empty(),
            replay_clock: Default::default(),
            metrics: trunkd_core::Metrics::new(),
            sites: Arc::new(Vec::new()),
        };
        let metrics = shared.metrics.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let (_tx, spectrum_rx) = trunkd_core::chan::bounded::<SpectrumFrame>(1);
        let handle = {
            let (desired, stop, descriptors) = (
                state_for(&[dev("t0", DeviceRole::Control, 851_000_000)]),
                stop.clone(),
                vec![prefetch_descriptor("t0")],
            );
            std::thread::spawn(move || {
                run(shared, descriptors, desired, cfg, spectrum_rx, stop);
            })
        };

        // The loop fires once on startup, whatever the interval.
        assert!(until(|| stub.served() >= 1), "the startup poll");
        assert!(
            until(|| recorder.desired_seen() == Some(4)),
            "version noted"
        );
        let baseline = stub.served();

        // A push: the next GET must land in well under the hour-long interval.
        fetch_now.store(true, Ordering::Relaxed);
        assert!(
            until(|| stub.served() > baseline),
            "the push pulled a GET in"
        );
        assert!(
            !fetch_now.load(Ordering::Relaxed),
            "the flag is taken, not left set"
        );
        assert_eq!(metrics.counter("control.stream_notified").get(), 1);

        // And nothing else polls: the interval is still an hour away.
        let after = stub.served();
        std::thread::sleep(Duration::from_millis(400));
        assert_eq!(
            stub.served(),
            after,
            "no extra polls once the push is spent"
        );

        stop.store(true, Ordering::Relaxed);
        handle.join().expect("joins");
    }

    #[test]
    fn the_seen_desired_version_only_moves_forward() {
        let rc = recorder_control();
        assert_eq!(rc.desired_seen(), None, "nothing fetched yet");
        rc.note_desired_seen(4);
        assert_eq!(rc.desired_seen(), Some(4));
        rc.note_desired_seen(2);
        assert_eq!(rc.desired_seen(), Some(4), "a stale body cannot rewind it");
        rc.note_desired_seen(9);
        assert_eq!(rc.desired_seen(), Some(9));
    }

    fn prefetch_cfg(url: String, timeout: Duration) -> PrefetchConfig {
        PrefetchConfig {
            desired_url: url,
            api_key: "kup_test".into(),
            instance_id: "rec-1".into(),
            timeout,
            ppm_memory_path: std::path::PathBuf::from("/nonexistent/trunkd-ppm.json"),
        }
    }

    fn prefetch_descriptor(id: &str) -> TunerDescriptor {
        TunerDescriptor {
            id: id.into(),
            serial: None,
            backend: Backend::Iqfile,
            driver: None,
            sample_rate: 20_000_000,
            site_label: None,
            site_metric_prefix: None,
            description: TunerDescription::default(),
            channels: Vec::new(),
        }
    }

    /// The desired state the stub serves: a center move and `data_follow`, the
    /// pair that forced a restart one second after every start.
    const PREFETCH_BODY: &str = r#"{
        "version": 4,
        "recorder": { "version": 7, "spool_only": true },
        "tuners": {
            "t0": {
                "version": 4,
                "config": { "data_follow": true, "center_frequency": 855000000 }
            }
        }
    }"#;

    /// The same document as the poll loop would parse it, for the second pass.
    fn prefetched_tuner() -> DesiredTuner {
        parse_desired(PREFETCH_BODY)
            .tuners
            .into_iter()
            .next()
            .expect("one tuner")
            .1
    }

    fn recorder_control() -> Arc<RecorderControl> {
        RecorderControl::for_test(Arc::new(UplinkHold::new(false)))
    }

    #[test]
    fn a_prefetch_makes_the_first_build_use_keyups_config() {
        let stub = StubKeyup::start(Some(PREFETCH_BODY));
        let state = state_for(&[dev("t0", DeviceRole::Auto, 851_000_000)]);
        let recorder = recorder_control();
        prefetch_desired(
            &prefetch_cfg(stub.url(), Duration::from_secs(3)),
            &[prefetch_descriptor("t0")],
            &state,
            &recorder,
        );

        // The build the device factory is about to do already carries them.
        let built = state.take_for_build("t0");
        assert_eq!(built.config.center_frequency, Some(855_000_000));
        assert_eq!(built.config.data_follow, Some(true));
        assert_eq!(built.version, 4);
        assert_eq!(built.generation, 0);
        // It is not applied until that build opens with it: a config that
        // cannot open must stay pending on keyup, restart after restart.
        assert_eq!(state.applied("t0").expect("applied").version, 0);
        assert!(state.is_pending("t0"));
        state.confirm_applied("t0", &built.config, built.version);
        assert_eq!(state.applied("t0").expect("applied").version, 4);
        // The recorder-level block is seeded the same way.
        assert_eq!(applied_pair(&recorder), Some((7, true)));
        assert!(recorder.hold().is_held());
    }

    /// The other half of the same live sequence: after the process restart at
    /// 06:33 the *prefetch* — not a poll — was what built the tuner, and it
    /// built it disabled, so the recorder came up decoding nothing at all. The
    /// prefetch must carry `enabled` through, and a later push must be able to
    /// switch it back on in place.
    #[test]
    fn a_prefetched_disabled_tuner_is_built_disabled_and_can_be_switched_back_on() {
        const OFF: &str = r#"{
            "version": 5,
            "tuners": {
                "t0": {
                    "version": 5,
                    "config": { "enabled": false, "center_frequency": 855500000 }
                }
            }
        }"#;
        let stub = StubKeyup::start(Some(OFF));
        let state = state_for(&[dev("t0", DeviceRole::Auto, 851_000_000)]);
        prefetch_desired(
            &prefetch_cfg(stub.url(), Duration::from_secs(3)),
            &[prefetch_descriptor("t0")],
            &state,
            &recorder_control(),
        );

        let built = state.take_for_build("t0");
        assert!(
            !built.config.enabled,
            "the first build is already the idle one"
        );
        assert_eq!(built.version, 5);

        // And the poll loop can bring it back without a process restart.
        assert_eq!(
            state.apply_desired("t0", &desired(6, Some(855_500_000), true)),
            ApplyDecision::Restart
        );
        assert!(state.take_for_build("t0").config.enabled);
    }

    #[test]
    fn the_poll_after_a_prefetch_does_not_restart_the_tuner() {
        let stub = StubKeyup::start(Some(PREFETCH_BODY));
        let state = state_for(&[dev("t0", DeviceRole::Auto, 851_000_000)]);
        let recorder = recorder_control();
        prefetch_desired(
            &prefetch_cfg(stub.url(), Duration::from_secs(3)),
            &[prefetch_descriptor("t0")],
            &state,
            &recorder,
        );
        let generation = state.generation_of("t0");

        // What the control loop's first poll sees a few hundred ms later.
        assert_eq!(
            state.apply_desired("t0", &prefetched_tuner()),
            ApplyDecision::NoOp
        );
        assert_eq!(state.generation_of("t0"), generation);
        // A redelivery of the recorder block is likewise a no-op.
        let snapshot = parse_desired(PREFETCH_BODY);
        assert!(!recorder.apply_desired(&snapshot.recorder.expect("recorder block")));
    }

    #[test]
    fn a_prefetch_that_cannot_reach_keyup_starts_from_the_toml() {
        // Bound and immediately dropped: the port is closed, so the connect is
        // refused rather than hanging.
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
            l.local_addr().expect("addr").port()
        };
        let state = state_for(&[dev("t0", DeviceRole::Auto, 851_000_000)]);
        let recorder = recorder_control();
        prefetch_desired(
            &prefetch_cfg(
                format!("http://127.0.0.1:{port}/api/recorder/desired-state"),
                Duration::from_secs(3),
            ),
            &[prefetch_descriptor("t0")],
            &state,
            &recorder,
        );

        let built = state.take_for_build("t0");
        assert_eq!(built.config.center_frequency, Some(851_000_000));
        assert_eq!(built.version, 0);
        assert_eq!(recorder.applied(), None);
        // And the poll loop then applies keyup's state exactly as it always has.
        assert_eq!(
            state.apply_desired("t0", &prefetched_tuner()),
            ApplyDecision::Restart
        );
        assert_eq!(state.generation_of("t0"), 1);
    }

    #[test]
    fn a_prefetch_never_blocks_longer_than_its_timeout() {
        let stub = StubKeyup::start(None);
        let state = state_for(&[dev("t0", DeviceRole::Auto, 851_000_000)]);
        let recorder = recorder_control();
        let start = Instant::now();
        prefetch_desired(
            &prefetch_cfg(stub.url(), Duration::from_millis(300)),
            &[prefetch_descriptor("t0")],
            &state,
            &recorder,
        );
        let elapsed = start.elapsed();
        assert!(elapsed < Duration::from_secs(3), "blocked for {elapsed:?}");
        assert_eq!(
            state.take_for_build("t0").config.center_frequency,
            Some(851_000_000)
        );
    }

    #[test]
    fn a_prefetch_is_skipped_without_a_url_or_a_timeout() {
        let state = state_for(&[dev("t0", DeviceRole::Auto, 851_000_000)]);
        let recorder = recorder_control();
        prefetch_desired(
            &prefetch_cfg(String::new(), Duration::from_secs(3)),
            &[prefetch_descriptor("t0")],
            &state,
            &recorder,
        );
        // A configured URL with a zero timeout is the operator's opt-out.
        let stub = StubKeyup::start(Some(PREFETCH_BODY));
        prefetch_desired(
            &prefetch_cfg(stub.url(), Duration::ZERO),
            &[prefetch_descriptor("t0")],
            &state,
            &recorder,
        );
        assert_eq!(state.take_for_build("t0").version, 0);
        assert_eq!(recorder.applied(), None);
    }
}
