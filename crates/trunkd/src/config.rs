// SPDX-License-Identifier: GPL-3.0-or-later
//! Configuration model and loading (TOML file + `TRUNKD_` env overrides).
//!
//! Mirrors ARCHITECTURE §7. The decode-specific site/channel fields are stubbed
//! to just what A0/A1 needs: enough to represent a P25 control channel (a device
//! by Soapy args, a site with control frequencies and a traffic pool) plus
//! conventional NBFM/AM channels.

use std::collections::BTreeMap;
use std::path::Path;

use figment::providers::{Env, Format, Toml};
use figment::Figment;
use serde::{Deserialize, Serialize};
use trunkd_core::{Error, Result};

/// Root configuration document.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    /// Control-plane server settings.
    #[serde(default)]
    pub server: Server,
    /// Operator-entered facts about this station (display/telemetry only).
    #[serde(default)]
    pub station: Station,
    /// Uplink (Keyup) settings.
    #[serde(default)]
    pub uplink: Uplink,
    /// SDR devices.
    #[serde(default)]
    pub device: Vec<Device>,
    /// Trunked P25 sites.
    #[serde(default)]
    pub site: Vec<Site>,
    /// Conventional analog channels.
    #[serde(default)]
    pub channel: Vec<Channel>,
    /// Health thresholds.
    #[serde(default)]
    pub health: Health,
    /// Local, non-uplink recorder options.
    #[serde(default)]
    pub recorder: Recorder,
    /// Recorder-to-recorder link (off by default).
    #[serde(default)]
    pub link: Link,
}

/// `[station]` — who and where this recorder is.
///
/// Every field is operator-entered free text (or a number they typed): nothing
/// here is measured, and nothing here changes what the recorder does. It exists
/// because a fleet of recorders is only useful if each one can say what it is:
/// keyup will later pick the best source for a call among several instances
/// reporting the same site, and "best" is a judgement about antennas, height and
/// location as much as it is about live link quality. It is reported on the
/// tuner-status POST and is one-way — keyup displays it and can never override
/// it, because only the person standing next to the antenna knows the truth.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Station {
    /// Human name for the station, e.g. `"Example County — hilltop"`.
    #[serde(
        default,
        deserialize_with = "de_text",
        skip_serializing_if = "String::is_empty"
    )]
    pub name: String,
    /// Free-text location, e.g. `"Springfield"`. Deliberately not parsed: a
    /// street address, a grid square and a landmark are all valid answers.
    #[serde(
        default,
        deserialize_with = "de_text",
        skip_serializing_if = "String::is_empty"
    )]
    pub location: String,
    /// Latitude in decimal degrees, WGS84. Optional because a great many
    /// stations will never enter one, and a wrong guess is worse than nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latitude: Option<f64>,
    /// Longitude in decimal degrees, WGS84.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub longitude: Option<f64>,
    /// Antenna-site elevation above sea level in metres — the ground, not the
    /// mast; per-tuner mast height lives on the device (`height_m`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elevation_m: Option<f64>,
    /// How to reach whoever runs this box (callsign, email, handle). Shown to
    /// other operators of the same feed, so keep it something you want seen.
    #[serde(
        default,
        deserialize_with = "de_text",
        skip_serializing_if = "String::is_empty"
    )]
    pub contact: String,
    /// Anything else worth knowing: known blind spots, a maintenance window, a
    /// reason this station is temporarily deaf on one band.
    #[serde(
        default,
        deserialize_with = "de_notes",
        skip_serializing_if = "String::is_empty"
    )]
    pub notes: String,
}

impl Station {
    /// Whether the operator filled in nothing at all. The status POST omits the
    /// whole `station` object then, rather than sending an object of empties.
    pub fn is_empty(&self) -> bool {
        self == &Station::default()
    }

    /// Reject an out-of-range coordinate rather than clamping it.
    ///
    /// A clamped longitude is a silent lie: `-1810.0` becoming `-180.0` puts
    /// the station in the Pacific and nothing ever says so, while a typo caught
    /// at startup costs one restart. Same for a swapped lat/lon pair, which
    /// this catches whenever the longitude exceeds 90.
    pub fn validate(&self) -> Result<()> {
        if let Some(lat) = self.latitude {
            if lat.is_nan() || !(-90.0..=90.0).contains(&lat) {
                return Err(Error::config(format!(
                    "station: latitude must be within -90..=90 degrees (got {lat})"
                )));
            }
        }
        if let Some(lon) = self.longitude {
            if lon.is_nan() || !(-180.0..=180.0).contains(&lon) {
                return Err(Error::config(format!(
                    "station: longitude must be within -180..=180 degrees (got {lon})"
                )));
            }
        }
        Ok(())
    }
}

/// Cap on a free-text station/device field, in characters.
///
/// Capped at *parse* rather than at send: a value that survives loading is a
/// value that has already been bounded, so no later consumer (the status POST,
/// a log line, a config dump) has to remember to bound it again. 256 characters
/// is a generous one-liner and still keeps the whole status body small enough
/// to be uninteresting.
pub const MAX_TEXT_LEN: usize = 256;

/// Cap on the free-text `notes` fields, in characters. Four times
/// [`MAX_TEXT_LEN`]: notes are where an operator writes a paragraph.
pub const MAX_NOTES_LEN: usize = 1024;

/// Deserialize a free-text field, truncated to [`MAX_TEXT_LEN`] characters.
fn de_text<'de, D: serde::Deserializer<'de>>(d: D) -> std::result::Result<String, D::Error> {
    Ok(truncate_chars(&String::deserialize(d)?, MAX_TEXT_LEN))
}

/// Deserialize a notes field, truncated to [`MAX_NOTES_LEN`] characters.
fn de_notes<'de, D: serde::Deserializer<'de>>(d: D) -> std::result::Result<String, D::Error> {
    Ok(truncate_chars(&String::deserialize(d)?, MAX_NOTES_LEN))
}

/// `[recorder]` — what the recorder keeps on the box itself.
///
/// Distinct from `[uplink]`, which is about what leaves it: nothing here is
/// ever sent anywhere. See `docs/NON-VOICE-DATA.md` §7 for why a day-long log
/// of the control channel is worth its disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Recorder {
    /// Directory for the raw non-voice event log, relative to the working
    /// directory. Empty disables the log.
    #[serde(default = "default_event_log_dir")]
    pub event_log_dir: String,
    /// Whether the event log is written at all.
    #[serde(default = "default_true")]
    pub event_log: bool,
    /// Days of event log kept; older files are deleted at rotation. 0 keeps
    /// them forever.
    #[serde(default = "default_event_log_keep_days")]
    pub event_log_keep_days: u16,
    /// Whether traffic-channel signaling (link control, HDU, PDU, LSD) is
    /// logged as well as the control channel.
    ///
    /// Voice is never logged either way: the engine drops IMBE frames from the
    /// pass-through at the source.
    #[serde(default = "default_true")]
    pub event_log_traffic: bool,
    /// Upload each finished day's `.jsonl.gz` to keyup once it is complete.
    ///
    /// Off by default, like everything that sends a new kind of data: the event
    /// log is deliberately the one thing that stays on the box, and shipping a
    /// few hundred megabytes a day is a decision an operator makes rather than
    /// inherits. When on, a `<file>.archived` marker beside each day records
    /// keyup's reply, and days finished while the recorder was off (or keyup
    /// was unreachable) are caught by the startup scan.
    #[serde(default)]
    pub event_log_archive: bool,
    /// Whether per-radio events are batched and POSTed to Keyup.
    #[serde(default = "default_true")]
    pub radio_events: bool,
    /// Cap on the undelivered radio-event spool, in bytes.
    #[serde(default = "default_event_spool_cap")]
    pub event_spool_cap_bytes: u64,
    /// Depth of the DSP -> observations/event-log queue, in events.
    ///
    /// The queue is drop-oldest: when it fills, the oldest event is evicted and
    /// counted, which the status line reports as `event_log_dropped`. Raise this
    /// if that counter climbs; see `docs/operations.md`.
    #[serde(default = "default_event_queue_depth")]
    pub event_queue_depth: usize,
    /// Stable identifier for this trunkd instance, used as the `instance_id` on
    /// the keyup control loop (desired-state and tuner-status endpoints). Empty
    /// falls back to the machine hostname at runtime; see
    /// [`Recorder::resolved_instance_id`].
    #[serde(default)]
    pub instance_id: String,
    /// Where the per-board ppm memory lives (see
    /// [`crate::pipeline::ppmmemory`]). Empty picks the default,
    /// `<[uplink] spool_path>/../state/ppm-memory.json`.
    ///
    /// One file for the whole recorder, keyed by board rather than by
    /// `[[device]]` id, so a tuner that has already been measured re-opens on
    /// its known offset and locks immediately instead of hunting its way back
    /// to it on every restart.
    #[serde(default)]
    pub ppm_memory_path: String,
    /// Fewest voice frames a finished call must carry to be encoded and
    /// uploaded at all.
    ///
    /// A call that ends with none is a grant the traffic channel never
    /// delivered: signaling identified it, so it is not caught by the
    /// "insubstantive fragment" rule, but there is nothing to hear. Encoding
    /// one produces a 288-byte MP3 that is a LAME header and no audio, which
    /// Keyup's decoder then fails on — ~40 a day on a busy site. The default
    /// of 1 discards exactly those and nothing else.
    ///
    /// The call still *happened*, and this only decides whether its audio is
    /// shipped: the control-channel picture Keyup builds is made of
    /// observations and radio events, which are emitted from the DSP side and
    /// are not touched by this.
    ///
    /// 0 restores the old behaviour (upload everything).
    #[serde(default = "default_min_voice_frames")]
    pub min_voice_frames: u32,
    /// Follow unit-to-unit (private) voice grants, default **on**.
    ///
    /// The measured site carries its private-call setup as multi-block AMBT
    /// messages, which this recorder decoded and logged but never acted on:
    /// ~45-60 conversations and about eight minutes of clear audio a day that
    /// it knew about and declined to record
    /// (`private/docs/analysis/unknown-phase3-2026-09-05.md` §15). On, such a grant
    /// competes for a traffic channel exactly as an unpatched group call does,
    /// and the call is uploaded with `call_kind=unit` and `target_radio_id`
    /// instead of a talkgroup.
    ///
    /// Off leaves the grants decoded, counted and logged but unfollowed — for a
    /// recorder whose operator does not want two-party conversations on the
    /// archive.
    #[serde(default = "default_true")]
    pub follow_unit_calls: bool,
    /// Sanity box a decoded LRRP location must land in, as
    /// `[lat_min, lon_min, lat_max, lon_max]`. An **empty list** means no box.
    ///
    /// The default is a generous CONUS rectangle. It exists because the
    /// 2026-09-04 phase-3 analysis found every `location` in a four-day event
    /// log to be false — one constant South Atlantic coordinate read out of the
    /// padding behind an ordinary outbound poll. The box is the last of several
    /// plausibility rules (see [`crate::pipeline::lrrp_gate`]) and the only one
    /// an operator outside CONUS has to touch.
    #[serde(default = "default_location_bounds")]
    pub location_bounds: Vec<f64>,
    /// Talkgroups this recorder never records: no tuner is claimed, no call is
    /// opened, nothing is uploaded, and nothing is published for a sibling
    /// tuner or a link peer to record either.
    ///
    /// This is the operator's own half of the list. Keyup pushes the other half
    /// as `recorder.skip_talkgroups` — the talkgroups whose Record, Transcribe
    /// and every export switch it has turned off — and the two are **unioned**:
    /// keyup can add to this list at runtime but can never take an entry of it
    /// away. The grant is still decoded and logged, and the radio events around
    /// it are still archived, so the record of the radio keying up survives; it
    /// is only the recording that does not happen.
    #[serde(default)]
    pub skip_talkgroups: Vec<u32>,
}

impl Default for Recorder {
    fn default() -> Self {
        Recorder {
            event_log_dir: default_event_log_dir(),
            event_log: true,
            event_log_keep_days: default_event_log_keep_days(),
            event_log_traffic: true,
            event_log_archive: false,
            radio_events: true,
            event_spool_cap_bytes: default_event_spool_cap(),
            event_queue_depth: default_event_queue_depth(),
            instance_id: String::new(),
            ppm_memory_path: String::new(),
            min_voice_frames: default_min_voice_frames(),
            follow_unit_calls: true,
            location_bounds: default_location_bounds(),
            skip_talkgroups: Vec::new(),
        }
    }
}

/// The default LRRP sanity box: `[lat_min, lon_min, lat_max, lon_max]` over a
/// generous CONUS rectangle.
fn default_location_bounds() -> Vec<f64> {
    crate::pipeline::lrrp_gate::DEFAULT_LOCATION_BOUNDS.to_vec()
}

/// keyup caps an `instance_id` at 128 characters.
pub const MAX_INSTANCE_ID_LEN: usize = 128;

impl Recorder {
    /// The instance id to present to keyup: the configured value, or the machine
    /// hostname when it is blank, or `"trunkd"` as a last resort. Always trimmed
    /// to keyup's [`MAX_INSTANCE_ID_LEN`] on a character boundary.
    pub fn resolved_instance_id(&self) -> String {
        let raw = if self.instance_id.trim().is_empty() {
            hostname().unwrap_or_else(|| "trunkd".to_owned())
        } else {
            self.instance_id.trim().to_owned()
        };
        truncate_chars(&raw, MAX_INSTANCE_ID_LEN)
    }

    /// The LRRP sanity box as `[lat_min, lon_min, lat_max, lon_max]`, or `None`
    /// when the list is empty (no box). Assumes [`Recorder::validate`] has
    /// already rejected a list that is neither empty nor four numbers.
    pub fn location_bounds_box(&self) -> Option<[f64; 4]> {
        let b = self.location_bounds.as_slice();
        (b.len() == 4).then(|| [b[0], b[1], b[2], b[3]])
    }

    /// Reject a queue depth that cannot work rather than clamping it silently.
    ///
    /// Too small and the recorder shed events by construction; too large and a
    /// stalled observations thread turns into unbounded memory. Both are worth
    /// one startup error.
    pub fn validate(&self) -> Result<()> {
        if !(MIN_EVENT_QUEUE_DEPTH..=MAX_EVENT_QUEUE_DEPTH).contains(&self.event_queue_depth) {
            return Err(Error::config(format!(
                "recorder: event_queue_depth must be within \
                 {MIN_EVENT_QUEUE_DEPTH}..={MAX_EVENT_QUEUE_DEPTH} (got {})",
                self.event_queue_depth
            )));
        }
        if !self.location_bounds.is_empty() {
            let b = self.location_bounds.as_slice();
            if b.len() != 4 {
                return Err(Error::config(format!(
                    "recorder: location_bounds must be empty or \
                     [lat_min, lon_min, lat_max, lon_max] (got {} values)",
                    b.len()
                )));
            }
            let sane = (-90.0..=90.0).contains(&b[0])
                && (-90.0..=90.0).contains(&b[2])
                && (-180.0..=180.0).contains(&b[1])
                && (-180.0..=180.0).contains(&b[3])
                && b[0] < b[2]
                && b[1] < b[3];
            if !sane {
                return Err(Error::config(
                    "recorder: location_bounds must be a real box: \
                     lat_min < lat_max within -90..90, lon_min < lon_max within -180..180"
                        .to_owned(),
                ));
            }
        }
        Ok(())
    }
}

/// Best-effort machine hostname, without pulling in a crate for it. Reads the
/// Linux kernel hostname, then `$HOSTNAME`; `None` if neither is usable.
fn hostname() -> Option<String> {
    let from_proc = std::fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty());
    from_proc.or_else(|| std::env::var("HOSTNAME").ok().filter(|s| !s.is_empty()))
}

/// Truncate `s` to at most `max` characters (not bytes), never splitting a
/// multi-byte character.
pub(crate) fn truncate_chars(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((byte_idx, _)) => s[..byte_idx].to_owned(),
        None => s.to_owned(),
    }
}

fn default_event_log_dir() -> String {
    "events".into()
}

fn default_event_log_keep_days() -> u16 {
    14
}

/// 64 MiB: a batch of 500 events is roughly 100 kB, so this holds days of
/// outage at the measured ~600 events/hour.
fn default_event_spool_cap() -> u64 {
    64 * 1024 * 1024
}

