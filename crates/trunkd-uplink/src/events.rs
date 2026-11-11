// SPDX-License-Identifier: GPL-3.0-or-later
//! Batched delivery of non-voice **radio events** to Keyup.
//!
//! A radio event is what the control channel says about a *subscriber* rather
//! than about a call: it registered, it affiliated to a talkgroup, the FNE
//! denied it a channel, it asked for a data channel. `docs/NON-VOICE-DATA.md`
//! §4 measures these at roughly 600/hour on the Example Network site — three
//! orders of magnitude below the TSBK firehose, and the single best answer to
//! "which radios are here", because a radio that never keys up is invisible to
//! the call archive.
//!
//! # The contract
//!
//! ```text
//! POST <base>/api/recorder/events
//! Authorization: Bearer <api key>
//! Content-Type: application/json
//!
//! { "recorder": "trunkd", "recorder_version": "0.1.0",
//!   "system": { "label": …, "wacn": …, "system_id": …, "nac": … },
//!   "site":   { "label": …, "rfss": …, "site": … },
//!   "events": [ { "at": "2026-08-31T09:12:03.481Z", "kind": "group_affiliation",
//!                 "opcode": 40, "mfid": 0, "radio_id": 1620719,
//!                 "radio_wacn": null, "radio_system": null,
//!                 "target_radio_id": null, "talkgroup": 52101,
//!                 "result": "accepted", "frequency_hz": null, "meta": {…} } ] }
//! ```
//!
//! A success answers `200 {"stored": n}`. Anything else — including the `404`
//! Keyup returns until the route is deployed — spools the batch and is retried.
//!
//! # Spool
//!
//! ```text
//! <spool_dir>/events/YYYY/MM/DD/<unix-ms>-<seq>.json
//! ```
//!
//! One file per batch, holding the request body verbatim, written temp-and-
//! renamed like the call spool. A drain pass retries **oldest first** every
//! [`EventUplinkConfig::drain_interval`]; the directory is capped and evicted
//! oldest-first, because a recorder that cannot reach Keyup for a week must
//! cost a bounded number of bytes, not an unbounded one. The same free-space
//! floor as the call spool applies on top of that cap
//! ([`EventUplinkConfig::spool_min_free_bytes`]): batches are cheap, but a
//! volume that is out of space stops the *calls* too.
//!
//! Batches are formed by whichever comes first: [`MAX_BATCH`] events or
//! [`EventUplinkConfig::max_batch_age`].

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use serde_json::{json, Value};
use time::OffsetDateTime;

use crate::disk::{self, FreeSpaceFn, Sweep};

/// Events per batch, whichever limit is reached first.
pub const MAX_BATCH: usize = 500;

/// Derive the radio-events endpoint from the configured call-upload URL.
///
/// Same rule as the observations endpoint: the operator configures one URL and
/// the routes live on the same host, so the upload path is swapped out. A URL
/// that is not the upload route simply gains the route, which keeps a
/// hand-written override working.
pub fn events_url(call_upload_url: &str) -> String {
    const UPLOAD: &str = "/api/call-upload";
    const EVENTS: &str = "/api/recorder/events";
    let trimmed = call_upload_url.trim_end_matches('/');
    match trimmed.rfind(UPLOAD) {
        Some(at) => format!(
            "{}{EVENTS}{}",
            &trimmed[..at],
            &trimmed[at + UPLOAD.len()..]
        ),
        None => format!("{trimmed}{EVENTS}"),
    }
}

/// One non-voice event about one radio.
///
/// Every field but `at`, `kind`, `opcode` and `mfid` is optional: the messages
/// this is built from disagree about which of them they carry, and a `null` is
/// honest where a `0` would be a claim.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RadioEvent {
    /// Stream time of the message, in milliseconds.
    pub at_ms: u64,
    /// Event class, e.g. `unit_registration` or `group_affiliation`.
    pub kind: &'static str,
    /// TSBK opcode the event came from.
    pub opcode: u8,
    /// Manufacturer id (0 = standard).
    pub mfid: u8,
    /// The subscriber the message is about.
    pub radio_id: Option<u32>,
    /// The subscriber's WACN, when the message qualified it.
    pub radio_wacn: Option<u32>,
    /// The subscriber's home system id, when the message qualified it.
    pub radio_system: Option<u16>,
    /// Unit-to-unit or query target / the other party.
    pub target_radio_id: Option<u32>,
    /// Talkgroup the event concerns.
    pub talkgroup: Option<u32>,
    /// `accepted` | `failed` | `denied` | `refused`, where the message says.
    pub result: Option<&'static str>,
    /// Frequency the event granted or announced.
    pub frequency_hz: Option<u64>,
    /// Everything else the message carried, opcode-specific.
    pub meta: Value,
}

