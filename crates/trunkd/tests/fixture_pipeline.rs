// SPDX-License-Identifier: GPL-3.0-or-later
//! End-to-end replay of a real wideband capture through the whole recorder.
//!
//! One `run_once` over the 60 s Example Network capture with a `backend =
//! "iqfile"` device, against a mock Keyup on `127.0.0.1:0` — the same
//! `TcpListener` mock `trunkd-uplink`'s own tests use, extended to answer the
//! observations route. Nothing here touches a real Keyup or any
//! hardware.
//!
//! What it proves, which no unit test can: that the SDR ring, the channelizer,
//! the control hunt, the traffic pool, the IMBE vocoder, call assembly, MP3
//! encoding and the upload contract all agree with each other on real off-air
//! signal — and that what the recorder tells Keyup about the site (its band
//! plan, its traffic frequencies, who keyed each call) holds no ghost.
//!
//! Both tests run in every `cargo t`: the workspace's test profile optimizes
//! the signal-processing crates, so each replay takes well under a minute with
//! debug assertions on. The fixture is 2.4 GB and gitignored; where it is
//! absent the build script sets `trunkd_fixture_missing` and both tests are
//! reported **ignored**, naming the fixture — never a silent pass.
//!
//! ```text
//! cargo t -p trunkd --test fixture_pipeline -- --nocapture
//! ```
//!
//! Point `TRUNKD_FIXTURES` at the corpus if it does not sit at the workspace
//! root (how to capture one: `fixtures/README.md`).

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use trunkd::config::{
    AudioFormatName, Backend, Config, Device, PacingName, Recorder, Site, Uplink,
};

/// The 60 s capture: centre 855.5 MHz, 20 Msps, signed 8-bit interleaved.
const FIXTURE: &str = "wb855500k_20M_int8_run2.iq";
const RATE: u32 = 20_000_000;
const CENTER_HZ: u64 = 855_500_000;

/// Every control candidate of this site. 859.9875 MHz was carrying a voice
/// call during the capture: the hunt must not be fooled by it.
const CANDIDATES: [u64; 11] = [
    851_050_000,
    851_787_500,
    852_137_500,
    852_537_500,
    852_825_000,
    853_112_500,
    854_237_500,
    854_437_500,
    856_712_500,
    858_987_500,
    859_987_500,
];

/// The site's band plan as `(id, base, spacing, offset, bandwidth)`: every
/// identifier update in the capture decodes to one of these two (47 times
/// each), so an observation naming anything else published a ghost.
const SITE_BANDS: [(u64, u64, u64, i64, u64); 2] = [
    (0, 769_006_250, 12_500, 30_000_000, 12_500),
    (1, 851_012_500, 12_500, -45_000_000, 12_500),
];

/// Every frequency the capture carries voice on: the traffic table's ceiling.
const TRAFFIC: [u64; 9] = [
    851_787_500,
    852_137_500,
    852_825_000,
    853_112_500,
    854_237_500,
    854_437_500,
    856_712_500,
    858_987_500,
    859_987_500,
];

/// `(frequency, talkgroup)` of the capture's 15 calls, sorted — what either
/// replay must upload, however its tuners split the band.
const CALLS: [(u64, u16); 15] = [
    (851_787_500, 57011),
    (852_137_500, 52103),
    (852_137_500, 57012),
    (852_825_000, 52103),
    (852_825_000, 57151),
    (853_112_500, 52103),
    (853_112_500, 57011),
    (854_237_500, 57012),
    (854_437_500, 57011),
    (854_437_500, 57012),
    (856_712_500, 52103),
    (856_712_500, 57012),
    (858_987_500, 57011),
    (859_987_500, 52103),
    (859_987_500, 57151),
];

// ---------------------------------------------------------------------------
// mock Keyup
// ---------------------------------------------------------------------------

/// One request the mock received.
#[derive(Clone, Debug, Default)]
struct Req {
    path: String,
    authorization: Option<String>,
    fields: HashMap<String, String>,
    /// `(filename, content-type, bytes)` of the `audio` part, when sent.
    audio: Option<(String, String, Vec<u8>)>,
    /// Raw body, for the JSON routes.
    body: Vec<u8>,
}

impl Req {
    fn field(&self, name: &str) -> Option<&str> {
        self.fields.get(name).map(String::as_str)
    }
}

struct Mock {
    port: u16,
    requests: Arc<Mutex<Vec<Req>>>,
    stop: Arc<AtomicBool>,
}

impl Mock {
    fn start() -> Mock {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        listener.set_nonblocking(true).expect("nonblocking");

        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (r, s) = (requests.clone(), stop.clone());
        std::thread::spawn(move || {
            while !s.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let r = r.clone();
                        std::thread::spawn(move || serve(stream, r));
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    Err(_) => break,
                }
            }
        });
        Mock {
            port,
            requests,
            stop,
        }
    }

    fn upload_url(&self) -> String {
        format!("http://127.0.0.1:{}/api/call-upload", self.port)
    }

    fn requests(&self) -> Vec<Req> {
        self.requests.lock().expect("mock lock").clone()
    }
}