/// Smallest accepted `[recorder] event_queue_depth`.
///
/// Below a few hundred events the queue cannot absorb an ordinary scheduling
/// quantum on the observations thread, let alone one HTTP POST, so the recorder
/// would shed events as a matter of course rather than under load.
pub const MIN_EVENT_QUEUE_DEPTH: usize = 256;

/// Largest accepted `[recorder] event_queue_depth`: 1 Mi events, about 112 MiB
/// of resident queue at the 112 bytes an `ObsMsg` occupies (plus any PDU
/// payload it carries). Past that a wedged observations thread is a memory
/// problem, not a buffering one.
pub const MAX_EVENT_QUEUE_DEPTH: usize = 1 << 20;

/// 16 384 events.
///
/// The queue drains as fast as the event log can be written, so its depth only
/// has to cover the observations thread being busy elsewhere — a rotation, a
/// gzip, or one 10 s uplink POST — while a radio runs a data session. Those
/// sessions peak around 500 PDU-class events a minute on this system, but the
/// bursts that shed at the previous fixed depth of 4 096 lost up to ~450 events
/// in a window, so the live worst case is roughly 4.5 k queued at once. 16 384
/// is ~3.6x that headroom and costs about 1.75 MiB resident (112 bytes per
/// `ObsMsg`, plus the reassembled payload a `Pdu` event carries) — cheap enough
/// to be the default, small enough that a wedged consumer is still bounded.
fn default_event_queue_depth() -> usize {
    16_384
}

/// One: discard a call that carried no voice at all, keep everything else.
fn default_min_voice_frames() -> u32 {
    1
}

/// `[server]` — the local control panel: bind address, auth tokens, logging.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Server {
    /// Whether the local control panel is served at all.
    ///
    /// Off by default, deliberately: most deployments are headless boxes that
    /// nobody logs into, and a listening port nobody asked for is a liability
    /// rather than a feature. Turning it on is a decision.
    #[serde(default)]
    pub enabled: bool,
    /// Panel bind address. Loopback by default, so an enabled panel is still
    /// reachable only from the box itself (or through an SSH tunnel); set it to
    /// `0.0.0.0:PORT` deliberately to expose it on the LAN.
    #[serde(default = "default_bind")]
    pub bind: String,
    /// Bearer tokens accepted by the panel's `/api/*` routes.
    ///
    /// Non-empty: every API request must carry `Authorization: Bearer <token>`
    /// matching one of these. Empty: reads are open and mutations (the tuner
    /// restart) are restricted to loopback peers — a LAN-exposed read-only
    /// panel is a choice, a LAN-exposed restart button with no auth is not.
    /// Empty or whitespace-only entries are ignored at load with a warning;
    /// they never authenticate anything.
    #[serde(default)]
    pub tokens: Vec<String>,
    /// Whether the panel will serve live decoded audio (`/api/monitor/stream`).
    ///
    /// Off by default, and a separate decision from [`enabled`](Self::enabled):
    /// a panel is a status page, while this is a live tap on the voice traffic
    /// the recorder is decoding. It is there so an operator can hear whether
    /// what this box is producing is intelligible — a misaligned tuner uploads
    /// perfectly and sounds like gravel — not as a scanner. Listening to
    /// finished calls stays in Keyup.
    ///
    /// Costs nothing when off: the fan-out hub is not constructed, so the call
    /// thread publishes nothing. Encrypted calls never carry audio here for
    /// the same reason they never reach an upload — the voice is discarded
    /// before it is vocoded.
    #[serde(default)]
    pub live_audio: bool,
    /// How the panel is reached from elsewhere, for the link Keyup shows on
    /// its own status page (`http://host:port`, no trailing path).
    ///
    /// Unset, the recorder derives one from [`bind`](Self::bind) — and a
    /// wildcard bind derives `127.0.0.1`, which is honest: it works from the
    /// recorder's own browser and nowhere else. Set it when the panel is bound
    /// to the LAN and should be clickable from Keyup.
    #[serde(default)]
    pub public_url: Option<String>,
    /// Log verbosity (`error`|`warn`|`info`|`debug`|`trace`). `RUST_LOG` wins.
    #[serde(default = "default_log_level")]
    pub log_level: String,
    /// Optional directory for rolling log files; console logging is always on.
    #[serde(default)]
    pub log_dir: Option<String>,
}

impl Server {
    /// Drop every blank `tokens` entry and trim the rest, returning how many
    /// were dropped.
    ///
    /// An empty string is not a credential: `tokens = [""]` would otherwise
    /// switch the panel into bearer-token mode and then accept
    /// `Authorization: Bearer ` (or `?token=`) with nothing after it — every
    /// route unlocked by a typo. Trimmed because the panel trims what a client
    /// presents, so a configured token with stray whitespace could never match
    /// anyway.
    pub fn drop_blank_tokens(&mut self) -> usize {
        let before = self.tokens.len();
        self.tokens = std::mem::take(&mut self.tokens)
            .into_iter()
            .map(|t| t.trim().to_owned())
            .filter(|t| !t.is_empty())
            .collect();
        before - self.tokens.len()
    }

    /// Where the panel is reachable, as a base URL with no trailing slash.
    ///
    /// `public_url` verbatim when the operator set one; otherwise derived from
    /// the bind address, with a wildcard host (`0.0.0.0`, `[::]`, `::`)
    /// rendered as `127.0.0.1` — a link that only works from this machine is
    /// better than one that names an address nothing can connect to.
    pub fn public_base_url(&self) -> String {
        if let Some(u) = self.public_url.as_ref().map(|u| u.trim()) {
            if !u.is_empty() {
                return u.trim_end_matches('/').to_owned();
            }
        }
        let bind = self.bind.trim();
        // `[::1]:8095` and `0.0.0.0:8095` both split at the last colon; a
        // bracketed host keeps its brackets, which is what a URL wants anyway.
        let (host, port) = match bind.rsplit_once(':') {
            Some((h, p)) if !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) => (h, p),
            _ => (bind, "80"),
        };
        let host = match host {
            "0.0.0.0" | "[::]" | "::" | "" => "127.0.0.1",
            h => h,
        };
        format!("http://{host}:{port}")
    }
}

impl Default for Server {
    fn default() -> Self {
        Server {
            enabled: false,
            bind: default_bind(),
            tokens: Vec::new(),
            public_url: None,
            live_audio: false,
            log_level: default_log_level(),
            log_dir: None,
        }
    }
}

/// `[uplink]` — Keyup target and offline spool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Uplink {
    /// Keyup base URL (Rdio-Scanner upload contract).
    #[serde(default)]
    pub url: String,
    /// API key for the upload contract.
    #[serde(default)]
    pub api_key: String,
    /// Offline spool directory.
    #[serde(default = "default_spool_path")]
    pub spool_path: String,
    /// Spool size cap in bytes (oldest-first eviction).
    #[serde(default = "default_spool_cap")]
    pub spool_cap_bytes: u64,
    /// Free-space floor for the volume holding the spool, in bytes.
    ///
    /// The cap above bounds what *trunkd* keeps; this bounds what the whole
    /// filesystem must keep free, whoever is using it. Whenever less than this
    /// much is available the oldest spooled calls (and radio-event batches) are
    /// evicted until it is — a box whose disk is being filled by logs, a second
    /// recorder, or an operator's copy would otherwise hit `ENOSPC` with a
    /// spool nowhere near its cap, and a recorder that cannot write loses calls
    /// it has already decoded. 0 disables the floor.
    #[serde(default = "default_spool_min_free")]
    pub spool_min_free_bytes: u64,
    /// System label sent as `systemLabel`; Keyup files calls under it.
    #[serde(default)]
    pub system_label: String,
    /// Site label recorded on every call.
    #[serde(default)]
    pub site_label: String,
    /// Numeric `system` id. Keyup accepts and ignores it.
    #[serde(default = "default_system_id")]
    pub system_id: u32,
    /// Upload/spool audio encoding.
    #[serde(default)]
    pub audio_format: AudioFormatName,
    /// MP3 constant bitrate in kbit/s (ignored for WAV).
    #[serde(default = "default_mp3_bitrate")]
    pub mp3_bitrate_kbps: u32,
    /// Seconds between site-observation POSTs. 0 disables them.
    #[serde(default = "default_observations_interval")]
    pub observations_interval_s: u64,
    /// Seconds to wait for the one synchronous desired-state GET made *before*
    /// the tuners are built, so the first build already carries keyup's
    /// overrides and the process does not restart a tuner a second after its
    /// first lock. 0 disables the prefetch (the poll loop then applies keyup's
    /// state a few hundred ms in, with a restart if it differs).
    #[serde(default = "default_desired_prefetch_timeout")]
    pub desired_prefetch_timeout_s: u64,
    /// Whether to hold a WebSocket **push channel** open to keyup (W6).
    ///
    /// The channel carries no configuration: keyup sends a version number and
    /// the recorder answers by making the desired-state GET it would have made
    /// on its next poll anyway. So it is latency, not a second control path —
    /// turning it off leaves the poll loop as the only mechanism and costs
    /// nothing but the seconds between a keyup change and it taking effect.
    #[serde(default = "default_true")]
    pub stream: bool,
    /// Ceiling on the push channel's reconnect backoff, in seconds.
    ///
    /// The backoff doubles from 1 s and stops here, so a keyup that is down for
    /// an hour is retried every `stream_reconnect_max_s` rather than never.
    #[serde(default = "default_stream_reconnect_max")]
    pub stream_reconnect_max_s: u64,
    /// Whether calls are delivered at all. When false they are dropped after
    /// assembly (metrics still count them).
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Boot value of the **upload hold**: write every call and radio event
    /// straight to its spool and skip the observations POST.
    ///
    /// Not an offline switch. The keyup control plane stays connected — status
    /// up, desired state down — so the hold can be released from keyup or the
    /// local panel without editing this file, and the most recent writer wins.
    /// Use `enabled = false` for a recorder that must talk to nobody.
    #[serde(default)]
    pub spool_only: bool,
    /// Whether this box may hold Broadcastify Calls credentials and upload each
    /// finished call to Broadcastify directly, when keyup tells it to.
    ///
    /// The **recorder-side** half of the switch, and a veto rather than a
    /// trigger: keyup's `recorder.broadcastify` block is what turns the feature
    /// on, and setting this to `false` makes every such block be refused, so an
    /// operator can keep a given recorder out of the arrangement without keyup's
    /// cooperation. Nothing happens on a box keyup has said nothing to.
    #[serde(default = "default_true")]
    pub direct_broadcastify: bool,
}

/// `[uplink] audio_format` — the wire name for [`trunkd_call::AudioFormat`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum AudioFormatName {
    /// LAME MP3, mono CBR (the default: every downstream consumer takes it).
    #[default]
    Mp3,
    /// Uncompressed 8 kHz mono s16le RIFF/WAVE.
    Wav,
}

impl Default for Uplink {
    fn default() -> Self {
        Uplink {
            url: String::new(),
            api_key: String::new(),
            spool_path: default_spool_path(),
            spool_cap_bytes: default_spool_cap(),
            spool_min_free_bytes: default_spool_min_free(),
            system_label: String::new(),
            site_label: String::new(),
            system_id: default_system_id(),
            audio_format: AudioFormatName::default(),
            mp3_bitrate_kbps: default_mp3_bitrate(),
            observations_interval_s: default_observations_interval(),
            desired_prefetch_timeout_s: default_desired_prefetch_timeout(),
            stream: true,
            stream_reconnect_max_s: default_stream_reconnect_max(),
            enabled: true,
            spool_only: false,
            direct_broadcastify: true,
        }
    }
}

/// What a `[[device]]` (tuner) does for the site(s) it serves (multi-tuner
/// core, W1). Mirrors [`trunkd_p25::EngineRole`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum DeviceRole {
    /// Control **and** voice in one channelizer — today's single-tuner
    /// behavior, and the default.
    #[default]
    Auto,
    /// Control-channel follow only; publishes grants to the coordinator.
    Control,
    /// Traffic grabber only; records grants routed to it.
    Voice,
    /// Reserved for the SNDCP data follower (decode path stubbed).
    Data,
}

/// Which backend serves a `[[device]]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Backend {
    /// Any SDR SoapySDR has a module for — Airspy, SDRplay, RTL-SDR — through
    /// the SoapySDR C ABI loaded at run time (the `soapy` cargo feature, on by
    /// default; only the shared library is needed, and only when a Soapy
    /// `[[device]]` is actually configured).
    #[default]
    Soapy,
    /// IQ-file playback (the test source).
    Iqfile,
    /// A HackRF One driven through the `hackrf_transfer` CLI (the `hackrf`
    /// cargo feature, on by default).
    Hackrf,
}

