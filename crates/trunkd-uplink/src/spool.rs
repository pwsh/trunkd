// SPDX-License-Identifier: GPL-3.0-or-later
//! The bounded offline spool.
//!
//! When Keyup cannot be reached the call is written to disk instead of being
//! lost (ARCHITECTURE §3, `trunkd-uplink`):
//!
//! ```text
//! <spool_dir>/YYYY/MM/DD/<system>_<talkgroup>_<start-unix-ms>.mp3   the audio
//! <spool_dir>/YYYY/MM/DD/<system>_<talkgroup>_<start-unix-ms>.json  the sidecar
//! ```
//!
//! The date directory is the call's **start** date in UTC. Both files are
//! written to a `.tmp` sibling and renamed into place, so a crash mid-write
//! never leaves a half call behind — the spool is the only state in the process
//! that matters, and it is crash-safe by construction (ARCHITECTURE §6). The
//! audio is written first and the sidecar last, so a file-ingest watcher that
//! sees the JSON always finds complete audio beside it.
//!
//! An encrypted call spools its sidecar alone: Keyup ingests a metadata-only
//! call from the JSON.
//!
//! The sidecar is [`trunkd_call::sidecar_json`], which means a spool directory
//! can be handed to Keyup's file ingest as-is *and* can be read back by
//! [`call_from_sidecar`] when the uplink retries.
//!
//! The directory is bounded twice over — a byte cap on what trunkd keeps and a
//! free-space floor on what the volume must keep free for everyone else — both
//! evicting oldest-first from [`Spool::enforce_limits`]. See the
//! [`disk`](crate::disk) module for the reasoning.
//!
//! # Quarantine
//!
//! ```text
//! <spool_dir>/quarantine/YYYY/MM/DD/<stem>.{mp3,json}
//! ```
//!
//! A spooled call that keeps being refused while Keyup accepts everything
//! around it is moved here by [`Spool::quarantine`] rather than deleted, so it
//! stops holding up the drain without being lost. Nothing retries it: an
//! operator looks at it, and either moves it back under the spool root or
//! deletes it. It still counts against the cap and the floor — it is evicted
//! *first*, being the least likely to ever be delivered — so a quarantine that
//! grows cannot fill the disk either.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde_json::Value;
use time::OffsetDateTime;
use trunkd_call::{
    sidecar_json, BroadcastifyOutcome, CallKind, FinishedCall, FrequencySegment, SourceQuality,
    SourceSegment,
};

use crate::disk::{self, FreeSpaceFn, Sweep};

/// The quarantine directory's name under the spool root. Not all digits, so
/// [`is_owned_dir`] keeps every walk of the spool proper out of it.
pub const QUARANTINE_DIR: &str = "quarantine";

/// One spooled call: its sidecar, the audio beside it (absent when encrypted),
/// and what the pair costs on disk.
#[derive(Clone, Debug)]
pub struct SpoolEntry {
    /// Path of the JSON sidecar.
    pub json: PathBuf,
    /// Path of the audio file, if this call has audio.
    pub audio: Option<PathBuf>,
    /// Total bytes of both files.
    pub bytes: u64,
    /// Modification time of the sidecar, used for oldest-first ordering.
    pub written_at: SystemTime,
}

/// A directory of spooled calls with a size cap and a free-space floor.
///
/// The two limits are independent and both evict oldest-first: the cap bounds
/// what trunkd itself keeps on the volume, the floor keeps the volume usable no
/// matter who else is filling it. See the [`disk`](crate::disk) module for why
/// both exist.
#[derive(Clone, Debug)]
pub struct Spool {
    dir: PathBuf,
    cap_bytes: u64,
    min_free_bytes: u64,
    free_space: FreeSpaceFn,
}