impl RadioEvent {
    /// Render as the wire object, with `at` an RFC 3339 instant.
    pub fn to_json(&self, at: String) -> Value {
        json!({
            "at": at,
            "kind": self.kind,
            "opcode": self.opcode,
            "mfid": self.mfid,
            "radio_id": self.radio_id,
            "radio_wacn": self.radio_wacn,
            "radio_system": self.radio_system,
            "target_radio_id": self.target_radio_id,
            "talkgroup": self.talkgroup,
            "result": self.result,
            "frequency_hz": self.frequency_hz,
            "meta": self.meta,
        })
    }
}

/// How the site the events came from identifies itself on the wire.
#[derive(Clone, Debug, Default)]
pub struct EventSite {
    /// System label, as configured.
    pub system_label: String,
    /// Site label, as configured.
    pub site_label: String,
    /// WACN heard from a network-status broadcast.
    pub wacn: Option<u32>,
    /// System id heard from a network- or RFSS-status broadcast.
    pub system_id: Option<u16>,
    /// NAC heard from the NID.
    pub nac: Option<u16>,
    /// RFSS id heard from an RFSS-status broadcast.
    pub rfss: Option<u8>,
    /// Site id heard from an RFSS-status broadcast.
    pub site: Option<u8>,
}

/// Build the full request body for one batch.
///
/// `instance` is *which* trunkd said it: `recorder` names the software, and
/// several recorders running it hear the same site. Without the instance a
/// receiver cannot tell two boxes' views of one radio apart, which is exactly
/// the question a multi-recorder deployment asks. The same value the
/// tuner-status POST sends as `instance_id`.
pub fn events_body(site: &EventSite, events: &[Value], instance: &str) -> Value {
    json!({
        "recorder": "trunkd",
        "instance": instance,
        "recorder_version": env!("CARGO_PKG_VERSION"),
        "system": {
            "label": site.system_label,
            "wacn": site.wacn,
            "system_id": site.system_id,
            "nac": site.nac,
        },
        "site": {
            "label": site.site_label,
            "rfss": site.rfss,
            "site": site.site,
        },
        "events": events,
    })
}

/// How to reach Keyup's events route, and what to do when it cannot be reached.
#[derive(Clone, Debug)]
pub struct EventUplinkConfig {
    /// Full endpoint URL. Empty disables posting; batches are dropped rather
    /// than spooled, because nothing is meant to leave the box.
    pub url: String,
    /// API token, sent as `Authorization: Bearer`.
    pub api_key: String,
    /// This recorder's stable id, stamped on every batch as `instance`.
    pub instance: String,
    /// Root of the event spool (a sibling of the call spool's date tree).
    pub spool_dir: PathBuf,
    /// Maximum bytes the event spool may occupy.
    pub spool_cap_bytes: u64,
    /// Evict spooled batches whenever the volume holding them has less than
    /// this much free space, whoever filled it. 0 disables the floor. Normally
    /// the same value as the call spool's, since the two share a volume.
    pub spool_min_free_bytes: u64,
    /// Timeout for one POST, connect through response.
    pub timeout: Duration,
    /// How long a partial batch waits before being sent anyway.
    pub max_batch_age: Duration,
    /// How often the spool is retried.
    pub drain_interval: Duration,
}

impl Default for EventUplinkConfig {
    fn default() -> Self {
        EventUplinkConfig {
            url: String::new(),
            api_key: String::new(),
            instance: String::new(),
            spool_dir: PathBuf::from("spool/events"),
            spool_cap_bytes: 64 * 1024 * 1024,
            spool_min_free_bytes: 2 * 1024 * 1024 * 1024,
            timeout: Duration::from_secs(10),
            max_batch_age: Duration::from_secs(10),
            drain_interval: Duration::from_secs(30),
        }
    }
}

/// What one POST attempt concluded.
#[derive(Debug, PartialEq, Eq)]
pub enum EventAttempt {
    /// Keyup stored the batch.
    Stored(u64),
    /// The route is not deployed on this Keyup yet.
    NotDeployed,
    /// Anything else: spool and retry.
    Retry(String),
}

