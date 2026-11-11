// SPDX-License-Identifier: GPL-3.0-or-later
//! The raw non-voice event log: one JSON object per line, one file per UTC day.
//!
//! ```text
//! <event_log_dir>/2026-08-31.jsonl      today, appended to
//! <event_log_dir>/2026-08-30.jsonl.gz   yesterday, compressed at rotation
//! ```
//!
//! `docs/NON-VOICE-DATA.md` §7 is explicit that 150 s of capture cannot say how
//! often the rare messages appear — status updates, call alerts, emergency
//! responses and radio-unit-monitor commands appeared **zero times** — and that
//! a day-long log is what would settle it. This is that log. It is deliberately
//! *not* the uplink: nothing here leaves the box, and what it costs is disk.
//!
//! # Cost
//!
//! ~2.8 M lines/day for one busy site (2026-09-21), 515 MB as whole records
//! and 228 MB with repeats coalesced (below). The previous day is gzipped on
//! rotation (to ~18 MB), and files older than `event_log_keep_days` are
//! deleted. A recorder left alone therefore costs a bounded, predictable
//! amount of disk.
//!
//! # Rotation
//!
//! The log is on one day at a time, and that day **only ever moves forwards**.
//! Each line's own wall-clock timestamp (every line carries it as `at`) can
//! advance the day — which is what makes a replayed capture land in the day it
//! was recorded — but never move it back: lines from several tuners, each on
//! its own stream clock, arrive a little out of order around midnight, and a
//! late line for a day already closed goes into the current day's file rather
//! than reopening (and later re-compressing over) the closed one. A timestamp
//! further ahead of this machine's clock than [`FUTURE_SLACK_MS`] cannot
//! advance the day either, so one garbled stamp cannot jump the log into next
//! week. So each day's file is closed exactly once, and a restart picks the
//! day up from the newest file already on disk.
//!
//! A closed day is compressed by streaming (bounded memory — a live day is
//! over half a gigabyte) into a temp file that is renamed into place, and a
//! `.gz` that already exists is **never** replaced: a second file for the same
//! day is named `YYYY-MM-DD.N.jsonl.gz` (the archiver uploads a day's parts
//! together, as one concatenated gzip). Plain files a previous run left behind
//! (a restart across midnight, or a recorder that has written nothing since)
//! are compressed at start-up and by [`EventLog::tick`].
//!
//! # Repeats
//!
//! Most of a day is the site describing itself over and over: the same RFSS,
//! network, adjacent-site, band-plan and service broadcasts, the two L3Harris
//! beacons and the regroup broadcast, each byte-identical to the copy a second
//! or two earlier (2026-09-21: 75 % of 2.8 M lines, 56 % of 515 MB). A
//! *constant broadcast* ([`crate::pipeline::nonvoice::is_constant_broadcast`],
//! an allow-list) whose content — everything but `at` — matches one written in
//! full within the last [`REPEAT_WINDOW_MS`] is written as a back-reference:
//!
//! ```text
//! {"at":"2026-09-21T00:00:00.504Z","id":3,"ch":"control",…,"type":"tsbk"}
//! {"at":"2026-09-21T00:00:01.991Z","rep":3}
//! ```
//!
//! The full copy carries an `id` straight after `at`; a repeat is exactly
//! `{"at","rep"}` and means the nearest preceding line with that id. Every
//! occurrence keeps its own line, its own `at` and its place in the file, so
//! nothing is lost: taking the `id` out and replacing each repeat's body gives
//! back, byte for byte, what the log wrote before repeats existed
//! (`harness/eventlog_repeats.py`, which the offline analyses read through).
//! The table of what was written resets with each file, so every file — and
//! every run appended to one — stands alone, and a reader starting anywhere
//! meets a full copy of each broadcast at least once a window. Grants, grant
//! updates, affiliations, registrations, data and link control are never
//! coalesced.
//!
//! # Where it runs
//!
//! On the observations thread, never the DSP thread: the events arrive over the
//! same bounded queue the site inventory reads, so a slow disk costs dropped
//! events (counted, and shown in the status line) and never a stalled decoder.
//!
//! **IMBE voice frames are never written**, and cannot be: the engine's
//! `SiteEvent::Traffic` pass-through drops them at the source.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::Value;
use time::{Date, OffsetDateTime};

/// How far ahead of this machine's clock a line's timestamp may be and still
/// move the log onto a new day. Stream clocks are resynced against the wall
/// clock and sit well inside this; a stamp beyond it is a fault, and its line
/// is written to the current day's file instead.
const FUTURE_SLACK_MS: i64 = 10 * 60 * 1000;

/// How often [`EventLog::tick`] looks for plain day files left uncompressed.
const SWEEP_INTERVAL: Duration = Duration::from_secs(60);

/// How far apart (by their own `at`) a repeat and the full copy it refers to
/// may be. 60 s is the knee on a real day (2026-09-21, 515 MB): 10 s leaves
/// 252 MB, 60 s 228 MB, an hour 222 MB — and a minute keeps a full copy of
/// every broadcast within a minute of anywhere a reader starts.
const REPEAT_WINDOW_MS: i64 = 60_000;

/// Most distinct broadcasts remembered at once. A site has a few dozen, plus
/// one per regroup patch state; past this the stale ones are forgotten (and
/// all of them, should that not be enough), which only costs full copies.
const REPEAT_TABLE_CAP: usize = 4096;

