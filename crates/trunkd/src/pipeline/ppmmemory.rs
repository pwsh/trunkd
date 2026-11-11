// SPDX-License-Identifier: GPL-3.0-or-later
//! What each board's oscillator turned out to be worth, kept across restarts.
//!
//! # The failure this exists for
//!
//! [`AutoPpm`](crate::pipeline::autoppm::AutoPpm) learns a board's frequency
//! error from the air, and then forgets it: the loop is built per stream, so a
//! restart — a config push, a USB glitch, a reboot — re-opens the board with
//! nothing but `[[device]] ppm` and spends the next ten or twenty seconds
//! hunting its way back to the number it already knew last week. On a tuner
//! whose TCXO is far enough out that the LSM demodulator never pulls in
//! unaided, those are seconds of a site not being recorded, every single
//! restart, forever.
//!
//! A crystal's error is a property of the board, not of the run. So it is
//! written down, keyed by the board, and applied at open.
//!
//! # Shape
//!
//! A single small JSON file for the whole recorder (`[recorder]
//! ppm_memory_path`), holding one entry per board keyed by
//! [`board_key`] — backend plus serial where the hardware offers one, so the
//! memory follows the board rather than the `[[device]]` id an operator may
//! rename or re-slot. Every write is temp-and-rename under a process-wide lock,
//! because two tuners on one recorder share the file and a half-written state
//! file is worse than none.
//!
//! # What the numbers mean
//!
//! [`PpmEntry::ppm`] is the **total** correction that was in force — the
//! `[[device]] ppm` of the moment plus whatever the loop had added on top — and
//! [`PpmEntry::config_ppm`] records what that `[[device]] ppm` was. Storing
//! both is what lets an operator change `ppm` later without the memory silently
//! double-counting or discarding their number: the remembered *delta* is
//! `ppm - config_ppm`, and it is re-based onto whatever `ppm` says today. See
//! [`open_ppm`].

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use crate::config::{Backend, Config};

/// Serialized shape version. Bumped only for a change a previous trunkd could
/// misread; unknown values are ignored the same way a corrupt file is.
const FORMAT_VERSION: u32 = 1;

/// One board's remembered frequency correction.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PpmEntry {
    /// Total applied correction in ppm, `[[device]] ppm` included.
    pub ppm: f64,
    /// When it was last measured, RFC 3339 UTC.
    pub measured_at: String,
    /// The `[[device]] ppm` in force when it was measured, so the remembered
    /// delta can be re-based if the operator changes it.
    #[serde(default)]
    pub config_ppm: f64,
    /// How many closed-loop corrections have gone into it. 0 means an operator
    /// (or keyup) set the number rather than the loop measuring it.
    #[serde(default)]
    pub samples: u64,
}

impl PpmEntry {
    /// The correction this entry contributes *on top of* the config ppm.
    pub fn delta(&self) -> f64 {
        self.ppm - self.config_ppm
    }

    /// `measured_at` as unix milliseconds, when it parses.
    pub fn measured_at_ms(&self) -> Option<i64> {
        parse_rfc3339_ms(&self.measured_at)
    }
}

/// The whole state file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PpmMemory {
    /// Format version; see [`FORMAT_VERSION`].
    #[serde(default)]
    pub version: u32,
    /// Board key -> entry.
    #[serde(default)]
    pub boards: BTreeMap<String, PpmEntry>,
}

impl Default for PpmMemory {
    fn default() -> Self {
        PpmMemory {
            version: FORMAT_VERSION,
            boards: BTreeMap::new(),
        }
    }
}

impl PpmMemory {
    /// This board's entry, if the file holds one.
    pub fn get(&self, key: &str) -> Option<&PpmEntry> {
        self.boards.get(key)
    }
}

/// Serializes the read-modify-write cycle across this recorder's tuner threads.
///
/// The file is one document for every board, so two tuners correcting in the
/// same second would otherwise each write a copy of the state they read before
/// the other's change — losing one of them. Uncontended in the normal case: a
/// correction is seconds apart at best.
static WRITE_LOCK: Mutex<()> = Mutex::new(());

/// Set the first time an unreadable file is skipped, so a state file that
/// cannot be parsed costs one warning rather than one per tuner per restart.
static WARNED: AtomicBool = AtomicBool::new(false);

