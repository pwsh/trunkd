// SPDX-License-Identifier: GPL-3.0-or-later
//! Shipping finished event-log days to keyup.
//!
//! [`crate::pipeline::eventlog`] writes one JSONL file per UTC day and gzips
//! the previous one at rotation. That file is then *finished*: nothing will
//! ever append to it again. `[recorder] event_log_archive = true` uploads each
//! finished `.jsonl.gz` to `{keyup}/api/recorder/event-log` and leaves a
//! `<file>.archived` marker beside it holding keyup's reply.
//!
//! # Why a marker file rather than a database
//!
//! The marker is the only state this needs, and it lives next to the thing it
//! describes: a directory listing answers "what has been shipped?", copying the
//! directory copies the answer, and deleting a marker re-uploads that day. The
//! startup scan is therefore not a special case but *the* mechanism — it is how
//! days that finished while the recorder was off, or while keyup was
//! unreachable, are caught.
//!
//! # Where it runs
//!
//! On its own thread, a sibling of the observations and control threads. Never
//! the DSP thread: this reads whole files off disk and does HTTP. It also
//! respects the uplink hold — a recorder told to stop uploading is told to stop
//! uploading, and the day's log is not going anywhere.
//!
//! # A day in more than one file
//!
//! The event log never replaces a finished `.gz`: a day it has to finish a
//! second time (late lines, or a plain file a restart left behind) becomes
//! `YYYY-MM-DD.N.jsonl.gz` beside the first. Keyup keeps **one** file per
//! day and replaces it when the same date arrives with a different digest, so
//! the parts are never uploaded separately — a lone `.1` would replace the whole
//! day with its few late lines. A day is uploaded as one stream instead: its
//! own `.gz` followed by each `.N` in order, byte for byte. Concatenated gzip
//! members are a valid gzip file (`zcat` and Python's `gzip` read it whole), so
//! what keyup stores is the complete day. A part appearing after the day was
//! archived makes the day pending again, and the superset replaces what keyup
//! had. Every part gets its marker only once keyup has accepted the whole.
//!
//! # Failure
//!
//! A failed upload backs off from [`MIN_BACKOFF`] to [`MAX_BACKOFF`] and is
//! retried; nothing is deleted and nothing is lost. A `409` from keyup means it
//! already has that day, which is a success: the marker is written so the file
//! stops being offered.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use trunkd_uplink::UplinkHold;

/// Timeout for one archive POST, connect through response. Generous next to the
/// control loop's: a day of gzipped event log is tens of megabytes.
const HTTP_TIMEOUT: Duration = Duration::from_secs(120);

/// First retry delay after a failure.
pub(crate) const MIN_BACKOFF: Duration = Duration::from_secs(60);

/// Ceiling on the retry delay.
pub(crate) const MAX_BACKOFF: Duration = Duration::from_secs(3600);

/// How often the directory is rescanned, and how promptly a released uplink
/// hold is noticed. A `read_dir` of a directory with a fortnight of days in it
/// is nothing next to the once-a-day event this thread exists for.
const SCAN_INTERVAL: Duration = Duration::from_secs(1);

/// Suffix of a finished, compressed day.
const GZ_SUFFIX: &str = ".jsonl.gz";

/// Suffix of the marker written beside an uploaded day.
pub(crate) const ARCHIVED_SUFFIX: &str = ".archived";

/// Derive the event-log archive endpoint from the configured call-upload URL,
/// exactly as the observations and control endpoints are derived.
pub(crate) fn event_log_url(call_upload_url: &str) -> String {
    const UPLOAD: &str = "/api/call-upload";
    const ROUTE: &str = "/api/recorder/event-log";
    let trimmed = call_upload_url.trim_end_matches('/');
    match trimmed.rfind(UPLOAD) {
        Some(at) => format!("{}{ROUTE}{}", &trimmed[..at], &trimmed[at + UPLOAD.len()..]),
        None => format!("{trimmed}{ROUTE}"),
    }
}

// ---------------------------------------------------------------------------
// The reported state
// ---------------------------------------------------------------------------

/// Who last set the archive flag: the TOML, or keyup's desired state.
///
/// Reported beside the flag itself for the same reason the uplink hold reports
/// its source — an operator looking at a box that is not shipping its event log
/// needs to know whether that is what the file says or what keyup asked for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum ArchiveSource {
    /// `[recorder] event_log_archive` in the TOML.
    #[default]
    Config,
    /// keyup's desired-state `recorder.event_log_archive`.
    Keyup,
}

impl ArchiveSource {
    /// The wire name.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            ArchiveSource::Config => "config",
            ArchiveSource::Keyup => "keyup",
        }
    }
}

/// The `event_log` object of the status POST and the panel.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct EventLogReport {
    /// Whether archiving is enabled at all.
    pub archive: bool,
    /// Who last set that: `config` | `keyup`.
    pub source: ArchiveSource,
    /// Finished days still waiting to be uploaded.
    pub pending: u64,
    /// The most recent day successfully archived, `YYYY-MM-DD`.
    pub last_archived: Option<String>,
    /// Why the last attempt failed, when one did.
    pub last_error: Option<String>,
    /// The timestamp of the newest line the event log actually took, RFC 3339;
    /// `None` while the log is off or nothing has been written this run.
    ///
    /// "The event log stopped an hour ago" is otherwise only answerable by
    /// looking at a file's mtime on the box — which is exactly the question
    /// asked when a recorder goes quiet, and the one field that answers it
    /// from the panel or from keyup.
    pub last_written_at: Option<String>,
}

impl EventLogReport {
    /// The wire object.
    pub(crate) fn to_json(&self) -> Value {
        json!({
            "archive": self.archive,
            "source": self.source.as_str(),
            "pending": self.pending,
            "last_archived": self.last_archived,
            "last_error": self.last_error,
            "last_written_at": self.last_written_at,
        })
    }
}