impl Drop for Mock {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn serve(mut stream: TcpStream, requests: Arc<Mutex<Vec<Req>>>) {
    // Windows hands an accepted socket the listener's non-blocking mode (Linux
    // does not); the read timeout below only means something in blocking mode.
    let _ = stream.set_nonblocking(false);
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("read timeout");
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];

    let head_end = loop {
        if let Some(i) = find(&buf, b"\r\n\r\n", 0) {
            break i;
        }
        match stream.read(&mut chunk) {
            Ok(0) => return,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(_) => return,
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let len: usize = header(&head, "content-length")
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0);
    let content_type = header(&head, "content-type").unwrap_or_default();

    while buf.len() < head_end + 4 + len {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(_) => break,
        }
    }
    let body = buf[head_end + 4..].to_vec();

    let path = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .unwrap_or_default()
        .to_owned();
    let mut req = if content_type.starts_with("multipart/") {
        parse_multipart(&content_type, &body)
    } else {
        Req::default()
    };
    req.path = path.clone();
    req.authorization = header(&head, "authorization");
    req.body = body;
    let req_path = path;
    let req_event_count = serde_json::from_slice::<serde_json::Value>(&req.body)
        .ok()
        .and_then(|v| v.get("events").and_then(|e| e.as_array().map(Vec::len)))
        .unwrap_or(0);
    requests.lock().expect("mock lock").push(req);

    // The events route answers Keyup's `{"stored": n}`; everything else gets
    // the Rdio-Scanner upload reply.
    let (ctype, text) = if req_path == "/api/recorder/events" {
        let n = req_event_count;
        ("application/json", format!("{{\"stored\": {n}}}"))
    } else {
        ("text/plain", "Call imported successfully.".to_owned())
    };
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{text}",
        text.len()
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
    let _ = stream.shutdown(std::net::Shutdown::Write);
}

fn header(head: &str, name: &str) -> Option<String> {
    let needle = format!("{name}:").to_ascii_lowercase();
    head.lines()
        .find(|l| l.to_ascii_lowercase().starts_with(&needle))
        .and_then(|l| l.split_once(':'))
        .map(|(_, v)| v.trim().to_owned())
}

fn find(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    (from..=haystack.len() - needle.len()).find(|&i| &haystack[i..i + needle.len()] == needle)
}

fn parse_multipart(content_type: &str, body: &[u8]) -> Req {
    let mut req = Req::default();
    let Some(boundary) = content_type.split("boundary=").nth(1) else {
        return req;
    };
    let sep = format!("--{}", boundary.trim()).into_bytes();

    let mut starts = Vec::new();
    let mut at = 0;
    while let Some(i) = find(body, &sep, at) {
        starts.push(i);
        at = i + sep.len();
    }
    for pair in starts.windows(2) {
        let raw = &body[pair[0] + sep.len()..pair[1]];
        let raw = raw.strip_prefix(b"\r\n").unwrap_or(raw);
        let raw = raw.strip_suffix(b"\r\n").unwrap_or(raw);
        let Some(split) = find(raw, b"\r\n\r\n", 0) else {
            continue;
        };
        let head = String::from_utf8_lossy(&raw[..split]).into_owned();
        let value = &raw[split + 4..];
        let name = quoted(&head, "name=\"").unwrap_or_default();
        if let Some(filename) = quoted(&head, "filename=\"") {
            let ctype = head
                .lines()
                .find(|l| l.to_ascii_lowercase().starts_with("content-type:"))
                .and_then(|l| l.split_once(':'))
                .map(|(_, v)| v.trim().to_owned())
                .unwrap_or_default();
            req.audio = Some((filename, ctype, value.to_vec()));
        } else {
            req.fields
                .insert(name, String::from_utf8_lossy(value).into_owned());
        }
    }
    req
}

fn quoted(head: &str, key: &str) -> Option<String> {
    head.split(key)
        .nth(1)
        .and_then(|s| s.split('"').next())
        .map(str::to_owned)
}

// ---------------------------------------------------------------------------
// harness
// ---------------------------------------------------------------------------

/// Locate the gitignored capture: in `$TRUNKD_FIXTURES`, then the workspace's
/// `fixtures/`, the file `$TRUNKD_FIXTURE_NAME` if set, else the first file
/// named [`FIXTURE`] or `<system prefix>_`[`FIXTURE`] — the same rule the build
/// script applies.
fn fixture_path() -> Option<PathBuf> {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut roots = Vec::new();
    if let Ok(dir) = std::env::var("TRUNKD_FIXTURES") {
        roots.push(PathBuf::from(dir));
    }
    roots.push(manifest.join("../../fixtures"));
    roots.iter().find_map(|r| find_fixture(r))
}