/// `[[device]]` — one SDR.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Device {
    /// Stable device id, referenced by sites/channels.
    pub id: String,
    /// Backend selector.
    #[serde(default)]
    pub backend: Backend,
    /// Verbatim Soapy args string, e.g. `driver=airspy,serial=deadbeef` (when
    /// `backend = "soapy"`). Overrides [`Device::driver`]; set one or the
    /// other. The escape hatch for driver args trunkd has no field for.
    #[serde(default)]
    pub soapy: Option<String>,
    /// SoapySDR driver module to open: `airspy`, `sdrplay`, `rtlsdr`, ... The
    /// ordinary way to name a Soapy device; combined with [`Device::serial`]
    /// into the args string. See [`Device::effective_soapy_args`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub driver: Option<String>,
    /// Per-element gains for a Soapy device, by the driver's own element names
    /// (Airspy `LNA`/`MIX`/`VGA`, SDRplay `IFGR`/`RFGR`, RTL-SDR `TUNER`).
    /// Applied after [`Device::gain`], so these win. The open-time info log
    /// lists every element the driver has, with its range.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub gains: BTreeMap<String, f64>,
    /// Hardware AGC for a Soapy device. Omit to leave the driver's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agc: Option<bool>,
    /// Analog filter bandwidth in Hz for a Soapy device. Worth setting on an
    /// SDRplay, which otherwise filters the edges off a wide capture.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bandwidth_hz: Option<u32>,
    /// RX port/antenna name a Soapy driver exposes, e.g. `"Antenna B"` on an
    /// RSPdx. Distinct from [`Device::antenna`], which is prose about the
    /// aerial on the mast and changes nothing the radio does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rx_port: Option<String>,
    /// Driver settings written with `writeSetting`, e.g. `biastee = "false"`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub settings: BTreeMap<String, String>,
    /// IQ file path (when `backend = "iqfile"`).
    #[serde(default)]
    pub path: Option<String>,
    /// Complex sample rate in samples/sec.
    pub sample_rate: u32,
    /// Gain in dB; omit for hardware default.
    #[serde(default)]
    pub gain: Option<f64>,
    /// Frequency correction in ppm.
    #[serde(default)]
    pub ppm: f64,
    /// Correct the tuner's frequency automatically while it runs.
    ///
    /// On by default because the failure it fixes is silent: a board whose
    /// TCXO is ~1 ppm out streams perfectly and simply never acquires, and
    /// `ppm` alone is applied once at open and never revisited. The recorder
    /// measures the control candidates' carrier offset and re-tunes the board
    /// until they are centred. Set `false` to pin a board to `ppm` exactly.
    #[serde(default = "default_true")]
    pub auto_ppm: bool,
    /// Ceiling on the total automatic correction, in ppm.
    ///
    /// A guard rail, not a tuning knob: real oscillator error is a few ppm, so
    /// anything past this is a mis-measurement (or a site that is genuinely
    /// not there) and walking the tuner off the site would be worse than
    /// leaving it where the operator put it.
    #[serde(default = "default_auto_ppm_limit")]
    pub auto_ppm_limit: f64,
    /// Seconds of samples the SDR ring holds before it must drop.
    ///
    /// The ring is the only thing between a device that never pauses and a DSP
    /// thread that can be descheduled. 0.2 s was not enough: a build running
    /// beside the recorder on the same laptop stalled the DSP thread for ~2 s
    /// and cost 4.6 M samples. 1.0 s is rounded up to 2^25 samples, 256 MiB at
    /// 20 Msps — real memory, and the reason this is a knob rather than a
    /// constant.
    #[serde(default = "default_ring_seconds")]
    pub ring_seconds: f64,
    /// Center (tune) frequency in Hz. Required by `backend = "hackrf"`, which
    /// has no way to derive one; the frequency planner will own this later.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub center_frequency: Option<u64>,
    /// HackRF LNA (IF) gain in dB: 0–40 in 8 dB steps.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lna_db: Option<u32>,
    /// HackRF VGA (baseband) gain in dB: 0–62 in 2 dB steps.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vga_db: Option<u32>,
    /// HackRF front-end RF amplifier (+14 dB) enable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub amp: Option<bool>,
    /// Board serial number, to pin a specific device when several are attached.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub serial: Option<String>,
    /// PortaPack/Mayhem CDC console (e.g. `/dev/ttyACM0`). When set, the board
    /// is switched out of the PortaPack UI into HackRF mode before opening.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub portapack_port: Option<String>,
    /// Reset the board back into the PortaPack UI when trunkd exits. Off by
    /// default: an unattended recorder that restarts wants HackRF mode to
    /// persist.
    #[serde(default)]
    pub restore_portapack_on_exit: bool,
    /// IQ-file playback pacing (`backend = "iqfile"`).
    #[serde(default)]
    pub pacing: PacingName,
    /// What this tuner does for the site(s) it serves (multi-tuner core, W1).
    /// `auto` (the default) is today's combined control+voice behavior.
    #[serde(default)]
    pub role: DeviceRole,
    /// Receiver hardware description, e.g. `"HackRF One + Mayhem"`. Free text,
    /// because the useful answer is the operator's ("RTL-SDR v4 clone", "Airspy
    /// R2 borrowed from the club") and no enum survives contact with a shelf of
    /// SDRs. Empty reports a default derived from the backend at status time.
    #[serde(
        default,
        deserialize_with = "de_text",
        skip_serializing_if = "String::is_empty"
    )]
    pub receiver: String,
    /// Antenna description, e.g. `"Diamond X50 @ 8m"`. The single biggest
    /// predictor of which of two recorders heard a call better, and the one
    /// thing no amount of telemetry can infer.
    #[serde(
        default,
        deserialize_with = "de_text",
        skip_serializing_if = "String::is_empty"
    )]
    pub antenna: String,
    /// Feedline description, e.g. `"25 m LMR-400"`. Loss lives here, and it is
    /// why two stations with the same antenna do not hear the same thing.
    #[serde(
        default,
        deserialize_with = "de_text",
        skip_serializing_if = "String::is_empty"
    )]
    pub cable: String,
    /// External amplifier / filter chain, e.g. `"NooElec LaNA + 800 MHz BPF"`.
    ///
    /// Distinct from [`Device::amp`], which is the HackRF's own +14 dB
    /// front-end amp (a boolean the hardware honours). This one is prose about
    /// what is bolted to the mast and changes nothing the recorder does.
    #[serde(
        default,
        deserialize_with = "de_text",
        skip_serializing_if = "String::is_empty"
    )]
    pub amplifier: String,
    /// Height of *this tuner's* antenna above ground, in metres. Per-device
    /// rather than per-station: one box can feed a mast antenna and a desk
    /// whip, and they are not the same receiver.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height_m: Option<f64>,
    /// Anything else about this tuner's hardware worth recording.
    #[serde(
        default,
        deserialize_with = "de_notes",
        skip_serializing_if = "String::is_empty"
    )]
    pub notes: String,
    /// Downlink band labels this tuner is responsible for recording, e.g.
    /// `["700", "800"]`. Empty means the whole usable span. Each label must
    /// name a known band whose range overlaps the tuner's capture; a `voice`
    /// tuner is routed grants inside these bands, and an `auto` tuner keeps
    /// grants inside them local and publishes the rest.
    #[serde(default)]
    pub bands: Vec<String>,
    /// Optional site this tuner focuses on. Advisory today (a tuner already
    /// serves every site whose `devices` list names it); reserved for the
    /// frequency planner and the keyup control loop.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub site: Option<String>,
    /// Whether this tuner accepts grants heard by a *peer* recorder over the
    /// `[link]`. `None` takes the default for its role: a `voice` or `data`
    /// tuner exists to be handed traffic and takes them; an `auto` or
    /// `control` tuner is busy with its own control channel and does not,
    /// until the operator says otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_grants: Option<bool>,
}

impl Device {
    /// Whether this tuner accepts grants from a peer recorder.
    ///
    /// See [`Device::remote_grants`] for why the roles differ by default: the
    /// point of a link is to give idle traffic capacity somewhere to look, not
    /// to pull a control front off its own site.
    pub fn accepts_remote_grants(&self) -> bool {
        self.remote_grants
            .unwrap_or(matches!(self.role, DeviceRole::Voice | DeviceRole::Data))
    }
}

/// `[[device]] pacing` — the wire name for [`trunkd_sdr::Pacing`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum PacingName {
    /// Emit samples at the configured sample rate, as hardware would.
    #[default]
    Realtime,
    /// Emit as fast as the disk allows (offline replay, tests).
    Unthrottled,
}

/// HackRF LNA gain step in dB.
///
/// Duplicated from `trunkd_sdr::hackrf` deliberately: config validation must
/// work regardless of which backend features are compiled in. Kept in sync with
/// `trunkd_sdr::hackrf::validate_gains`.
const HACKRF_LNA_STEP_DB: u32 = 8;
/// HackRF maximum LNA gain in dB.
const HACKRF_LNA_MAX_DB: u32 = 40;
/// HackRF VGA gain step in dB.
const HACKRF_VGA_STEP_DB: u32 = 2;
/// HackRF maximum VGA gain in dB.
const HACKRF_VGA_MAX_DB: u32 = 62;

impl Device {
    /// The Soapy args string this device opens with.
    ///
    /// An explicit `soapy` string wins; otherwise it is built from `driver`
    /// plus, when set, `serial` to pin one board out of several.
    pub fn effective_soapy_args(&self) -> String {
        trunkd_sdr::soapy_args(
            self.soapy.as_deref(),
            self.driver.as_deref(),
            self.serial.as_deref(),
        )
    }

    /// Validate the `backend = "soapy"` fields: the device must be nameable and
    /// have a center frequency, exactly as `hackrf` does.
    fn validate_soapy(&self) -> Result<()> {
        if self.effective_soapy_args().is_empty() {
            return Err(Error::config(format!(
                "device '{}': backend 'soapy' requires a `driver` (airspy, sdrplay, \
                 rtlsdr, ...) or an explicit `soapy` args string",
                self.id
            )));
        }
        if self.center_frequency.is_none() {
            return Err(Error::config(format!(
                "device '{}': backend 'soapy' requires a `center_frequency`",
                self.id
            )));
        }
        Ok(())
    }
}

/// Check a HackRF LNA gain against the hardware's steps.
///
/// Split out of [`Device::validate_hackrf`] because the keyup control loop
/// validates a *pushed* gain by exactly the same rule, and two copies of a
/// hardware constraint is one copy too many. `Err` carries the reason, which
/// the control loop reports back as a `rejected` entry.
pub(crate) fn check_hackrf_lna(lna: u32) -> std::result::Result<(), String> {
    if lna > HACKRF_LNA_MAX_DB || !lna.is_multiple_of(HACKRF_LNA_STEP_DB) {
        return Err(format!(
            "must be 0..={HACKRF_LNA_MAX_DB} in {HACKRF_LNA_STEP_DB} dB steps, got {lna}"
        ));
    }
    Ok(())
}

/// Check a HackRF VGA gain against the hardware's steps. See
/// [`check_hackrf_lna`].
pub(crate) fn check_hackrf_vga(vga: u32) -> std::result::Result<(), String> {
    if vga > HACKRF_VGA_MAX_DB || !vga.is_multiple_of(HACKRF_VGA_STEP_DB) {
        return Err(format!(
            "must be 0..={HACKRF_VGA_MAX_DB} in {HACKRF_VGA_STEP_DB} dB steps, got {vga}"
        ));
    }
    Ok(())
}

impl Device {
    /// Validate the `backend = "hackrf"` fields: a center frequency is
    /// mandatory and both gains must land on the hardware's steps.
    fn validate_hackrf(&self) -> Result<()> {
        if self.center_frequency.is_none() {
            return Err(Error::config(format!(
                "device '{}': backend 'hackrf' requires a `center_frequency`",
                self.id
            )));
        }
        if let Some(lna) = self.lna_db {
            check_hackrf_lna(lna)
                .map_err(|e| Error::config(format!("device '{}': lna_db {e}", self.id)))?;
        }
        if let Some(vga) = self.vga_db {
            check_hackrf_vga(vga)
                .map_err(|e| Error::config(format!("device '{}': vga_db {e}", self.id)))?;
        }
        Ok(())
    }
}

/// `[[site]]` — one P25 trunked site.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Site {
    /// System label (e.g. "Example Network").
    pub system: String,
    /// Site label (e.g. "Example County").
    pub site: String,
    /// Single device id that serves this site — the historical form. Kept for
    /// backward compatibility; prefer `devices`. When `devices` is empty this
    /// device is the site's one-element tuner set.
    #[serde(default)]
    pub device: String,
    /// The set of tuners that serve this site (multi-tuner core, W1). Each is a
    /// `[[device]]` id with its own role and bands. Empty falls back to the
    /// single `device`. Use [`Site::device_ids`] rather than either field.
    #[serde(default)]
    pub devices: Vec<String>,
    /// Whether the site is started at run time. Disabled sites stay in the
    /// config (they are toggled via the control API) but are not decoded.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Control channel + alternates, in Hz. First is primary.
    #[serde(default)]
    pub control_frequencies: Vec<u64>,
    /// Traffic-channel recorder pool size (bounded; exhaustion is counted).
    #[serde(default = "default_pool_size")]
    pub traffic_pool_size: usize,
    /// Delay in ms before rotating to the next control frequency while hunting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rotation_delay_ms: Option<u64>,
    /// How long a traffic subscription is held after a call ends, so a
    /// follow-on transmission on the same frequency is caught in full.
    /// Unset uses the engine default (2000 ms).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hang_ms: Option<u64>,
    /// A call with no voice or signaling for this long is closed. Unset uses
    /// the engine default (3000 ms).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_inactivity_ms: Option<u64>,
    /// Phase 2 support (stub; decode wiring lands in A6).
    #[serde(default)]
    pub phase2: bool,
    /// Follow SNDCP data-channel grants and reassemble their PDUs (opt-in,
    /// default off). Off keeps the single downlink tuner on the voice fast path.
    /// On, an in-span data grant is followed and its PDUs are mined for LRRP —
    /// but note a downlink capture yields LRRP *transport* detection and PDU
    /// logging only, not GPS coordinates (those are inbound/uplink; see W5).
    #[serde(default)]
    pub data_follow: bool,
}

impl Site {
    /// The tuner set that serves this site: `devices` when set, else the single
    /// `device`. Empty only for a misconfigured site (caught by validation).
    pub fn device_ids(&self) -> Vec<&str> {
        if !self.devices.is_empty() {
            self.devices.iter().map(String::as_str).collect()
        } else if !self.device.is_empty() {
            vec![self.device.as_str()]
        } else {
            Vec::new()
        }
    }
}

/// Resolve a `[[device]] bands` label to a downlink range `[lo, hi]` in Hz.
///
/// Accepts either a known band label (below) or an explicit `"lo-hi"` range in
/// Hz (e.g. `"856000000-866000000"`) for finer partitioning than a whole band.
/// Returns `None` for an unrecognised label.
pub fn resolve_band(label: &str) -> Option<(u64, u64)> {
    // Coarse US P25 downlink ranges — generous on purpose; they exist to keep a
    // grant off a tuner that physically cannot reach the band, not to model the
    // exact channel plan (the IDEN does that).
    let known = match label.trim().to_ascii_lowercase().as_str() {
        "vhf" => Some((136_000_000, 174_000_000)),
        "uhf" => Some((380_000_000, 520_000_000)),
        "700" => Some((758_000_000, 806_000_000)),
        "800" => Some((806_000_000, 869_000_000)),
        "900" => Some((896_000_000, 941_000_000)),
        _ => None,
    };
    if known.is_some() {
        return known;
    }
    // Explicit "lo-hi" Hz range.
    let (lo, hi) = label.split_once('-')?;
    let lo: u64 = lo.trim().parse().ok()?;
    let hi: u64 = hi.trim().parse().ok()?;
    (lo <= hi).then_some((lo, hi))
}

/// The usable downlink span of a capture — `center ± usable_half_span`, one bin
/// short of Nyquist at each edge, which is the span the channelizer will
/// actually accept a subscription in.
///
/// `None` when the tuner has no center, or a sample rate the channelizer cannot
/// drive: it can then record nothing at all, whatever its bands say.
pub fn usable_span_hz(center: Option<u64>, sample_rate: u32) -> Option<(u64, u64)> {
    let center = center?;
    let half = trunkd_p25::dsp::channelizer::usable_half_span_for(sample_rate)?;
    Some((
        (center as f64 - half).max(0.0) as u64,
        (center as f64 + half) as u64,
    ))
}

/// The downlink ranges a recording tuner will accept grants for: each of its
/// bands clamped to the usable capture span, or the whole usable span when no
/// bands are configured.
///
/// One place, because three callers must agree on it: startup validation, the
/// coordinator's route table, and the keyup control loop's apply-time sanity
/// check on a pushed `bands` list. Empty means "this tuner can record nothing"
/// — either it has no usable capture at all, or every band it was given falls
/// outside the one it has.
pub fn band_ranges(center: Option<u64>, sample_rate: u32, bands: &[String]) -> Vec<(u64, u64)> {
    let Some((usable_lo, usable_hi)) = usable_span_hz(center, sample_rate) else {
        return Vec::new();
    };
    if bands.is_empty() {
        return vec![(usable_lo, usable_hi)];
    }
    bands
        .iter()
        .filter_map(|label| resolve_band(label))
        .filter_map(|(lo, hi)| {
            let lo = lo.max(usable_lo);
            let hi = hi.min(usable_hi);
            (lo <= hi).then_some((lo, hi))
        })
        .collect()
}

/// Conventional analog channel mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ChannelMode {
    /// Narrowband FM.
    #[default]
    Nbfm,
    /// Amplitude modulation.
    Am,
}

/// `[channel.squelch]` — squelch and call-segmentation tuning for one
/// conventional channel. Every field is optional; an unset one takes the
/// engine default ([`trunkd_analog::SquelchParams::default`]).
///
/// The four `noise_*`/`hysteresis_*` fields are sdrtrunk's dimensionless
/// `decodeConfigNBFM` squelch attributes. `trunkd import-sdrtrunk` copies them
/// and older configs carry them, so they are still accepted, but they mean
/// nothing to trunkd's dB squelch and are ignored (logged once at load).
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct Squelch {
    /// Open when channel power is at least this many dB above the tracked
    /// noise floor. Default 8.0; must be greater than `close_db`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_db: Option<f32>,
    /// Close when channel power falls to or below this many dB above the noise
    /// floor. Default 5.0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub close_db: Option<f32>,
    /// How long power must stay above `open_db` before the squelch opens, ms.
    /// Default 40.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attack_ms: Option<u32>,
    /// How long power must stay below `close_db` before a call ends, ms; a
    /// re-key inside it continues the call. Default 1000.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hang_ms: Option<u32>,
    /// Tone-gated channels: how long after carrier-up the tone must be
    /// confirmed before the held audio is discarded, ms. Default 750.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tone_timeout_ms: Option<u32>,
    /// Longest call, seconds; a longer transmission is split. Default 180.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_call_s: Option<u32>,
    /// Shortest call kept, ms; shorter ones (kerchunks) are dropped and
    /// counted. Default 500.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_call_ms: Option<u32>,
    /// Legacy sdrtrunk field; accepted and ignored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub noise_open: Option<f32>,
    /// Legacy sdrtrunk field; accepted and ignored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub noise_close: Option<f32>,
    /// Legacy sdrtrunk field; accepted and ignored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hysteresis_open: Option<u32>,
    /// Legacy sdrtrunk field; accepted and ignored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hysteresis_close: Option<u32>,
}

impl Squelch {
    /// Whether any of sdrtrunk's legacy squelch fields is set.
    pub fn has_legacy_fields(&self) -> bool {
        self.noise_open.is_some()
            || self.noise_close.is_some()
            || self.hysteresis_open.is_some()
            || self.hysteresis_close.is_some()
    }