/// Where this recorder keeps its ppm memory.
///
/// `[recorder] ppm_memory_path` wins when set. The default sits beside the
/// spool rather than inside it — `<spool_path>/../state/ppm-memory.json` — so
/// the spool stays a directory of nothing but undelivered calls (its eviction
/// sweep walks it, and a state file it does not recognize has no business
/// being in its way), while the memory still lands wherever the operator put
/// the recorder's runtime data. Relative paths resolve against the working
/// directory, which is where every other `[recorder]`/`[uplink]` path already
/// resolves, so a Windows service install with an absolute `spool_path` gets an
/// absolute state path by the same rule.
pub fn memory_path(cfg: &Config) -> PathBuf {
    let configured = cfg.recorder.ppm_memory_path.trim();
    if !configured.is_empty() {
        return PathBuf::from(configured);
    }
    let spool = Path::new(cfg.uplink.spool_path.trim());
    let base = spool.parent().unwrap_or_else(|| Path::new(""));
    base.join("state").join("ppm-memory.json")
}

/// The stable identity of the board behind a `[[device]]`.
///
/// Serial first, because that is what survives an operator renaming the device
/// id or moving it to another USB port — the two events most likely to happen
/// between the measurement and the restart that wants it. A board that reports
/// no serial falls back to the config id, which at least survives a reboot.
pub fn board_key(
    backend: Backend,
    driver: Option<&str>,
    serial: Option<&str>,
    device_id: &str,
) -> String {
    let serial = serial.map(str::trim).filter(|s| !s.is_empty());
    match (backend, serial) {
        // A Soapy serial is only unique within its driver: two vendors number
        // their boards independently, so the driver is part of the identity.
        (Backend::Soapy, Some(s)) => match driver.map(str::trim).filter(|d| !d.is_empty()) {
            Some(d) => format!("soapy:{d}:{s}"),
            None => format!("soapy:{s}"),
        },
        (Backend::Hackrf, Some(s)) => format!("hackrf:{s}"),
        (b, _) => format!("{}:{device_id}", backend_str(b)),
    }
}

fn backend_str(b: Backend) -> &'static str {
    match b {
        Backend::Hackrf => "hackrf",
        Backend::Soapy => "soapy",
        Backend::Iqfile => "iqfile",
    }
}

/// Read the state file. `Err` for anything that is not a readable, parseable
/// document of a version this trunkd understands; a *missing* file is `Ok` and
/// empty, which is what a first run legitimately looks like.
pub fn read(path: &Path) -> Result<PpmMemory, String> {
    let raw = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(PpmMemory::default()),
        Err(e) => return Err(format!("read {}: {e}", path.display())),
    };
    let mem: PpmMemory =
        serde_json::from_str(&raw).map_err(|e| format!("parse {}: {e}", path.display()))?;
    if mem.version > FORMAT_VERSION {
        return Err(format!(
            "{}: format version {} is newer than this trunkd understands ({FORMAT_VERSION})",
            path.display(),
            mem.version
        ));
    }
    Ok(mem)
}

/// Read the state file, treating an unreadable one as empty.
///
/// A corrupt state file must never stop a recorder from starting: the worst it
/// costs is one hunt per tuner, which is exactly the behavior of a recorder
/// that has never run before. Said once per process, because every tuner and
/// every restart would otherwise repeat it.
pub fn load(path: &Path) -> PpmMemory {
    match read(path) {
        Ok(m) => m,
        Err(e) => {
            if !WARNED.swap(true, Ordering::Relaxed) {
                tracing::warn!(error = %e, "ppm memory unreadable; starting from no memory");
            }
            PpmMemory::default()
        }
    }
}

/// Write the state file atomically: a full temp file, then one rename.
///
/// The directory is created on the way, because the default path names a
/// `state/` directory that nothing else makes.
fn store(path: &Path, mem: &PpmMemory) -> Result<(), String> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    }
    let body =
        serde_json::to_string_pretty(mem).map_err(|e| format!("serialize ppm memory: {e}"))? + "\n";
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, body.as_bytes()).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("rename {}: {e}", path.display()))
}