/// Classify an events POST from its status and body.
pub fn classify_events(status: u16, body: &str) -> EventAttempt {
    if status == 404 {
        return EventAttempt::NotDeployed;
    }
    if (200..300).contains(&status) {
        let stored = serde_json::from_str::<Value>(body)
            .ok()
            .and_then(|v| v.get("stored").and_then(Value::as_u64))
            .unwrap_or(0);
        return EventAttempt::Stored(stored);
    }
    EventAttempt::Retry(format!("HTTP {status}: {}", body.trim()))
}

/// The bounded on-disk spool of undelivered batches.
///
/// Bounded the same two ways as the call spool: a byte cap on what trunkd
/// keeps, and a floor on what the volume keeps free for everyone.
#[derive(Clone, Debug)]
pub struct EventSpool {
    dir: PathBuf,
    cap_bytes: u64,
    min_free_bytes: u64,
    free_space: FreeSpaceFn,
}

/// One spooled batch.
#[derive(Clone, Debug)]
pub struct EventSpoolEntry {
    /// Path of the JSON body.
    pub path: PathBuf,
    /// Size on disk.
    pub bytes: u64,
    /// Modification time, for oldest-first ordering.
    pub written_at: SystemTime,
}

impl EventSpool {
    /// Open (creating if needed) a spool rooted at `dir`.
    ///
    /// The free-space floor is off until [`EventSpool::with_min_free_bytes`]
    /// sets one.
    pub fn open(dir: impl Into<PathBuf>, cap_bytes: u64) -> io::Result<Self> {
        let dir = dir.into();
        fs::create_dir_all(&dir)?;
        Ok(EventSpool {
            dir,
            cap_bytes,
            min_free_bytes: 0,
            free_space: disk::available_space,
        })
    }

    /// Also evict whenever the volume has less than `bytes` free. 0 disables
    /// the floor.
    pub fn with_min_free_bytes(mut self, bytes: u64) -> Self {
        self.min_free_bytes = bytes;
        self
    }

    /// Replace the free-space source; for tests, as on [`crate::Spool`].
    pub fn with_free_space_source(mut self, free_space: FreeSpaceFn) -> Self {
        self.free_space = free_space;
        self
    }

    /// The spool root.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Write one batch body under `YYYY/MM/DD/`, returning the bytes written.
    ///
    /// `unix_ms` dates the file and names it, with `seq` breaking ties between
    /// two batches spooled in the same millisecond.
    pub fn store(&self, body: &str, unix_ms: i64, seq: u64) -> io::Result<u64> {
        let dir = self.dir.join(date_path(unix_ms));
        fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{unix_ms}-{seq:06}.json"));
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, body.as_bytes())?;
        fs::rename(&tmp, &path)?;
        Ok(body.len() as u64)
    }

    /// Every spooled batch, oldest first.
    pub fn entries(&self) -> Vec<EventSpoolEntry> {
        let mut out = Vec::new();
        collect(&self.dir, &mut out);
        out.sort_by(|a, b| {
            a.written_at
                .cmp(&b.written_at)
                .then_with(|| a.path.cmp(&b.path))
        });
        out
    }

    /// Total bytes currently spooled.
    pub fn total_bytes(&self) -> u64 {
        self.entries().iter().map(|e| e.bytes).sum()
    }

    /// Delete one entry. A missing file is not an error.
    pub fn remove(&self, entry: &EventSpoolEntry) {
        let _ = fs::remove_file(&entry.path);
    }

    /// Drop oldest batches until the spool fits under its cap *and* the volume
    /// is back above its free-space floor.
    ///
    /// Safe to call on a timer with nothing new spooled: the volume can fill up
    /// without this recorder writing a byte.
    pub fn enforce_limits(&self) -> Sweep {
        let entries = self.entries();
        disk::enforce(
            &entries,
            |e| e.bytes,
            |e| self.remove(e),
            &self.dir,
            self.cap_bytes,
            self.min_free_bytes,
            self.free_space,
        )
    }
}

fn collect(dir: &Path, out: &mut Vec<EventSpoolEntry>) {
    let Ok(rd) = fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(&path, out);
        } else if path.extension().is_some_and(|e| e == "json") {
            let Ok(meta) = entry.metadata() else { continue };
            out.push(EventSpoolEntry {
                bytes: meta.len(),
                written_at: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                path,
            });
        }
    }
}