    /// The engine's squelch settings: each set field over the defaults.
    pub fn params(&self) -> trunkd_analog::SquelchParams {
        let d = trunkd_analog::SquelchParams::default();
        trunkd_analog::SquelchParams {
            open_db: self.open_db.unwrap_or(d.open_db),
            close_db: self.close_db.unwrap_or(d.close_db),
            attack_ms: self.attack_ms.unwrap_or(d.attack_ms),
            hang_ms: self.hang_ms.unwrap_or(d.hang_ms),
            tone_timeout_ms: self.tone_timeout_ms.unwrap_or(d.tone_timeout_ms),
            max_call_s: self.max_call_s.unwrap_or(d.max_call_s),
            min_call_ms: self.min_call_ms.unwrap_or(d.min_call_ms),
        }
    }
}

/// `[[channel]]` — one conventional analog channel.
///
/// Only `name` and `frequency` are required. A recorded call is filed in Keyup
/// as a group call under (`system`, `talkgroup`) with `name` as its label.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Channel {
    /// Display name, sent as the call's `talkgroupLabel`.
    pub name: String,
    /// System label the calls are filed under. Blank uses `[uplink]
    /// system_label`. See [`Config::channel_system`].
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub system: String,
    /// Site label recorded on the calls. Blank uses `[uplink] site_label`, then
    /// the device id. See [`Config::channel_site`].
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub site: String,
    /// Device id serving this channel. Blank picks the one `[[device]]` whose
    /// usable span covers `frequency` (an error if none or several do). See
    /// [`Config::channel_device_for`].
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub device: String,
    /// Channel center frequency in Hz.
    pub frequency: u64,
    /// Demodulation mode. `am` is parsed but not recorded.
    #[serde(default)]
    pub mode: ChannelMode,
    /// Whether the channel is started at run time.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Channel bandwidth in Hz: 12500 (narrowband, ±2.5 kHz deviation), the
    /// only value supported today. 25000 (wideband) is refused with a clear
    /// error: the channel bank delivers one 12.5 kHz slice per channel.
    #[serde(default = "default_bandwidth_hz")]
    pub bandwidth_hz: u32,
    /// Talkgroup id reported on calls from this channel. Unset uses
    /// `frequency / 1000` (see [`Channel::talkgroup_id`]). Must be unique within
    /// the channel's system across enabled channels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub talkgroup: Option<u32>,
    /// Required sub-audible tone: a CTCSS frequency in Hz from the standard
    /// table (`"167.9"`) or a DCS code (`"D445N"`, `"D445I"`, `"445"`). Unset
    /// is carrier squelch. A bare TOML number is accepted too.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "de_tone"
    )]
    pub tone: Option<String>,
    /// DCS only: require an error-free code word instead of allowing a
    /// single-bit correction.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub dcs_strict: bool,
    /// De-emphasis time constant in µs. Unset is 750; 0 disables it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deemphasis_us: Option<u32>,
    /// Squelch tuning. Serialized last: TOML requires sub-tables after scalars.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub squelch: Option<Squelch>,
}

/// `tone` as written, or a bare TOML number (`tone = 167.9`, `tone = 445`)
/// turned into the same string.
fn de_tone<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> std::result::Result<Option<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Raw {
        Text(String),
        Int(i64),
        Float(f64),
    }
    Ok(match Option::<Raw>::deserialize(d)? {
        None => None,
        Some(Raw::Text(s)) => Some(s),
        Some(Raw::Int(i)) => Some(i.to_string()),
        Some(Raw::Float(f)) => Some(format!("{f:.1}")),
    })
}

impl Channel {
    /// The talkgroup id calls are filed under: `talkgroup`, else
    /// `frequency / 1000` (154.250 MHz → 154250).
    pub fn talkgroup_id(&self) -> u32 {
        self.talkgroup
            .unwrap_or_else(|| u32::try_from(self.frequency / 1000).unwrap_or(u32::MAX))
    }

    /// The parsed `tone`; `None` is carrier squelch.
    pub fn parsed_tone(&self) -> Result<Option<trunkd_analog::Tone>> {
        self.tone
            .as_deref()
            .map(|t| {
                t.parse::<trunkd_analog::Tone>()
                    .map_err(|e| Error::config(format!("channel '{}': tone {t:?}: {e}", self.name)))
            })
            .transpose()
    }

    /// The engine settings this channel asks for, checked.
    pub fn params(&self) -> Result<trunkd_analog::ChannelParams> {
        // The channel bank hands each channel a 12.5 kHz slice (one bin, or
        // a two-bin seam), so a 25 kHz channel would lose half its signal.
        // `Bandwidth::Wide` stays reserved in the engine until the bank can
        // deliver a wide slice.
        let bandwidth = match self.bandwidth_hz {
            12_500 => trunkd_analog::Bandwidth::Narrow,
            25_000 => {
                return Err(Error::config(format!(
                    "channel '{}': 25 kHz channels are not supported yet \
                     (bandwidth_hz = 25000); use 12500",
                    self.name
                )))
            }
            other => {
                return Err(Error::config(format!(
                    "channel '{}': bandwidth_hz must be 12500 (got {other})",
                    self.name
                )))
            }
        };
        let defaults = trunkd_analog::ChannelParams::default();
        let params = trunkd_analog::ChannelParams {
            bandwidth,
            tone: self.parsed_tone()?,
            deemphasis_us: self.deemphasis_us.unwrap_or(defaults.deemphasis_us),
            squelch: self.squelch.unwrap_or_default().params(),
            dcs_strict: self.dcs_strict,
        };
        params
            .validate()
            .map_err(|e| Error::config(format!("channel '{}': {e}", self.name)))?;
        Ok(params)
    }
}

/// `[health]` — watchdog thresholds.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Health {
    /// Overflow samples/sec above which a device is flagged Degraded.
    #[serde(default = "default_overflow_warn")]
    pub overflow_warn_per_sec: u64,
    /// Seconds without control-channel sync before hunting alternates.
    #[serde(default = "default_control_timeout")]
    pub control_sync_timeout_secs: u64,
}

impl Default for Health {
    fn default() -> Self {
        Health {
            overflow_warn_per_sec: default_overflow_warn(),
            control_sync_timeout_secs: default_control_timeout(),
        }
    }
}

/// `[link]` — the recorder-to-recorder UDP link.
///
/// Off by default, and deliberately so: it opens a socket, and a recorder that
/// nobody is peered with gains nothing from having one. Turned on, it is the
/// same mechanism the in-process coordinator already uses between two tuners of
/// one recorder, extended to tuners in another process or on another box —
/// grants published, grants accepted, and everyone saying what they are on.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Link {
    /// Whether the link runs at all.
    #[serde(default)]
    pub enabled: bool,
    /// Local UDP bind address.
    #[serde(default = "default_link_bind")]
    pub bind: String,
    /// Peer recorders, as `host:port`. Unicast; every peer gets every message.
    #[serde(default)]
    pub peers: Vec<String>,
    /// Shared secret authenticating every datagram in both directions. Required
    /// when `enabled`, and at least [`trunkd_link::MIN_KEY_LEN`] characters —
    /// an unauthenticated link is a remote "record this frequency" button.
    #[serde(default)]
    pub key: String,
    /// Seconds between `Hello` advertisements. A peer is considered alive for
    /// three of these after its last message.
    #[serde(default = "default_hello_interval")]
    pub hello_interval_s: u64,
    /// Whether grants heard by a peer are routed to this recorder's tuners.
    #[serde(default = "default_true")]
    pub accept_remote_grants: bool,
    /// Whether grants heard here are published to peers.
    #[serde(default = "default_true")]
    pub publish_grants: bool,
    /// Whether a peer's `Claim` suppresses following the same call here.
    ///
    /// Off by default. keyup already picks the best source among several
    /// recordings of one call, so a second copy costs a little disk and buys a
    /// better one; a suppressed call that the claiming peer then loses is gone
    /// for good. Turn it on when tuner capacity, not disk, is the constraint.
    #[serde(default)]
    pub defer_to_claims: bool,
    /// How many times a `Grant` is retransmitted to a peer that has not
    /// acknowledged it. `0` turns the retry off (the ack is still counted).
    ///
    /// A grant is the one message where loss costs a call rather than a stale
    /// advertisement, so it is the one message that is retried; Hello, Claim,
    /// Release and ControlState stay fire-and-forget.
    #[serde(default = "default_grant_retries")]
    pub grant_retries: u32,
    /// Milliseconds between those retransmissions. The default spends 24 ms of
    /// the 100–300 ms a P25 grant has before its voice starts.
    #[serde(default = "default_grant_retry_ms")]
    pub grant_retry_ms: u64,
}

impl Default for Link {
    fn default() -> Self {
        Link {
            enabled: false,
            bind: default_link_bind(),
            peers: Vec::new(),
            key: String::new(),
            hello_interval_s: default_hello_interval(),
            accept_remote_grants: true,
            publish_grants: true,
            defer_to_claims: false,
            grant_retries: default_grant_retries(),
            grant_retry_ms: default_grant_retry_ms(),
        }
    }
}

impl Link {
    /// Reject a link that cannot work rather than starting one that silently
    /// does nothing (or, worse, one anybody can drive).
    pub fn validate(&self) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }
        if self.key.trim().len() < trunkd_link::MIN_KEY_LEN {
            return Err(Error::config(format!(
                "link: key must be at least {} characters when the link is enabled",
                trunkd_link::MIN_KEY_LEN
            )));
        }
        if self.bind.trim().is_empty() {
            return Err(Error::config("link: bind must not be empty"));
        }
        if self.bind.parse::<std::net::SocketAddr>().is_err() {
            return Err(Error::config(format!(
                "link: bind must be host:port (got {:?})",
                self.bind
            )));
        }
        for p in &self.peers {
            if !p.contains(':') {
                return Err(Error::config(format!("link: peer {p:?} must be host:port")));
            }
        }
        if self.hello_interval_s == 0 || self.hello_interval_s > 3_600 {
            return Err(Error::config(format!(
                "link: hello_interval_s must be within 1..=3600 (got {})",
                self.hello_interval_s
            )));
        }
        if self.grant_retries > trunkd_link::MAX_GRANT_RETRIES {
            return Err(Error::config(format!(
                "link: grant_retries must be within 0..={} (got {})",
                trunkd_link::MAX_GRANT_RETRIES,
                self.grant_retries
            )));
        }
        if self.grant_retry_ms == 0 || self.grant_retry_ms > trunkd_link::MAX_GRANT_RETRY_MS {
            return Err(Error::config(format!(
                "link: grant_retry_ms must be within 1..={} (got {})",
                trunkd_link::MAX_GRANT_RETRY_MS,
                self.grant_retry_ms
            )));
        }
        Ok(())
    }
}

fn default_link_bind() -> String {
    "0.0.0.0:47123".to_owned()
}
fn default_hello_interval() -> u64 {
    5
}

fn default_grant_retries() -> u32 {
    trunkd_link::DEFAULT_GRANT_RETRIES
}

fn default_grant_retry_ms() -> u64 {
    trunkd_link::DEFAULT_GRANT_RETRY_MS
}

fn default_bind() -> String {
    "127.0.0.1:8080".to_owned()
}
fn default_log_level() -> String {
    "info".to_owned()
}
fn default_spool_path() -> String {
    "spool".to_owned()
}
fn default_spool_cap() -> u64 {
    1024 * 1024 * 1024
}
/// 2 GiB: enough headroom for a log rotation, an MP3 backlog being written
/// while the sweep runs, and the filesystem's own metadata churn on a small
/// SSD, without reserving a meaningful slice of a large disk.
fn default_spool_min_free() -> u64 {
    2 * 1024 * 1024 * 1024
}
fn default_pool_size() -> usize {
    20
}
fn default_system_id() -> u32 {
    1
}
fn default_mp3_bitrate() -> u32 {
    16
}
fn default_observations_interval() -> u64 {
    300
}
/// Long enough for a healthy keyup on a local network, short enough that a
/// keyup that is down delays the first sample by an unnoticeable amount.
fn default_desired_prefetch_timeout() -> u64 {
    3
}
/// Long enough that a keyup restart is not hammered, short enough that an
/// operator waiting on a push does not notice the gap.
fn default_stream_reconnect_max() -> u64 {
    30
}
/// Shallowest ring that can absorb an ordinary scheduling quantum.
pub const MIN_RING_SECONDS: f64 = 0.1;

/// One second of ring: deep enough to ride out a multi-second scheduling
/// stall, shallow enough that a genuinely wedged DSP thread still shows up as
/// overflow within a second.
fn default_ring_seconds() -> f64 {
    1.0
}

/// Largest total automatic frequency correction, in ppm.
pub const MAX_AUTO_PPM_LIMIT: f64 = 50.0;

/// Five ppm: comfortably past any TCXO error worth correcting (the boards seen
/// here run 0.05–1 ppm out) and 4.3 kHz at 855 MHz, so a runaway loop cannot
/// walk a tuner off its own channel plan.
fn default_auto_ppm_limit() -> f64 {
    5.0
}

fn default_true() -> bool {
    true
}
fn default_bandwidth_hz() -> u32 {
    12_500
}
fn default_overflow_warn() -> u64 {
    10_000
}
fn default_control_timeout() -> u64 {
    30
}

impl Config {
    /// Load config from a TOML file plus `TRUNKD_` environment overrides.
    ///
    /// Env keys use `__` to separate nesting, e.g.
    /// `TRUNKD_SERVER__BIND=0.0.0.0:9000`.
    pub fn load(path: &Path) -> Result<Config> {
        let mut cfg: Config = Figment::new()
            .merge(Toml::file(path))
            .merge(Env::prefixed("TRUNKD_").split("__"))
            .extract()
            .map_err(|e| Error::config(format!("{path:?}: {e}")))?;
        cfg.sanitize();
        cfg.validate()?;
        cfg.apply_process_settings();
        Ok(cfg)
    }

    /// Repair, with a warning, what is harmless to repair rather than refuse.
    ///
    /// Today: blank `[server] tokens` entries, which are ignored rather than
    /// failing the load — a recorder that will not start over a panel setting
    /// is worse than one whose panel ignores a meaningless token — but never
    /// allowed to match (see [`Server::drop_blank_tokens`]). Called by the
    /// loaders a running recorder uses; the panel's dry-run
    /// [`Config::from_toml_str`] has no process to warn on behalf of.
    pub fn sanitize(&mut self) {
        let dropped = self.server.drop_blank_tokens();
        if dropped > 0 {
            tracing::warn!(
                dropped,
                kept = self.server.tokens.len(),
                "[server] tokens: ignoring empty or whitespace-only token(s); \
                 an empty token never authenticates"
            );
        }
        for c in &self.channel {
            if c.squelch.is_some_and(|q| q.has_legacy_fields()) {
                tracing::info!(
                    channel = %c.name,
                    "legacy sdrtrunk squelch fields are ignored; use open_db/close_db"
                );
            }
        }
    }

    /// Push the handful of settings that live in process-global state rather
    /// than being threaded through a struct.
    ///
    /// Today that is one thing: the LRRP sanity box, which the location gate
    /// reads from free functions on the observations thread and so cannot be
    /// handed a config. Called from [`Config::load`] and
    /// [`Config::load_env_only`] — the paths a *running* recorder takes — and
    /// deliberately not from [`Config::from_toml_str`], which exists to
    /// dry-run an edit the operator has not committed to yet.
    pub fn apply_process_settings(&self) {
        crate::pipeline::lrrp_gate::configure(self.recorder.location_bounds_box());
    }