/// How a write should treat the sample count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Samples {
    /// One more closed-loop correction went into this number.
    Bump,
    /// Re-assert a number already counted (a clean-shutdown flush).
    Keep,
    /// An operator or keyup set the number outright; the measurement history
    /// behind it is gone.
    Reset,
}

/// Record `ppm` (a *total*, config ppm included) for `key`.
///
/// Re-reads the file under the lock rather than holding a cached copy: another
/// tuner on this recorder may have written its own board since we loaded, and
/// its entry must survive our write.
pub fn record(path: &Path, key: &str, ppm: f64, config_ppm: f64, samples: Samples) {
    if let Err(e) = try_record(path, key, ppm, config_ppm, samples) {
        tracing::warn!(error = %e, board = %key, "could not write ppm memory");
    }
}

/// The fallible half of [`record`], so tests can assert on the failure.
pub fn try_record(
    path: &Path,
    key: &str,
    ppm: f64,
    config_ppm: f64,
    samples: Samples,
) -> Result<(), String> {
    let _guard = WRITE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut mem = load(path);
    let previous = mem.boards.get(key).map(|e| e.samples).unwrap_or(0);
    let samples = match samples {
        Samples::Bump => previous.saturating_add(1),
        Samples::Keep => previous,
        Samples::Reset => 0,
    };
    mem.version = FORMAT_VERSION;
    mem.boards.insert(
        key.to_owned(),
        PpmEntry {
            ppm,
            measured_at: now_rfc3339(),
            config_ppm,
            samples,
        },
    );
    store(path, &mem)
}

/// What to open a board with, given what it remembers.
///
/// Returns `(ppm to hand the backend, the delta seeded into the loop, clamped)`.
/// The delta is the remembered correction re-based onto today's `config_ppm`,
/// so an operator who edits `[[device]] ppm` moves the whole result by exactly
/// what they typed instead of fighting a number measured against the old one.
///
/// Clamping to `limit_ppm` is not defensive tidiness: the loop's own limit
/// accounting starts from this delta, and a seed outside the limit would mean
/// the very first correction is refused for having nowhere to go.
pub fn open_ppm(entry: &PpmEntry, config_ppm: f64, limit_ppm: f64) -> (f64, f64, bool) {
    let wanted = entry.delta();
    if !wanted.is_finite() {
        return (config_ppm, 0.0, false);
    }
    let limit = limit_ppm.max(0.0);
    let delta = wanted.clamp(-limit, limit);
    (config_ppm + delta, delta, delta != wanted)
}

/// Unix milliseconds as an RFC 3339 UTC instant.
fn now_rfc3339() -> String {
    crate::pipeline::observations::rfc3339(trunkd_core::Timestamp::now().unix_millis())
}

/// An RFC 3339 instant as unix milliseconds.
pub fn parse_rfc3339_ms(s: &str) -> Option<i64> {
    OffsetDateTime::parse(s, &Rfc3339)
        .ok()
        .map(|t| (t.unix_timestamp_nanos() / 1_000_000) as i64)
}