/// Read/write buffer for streaming compression: memory use is this, plus the
/// encoder's fixed window, whatever the size of the day.
const COMPRESS_BUF: usize = 64 * 1024;

/// Appends event-log lines, rotating when the day moves forwards.
#[derive(Debug)]
pub(crate) struct EventLog {
    dir: PathBuf,
    keep_days: u16,
    /// The day the log is on: the newest it has seen, never moving backwards.
    day: Option<Date>,
    /// The open day's file, and the day it is for.
    open: Option<(Date, BufWriter<File>)>,
    /// Lines written since start, for the status line.
    written: u64,
    /// Lines whose own day was already closed, written to the current file.
    late: u64,
    /// The constant broadcasts written in full to the open file.
    repeats: Repeats,
    /// Lines written as a back-reference to a full copy, since start.
    repeated: u64,
    /// When [`EventLog::tick`] last swept for stale plain files.
    last_sweep: Instant,
}

impl EventLog {
    /// Open (creating if needed) a log directory keeping `keep_days` of files.
    ///
    /// `keep_days` of 0 disables pruning.
    ///
    /// The log resumes on the newest day already on disk (so a restart cannot
    /// move it backwards), removes temp files an interrupted compression left,
    /// and compresses every plain day file older than that one.
    pub(crate) fn open(dir: impl Into<PathBuf>, keep_days: u16) -> io::Result<Self> {
        let dir = dir.into();
        fs::create_dir_all(&dir)?;
        let limit = utc_date(now_ms().saturating_add(FUTURE_SLACK_MS));
        let mut newest: Option<Date> = None;
        for entry in fs::read_dir(&dir)?.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if name.ends_with(".jsonl.gz.tmp") {
                let _ = fs::remove_file(&path);
                continue;
            }
            if let Some((day, _)) = parse_log_name(name) {
                if day <= limit && newest.is_none_or(|n| day > n) {
                    newest = Some(day);
                }
            }
        }
        let log = EventLog {
            dir,
            keep_days,
            day: newest,
            open: None,
            written: 0,
            late: 0,
            repeats: Repeats::default(),
            repeated: 0,
            last_sweep: Instant::now(),
        };
        log.compress_stale(newest);
        Ok(log)
    }

    /// Lines written so far.
    pub(crate) fn written(&self) -> u64 {
        self.written
    }

    /// Lines that arrived after their own day was closed and were written to
    /// the current day's file instead.
    #[cfg(test)]
    pub(crate) fn late(&self) -> u64 {
        self.late
    }

    /// Lines written as a back-reference to a full copy.
    #[cfg(test)]
    pub(crate) fn repeated(&self) -> u64 {
        self.repeated
    }

    /// Append one line as it is, rotating first if the day has moved forwards
    /// (the day rules of [`EventLog::write_record`]).
    #[cfg(test)]
    pub(crate) fn write(&mut self, unix_ms: i64, line: &str) -> io::Result<()> {
        self.select_day(unix_ms)?;
        self.append(line)
    }

    /// Append one event record (a JSON object whose first key is `at`),
    /// rotating first if the day has moved forwards.
    ///
    /// `unix_ms` dates the line; it is the event's own wall clock, so an event
    /// replayed from a capture lands in the day it was recorded. It can only
    /// move the log's day forwards (see the module docs): a line for a day
    /// already closed is written to the current file, where its own `at` still
    /// says when it happened.
    ///
    /// A `repeatable` record — a constant broadcast — whose content matches
    /// one this file holds in full from within [`REPEAT_WINDOW_MS`] is written
    /// as a back-reference instead (see the module docs); written in full, it
    /// carries the `id` later repeats name. Anything else is written as is.
    pub(crate) fn write_record(
        &mut self,
        unix_ms: i64,
        mut record: Value,
        repeatable: bool,
    ) -> io::Result<()> {
        self.select_day(unix_ms)?;
        let at = match record.get("at") {
            Some(Value::String(at)) if repeatable => at.clone(),
            _ => return self.append(&record.to_string()),
        };
        if let Some(fields) = record.as_object_mut() {
            fields.remove("at");
        }
        // `{…}` without `at`: the key, and the full line's tail. The plain
        // line is `{"at":…,` and this tail, since `at` sorts (and is built)
        // first — which is what makes a repeat's expansion byte-exact.
        let content = record.to_string();
        let at = Value::String(at).to_string();
        let line = match self.repeats.note(&content, unix_ms) {
            (Form::Repeat, id) => {
                self.repeated += 1;
                format!(r#"{{"at":{at},"rep":{id}}}"#)
            }
            (Form::Full, id) => match content.strip_prefix('{') {
                Some(rest) if rest != "}" => format!(r#"{{"at":{at},"id":{id},{rest}"#),
                _ => format!(r#"{{"at":{at},"id":{id}}}"#),
            },
        };
        self.append(&line)
    }

    /// Settle which day `unix_ms` goes into and have that day's file open.
    fn select_day(&mut self, unix_ms: i64) -> io::Result<()> {
        let limit = utc_date(now_ms().saturating_add(FUTURE_SLACK_MS));
        let own = utc_date(unix_ms).min(limit);
        let day = match self.day {
            Some(current) if own < current => {
                self.late += 1;
                current
            }
            _ => own,
        };
        self.day = Some(day);
        match &self.open {
            Some((open_day, _)) if *open_day == day => Ok(()),
            _ => self.rotate_to(day),
        }
    }

    /// Write one line to the open file.
    fn append(&mut self, line: &str) -> io::Result<()> {
        let Some((_, w)) = self.open.as_mut() else {
            return Ok(());
        };
        w.write_all(line.as_bytes())?;
        w.write_all(b"\n")?;
        self.written += 1;
        Ok(())
    }

    /// Periodic housekeeping, cheap to call often: at most once a
    /// [`SWEEP_INTERVAL`] it compresses every plain day file older than this
    /// machine's UTC date other than the one open, and prunes.
    ///
    /// This is what finishes a day nobody wrote to after midnight — a recorder
    /// restarted in the afternoon whose only tuner keyup then disabled would
    /// otherwise leave that day plain, and unarchived, for good.
    pub(crate) fn tick(&mut self) {
        if self.last_sweep.elapsed() < SWEEP_INTERVAL {
            return;
        }
        self.last_sweep = Instant::now();
        let open = self.open.as_ref().map(|(d, _)| *d);
        self.compress_stale(open);
        self.prune();
    }

    /// Compress every plain `YYYY-MM-DD.jsonl` older than this machine's UTC
    /// date, except `keep` (the day the log is on or has open).
    fn compress_stale(&self, keep: Option<Date>) {
        let today = utc_date(now_ms());
        let Ok(rd) = fs::read_dir(&self.dir) else {
            return;
        };
        for entry in rd.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let Some((day, LogKind::Plain)) = parse_log_name(name) else {
                continue;
            };
            if day < today && Some(day) != keep {
                if let Err(e) = compress_day(&path) {
                    tracing::warn!(file = %path.display(), error = %e, "could not compress a stale event log");
                }
            }
        }
    }

    /// Flush the open file, so a log tailed live is current.
    pub(crate) fn flush(&mut self) {
        if let Some((_, w)) = self.open.as_mut() {
            let _ = w.flush();
        }
    }

    /// Close the current day and open `day`, compressing what was open and
    /// pruning anything past the retention window.
    fn rotate_to(&mut self, day: Date) -> io::Result<()> {
        if let Some((old_day, mut w)) = self.open.take() {
            w.flush()?;
            drop(w);
            // Best effort: a log that could not be compressed is still a log,
            // and failing the rotation would lose the new day as well.
            if let Err(e) = compress_day(&self.path_for(old_day)) {
                tracing::warn!(day = %day_name(old_day), error = %e, "could not compress the event log");
            }
            if self.late > 0 {
                tracing::info!(
                    closed = %day_name(old_day),
                    late_lines_since_start = self.late,
                    "event log: lines for already-closed days went into the next day's file"
                );
            }
            self.prune();
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.path_for(day))?;
        self.open = Some((day, BufWriter::new(file)));
        // A repeat may only name a full copy in the same file (and the same
        // run of it): a reader of one file must never need another.
        self.repeats = Repeats::default();
        Ok(())
    }

    fn path_for(&self, day: Date) -> PathBuf {
        self.dir.join(format!("{}.jsonl", day_name(day)))
    }

    /// Delete log files older than the retention window.
    ///
    /// Age is read from the *file name*, not its mtime: a name is the day the
    /// events happened, which is what retention is about, and it survives a
    /// copy that rewrites timestamps.
    pub(crate) fn prune(&self) {
        if self.keep_days == 0 {
            return;
        }
        let today = OffsetDateTime::now_utc().date();
        let Ok(rd) = fs::read_dir(&self.dir) else {
            return;
        };
        for entry in rd.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let Some((date, _)) = parse_log_name(name) else {
                continue;
            };
            if (today - date).whole_days() > i64::from(self.keep_days) {
                let _ = fs::remove_file(&path);
            }
        }
    }
}