/// The live state behind [`EventLogReport`], shared with the status reporters.
///
/// Written only by the archive thread and read by the control and panel
/// threads, so the counters are atomics and the two strings sit behind a mutex
/// nothing holds across an I/O call.
#[derive(Debug, Default)]
pub(crate) struct ArchiveState {
    archive: AtomicBool,
    /// Whether keyup, rather than the TOML, last set `archive`.
    from_keyup: AtomicBool,
    pending: AtomicU64,
    /// Wall clock of the newest event-log line written, in unix milliseconds;
    /// 0 means "nothing written". Written by the observations thread (one
    /// relaxed store per line, off any hot path) and read by the reporters.
    last_written_ms: AtomicI64,
    last_archived: Mutex<Option<String>>,
    last_error: Mutex<Option<String>>,
}

impl ArchiveState {
    /// A state block for a recorder with archiving `enabled` or not.
    pub(crate) fn new(enabled: bool) -> Arc<ArchiveState> {
        Arc::new(ArchiveState {
            archive: AtomicBool::new(enabled),
            ..ArchiveState::default()
        })
    }

    /// Whether uploading is on right now.
    pub(crate) fn enabled(&self) -> bool {
        self.archive.load(Ordering::Relaxed)
    }

    /// Who last set the flag.
    pub(crate) fn source(&self) -> ArchiveSource {
        match self.from_keyup.load(Ordering::Relaxed) {
            true => ArchiveSource::Keyup,
            false => ArchiveSource::Config,
        }
    }

    /// Turn archiving on or off at runtime, recording who did it. Returns
    /// whether the flag actually moved.
    ///
    /// Live: the archive thread runs whether or not this is set and consults it
    /// each pass, so turning it on ships the backlog the directory already
    /// holds rather than skipping the days that accumulated while it was off.
    pub(crate) fn set_archive(&self, enabled: bool, source: ArchiveSource) -> bool {
        self.from_keyup
            .store(matches!(source, ArchiveSource::Keyup), Ordering::Relaxed);
        self.archive.swap(enabled, Ordering::Relaxed) != enabled
    }

    /// Snapshot for a status POST.
    pub(crate) fn report(&self) -> EventLogReport {
        EventLogReport {
            archive: self.archive.load(Ordering::Relaxed),
            source: self.source(),
            pending: self.pending.load(Ordering::Relaxed),
            last_archived: lock(&self.last_archived).clone(),
            last_error: lock(&self.last_error).clone(),
            last_written_at: match self.last_written_ms.load(Ordering::Relaxed) {
                0 => None,
                ms => Some(crate::pipeline::observations::rfc3339(ms)),
            },
        }
    }

    /// Record that the event log took a line stamped `unix_ms`.
    ///
    /// Never moves backwards: the log is fed from a queue that can hold events
    /// from two tuners with their own timelines, and the reported figure is
    /// "the newest line we have written", not "the last one we happened to
    /// see".
    pub(crate) fn note_written(&self, unix_ms: i64) {
        self.last_written_ms.fetch_max(unix_ms, Ordering::Relaxed);
    }

    /// The newest event-log line written, in unix milliseconds; 0 if none.
    #[cfg(test)]
    pub(crate) fn last_written_ms(&self) -> i64 {
        self.last_written_ms.load(Ordering::Relaxed)
    }

    fn set_pending(&self, n: u64) {
        self.pending.store(n, Ordering::Relaxed);
    }

    fn note_archived(&self, day: &str) {
        *lock(&self.last_archived) = Some(day.to_owned());
        *lock(&self.last_error) = None;
    }

    fn note_error(&self, error: String) {
        *lock(&self.last_error) = Some(error);
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

/// One finished day to upload: every compressed part of it, in upload order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DayArchive {
    /// `YYYY-MM-DD`.
    pub day: String,
    /// `YYYY-MM-DD.jsonl.gz` (when present) first, then each
    /// `YYYY-MM-DD.N.jsonl.gz` by `N` — already-archived parts included, since
    /// keyup replaces the day with whatever is sent.
    pub parts: Vec<PathBuf>,
}

/// The finished days in `dir` with at least one part that has no `.archived`
/// marker, oldest day first.
///
/// Sorted by day, which for `YYYY-MM-DD` is chronological — a recorder
/// catching up after a long outage ships the oldest day first, so a keyup
/// watching the stream sees the log fill forwards.
pub(crate) fn pending_archives(dir: &Path) -> Vec<DayArchive> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut days: std::collections::BTreeMap<String, Vec<(u32, PathBuf)>> = Default::default();
    for path in rd.flatten().map(|e| e.path()) {
        if let Some((day, n)) = part_of(&path) {
            days.entry(day).or_default().push((n, path));
        }
    }
    days.into_iter()
        .filter(|(_, parts)| parts.iter().any(|(_, p)| !marker_for(p).exists()))
        .map(|(day, mut parts)| {
            parts.sort();
            DayArchive {
                day,
                parts: parts.into_iter().map(|(_, p)| p).collect(),
            }
        })
        .collect()
}

/// The marker path for a finished day.
pub(crate) fn marker_for(gz: &Path) -> PathBuf {
    let mut name = gz.as_os_str().to_owned();
    name.push(ARCHIVED_SUFFIX);
    PathBuf::from(name)
}

/// The `YYYY-MM-DD` a finished-day file is for and its part number, from its
/// name: `YYYY-MM-DD.jsonl.gz` is part 0, `YYYY-MM-DD.N.jsonl.gz` — a later
/// file the event log wrote for a day it had already finished once, rather
/// than replacing the first — is part `N`. `None` for anything else.
pub(crate) fn part_of(gz: &Path) -> Option<(String, u32)> {
    let stem = gz.file_name()?.to_str()?.strip_suffix(GZ_SUFFIX)?;
    let day = crate::pipeline::eventlog::day_stem(stem)?;
    let n = match stem.strip_prefix(day)?.strip_prefix('.') {
        None => 0,
        Some(n) => n.parse().ok()?,
    };
    Some((day.to_owned(), n))
}

/// The next backoff after a failure: double it, capped at [`MAX_BACKOFF`].
pub(crate) fn next_backoff(current: Duration) -> Duration {
    (current * 2).min(MAX_BACKOFF)
}