    /// Load config from TOML *text* rather than a file.
    ///
    /// The panel's config writer needs to prove an edited document still
    /// loads before it replaces the operator's file, and "loads" has to mean
    /// exactly what [`Config::load`] means or the check is worthless. Env
    /// overrides are deliberately absent: the question is whether the *file*
    /// is good, and a `TRUNKD_` variable that papers over a broken key here
    /// would not be set on the next operator's box.
    pub fn from_toml_str(text: &str) -> Result<Config> {
        let cfg: Config = Figment::new()
            .merge(Toml::string(text))
            .extract()
            .map_err(|e| Error::config(e.to_string()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Load only environment overrides (no file); used when no path is given.
    pub fn load_env_only() -> Result<Config> {
        let mut cfg: Config = Figment::new()
            .merge(Env::prefixed("TRUNKD_").split("__"))
            .extract()
            .map_err(|e| Error::config(e.to_string()))?;
        cfg.sanitize();
        cfg.validate()?;
        cfg.apply_process_settings();
        Ok(cfg)
    }

    /// Number of enabled sites.
    pub fn enabled_sites(&self) -> usize {
        self.site.iter().filter(|s| s.enabled).count()
    }

    /// Number of enabled conventional channels.
    pub fn enabled_channels(&self) -> usize {
        self.channel.iter().filter(|c| c.enabled).count()
    }

    /// Ids of devices referenced by at least one enabled site or channel.
    ///
    /// A device serving nothing enabled has no work to do and is not started.
    pub fn active_device_ids(&self) -> Vec<&str> {
        let referenced = self
            .site
            .iter()
            .filter(|s| s.enabled)
            .flat_map(|s| s.device_ids())
            .chain(
                self.channel
                    .iter()
                    .filter(|c| c.enabled)
                    .filter_map(|c| self.channel_device_for(c).ok())
                    .map(|d| d.id.as_str()),
            );
        let mut ids: Vec<&str> = Vec::new();
        for id in referenced {
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
        ids
    }

    /// The device that serves channel `c`: the named `device`, or, when that
    /// is blank, the one `[[device]]` whose usable span (see
    /// [`usable_span_hz`]) covers the channel's frequency.
    ///
    /// Errors name the channel: an unknown device, no covering device, or
    /// several (then the operator has to say which).
    pub fn channel_device_for(&self, c: &Channel) -> Result<&Device> {
        if !c.device.is_empty() {
            return self
                .device
                .iter()
                .find(|d| d.id == c.device)
                .ok_or_else(|| {
                    Error::config(format!(
                        "channel '{}' references unknown device '{}'",
                        c.name, c.device
                    ))
                });
        }
        let covering: Vec<&Device> = self
            .device
            .iter()
            .filter(|d| {
                usable_span_hz(d.center_frequency, d.sample_rate)
                    .is_some_and(|(lo, hi)| c.frequency >= lo && c.frequency <= hi)
            })
            .collect();
        match covering.as_slice() {
            [d] => Ok(d),
            [] => Err(Error::config(format!(
                "channel '{}': no device's usable span covers {} Hz; set `device` \
                 or a device `center_frequency` that reaches it",
                c.name, c.frequency
            ))),
            several => Err(Error::config(format!(
                "channel '{}': {} Hz is covered by several devices ({}); set `device`",
                c.name,
                c.frequency,
                several
                    .iter()
                    .map(|d| d.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))),
        }
    }

    /// System label calls from channel `c` are filed under: its `system`, else
    /// `[uplink] system_label`.
    pub fn channel_system<'a>(&'a self, c: &'a Channel) -> &'a str {
        if c.system.trim().is_empty() {
            &self.uplink.system_label
        } else {
            &c.system
        }
    }

    /// Site label recorded on calls from channel `c`: its `site`, else
    /// `[uplink] site_label`, else the id of the device serving it.
    pub fn channel_site<'a>(&'a self, c: &'a Channel) -> &'a str {
        if !c.site.trim().is_empty() {
            &c.site
        } else if !self.uplink.site_label.trim().is_empty() {
            &self.uplink.site_label
        } else {
            self.channel_device_for(c).map_or("", |d| d.id.as_str())
        }
    }

    /// Semantic validation beyond deserialization.
    pub fn validate(&self) -> Result<()> {
        self.station.validate()?;
        self.recorder.validate()?;
        self.link.validate()?;
        for d in &self.device {
            if d.sample_rate == 0 {
                return Err(Error::config(format!(
                    "device '{}': sample_rate must be > 0",
                    d.id
                )));
            }
            // Below 0.1 s the ring cannot absorb even an ordinary scheduling
            // quantum, and the recorder would drop samples as a matter of
            // course rather than under load.
            // `is_nan` is tested explicitly rather than relying on a negated
            // comparison: a NaN must be rejected, not silently accepted.
            if d.ring_seconds.is_nan() || d.ring_seconds < MIN_RING_SECONDS {
                return Err(Error::config(format!(
                    "device '{}': ring_seconds must be >= {MIN_RING_SECONDS} (got {})",
                    d.id, d.ring_seconds
                )));
            }
            // The same `is_nan` discipline: a NaN limit would clamp to nothing
            // and let the correction loop run unbounded.
            if d.auto_ppm_limit.is_nan()
                || d.auto_ppm_limit <= 0.0
                || d.auto_ppm_limit > MAX_AUTO_PPM_LIMIT
            {
                return Err(Error::config(format!(
                    "device '{}': auto_ppm_limit must be > 0 and <= {MAX_AUTO_PPM_LIMIT} (got {})",
                    d.id, d.auto_ppm_limit
                )));
            }
            match d.backend {
                Backend::Soapy => d.validate_soapy()?,
                Backend::Iqfile if d.path.is_none() => {
                    return Err(Error::config(format!(
                        "device '{}': backend 'iqfile' requires a `path`",
                        d.id
                    )));
                }
                Backend::Hackrf => d.validate_hackrf()?,
                _ => {}
            }
        }
        let known = |id: &str| self.device.iter().any(|d| d.id == id);
        // Every band label on a device must resolve, and its range must overlap
        // the tuner's *usable* span — the same span routing clamps to (one bin
        // short of Nyquist at each edge). Validating against the raw span would
        // let a band touching only the guard edge pass yet yield no route, so
        // the grant would be silently unrouted; validating against the usable
        // span turns that into a startup error instead.
        //
        // This loop only ever sees the TOML, and it runs once at load: keyup can
        // push a different `bands` list at run time, so `DesiredState::
        // apply_desired` re-checks the same overlap invariant (via
        // [`band_ranges`]) before it stages one, and refuses the push otherwise.
        for d in &self.device {
            // A control tuner publishes every grant regardless of its bands, so
            // bands there have no effect — flag the likely misconfiguration.
            if d.role == DeviceRole::Control && !d.bands.is_empty() {
                tracing::warn!(
                    device = %d.id,
                    "bands are configured on a 'control' tuner but have no effect: a \
                     control front publishes every grant; set bands on the recording tuners"
                );
            }
            let usable_half = trunkd_p25::dsp::channelizer::usable_half_span_for(d.sample_rate);
            for label in &d.bands {
                let Some((lo, hi)) = resolve_band(label) else {
                    return Err(Error::config(format!(
                        "device '{}': unknown band label '{label}' (known: vhf, uhf, \
                         700, 800, 900, or an explicit \"lo-hi\" Hz range)",
                        d.id
                    )));
                };
                // The rate may be off-grid (caught when the engine builds); skip
                // the span check then rather than reporting a confusing span.
                if let (Some(center), Some(half)) = (d.center_frequency, usable_half) {
                    let span_lo = (center as f64 - half).max(0.0) as u64;
                    let span_hi = (center as f64 + half) as u64;
                    if hi < span_lo || lo > span_hi {
                        return Err(Error::config(format!(
                            "device '{}': band '{label}' ({lo}..={hi} Hz) does not \
                             overlap the usable capture span {span_lo}..={span_hi} Hz",
                            d.id
                        )));
                    }
                }
            }
        }
        for s in &self.site {
            let ids = s.device_ids();
            if ids.is_empty() {
                return Err(Error::config(format!(
                    "site '{}/{}' names no device (set `device` or `devices`)",
                    s.system, s.site
                )));
            }
            for id in &ids {
                if !known(id) {
                    return Err(Error::config(format!(
                        "site '{}/{}' references unknown device '{}'",
                        s.system, s.site, id
                    )));
                }
            }
            if s.control_frequencies.is_empty() {
                return Err(Error::config(format!(
                    "site '{}/{}' has no control_frequencies",
                    s.system, s.site
                )));
            }
        }
        for c in &self.channel {
            if !c.device.is_empty() && !known(&c.device) {
                return Err(Error::config(format!(
                    "channel '{}' references unknown device '{}'",
                    c.name, c.device
                )));
            }
        }
        self.validate_site_coverage()?;
        self.validate_channels()?;
        Ok(())
    }

    /// The `[[channel]]` rules, applied to enabled NBFM channels only: a
    /// disabled channel is never started, and AM is out of scope (warned about
    /// and skipped at run time), so neither can fail a load that used to pass.
    ///
    /// Per channel: the settings parse (`bandwidth_hz`, `tone`, squelch), the
    /// serving device resolves and has a centre and a channelizer-legal rate,
    /// and the frequency is in that device's usable span and off its DC bin
    /// (both via the channelizer's own `bin_for`). Across channels: no two on
    /// one device closer than 6.25 kHz, none closer than 12.5 kHz to a control
    /// candidate of an enabled site on the same device, and (system,
    /// talkgroup) unique.
    fn validate_channels(&self) -> Result<()> {
        use trunkd_p25::dsp::channelizer::{ChannelizerError, PolyphaseChannelizerM2};

        let nbfm = || {
            self.channel
                .iter()
                .filter(|c| c.enabled && c.mode == ChannelMode::Nbfm)
        };
        // One filterbank per device, built only to ask it `bin_for`.
        let mut banks: BTreeMap<&str, PolyphaseChannelizerM2> = BTreeMap::new();
        // (device id, frequency, channel name) for the spacing checks.
        let mut placed: Vec<(&str, u64, &str)> = Vec::new();
        let mut talkgroups: BTreeMap<(&str, u32), &str> = BTreeMap::new();

        for c in nbfm() {
            c.params()?;
            let d = self.channel_device_for(c)?;
            let Some(center) = d.center_frequency else {
                return Err(Error::config(format!(
                    "channel '{}': device '{}' has no `center_frequency`",
                    c.name, d.id
                )));
            };
            if !banks.contains_key(d.id.as_str()) {
                let bank =
                    PolyphaseChannelizerM2::new(d.sample_rate, center as f64).map_err(|e| {
                        Error::config(format!(
                            "channel '{}': device '{}' cannot be channelized: {e}",
                            c.name, d.id
                        ))
                    })?;
                banks.insert(d.id.as_str(), bank);
            }
            let bank = &banks[d.id.as_str()];
            match bank.bin_for(c.frequency as f64) {
                Ok(_) => {}
                Err(ChannelizerError::DcBin { .. }) => {
                    return Err(Error::config(format!(
                        "channel '{}': {} Hz is on device '{}''s capture centre (the DC \
                         bin, which the channelizer refuses); move the device's \
                         `center_frequency`",
                        c.name, c.frequency, d.id
                    )));
                }
                Err(ChannelizerError::FrequencyOutOfBand { .. }) => {
                    let half = bank.usable_half_span_hz();
                    return Err(Error::config(format!(
                        "channel '{}': {} Hz is outside device '{}''s usable span \
                         {}..={} Hz",
                        c.name,
                        c.frequency,
                        d.id,
                        (center as f64 - half).max(0.0) as u64,
                        (center as f64 + half) as u64
                    )));
                }
                Err(e) => {
                    return Err(Error::config(format!(
                        "channel '{}' on device '{}': {e}",
                        c.name, d.id
                    )));
                }
            }
            let half = u64::from(d.sample_rate) / 2;
            let guard = span_guard_hz(d.sample_rate);
            let (edge_lo, edge_hi) = (center.saturating_sub(half), center + half);
            if c.frequency < edge_lo + guard || c.frequency + guard > edge_hi {
                tracing::warn!(
                    channel = %c.name,
                    device = %d.id,
                    frequency = c.frequency,
                    "conventional channel is within the capture's edge guard band; \
                     the tuner's own filter may attenuate it"
                );
            }

            for s in self.site.iter().filter(|s| s.enabled) {
                if !s.device_ids().contains(&d.id.as_str()) {
                    continue;
                }
                if let Some(cc) = s
                    .control_frequencies
                    .iter()
                    .find(|&&cc| cc.abs_diff(c.frequency) < CHANNEL_CONTROL_SPACING_HZ)
                {
                    return Err(Error::config(format!(
                        "channel '{}' ({} Hz) is within 12.5 kHz of site '{}/{}' \
                         control frequency {cc} Hz on device '{}': a conventional \
                         channel cannot share a P25 control channel",
                        c.name, c.frequency, s.system, s.site, d.id
                    )));
                }
            }
            if let Some((_, f, other)) = placed.iter().find(|(dev, f, _)| {
                *dev == d.id.as_str() && f.abs_diff(c.frequency) < CHANNEL_MIN_SPACING_HZ
            }) {
                return Err(Error::config(format!(
                    "channels '{other}' ({f} Hz) and '{}' ({} Hz) on device '{}' are \
                     closer than 6.25 kHz",
                    c.name, c.frequency, d.id
                )));
            }
            placed.push((d.id.as_str(), c.frequency, c.name.as_str()));

            let key = (self.channel_system(c), c.talkgroup_id());
            if let Some(other) = talkgroups.insert(key, c.name.as_str()) {
                return Err(Error::config(format!(
                    "channels '{other}' and '{}' both file calls as talkgroup {} in \
                     system '{}'; give one a different `talkgroup` or `system`",
                    c.name, key.1, key.0
                )));
            }
        }
        Ok(())
    }

    /// Every enabled site's control candidates must sit inside the span its
    /// device actually captures.
    ///
    /// A candidate outside `center ± sample_rate/2` (less [`SPAN_GUARD_HZ`] at
    /// each edge, where the channelizer's transition band lives) can never be
    /// decoded, and a site whose whole candidate list is out of band would
    /// hunt forever without ever saying why. Catching it at load turns a silent
    /// dead recorder into a startup error.
    fn validate_site_coverage(&self) -> Result<()> {
        for s in self.site.iter().filter(|s| s.enabled) {
            // The control-following tuners of the site: an `auto` or `control`
            // device. A site with none can never acquire.
            let control_devs: Vec<&Device> = s
                .device_ids()
                .iter()
                .filter_map(|id| self.device.iter().find(|d| d.id == *id))
                .filter(|d| matches!(d.role, DeviceRole::Auto | DeviceRole::Control))
                .collect();
            if control_devs.is_empty() {
                return Err(Error::config(format!(
                    "site '{}/{}' has no control-following tuner (needs a device \
                     with role 'auto' or 'control')",
                    s.system, s.site
                )));
            }
            // The site must also have a tuner that *records* traffic, or it
            // would follow control and route every grant into the void
            // (control-only tuners publish; nothing recording them). Data is a
            // stub, so it does not count as a recording tuner.
            //
            // The link is the exception that proves it: with `[link] enabled`
            // and grants published, the tuner that records them can be on
            // another box, and a control-only recorder is a deliberate and
            // useful thing to run.
            let records = s
                .device_ids()
                .iter()
                .filter_map(|id| self.device.iter().find(|d| d.id == *id))
                .any(|d| matches!(d.role, DeviceRole::Auto | DeviceRole::Voice));
            if !records && !(self.link.enabled && self.link.publish_grants) {
                return Err(Error::config(format!(
                    "site '{}/{}' has no recording tuner (needs a device with role \
                     'auto' or 'voice'); a control-only site records nothing",
                    s.system, s.site
                )));
            }
            for &f in &s.control_frequencies {
                let covered = control_devs.iter().any(|d| {
                    let Some(center) = d.center_frequency else {
                        return false;
                    };
                    let guard = span_guard_hz(d.sample_rate);
                    let half = u64::from(d.sample_rate) / 2;
                    let (lo, hi) = (center.saturating_sub(half) + guard, center + half - guard);
                    f >= lo && f <= hi
                });
                if !covered {
                    // Report against the first control device for a clear message.
                    let d = control_devs[0];
                    if d.center_frequency.is_none() {
                        return Err(Error::config(format!(
                            "site '{}/{}': control device '{}' has no `center_frequency`, \
                             so the control channels cannot be placed in the capture",
                            s.system, s.site, d.id
                        )));
                    }
                    return Err(Error::config(format!(
                        "site '{}/{}': control frequency {f} Hz is outside every \
                         control tuner's span (e.g. device '{}', center {}, rate {})",
                        s.system,
                        s.site,
                        d.id,
                        d.center_frequency.unwrap_or(0),
                        d.sample_rate
                    )));
                }
            }
        }
        Ok(())
    }
}

/// Two enabled conventional channels on one device must be at least this far
/// apart (the finest land-mobile raster).
pub const CHANNEL_MIN_SPACING_HZ: u64 = 6_250;

/// An enabled conventional channel must be at least this far from a control
/// candidate of an enabled site on the same device (one P25 channel).
pub const CHANNEL_CONTROL_SPACING_HZ: u64 = 12_500;

/// Guard band excluded from each edge of a capture: 1 % of the sample rate,
/// never less than one 25 kHz P25 channel.
pub fn span_guard_hz(sample_rate: u32) -> u64 {
    u64::from(sample_rate / 100).max(25_000)
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- the recorder link ---------------------------------------------------

    fn link_toml(body: &str) -> Result<Config> {
        let cfg: Config = toml::from_str(body).expect("parses as TOML");
        cfg.validate().map(|()| cfg)
    }

    /// A key long enough for [`Link::validate`] to accept.
    const GOOD_KEY: &str = "key = \"a-sixteen-byte-key-or-longer\"\n";

    // -- the panel's tokens --------------------------------------------------

    #[test]
    fn a_blank_panel_token_is_dropped_at_load_and_the_rest_are_trimmed() {
        let dir = std::env::temp_dir().join(format!(
            "trunkd-config-tokens-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let path = dir.join("trunkd.toml");
        std::fs::write(
            &path,
            "[server]\ntokens = [\"\", \"   \", \" s3cret \", \"\\t\"]\n",
        )
        .expect("config file");
        let cfg = Config::load(&path).expect("a blank token is not a load failure");
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(cfg.server.tokens, vec!["s3cret".to_owned()]);

        // All blank: the panel is left in its no-token mode, not an
        // everything-matches one.
        let mut only_blank = Server {
            tokens: vec![String::new(), " ".into()],
            ..Server::default()
        };
        assert_eq!(only_blank.drop_blank_tokens(), 2);
        assert!(only_blank.tokens.is_empty());
    }

    #[test]
    fn a_control_only_site_is_only_allowed_when_it_publishes() {
        let toml = r#"
            [[device]]
            id = "ctrl"
            backend = "iqfile"
            path = "c.iq"
            sample_rate = 8000000
            center_frequency = 851050000
            role = "control"
            [[site]]
            system = "S"
            site = "T"
            devices = ["ctrl"]
            control_frequencies = [851050000]
            "#;
        assert!(
            link_toml(toml)
                .expect_err("rejected")
                .to_string()
                .contains("no recording tuner"),
            "on its own it routes every grant into the void"
        );
        // With the link publishing, the tuner that records them is on another
        // box, which is the whole point.
        link_toml(&format!("{toml}\n[link]\nenabled = true\n{GOOD_KEY}"))
            .expect("a publishing control-only recorder is a real deployment");
    }

    #[test]
    fn a_link_is_off_and_unconfigured_by_default() {
        let cfg = link_toml("").expect("an empty config is valid");
        assert!(!cfg.link.enabled);
        assert_eq!(cfg.link.bind, "0.0.0.0:47123");
        assert_eq!(cfg.link.hello_interval_s, 5);
        assert!(cfg.link.accept_remote_grants);
        assert!(cfg.link.publish_grants);
        assert!(!cfg.link.defer_to_claims, "two recordings beat none");
        assert_eq!(cfg.link.grant_retries, 3);
        assert_eq!(cfg.link.grant_retry_ms, 8, "24 ms of retry, worst case");
        assert!(cfg.link.peers.is_empty());
    }

    #[test]
    fn a_disabled_link_is_never_validated() {
        // Nonsense in a section nobody reads is not worth stopping a recorder
        // for; enabling it is what makes the nonsense matter.
        link_toml("[link]\nkey = \"x\"\nbind = \"not-an-address\"\n")
            .expect("an off link is not checked");
    }

    #[test]
    fn an_enabled_link_needs_a_real_key_and_a_real_bind() {
        let err = |body: String| link_toml(&body).expect_err("rejected").to_string();
        assert!(
            err("[link]\nenabled = true\n".to_owned()).contains("key must be at least"),
            "an unauthenticated link is a remote record button"
        );
        assert!(err("[link]\nenabled = true\nkey = \"short\"\n".to_owned())
            .contains("key must be at least"));
        assert!(err(format!(
            "[link]\nenabled = true\n{GOOD_KEY}bind = \"nonsense\"\n"
        ))
        .contains("host:port"));
        assert!(err(format!(
            "[link]\nenabled = true\n{GOOD_KEY}peers = [\"192.0.2.30\"]\n"
        ))
        .contains("host:port"));
        assert!(err(format!(
            "[link]\nenabled = true\n{GOOD_KEY}hello_interval_s = 0\n"
        ))
        .contains("hello_interval_s"));
        // The grant retry is a millisecond-scale insurance policy, not a place
        // to spend the grant-to-voice budget.
        assert!(err(format!(
            "[link]\nenabled = true\n{GOOD_KEY}grant_retries = 99\n"
        ))
        .contains("grant_retries"));
        assert!(err(format!(
            "[link]\nenabled = true\n{GOOD_KEY}grant_retry_ms = 0\n"
        ))
        .contains("grant_retry_ms"));
        assert!(err(format!(
            "[link]\nenabled = true\n{GOOD_KEY}grant_retry_ms = 5000\n"
        ))
        .contains("grant_retry_ms"));
        link_toml(&format!(
            "[link]\nenabled = true\n{GOOD_KEY}grant_retries = 0\ngrant_retry_ms = 8\n"
        ))
        .expect("turning the retry off is a legitimate choice");
        link_toml(&format!(
            "[link]\nenabled = true\n{GOOD_KEY}peers = [\"192.0.2.30:47123\"]\n"
        ))
        .expect("a complete link config is accepted");
    }

    // -- station self-description -------------------------------------------

    /// The minimum a `Config` needs to validate, so a station test exercises
    /// only the station rules.
    fn with_station(station_toml: &str) -> Result<Config> {
        let toml = format!(
            r#"
            {station_toml}
            [[device]]
            id = "hackrf0"
            backend = "iqfile"
            path = "/dev/null"
            sample_rate = 20000000
            center_frequency = 851050000
            [[site]]
            system = "S"
            site = "T"
            device = "hackrf0"
            control_frequencies = [851050000]
        "#
        );
        let cfg: Config = toml::from_str(&toml).map_err(|e| Error::config(e.to_string()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    #[test]
    fn a_config_without_a_station_section_describes_nothing() {
        // Absent must stay absent: the status POST omits the whole object, so
        // keyup can tell "not described" from "described as nothing".
        let cfg = with_station("").expect("valid");
        assert!(cfg.station.is_empty());
        assert_eq!(cfg.station.name, "");
        assert_eq!(cfg.station.latitude, None);
        assert!(cfg.device[0].receiver.is_empty());
        assert!(cfg.device[0].antenna.is_empty());
        assert_eq!(cfg.device[0].height_m, None);
    }

    #[test]
    fn station_fields_round_trip() {
        let cfg = with_station(
            r#"
            [station]
            name = "Example hilltop"
            location = "Springfield"
            latitude = 40.1234
            longitude = -100.5678
            elevation_m = 320.0
            contact = "N0CALL"
            notes = "deaf on VHF while the mast is down"
        "#,
        )
        .expect("valid");
        assert!(!cfg.station.is_empty());
        assert_eq!(cfg.station.name, "Example hilltop");
        assert_eq!(cfg.station.location, "Springfield");
        assert_eq!(cfg.station.latitude, Some(40.1234));
        assert_eq!(cfg.station.longitude, Some(-100.5678));
        assert_eq!(cfg.station.elevation_m, Some(320.0));
        assert_eq!(cfg.station.contact, "N0CALL");
        assert!(cfg.station.notes.starts_with("deaf on VHF"));
    }

    #[test]
    fn free_text_is_capped_at_parse_not_at_send() {
        // Capping here means no later consumer has to remember to cap.
        let cfg = with_station(&format!(
            r#"
            [station]
            name = "{}"
            notes = "{}"
        "#,
            "n".repeat(MAX_TEXT_LEN + 500),
            "z".repeat(MAX_NOTES_LEN + 500)
        ))
        .expect("valid");
        assert_eq!(cfg.station.name.chars().count(), MAX_TEXT_LEN);
        assert_eq!(cfg.station.notes.chars().count(), MAX_NOTES_LEN);
    }

    #[test]
    fn a_multibyte_field_is_truncated_on_a_character_boundary() {
        let cfg = with_station(&format!(
            r#"
            [station]
            location = "{}"
        "#,
            "é".repeat(MAX_TEXT_LEN + 10)
        ))
        .expect("valid");
        assert_eq!(cfg.station.location.chars().count(), MAX_TEXT_LEN);
    }

    #[test]
    fn device_description_fields_parse_and_cap() {
        let toml = format!(
            r#"
            [[device]]
            id = "hackrf0"
            backend = "iqfile"
            path = "/dev/null"
            sample_rate = 20000000
            center_frequency = 851050000
            receiver = "HackRF One + Mayhem"
            antenna = "Diamond X50 @ 8m"
            cable = "25 m LMR-400"
            amplifier = "NooElec LaNA"
            height_m = 8.0
            notes = "{}"
            [[site]]
            system = "S"
            site = "T"
            device = "hackrf0"
            control_frequencies = [851050000]
        "#,
            "z".repeat(MAX_NOTES_LEN + 10)
        );
        let cfg: Config = toml::from_str(&toml).expect("parses");
        cfg.validate().expect("valid");
        let d = &cfg.device[0];
        assert_eq!(d.receiver, "HackRF One + Mayhem");
        assert_eq!(d.antenna, "Diamond X50 @ 8m");
        assert_eq!(d.cable, "25 m LMR-400");
        assert_eq!(d.amplifier, "NooElec LaNA");
        assert_eq!(d.height_m, Some(8.0));
        assert_eq!(d.notes.chars().count(), MAX_NOTES_LEN);
        // The description says nothing about the HackRF's own +14 dB amp.
        assert_eq!(d.amp, None);
    }

    #[test]
    fn an_out_of_range_coordinate_is_rejected_not_clamped() {
        // A clamped longitude is a silent lie about where the station is; a
        // startup error costs one restart and is obvious.
        for bad in [
            "[station]
latitude = 91.0",
            "[station]
latitude = -90.5",
            "[station]
longitude = 180.5",
            "[station]
longitude = -1810.0",
            "[station]
latitude = nan",
        ] {
            let err = with_station(bad).expect_err("out of range must be refused");
            assert!(
                err.to_string().contains("station:"),
                "unexpected error: {err}"
            );
        }
        // The edges themselves are legal.
        assert!(with_station(
            "[station]
latitude = 90.0
longitude = -180.0"
        )
        .is_ok());
    }

    #[test]
    fn a_ring_shallower_than_the_minimum_is_refused() {
        let toml = r#"
            [[device]]
            id = "hackrf0"
            backend = "iqfile"
            path = "/dev/null"
            sample_rate = 20000000
            ring_seconds = 0.05
            [[site]]
            system = "S"
            site = "T"
            device = "hackrf0"
            control_frequencies = [851050000]
        "#;
        let cfg: Config = toml::from_str(toml).expect("parses");
        let err = cfg.validate().expect_err("0.05 s is below the minimum");
        assert!(
            err.to_string().contains("ring_seconds"),
            "the error names the key: {err}"
        );
    }

    /// Automatic frequency correction is on by default — the failure it fixes
    /// is silent, so an operator who never heard of it should still get it —
    /// and its ceiling is bounded on both sides.
    #[test]
    fn auto_ppm_defaults_on_and_its_limit_is_bounded() {
        let device = |extra: &str| {
            format!(
                r#"
                [[device]]
                id = "airspy0"
                backend = "iqfile"
                path = "/dev/null"
                sample_rate = 20000000
                center_frequency = 855500000
                {extra}
                [[site]]
                system = "S"
                site = "T"
                device = "airspy0"
                control_frequencies = [851050000]
            "#
            )
        };
        let cfg: Config = toml::from_str(&device("")).expect("parses");
        cfg.validate().expect("the defaults are valid");
        assert!(cfg.device[0].auto_ppm);
        assert_eq!(cfg.device[0].auto_ppm_limit, 5.0);

        for bad in [
            "auto_ppm_limit = 0.0",
            "auto_ppm_limit = -1.0",
            "auto_ppm_limit = 51.0",
            "auto_ppm_limit = nan",
        ] {
            let cfg: Config = toml::from_str(&device(bad)).expect("parses");
            let err = cfg.validate().expect_err("out of range must be refused");
            assert!(
                err.to_string().contains("auto_ppm_limit"),
                "the error names the key: {err}"
            );
        }
        // The edges themselves are legal.
        for good in [
            "auto_ppm_limit = 0.1",
            "auto_ppm_limit = 50.0",
            "auto_ppm = false",
        ] {
            let cfg: Config = toml::from_str(&device(good)).expect("parses");
            cfg.validate().expect("in range");
        }
    }

    #[test]
    fn the_ring_defaults_to_one_second() {
        let cfg: Config = toml::from_str(
            r#"
            [[device]]
            id = "d"
            backend = "iqfile"
            path = "/dev/null"
            sample_rate = 20000000
            "#,
        )
        .expect("parses");
        assert_eq!(cfg.device[0].ring_seconds, 1.0);
    }

    #[test]
    fn the_recorder_section_defaults_to_a_log_that_is_on() {
        let cfg: Config = toml::from_str("").expect("parses");
        assert!(cfg.recorder.event_log);
        assert_eq!(cfg.recorder.event_log_dir, "events");
        assert_eq!(cfg.recorder.event_log_keep_days, 14);
        assert!(cfg.recorder.event_log_traffic);
        assert!(cfg.recorder.radio_events);
        assert_eq!(cfg.recorder.event_queue_depth, 16_384);
        assert_eq!(
            cfg.recorder.location_bounds_box(),
            Some([24.0, -125.0, 50.0, -66.0]),
            "a generous CONUS box unless an operator says otherwise"
        );
    }

    /// The LRRP sanity box is four numbers or none, and has to be a real box.
    #[test]
    fn the_location_bounds_must_be_a_real_box_or_empty() {
        let cfg: Config = toml::from_str("[recorder]\nlocation_bounds = []\n").expect("parses");
        assert_eq!(cfg.recorder.location_bounds_box(), None, "no box");
        cfg.recorder.validate().expect("an empty list is allowed");

        let cfg: Config =
            toml::from_str("[recorder]\nlocation_bounds = [40.0, -95.0, 45.0, -90.0]\n")
                .expect("parses");
        assert_eq!(
            cfg.recorder.location_bounds_box(),
            Some([40.0, -95.0, 45.0, -90.0])
        );
        cfg.recorder.validate().expect("a real box is allowed");

        for bad in [
            "location_bounds = [40.0, -95.0, 45.0]",
            "location_bounds = [45.0, -95.0, 40.0, -90.0]",
            "location_bounds = [40.0, -95.0, 95.0, -90.0]",
            "location_bounds = [40.0, -95.0, 45.0, -195.0]",
        ] {
            let cfg: Config = toml::from_str(&format!("[recorder]\n{bad}\n")).expect("parses");
            assert!(cfg.recorder.validate().is_err(), "{bad}");
        }
    }

    #[test]
    fn the_event_queue_depth_is_configurable() {
        let cfg: Config =
            toml::from_str("[recorder]\nevent_queue_depth = 65536\n").expect("parses");
        assert_eq!(cfg.recorder.event_queue_depth, 65_536);
        cfg.validate().expect("65 536 is in range");
    }

    #[test]
    fn an_out_of_range_event_queue_depth_is_rejected() {
        for depth in [MIN_EVENT_QUEUE_DEPTH - 1, 0, MAX_EVENT_QUEUE_DEPTH + 1] {
            let cfg: Config = toml::from_str(&format!("[recorder]\nevent_queue_depth = {depth}\n"))
                .expect("parses");
            let err = cfg
                .validate()
                .expect_err("the depth is outside the accepted range");
            assert!(
                err.to_string().contains("event_queue_depth"),
                "the error names the key: {err}"
            );
        }
        // The bounds themselves are accepted.
        for depth in [MIN_EVENT_QUEUE_DEPTH, MAX_EVENT_QUEUE_DEPTH] {
            let cfg: Config = toml::from_str(&format!("[recorder]\nevent_queue_depth = {depth}\n"))
                .expect("parses");
            cfg.validate().expect("a boundary value is in range");
        }
    }

    #[test]
    fn instance_id_uses_the_configured_value_when_set() {
        let cfg: Config =
            toml::from_str("[recorder]\ninstance_id = \"rec-north-01\"\n").expect("parse");
        assert_eq!(cfg.recorder.resolved_instance_id(), "rec-north-01");
    }

    #[test]
    fn a_blank_instance_id_falls_back_to_a_non_empty_default() {
        let cfg: Config = toml::from_str("").expect("parse");
        // Hostname on the build host, or "trunkd"; either way non-empty and
        // within the cap.
        let id = cfg.recorder.resolved_instance_id();
        assert!(!id.is_empty());
        assert!(id.chars().count() <= MAX_INSTANCE_ID_LEN);
    }

    #[test]
    fn a_long_instance_id_is_trimmed_to_the_keyup_cap() {
        let long = "x".repeat(200);
        let cfg: Config =
            toml::from_str(&format!("[recorder]\ninstance_id = \"{long}\"\n")).expect("parse");
        assert_eq!(
            cfg.recorder.resolved_instance_id().chars().count(),
            MAX_INSTANCE_ID_LEN
        );
    }

    // -- soapy device fields -------------------------------------------------

    fn soapy_device(extra: &str) -> Result<Config> {
        let cfg: Config = toml::from_str(&format!(
            "[[device]]\n\
             id = \"sdr0\"\n\
             backend = \"soapy\"\n\
             sample_rate = 10000000\n{extra}"
        ))
        .expect("parse");
        cfg.validate()?;
        Ok(cfg)
    }

    #[test]
    fn a_soapy_device_composes_its_args_from_driver_and_serial() {
        let cfg = soapy_device(
            "driver = \"airspy\"\nserial = \"deadbeef\"\ncenter_frequency = 855000000\n",
        )
        .expect("valid");
        assert_eq!(
            cfg.device[0].effective_soapy_args(),
            "driver=airspy,serial=deadbeef"
        );
    }

    #[test]
    fn an_explicit_soapy_string_overrides_the_driver_field() {
        let cfg = soapy_device(
            "driver = \"airspy\"\nsoapy = \"driver=sdrplay,mode=rspdx\"\n\
             center_frequency = 855000000\n",
        )
        .expect("valid");
        assert_eq!(
            cfg.device[0].effective_soapy_args(),
            "driver=sdrplay,mode=rspdx"
        );
    }

    #[test]
    fn a_soapy_device_needs_a_driver_or_an_args_string() {
        let err = soapy_device("center_frequency = 855000000\n").expect_err("nameless");
        assert!(err.to_string().contains("requires a `driver`"), "{err}");
    }

    #[test]
    fn a_soapy_device_needs_a_center_frequency_like_hackrf_does() {
        let err = soapy_device("driver = \"rtlsdr\"\n").expect_err("no center");
        assert!(
            err.to_string().contains("requires a `center_frequency`"),
            "{err}"
        );
    }

    #[test]
    fn per_element_gains_and_settings_parse_and_stay_ordered() {
        let cfg = soapy_device(
            "driver = \"sdrplay\"\n\
             center_frequency = 855000000\n\
             bandwidth_hz = 8000000\n\
             rx_port = \"Antenna B\"\n\
             agc = false\n\
             [device.gains]\n\
             IFGR = 40.0\n\
             RFGR = 3.0\n\
             [device.settings]\n\
             rfnotch_ctrl = \"false\"\n",
        )
        .expect("valid");
        let d = &cfg.device[0];
        assert_eq!(d.bandwidth_hz, Some(8_000_000));
        assert_eq!(d.rx_port.as_deref(), Some("Antenna B"));
        assert_eq!(d.agc, Some(false));
        assert_eq!(d.gains.get("IFGR"), Some(&40.0));
        assert_eq!(d.gains.get("RFGR"), Some(&3.0));
        assert_eq!(
            d.settings.get("rfnotch_ctrl").map(String::as_str),
            Some("false")
        );
    }

    #[test]
    fn parses_minimal_site_config() {
        let toml = r#"
            [[device]]
            id = "sdr0"
            backend = "soapy"
            soapy = "driver=hackrf"
            sample_rate = 2400000
            center_frequency = 770537500

            [[site]]
            system = "Example Network"
            site = "Example County"
            device = "sdr0"
            control_frequencies = [770056250, 771018750]
            traffic_pool_size = 20
        "#;
        let cfg: Config = toml::from_str(toml).expect("parse");
        cfg.validate().expect("valid");
        assert_eq!(cfg.device.len(), 1);
        assert_eq!(cfg.site[0].control_frequencies.len(), 2);
        assert_eq!(cfg.site[0].traffic_pool_size, 20);
    }

    /// A `[[device]]` block for a HackRF, with `extra` lines appended.
    fn hackrf_toml(extra: &str) -> String {
        format!(
            "[[device]]\n\
             id = \"hackrf0\"\n\
             backend = \"hackrf\"\n\
             sample_rate = 20000000\n\
             {extra}\n"
        )
    }

    #[test]
    fn parses_hackrf_device_config() {
        let cfg: Config = toml::from_str(&hackrf_toml(
            "center_frequency = 855500000\n\
             lna_db = 40\n\
             vga_db = 40\n\
             amp = true\n\
             serial = \"0000000000000000deadbeef00000001\"\n\
             portapack_port = \"/dev/ttyACM0\"",
        ))
        .expect("parse");
        cfg.validate().expect("valid");
        let d = &cfg.device[0];
        assert_eq!(d.backend, Backend::Hackrf);
        assert_eq!(d.center_frequency, Some(855_500_000));
        assert_eq!(d.lna_db, Some(40));
        assert_eq!(d.vga_db, Some(40));
        assert_eq!(d.amp, Some(true));
        assert_eq!(d.portapack_port.as_deref(), Some("/dev/ttyACM0"));
    }

    #[test]
    fn hackrf_requires_a_center_frequency() {
        let cfg: Config = toml::from_str(&hackrf_toml("lna_db = 24")).expect("parse");
        let err = cfg.validate().expect_err("must require center_frequency");
        assert!(err.to_string().contains("center_frequency"), "{err}");
    }

    #[test]
    fn hackrf_rejects_off_step_gains() {
        for (extra, needle) in [
            ("center_frequency = 855500000\nlna_db = 41", "lna_db"),
            ("center_frequency = 855500000\nlna_db = 48", "lna_db"),
            ("center_frequency = 855500000\nvga_db = 21", "vga_db"),
            ("center_frequency = 855500000\nvga_db = 64", "vga_db"),
        ] {
            let cfg: Config = toml::from_str(&hackrf_toml(extra)).expect("parse");
            let err = cfg.validate().expect_err("must reject");
            assert!(err.to_string().contains(needle), "{err}");
        }
        // The legal extremes pass.
        let cfg: Config = toml::from_str(&hackrf_toml(
            "center_frequency = 855500000\nlna_db = 40\nvga_db = 62",
        ))
        .expect("parse");
        cfg.validate().expect("valid");
    }

    #[test]
    fn existing_configs_without_hackrf_fields_still_parse() {
        let cfg: Config = toml::from_str(
            r#"
            [[device]]
            id = "sdr0"
            backend = "soapy"
            soapy = "driver=hackrf"
            sample_rate = 2400000
            center_frequency = 855000000
            gain = 40.0
        "#,
        )
        .expect("parse");
        cfg.validate().expect("valid");
        assert_eq!(cfg.device[0].lna_db, None);
        assert_eq!(cfg.device[0].vga_db, None);
        assert_eq!(cfg.device[0].amp, None);
        assert_eq!(cfg.device[0].serial, None);
    }

    /// A site whose control channels sit inside the device's 20 MHz span.
    fn site_toml(controls: &str) -> String {
        format!(
            "[[device]]\n\
             id = \"hackrf0\"\n\
             backend = \"hackrf\"\n\
             sample_rate = 20000000\n\
             center_frequency = 855500000\n\
             \n\
             [[site]]\n\
             system = \"Example Network\"\n\
             site = \"Central\"\n\
             device = \"hackrf0\"\n\
             control_frequencies = [{controls}]\n"
        )
    }

    #[test]
    fn accepts_control_frequencies_inside_the_capture_span() {
        let cfg: Config = toml::from_str(&site_toml("851050000, 859987500")).expect("parse");
        cfg.validate()
            .expect("both candidates are inside 845.7..865.3 MHz");
    }

    #[test]
    fn rejects_control_frequencies_outside_the_capture_span() {
        // 770.05625 MHz: the old 700 MHz plan, nowhere near an 855.5 MHz capture.
        let cfg: Config = toml::from_str(&site_toml("851050000, 770056250")).expect("parse");
        let err = cfg
            .validate()
            .expect_err("out-of-span candidate must be rejected");
        assert!(err.to_string().contains("770056250"), "{err}");
        // The guard band bites just inside the nominal edge.
        let cfg: Config = toml::from_str(&site_toml("845600000")).expect("parse");
        assert!(
            cfg.validate().is_err(),
            "845.6 MHz is inside the 200 kHz guard"
        );
    }

    #[test]
    fn a_disabled_site_is_not_span_checked() {
        let mut text = site_toml("770056250");
        text.push_str("enabled = false\n");
        let cfg: Config = toml::from_str(&text).expect("parse");
        cfg.validate()
            .expect("disabled sites are not decoded, so not checked");
    }

    #[test]
    fn parses_the_new_uplink_and_site_keys() {
        let cfg: Config = toml::from_str(
            r#"
            [uplink]
            url = "http://192.0.2.60/api/call-upload"
            api_key = "kup_x"
            system_label = "Example Network"
            site_label = "Central"
            audio_format = "wav"
            mp3_bitrate_kbps = 32
            observations_interval_s = 60
            spool_only = true
            stream = false
            stream_reconnect_max_s = 120

            [[device]]
            id = "replay0"
            backend = "iqfile"
            path = "/tmp/x.iq"
            sample_rate = 20000000
            center_frequency = 855500000
            pacing = "unthrottled"
            restore_portapack_on_exit = true

            [[site]]
            system = "Example Network"
            site = "Central"
            device = "replay0"
            control_frequencies = [851050000]
            hang_ms = 1500
            call_inactivity_ms = 2500
        "#,
        )
        .expect("parse");
        cfg.validate().expect("valid");
        assert_eq!(cfg.uplink.system_label, "Example Network");
        assert_eq!(cfg.uplink.audio_format, AudioFormatName::Wav);
        assert_eq!(cfg.uplink.mp3_bitrate_kbps, 32);
        assert_eq!(cfg.uplink.observations_interval_s, 60);
        assert!(cfg.uplink.spool_only);
        assert!(cfg.uplink.enabled, "enabled defaults to true");
        assert!(!cfg.uplink.stream, "the push channel can be turned off");
        assert_eq!(cfg.uplink.stream_reconnect_max_s, 120);
        assert_eq!(cfg.device[0].pacing, PacingName::Unthrottled);
        assert!(cfg.device[0].restore_portapack_on_exit);
        assert_eq!(cfg.site[0].hang_ms, Some(1500));
        assert_eq!(cfg.site[0].call_inactivity_ms, Some(2500));
    }

    #[test]
    fn uplink_defaults_match_the_documented_contract() {
        let cfg: Config = toml::from_str("").expect("parse");
        assert_eq!(cfg.uplink.audio_format, AudioFormatName::Mp3);
        assert_eq!(cfg.uplink.mp3_bitrate_kbps, 16);
        assert_eq!(cfg.uplink.observations_interval_s, 300);
        assert!(cfg.uplink.enabled);
        assert!(!cfg.uplink.spool_only);
        assert!(cfg.uplink.stream);
        assert_eq!(cfg.uplink.stream_reconnect_max_s, 30);
    }

    #[test]
    fn rejects_site_with_unknown_device() {
        let toml = r#"
            [[site]]
            system = "X"
            site = "Y"
            device = "ghost"
            control_frequencies = [100000000]
        "#;
        let cfg: Config = toml::from_str(toml).expect("parse");
        assert!(cfg.validate().is_err());
    }

    // -- multi-tuner core (W1) ------------------------------------------------

    #[test]
    fn the_single_device_field_still_works_as_a_one_element_tuner_set() {
        let s = Site {
            system: "S".into(),
            site: "T".into(),
            device: "d0".into(),
            devices: Vec::new(),
            enabled: true,
            control_frequencies: vec![851_050_000],
            traffic_pool_size: 20,
            rotation_delay_ms: None,
            hang_ms: None,
            call_inactivity_ms: None,
            phase2: false,
            data_follow: false,
        };
        assert_eq!(s.device_ids(), vec!["d0"]);
    }

    #[test]
    fn devices_list_supersedes_the_single_device() {
        let mut s = Site {
            system: "S".into(),
            site: "T".into(),
            device: "ignored".into(),
            devices: vec!["a".into(), "b".into()],
            enabled: true,
            control_frequencies: vec![851_050_000],
            traffic_pool_size: 20,
            rotation_delay_ms: None,
            hang_ms: None,
            call_inactivity_ms: None,
            phase2: false,
            data_follow: false,
        };
        assert_eq!(s.device_ids(), vec!["a", "b"]);
        s.devices.clear();
        assert_eq!(s.device_ids(), vec!["ignored"]);
    }

    #[test]
    fn a_two_tuner_site_parses_roles_bands_and_the_devices_list() {
        let toml = r#"
            [[device]]
            id = "ctrl"
            backend = "iqfile"
            path = "/dev/null"
            sample_rate = 20000000
            center_frequency = 855500000
            role = "control"

            [[device]]
            id = "voice0"
            backend = "iqfile"
            path = "/dev/null"
            sample_rate = 20000000
            center_frequency = 855500000
            role = "voice"
            bands = ["800"]

            [[site]]
            system = "Example"
            site = "Central"
            devices = ["ctrl", "voice0"]
            control_frequencies = [851050000]
        "#;
        let cfg: Config = toml::from_str(toml).expect("parse");
        cfg.validate().expect("valid two-tuner config");
        assert_eq!(cfg.device[0].role, DeviceRole::Control);
        assert_eq!(cfg.device[1].role, DeviceRole::Voice);
        assert_eq!(cfg.device[1].bands, vec!["800".to_owned()]);
        assert_eq!(cfg.site[0].device_ids(), vec!["ctrl", "voice0"]);
        // Both tuners are active (the voice one is reached via the site's list).
        let mut active = cfg.active_device_ids();
        active.sort_unstable();
        assert_eq!(active, vec!["ctrl", "voice0"]);
    }

    #[test]
    fn an_unknown_band_label_is_rejected() {
        let toml = r#"
            [[device]]
            id = "d0"
            backend = "iqfile"
            path = "/dev/null"
            sample_rate = 20000000
            center_frequency = 855500000
            bands = ["nonsense"]
            [[site]]
            system = "S"
            site = "T"
            device = "d0"
            control_frequencies = [851050000]
        "#;
        let cfg: Config = toml::from_str(toml).expect("parse");
        assert!(cfg.validate().is_err(), "an unknown band must be refused");
    }

    #[test]
    fn a_band_that_misses_the_capture_span_is_rejected() {
        // Device captures 800 MHz; a VHF band cannot overlap it.
        let toml = r#"
            [[device]]
            id = "d0"
            backend = "iqfile"
            path = "/dev/null"
            sample_rate = 20000000
            center_frequency = 855500000
            bands = ["vhf"]
            [[site]]
            system = "S"
            site = "T"
            device = "d0"
            control_frequencies = [851050000]
        "#;
        let cfg: Config = toml::from_str(toml).expect("parse");
        assert!(
            cfg.validate().is_err(),
            "a non-overlapping band must be refused"
        );
    }

    #[test]
    fn a_site_without_a_control_following_tuner_is_rejected() {
        let toml = r#"
            [[device]]
            id = "v"
            backend = "iqfile"
            path = "/dev/null"
            sample_rate = 20000000
            center_frequency = 855500000
            role = "voice"
            [[site]]
            system = "S"
            site = "T"
            devices = ["v"]
            control_frequencies = [851050000]
        "#;
        let cfg: Config = toml::from_str(toml).expect("parse");
        assert!(
            cfg.validate().is_err(),
            "a site of only voice tuners can never acquire control"
        );
    }

    #[test]
    fn an_explicit_hz_range_band_resolves_and_validates() {
        assert_eq!(resolve_band("800"), Some((806_000_000, 869_000_000)));
        assert_eq!(
            resolve_band("851000000-856000000"),
            Some((851_000_000, 856_000_000))
        );
        assert_eq!(resolve_band("bogus"), None);
    }

    #[test]
    fn band_ranges_clamp_to_the_usable_capture() {
        let center = Some(855_500_000);
        let rate = 20_000_000;
        let (lo, hi) = usable_span_hz(center, rate).expect("a usable capture");
        assert!(
            lo > 845_000_000 && hi < 866_000_000,
            "guarded at both edges"
        );

        // No bands: the whole usable span, as one range.
        assert_eq!(band_ranges(center, rate, &[]), vec![(lo, hi)]);

        // Several disjoint bands stay disjoint, each clamped to the capture.
        let bands = ["800".to_owned(), "846000000-848000000".to_owned()];
        let ranges = band_ranges(center, rate, &bands);
        assert_eq!(ranges.len(), 2);
        assert_eq!(ranges[0], (806_000_000.max(lo), 869_000_000.min(hi)));
        assert_eq!(ranges[1], (846_000_000, 848_000_000));

        // A band the capture cannot reach contributes nothing, and a list of
        // only such bands leaves the tuner able to record nothing — which is
        // what the control loop refuses to apply.
        assert!(band_ranges(center, rate, &["vhf".to_owned()]).is_empty());
        // No center or an unusable rate: nothing either way.
        assert!(band_ranges(None, rate, &[]).is_empty());
        assert!(usable_span_hz(center, 0).is_none());
    }

    /// The panel link Keyup is given: the operator's `public_url` when set,
    /// and otherwise the bind address with a wildcard host made honest.
    #[test]
    fn the_panel_base_url_is_derived_from_the_bind_unless_one_is_set() {
        let wildcard = Server {
            bind: "0.0.0.0:8095".into(),
            ..Default::default()
        };
        assert_eq!(wildcard.public_base_url(), "http://127.0.0.1:8095");
        let v6 = Server {
            bind: "[::]:9000".into(),
            ..Default::default()
        };
        assert_eq!(v6.public_base_url(), "http://127.0.0.1:9000");
        let lan = Server {
            bind: "192.0.2.30:8095".into(),
            ..Default::default()
        };
        assert_eq!(lan.public_base_url(), "http://192.0.2.30:8095");
        let explicit = Server {
            bind: "0.0.0.0:8095".into(),
            public_url: Some("https://recorder.example/panel/".into()),
            ..Default::default()
        };
        assert_eq!(
            explicit.public_base_url(),
            "https://recorder.example/panel",
            "a trailing slash would double up against the stream path"
        );
    }

    // -- conventional channels -------------------------------------------------

    /// Two fictional tuners: VHF at 155.0 MHz and UHF at 460.0 MHz, both
    /// 2.5 Msps (200 bins, usable ±1.2375 MHz).
    const TWO_TUNERS: &str = r#"
        [uplink]
        system_label = "Example County"
        site_label = "Example Hill"
        [[device]]
        id = "vhf0"
        backend = "iqfile"
        path = "v.iq"
        sample_rate = 2500000
        center_frequency = 155000000
        [[device]]
        id = "uhf0"
        backend = "iqfile"
        path = "u.iq"
        sample_rate = 2500000
        center_frequency = 460000000
        "#;

    fn channels(body: &str) -> Result<Config> {
        Config::from_toml_str(&format!("{TWO_TUNERS}\n{body}"))
    }

    fn rejects(body: &str, needle: &str) {
        let err = channels(body).expect_err("rejected").to_string();
        assert!(err.contains(needle), "{needle:?} not in {err:?}");
    }

    /// `trunkd check` is `Config::load` + print: a config with channels and no
    /// `[[site]]` must load.
    #[test]
    fn a_channels_only_config_is_valid() {
        let cfg = channels(
            r#"
            [[channel]]
            name = "Fire Dispatch"
            system = "Example County Fire"
            frequency = 154250000
            tone = "167.9"
            [[channel]]
            name = "Public Works"
            frequency = 460525000
            tone = "D445N"
            "#,
        )
        .expect("valid");
        assert!(cfg.site.is_empty());
        assert_eq!(cfg.enabled_channels(), 2);
        // Both tuners are started: each serves one channel, picked by span.
        assert_eq!(cfg.active_device_ids(), vec!["vhf0", "uhf0"]);

        let dir = std::env::temp_dir().join(format!("trunkd-config-chan-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let path = dir.join("trunkd.toml");
        std::fs::write(
            &path,
            format!("{TWO_TUNERS}\n[[channel]]\nname = \"A\"\nfrequency = 155300000\n"),
        )
        .expect("config file");
        let loaded = Config::load(&path);
        let _ = std::fs::remove_dir_all(&dir);
        loaded.expect("loads like `trunkd check` loads it");
    }

    #[test]
    fn channel_labels_and_talkgroup_default() {
        let cfg = channels(
            r#"
            [[channel]]
            name = "Fire Dispatch"
            system = "Example County Fire"
            site = "Example Ridge"
            frequency = 154250000
            talkgroup = 14
            [[channel]]
            name = "Ops"
            frequency = 155400000
            "#,
        )
        .expect("valid");
        let (fire, ops) = (&cfg.channel[0], &cfg.channel[1]);
        assert_eq!(cfg.channel_system(fire), "Example County Fire");
        assert_eq!(cfg.channel_site(fire), "Example Ridge");
        assert_eq!(fire.talkgroup_id(), 14);
        // Unset: the uplink labels, and frequency / 1000.
        assert_eq!(cfg.channel_system(ops), "Example County");
        assert_eq!(cfg.channel_site(ops), "Example Hill");
        assert_eq!(ops.talkgroup_id(), 155_400);

        // No uplink site label either: the device id.
        let mut bare = cfg.clone();
        bare.uplink.site_label.clear();
        assert_eq!(bare.channel_site(&bare.channel[1]), "vhf0");
    }

    #[test]
    fn channel_device_auto_pick_single_none_ambiguous() {
        let cfg = channels("[[channel]]\nname = \"A\"\nfrequency = 155300000\n").expect("single");
        assert_eq!(
            cfg.channel_device_for(&cfg.channel[0]).expect("picked").id,
            "vhf0"
        );

        rejects(
            "[[channel]]\nname = \"Far\"\nfrequency = 851012500\n",
            "no device's usable span covers",
        );

        let third = r#"
            [[device]]
            id = "vhf1"
            backend = "iqfile"
            path = "w.iq"
            sample_rate = 2500000
            center_frequency = 155500000
            "#;
        rejects(
            &format!("{third}\n[[channel]]\nname = \"Both\"\nfrequency = 155300000\n"),
            "covered by several devices (vhf0, vhf1)",
        );
        // Naming the device settles it.
        channels(&format!(
            "{third}\n[[channel]]\nname = \"Both\"\ndevice = \"vhf1\"\nfrequency = 155300000\n"
        ))
        .expect("explicit device");
        rejects(
            "[[channel]]\nname = \"A\"\ndevice = \"nope\"\nfrequency = 155300000\n",
            "unknown device 'nope'",
        );
    }

    #[test]
    fn a_channel_must_be_in_its_devices_usable_span() {
        // vhf0 usable span is 155 MHz ± (1.25 MHz − 12.5 kHz).
        rejects(
            "[[channel]]\nname = \"Edge\"\ndevice = \"vhf0\"\nfrequency = 156250000\n",
            "outside device 'vhf0''s usable span 153762500..=156237500 Hz",
        );
        channels("[[channel]]\nname = \"Inside\"\ndevice = \"vhf0\"\nfrequency = 156230000\n")
            .expect("just inside the usable span");
        // A device with no centre cannot place a channel.
        let cfg = format!(
            "{TWO_TUNERS}\n[[device]]\nid = \"nc\"\nbackend = \"iqfile\"\npath = \"n.iq\"\n\
             sample_rate = 2500000\n[[channel]]\nname = \"A\"\ndevice = \"nc\"\nfrequency = 155300000\n"
        );
        assert!(Config::from_toml_str(&cfg)
            .expect_err("rejected")
            .to_string()
            .contains("has no `center_frequency`"));
    }

    #[test]
    fn a_channel_on_the_dc_bin_is_rejected() {
        rejects(
            "[[channel]]\nname = \"DC\"\nfrequency = 155000000\n",
            "DC bin",
        );
        // Off-grid but straddling the centre bin: still DC, by `bin_for`.
        rejects(
            "[[channel]]\nname = \"DC\"\nfrequency = 155007500\n",
            "DC bin",
        );
        channels("[[channel]]\nname = \"Near\"\nfrequency = 155025000\n").expect("two bins off");
    }

    #[test]
    fn a_channel_cannot_share_a_control_channel() {
        let site = r#"
            [[site]]
            system = "Example System"
            site = "Example County"
            device = "uhf0"
            control_frequencies = [460500000]
            "#;
        rejects(
            &format!("{site}\n[[channel]]\nname = \"C\"\nfrequency = 460506250\n"),
            "a conventional channel cannot share a P25 control channel",
        );
        // 12.5 kHz away is the next channel, not an overlap.
        channels(&format!(
            "{site}\n[[channel]]\nname = \"C\"\nfrequency = 460512500\n"
        ))
        .expect("adjacent channel");
        // A disabled site's candidates do not count.
        channels(&format!(
            "{}\n[[channel]]\nname = \"C\"\nfrequency = 460506250\n",
            site.replace(
                "device = \"uhf0\"",
                "device = \"uhf0\"\n            enabled = false"
            )
        ))
        .expect("site disabled");
    }

    #[test]
    fn two_channels_closer_than_6_25_khz_are_rejected() {
        rejects(
            "[[channel]]\nname = \"A\"\nfrequency = 155300000\n\
             [[channel]]\nname = \"B\"\nfrequency = 155305000\n",
            "closer than 6.25 kHz",
        );
        rejects(
            "[[channel]]\nname = \"A\"\nfrequency = 155300000\n\
             [[channel]]\nname = \"B\"\nfrequency = 155300000\ntalkgroup = 9\n",
            "closer than 6.25 kHz",
        );
        channels(
            "[[channel]]\nname = \"A\"\nfrequency = 155300000\n\
             [[channel]]\nname = \"B\"\nfrequency = 155306250\ntalkgroup = 9\n",
        )
        .expect("6.25 kHz apart");
        // A disabled duplicate is left alone.
        channels(
            "[[channel]]\nname = \"A\"\nfrequency = 155300000\n\
             [[channel]]\nname = \"B\"\nfrequency = 155300000\nenabled = false\n",
        )
        .expect("disabled duplicate");
    }

    #[test]
    fn system_talkgroup_pairs_are_unique() {
        rejects(
            "[[channel]]\nname = \"A\"\nfrequency = 155300000\ntalkgroup = 1\n\
             [[channel]]\nname = \"B\"\nfrequency = 155400000\ntalkgroup = 1\n",
            "both file calls as talkgroup 1 in system 'Example County'",
        );
        // An explicit talkgroup can collide with another channel's default.
        rejects(
            "[[channel]]\nname = \"A\"\nfrequency = 155300000\n\
             [[channel]]\nname = \"B\"\nfrequency = 155400000\ntalkgroup = 155300\n",
            "talkgroup 155300",
        );
        // Different systems, or a disabled twin, are fine.
        channels(
            "[[channel]]\nname = \"A\"\nfrequency = 155300000\ntalkgroup = 1\n\
             [[channel]]\nname = \"B\"\nsystem = \"Example Schools\"\nfrequency = 155400000\n\
             talkgroup = 1\n\
             [[channel]]\nname = \"C\"\nfrequency = 155500000\ntalkgroup = 1\nenabled = false\n",
        )
        .expect("unique per system");
    }

    #[test]
    fn channel_tone_parses() {
        for tone in [
            "\"167.9\"",
            "\"D445N\"",
            "\"D445I\"",
            "\"445\"",
            "\"DCS 445\"",
            "167.9",
            "445",
        ] {
            let cfg = channels(&format!(
                "[[channel]]\nname = \"T\"\nfrequency = 155300000\ntone = {tone}\n"
            ))
            .unwrap_or_else(|e| panic!("tone {tone}: {e}"));
            assert!(cfg.channel[0].parsed_tone().expect("parses").is_some());
        }
        let cfg = channels("[[channel]]\nname = \"T\"\nfrequency = 155300000\ntone = 167.9\n")
            .expect("numeric tone");
        assert_eq!(cfg.channel[0].tone.as_deref(), Some("167.9"));
        assert_eq!(
            cfg.channel[0]
                .params()
                .expect("params")
                .tone
                .map(|t| t.to_string()),
            Some("167.9".to_owned())
        );
        rejects(
            "[[channel]]\nname = \"T\"\nfrequency = 155300000\ntone = \"150.0\"\n",
            "channel 'T': tone \"150.0\"",
        );
        rejects(
            "[[channel]]\nname = \"T\"\nfrequency = 155300000\ntone = \"D999N\"\n",
            "tone",
        );
        // No tone: carrier squelch.
        let cfg = channels("[[channel]]\nname = \"T\"\nfrequency = 155300000\n").expect("valid");
        assert_eq!(cfg.channel[0].params().expect("params").tone, None);
    }

    #[test]
    fn channel_bandwidth_is_12_5_khz_only() {
        channels("[[channel]]\nname = \"B\"\nfrequency = 155300000\nbandwidth_hz = 12500\n")
            .expect("12.5 kHz");
        rejects(
            "[[channel]]\nname = \"B\"\nfrequency = 155300000\nbandwidth_hz = 25000\n",
            "channel 'B': 25 kHz channels are not supported yet (bandwidth_hz = 25000); use 12500",
        );
        rejects(
            "[[channel]]\nname = \"B\"\nfrequency = 155300000\nbandwidth_hz = 6250\n",
            "bandwidth_hz must be 12500 (got 6250)",
        );
    }

    #[test]
    fn channel_squelch_open_must_exceed_close() {
        rejects(
            "[[channel]]\nname = \"S\"\nfrequency = 155300000\n\
             [channel.squelch]\nopen_db = 5.0\nclose_db = 5.0\n",
            "must be greater than close_db",
        );
        let cfg = channels(
            "[[channel]]\nname = \"S\"\nfrequency = 155300000\ndeemphasis_us = 0\n\
             [channel.squelch]\nopen_db = 10.0\nclose_db = 6.0\nhang_ms = 1500\n",
        )
        .expect("valid");
        let p = cfg.channel[0].params().expect("params");
        assert_eq!((p.squelch.open_db, p.squelch.close_db), (10.0, 6.0));
        assert_eq!(p.squelch.hang_ms, 1500);
        assert_eq!(p.squelch.attack_ms, 40, "unset fields keep the defaults");
        assert_eq!(p.deemphasis_us, 0);
    }

    /// What `trunkd import-sdrtrunk` wrote before conventional channels were
    /// built: an explicit device, sdrtrunk's four squelch numbers, no labels.
    #[test]
    fn legacy_sdrtrunk_squelch_fields_are_accepted_and_ignored() {
        let cfg = channels(
            r#"
            [[channel]]
            name = "Ops"
            device = "vhf0"
            frequency = 155300000
            mode = "nbfm"
            enabled = true
            bandwidth_hz = 12500
            talkgroup = 1

            [channel.squelch]
            noise_open = 0.1
            noise_close = 0.19
            hysteresis_open = 4
            hysteresis_close = 6
            "#,
        )
        .expect("legacy squelch still loads");
        let sq = cfg.channel[0].squelch.expect("kept");
        assert!(sq.has_legacy_fields());
        assert_eq!(sq.noise_open, Some(0.1));
        assert_eq!(
            sq.params(),
            trunkd_analog::SquelchParams::default(),
            "the legacy numbers change nothing"
        );
        // And it serializes back the way it came.
        let out = toml::to_string_pretty(&cfg).expect("serialize");
        assert!(out.contains("noise_open = 0.1"), "{out}");
        assert!(!out.contains("open_db"), "{out}");
    }

    /// AM is parsed and skipped at run time, and a disabled channel is never
    /// started: neither is held to the NBFM rules, so configs that loaded
    /// before still load.
    #[test]
    fn am_and_disabled_channels_are_not_held_to_the_nbfm_rules() {
        channels(
            r#"
            [[channel]]
            name = "Tower"
            device = "vhf0"
            frequency = 118300000
            mode = "am"
            bandwidth_hz = 8333
            [[channel]]
            name = "Off"
            device = "uhf0"
            frequency = 155000000
            enabled = false
            tone = "nonsense"
            "#,
        )
        .expect("AM and disabled channels load as before");
        // The device must still exist, as it always had to.
        rejects(
            "[[channel]]\nname = \"Tower\"\ndevice = \"gone\"\nfrequency = 118300000\nmode = \"am\"\n",
            "unknown device 'gone'",
        );
    }
}