impl Drop for EventLog {
    fn drop(&mut self) {
        self.flush();
    }
}

/// Whether a constant broadcast goes out in full or as a back-reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Form {
    Full,
    Repeat,
}

/// The constant broadcasts the open file holds in full: content (the record
/// without `at`, serialized) to the id it was written with and the `at` of
/// that full copy.
#[derive(Debug, Default)]
struct Repeats {
    seen: HashMap<String, (u32, i64)>,
    next_id: u32,
}

impl Repeats {
    /// Note `content` heard at `at_ms`: a repeat of the full copy with that
    /// id if one is within the window, otherwise a full copy — under the id
    /// that content had before if it had one (ids stay small), else a new one.
    fn note(&mut self, content: &str, at_ms: i64) -> (Form, u32) {
        match self.seen.get_mut(content) {
            Some((id, full_at)) if (at_ms - *full_at).abs() <= REPEAT_WINDOW_MS => {
                return (Form::Repeat, *id);
            }
            Some((id, full_at)) => {
                *full_at = at_ms;
                return (Form::Full, *id);
            }
            None => {}
        }
        if self.seen.len() >= REPEAT_TABLE_CAP {
            self.seen
                .retain(|_, (_, t)| (at_ms - *t).abs() <= REPEAT_WINDOW_MS);
            if self.seen.len() >= REPEAT_TABLE_CAP {
                self.seen.clear();
            }
        }
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        self.seen.insert(content.to_owned(), (id, at_ms));
        (Form::Full, id)
    }
}