/// `YYYY/MM/DD` for a unix-millisecond instant, in UTC.
fn date_path(unix_ms: i64) -> PathBuf {
    let secs = unix_ms.div_euclid(1000);
    let dt = OffsetDateTime::from_unix_timestamp(secs).unwrap_or(OffsetDateTime::UNIX_EPOCH);
    PathBuf::from(format!(
        "{:04}/{:02}/{:02}",
        dt.year(),
        u8::from(dt.month()),
        dt.day()
    ))
}

/// Accumulates events and says when a batch is due.
#[derive(Debug)]
pub struct EventBatcher {
    pending: Vec<Value>,
    since: Instant,
    max_age: Duration,
}

impl EventBatcher {
    /// A batcher that flushes after `max_age` or [`MAX_BATCH`] events.
    pub fn new(max_age: Duration) -> Self {
        EventBatcher {
            pending: Vec::new(),
            since: Instant::now(),
            max_age,
        }
    }

    /// Queue one rendered event.
    pub fn push(&mut self, event: Value) {
        if self.pending.is_empty() {
            self.since = Instant::now();
        }
        self.pending.push(event);
    }

    /// Number of events waiting.
    pub fn len(&self) -> usize {
        self.pending.len()
    }

    /// True when nothing is waiting.
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    /// True when the batch is full or has waited long enough.
    pub fn is_due(&self) -> bool {
        !self.pending.is_empty()
            && (self.pending.len() >= MAX_BATCH || self.since.elapsed() >= self.max_age)
    }