/// Whether an HTTP status means "keyup has this day now".
///
/// `409 Conflict` is keyup saying it already holds that day, which is the
/// outcome we wanted: re-uploading it would only cost bandwidth, and treating
/// it as a failure would retry forever.
pub(crate) fn archive_accepted(status: u16) -> bool {
    (200..300).contains(&status) || status == 409
}

// ---------------------------------------------------------------------------
// The thread
// ---------------------------------------------------------------------------

/// What the archive thread needs to run.
#[derive(Clone)]
pub(crate) struct ArchiveConfig {
    /// The event-log directory to scan.
    pub dir: PathBuf,
    /// Full endpoint URL. Empty disables uploading (the scan still counts, so
    /// the status block is honest about the backlog).
    pub url: String,
    /// keyup API token, the same one observations use.
    pub api_key: String,
    /// This recorder's instance id.
    pub instance: String,
    /// The uplink hold: while it is set, nothing is uploaded.
    pub hold: Arc<UplinkHold>,
}

/// Run the archive loop until `stop` is set.
pub(crate) fn run(cfg: ArchiveConfig, state: Arc<ArchiveState>, stop: Arc<AtomicBool>) {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(HTTP_TIMEOUT))
        .build()
        .into();
    let mut backoff = MIN_BACKOFF;
    let mut next_attempt = Instant::now();

    while !stop.load(Ordering::Relaxed) {
        let pending = pending_archives(&cfg.dir);
        state.set_pending(pending.len() as u64);

        let ready = Instant::now() >= next_attempt
            && state.enabled()
            && !cfg.url.is_empty()
            && !cfg.hold.is_held()
            && !pending.is_empty();
        if ready {
            // Oldest first, one per pass: a recorder catching up must not spend
            // minutes inside one iteration ignoring its stop flag.
            let day = &pending[0];
            match upload_day(&agent, &cfg, day) {
                Ok(day) => {
                    state.note_archived(&day);
                    backoff = MIN_BACKOFF;
                    next_attempt = Instant::now();
                    tracing::info!(day = %day, "event-log archive uploaded");
                    continue;
                }
                Err(e) => {
                    state.note_error(e.clone());
                    next_attempt = Instant::now() + backoff;
                    tracing::warn!(
                        day = %day.day,
                        parts = day.parts.len(),
                        error = %e,
                        retry_in_s = backoff.as_secs(),
                        "event-log archive upload failed"
                    );
                    backoff = next_backoff(backoff);
                }
            }
        }
        sleep_until(&stop, SCAN_INTERVAL);
    }
}

/// Sleep in short slices so the stop flag is honoured promptly.
fn sleep_until(stop: &AtomicBool, total: Duration) {
    let deadline = Instant::now() + total;
    while Instant::now() < deadline && !stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Upload one finished day — all of its parts, as one stream — and mark every
/// part on success. Returns the day.
///
/// Two passes over the files, neither holding more than a buffer: the first
/// hashes and measures the concatenation (keyup verifies both), the second
/// streams it as the multipart file body with an exact `Content-Length`.
fn upload_day(
    agent: &ureq::Agent,
    cfg: &ArchiveConfig,
    day: &DayArchive,
) -> Result<String, String> {
    let (digest, lengths) = digest_parts(&day.parts)?;
    let len: u64 = lengths.iter().sum();
    let len_text = len.to_string();
    // The upload *is* the day, whichever parts it is made of.
    let filename = format!("{}{GZ_SUFFIX}", day.day);
    let (content_type, head, tail) = multipart_frame(
        &[
            ("instance", cfg.instance.as_str()),
            ("date", day.day.as_str()),
            ("sha256", digest.as_str()),
            ("bytes", len_text.as_str()),
        ],
        &filename,
    )?;
    let total = head.len() as u64 + len + tail.len() as u64;
    let body = concatenation(head, &day.parts, &lengths, tail)?;

    let (status, reply) = match agent
        .post(&cfg.url)
        .header("Authorization", &format!("Bearer {}", cfg.api_key))
        .header("Content-Type", &content_type)
        .header("Content-Length", &total.to_string())
        .send(ureq::SendBody::from_owned_reader(body))
    {
        Ok(mut resp) => (
            resp.status().as_u16(),
            resp.body_mut().read_to_string().unwrap_or_default(),
        ),
        // A status code is a reply, not a transport error: 409 in particular
        // has to reach the accept test below rather than being read as "keyup
        // is unreachable".
        Err(ureq::Error::StatusCode(code)) => (code, String::new()),
        Err(e) => return Err(e.to_string()),
    };
    if !archive_accepted(status) {
        return Err(format!(
            "keyup answered {status}: {}",
            truncate(&reply, 200)
        ));
    }
    for part in &day.parts {
        write_marker(part, status, &reply)?;
    }
    Ok(day.day.clone())
}

/// The SHA-256 of `parts` concatenated, and each part's length, read a buffer
/// at a time.
fn digest_parts(parts: &[PathBuf]) -> Result<(String, Vec<u64>), String> {
    use std::io::Read;
    let mut hasher = Sha256::new();
    let mut lengths = Vec::with_capacity(parts.len());
    let mut buf = vec![0u8; 64 * 1024];
    for part in parts {
        let mut f =
            std::fs::File::open(part).map_err(|e| format!("read {}: {e}", part.display()))?;
        let mut n_total = 0u64;
        loop {
            let n = match f.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(format!("read {}: {e}", part.display())),
            };
            hasher.update(&buf[..n]);
            n_total += n as u64;
        }
        lengths.push(n_total);
    }
    Ok((hasher.finish(), lengths))
}

/// The request body as a reader: the multipart `head`, each part (held to the
/// length it was hashed at, so a file that changed underneath cannot send
/// bytes the digest does not cover), then the `tail`.
fn concatenation(
    head: Vec<u8>,
    parts: &[PathBuf],
    lengths: &[u64],
    tail: Vec<u8>,
) -> Result<Box<dyn std::io::Read + Send>, String> {
    use std::io::Read;
    let mut body: Box<dyn Read + Send> = Box::new(std::io::Cursor::new(head));
    for (part, len) in parts.iter().zip(lengths) {
        let f = std::fs::File::open(part).map_err(|e| format!("read {}: {e}", part.display()))?;
        body = Box::new(body.chain(f.take(*len)));
    }
    Ok(Box::new(body.chain(std::io::Cursor::new(tail))))
}