/// Which of the two day-file shapes a name is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LogKind {
    /// `YYYY-MM-DD.jsonl`, a day being (or last) written.
    Plain,
    /// `YYYY-MM-DD.jsonl.gz` or `YYYY-MM-DD.N.jsonl.gz`, a finished day.
    Compressed,
}

/// The day and shape of an event-log file name; `None` for anything else
/// (markers, temp files, notes an operator left in the directory).
fn parse_log_name(name: &str) -> Option<(Date, LogKind)> {
    if let Some(stem) = name.strip_suffix(".jsonl.gz") {
        return Some((parse_day(day_stem(stem)?)?, LogKind::Compressed));
    }
    let stem = name.strip_suffix(".jsonl")?;
    Some((parse_day(stem)?, LogKind::Plain))
}

/// The `YYYY-MM-DD` of a compressed file's stem, which is either the day
/// itself or the day plus a `.N` collision suffix.
pub(crate) fn day_stem(stem: &str) -> Option<&str> {
    match stem.split_once('.') {
        None => Some(stem),
        Some((day, n)) if !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) => Some(day),
        Some(_) => None,
    }
}

/// This machine's clock, unix milliseconds.
fn now_ms() -> i64 {
    OffsetDateTime::now_utc().unix_timestamp() * 1000
}

/// The UTC date of a unix-millisecond instant.
fn utc_date(unix_ms: i64) -> Date {
    let secs = unix_ms.div_euclid(1000);
    OffsetDateTime::from_unix_timestamp(secs)
        .unwrap_or(OffsetDateTime::UNIX_EPOCH)
        .date()
}

/// `YYYY-MM-DD` for a date.
fn day_name(d: Date) -> String {
    format!("{:04}-{:02}-{:02}", d.year(), u8::from(d.month()), d.day())
}

/// Parse a `YYYY-MM-DD` file stem back into a date.
fn parse_day(s: &str) -> Option<time::Date> {
    let mut parts = s.split('-');
    let y: i32 = parts.next()?.parse().ok()?;
    let m: u8 = parts.next()?.parse().ok()?;
    let d: u8 = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    time::Date::from_calendar_date(y, time::Month::try_from(m).ok()?, d).ok()
}

/// Compress a plain day file `YYYY-MM-DD.jsonl` and remove the original,
/// returning the compressed file's path (`None` when there was nothing to do).
///
/// The target is `YYYY-MM-DD.jsonl.gz`, or — when that already exists, because
/// a previous run or an earlier rotation finished this day once already —
/// the first free `YYYY-MM-DD.N.jsonl.gz`. An existing `.gz` is never
/// replaced: it may be the whole day, and it may already be archived.
///
/// Streams through a fixed buffer ([`gzip_stream`]) into a temp file that is
/// synced and renamed into place, so memory stays flat whatever the day's size
/// and an interrupted compression leaves the plain file, not a truncated `.gz`.
/// A missing file is a no-op, so a restart that rotates twice cannot fail.
fn compress_day(path: &Path) -> io::Result<Option<PathBuf>> {
    let Some(stem) = path
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.strip_suffix(".jsonl"))
        .map(str::to_owned)
    else {
        return Ok(None);
    };
    let input = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let dir = path.parent().unwrap_or(Path::new("."));
    let gz_path = free_gz_path(dir, &stem);
    let tmp = {
        let mut name = gz_path.as_os_str().to_owned();
        name.push(".tmp");
        PathBuf::from(name)
    };
    let result = (|| {
        let out = File::create(&tmp)?;
        let out = gzip_stream(BufReader::with_capacity(COMPRESS_BUF, input), out)?;
        out.sync_all()?;
        drop(out);
        // Checked again at the last moment: nothing else in this process
        // writes these names, but `rename` would replace one silently.
        if gz_path.exists() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{} appeared while compressing", gz_path.display()),
            ));
        }
        fs::rename(&tmp, &gz_path)
    })();
    if let Err(e) = result {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    fs::remove_file(path)?;
    Ok(Some(gz_path))
}

/// The first of `stem.jsonl.gz`, `stem.1.jsonl.gz`, `stem.2.jsonl.gz`, … that
/// does not exist yet.
fn free_gz_path(dir: &Path, stem: &str) -> PathBuf {
    let first = dir.join(format!("{stem}.jsonl.gz"));
    if !first.exists() {
        return first;
    }
    (1u32..)
        .map(|n| dir.join(format!("{stem}.{n}.jsonl.gz")))
        .find(|p| !p.exists())
        .expect("an unbounded range always has a free name")
}