    /// Take everything queued, whether or not it was due.
    pub fn take(&mut self) -> Vec<Value> {
        self.since = Instant::now();
        std::mem::take(&mut self.pending)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_events_endpoint_replaces_the_upload_route() {
        assert_eq!(
            events_url("http://192.0.2.60/api/call-upload"),
            "http://192.0.2.60/api/recorder/events"
        );
        assert_eq!(
            events_url("http://192.0.2.60/api/call-upload/"),
            "http://192.0.2.60/api/recorder/events"
        );
        assert_eq!(
            events_url("http://192.0.2.60"),
            "http://192.0.2.60/api/recorder/events"
        );
    }

    #[test]
    fn a_stored_reply_reports_its_count() {
        assert_eq!(
            classify_events(200, r#"{"stored": 42}"#),
            EventAttempt::Stored(42)
        );
        // A 2xx with a body Keyup did not shape is still a success.
        assert_eq!(classify_events(201, "ok"), EventAttempt::Stored(0));
    }

    #[test]
    fn a_404_is_the_route_not_being_deployed_yet() {
        assert_eq!(classify_events(404, "Not Found"), EventAttempt::NotDeployed);
    }

    #[test]
    fn anything_else_is_retried() {
        assert!(matches!(
            classify_events(500, "boom"),
            EventAttempt::Retry(_)
        ));
        assert!(matches!(
            classify_events(401, "invalid key"),
            EventAttempt::Retry(_)
        ));
    }

    #[test]
    fn a_batch_is_due_on_size_or_age() {
        let mut b = EventBatcher::new(Duration::from_secs(3600));
        assert!(!b.is_due(), "an empty batch is never due");
        for _ in 0..MAX_BATCH - 1 {
            b.push(json!({}));
        }
        assert!(!b.is_due());
        b.push(json!({}));
        assert!(b.is_due(), "{MAX_BATCH} events fill a batch");
        assert_eq!(b.take().len(), MAX_BATCH);
        assert!(b.is_empty());

        // Age alone is enough.
        let mut b = EventBatcher::new(Duration::ZERO);
        b.push(json!({}));
        assert!(b.is_due());
    }

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let mut p = std::env::temp_dir();
            let nanos = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            p.push(format!("trunkd-{tag}-{}-{nanos}", std::process::id()));
            let _ = fs::remove_dir_all(&p);
            fs::create_dir_all(&p).expect("temp dir");
            TempDir(p)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_sweep_puts_back_a_spool_directory_something_removed() {
        // The call spool's pruning once deleted this spool's base directory;
        // the sweep must recreate it and keep measuring rather than disable
        // the floor with a warning about a path that is easy to put back.
        let dir = TempDir::new("events-swept-away");
        let base = dir.0.join("events");
        let spool = EventSpool::open(&base, 1 << 20).expect("open");
        std::fs::remove_dir_all(&base).expect("someone else pruned it");
        let sweep = spool.enforce_limits();
        assert!(base.exists(), "the sweep recreates its directory");
        assert!(
            sweep.free_bytes.is_some(),
            "and the floor is measured, not skipped"
        );
    }

    #[test]
    fn a_spooled_batch_lands_under_its_utc_date_and_reads_back() {
        let dir = TempDir::new("events-spool");
        let spool = EventSpool::open(&dir.0, 1 << 20).expect("open");
        // 2026-08-31T12:00:00Z
        let at = 1_788_177_600_000i64;
        spool.store(r#"{"events":[]}"#, at, 0).expect("store");

        let entries = spool.entries();
        assert_eq!(entries.len(), 1);
        let parent = entries[0].path.parent().expect("date dir");
        assert!(
            parent.ends_with(Path::new("2026").join("08").join("31")),
            "{:?}",
            entries[0].path
        );
        assert_eq!(
            fs::read_to_string(&entries[0].path).expect("read"),
            r#"{"events":[]}"#
        );
        // The temp file is renamed, never left behind.
        assert!(!entries[0].path.with_extension("json.tmp").exists());
    }

    #[test]
    fn the_spool_evicts_oldest_first_under_its_cap() {
        let dir = TempDir::new("events-cap");
        let body = "x".repeat(100);
        let spool = EventSpool::open(&dir.0, 250).expect("open");
        for i in 0..5u64 {
            spool
                .store(&body, 1_788_177_600_000 + i as i64 * 1000, i)
                .expect("store");
            // Distinct mtimes, so "oldest" is well defined.
            std::thread::sleep(Duration::from_millis(12));
        }
        assert_eq!(spool.entries().len(), 5);
        let sweep = spool.enforce_limits();
        assert_eq!(sweep.cap_evicted, 3, "500 bytes over a 250-byte cap");
        assert!(sweep.bytes <= 250);
        let left = spool.entries();
        assert_eq!(left.len(), 2);
        // The two survivors are the newest two.
        assert!(left[0].path.to_string_lossy().contains("-000003"));
        assert!(left[1].path.to_string_lossy().contains("-000004"));
    }

    #[test]
    fn the_body_names_the_system_and_site() {
        let site = EventSite {
            system_label: "Example Network".into(),
            site_label: "Central".into(),
            wacn: Some(0xBEE00),
            system_id: Some(0x1F5),
            nac: Some(0x293),
            rfss: Some(10),
            site: Some(1),
        };
        let ev = RadioEvent {
            at_ms: 0,
            kind: "group_affiliation",
            opcode: 40,
            mfid: 0,
            radio_id: Some(1_620_719),
            talkgroup: Some(52_101),
            result: Some("accepted"),
            ..RadioEvent::default()
        };
        let body = events_body(
            &site,
            &[ev.to_json("2026-08-31T09:12:03.481Z".into())],
            "rec-01",
        );
        assert_eq!(body["recorder"], "trunkd");
        assert_eq!(body["instance"], "rec-01");
        assert_eq!(body["system"]["wacn"], 0xBEE00);
        assert_eq!(body["site"]["rfss"], 10);
        let e = &body["events"][0];
        assert_eq!(e["kind"], "group_affiliation");
        assert_eq!(e["radio_id"], 1_620_719);
        assert_eq!(e["talkgroup"], 52_101);
        assert_eq!(e["result"], "accepted");
        // Absent fields are explicit nulls, not omitted: Keyup's column is
        // nullable and "we did not hear one" is the fact being reported.
        assert!(e["radio_wacn"].is_null());
        assert!(e["frequency_hz"].is_null());
    }

    fn nearly_full(_: &Path) -> io::Result<u64> {
        Ok(120)
    }

    #[test]
    fn the_event_spool_honours_the_free_space_floor_too() {
        let dir = TempDir::new("events-floor");
        let body = "x".repeat(100);
        let spool = EventSpool::open(&dir.0, u64::MAX)
            .expect("open")
            .with_min_free_bytes(300)
            .with_free_space_source(nearly_full);
        for i in 0..5u64 {
            spool
                .store(&body, 1_788_177_600_000 + i as i64 * 1000, i)
                .expect("store");
            std::thread::sleep(Duration::from_millis(12));
        }
        let sweep = spool.enforce_limits();
        assert_eq!(sweep.cap_evicted, 0, "the cap is effectively infinite");
        // 120 free + two 100-byte batches clears a 300-byte floor.
        assert_eq!(sweep.floor_evicted, 2);
        let left = spool.entries();
        assert_eq!(left.len(), 3);
        assert!(left[0].path.to_string_lossy().contains("-000002"));
    }
}