/// A multipart form with `fields` and one `file` part named `filename`, split
/// around where the file's bytes go: `(content type, head, tail)`. The file is
/// then streamed between the two rather than copied into one buffer.
fn multipart_frame(
    fields: &[(&str, &str)],
    filename: &str,
) -> Result<(String, Vec<u8>, Vec<u8>), String> {
    let (content_type, mut body) =
        trunkd_uplink::build_form(fields, Some(("file", filename, "application/gzip", &[])));
    let boundary = content_type
        .split("boundary=")
        .nth(1)
        .ok_or("multipart form without a boundary")?;
    // An empty file part ends in its CRLF, then the closing delimiter.
    let tail = format!("\r\n--{boundary}--\r\n").into_bytes();
    if !body.ends_with(&tail) {
        return Err("unexpected multipart form layout".to_owned());
    }
    body.truncate(body.len() - tail.len());
    Ok((content_type, body, tail))
}

/// Write the `.archived` marker beside a shipped day, holding keyup's reply.
///
/// Temp-and-rename, so an interrupted write never leaves a marker that claims a
/// day was archived when the file was only half written.
fn write_marker(gz: &Path, status: u16, reply: &str) -> Result<(), String> {
    let marker = marker_for(gz);
    // keyup's reply is JSON when it has something to say; anything else is
    // recorded verbatim so an operator reading the marker sees what happened.
    let body = match serde_json::from_str::<Value>(reply) {
        Ok(v) => v,
        Err(_) => json!({ "status": status, "reply": truncate(reply, 500) }),
    };
    let tmp = marker.with_extension("archived.tmp");
    std::fs::write(&tmp, body.to_string().as_bytes())
        .map_err(|e| format!("write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &marker).map_err(|e| format!("rename {}: {e}", marker.display()))
}

fn truncate(s: &str, max: usize) -> String {
    crate::config::truncate_chars(s, max)
}

// ---------------------------------------------------------------------------
// SHA-256
// ---------------------------------------------------------------------------

/// The lower-hex SHA-256 of `data` (FIPS 180-4).
///
/// Implemented here rather than pulled from a crate: it is sixty lines of
/// table-driven arithmetic used in exactly one place, and the alternative is a
/// new dependency in the trust path of a recorder that deliberately has very
/// few. Verified against the standard's own test vectors below.
pub(crate) fn sha256_hex(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    h.finish()
}

/// Incremental SHA-256, so a day of event log is hashed a buffer at a time.
pub(crate) struct Sha256 {
    h: [u32; 8],
    block: [u8; 64],
    filled: usize,
    len: u64,
}

impl Sha256 {
    pub(crate) fn new() -> Self {
        Sha256 {
            h: [
                0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
                0x5be0cd19,
            ],
            block: [0; 64],
            filled: 0,
            len: 0,
        }
    }

    pub(crate) fn update(&mut self, mut data: &[u8]) {
        self.len = self.len.wrapping_add(data.len() as u64);
        while !data.is_empty() {
            let take = (64 - self.filled).min(data.len());
            self.block[self.filled..self.filled + take].copy_from_slice(&data[..take]);
            self.filled += take;
            data = &data[take..];
            if self.filled == 64 {
                compress(&mut self.h, &self.block);
                self.filled = 0;
            }
        }
    }

    /// Pad, finish and render as lower hex.
    pub(crate) fn finish(mut self) -> String {
        // Message, 0x80, zero pad to 56 mod 64, then the bit length big-endian.
        let bit_len = self.len.wrapping_mul(8);
        self.block[self.filled] = 0x80;
        self.filled += 1;
        if self.filled > 56 {
            self.block[self.filled..].fill(0);
            compress(&mut self.h, &self.block);
            self.filled = 0;
        }
        self.block[self.filled..56].fill(0);
        self.block[56..].copy_from_slice(&bit_len.to_be_bytes());
        compress(&mut self.h, &self.block);
        self.h.iter().map(|w| format!("{w:08x}")).collect()
    }
}

/// One 64-byte block of the FIPS 180-4 compression function.
fn compress(h: &mut [u32; 8], chunk: &[u8; 64]) {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut w = [0u32; 64];
    for (i, word) in w.iter_mut().enumerate().take(16) {
        let b = &chunk[i * 4..i * 4 + 4];
        *word = u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
    }
    for i in 16..64 {
        let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
        let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
        w[i] = w[i - 16]
            .wrapping_add(s0)
            .wrapping_add(w[i - 7])
            .wrapping_add(s1);
    }
    let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = *h;
    for i in 0..64 {
        let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
        let ch = (e & f) ^ ((!e) & g);
        let t1 = hh
            .wrapping_add(s1)
            .wrapping_add(ch)
            .wrapping_add(K[i])
            .wrapping_add(w[i]);
        let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
        let maj = (a & b) ^ (a & c) ^ (b & c);
        let t2 = s0.wrapping_add(maj);
        hh = g;
        g = f;
        f = e;
        e = d.wrapping_add(t1);
        d = c;
        c = b;
        b = a;
        a = t1.wrapping_add(t2);
    }
    for (slot, v) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
        *slot = slot.wrapping_add(v);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_matches_the_standard_test_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        // Crosses the padding boundary in both directions (55, 56, 64 bytes).
        assert_eq!(
            sha256_hex(&[b'a'; 55]),
            "9f4390f8d30c2dd92ec9f095b65e2b9ae9b0a925a5258e241c9f1e910f734318"
        );
        assert_eq!(
            sha256_hex(&[b'a'; 56]),
            "b35439a4ac6f0948b6d6f9e3c6af0f5f590ce20f1bde7090ef7970686ec6738a"
        );
        assert_eq!(
            sha256_hex(&[b'a'; 64]),
            "ffe054fe7ae0cb6dc65c3af9b61d5209f439851db43d0ba5997337df154668eb"
        );
    }

    #[test]
    fn the_archive_endpoint_replaces_the_upload_route() {
        assert_eq!(
            event_log_url("http://192.0.2.60/api/call-upload"),
            "http://192.0.2.60/api/recorder/event-log"
        );
        assert_eq!(
            event_log_url("http://192.0.2.60/"),
            "http://192.0.2.60/api/recorder/event-log"
        );
    }

    #[test]
    fn a_409_counts_as_delivered_and_a_500_does_not() {
        assert!(archive_accepted(200));
        assert!(archive_accepted(204));
        assert!(archive_accepted(409), "keyup already has that day");
        assert!(!archive_accepted(500));
        assert!(!archive_accepted(401));
    }

    #[test]
    fn the_backoff_doubles_up_to_the_cap() {
        let mut b = MIN_BACKOFF;
        assert_eq!(b, Duration::from_secs(60));
        b = next_backoff(b);
        assert_eq!(b, Duration::from_secs(120));
        for _ in 0..20 {
            b = next_backoff(b);
        }
        assert_eq!(b, MAX_BACKOFF);
    }

    // -- the upload itself, against a stub keyup ---------------------------

    use std::collections::HashMap;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// One request the stub received: the text fields and the file part.
    #[derive(Clone, Debug, Default)]
    struct Req {
        fields: HashMap<String, String>,
        file: Option<(String, Vec<u8>)>,
        authorization: String,
    }

    /// A one-route stub keyup, in the shape `trunkd-uplink`'s upload tests use:
    /// a plain `TcpListener` on an ephemeral port that parses the multipart body
    /// and answers with whatever the test asked for.
    struct Stub {
        port: u16,
        requests: Arc<Mutex<Vec<Req>>>,
        stop: Arc<AtomicBool>,
    }

    impl Stub {
        fn start(status: u16, body: &str) -> Stub {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
            let port = listener.local_addr().expect("addr").port();
            listener.set_nonblocking(true).expect("nonblocking");
            let requests = Arc::new(Mutex::new(Vec::new()));
            let reply = Arc::new(Mutex::new((status, body.to_owned())));
            let stop = Arc::new(AtomicBool::new(false));
            let (r, p, st) = (requests.clone(), reply.clone(), stop.clone());
            std::thread::spawn(move || {
                while !st.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            // Windows hands an accepted socket the listener's
                            // non-blocking mode (Linux does not), and a
                            // WouldBlock read would truncate the request.
                            let _ = stream.set_nonblocking(false);
                            let raw = read_request(&mut stream);
                            let req = parse_request(&raw);
                            lock(&r).push(req);
                            let (status, body) = lock(&p).clone();
                            let _ = stream.write_all(
                                format!(
                                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n\
                                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                                    body.len()
                                )
                                .as_bytes(),
                            );
                            let _ = stream.flush();
                        }
                        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(2));
                        }
                        Err(_) => break,
                    }
                }
            });
            Stub {
                port,
                requests,
                stop,
            }
        }

        fn url(&self) -> String {
            format!("http://127.0.0.1:{}/api/recorder/event-log", self.port)
        }

        fn requests(&self) -> Vec<Req> {
            lock(&self.requests).clone()
        }
    }

    impl Drop for Stub {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
        }
    }

    /// Read one request: headers, then exactly `Content-Length` bytes.
    fn read_request(stream: &mut std::net::TcpStream) -> Vec<u8> {
        let mut raw: Vec<u8> = Vec::new();
        let mut buf = [0u8; 8192];
        let mut header_end = None;
        let mut want = 0usize;
        loop {
            match stream.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    raw.extend_from_slice(&buf[..n]);
                    if header_end.is_none() {
                        if let Some(at) = find(&raw, b"\r\n\r\n") {
                            header_end = Some(at + 4);
                            let head = String::from_utf8_lossy(&raw[..at]).to_lowercase();
                            want = head
                                .lines()
                                .find_map(|l| l.strip_prefix("content-length:"))
                                .and_then(|v| v.trim().parse().ok())
                                .unwrap_or(0);
                        }
                    }
                    if let Some(start) = header_end {
                        if raw.len() >= start + want {
                            break;
                        }
                    }
                }
                Err(_) => break,
            }
        }
        raw
    }

    fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack.windows(needle.len()).position(|w| w == needle)
    }

    /// Pull the multipart fields and the one file part out of a raw request.
    fn parse_request(raw: &[u8]) -> Req {
        let at = find(raw, b"\r\n\r\n").map(|a| a + 4).unwrap_or(0);
        let head = String::from_utf8_lossy(&raw[..at.saturating_sub(4)]).to_string();
        let boundary = head
            .lines()
            .find_map(|l| {
                l.to_lowercase()
                    .starts_with("content-type:")
                    .then(|| l.to_owned())
            })
            .and_then(|l| l.split("boundary=").nth(1).map(str::to_owned))
            .unwrap_or_default();
        let mut req = Req {
            // Header names are case-insensitive and clients differ.
            authorization: head
                .lines()
                .find(|l| l.to_lowercase().starts_with("authorization:"))
                .and_then(|l| l.split_once(':'))
                .map(|(_, v)| v.trim().to_owned())
                .unwrap_or_default(),
            ..Req::default()
        };
        if boundary.is_empty() {
            return req;
        }
        let sep = format!("--{boundary}");
        let body = &raw[at..];
        for part in split_on(body, sep.as_bytes()) {
            let Some(hend) = find(part, b"\r\n\r\n") else {
                continue;
            };
            let disp = String::from_utf8_lossy(&part[..hend]).to_string();
            let Some(name) = between(&disp, "name=\"", "\"") else {
                continue;
            };
            // Every part ends with the CRLF that precedes the next boundary.
            let value = &part[hend + 4..part.len().saturating_sub(2)];
            match between(&disp, "filename=\"", "\"") {
                Some(filename) => req.file = Some((filename, value.to_vec())),
                None => {
                    req.fields
                        .insert(name, String::from_utf8_lossy(value).to_string());
                }
            }
        }
        req
    }

    fn split_on<'a>(body: &'a [u8], sep: &[u8]) -> Vec<&'a [u8]> {
        let mut out = Vec::new();
        let mut rest = body;
        while let Some(at) = find(rest, sep) {
            let (before, after) = rest.split_at(at);
            if !before.is_empty() {
                out.push(&before[..before.len().saturating_sub(0)]);
            }
            rest = &after[sep.len()..];
        }
        // Trim the leading CRLF each part carries after its boundary.
        out.into_iter()
            .map(|p| p.strip_prefix(b"\r\n".as_slice()).unwrap_or(p))
            .collect()
    }

    fn between(s: &str, open: &str, close: &str) -> Option<String> {
        let start = s.find(open)? + open.len();
        let end = s[start..].find(close)? + start;
        Some(s[start..end].to_owned())
    }

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

    /// A finished day on disk, with recognisable contents.
    fn write_day(dir: &Path, day: &str, body: &[u8]) -> PathBuf {
        let p = dir.join(format!("{day}.jsonl.gz"));
        std::fs::write(&p, body).expect("write day");
        p
    }

    fn cfg_for(stub: &Stub, dir: &Path, hold: Arc<UplinkHold>) -> ArchiveConfig {
        ArchiveConfig {
            dir: dir.to_owned(),
            url: stub.url(),
            api_key: "kup_test".into(),
            instance: "rec-01".into(),
            hold,
        }
    }

    /// Wait for `cond` for up to a second and a half, polling briefly.
    fn wait_for(mut cond: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            if cond() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        false
    }

    #[test]
    fn only_finished_days_without_a_marker_are_pending() {
        let dir = TempDir::new("archive-pending");
        write_day(&dir.0, "2026-08-29", b"old");
        let done = write_day(&dir.0, "2026-08-30", b"done");
        write_day(&dir.0, "2026-08-31", b"new");
        // Today's open log is not finished and must never be offered.
        std::fs::write(dir.0.join("2026-09-01.jsonl"), b"open").expect("write");
        std::fs::write(marker_for(&done), b"{}").expect("marker");

        let pending = pending_archives(&dir.0);
        assert_eq!(
            pending,
            vec![
                DayArchive {
                    day: "2026-08-29".into(),
                    parts: vec![dir.0.join("2026-08-29.jsonl.gz")],
                },
                DayArchive {
                    day: "2026-08-31".into(),
                    parts: vec![dir.0.join("2026-08-31.jsonl.gz")],
                },
            ],
            "oldest day first, the marked one skipped, the open day untouched"
        );
    }

    /// The event log never replaces a finished day; a later file for the
    /// same day is `YYYY-MM-DD.N.jsonl.gz`. The parts are one pending day, the
    /// day's own file first and then by `N` (numerically: 10 after 2).
    #[test]
    fn a_days_parts_are_offered_together_in_order() {
        let dir = TempDir::new("archive-suffix");
        let first = write_day(&dir.0, "2026-08-31", b"first");
        let tenth = dir.0.join("2026-08-31.10.jsonl.gz");
        let second = dir.0.join("2026-08-31.2.jsonl.gz");
        std::fs::write(&tenth, b"tenth").expect("write");
        std::fs::write(&second, b"second").expect("write");
        std::fs::write(dir.0.join("notes.x.jsonl.gz"), b"?").expect("write");
        assert_eq!(
            pending_archives(&dir.0),
            vec![DayArchive {
                day: "2026-08-31".into(),
                parts: vec![first.clone(), second.clone(), tenth.clone()],
            }]
        );
        assert_eq!(part_of(&first), Some(("2026-08-31".into(), 0)));
        assert_eq!(part_of(&tenth), Some(("2026-08-31".into(), 10)));
        assert_eq!(part_of(&dir.0.join("notes.x.jsonl.gz")), None);
    }

    /// A gzip member holding `text`.
    fn gz(text: &str) -> Vec<u8> {
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(text.as_bytes()).expect("compress");
        enc.finish().expect("finish")
    }

    /// Everything in a (possibly multi-member) gzip stream.
    fn gunzip_all(bytes: &[u8]) -> String {
        let mut out = String::new();
        flate2::read::MultiGzDecoder::new(bytes)
            .read_to_string(&mut out)
            .expect("valid gzip");
        out
    }

    /// Run the archiver against `stub` until `done`, then stop it.
    fn archive_until(stub: &Stub, dir: &Path, done: impl FnMut() -> bool) {
        let state = ArchiveState::new(true);
        let stop = Arc::new(AtomicBool::new(false));
        let cfg = cfg_for(stub, dir, Arc::new(UplinkHold::new(false)));
        let (s, t) = (state.clone(), stop.clone());
        let thread = std::thread::spawn(move || run(cfg, s, t));
        let finished = wait_for(done);
        stop.store(true, Ordering::Relaxed);
        let _ = thread.join();
        assert!(finished, "the archiver did not get there");
    }

    /// Keyup keeps one file per day and replaces it on a new digest, so a
    /// day's parts go up as one stream — the day's own member first, then each
    /// part — which decompresses to every line of every part, in order.
    #[test]
    fn a_day_in_parts_is_uploaded_as_one_concatenated_gzip() {
        let dir = TempDir::new("archive-parts");
        let main = gz("a\nb\n");
        let one = gz("late1\n");
        let two = gz("late2\n");
        let p0 = write_day(&dir.0, "2026-08-31", &main);
        let p1 = dir.0.join("2026-08-31.1.jsonl.gz");
        let p2 = dir.0.join("2026-08-31.2.jsonl.gz");
        std::fs::write(&p1, &one).expect("write");
        std::fs::write(&p2, &two).expect("write");
        let stub = Stub::start(200, r#"{"stored":true}"#);
        archive_until(&stub, &dir.0, || {
            [&p0, &p1, &p2].iter().all(|p| marker_for(p).exists())
        });

        let reqs = stub.requests();
        assert_eq!(reqs.len(), 1, "one upload for the day, not one per part");
        let req = &reqs[0];
        let (filename, body) = req.file.as_ref().expect("file part");
        assert_eq!(filename, "2026-08-31.jsonl.gz");
        let expected: Vec<u8> = [main, one, two].concat();
        assert_eq!(body, &expected, "the parts byte for byte, in order");
        assert_eq!(gunzip_all(body), "a\nb\nlate1\nlate2\n");
        assert_eq!(
            req.fields.get("date").map(String::as_str),
            Some("2026-08-31")
        );
        assert_eq!(
            req.fields.get("sha256").map(String::as_str),
            Some(sha256_hex(&expected).as_str()),
            "the digest is of the concatenation"
        );
        assert_eq!(
            req.fields.get("bytes").map(String::as_str),
            Some(expected.len().to_string().as_str())
        );
    }

    /// A part that appears after the day was archived makes the day pending
    /// again, and the whole day — not the new part alone — goes up, replacing
    /// what keyup had with the superset.
    #[test]
    fn a_day_that_gains_a_part_after_archiving_is_uploaded_again() {
        let dir = TempDir::new("archive-regain");
        let main = gz("a\n");
        let p0 = write_day(&dir.0, "2026-08-31", &main);
        let stub = Stub::start(200, r#"{"stored":true}"#);
        archive_until(&stub, &dir.0, || marker_for(&p0).exists());
        assert!(pending_archives(&dir.0).is_empty());

        let late = gz("late\n");
        let p1 = dir.0.join("2026-08-31.1.jsonl.gz");
        std::fs::write(&p1, &late).expect("write");
        assert_eq!(pending_archives(&dir.0).len(), 1, "pending again");
        archive_until(&stub, &dir.0, || marker_for(&p1).exists());

        let reqs = stub.requests();
        assert_eq!(reqs.len(), 2);
        let (_, body) = reqs[1].file.as_ref().expect("file part");
        assert_eq!(body, &[main, late].concat());
        assert_eq!(gunzip_all(body), "a\nlate\n");
        assert!(pending_archives(&dir.0).is_empty());
    }

    /// Keyup already having the day (409) is delivery for every part of it.
    #[test]
    fn a_409_marks_every_part_archived() {
        let dir = TempDir::new("archive-parts-409");
        let p0 = write_day(&dir.0, "2026-08-30", &gz("a\n"));
        let p1 = dir.0.join("2026-08-30.1.jsonl.gz");
        std::fs::write(&p1, gz("b\n")).expect("write");
        let stub = Stub::start(409, r#"{"error":"duplicate"}"#);
        archive_until(&stub, &dir.0, || {
            marker_for(&p0).exists() && marker_for(&p1).exists()
        });
        assert_eq!(stub.requests().len(), 1);
        assert!(pending_archives(&dir.0).is_empty());
    }

    /// Hashing a buffer at a time gives the one-shot digest wherever the
    /// buffers happen to split.
    #[test]
    fn incremental_sha256_matches_one_shot_across_splits() {
        let data: Vec<u8> = (0..1000u32).map(|i| (i * 7 % 251) as u8).collect();
        let whole = sha256_hex(&data);
        for split in [0usize, 1, 55, 56, 63, 64, 65, 127, 128, 999, 1000] {
            let mut h = Sha256::new();
            h.update(&data[..split]);
            h.update(&data[split..]);
            assert_eq!(h.finish(), whole, "split at {split}");
        }
        let mut h = Sha256::new();
        for b in &data {
            h.update(std::slice::from_ref(b));
        }
        assert_eq!(h.finish(), whole, "a byte at a time");
    }

    #[test]
    fn a_finished_day_is_uploaded_and_marked() {
        let dir = TempDir::new("archive-upload");
        let payload = b"gzipped-event-log-bytes";
        write_day(&dir.0, "2026-08-31", payload);
        let stub = Stub::start(200, r#"{"stored":true,"lines":2300000}"#);
        let hold = Arc::new(UplinkHold::new(false));
        let state = ArchiveState::new(true);
        let stop = Arc::new(AtomicBool::new(false));
        let cfg = cfg_for(&stub, &dir.0, hold);
        let (s, t) = (state.clone(), stop.clone());
        let thread = std::thread::spawn(move || run(cfg, s, t));

        assert!(
            wait_for(|| !stub.requests().is_empty()),
            "the day is offered without waiting for a rotation"
        );
        // The stub records the request before it answers, and `pending` only
        // drops at the rescan after the upload; stopping now could end the
        // loop before that rescan and leave the count at 1. Wait for the
        // marker (the upload is done) and the rescan that follows it.
        let marker = marker_for(&dir.0.join("2026-08-31.jsonl.gz"));
        assert!(
            wait_for(|| marker.exists() && state.report().pending == 0),
            "the day is marked and the next scan finds nothing owed"
        );
        stop.store(true, Ordering::Relaxed);
        let _ = thread.join();

        let req = &stub.requests()[0];
        assert_eq!(req.authorization, "Bearer kup_test");
        assert_eq!(
            req.fields.get("instance").map(String::as_str),
            Some("rec-01")
        );
        assert_eq!(
            req.fields.get("date").map(String::as_str),
            Some("2026-08-31")
        );
        assert_eq!(
            req.fields.get("bytes").map(String::as_str),
            Some(payload.len().to_string().as_str())
        );
        assert_eq!(
            req.fields.get("sha256").map(String::as_str),
            Some(sha256_hex(payload).as_str()),
            "the digest is of the gz as it sits on disk"
        );
        let (filename, bytes) = req.file.as_ref().expect("the gz is sent as `file`");
        assert_eq!(filename, "2026-08-31.jsonl.gz");
        assert_eq!(bytes, payload);

        // The marker holds keyup's own reply, so an operator reading the
        // directory sees what it said.
        let written: Value =
            serde_json::from_slice(&std::fs::read(&marker).expect("marker")).expect("json");
        assert_eq!(written["stored"], true);
        assert_eq!(written["lines"], 2_300_000);
        // The file itself is kept: retention is the event log's business.
        assert!(dir.0.join("2026-08-31.jsonl.gz").exists());

        let report = state.report();
        assert!(report.archive);
        assert_eq!(report.last_archived.as_deref(), Some("2026-08-31"));
        assert_eq!(report.last_error, None);
        assert_eq!(report.pending, 0, "nothing left once it is marked");
    }

    #[test]
    fn a_409_means_keyup_already_has_the_day() {
        let dir = TempDir::new("archive-409");
        write_day(&dir.0, "2026-08-30", b"already-there");
        let stub = Stub::start(409, r#"{"error":"duplicate"}"#);
        let state = ArchiveState::new(true);
        let stop = Arc::new(AtomicBool::new(false));
        let cfg = cfg_for(&stub, &dir.0, Arc::new(UplinkHold::new(false)));
        let (s, t) = (state.clone(), stop.clone());
        let thread = std::thread::spawn(move || run(cfg, s, t));

        let marker = marker_for(&dir.0.join("2026-08-30.jsonl.gz"));
        assert!(
            wait_for(|| marker.exists()),
            "a duplicate is delivered, not a failure to retry forever"
        );
        stop.store(true, Ordering::Relaxed);
        let _ = thread.join();

        assert_eq!(state.report().last_archived.as_deref(), Some("2026-08-30"));
        assert_eq!(state.report().last_error, None);
        assert_eq!(stub.requests().len(), 1, "and it is not offered again");
    }

    #[test]
    fn the_uplink_hold_suppresses_the_upload_until_it_is_released() {
        let dir = TempDir::new("archive-hold");
        write_day(&dir.0, "2026-08-31", b"held");
        let stub = Stub::start(200, "{}");
        let hold = Arc::new(UplinkHold::new(true));
        let state = ArchiveState::new(true);
        let stop = Arc::new(AtomicBool::new(false));
        let cfg = cfg_for(&stub, &dir.0, hold.clone());
        let (s, t) = (state.clone(), stop.clone());
        let thread = std::thread::spawn(move || run(cfg, s, t));

        // Held: nothing leaves the box, but the backlog is still counted, so
        // the status block explains the silence rather than hiding it.
        assert!(wait_for(|| state.report().pending == 1));
        std::thread::sleep(Duration::from_millis(500));
        assert!(
            stub.requests().is_empty(),
            "a held recorder uploads nothing"
        );

        hold.set(false, trunkd_uplink::HoldSource::Panel);
        assert!(
            wait_for(|| !stub.requests().is_empty()),
            "releasing the hold releases the archive too"
        );
        stop.store(true, Ordering::Relaxed);
        let _ = thread.join();
    }

    #[test]
    fn turning_the_archive_on_at_runtime_ships_the_backlog_it_finds() {
        // keyup's `event_log_archive` is live: the thread runs whether or not
        // the feature is on, so switching it on ships the days that piled up
        // while it was off rather than skipping them.
        let dir = TempDir::new("archive-runtime");
        write_day(&dir.0, "2026-08-29", b"backlog-a");
        write_day(&dir.0, "2026-08-30", b"backlog-b");
        let stub = Stub::start(200, "{}");
        let state = ArchiveState::new(false);
        let stop = Arc::new(AtomicBool::new(false));
        let cfg = cfg_for(&stub, &dir.0, Arc::new(UplinkHold::new(false)));
        let (s, t) = (state.clone(), stop.clone());
        let thread = std::thread::spawn(move || run(cfg, s, t));

        // Off: the backlog is counted and reported, and nothing is uploaded or
        // marked — a day skipped here would be a day keyup never gets.
        assert!(wait_for(|| state.report().pending == 2));
        std::thread::sleep(Duration::from_millis(400));
        assert!(stub.requests().is_empty(), "archiving is off");
        assert_eq!(state.report().source, ArchiveSource::Config);

        assert!(state.set_archive(true, ArchiveSource::Keyup), "it moved");
        assert!(
            wait_for(|| state.report().pending == 0),
            "both pending days are shipped, oldest first"
        );
        stop.store(true, Ordering::Relaxed);
        let _ = thread.join();

        let report = state.report();
        assert!(report.archive);
        assert_eq!(report.source, ArchiveSource::Keyup, "keyup set it");
        assert_eq!(report.last_archived.as_deref(), Some("2026-08-30"));
        assert_eq!(stub.requests().len(), 2, "no day was skipped");
        assert_eq!(
            report.to_json()["source"],
            "keyup",
            "and the status block says who"
        );
    }

    #[test]
    fn turning_the_archive_off_stops_uploads_without_stopping_the_thread() {
        let dir = TempDir::new("archive-off");
        let stub = Stub::start(200, "{}");
        let state = ArchiveState::new(true);
        let stop = Arc::new(AtomicBool::new(false));
        let cfg = cfg_for(&stub, &dir.0, Arc::new(UplinkHold::new(false)));
        let (s, t) = (state.clone(), stop.clone());
        let thread = std::thread::spawn(move || run(cfg, s, t));

        assert!(state.set_archive(false, ArchiveSource::Keyup));
        write_day(&dir.0, "2026-08-31", b"not-yet");
        assert!(
            wait_for(|| state.report().pending == 1),
            "the thread is still scanning"
        );
        std::thread::sleep(Duration::from_millis(400));
        assert!(stub.requests().is_empty(), "but it uploads nothing");
        stop.store(true, Ordering::Relaxed);
        let _ = thread.join();
    }

    #[test]
    fn a_refusal_is_reported_and_the_day_stays_pending() {
        let dir = TempDir::new("archive-fail");
        write_day(&dir.0, "2026-08-31", b"nope");
        let stub = Stub::start(500, "boom");
        let state = ArchiveState::new(true);
        let stop = Arc::new(AtomicBool::new(false));
        let cfg = cfg_for(&stub, &dir.0, Arc::new(UplinkHold::new(false)));
        let (s, t) = (state.clone(), stop.clone());
        let thread = std::thread::spawn(move || run(cfg, s, t));

        assert!(wait_for(|| state.report().last_error.is_some()));
        stop.store(true, Ordering::Relaxed);
        let _ = thread.join();

        assert!(!marker_for(&dir.0.join("2026-08-31.jsonl.gz")).exists());
        assert_eq!(state.report().pending, 1, "still owed");
        assert_eq!(state.report().last_archived, None);
        // And the retry is backed off rather than hammered: one attempt in the
        // window above, not one per pass.
        assert_eq!(stub.requests().len(), 1);
    }
}