impl Spool {
    /// Open (creating if needed) a spool rooted at `dir`, capped at
    /// `cap_bytes` total.
    ///
    /// The free-space floor is off until [`Spool::with_min_free_bytes`] sets
    /// one: a spool opened by a test or a tool has no business deleting calls
    /// because the host's disk happens to be full.
    pub fn open(dir: impl Into<PathBuf>, cap_bytes: u64) -> io::Result<Self> {
        let dir = dir.into();
        fs::create_dir_all(&dir)?;
        Ok(Spool {
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

    /// Replace the free-space source. For tests: enforcement must be
    /// exercisable without depending on the host's disk.
    pub fn with_free_space_source(mut self, free_space: FreeSpaceFn) -> Self {
        self.free_space = free_space;
        self
    }

    /// The spool root.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Write one call: `<YYYY>/<MM>/<DD>/<stem>.<ext>` plus `<stem>.json`.
    ///
    /// Returns the bytes written. Pass `audio: None` for a metadata-only call.
    pub fn store(&self, call: &FinishedCall, audio: Option<&[u8]>, ext: &str) -> io::Result<u64> {
        let dir = self.dir.join(date_path(call.started_at_ms));
        fs::create_dir_all(&dir)?;
        let stem = call.audio_stem();

        let mut written = 0u64;
        if let Some(bytes) = audio.filter(|b| !b.is_empty()) {
            let path = dir.join(format!("{stem}.{ext}"));
            write_atomic(&path, bytes)?;
            written += bytes.len() as u64;
        }
        let sidecar = sidecar_json(call);
        write_atomic(&dir.join(format!("{stem}.json")), sidecar.as_bytes())?;
        written += sidecar.len() as u64;
        Ok(written)
    }

    /// Every spooled call, oldest first.
    ///
    /// Ordering is by sidecar mtime (then path, so the order is total and
    /// stable): the spool is drained and evicted oldest-first.
    pub fn entries(&self) -> Vec<SpoolEntry> {
        oldest_first(&self.dir)
    }

    /// Every quarantined call, oldest first. See [`Spool::quarantine`].
    pub fn quarantined(&self) -> Vec<SpoolEntry> {
        oldest_first(&self.dir.join(QUARANTINE_DIR))
    }

    /// Move one entry out of the drain's way into
    /// `<spool>/quarantine/YYYY/MM/DD/`, keeping its files and their names.
    ///
    /// The sidecar moves first: from that instant the drain no longer sees the
    /// entry, and a crash before the audio follows leaves the audio behind in
    /// the spool rather than a sidecar-only call the drain would post without
    /// its audio. Returns the sidecar's new path.
    pub fn quarantine(&self, entry: &SpoolEntry) -> io::Result<PathBuf> {
        let rel = entry.json.strip_prefix(&self.dir).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "entry is not in this spool")
        })?;
        let dest = self.dir.join(QUARANTINE_DIR).join(rel);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::rename(&entry.json, &dest)?;
        if let Some(audio) = &entry.audio {
            if let Some(name) = audio.file_name() {
                let target = dest.with_file_name(name);
                if let Err(e) = fs::rename(audio, &target) {
                    tracing::warn!(
                        audio = %audio.display(),
                        error = %e,
                        "quarantined a call's sidecar but not its audio"
                    );
                }
            }
        }
        Ok(dest)
    }

    /// Total bytes currently spooled.
    pub fn total_bytes(&self) -> u64 {
        self.entries().iter().map(|e| e.bytes).sum()
    }

    /// Delete one entry (both files). Missing files are not an error — a drain
    /// that raced an eviction is normal.
    pub fn remove(&self, entry: &SpoolEntry) {
        if let Some(audio) = &entry.audio {
            let _ = fs::remove_file(audio);
        }
        let _ = fs::remove_file(&entry.json);
    }