/// The capture in one corpus directory (see [`fixture_path`]).
fn find_fixture(root: &std::path::Path) -> Option<PathBuf> {
    if let Ok(name) = std::env::var("TRUNKD_FIXTURE_NAME") {
        let p = root.join(name);
        return p.is_file().then_some(p);
    }
    let suffix = format!("_{FIXTURE}");
    let mut hits: Vec<PathBuf> = std::fs::read_dir(root)
        .ok()?
        .filter_map(Result::ok)
        .filter(|e| {
            let name = e.file_name();
            let name = name.to_string_lossy();
            name == FIXTURE || name.ends_with(&suffix)
        })
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .collect();
    hits.sort();
    hits.into_iter().next()
}

/// The capture, which the build script found when it compiled this test.
fn fixture() -> PathBuf {
    fixture_path().unwrap_or_else(|| {
        panic!(
            "fixture {FIXTURE} not found although it was present at build time \
             (set TRUNKD_FIXTURES, see fixtures/README.md)"
        )
    })
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

/// A `[[device]]` with only the fields this test sets; the rest come from the
/// TOML defaults, which is also a check that they are all optional.
fn device(path: &Path) -> Device {
    let mut d: Device = toml::from_str(
        r#"
        id = "replay0"
        sample_rate = 20000000
        "#,
    )
    .expect("device defaults");
    d.backend = Backend::Iqfile;
    d.path = Some(path.display().to_string());
    d.sample_rate = RATE;
    d.center_frequency = Some(CENTER_HZ);
    // The whole point of an offline replay: decode as fast as the disk allows.
    d.pacing = PacingName::Unthrottled;
    d
}

fn site() -> Site {
    let mut s: Site = toml::from_str(
        r#"
        system = "Example Network"
        site = "Central"
        device = "replay0"
        control_frequencies = []
        "#,
    )
    .expect("site defaults");
    s.control_frequencies = CANDIDATES.to_vec();
    s.traffic_pool_size = 20;
    s
}

/// True if `bytes` starts with an MPEG audio frame sync (11 set bits) in the
/// first few bytes — the cheapest honest check that this is really MP3 and not
/// a WAV or an empty part.
fn looks_like_mp3(bytes: &[u8]) -> bool {
    bytes
        .windows(2)
        .take(64)
        .any(|w| w[0] == 0xFF && (w[1] & 0xE0) == 0xE0)
}

/// `(frequency, talkgroup)` of every upload, sorted.
fn call_keys(uploads: &[&Req]) -> Vec<(u64, u16)> {
    let mut keys: Vec<(u64, u16)> = uploads
        .iter()
        .map(|r| {
            (
                r.field("frequency")
                    .and_then(|f| f.parse().ok())
                    .expect("frequency"),
                r.field("talkgroup")
                    .and_then(|t| t.parse().ok())
                    .expect("talkgroup"),
            )
        })
        .collect();
    keys.sort_unstable();
    keys
}

/// No call names a radio that keyed a *different* call on its frequency.
///
/// Every radio in this capture keys exactly one talkgroup, so a radio listed
/// under two talkgroups is a caller carried over from the previous call on a
/// re-used frequency — which is what a new call heard first as a grant update
/// used to inherit (review 2026-09-22, D3): 852.825 MHz carries radio
/// 5715173's talkgroup-57151 call, then a talkgroup-52103 call opened by an
/// update.
fn assert_callers_are_their_own(uploads: &[&Req]) {
    let mut talkgroups_of: HashMap<u64, Vec<String>> = HashMap::new();
    for r in uploads {
        let tg = r.field("talkgroup").expect("talkgroup").to_owned();
        let sources: serde_json::Value =
            serde_json::from_str(r.field("sources").expect("sources")).expect("sources is JSON");
        for s in sources.as_array().expect("sources array") {
            let src = s["src"].as_u64().expect("src");
            if src != 0 {
                let tgs = talkgroups_of.entry(src).or_default();
                if !tgs.contains(&tg) {
                    tgs.push(tg.clone());
                }
            }
        }
    }
    for (src, tgs) in &talkgroups_of {
        assert_eq!(
            tgs.len(),
            1,
            "radio {src} is named as the caller of calls on talkgroups {tgs:?}"
        );
    }
    // T-B7: radio 0 means "not named yet". It is never listed in `sources`
    // (Keyup takes `sources[0]` as the caller), and a call whose radio is named
    // at any point uploads that radio as `source`; a call never named uploads
    // no `source` at all rather than a 0.
    for r in uploads {
        let sources: serde_json::Value =
            serde_json::from_str(r.field("sources").expect("sources")).expect("sources is JSON");
        let srcs: Vec<u64> = sources
            .as_array()
            .expect("sources array")
            .iter()
            .map(|s| s["src"].as_u64().expect("src"))
            .collect();
        let call = (r.field("frequency"), r.field("talkgroup"));
        assert!(
            !srcs.contains(&0),
            "call {call:?} lists radio 0 in its sources: {srcs:?}"
        );
        let first = srcs.first().map(u64::to_string);
        assert_eq!(
            r.field("source"),
            first.as_deref(),
            "call {call:?} uploads its first named radio as the caller: {srcs:?}"
        );
    }
    let on_852825: Vec<(Option<&str>, Option<&str>)> = uploads
        .iter()
        .filter(|r| r.field("frequency") == Some("852825000"))
        .map(|r| (r.field("talkgroup"), r.field("source")))
        .collect();
    assert!(
        on_852825.contains(&(Some("52103"), Some("5200002")))
            && on_852825.contains(&(Some("57151"), Some("5715173"))),
        "each call on 852.825 MHz names its own radio: {on_852825:?}"
    );
}

// ---------------------------------------------------------------------------
// the test
// ---------------------------------------------------------------------------

#[test]
#[cfg_attr(
    trunkd_fixture_missing,
    ignore = "needs fixture *_wb855500k_20M_int8_run2.iq (2.4 GB, gitignored): \
              put it in fixtures/ or set TRUNKD_FIXTURES (and TRUNKD_FIXTURE_NAME), then rebuild — see fixtures/README.md"
)]
fn records_the_reference_capture_and_uploads_it() {
    let path = fixture();

    let mock = Mock::start();
    let spool = TempDir::new("fixture-spool");
    let events_dir = TempDir::new("fixture-events");
    let cfg = Config {
        uplink: Uplink {
            url: mock.upload_url(),
            api_key: "kup_test".into(),
            spool_path: spool.0.display().to_string(),
            system_label: "Example Network".into(),
            site_label: "Central".into(),
            audio_format: AudioFormatName::Mp3,
            mp3_bitrate_kbps: 16,
            // The capture is 60 s of stream time replayed in a fraction of
            // that, so a wall-clock interval this short is what makes at least
            // one observations POST land mid-run.
            observations_interval_s: 2,
            ..Uplink::default()
        },
        recorder: Recorder {
            event_log_dir: events_dir.0.display().to_string(),
            // The replay is far shorter than a day, so nothing rotates; the
            // point here is that the file exists and carries several types.
            event_log_keep_days: 14,
            // Named so the uploads' `sourceQuality.instance` is assertable
            // rather than being whatever this machine's hostname happens to be.
            instance_id: "fixture-recorder".to_owned(),
            ..Recorder::default()
        },
        device: vec![device(&path)],
        site: vec![site()],
        ..Config::default()
    };
    cfg.validate().expect("the fixture config is valid");

    let stop = Arc::new(AtomicBool::new(false));
    let summary = trunkd::run_once(&cfg, stop).expect("the recorder runs to the end of the file");

    let requests = mock.requests();
    let uploads: Vec<&Req> = requests
        .iter()
        .filter(|r| r.path == "/api/call-upload")
        .collect();
    let observations: Vec<&Req> = requests
        .iter()
        .filter(|r| r.path == "/api/recorder/observations")
        .collect();
    let event_batches: Vec<&Req> = requests
        .iter()
        .filter(|r| r.path == "/api/recorder/events")
        .collect();

    eprintln!(
        "calls started={} ended={} dropped={} voice_frames={} uploads={} observations={} \
         uploaded={} failed={} spooled={} overflow={}",
        summary.calls_started,
        summary.calls_ended,
        summary.calls_dropped,
        summary.voice_frames,
        uploads.len(),
        observations.len(),
        summary.uplink.uploaded,
        summary.uplink.failed,
        summary.uplink.spooled,
        summary.overflow_samples,
    );

    // -- calls reached Keyup ------------------------------------------------
    assert!(
        uploads.len() >= 10,
        "expected at least 10 uploads from the capture's 15 calls, got {}",
        uploads.len()
    );
    assert_eq!(
        summary.uplink.uploaded as usize,
        uploads.len(),
        "every upload the mock saw was counted as stored"
    );
    assert_eq!(summary.uplink.failed, 0, "no call was rejected");
    assert_eq!(
        summary.uplink.spooled, 0,
        "the mock answered every call, so nothing should have spooled"
    );
    assert!(
        summary.voice_frames > 1_500,
        "expected the capture's voice, got {} frames",
        summary.voice_frames
    );

    // -- every call started was ended, uploaded, and nothing was lost -------
    // The call thread kept up with an unthrottled replay, so the orphan sweep
    // (1a43715) had nothing to close: a call it closes here is one whose
    // `Ended` went missing between the engine and the uploader.
    assert_eq!(summary.calls_started, 15, "the capture's 15 calls started");
    assert_eq!(summary.calls_ended, 15, "and all of them ended");
    assert_eq!(summary.call_queue_dropped, 0, "no call message was evicted");
    assert_eq!(summary.calls_orphaned, 0, "no call was closed as orphaned");
    assert_eq!(
        summary.overflow_samples, 0,
        "the file source never overflows"
    );
    assert_eq!(uploads.len(), 15, "one upload per call");
    assert_eq!(
        call_keys(&uploads),
        CALLS.to_vec(),
        "each call on its frequency and talkgroup"
    );
    assert_callers_are_their_own(&uploads);

    // -- the contract fields ------------------------------------------------
    for r in &uploads {
        assert_eq!(r.field("key"), Some("kup_test"));
        assert_eq!(r.field("systemLabel"), Some("Example Network"));
        assert_eq!(r.field("audioType"), Some("audio/mpeg"));
        assert!(r.field("dateTime").is_some(), "every call is timestamped");
        assert!(
            r.field("frequency").and_then(|f| f.parse::<u64>().ok()) > Some(0),
            "every call names the frequency it rode on"
        );
        // Sending `sources`/`frequencies` is what selects Keyup's
        // trunk-recorder dialect.
        assert!(r.field("frequencies").is_some());
        // Every call names the recorder that heard it and what its vocoder made
        // of the voice — the facts keyup needs to choose between two recorders'
        // copies of one call.
        let quality: serde_json::Value = serde_json::from_str(
            r.field("sourceQuality")
                .expect("every call reports its source quality"),
        )
        .expect("sourceQuality is JSON");
        assert_eq!(quality["instance"], "fixture-recorder");
        let frames = quality["voice_frames"].as_u64().expect("voice_frames");
        let bad = quality["bad_frames"].as_u64().expect("bad_frames");
        assert!(
            bad <= frames,
            "bad frames are a subset of the frames decoded: {quality}"
        );
        // A clear call reached this assertion only by carrying audio, so it
        // decoded voice; an encrypted one honestly reports none.
        if r.field("encrypted") == Some("0") {
            assert!(frames > 0, "a clear call decoded voice: {quality}");
        }
        // The capture is a real off-air recording, so the device measured a
        // level — and a real one is below full scale.
        let rssi = quality["rssi_dbfs"].as_f64().expect("rssi_dbfs");
        assert!(rssi < 0.0, "a wideband level is below full scale: {rssi}");
    }

    // -- clear calls carry real MP3 -----------------------------------------
    let clear: Vec<&&Req> = uploads
        .iter()
        .filter(|r| r.field("encrypted") != Some("1"))
        .collect();
    let encrypted: Vec<&&Req> = uploads
        .iter()
        .filter(|r| r.field("encrypted") == Some("1"))
        .collect();
    assert!(!clear.is_empty(), "the capture carries clear calls");
    assert!(!encrypted.is_empty(), "the capture carries encrypted calls");

    for r in &clear {
        let (name, ctype, bytes) = r.audio.as_ref().expect("a clear call carries audio");
        assert!(name.ends_with(".mp3"), "audio is named {name}");
        assert_eq!(ctype, "audio/mpeg");
        assert!(
            looks_like_mp3(bytes),
            "{name}: {} bytes with no MPEG frame sync",
            bytes.len()
        );
    }
    assert!(
        clear
            .iter()
            .any(|r| r.audio.as_ref().is_some_and(|a| a.2.len() > 2_000)),
        "at least one substantial clear call (the capture's longest is ~3 s of MP3)"
    );

    // -- encrypted calls are metadata only ----------------------------------
    // P25 encrypts only the IMBE payload, so the call is still fully described.
    for r in &encrypted {
        assert!(
            r.audio.is_none(),
            "an encrypted call must not carry an audio part"
        );
        assert_eq!(r.field("encrypted"), Some("1"));
        assert!(
            r.field("talkgroup").is_some(),
            "encryption hides the voice, never the signaling"
        );
    }
    assert!(
        encrypted
            .iter()
            .any(|r| r.field("algorithm") == Some("AES-256")),
        "the site's encrypted traffic is AES-256"
    );

    // -- site observations --------------------------------------------------
    assert!(
        !observations.is_empty(),
        "expected at least one observations POST"
    );
    assert!(
        summary.observations_posted > 0,
        "the observations counter must agree with the mock"
    );
    let obs = observations.first().expect("an observation");
    assert_eq!(
        obs.authorization.as_deref(),
        Some("Bearer kup_test"),
        "observations are bearer-authenticated, unlike the uploads"
    );
    let body: serde_json::Value =
        serde_json::from_slice(&obs.body).expect("the observations body is JSON");
    assert_eq!(body["recorder"], "trunkd");
    assert_eq!(body["system"]["label"], "Example Network");
    assert_eq!(body["site"]["label"], "Central");
    // NAC 0x034, the site the capture was made on.
    assert_eq!(body["system"]["nac"], 0x034);
    assert!(
        body["control"]["frequency_hz"] == 851_050_000u64
            || body["configured_control"]
                .as_array()
                .is_some_and(|a| a.len() == CANDIDATES.len()),
        "the report names the control channel it heard: {}",
        body["control"]
    );
    assert!(
        body["observed_at"]
            .as_str()
            .is_some_and(|s| s.ends_with('Z')),
        "observed_at is RFC 3339 UTC"
    );

    // -- the site inventory Keyup is sent: no ghost bands or traffic --------
    // Every window, not just the last: a ghost published once is on Keyup's
    // site map. The inventory is fed each IDEN block twice (the `Tsbk` and
    // the trunking layer's `IdenUpdate`) and must count it once (T-B2).
    for (i, body) in observations
        .iter()
        .filter_map(|r| serde_json::from_slice::<serde_json::Value>(&r.body).ok())
        .enumerate()
    {
        for band in body["bands"].as_array().expect("bands array") {
            let key = (
                band["id"].as_u64().expect("id"),
                band["base_hz"].as_u64().expect("base_hz"),
                band["spacing_hz"].as_u64().expect("spacing_hz"),
                band["offset_hz"].as_i64().expect("offset_hz"),
                band["bandwidth_hz"].as_u64().expect("bandwidth_hz"),
            );
            assert!(
                SITE_BANDS.contains(&key) && band["tdma"] == false,
                "observation {i} publishes a band the site does not have: {band}"
            );
        }
        for t in body["traffic"].as_array().expect("traffic array") {
            let hz = t["frequency_hz"].as_u64().expect("frequency_hz");
            assert!(
                TRAFFIC.contains(&hz),
                "observation {i} publishes traffic on {hz} Hz, which the capture never used"
            );
        }
    }
    let last: serde_json::Value =
        serde_json::from_slice(&observations.last().expect("an observation").body)
            .expect("the last observations body is JSON");
    let bands: Vec<u64> = last["bands"]
        .as_array()
        .expect("bands")
        .iter()
        .filter_map(|b| b["id"].as_u64())
        .collect();
    assert_eq!(
        bands,
        vec![0, 1],
        "the last window names both of the site's bands"
    );
    let mut traffic: Vec<u64> = last["traffic"]
        .as_array()
        .expect("traffic")
        .iter()
        .filter_map(|t| t["frequency_hz"].as_u64())
        .collect();
    traffic.sort_unstable();
    assert_eq!(
        traffic,
        TRAFFIC.to_vec(),
        "by the end every voice frequency has been granted, and nothing else"
    );

    // -- radio events -------------------------------------------------------
    // 60 s of this control channel carries a handful of registration and
    // affiliation responses (docs/NON-VOICE-DATA.md §3.1 counts 12 unit
    // registrations and 7 affiliation responses across the full 150 s of both
    // wideband runs).
    assert!(
        !event_batches.is_empty(),
        "expected at least one radio-events POST"
    );
    assert_eq!(
        event_batches[0].authorization.as_deref(),
        Some("Bearer kup_test"),
        "radio events are bearer-authenticated"
    );
    assert_eq!(
        summary.radio_events_spooled, 0,
        "the mock accepted every batch, so nothing should have spooled"
    );

    let mut by_kind: HashMap<String, usize> = HashMap::new();
    for batch in &event_batches {
        let b: serde_json::Value =
            serde_json::from_slice(&batch.body).expect("the events body is JSON");
        assert_eq!(b["recorder"], "trunkd");
        assert_eq!(b["system"]["label"], "Example Network");
        assert_eq!(b["site"]["label"], "Central");
        for e in b["events"].as_array().expect("events array") {
            assert!(
                e["at"].as_str().is_some_and(|s| s.ends_with('Z')),
                "every event is stamped RFC 3339 UTC: {e}"
            );
            assert!(e["opcode"].is_number(), "every event names its opcode: {e}");
            *by_kind
                .entry(e["kind"].as_str().expect("kind").to_owned())
                .or_default() += 1;
        }
    }
    let mut kinds: Vec<(&String, &usize)> = by_kind.iter().collect();
    kinds.sort();
    eprintln!(
        "radio events by kind: {kinds:?} in {} batches",
        event_batches.len()
    );
    assert_eq!(
        summary.radio_events_sent as usize,
        by_kind.values().sum::<usize>(),
        "the sent counter agrees with what the mock received"
    );
    for kind in ["unit_registration", "group_affiliation"] {
        assert!(
            by_kind.contains_key(kind),
            "the capture's control channel carries {kind}; got {kinds:?}"
        );
    }

    // -- the window histogram rides the observations body -------------------
    // Every window carries its own mix; an early one can be posted before the
    // control channel is acquired, so assert on a window that heard something
    // rather than on whichever happened to be first.
    let bodies: Vec<serde_json::Value> = observations
        .iter()
        .filter_map(|r| serde_json::from_slice(&r.body).ok())
        .collect();
    let body = bodies
        .iter()
        .find(|b| {
            b["message_mix"]
                .as_object()
                .is_some_and(|m| m.contains_key("164:15"))
        })
        .unwrap_or_else(|| {
            panic!(
                "no observation window counted the Harris beacon; mixes were {:?}",
                bodies.iter().map(|b| &b["message_mix"]).collect::<Vec<_>>()
            )
        });
    let mix = &body["message_mix"];
    assert!(
        mix.is_object() && !mix.as_object().expect("object").is_empty(),
        "the observations body carries a message mix: {mix}"
    );
    // A quarter of every TSBK on this site is one of two L3Harris beacons
    // (§3.1); both must be counted, and both must appear as exemplars, since
    // neither is a message the engine parses.
    for key in ["164:1", "164:15"] {
        assert!(
            mix[key].as_u64().unwrap_or(0) > 0,
            "the Harris beacon {key} must be counted: {mix}"
        );
    }
    let exemplars = body["unknown_exemplars"]
        .as_array()
        .expect("unknown_exemplars array");
    assert!(
        exemplars
            .iter()
            .any(|e| e["mfid"] == 164 && e["opcode"] == 15),
        "an unparsed vendor opcode keeps one raw exemplar: {exemplars:?}"
    );
    assert!(
        body["data_channel"]["grants"].is_number(),
        "the data-channel counters are present: {}",
        body["data_channel"]
    );
    assert!(body["lsd_nonzero"].is_number());
    assert!(
        body["link_control_mix"].is_object(),
        "traffic-channel link control is counted too"
    );

    // -- the raw event log --------------------------------------------------
    let logs: Vec<PathBuf> = std::fs::read_dir(&events_dir.0)
        .expect("the event log directory exists")
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "jsonl"))
        .collect();
    assert_eq!(logs.len(), 1, "one day of replay is one log file: {logs:?}");
    let log = std::fs::read_to_string(&logs[0]).expect("read the event log");
    let mut log_types: HashMap<String, usize> = HashMap::new();
    // A constant broadcast heard again within a minute is a back-reference,
    // `{"at","rep":id}`, to the nearest preceding full copy carrying `id`.
    let mut full_copies: HashMap<u64, String> = HashMap::new();
    let mut repeats = 0usize;
    for line in log.lines() {
        let v: serde_json::Value =
            serde_json::from_str(line).unwrap_or_else(|e| panic!("bad log line {line}: {e}"));
        let ty = if let Some(id) = v["rep"].as_u64() {
            repeats += 1;
            assert_eq!(v.as_object().map(|o| o.len()), Some(2), "{line}");
            full_copies
                .get(&id)
                .unwrap_or_else(|| panic!("a repeat names a full copy before it: {line}"))
                .clone()
        } else {
            let ty = v["type"].as_str().expect("type").to_owned();
            if let Some(id) = v["id"].as_u64() {
                assert!(
                    line.starts_with(&format!(r#"{{"at":{},"id":{id},"#, v["at"])),
                    "the id follows at: {line}"
                );
                full_copies.insert(id, ty.clone());
            }
            ty
        };
        *log_types.entry(ty).or_default() += 1;
    }
    let mut types: Vec<(&String, &usize)> = log_types.iter().collect();
    types.sort();
    eprintln!(
        "event log: {} lines ({repeats} repeats), types {types:?}",
        log.lines().count()
    );
    assert!(
        repeats > 0,
        "the site's constant broadcasts repeat within a minute on any replay"
    );
    assert!(
        log_types.len() >= 3,
        "the log carries several event types, got {types:?}"
    );
    for t in ["tsbk", "lc"] {
        assert!(
            log_types.contains_key(t),
            "the log must carry {t} lines: {types:?}"
        );
    }
    assert_eq!(
        summary.event_log_dropped, 0,
        "nothing should have been dropped on a replay this size"
    );
    // The invariant that matters most: voice never reaches the log.
    assert!(
        !log_types.contains_key("imbe") && !log.contains("\"imbe\""),
        "IMBE voice frames must never be logged"
    );

    // -- a roaming talker's SUID reaches the call upload --------------------
    // LC 9 decoded to 11 distinct fully qualified subscribers across the two
    // wideband runs (§3.2), so at least one call here should carry one.
    let with_suid: Vec<&&Req> = uploads
        .iter()
        .filter(|r| r.field("sourceWacn").is_some())
        .collect();
    eprintln!(
        "calls carrying a Source ID Extension: {}/{}",
        with_suid.len(),
        uploads.len()
    );
    assert!(
        !with_suid.is_empty(),
        "at least one call's talker was qualified by an LC 9 Source ID Extension"
    );
    for r in &with_suid {
        assert_eq!(
            r.field("sourceWacn"),
            Some("284004"),
            "the site's WACN, as the network-status broadcast reports it"
        );
        assert!(
            r.field("sourceSystem")
                .and_then(|s| s.parse::<u16>().ok())
                .is_some_and(|s| s > 0),
            "a qualified talker names its home system too"
        );
    }
}

// ---------------------------------------------------------------------------
// multi-tuner core (W1): cross-tuner grant routing over one capture
// ---------------------------------------------------------------------------

/// A `[[device]]` reading the fixture with a role and band partition.
fn partitioned_device(
    path: &Path,
    id: &str,
    role: trunkd::config::DeviceRole,
    bands: &[&str],
) -> Device {
    let mut d = device(path);
    d.id = id.to_owned();
    d.role = role;
    d.bands = bands.iter().map(|b| (*b).to_owned()).collect();
    d
}

/// Two tuners over the *same* capture with complementary band partitions: a
/// control/auto front owning the lower half, a voice front owning the upper
/// half. A grant on an upper-half frequency is outside the control front's band
/// and can only be recorded by the voice front — cross-routing — while a
/// lower-half grant stays local. Also exercises the per-tuner epoch: both
/// timelines advance independently over one file.
#[test]
#[cfg_attr(
    trunkd_fixture_missing,
    ignore = "needs fixture *_wb855500k_20M_int8_run2.iq (2.4 GB, gitignored): \
              put it in fixtures/ or set TRUNKD_FIXTURES (and TRUNKD_FIXTURE_NAME), then rebuild — see fixtures/README.md"
)]
fn cross_routes_grants_between_two_tuners_over_one_capture() {
    let path = fixture();

    // The partition boundary sits between the capture's lower- and upper-half
    // traffic. Control (851.05) and the lower calls fall to the auto front; the
    // upper calls (856.7125, 858.9875, 859.9875) fall to the voice front.
    const BOUNDARY: u64 = 855_000_000;

    let mock = Mock::start();
    let spool = TempDir::new("cross-spool");
    let ctrl = partitioned_device(
        &path,
        "ctrl",
        trunkd::config::DeviceRole::Auto,
        &["845000000-855000000"],
    );
    let voice = partitioned_device(
        &path,
        "voice0",
        trunkd::config::DeviceRole::Voice,
        &["855000000-866000000"],
    );
    let mut site = site();
    site.device = String::new();
    site.devices = vec!["ctrl".into(), "voice0".into()];

    let cfg = Config {
        uplink: Uplink {
            url: mock.upload_url(),
            api_key: "kup_test".into(),
            spool_path: spool.0.display().to_string(),
            system_label: "Example Network".into(),
            site_label: "Central".into(),
            audio_format: AudioFormatName::Mp3,
            mp3_bitrate_kbps: 16,
            ..Uplink::default()
        },
        device: vec![ctrl, voice],
        site: vec![site],
        ..Config::default()
    };
    cfg.validate().expect("the two-tuner config is valid");

    let stop = Arc::new(AtomicBool::new(false));
    let summary = trunkd::run_once(&cfg, stop).expect("the recorder runs to the end of the file");

    let requests = mock.requests();
    let freqs: Vec<u64> = requests
        .iter()
        .filter(|r| r.path == "/api/call-upload")
        .filter_map(|r| r.field("frequency").and_then(|f| f.parse::<u64>().ok()))
        .collect();

    let uploads: Vec<&Req> = requests
        .iter()
        .filter(|r| r.path == "/api/call-upload")
        .collect();
    let lower = freqs.iter().filter(|&&f| f < BOUNDARY).count();
    let upper = freqs.iter().filter(|&&f| f >= BOUNDARY).count();
    eprintln!(
        "cross-routing: {} uploads, {lower} lower-band (local), {upper} upper-band (cross-routed), \
         calls_started={}",
        freqs.len(),
        summary.calls_started,
    );

    assert!(
        lower > 0,
        "the control/auto front records its own in-band grants locally"
    );
    assert!(
        upper > 0,
        "an upper-band grant is outside the control front's band and can only \
         reach Keyup if the coordinator routed it to the voice front"
    );

    // Split across two tuners, the capture is still exactly its 15 calls —
    // 10 below the boundary, 5 above — each recorded once, none lost between
    // a tuner and the call thread, none closed as orphaned.
    assert_eq!((lower, upper), (10, 5), "calls per side of the partition");
    assert_eq!(
        call_keys(&uploads),
        CALLS.to_vec(),
        "each call on its frequency and talkgroup, once"
    );
    assert_eq!(summary.calls_started, 15);
    assert_eq!(summary.calls_ended, 15);
    assert_eq!(summary.call_queue_dropped, 0);
    assert_eq!(summary.calls_orphaned, 0);
    assert_eq!(summary.uplink.uploaded, 15);
    // The voice front learns the band plan from the control front's tracker
    // (which only confirmed plans reach) and resolves no grant of its own, so
    // a cross-routed call is attributed exactly as a local one.
    assert_callers_are_their_own(&uploads);
}
