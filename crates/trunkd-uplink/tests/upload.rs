// SPDX-License-Identifier: GPL-3.0-or-later
//! End-to-end tests against a mock `POST /api/call-upload`.
//!
//! The mock is a plain [`TcpListener`] on `127.0.0.1:0` that parses the
//! multipart body and answers with whichever of Keyup's literal replies the
//! test asks for. Nothing here touches a real Keyup.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use trunkd_call::{AudioFormat, CallBuilder, FinishedCall};
use trunkd_core::Metrics;
use trunkd_uplink::{KeyupUplink, Submitted, UplinkConfig};

// ---------------------------------------------------------------------------
// mock server
// ---------------------------------------------------------------------------

/// One parsed upload request.
#[derive(Clone, Debug, Default)]
struct Req {
    fields: HashMap<String, String>,
    /// `(filename, content-type, bytes)` of the `audio` part, when sent.
    audio: Option<(String, String, Vec<u8>)>,
}

impl Req {
    fn field(&self, name: &str) -> Option<&str> {
        self.fields.get(name).map(String::as_str)
    }
}

struct Mock {
    port: u16,
    requests: Arc<Mutex<Vec<Req>>>,
    reply: Arc<Mutex<(u16, String)>>,
    delay: Arc<Mutex<Duration>>,
    stop: Arc<AtomicBool>,
}

impl Mock {
    fn start() -> Mock {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        listener.set_nonblocking(true).expect("nonblocking");

        let requests = Arc::new(Mutex::new(Vec::new()));
        let reply = Arc::new(Mutex::new((
            200u16,
            "Call imported successfully.".to_owned(),
        )));
        let delay = Arc::new(Mutex::new(Duration::ZERO));
        let stop = Arc::new(AtomicBool::new(false));

        let (r, p, d, s) = (requests.clone(), reply.clone(), delay.clone(), stop.clone());
        std::thread::spawn(move || {
            while !s.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let (r, p, d) = (r.clone(), p.clone(), d.clone());
                        std::thread::spawn(move || serve(stream, r, p, d));
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
            reply,
            delay,
            stop,
        }
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}/api/call-upload", self.port)
    }

    fn set_reply(&self, status: u16, body: &str) {
        *self.reply.lock().unwrap() = (status, body.to_owned());
    }

    fn set_delay(&self, d: Duration) {
        *self.delay.lock().unwrap() = d;
    }

    fn requests(&self) -> Vec<Req> {
        self.requests.lock().unwrap().clone()
    }

    fn count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}

impl Drop for Mock {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn serve(
    mut stream: TcpStream,
    requests: Arc<Mutex<Vec<Req>>>,
    reply: Arc<Mutex<(u16, String)>>,
    delay: Arc<Mutex<Duration>>,
) {
    // Windows hands an accepted socket the listener's non-blocking mode (Linux
    // does not); the read timeout below only means something in blocking mode.
    let _ = stream.set_nonblocking(false);
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("read timeout");
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];

    // Headers first.
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

    // Then exactly Content-Length bytes of body.
    while buf.len() < head_end + 4 + len {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(_) => break,
        }
    }
    let body = &buf[head_end + 4..];
    requests
        .lock()
        .unwrap()
        .push(parse_multipart(&content_type, body));

    let wait = *delay.lock().unwrap();
    if !wait.is_zero() {
        std::thread::sleep(wait);
    }

    let (status, text) = reply.lock().unwrap().clone();
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/plain\r\nContent-Length: {n}\r\nConnection: close\r\n\r\n{text}",
        reason = if status == 200 { "OK" } else { "Error" },
        n = text.len(),
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
    let _ = stream.shutdown(std::net::Shutdown::Write);
}