/// A human age like `3d 4h` / `12m`, for the `trunkd check` table.
pub fn age_label(measured_at_ms: i64, now_ms: i64) -> String {
    let secs = now_ms.saturating_sub(measured_at_ms) / 1000;
    if secs < 0 {
        return "in the future".to_owned();
    }
    let (d, h, m) = (secs / 86_400, (secs % 86_400) / 3600, (secs % 3600) / 60);
    if d > 0 {
        format!("{d}d {h}h")
    } else if h > 0 {
        format!("{h}h {m}m")
    } else {
        format!("{m}m")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "trunkd-ppm-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn entry(ppm: f64, config_ppm: f64) -> PpmEntry {
        PpmEntry {
            ppm,
            measured_at: "2026-09-01T00:00:00Z".to_owned(),
            config_ppm,
            samples: 3,
        }
    }

    #[test]
    fn a_board_key_prefers_the_serial_and_falls_back_to_the_device_id() {
        assert_eq!(
            board_key(Backend::Soapy, Some("airspy"), Some("2608b1c4"), "d0"),
            "soapy:airspy:2608b1c4"
        );
        assert_eq!(
            board_key(Backend::Hackrf, None, Some("0000cafe"), "d1"),
            "hackrf:0000cafe"
        );
        // No serial: the config id is the only stable thing left.
        assert_eq!(
            board_key(Backend::Soapy, Some("rtlsdr"), None, "d2"),
            "soapy:d2"
        );
        // A blank serial is not a serial.
        assert_eq!(
            board_key(Backend::Hackrf, None, Some("  "), "d3"),
            "hackrf:d3"
        );
        assert_eq!(
            board_key(Backend::Iqfile, None, None, "file0"),
            "iqfile:file0"
        );
    }

    #[test]
    fn the_file_round_trips_and_every_write_is_atomic() {
        let dir = tmpdir("roundtrip");
        // The default path names a directory nothing else creates, so the
        // write has to make it.
        let path = dir.join("state").join("ppm-memory.json");
        assert_eq!(read(&path).expect("missing is empty").boards.len(), 0);

        try_record(&path, "hackrf:0000cafe", 1.25, 0.0, Samples::Bump).expect("write");
        let mem = read(&path).expect("read back");
        let e = mem.get("hackrf:0000cafe").expect("the board is remembered");
        assert_eq!(e.ppm, 1.25);
        assert_eq!(e.config_ppm, 0.0);
        assert_eq!(e.samples, 1, "the first correction counts as one sample");
        assert!(e.measured_at_ms().is_some(), "{e:?}");

        // A second board does not displace the first, and the sample count
        // accumulates per board.
        try_record(&path, "soapy:airspy:2608", -0.5, 0.0, Samples::Bump).expect("write");
        try_record(&path, "hackrf:0000cafe", 1.4, 0.0, Samples::Bump).expect("write");
        let mem = read(&path).expect("read back");
        assert_eq!(mem.boards.len(), 2);
        assert_eq!(mem.get("hackrf:0000cafe").expect("kept").samples, 2);
        assert_eq!(mem.get("soapy:airspy:2608").expect("kept").ppm, -0.5);

        // A clean-shutdown flush re-asserts without counting a measurement.
        try_record(&path, "hackrf:0000cafe", 1.4, 0.0, Samples::Keep).expect("write");
        assert_eq!(
            read(&path)
                .expect("read")
                .get("hackrf:0000cafe")
                .expect("kept")
                .samples,
            2
        );

        // Nothing is left behind: the temp file is renamed, never orphaned.
        let leftovers: Vec<_> = std::fs::read_dir(path.parent().expect("dir"))
            .expect("list")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            leftovers,
            vec!["ppm-memory.json".to_owned()],
            "{leftovers:?}"
        );
    }

    #[test]
    fn a_missing_or_corrupt_file_reads_as_no_memory() {
        let dir = tmpdir("corrupt");
        let missing = dir.join("nope.json");
        assert!(read(&missing).is_ok(), "a first run is not an error");
        assert_eq!(load(&missing).boards.len(), 0);

        let bad = dir.join("bad.json");
        std::fs::write(&bad, b"{not json at all").expect("write");
        assert!(read(&bad).is_err(), "a corrupt file is an error to `read`");
        // ...but never to the recorder: it starts with an empty memory.
        assert_eq!(load(&bad).boards.len(), 0);

        // A file from a future trunkd is refused rather than half-read.
        let future = dir.join("future.json");
        std::fs::write(&future, br#"{"version":99,"boards":{}}"#).expect("write");
        assert!(read(&future).is_err());
        assert_eq!(load(&future).boards.len(), 0);
    }

    #[test]
    fn the_remembered_delta_is_re_based_when_the_config_ppm_changes() {
        // Measured at +1.25 total with a config ppm of 0: the board's own
        // error is +1.25.
        let e = entry(1.25, 0.0);
        assert_eq!(open_ppm(&e, 0.0, 10.0), (1.25, 1.25, false));
        // The operator then types `ppm = 2.0`. Their number moves the result
        // by exactly 2, rather than being swallowed by the measurement.
        assert_eq!(open_ppm(&e, 2.0, 10.0), (3.25, 1.25, false));

        // Measured on top of a config ppm: only the loop's own contribution is
        // remembered as a delta.
        let e = entry(3.0, 2.0);
        assert_eq!(open_ppm(&e, 2.0, 10.0), (3.0, 1.0, false));
        assert_eq!(open_ppm(&e, 0.0, 10.0), (1.0, 1.0, false));
        assert_eq!(open_ppm(&e, -1.0, 10.0), (0.0, 1.0, false));
    }

    #[test]
    fn a_remembered_delta_past_the_limit_is_clamped() {
        let e = entry(4.0, 0.0);
        let (ppm, delta, clamped) = open_ppm(&e, 0.0, 1.5);
        assert_eq!((ppm, delta), (1.5, 1.5));
        assert!(
            clamped,
            "a seed the loop could never have reached is capped"
        );

        let e = entry(-4.0, 0.0);
        let (ppm, delta, clamped) = open_ppm(&e, 0.0, 1.5);
        assert_eq!((ppm, delta), (-1.5, -1.5));
        assert!(clamped);

        // A limit of zero means the loop cannot move at all, so neither can
        // its memory — but the operator's own `ppm` still applies.
        assert_eq!(open_ppm(&entry(4.0, 2.0), 2.0, 0.0), (2.0, 0.0, true));

        // Garbage in the file is not applied.
        let mut junk = entry(0.0, 0.0);
        junk.ppm = f64::NAN;
        assert_eq!(open_ppm(&junk, 1.0, 5.0), (1.0, 0.0, false));
    }

    #[test]
    fn an_operator_push_resets_the_measurement_history() {
        let dir = tmpdir("reset");
        let path = dir.join("ppm-memory.json");
        try_record(&path, "hackrf:0000cafe", 1.25, 0.0, Samples::Bump).expect("write");
        try_record(&path, "hackrf:0000cafe", 1.30, 0.0, Samples::Bump).expect("write");
        assert_eq!(
            read(&path)
                .expect("read")
                .get("hackrf:0000cafe")
                .expect("kept")
                .samples,
            2
        );

        try_record(&path, "hackrf:0000cafe", -3.0, -3.0, Samples::Reset).expect("write");
        let e = read(&path)
            .expect("read")
            .get("hackrf:0000cafe")
            .cloned()
            .expect("kept");
        assert_eq!(e.ppm, -3.0);
        assert_eq!(e.config_ppm, -3.0);
        assert_eq!(e.samples, 0, "0 samples marks a number nobody measured");
        // An operator's number contributes no delta of its own: it *is* the
        // config ppm now.
        assert_eq!(open_ppm(&e, -3.0, 10.0), (-3.0, 0.0, false));
    }

    #[test]
    fn the_default_path_sits_beside_the_spool_not_inside_it() {
        let mut cfg = Config::default();
        cfg.uplink.spool_path = "spool".to_owned();
        assert_eq!(memory_path(&cfg), PathBuf::from("state/ppm-memory.json"));
        cfg.uplink.spool_path = "/var/lib/trunkd/spool".to_owned();
        assert_eq!(
            memory_path(&cfg),
            PathBuf::from("/var/lib/trunkd/state/ppm-memory.json")
        );
        // An explicit path wins outright.
        cfg.recorder.ppm_memory_path = "/etc/trunkd/ppm.json".to_owned();
        assert_eq!(memory_path(&cfg), PathBuf::from("/etc/trunkd/ppm.json"));
    }

    #[test]
    fn ages_read_the_way_an_operator_would_say_them() {
        let ms = |s: i64| s * 1000;
        assert_eq!(age_label(0, ms(90)), "1m");
        assert_eq!(age_label(0, ms(3 * 3600 + 25 * 60)), "3h 25m");
        assert_eq!(age_label(0, ms(3 * 86_400 + 4 * 3600)), "3d 4h");
    }

    #[test]
    fn timestamps_round_trip_through_the_file() {
        assert_eq!(
            parse_rfc3339_ms("2023-11-14T22:13:20.123Z"),
            Some(1_700_000_000_123)
        );
        assert_eq!(parse_rfc3339_ms("not a time"), None);
        let now = trunkd_core::Timestamp::now().unix_millis();
        let parsed = parse_rfc3339_ms(&now_rfc3339()).expect("our own stamp parses");
        assert!((parsed - now).abs() < 5_000, "{parsed} vs {now}");
    }
}