    /// Drop oldest entries until the spool fits under its cap *and* the volume
    /// is back above its free-space floor.
    ///
    /// Safe to call on a timer with nothing new stored — that is the point: a
    /// disk filled by something other than trunkd would otherwise only be
    /// noticed the next time a call happened to be spooled.
    ///
    /// Quarantined calls count towards both limits and go first: they are the
    /// ones least likely ever to be delivered.
    pub fn enforce_limits(&self) -> Sweep {
        let mut entries = self.quarantined();
        entries.extend(self.entries());
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

    /// Remove date directories left empty by draining or eviction.
    ///
    /// Best effort and cheap: `remove_dir` fails harmlessly on a non-empty
    /// directory, which is exactly the test we want.
    pub fn prune_empty_dirs(&self) {
        prune(&self.dir, 0);
        // Its own `YYYY/MM/DD` under it; depth 0 keeps the quarantine root.
        prune(&self.dir.join(QUARANTINE_DIR), 0);
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

/// Write `bytes` to `path` via a temporary sibling and a rename.
fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension(format!(
        "{}.tmp",
        path.extension().and_then(|e| e.to_str()).unwrap_or("part")
    ));
    fs::write(&tmp, bytes)?;
    match fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Whether a directory under the spool root belongs to this spool's
/// `YYYY/MM/DD` layout — an all-digit name.
///
/// The spool may share its base directory with someone else's tree: the
/// radio-event spool lives at `<spool>/events`, and its batches are `*.json`
/// too. Every walk of this spool (listing, draining, eviction, pruning) goes
/// through this one test, so a foreign tree is never read as spooled calls —
/// once, the call drain posted each spooled event batch to Keyup as a
/// talkgroup-0 call and deleted it when Keyup refused. The same test keeps the
/// drain out of [`QUARANTINE_DIR`], which is walked on its own.
fn is_owned_dir(entry: &fs::DirEntry) -> bool {
    entry
        .file_name()
        .to_str()
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// The calls under `dir`'s `YYYY/MM/DD` tree, by sidecar mtime (then path, so
/// the order is total and stable).
fn oldest_first(dir: &Path) -> Vec<SpoolEntry> {
    let mut out = Vec::new();
    collect(dir, &mut out);
    out.sort_by(|a, b| {
        a.written_at
            .cmp(&b.written_at)
            .then_with(|| a.json.cmp(&b.json))
    });
    out
}

fn collect(dir: &Path, out: &mut Vec<SpoolEntry>) {
    let Ok(read) = fs::read_dir(dir) else {
        return;
    };
    for entry in read.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if is_owned_dir(&entry) {
                collect(&path, out);
            }
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let json_len = entry.metadata().map(|m| m.len()).unwrap_or(0);
        let written_at = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        let (audio, audio_len) = find_audio(&path);
        out.push(SpoolEntry {
            json: path,
            audio,
            bytes: json_len + audio_len,
            written_at,
        });
    }
}

/// The audio file beside a sidecar, whichever format it was spooled in.
fn find_audio(json: &Path) -> (Option<PathBuf>, u64) {
    for ext in ["mp3", "wav", "m4a"] {
        let candidate = json.with_extension(ext);
        if let Ok(meta) = fs::metadata(&candidate) {
            if meta.is_file() {
                return (Some(candidate), meta.len());
            }
        }
    }
    (None, 0)
}

fn prune(dir: &Path, depth: usize) {
    let Ok(read) = fs::read_dir(dir) else {
        return;
    };
    for entry in read.flatten() {
        let path = entry.path();
        // Recurse only into the `YYYY/MM/DD` layout this spool writes:
        // pruning a directory this spool did not create once deleted the
        // event spool's base out from under its free-space sweep.
        if is_owned_dir(&entry) && path.is_dir() {
            prune(&path, depth + 1);
        }
    }
    if depth > 0 {
        let _ = fs::remove_dir(dir);
    }
}

/// Rebuild a [`FinishedCall`] from the sidecar written by
/// [`trunkd_call::sidecar_json`] — the reverse direction, used when the drain
/// pass retries a spooled call.
///
/// The audio is *not* part of it: [`FinishedCall::pcm`] comes back empty and
/// the encoded bytes are read from the file beside the sidecar and posted
/// as-is, so a spooled call is never re-encoded.
///
/// Returns `None` if the text is not a JSON object; every individual field
/// degrades to a default rather than failing, because a call with some
/// metadata missing is still worth uploading.
pub fn call_from_sidecar(text: &str) -> Option<FinishedCall> {
    let v: Value = serde_json::from_str(text).ok()?;
    let obj = v.as_object()?;

    let started_at_ms = obj
        .get("start_time_ms")
        .and_then(Value::as_i64)
        .or_else(|| {
            obj.get("start_time")
                .and_then(Value::as_i64)
                .map(|s| s * 1000)
        })
        .unwrap_or(0);
    let ended_at_ms = obj
        .get("stop_time_ms")
        .and_then(Value::as_i64)
        .or_else(|| {
            obj.get("stop_time")
                .and_then(Value::as_i64)
                .map(|s| s * 1000)
        })
        .unwrap_or(started_at_ms);

    let sources: Vec<SourceSegment> = obj
        .get("srcList")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|e| {
                    Some(SourceSegment {
                        src: e.get("src").and_then(Value::as_u64)? as u32,
                        pos_s: e.get("pos").and_then(Value::as_f64).unwrap_or(0.0),
                        emergency: truthy(e.get("emergency")),
                        tag: e
                            .get("tag")
                            .and_then(Value::as_str)
                            .filter(|s| !s.is_empty())
                            .map(str::to_owned),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    let frequencies: Vec<FrequencySegment> = obj
        .get("freqList")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|e| {
                    Some(FrequencySegment {
                        freq_hz: e.get("freq").and_then(Value::as_u64)?,
                        pos_s: e.get("pos").and_then(Value::as_f64).unwrap_or(0.0),
                        len_s: e.get("len").and_then(Value::as_f64).unwrap_or(0.0),
                        error_count: e.get("error_count").and_then(Value::as_u64).unwrap_or(0)
                            as u32,
                        spike_count: e.get("spike_count").and_then(Value::as_u64).unwrap_or(0)
                            as u32,
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    Some(FinishedCall {
        system: string(obj.get("short_name")).unwrap_or_default(),
        site: string(obj.get("site")).unwrap_or_default(),
        started_at_ms,
        ended_at_ms,
        frequency_hz: obj.get("freq").and_then(Value::as_u64).unwrap_or(0),
        talkgroup: obj.get("talkgroup").and_then(Value::as_u64).unwrap_or(0) as u32,
        // A private call's sidecar says so; every sidecar written before the
        // key existed (and every group call's) has no `call_kind` and loads as
        // the group call it is.
        call_kind: match obj.get("call_kind").and_then(Value::as_str) {
            Some("unit") => CallKind::Unit,
            _ => CallKind::Group,
        },
        target_radio: obj
            .get("target_radio_id")
            .and_then(Value::as_u64)
            .map(|v| v as u32),
        talkgroup_label: string(obj.get("talkgroup_group_tag")),
        source: obj
            .get("source")
            .and_then(Value::as_u64)
            .map(|v| v as u32)
            .or_else(|| sources.first().map(|s| s.src)),
        sources,
        frequencies,
        patches: obj
            .get("patched_talkgroups")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_u64)
                    .map(|v| v as u32)
                    .collect()
            })
            .unwrap_or_default(),
        emergency: truthy(obj.get("emergency")),
        encrypted: truthy(obj.get("encrypted")),
        algorithm: string(obj.get("algorithm")),
        key_id: obj.get("keyId").and_then(Value::as_u64).map(|v| v as u16),
        talker_alias: string(obj.get("talkerAlias")),
        source_wacn: obj
            .get("sourceWacn")
            .and_then(Value::as_u64)
            .map(|v| v as u32),
        source_system: obj
            .get("sourceSystem")
            .and_then(Value::as_u64)
            .map(|v| v as u16),
        nac: obj.get("nac").and_then(Value::as_u64).map(|v| v as u16),
        // Absent in every sidecar written before this field existed, and in
        // any written by a recorder with nothing to report; `SourceQuality`
        // defaults every member, so a partial object loads too.
        source_quality: obj
            .get("sourceQuality")
            .cloned()
            .and_then(|v| serde_json::from_value::<SourceQuality>(v).ok())
            .filter(|q| !q.is_empty()),
        // Absent unless direct-to-Broadcastify decided something about this
        // call before it was spooled. Reading it back is what stops the drain
        // pass registering the call with Broadcastify a second time.
        broadcastify: obj
            .get("broadcastify")
            .cloned()
            .and_then(|v| serde_json::from_value::<BroadcastifyOutcome>(v).ok()),
        // Measured when the call was assembled and written into the sidecar,
        // because the spooled copy is an MP3 and the PCM is long gone. A
        // sidecar written before the key existed reads as 0.0, which only ever
        // costs a retry its Broadcastify attempt — and that attempt was already
        // decided (and recorded above) before the call was spooled.
        speech_s: obj
            .get("speechS")
            .and_then(Value::as_f64)
            .filter(|v| v.is_finite() && *v >= 0.0)
            .unwrap_or(0.0) as f32,
        pcm: Vec::new(),
    })
}

fn string(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// trunk-recorder writes flags as `0`/`1`; be liberal about booleans too.
fn truthy(v: Option<&Value>) -> bool {
    match v {
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_i64().unwrap_or(0) != 0,
        Some(Value::String(s)) => matches!(s.as_str(), "1" | "true" | "yes" | "on"),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use trunkd_call::CallBuilder;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let mut p = std::env::temp_dir();
            p.push(format!(
                "trunkd-spool-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
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
    fn prune_leaves_a_sibling_tree_it_does_not_own_alone() {
        // The radio-event spool lives at `<spool>/events`; pruning it away
        // left its free-space sweep measuring a path that no longer existed.
        let dir = TempDir::new("prune-foreign");
        let spool = Spool::open(&dir.0, 1 << 30).expect("open");
        let foreign = dir.0.join("events");
        fs::create_dir_all(&foreign).expect("foreign dir");
        let empty_date = dir.0.join("2026").join("09").join("01");
        fs::create_dir_all(&empty_date).expect("date dirs");
        spool.prune_empty_dirs();
        assert!(!empty_date.exists(), "its own empty date dirs go");
        assert!(foreign.exists(), "a tree it did not create stays");
    }

    /// A call spool with two real calls and the radio-event spool nested at
    /// `<spool>/events`, as the recorder lays them out.
    fn spool_beside_an_event_spool(dir: &Path, cap: u64) -> (Spool, crate::events::EventSpool) {
        let spool = Spool::open(dir, cap).expect("open");
        let events = crate::events::EventSpool::open(dir.join("events"), 1 << 30).expect("events");
        let audio = vec![0u8; 4096];
        for i in 0..2 {
            let call = call(1_788_012_000_123 + i * 86_400_000);
            spool.store(&call, Some(&audio), "mp3").expect("store");
        }
        // A batch body is a JSON object, which `call_from_sidecar` would
        // happily read as a talkgroup-0 call.
        let body = r#"{"site":{"system":"S"},"events":[{"kind":"affiliation"}]}"#;
        events.store(body, 1_788_012_000_500, 0).expect("batch");
        events.store(body, 1_788_098_400_500, 1).expect("batch");
        (spool, events)
    }

    #[test]
    fn the_event_spool_nested_under_the_call_spool_is_not_calls() {
        let dir = TempDir::new("events-nested");
        let (spool, events) = spool_beside_an_event_spool(&dir.0, 1 << 30);
        let entries = spool.entries();
        assert_eq!(entries.len(), 2, "only the calls: {entries:?}");
        assert!(entries
            .iter()
            .all(|e| !e.json.starts_with(events.dir()) && e.audio.is_some()));
        let calls: u64 = entries.iter().map(|e| e.bytes).sum();
        assert_eq!(spool.total_bytes(), calls, "event bytes are not counted");
        assert_eq!(events.entries().len(), 2);
    }

    #[test]
    fn eviction_and_pruning_never_touch_the_event_spool() {
        let dir = TempDir::new("events-evict");
        // A cap nothing fits under: every call goes, and only calls.
        let (spool, events) = spool_beside_an_event_spool(&dir.0, 1);
        let before = events.entries();
        let sweep = spool.enforce_limits();
        assert_eq!(sweep.cap_evicted, 2);
        assert_eq!(sweep.bytes, 0);
        spool.prune_empty_dirs();
        assert!(spool.entries().is_empty());
        let after = events.entries();
        assert_eq!(after.len(), 2, "the event backlog survives");
        assert!(before.iter().all(|e| e.path.is_file()));
    }

    #[test]
    fn a_quarantined_call_leaves_the_drain_but_not_the_disk_budget() {
        let dir = TempDir::new("quarantine");
        let (spool, events) = spool_beside_an_event_spool(&dir.0, 1 << 30);
        let before = spool.entries();
        assert_eq!(before.len(), 2);

        let dest = spool.quarantine(&before[0]).expect("quarantine");
        assert!(dest.starts_with(dir.0.join(QUARANTINE_DIR)));
        assert!(dest.ends_with(before[0].json.strip_prefix(&dir.0).unwrap()));
        assert!(!before[0].json.exists() && !before[0].audio.as_ref().unwrap().exists());

        // The drain's walk skips it, like it skips the event spool.
        let entries = spool.entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].json, before[1].json);
        let q = spool.quarantined();
        assert_eq!(q.len(), 1);
        assert_eq!(q[0].json, dest);
        assert!(q[0].audio.is_some(), "the audio moved with it");
        assert_eq!(q[0].bytes, before[0].bytes);

        // Pruning keeps the quarantine root and the files in it.
        spool.prune_empty_dirs();
        assert!(dest.is_file());

        // It still counts against the cap, and goes first.
        let one_call = before[1].bytes;
        let capped = Spool::open(&dir.0, one_call).expect("reopen");
        let sweep = capped.enforce_limits();
        assert_eq!(sweep.cap_evicted, 1);
        assert_eq!(sweep.bytes, one_call);
        assert!(capped.quarantined().is_empty(), "the quarantined call went");
        assert_eq!(capped.entries().len(), 1, "the drainable one stayed");
        capped.prune_empty_dirs();
        assert!(dir.0.join(QUARANTINE_DIR).is_dir(), "the root stays");
        assert_eq!(events.entries().len(), 2, "the event spool is untouched");
    }

    fn call(started_at_ms: i64) -> FinishedCall {
        let mut b = CallBuilder::new(
            "Example Network",
            "Site 1",
            started_at_ms,
            852_137_500,
            52101,
        );
        b.set_talkgroup_label(Some("PD DISPATCH".into()));
        b.set_nac(Some(659));
        b.set_patches(vec![19282]);
        b.talker(9_000_011, started_at_ms, true, Some("UNIT_313".into()));
        // Loud enough to measure: half a second of speech in a one-second call.
        b.push_pcm(
            &(0..4000)
                .map(|i| if i % 2 == 0 { 4000 } else { -4000 })
                .collect::<Vec<i16>>(),
        );
        b.push_pcm(&[0i16; 4000]);
        b.finish(started_at_ms + 1000)
    }

    #[test]
    fn stores_under_the_utc_date_and_renames_into_place() {
        let dir = TempDir::new("store");
        let spool = Spool::open(&dir.0, 1 << 20).expect("open");
        // 1788012000123 ms = 2026-08-29T14:00:00.123Z
        let call = call(1_788_012_000_123);
        spool.store(&call, Some(b"MP3BYTES"), "mp3").expect("store");

        let day = dir.0.join("2026/08/29");
        let stem = "Example-Network_52101_1788012000123";
        assert!(day.join(format!("{stem}.mp3")).is_file());
        assert!(day.join(format!("{stem}.json")).is_file());
        // No temporary files survive.
        let leftovers: Vec<_> = fs::read_dir(&day)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains("tmp"))
            .collect();
        assert!(leftovers.is_empty(), "left {leftovers:?}");
    }

    #[test]
    fn encrypted_call_spools_the_sidecar_alone() {
        let dir = TempDir::new("enc");
        let spool = Spool::open(&dir.0, 1 << 20).expect("open");
        let mut b = CallBuilder::new("S", "T", 1_788_012_000_000, 851_000_000, 7);
        b.set_encryption(Some("AES-256".into()), Some(42));
        let call = b.finish(1_788_012_002_000);
        spool.store(&call, None, "mp3").expect("store");

        let entries = spool.entries();
        assert_eq!(entries.len(), 1);
        assert!(entries[0].audio.is_none());
    }

    #[test]
    fn round_trips_a_call_through_its_sidecar() {
        let original = call(1_788_012_000_123);
        let back = call_from_sidecar(&sidecar_json(&original)).expect("parses");
        assert_eq!(back.system, original.system);
        assert_eq!(back.site, original.site);
        assert_eq!(back.started_at_ms, original.started_at_ms);
        assert_eq!(back.ended_at_ms, original.ended_at_ms);
        assert_eq!(back.frequency_hz, original.frequency_hz);
        assert_eq!(back.talkgroup, original.talkgroup);
        assert_eq!(back.talkgroup_label, original.talkgroup_label);
        assert_eq!(back.source, original.source);
        assert_eq!(back.sources, original.sources);
        assert_eq!(back.frequencies, original.frequencies);
        assert_eq!(back.patches, original.patches);
        assert_eq!(back.emergency, original.emergency);
        assert_eq!(back.nac, original.nac);
        // Measured from PCM the spooled copy no longer has, so it rides in the
        // sidecar: a retry must not read as a silent call.
        assert_eq!(original.speech_s, 0.5);
        assert_eq!(back.speech_s, original.speech_s);
        assert!(back.pcm.is_empty());
        assert_eq!(back.audio_stem(), original.audio_stem());
    }

    #[test]
    fn round_trips_an_encrypted_call() {
        let mut b = CallBuilder::new("S", "T", 1_788_012_000_000, 851_000_000, 7);
        b.set_encryption(Some("AES-256".into()), Some(42));
        b.set_talker_alias(Some("MEDIC 1".into()));
        let original = b.finish(1_788_012_002_000);
        let back = call_from_sidecar(&sidecar_json(&original)).expect("parses");
        assert!(back.encrypted);
        assert_eq!(back.algorithm.as_deref(), Some("AES-256"));
        assert_eq!(back.key_id, Some(42));
        assert_eq!(back.talker_alias.as_deref(), Some("MEDIC 1"));
    }

    #[test]
    fn round_trips_source_quality_so_a_retry_reports_the_same_facts() {
        // The whole point of spooling: a call that waited out an outage must be
        // re-offered with exactly the quality facts it would have carried live,
        // or a best-source negotiation would rank the retry below a rival copy
        // for no reason but the delay.
        let mut b = CallBuilder::new("S", "T", 1_788_012_000_000, 851_000_000, 7);
        b.set_instance(Some("rec-north-01".into()));
        b.set_rssi_dbfs(Some(-38.4));
        for i in 0..100 {
            b.push_voice_frame(&[3i16; 160], i % 20 == 0);
        }
        let original = b.finish(1_788_012_002_000);
        let back = call_from_sidecar(&sidecar_json(&original)).expect("parses");
        assert_eq!(back.source_quality, original.source_quality);
        let q = back
            .source_quality
            .expect("quality survived the round trip");
        assert_eq!(q.instance.as_deref(), Some("rec-north-01"));
        assert_eq!(q.voice_frames, 100);
        assert_eq!(q.bad_frames, 5);
        assert_eq!(q.rssi_dbfs, Some(-38.4));
    }

    #[test]
    fn the_broadcastify_verdict_survives_the_spool_so_a_retry_never_re_registers() {
        // The whole reason the verdict is persisted: the drain pass re-POSTs a
        // spooled call to Keyup, and the outcome that comes back with it must
        // be the one the live attempt produced — not a second registration
        // days after the call.
        let mut original = call(1_788_012_000_123);
        original.broadcastify = Some(trunkd_call::BroadcastifyOutcome {
            outcome: "uploaded".into(),
            code: Some(0),
            response: None,
            remote_id: Some("abc123".into()),
            holder: None,
        });
        let back = call_from_sidecar(&sidecar_json(&original)).expect("parses");
        assert_eq!(back.broadcastify, original.broadcastify);

        // And a call the feature never looked at comes back with nothing, as
        // does every sidecar written before the key existed.
        let plain = call(1_788_012_000_123);
        assert_eq!(
            call_from_sidecar(&sidecar_json(&plain))
                .expect("parses")
                .broadcastify,
            None
        );
    }

    #[test]
    fn a_sidecar_written_before_source_quality_existed_still_loads() {
        // An upgrade must not strand what the previous binary left on disk.
        let original = call(1_788_012_000_123);
        let mut v: Value = serde_json::from_str(&sidecar_json(&original)).expect("valid json");
        v.as_object_mut().expect("object").remove("sourceQuality");
        let back = call_from_sidecar(&v.to_string()).expect("an older sidecar still parses");
        assert!(back.source_quality.is_none());
        assert_eq!(back.talkgroup, original.talkgroup);
        assert_eq!(back.sources, original.sources);
    }

    #[test]
    fn a_sidecar_whose_quality_is_unreadable_still_yields_a_call() {
        // Every other field degrades to a default rather than failing the call;
        // this one must too.
        let original = call(1_788_012_000_123);
        let mut v: Value = serde_json::from_str(&sidecar_json(&original)).expect("valid json");
        v.as_object_mut()
            .expect("object")
            .insert("sourceQuality".into(), Value::String("nonsense".into()));
        let back = call_from_sidecar(&v.to_string()).expect("the call still parses");
        assert!(back.source_quality.is_none());
        assert_eq!(back.talkgroup, original.talkgroup);
    }

    #[test]
    fn rejects_text_that_is_not_a_json_object() {
        assert!(call_from_sidecar("not json").is_none());
        assert!(call_from_sidecar("[1,2,3]").is_none());
    }

    #[test]
    fn cap_evicts_oldest_first() {
        let dir = TempDir::new("cap");
        let audio = vec![0u8; 4096];
        // Cap fits roughly two entries.
        let spool = Spool::open(&dir.0, 10_000).expect("open");
        for i in 0..4 {
            let call = call(1_788_012_000_123 + i * 86_400_000);
            spool.store(&call, Some(&audio), "mp3").expect("store");
            // mtime granularity is coarse on some filesystems; entries() breaks
            // ties by path, and the paths here sort by date.
        }
        let sweep = spool.enforce_limits();
        assert!(sweep.cap_evicted >= 2, "evicted {}", sweep.cap_evicted);
        assert_eq!(sweep.floor_evicted, 0, "no floor is configured");
        assert!(sweep.bytes <= 10_000, "remaining {}", sweep.bytes);
        let left = spool.entries();
        assert!(!left.is_empty());
        // The survivors are the newest ones: the first call spooled is gone.
        assert!(left
            .iter()
            .all(|e| !e.json.to_string_lossy().contains("1788012000123")));
    }

    #[test]
    fn entries_are_oldest_first_and_remove_deletes_both_files() {
        let dir = TempDir::new("entries");
        let spool = Spool::open(&dir.0, 1 << 20).expect("open");
        spool
            .store(&call(1_788_012_000_123), Some(b"a"), "mp3")
            .expect("store");
        spool
            .store(&call(1_788_098_400_123), Some(b"b"), "mp3")
            .expect("store");
        let entries = spool.entries();
        assert_eq!(entries.len(), 2);
        assert!(entries[0].json.to_string_lossy().contains("1788012000123"));
        spool.remove(&entries[0]);
        assert!(!entries[0].json.exists());
        assert!(!entries[0].audio.as_ref().unwrap().exists());
        assert_eq!(spool.entries().len(), 1);
        spool.prune_empty_dirs();
        assert!(!dir.0.join("2026/08/29").exists());
        assert!(dir.0.exists());
    }

    /// Free-space sources for the floor tests. A `fn` pointer cannot close over
    /// a value, so each fake is its own function — which is also the clearest
    /// way to say what disk it is pretending to be.
    fn plenty_free(_: &Path) -> io::Result<u64> {
        Ok(100_000_000)
    }

    /// A volume 1 KiB short of a 5 KiB floor: one eviction should recover it.
    fn nearly_full(_: &Path) -> io::Result<u64> {
        Ok(4_096)
    }

    fn statvfs_broken(_: &Path) -> io::Result<u64> {
        Err(io::Error::new(io::ErrorKind::PermissionDenied, "no"))
    }

    /// Four calls of ~4 KiB each; the returned spool has a huge byte cap so
    /// only the floor can evict.
    fn spool_with_four_calls(dir: &Path, cap: u64) -> Spool {
        let spool = Spool::open(dir, cap).expect("open");
        let audio = vec![0u8; 4096];
        for i in 0..4 {
            let call = call(1_788_012_000_123 + i * 86_400_000);
            spool.store(&call, Some(&audio), "mp3").expect("store");
        }
        spool
    }

    #[test]
    fn the_floor_evicts_oldest_first_and_stops_once_free_space_recovers() {
        let dir = TempDir::new("floor");
        let spool = spool_with_four_calls(&dir.0, u64::MAX)
            .with_min_free_bytes(5_000)
            .with_free_space_source(nearly_full);
        let before = spool.entries();
        assert_eq!(before.len(), 4);

        let sweep = spool.enforce_limits();
        assert_eq!(sweep.cap_evicted, 0, "the cap is effectively infinite");
        // 4096 free + one ~4 KiB call deleted clears the 5000-byte floor.
        assert_eq!(sweep.floor_evicted, 1, "one eviction is enough");
        assert!(sweep.free_bytes.unwrap() >= 5_000);

        let left = spool.entries();
        assert_eq!(left.len(), 3);
        // The one that went is the oldest.
        assert!(!before[0].json.exists());
        assert_eq!(left[0].json, before[1].json);
    }

    #[test]
    fn the_floor_stops_at_an_empty_spool_rather_than_spinning() {
        let dir = TempDir::new("floor-empty");
        // A floor nothing we could delete would ever satisfy.
        let spool = spool_with_four_calls(&dir.0, u64::MAX)
            .with_min_free_bytes(u64::MAX)
            .with_free_space_source(nearly_full);
        let sweep = spool.enforce_limits();
        assert_eq!(sweep.floor_evicted, 4);
        assert_eq!(sweep.bytes, 0);
        assert!(spool.entries().is_empty());
    }

    #[test]
    fn a_free_space_reading_that_fails_evicts_nothing() {
        let dir = TempDir::new("floor-err");
        let spool = spool_with_four_calls(&dir.0, u64::MAX)
            .with_min_free_bytes(u64::MAX)
            .with_free_space_source(statvfs_broken);
        let sweep = spool.enforce_limits();
        assert_eq!(
            sweep.evicted(),
            0,
            "a bad reading is never a reason to delete"
        );
        assert_eq!(sweep.free_bytes, None);
        assert_eq!(spool.entries().len(), 4);
    }

    #[test]
    fn a_healthy_volume_leaves_the_spool_alone() {
        let dir = TempDir::new("floor-ok");
        let spool = spool_with_four_calls(&dir.0, u64::MAX)
            .with_min_free_bytes(5_000)
            .with_free_space_source(plenty_free);
        let sweep = spool.enforce_limits();
        assert_eq!(sweep.evicted(), 0);
        assert_eq!(sweep.free_bytes, Some(100_000_000));
        assert_eq!(spool.entries().len(), 4);
    }

    #[test]
    fn the_cap_runs_first_and_the_floor_picks_up_where_it_left_off() {
        let dir = TempDir::new("cap-and-floor");
        // ~4 KiB per call: a 10 KiB cap leaves two, and the floor then wants
        // one more (4096 free + ~4 KiB >= 8000).
        let spool = spool_with_four_calls(&dir.0, 10_000)
            .with_min_free_bytes(8_000)
            .with_free_space_source(nearly_full);
        let sweep = spool.enforce_limits();
        assert_eq!(sweep.cap_evicted, 2, "10 KiB cap over four ~4 KiB calls");
        assert_eq!(sweep.floor_evicted, 1);
        assert_eq!(spool.entries().len(), 1, "the newest call survives");
        assert!(sweep.bytes <= 10_000);
    }

    #[test]
    fn a_sweep_with_nothing_spooled_is_harmless() {
        let dir = TempDir::new("floor-idle");
        let spool = Spool::open(&dir.0, 1 << 20)
            .expect("open")
            .with_min_free_bytes(u64::MAX)
            .with_free_space_source(nearly_full);
        let sweep = spool.enforce_limits();
        assert_eq!(sweep.evicted(), 0);
        assert_eq!(sweep.bytes, 0);
        assert_eq!(sweep.free_bytes, Some(4_096));
    }
}