fn header(head: &str, name: &str) -> Option<String> {
    head.lines()
        .find(|l| {
            l.to_ascii_lowercase()
                .starts_with(&format!("{name}:").to_ascii_lowercase())
        })
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

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let mut p = std::env::temp_dir();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        p.push(format!(
            "trunkd-uplink-{tag}-{}-{nanos}",
            std::process::id()
        ));
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

fn config(mock: &Mock, spool: &TempDir) -> UplinkConfig {
    UplinkConfig {
        url: mock.url(),
        api_key: "kup_test".into(),
        system_id: 7,
        spool_dir: spool.0.clone(),
        spool_cap_bytes: 1 << 20,
        // The floor is off in tests: eviction must depend on what the test
        // spooled, not on how full the machine running it happens to be.
        spool_min_free_bytes: 0,
        timeout: Duration::from_secs(2),
        queue_capacity: 8,
        drain_interval: Duration::from_millis(50),
        auth_backoff: Duration::from_secs(3),
        audio_format: AudioFormat::default(),
        priority: "live".into(),
        // Not held: these tests are about the network path.
        hold: std::sync::Arc::new(trunkd_uplink::UplinkHold::new(false)),
        // Off: keyup has pushed no block, so no call is ever offered to
        // Broadcastify and no `broadcastify*` field reaches the Keyup body.
        broadcastify: trunkd_uplink::BroadcastifyState::new(false, &trunkd_core::Metrics::new()),
        instance_id: "rec-test".into(),
    }
}

fn tone(seconds: f64) -> Vec<i16> {
    let n = (seconds * 8000.0) as usize;
    (0..n)
        .map(|i| {
            let t = i as f64 / 8000.0;
            (6000.0 * (2.0 * std::f64::consts::PI * 700.0 * t).sin()) as i16
        })
        .collect()
}

fn voice_call(started_at_ms: i64) -> FinishedCall {
    let mut b = CallBuilder::new(
        "Example Network",
        "Site 1",
        started_at_ms,
        852_137_500,
        52101,
    );
    b.set_talkgroup_label(Some("PD DISPATCH".into()));
    b.set_nac(Some(659));
    b.set_patches(vec![19282, 50051]);
    b.set_talker_alias(Some("ENGINE 3".into()));
    b.talker(9_000_011, started_at_ms, false, Some("UNIT_313".into()));
    b.push_pcm(&tone(1.0));
    b.talker(9_000_012, started_at_ms + 1000, true, None);
    b.push_pcm(&tone(1.0));
    b.finish(started_at_ms + 2000)
}

fn encrypted_call(started_at_ms: i64) -> FinishedCall {
    let mut b = CallBuilder::new(
        "Example Network",
        "Site 1",
        started_at_ms,
        852_162_500,
        52102,
    );
    b.talker(9_000_053, started_at_ms, false, None);
    b.set_encryption(Some("AES-256".into()), Some(42));
    b.finish(started_at_ms + 3000)
}

/// Poll `cond` until it holds or the deadline passes.
fn wait_for(what: &str, cond: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if cond() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for {what}");
}

fn spooled_files(dir: &TempDir) -> Vec<String> {
    fn walk(dir: &std::path::Path, out: &mut Vec<String>) {
        let Ok(read) = std::fs::read_dir(dir) else {
            return;
        };
        for e in read.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else {
                out.push(p.file_name().unwrap().to_string_lossy().into_owned());
            }
        }
    }
    let mut out = Vec::new();
    walk(&dir.0, &mut out);
    out.sort();
    out
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[test]
fn uploads_a_call_with_every_contract_field() {
    let mock = Mock::start();
    let spool = TempDir::new("ok");
    let metrics = Metrics::new();
    let uplink = KeyupUplink::start(config(&mock, &spool), &metrics).expect("start");

    assert_eq!(
        uplink.submit(voice_call(1_788_012_000_123)),
        Submitted::Queued
    );
    wait_for("the upload", || uplink.stats().uploaded == 1);

    let reqs = mock.requests();
    assert_eq!(reqs.len(), 1);
    let r = &reqs[0];
    assert_eq!(r.field("key"), Some("kup_test"));
    assert_eq!(r.field("system"), Some("7"));
    assert_eq!(r.field("systemLabel"), Some("Example Network"));
    assert_eq!(r.field("dateTime"), Some("1788012000"));
    assert_eq!(r.field("startMs"), Some("1788012000123"));
    assert_eq!(r.field("endMs"), Some("1788012002123"));
    assert_eq!(r.field("talkgroup"), Some("52101"));
    assert_eq!(r.field("talkgroupLabel"), Some("PD DISPATCH"));
    assert_eq!(r.field("frequency"), Some("852137500"));
    assert_eq!(r.field("source"), Some("9000011"));
    assert_eq!(r.field("patches"), Some("[19282,50051]"));
    assert_eq!(r.field("emergency"), Some("1"));
    assert_eq!(r.field("encrypted"), Some("0"));
    assert_eq!(r.field("talkerAlias"), Some("ENGINE 3"));
    assert_eq!(r.field("recorder"), Some("trunkd"));
    assert_eq!(r.field("site"), Some("Site 1"));
    assert_eq!(r.field("nac"), Some("659"));
    assert_eq!(r.field("durationMs"), Some("2000"));
    assert_eq!(r.field("audioType"), Some("audio/mpeg"));
    assert_eq!(
        r.field("audioName"),
        Some("Example-Network_52101_1788012000123.mp3")
    );

    // The trunk-recorder dialect lists, as JSON.
    let sources: serde_json::Value =
        serde_json::from_str(r.field("sources").expect("sources")).expect("sources json");
    assert_eq!(sources[0]["src"], 9_000_011u64);
    assert_eq!(sources[0]["pos"], 0.0);
    assert_eq!(sources[0]["tag"], "UNIT_313");
    assert_eq!(sources[0]["emergency"], false);
    assert_eq!(sources[1]["src"], 9_000_012u64);
    assert_eq!(sources[1]["pos"], 1.0);
    assert_eq!(sources[1]["emergency"], true);
    let freqs: serde_json::Value =
        serde_json::from_str(r.field("frequencies").expect("frequencies")).expect("freq json");
    assert_eq!(freqs[0]["freq"], 852_137_500u64);
    assert_eq!(freqs[0]["len"], 2.0);
    assert_eq!(freqs[0]["error_count"], 0);

    // The audio part is a real MP3 named like the call.
    let (name, ctype, bytes) = r.audio.clone().expect("audio part");
    assert_eq!(name, "Example-Network_52101_1788012000123.mp3");
    assert_eq!(ctype, "audio/mpeg");
    assert!(bytes.len() > 1000, "mp3 was {} bytes", bytes.len());
    assert_eq!(bytes[0], 0xFF);
    assert_eq!(bytes[1] & 0xE0, 0xE0);

    assert!(
        spooled_files(&spool).is_empty(),
        "nothing should be spooled"
    );
    uplink.shutdown();
}

#[test]
fn wav_format_is_sent_as_wav() {
    let mock = Mock::start();
    let spool = TempDir::new("wav");
    let metrics = Metrics::new();
    let cfg = UplinkConfig {
        audio_format: AudioFormat::Wav,
        priority: "live".into(),
        ..config(&mock, &spool)
    };
    let uplink = KeyupUplink::start(cfg, &metrics).expect("start");
    uplink.submit(voice_call(1_788_012_000_123));
    wait_for("the upload", || uplink.stats().uploaded == 1);

    let r = mock.requests().remove(0);
    assert_eq!(r.field("audioType"), Some("audio/wav"));
    let (name, ctype, bytes) = r.audio.expect("audio part");
    assert_eq!(name, "Example-Network_52101_1788012000123.wav");
    assert_eq!(ctype, "audio/wav");
    assert_eq!(&bytes[0..4], b"RIFF");
    // 2 s of 8 kHz s16 plus the header.
    assert_eq!(bytes.len(), 44 + 2 * 8000 * 2);
    uplink.shutdown();
}

#[test]
fn encrypted_call_is_uploaded_without_an_audio_part() {
    let mock = Mock::start();
    let spool = TempDir::new("enc");
    let metrics = Metrics::new();
    let uplink = KeyupUplink::start(config(&mock, &spool), &metrics).expect("start");

    uplink.submit(encrypted_call(1_788_012_000_000));
    wait_for("the upload", || uplink.stats().uploaded == 1);

    let r = mock.requests().remove(0);
    assert!(r.audio.is_none(), "encrypted calls send no audio part");
    assert_eq!(r.field("encrypted"), Some("1"));
    assert_eq!(r.field("audioType"), Some("audio/mpeg"));
    assert_eq!(
        r.field("audioName"),
        Some("Example-Network_52102_1788012000000.mp3")
    );
    assert_eq!(r.field("algorithm"), Some("AES-256"));
    assert_eq!(r.field("keyId"), Some("42"));
    assert_eq!(r.field("durationMs"), Some("3000"));
    uplink.shutdown();
}

#[test]
fn duplicate_is_counted_as_a_success_and_not_spooled() {
    let mock = Mock::start();
    mock.set_reply(200, "duplicate call rejected");
    let spool = TempDir::new("dup");
    let metrics = Metrics::new();
    let uplink = KeyupUplink::start(config(&mock, &spool), &metrics).expect("start");

    uplink.submit(voice_call(1_788_012_000_123));
    wait_for("the duplicate", || uplink.stats().duplicates == 1);
    assert_eq!(uplink.stats().uploaded, 0);
    assert_eq!(uplink.stats().spooled, 0);
    assert!(spooled_files(&spool).is_empty());
    uplink.shutdown();
}

#[test]
fn incomplete_call_data_is_a_permanent_failure_and_is_not_spooled() {
    let mock = Mock::start();
    mock.set_reply(200, "incomplete call data: no audio");
    let spool = TempDir::new("incomplete");
    let metrics = Metrics::new();
    let uplink = KeyupUplink::start(config(&mock, &spool), &metrics).expect("start");

    uplink.submit(encrypted_call(1_788_012_000_000));
    wait_for("the rejection", || uplink.stats().failed == 1);
    assert_eq!(uplink.stats().spooled, 0);
    assert!(spooled_files(&spool).is_empty());
    // And it is not retried.
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(mock.count(), 1);
    uplink.shutdown();
}

#[test]
fn invalid_key_spools_and_backs_off() {
    let mock = Mock::start();
    mock.set_reply(401, "invalid key");
    let spool = TempDir::new("key");
    let metrics = Metrics::new();
    let uplink = KeyupUplink::start(config(&mock, &spool), &metrics).expect("start");

    uplink.submit(voice_call(1_788_012_000_123));
    wait_for("the spool write", || uplink.stats().spooled == 1);
    let files = spooled_files(&spool);
    assert_eq!(
        files,
        vec![
            "Example-Network_52101_1788012000123.json",
            "Example-Network_52101_1788012000123.mp3"
        ]
    );

    // A second call inside the backoff window is spooled without another POST,
    // and the drain pass does not hammer the endpoint either.
    uplink.submit(voice_call(1_788_012_010_123));
    wait_for("the second spool write", || uplink.stats().spooled == 2);
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(mock.count(), 1, "backoff should suppress further POSTs");
    assert!(uplink.stats().spool_bytes > 0);
    uplink.shutdown();
}

#[test]
fn server_error_spools_and_the_drain_pass_uploads_on_recovery() {
    let mock = Mock::start();
    mock.set_reply(500, "boom");
    let spool = TempDir::new("drain");
    let metrics = Metrics::new();
    let uplink = KeyupUplink::start(config(&mock, &spool), &metrics).expect("start");

    uplink.submit(voice_call(1_788_012_000_123));
    wait_for("the spool write", || uplink.stats().spooled == 1);
    assert_eq!(spooled_files(&spool).len(), 2);
    assert_eq!(uplink.stats().uploaded, 0);

    // Keyup comes back.
    mock.set_reply(200, "Call imported successfully.");
    wait_for("the drain", || uplink.stats().uploaded == 1);
    wait_for("the spool to empty", || spooled_files(&spool).is_empty());

    // The retried POST carries the same metadata and the spooled MP3 verbatim.
    let last = mock.requests().pop().expect("a request");
    assert_eq!(last.field("talkgroup"), Some("52101"));
    assert_eq!(last.field("systemLabel"), Some("Example Network"));
    assert_eq!(last.field("talkgroupLabel"), Some("PD DISPATCH"));
    assert_eq!(last.field("dateTime"), Some("1788012000"));
    // The millisecond edges survive the spool sidecar round-trip too.
    assert_eq!(last.field("startMs"), Some("1788012000123"));
    assert_eq!(last.field("endMs"), Some("1788012002123"));
    assert_eq!(last.field("emergency"), Some("1"));
    assert_eq!(last.field("patches"), Some("[19282,50051]"));
    assert_eq!(last.field("nac"), Some("659"));
    assert_eq!(last.field("recorder"), Some("trunkd"));
    let (name, _, bytes) = last.audio.expect("audio part");
    assert_eq!(name, "Example-Network_52101_1788012000123.mp3");
    assert_eq!(bytes[0], 0xFF);

    wait_for("the gauge", || uplink.stats().spool_bytes == 0);
    uplink.shutdown();
}

#[test]
fn encrypted_call_spools_metadata_only_and_drains() {
    let mock = Mock::start();
    mock.set_reply(503, "down for maintenance");
    let spool = TempDir::new("encdrain");
    let metrics = Metrics::new();
    let uplink = KeyupUplink::start(config(&mock, &spool), &metrics).expect("start");

    uplink.submit(encrypted_call(1_788_012_000_000));
    wait_for("the spool write", || uplink.stats().spooled == 1);
    assert_eq!(
        spooled_files(&spool),
        vec!["Example-Network_52102_1788012000000.json"]
    );

    mock.set_reply(200, "Call imported successfully.");
    wait_for("the drain", || uplink.stats().uploaded == 1);
    let last = mock.requests().pop().expect("a request");
    assert!(last.audio.is_none());
    assert_eq!(last.field("encrypted"), Some("1"));
    assert_eq!(last.field("algorithm"), Some("AES-256"));
    assert_eq!(last.field("keyId"), Some("42"));
    wait_for("the spool to empty", || spooled_files(&spool).is_empty());
    uplink.shutdown();
}

#[test]
fn spool_cap_evicts_oldest_first() {
    let mock = Mock::start();
    mock.set_reply(500, "boom");
    let spool = TempDir::new("cap");
    let metrics = Metrics::new();
    let cfg = UplinkConfig {
        // Room for roughly one call (a 2 s MP3 at 16 kbps is ~4 kB).
        spool_cap_bytes: 6_000,
        // Long enough that the drain does not interleave with the writes.
        drain_interval: Duration::from_secs(30),
        ..config(&mock, &spool)
    };
    let uplink = KeyupUplink::start(cfg, &metrics).expect("start");

    for i in 0..4 {
        uplink.submit(voice_call(1_788_012_000_123 + i * 10_000));
    }
    wait_for("four spool writes", || uplink.stats().spooled == 4);
    wait_for("an eviction", || uplink.stats().spool_evicted > 0);

    let files = spooled_files(&spool);
    assert!(files.len() <= 4, "spool kept {files:?}");
    assert!(
        !files.contains(&"Example-Network_52101_1788012000123.mp3".to_owned()),
        "the oldest call should have been evicted first: {files:?}"
    );
    assert!(uplink.stats().spool_bytes <= 6_000);
    uplink.shutdown();
}

#[test]
fn a_full_queue_spools_the_overflow_and_every_call_arrives() {
    let mock = Mock::start();
    // Slow enough that the worker cannot keep up with a burst.
    mock.set_delay(Duration::from_millis(200));
    let spool = TempDir::new("queue");
    let metrics = Metrics::new();
    let cfg = UplinkConfig {
        queue_capacity: 2,
        ..config(&mock, &spool)
    };
    let uplink = KeyupUplink::start(cfg, &metrics).expect("start");

    let mut spooled = 0;
    for i in 0..8 {
        match uplink.submit(voice_call(1_788_012_000_123 + i * 10_000)) {
            Submitted::Queued => {}
            Submitted::Spooled => spooled += 1,
            Submitted::Dropped => panic!("call {i} was lost"),
        }
    }
    assert!(spooled > 0, "a burst into a 2-deep queue must overflow");
    assert_eq!(uplink.stats().spooled_on_pressure, spooled);
    assert_eq!(uplink.stats().queue_dropped, 0);
    mock.set_delay(Duration::ZERO);
    // The overflow was the spool, not the floor: the drain delivers it.
    wait_for("all eight uploads", || uplink.stats().uploaded == 8);
    wait_for("the spool to empty", || spooled_files(&spool).is_empty());
    uplink.shutdown();
}

#[test]
fn an_empty_url_is_a_configuration_error() {
    let spool = TempDir::new("cfg");
    let metrics = Metrics::new();
    let cfg = UplinkConfig {
        url: "  ".into(),
        spool_dir: spool.0.clone(),
        ..UplinkConfig::default()
    };
    assert!(KeyupUplink::start(cfg, &metrics).is_err());
}

#[test]
fn shutdown_finishes_the_calls_already_queued() {
    let mock = Mock::start();
    let spool = TempDir::new("shutdown");
    let metrics = Metrics::new();
    let uplink = KeyupUplink::start(config(&mock, &spool), &metrics).expect("start");
    for i in 0..3 {
        uplink.submit(voice_call(1_788_012_000_123 + i * 10_000));
    }
    let stats = {
        uplink.submit(encrypted_call(1_788_012_100_000));
        wait_for("all four uploads", || uplink.stats().uploaded == 4);
        uplink.stats()
    };
    assert_eq!(stats.uploaded, 4);
    assert_eq!(stats.failed, 0);
    uplink.shutdown();
    assert_eq!(mock.count(), 4);
}