/// Gzip everything `input` yields into `output`, a buffer at a time, and hand
/// `output` back once the gzip trailer is written.
///
/// Memory is [`COMPRESS_BUF`] plus the encoder's fixed state however long the
/// input is — the property that matters for a day of event log, which used to
/// be read whole and compressed into a second in-memory copy.
fn gzip_stream<R: Read, W: Write>(mut input: R, output: W) -> io::Result<W> {
    let mut encoder = flate2::write::GzEncoder::new(
        BufWriter::with_capacity(COMPRESS_BUF, output),
        flate2::Compression::default(),
    );
    let mut buf = vec![0u8; COMPRESS_BUF];
    loop {
        let n = match input.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        encoder.write_all(&buf[..n])?;
    }
    encoder
        .finish()?
        .into_inner()
        .map_err(io::IntoInnerError::into_error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let mut p = std::env::temp_dir();
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
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

    /// 2026-08-31T12:00:00Z.
    const AUG31: i64 = 1_788_177_600_000;
    const DAY_MS: i64 = 86_400_000;

    #[test]
    fn lines_land_in_the_file_for_their_utc_day() {
        let dir = TempDir::new("eventlog");
        let mut log = EventLog::open(&dir.0, 14).expect("open");
        log.write(AUG31, r#"{"a":1}"#).expect("write");
        log.write(AUG31, r#"{"a":2}"#).expect("write");
        log.flush();

        let body = fs::read_to_string(dir.0.join("2026-08-31.jsonl")).expect("read");
        assert_eq!(body, "{\"a\":1}\n{\"a\":2}\n");
        assert_eq!(log.written(), 2);
    }

    /// Retention is off here: the fixed 2026-08-31 day would otherwise be
    /// pruned as expired once the real clock is more than `keep_days` past it.
    #[test]
    fn crossing_utc_midnight_rotates_and_gzips_the_previous_day() {
        let dir = TempDir::new("eventlog-rotate");
        let mut log = EventLog::open(&dir.0, 0).expect("open");
        log.write(AUG31, r#"{"day":"aug31"}"#).expect("write");
        log.write(AUG31 + DAY_MS, r#"{"day":"sep1"}"#)
            .expect("write");
        log.flush();

        // Yesterday is gone as plain text and present as gzip.
        assert!(!dir.0.join("2026-08-31.jsonl").exists());
        let gz = dir.0.join("2026-08-31.jsonl.gz");
        assert!(gz.exists(), "the rotated day is compressed");
        let mut out = String::new();
        flate2::read::GzDecoder::new(File::open(&gz).expect("open gz"))
            .read_to_string(&mut out)
            .expect("decompress");
        assert_eq!(out, "{\"day\":\"aug31\"}\n");

        // Today is open and plain.
        assert_eq!(
            fs::read_to_string(dir.0.join("2026-09-01.jsonl")).expect("read"),
            "{\"day\":\"sep1\"}\n"
        );
        // No temp file survives.
        assert!(!dir.0.join("2026-08-31.jsonl.gz.tmp").exists());
    }

    #[test]
    fn reopening_appends_rather_than_truncating() {
        let dir = TempDir::new("eventlog-append");
        {
            let mut log = EventLog::open(&dir.0, 14).expect("open");
            log.write(AUG31, "first").expect("write");
        }
        {
            let mut log = EventLog::open(&dir.0, 14).expect("reopen");
            log.write(AUG31, "second").expect("write");
        }
        assert_eq!(
            fs::read_to_string(dir.0.join("2026-08-31.jsonl")).expect("read"),
            "first\nsecond\n"
        );
    }

    #[test]
    fn pruning_deletes_by_the_day_in_the_name() {
        let dir = TempDir::new("eventlog-prune");
        let today = OffsetDateTime::now_utc().date();
        let old = today - time::Duration::days(30);
        let recent = today - time::Duration::days(2);
        let name = |d: time::Date, ext: &str| {
            format!(
                "{:04}-{:02}-{:02}.{ext}",
                d.year(),
                u8::from(d.month()),
                d.day()
            )
        };
        for (d, ext) in [
            (old, "jsonl"),
            (old, "jsonl.gz"),
            (recent, "jsonl"),
            (today, "jsonl"),
        ] {
            fs::write(dir.0.join(name(d, ext)), b"x").expect("write");
        }
        // A file that is not a day log must survive whatever it is called.
        fs::write(dir.0.join("notes.txt"), b"x").expect("write");

        let log = EventLog::open(&dir.0, 14).expect("open");
        log.prune();

        assert!(!dir.0.join(name(old, "jsonl")).exists());
        assert!(!dir.0.join(name(old, "jsonl.gz")).exists());
        // Opening compressed the stale plain days (the old one under a `.1`
        // name beside its existing `.gz`); pruning then dropped the old ones.
        assert!(!dir.0.join(name(old, "1.jsonl.gz")).exists());
        assert!(dir.0.join(name(recent, "jsonl.gz")).exists());
        assert!(dir.0.join(name(today, "jsonl")).exists());
        assert!(dir.0.join("notes.txt").exists());
    }

    /// Every file in `dir`, sorted, for asserting on exactly what exists.
    fn listing(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .expect("list")
            .flatten()
            .filter_map(|e| e.file_name().to_str().map(str::to_owned))
            .collect();
        names.sort();
        names
    }

    fn gunzip(path: &Path) -> String {
        let mut out = String::new();
        flate2::read::GzDecoder::new(File::open(path).expect("open gz"))
            .read_to_string(&mut out)
            .expect("decompress");
        out
    }

    /// Two tuners on independent stream clocks interleave around midnight.
    /// The old log rotated on every line's own day, so D+1, D, D+1 closed
    /// D+1 early and then gzipped a few-line D over the whole day's `.gz`.
    /// Now the day only moves forwards: D closes once, late D lines go into
    /// D+1's file (their `at` still says when they happened), nothing is lost.
    #[test]
    fn out_of_order_lines_around_midnight_close_each_day_once_and_lose_nothing() {
        let dir = TempDir::new("eventlog-ooo");
        let mut log = EventLog::open(&dir.0, 0).expect("open");
        // 2026-08-31T23:59:59Z and 2026-09-01T00:00:01Z.
        let before = AUG31 + DAY_MS / 2 - 1_000;
        let after = AUG31 + DAY_MS / 2 + 1_000;
        let lines = [
            (before - 5_000, "d1"),
            (before, "d2"),
            (after, "n1"),
            (before + 500, "late1"),
            (after + 1_000, "n2"),
            (before + 800, "late2"),
            (after + 2_000, "n3"),
        ];
        for (at, line) in lines {
            log.write(at, line).expect("write");
        }
        log.flush();

        assert_eq!(
            listing(&dir.0),
            vec!["2026-08-31.jsonl.gz", "2026-09-01.jsonl"],
            "exactly one file per day, and no second file for the closed day"
        );
        assert_eq!(gunzip(&dir.0.join("2026-08-31.jsonl.gz")), "d1\nd2\n");
        assert_eq!(
            fs::read_to_string(dir.0.join("2026-09-01.jsonl")).expect("read"),
            "n1\nlate1\nn2\nlate2\nn3\n"
        );
        assert_eq!(log.written(), lines.len() as u64, "every line was written");
        assert_eq!(log.late(), 2);
    }

    /// A finished day's `.gz` is never replaced — not by a rotation, not by a
    /// restart that finds the day's plain file again.
    #[test]
    fn an_existing_gz_is_never_overwritten() {
        let dir = TempDir::new("eventlog-noclobber");
        let original = dir.0.join("2026-08-31.jsonl.gz");
        {
            let mut enc = flate2::write::GzEncoder::new(
                File::create(&original).expect("create"),
                flate2::Compression::default(),
            );
            enc.write_all(b"the whole day\n").expect("write");
            enc.finish().expect("finish");
        }
        let before = fs::read(&original).expect("read");

        let mut log = EventLog::open(&dir.0, 0).expect("open");
        log.write(AUG31, "more of that day").expect("write");
        log.write(AUG31 + DAY_MS, "next day").expect("write");
        log.flush();

        assert_eq!(fs::read(&original).expect("read"), before, "untouched");
        assert_eq!(
            gunzip(&dir.0.join("2026-08-31.1.jsonl.gz")),
            "more of that day\n"
        );
        // And a third time, from the start-up sweep of a stale plain file.
        fs::write(dir.0.join("2026-08-31.jsonl"), b"stray\n").expect("write");
        drop(log);
        let _ = EventLog::open(&dir.0, 0).expect("reopen");
        assert_eq!(
            fs::read(&original).expect("read"),
            before,
            "still untouched"
        );
        assert_eq!(gunzip(&dir.0.join("2026-08-31.2.jsonl.gz")), "stray\n");
        assert!(!dir.0.join("2026-08-31.jsonl").exists());
    }

    /// A restart resumes on the newest day on disk: a late line for an older
    /// day cannot reopen that day's file.
    #[test]
    fn a_restart_does_not_move_the_day_backwards() {
        let dir = TempDir::new("eventlog-restart");
        {
            let mut log = EventLog::open(&dir.0, 0).expect("open");
            log.write(AUG31, "aug31").expect("write");
            log.write(AUG31 + DAY_MS, "sep1").expect("write");
        }
        let mut log = EventLog::open(&dir.0, 0).expect("reopen");
        log.write(AUG31 + 1, "late aug31").expect("write");
        log.flush();
        assert_eq!(
            listing(&dir.0),
            vec!["2026-08-31.jsonl.gz", "2026-09-01.jsonl"]
        );
        assert_eq!(
            fs::read_to_string(dir.0.join("2026-09-01.jsonl")).expect("read"),
            "sep1\nlate aug31\n"
        );
    }

    /// A stamp far ahead of this machine's clock is a fault, not tomorrow: it
    /// is written, but cannot move the log onto a future day.
    #[test]
    fn a_far_future_stamp_cannot_advance_the_day() {
        let dir = TempDir::new("eventlog-future");
        let mut log = EventLog::open(&dir.0, 0).expect("open");
        let now = now_ms();
        log.write(now, "now").expect("write");
        log.write(now + 3 * DAY_MS, "garbled").expect("write");
        log.flush();
        let future = format!("{}.jsonl", day_name(utc_date(now + 3 * DAY_MS)));
        assert!(!dir.0.join(future).exists());
        assert_eq!(log.written(), 2);
    }

    /// Plain days a previous run left behind are finished at start-up (all but
    /// the newest, which the log resumes on) and by the periodic sweep once
    /// nothing has them open — the restarted recorder that writes nothing.
    #[test]
    fn stale_plain_days_are_compressed_at_start_and_by_the_sweep() {
        let dir = TempDir::new("eventlog-stale");
        fs::write(dir.0.join("2026-08-30.jsonl"), b"a\n").expect("write");
        fs::write(dir.0.join("2026-08-31.jsonl"), b"b\n").expect("write");
        fs::write(dir.0.join("2026-08-31.jsonl.gz.tmp"), b"torn").expect("write");

        let mut log = EventLog::open(&dir.0, 0).expect("open");
        assert_eq!(
            listing(&dir.0),
            vec!["2026-08-30.jsonl.gz", "2026-08-31.jsonl"],
            "the older day compressed, the newest resumed, the torn temp gone"
        );

        log.last_sweep = Instant::now()
            .checked_sub(SWEEP_INTERVAL)
            .expect("monotonic clock past one interval");
        log.tick();
        assert_eq!(
            listing(&dir.0),
            vec!["2026-08-30.jsonl.gz", "2026-08-31.jsonl.gz"]
        );
        assert_eq!(gunzip(&dir.0.join("2026-08-31.jsonl.gz")), "b\n");
    }

    /// A reader that makes up `total` bytes of event-log-shaped text and
    /// records the largest single read it was asked for.
    struct SyntheticDay {
        total: u64,
        produced: std::rc::Rc<std::cell::Cell<u64>>,
        largest_read: usize,
    }

    impl Read for SyntheticDay {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.largest_read = self.largest_read.max(buf.len());
            let done = self.produced.get();
            let n = (self.total - done).min(buf.len() as u64) as usize;
            for (i, b) in buf[..n].iter_mut().enumerate() {
                let at = done + i as u64;
                // Hex-like lines of scrambled content: compressible, as a
                // log is, but not so much that a buffer of output needs the
                // whole input (the property under test).
                let mut x = at.wrapping_add(0x9e37_79b9_7f4a_7c15);
                x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
                x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
                x ^= x >> 31;
                *b = if at % 97 == 96 {
                    b'\n'
                } else {
                    b"0123456789abcdef"[(x & 15) as usize]
                };
            }
            self.produced.set(done + n as u64);
            Ok(n)
        }
    }

    /// A sink that notes how much input had been read when output first came
    /// out, and keeps a decoder running over what it is given.
    struct StreamingSink {
        produced: std::rc::Rc<std::cell::Cell<u64>>,
        input_at_first_output: Option<u64>,
        compressed: u64,
        decoder: flate2::write::GzDecoder<Counting>,
    }

    #[derive(Default)]
    struct Counting(u64);

    impl Write for Counting {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0 += buf.len() as u64;
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Write for StreamingSink {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.input_at_first_output
                .get_or_insert(self.produced.get());
            self.compressed += buf.len() as u64;
            self.decoder.write_all(buf)?;
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// The compressor streams: it reads a bounded buffer at a time and writes
    /// output while most of the input is still unread, so its memory cannot
    /// grow with the size of the day (the old one read the whole day and
    /// built the whole `.gz` in memory — ~600 MB at midnight on a busy site).
    #[test]
    fn compression_streams_in_bounded_buffers() {
        const TOTAL: u64 = 16 * 1024 * 1024;
        let produced = std::rc::Rc::new(std::cell::Cell::new(0u64));
        let input = SyntheticDay {
            total: TOTAL,
            produced: produced.clone(),
            largest_read: 0,
        };
        let sink = StreamingSink {
            produced: produced.clone(),
            input_at_first_output: None,
            compressed: 0,
            decoder: flate2::write::GzDecoder::new(Counting::default()),
        };
        let mut input = input;
        let sink = gzip_stream(&mut input, sink).expect("compress");

        assert_eq!(produced.get(), TOTAL, "all of the input was read");
        assert!(
            input.largest_read <= COMPRESS_BUF,
            "read {} bytes at once",
            input.largest_read
        );
        let first = sink.input_at_first_output.expect("some output");
        assert!(
            first < TOTAL / 8,
            "output began after {first} of {TOTAL} input bytes: not streaming"
        );
        assert!(sink.compressed < TOTAL * 3 / 4, "it did compress");
        let decoded = sink.decoder.finish().expect("valid gzip").0;
        assert_eq!(decoded, TOTAL, "round-trips to the same length");
    }

    /// What `harness/eventlog_repeats.py`'s `expand` does: drop each full
    /// copy's `id`, and give each repeat the body of the nearest preceding
    /// line with its id.
    fn expand(body: &str) -> String {
        let mut full: std::collections::HashMap<String, String> = Default::default();
        let mut out = String::new();
        for line in body.lines() {
            let at_end = line
                .strip_prefix(r#"{"at":""#)
                .and_then(|r| r.find('"'))
                .map(|i| i + 8)
                .expect("every line starts with at");
            let (head, rest) = line.split_at(at_end);
            if let Some(id) = rest
                .strip_prefix(r#","rep":"#)
                .and_then(|r| r.strip_suffix('}'))
            {
                out.push_str(head);
                out.push_str(&full[id]);
            } else if let Some(r) = rest.strip_prefix(r#","id":"#) {
                let digits = r.bytes().take_while(u8::is_ascii_digit).count();
                full.insert(r[..digits].to_owned(), r[digits..].to_owned());
                out.push_str(head);
                out.push_str(&r[digits..]);
            } else {
                out.push_str(line);
            }
            out.push('\n');
        }
        out
    }

    fn rfss(at_ms: i64, hex: &str) -> Value {
        serde_json::json!({
            "at": crate::pipeline::observations::rfc3339(at_ms),
            "ch": "control", "type": "tsbk", "mfid": 0, "opcode": 58,
            "name": "RFSS_STATUS_BCST", "hex": hex, "fields": {},
        })
    }

    fn grant(at_ms: i64) -> Value {
        serde_json::json!({
            "at": crate::pipeline::observations::rfc3339(at_ms),
            "ch": "control", "type": "tsbk", "mfid": 0, "opcode": 0,
            "name": "GRP_VCH_GRANT", "hex": "8000000a301306a80089544d",
            "fields": {"talkgroup": 1704, "source": 9000013},
        })
    }

    /// A constant broadcast heard again within the window is written as a
    /// back-reference; outside it, in full again under the same id. Anything
    /// not flagged repeatable — here a grant — is written in full however
    /// often it repeats. Expanded, the file is byte for byte what writing
    /// every record whole produces.
    #[test]
    fn constant_broadcasts_repeat_as_back_references_and_expand_exactly() {
        let records: Vec<(i64, Value, bool)> = vec![
            (0, rfss(AUG31, "ba000a30340a01103e70e839"), true),
            (1_000, grant(AUG31 + 1_000), false),
            (1_500, rfss(AUG31 + 1_500, "ba000a30340a01103e70e839"), true),
            (2_000, rfss(AUG31 + 2_000, "ba000a30340a01103e70ffff"), true),
            (2_500, rfss(AUG31 + 2_500, "ba000a30340a01103e70e839"), true),
            (3_000, grant(AUG31 + 3_000), false),
            (
                60_000,
                rfss(AUG31 + 60_000, "ba000a30340a01103e70e839"),
                true,
            ),
            (
                61_000,
                rfss(AUG31 + 61_000, "ba000a30340a01103e70e839"),
                true,
            ),
            (
                62_000,
                rfss(AUG31 + 62_000, "ba000a30340a01103e70e839"),
                true,
            ),
        ];
        let coalesced = TempDir::new("eventlog-rep");
        let whole = TempDir::new("eventlog-rep-whole");
        let mut log = EventLog::open(&coalesced.0, 0).expect("open");
        let mut plain = EventLog::open(&whole.0, 0).expect("open");
        for (dt, rec, repeatable) in &records {
            plain.write(AUG31 + dt, &rec.to_string()).expect("write");
            log.write_record(AUG31 + dt, rec.clone(), *repeatable)
                .expect("write");
        }
        log.flush();
        plain.flush();
        let body = fs::read_to_string(coalesced.0.join("2026-08-31.jsonl")).expect("read");
        let lines: Vec<&str> = body.lines().collect();

        assert!(lines[0].starts_with(r#"{"at":"2026-08-31T12:00:00Z","id":0,"ch":"control","#));
        assert!(lines[1].contains("GRP_VCH_GRANT") && !lines[1].contains(r#""id""#));
        assert_eq!(lines[2], r#"{"at":"2026-08-31T12:00:01.5Z","rep":0}"#);
        assert!(lines[3].contains(r#","id":1,"#), "new content, new id");
        assert_eq!(lines[4], r#"{"at":"2026-08-31T12:00:02.5Z","rep":0}"#);
        assert!(
            lines[5].contains("GRP_VCH_GRANT"),
            "a grant is never a repeat"
        );
        assert_eq!(lines[6], r#"{"at":"2026-08-31T12:01:00Z","rep":0}"#);
        assert!(
            lines[7].starts_with(r#"{"at":"2026-08-31T12:01:01Z","id":0,"#),
            "61 s after the full copy: written in full again, same id"
        );
        assert_eq!(lines[8], r#"{"at":"2026-08-31T12:01:02Z","rep":0}"#);
        assert_eq!(log.repeated(), 4);
        assert_eq!(log.written(), records.len() as u64, "one line per event");

        let whole = fs::read_to_string(whole.0.join("2026-08-31.jsonl")).expect("read");
        assert_eq!(expand(&body), whole);
        assert!(body.len() < whole.len());
    }

    /// A repeat never names a full copy in another file: the next day starts
    /// from nothing, so each file (and each archive of it) stands alone.
    #[test]
    fn every_file_holds_the_full_copies_its_repeats_name() {
        let dir = TempDir::new("eventlog-rep-day");
        let mut log = EventLog::open(&dir.0, 0).expect("open");
        let midnight = AUG31 + DAY_MS / 2;
        for at in [midnight - 2_000, midnight - 1_000, midnight + 1_000] {
            log.write_record(at, rfss(at, "ba000a30340a01103e70e839"), true)
                .expect("write");
        }
        log.flush();
        let sep1 = fs::read_to_string(dir.0.join("2026-09-01.jsonl")).expect("read");
        assert!(
            sep1.starts_with(r#"{"at":"2026-09-01T00:00:01Z","id":0,"ch""#),
            "{sep1}"
        );
        assert_eq!(
            gunzip(&dir.0.join("2026-08-31.jsonl.gz")).lines().count(),
            2
        );
        assert_eq!(log.repeated(), 1);
    }

    #[test]
    fn zero_keep_days_disables_pruning() {
        let dir = TempDir::new("eventlog-keep0");
        fs::write(dir.0.join("2000-01-01.jsonl"), b"x").expect("write");
        EventLog::open(&dir.0, 0).expect("open").prune();
        assert!(dir.0.join("2000-01-01.jsonl").exists());
    }
}
