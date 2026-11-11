// SPDX-License-Identifier: GPL-3.0-or-later
//! End-to-end replay of a synthetic conventional (analog NBFM) capture through
//! the whole recorder.
//!
//! The test writes a 2 Msps CF32 capture into a temp dir: four NBFM carriers
//! over a low noise floor, each keyed for a known stretch of the file —
//!
//! | carrier | configured | transmitted | expected |
//! |---|---|---|---|
//! | Fire Dispatch | CTCSS 167.9 | CTCSS 167.9 | one call |
//! | Public Works | DCS D445N | DCS D445N | one call |
//! | Mutual Aid | carrier squelch | no tone | one call |
//! | Ambulance | CTCSS 100.0 | CTCSS 103.5 | nothing |
//!
//! — then runs it through [`trunkd::run_once`] with a `backend = "iqfile"`
//! device and no `[[site]]` at all, against a mock Keyup on `127.0.0.1:0` (the
//! same mock `fixture_pipeline.rs` uses). It proves that the channel bank, the
//! demodulator, the tone gate, call assembly, MP3 encoding and the upload
//! contract (`docs/keyup.md`, conventional channels) agree with each other.
//! Every channel sits off the 12.5 kHz bin grid, so each one is taken through
//! the bank's two-bin path. Lastly, `trunkd check` must accept the same
//! channels-only config and reject a broken one.
//!
//! No fixture is needed and nothing touches hardware or a real Keyup; the run
//! takes a few seconds in a plain `cargo t`.
//!
//! ```text
//! cargo t -p trunkd --test conventional_pipeline -- --nocapture
//! ```

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use trunkd::config::Config;
use trunkd_analog::Tone;

/// Capture rate: 160 channelizer bins of 12.5 kHz.
const RATE: u32 = 2_000_000;
/// Capture centre. No channel is on it (the DC bin is refused).
const CENTER_HZ: u64 = 154_500_000;
/// Length of the capture, seconds. Every carrier is down at least 1.5 s
/// before the end, so each call closes on its squelch rather than at EOF.
const CAPTURE_S: f64 = 6.5;

/// `[uplink]` labels: what a channel without `system`/`site` is filed under.
const UPLINK_SYSTEM: &str = "Example Region";
const UPLINK_SITE: &str = "North Tower";

/// One synthetic transmitter and the `[[channel]]` that listens for it.
struct Carrier {
    name: &'static str,
    frequency: u64,
    /// The channel's `tone`, as written in the config.
    configured: Option<&'static str>,
    /// What the transmitter actually sends.
    sent: Option<&'static str>,
    /// The audio tone of its "voice", Hz.
    audio_hz: f64,
    /// When it is keyed, seconds into the capture.
    keyed: Range<f64>,
}

const FIRE: Carrier = Carrier {
    name: "Fire Dispatch",
    frequency: 154_130_000,
    configured: Some("167.9"),
    sent: Some("167.9"),
    audio_hz: 1_000.0,
    keyed: 1.0..4.0,
};

const WORKS: Carrier = Carrier {
    name: "Public Works",
    frequency: 154_995_000,
    configured: Some("D445N"),
    sent: Some("D445N"),
    audio_hz: 800.0,
    keyed: 1.2..4.7,
};

const MUTUAL_AID: Carrier = Carrier {
    name: "Mutual Aid",
    frequency: 154_280_000,
    configured: None,
    sent: None,
    audio_hz: 1_200.0,
    keyed: 1.4..4.4,
};

/// Configured for 100.0 Hz, keyed with the adjacent 103.5 Hz: another user of
/// the frequency, which the tone gate must never record.
const AMBULANCE: Carrier = Carrier {
    name: "Ambulance",
    frequency: 155_340_000,
    configured: Some("100.0"),
    sent: Some("103.5"),
    audio_hz: 900.0,
    keyed: 1.0..4.0,
};

const CARRIERS: [&Carrier; 4] = [&FIRE, &WORKS, &MUTUAL_AID, &AMBULANCE];

// ---------------------------------------------------------------------------
// synthetic capture
// ---------------------------------------------------------------------------

/// Peak voice deviation, Hz (of a 12.5 kHz channel's ±2.5 kHz).
const VOICE_DEV_HZ: f64 = 2_000.0;
/// CTCSS deviation, Hz.
const CTCSS_DEV_HZ: f64 = 600.0;
/// DCS deviation, Hz: NRZ, a 1 bit deviates positive.
const DCS_DEV_HZ: f64 = 500.0;
/// DCS bit rate, bits per second.
const DCS_BAUD: f64 = 134.4;
/// Carrier amplitude, against noise of ±0.01 per component: ~44 dB SNR in a
/// 12.5 kHz channel.
const CARRIER_AMPLITUDE: f32 = 0.1;

/// The sub-audible part of a transmitter's deviation at time `t`, Hz.
fn subaudio_hz(tone: Option<&Tone>, t: f64) -> f64 {
    match tone {
        None => 0.0,
        Some(tone) => {
            if let Some(hz) = tone.ctcss_hz() {
                CTCSS_DEV_HZ * (std::f64::consts::TAU * f64::from(hz) * t).sin()
            } else {
                let word = tone.dcs_word().expect("a DCS tone has a word");
                let bit = (t * DCS_BAUD) as u64 % 23;
                if word >> bit & 1 == 1 {
                    DCS_DEV_HZ
                } else {
                    -DCS_DEV_HZ
                }
            }
        }
    }
}

/// Write the capture to `path`: interleaved little-endian f32 I/Q at [`RATE`]
/// centred on [`CENTER_HZ`], every carrier of [`CARRIERS`] keyed over its
/// window. Synthesised directly at the wideband rate with one phase
/// accumulator per carrier (the generator of `recorder.rs`'s unit test, for
/// four transmitters and DCS).
fn write_capture(path: &Path) {
    let fs = f64::from(RATE);
    let tau = std::f64::consts::TAU;
    let n = (CAPTURE_S * fs) as usize;
    let tones: Vec<Option<Tone>> = CARRIERS
        .iter()
        .map(|c| c.sent.map(|s| s.parse().expect("a test tone parses")))
        .collect();
    let offsets: Vec<f64> = CARRIERS
        .iter()
        .map(|c| c.frequency as f64 - CENTER_HZ as f64)
        .collect();
    let mut phases = [0.0f64; 4];
    let mut seed = 0x2545_f491_4f6c_dd1du64;
    let mut noise = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        ((seed >> 40) as f64 / f64::from(1u32 << 24) - 0.5) as f32 * 0.02
    };
    let mut out = std::io::BufWriter::with_capacity(
        1 << 20,
        std::fs::File::create(path).expect("create the capture"),
    );
    let mut frame = [0u8; 8];
    for i in 0..n {
        let t = i as f64 / fs;
        let (mut re, mut im) = (noise(), noise());
        for (k, c) in CARRIERS.iter().enumerate() {
            if !c.keyed.contains(&t) {
                continue;
            }
            re += CARRIER_AMPLITUDE * phases[k].cos() as f32;
            im += CARRIER_AMPLITUDE * phases[k].sin() as f32;
            let dev =
                VOICE_DEV_HZ * (tau * c.audio_hz * t).sin() + subaudio_hz(tones[k].as_ref(), t);
            phases[k] = (phases[k] + tau * (offsets[k] + dev) / fs).rem_euclid(tau);
        }
        frame[..4].copy_from_slice(&re.to_le_bytes());
        frame[4..].copy_from_slice(&im.to_le_bytes());
        out.write_all(&frame).expect("write the capture");
    }
    out.flush().expect("flush the capture");
}

// ---------------------------------------------------------------------------
// config
// ---------------------------------------------------------------------------

/// A channel's `tone = ...` line; empty for carrier squelch.
fn tone_line(c: &Carrier) -> String {
    c.configured
        .map_or_else(String::new, |t| format!("tone = \"{t}\""))
}

/// The channels-only config, as an operator would write it. Fire Dispatch and
/// Public Works name their own system, site and talkgroup; Mutual Aid and
/// Ambulance take the `[uplink]` labels and the default talkgroup
/// (`frequency / 1000`). `upload_url` empty means spool-only.
fn config_toml(capture: &Path, spool: &Path, upload_url: &str) -> String {
    let uplink = if upload_url.is_empty() {
        "spool_only = true".to_owned()
    } else {
        format!("url = \"{upload_url}\"\napi_key = \"kup_test\"")
    };
    let path = capture.display().to_string().replace('\\', "/");
    let spool = spool.display().to_string().replace('\\', "/");
    format!(
        r#"
[uplink]
{uplink}
spool_path = '{spool}'
system_label = "{UPLINK_SYSTEM}"
site_label = "{UPLINK_SITE}"
audio_format = "mp3"
mp3_bitrate_kbps = 16

[recorder]
instance_id = "conv-e2e"

[[device]]
id = "vhf0"
backend = "iqfile"
path = '{path}'
sample_rate = {RATE}
center_frequency = {CENTER_HZ}
pacing = "unthrottled"

[[channel]]
name = "{fire}"
system = "Example County Fire"
site = "Example Hill"
frequency = {fire_hz}
talkgroup = 14
{fire_tone}

[[channel]]
name = "{works}"
system = "Example City"
frequency = {works_hz}
talkgroup = 210
{works_tone}

[[channel]]
name = "{aid}"
frequency = {aid_hz}
{aid_tone}

[[channel]]
name = "{amb}"
frequency = {amb_hz}
{amb_tone}
"#,
        fire = FIRE.name,
        fire_hz = FIRE.frequency,
        works = WORKS.name,
        works_hz = WORKS.frequency,
        aid = MUTUAL_AID.name,
        aid_hz = MUTUAL_AID.frequency,
        amb = AMBULANCE.name,
        amb_hz = AMBULANCE.frequency,
        fire_tone = tone_line(&FIRE),
        works_tone = tone_line(&WORKS),
        aid_tone = tone_line(&MUTUAL_AID),
        amb_tone = tone_line(&AMBULANCE),
    )
}

// ---------------------------------------------------------------------------
// mock Keyup (as in fixture_pipeline.rs)
// ---------------------------------------------------------------------------

/// One request the mock received.
#[derive(Clone, Debug, Default)]
struct Req {
    path: String,
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
    // Windows hands an accepted socket the listener's non-blocking mode.
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
            Ok(0) | Err(_) => return,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let len: usize = header(&head, "content-length")
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0);
    let content_type = header(&head, "content-type").unwrap_or_default();
    while buf.len() < head_end + 4 + len {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
    let body = &buf[head_end + 4..];
    let path = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .unwrap_or_default()
        .to_owned();
    let mut req = if content_type.starts_with("multipart/") {
        parse_multipart(&content_type, body)
    } else {
        Req::default()
    };
    req.path = path.clone();
    let events = serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("events").and_then(|e| e.as_array().map(Vec::len)))
        .unwrap_or(0);
    requests.lock().expect("mock lock").push(req);

    let (ctype, text) = if path == "/api/recorder/events" {
        ("application/json", format!("{{\"stored\": {events}}}"))
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
            let ctype = header(&head, "content-type").unwrap_or_default();
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
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let p = std::env::temp_dir().join(format!("trunkd-{tag}-{}-{nanos}", std::process::id()));
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

/// Playing time of an MP3 stream in seconds, by walking its MPEG audio frame
/// headers (Layer III, any MPEG version). A leading ID3v2 tag and a
/// Xing/Info header frame, if present, carry no audio and are skipped.
fn mp3_duration_s(bytes: &[u8]) -> f64 {
    const BITRATE_V1: [u32; 15] = [
        0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320,
    ];
    const BITRATE_V2: [u32; 15] = [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160];
    let mut at = 0;
    if bytes.starts_with(b"ID3") && bytes.len() >= 10 {
        let size = bytes[6..10]
            .iter()
            .fold(0usize, |acc, &b| (acc << 7) | usize::from(b & 0x7F));
        at = 10 + size;
    }
    let (mut samples, mut rate, mut frames) = (0u64, 0u32, 0usize);
    while at + 4 <= bytes.len() {
        let h = &bytes[at..at + 4];
        if h[0] != 0xFF || h[1] & 0xE0 != 0xE0 {
            break;
        }
        let version = (h[1] >> 3) & 3; // 0 = 2.5, 2 = 2, 3 = 1
        let layer = (h[1] >> 1) & 3; // 1 = Layer III
        let br_idx = usize::from(h[2] >> 4);
        let sr_idx = usize::from((h[2] >> 2) & 3);
        assert_eq!(layer, 1, "Layer III frames only (at byte {at})");
        assert!(version != 1 && br_idx != 0 && br_idx != 15 && sr_idx != 3);
        let sr = [
            [11_025, 12_000, 8_000],
            [0, 0, 0],
            [22_050, 24_000, 16_000],
            [44_100, 48_000, 32_000],
        ][usize::from(version)][sr_idx];
        let (kbps, per_frame, coef) = if version == 3 {
            (BITRATE_V1[br_idx], 1152u64, 144u32)
        } else {
            (BITRATE_V2[br_idx], 576u64, 72u32)
        };
        let len = (coef * kbps * 1000 / sr + u32::from((h[2] >> 1) & 1)) as usize;
        let body = &bytes[at..(at + len).min(bytes.len())];
        let tag_frame =
            frames == 0 && (find(body, b"Xing", 0).is_some() || find(body, b"Info", 0).is_some());
        if !tag_frame {
            samples += per_frame;
        }
        rate = sr;
        frames += 1;
        at += len;
    }
    assert!(frames > 0 && rate > 0, "no MPEG audio frames found");
    samples as f64 / f64::from(rate)
}

fn key_s(c: &Carrier) -> f64 {
    c.keyed.end - c.keyed.start
}

// ---------------------------------------------------------------------------
// the tests
// ---------------------------------------------------------------------------

#[test]
fn records_conventional_channels_and_uploads_them() {
    let begin = Instant::now();
    let dir = TempDir::new("conv-e2e");
    let capture = dir.0.join("vhf.cf32");
    write_capture(&capture);
    eprintln!(
        "capture: {:.1} s of {} Msps CF32 written in {:.1?}",
        CAPTURE_S,
        RATE / 1_000_000,
        begin.elapsed()
    );

    let mock = Mock::start();
    let spool = dir.0.join("spool");
    let cfg: Config = toml::from_str(&config_toml(&capture, &spool, &mock.upload_url()))
        .expect("the channels-only config parses");
    cfg.validate().expect("the channels-only config validates");

    let summary = trunkd::run_once(&cfg, Arc::new(AtomicBool::new(false)))
        .expect("the recorder runs to the end of the file");
    eprintln!("run done at {:.1?}: {summary:?}", begin.elapsed());

    let requests = mock.requests();
    let uploads: Vec<&Req> = requests
        .iter()
        .filter(|r| r.path == "/api/call-upload")
        .collect();
    for r in &uploads {
        eprintln!("upload: {:?}", {
            let mut f: Vec<_> = r.fields.iter().filter(|(k, _)| *k != "key").collect();
            f.sort();
            f
        });
    }

    // -- what reached Keyup -------------------------------------------------
    assert_eq!(summary.calls_started, 0, "no P25 traffic in the capture");
    assert_eq!(
        summary.overflow_samples, 0,
        "the file source never overflows"
    );
    assert_eq!(summary.call_queue_dropped, 0, "no call message was evicted");
    assert_eq!(summary.calls_orphaned, 0, "no call was closed as orphaned");
    assert_eq!(summary.uplink.failed, 0, "no call was rejected");
    assert_eq!(summary.uplink.spooled, 0, "the mock answered every call");
    assert_eq!(
        uploads.len(),
        3,
        "one call per matching carrier, none for the wrong tone"
    );
    assert_eq!(summary.uplink.uploaded, 3, "every upload counted as stored");

    let by_freq = |c: &Carrier| -> Vec<&Req> {
        let f = c.frequency.to_string();
        uploads
            .iter()
            .copied()
            .filter(|r| r.field("frequency") == Some(f.as_str()))
            .collect()
    };
    assert!(
        by_freq(&AMBULANCE).is_empty(),
        "the 103.5 Hz carrier on a 100.0 Hz channel is never recorded"
    );
    assert!(
        uploads
            .iter()
            .all(|r| r.field("talkgroupLabel") != Some(AMBULANCE.name)),
        "nothing filed under the wrong-tone channel"
    );

    // (carrier, systemLabel, site, talkgroup). The configured tone goes up as
    // `tone` and the one transmitted is heard as `toneDetected`.
    let aid_tg = (MUTUAL_AID.frequency / 1000).to_string();
    let expected = [
        (&FIRE, "Example County Fire", "Example Hill", "14"),
        (&WORKS, "Example City", UPLINK_SITE, "210"),
        (&MUTUAL_AID, UPLINK_SYSTEM, UPLINK_SITE, aid_tg.as_str()),
    ];
    for (c, system, site, tg) in expected {
        let calls = by_freq(c);
        assert_eq!(calls.len(), 1, "{}: one call, got {}", c.name, calls.len());
        let r = calls[0];
        let freq = c.frequency.to_string();
        assert_eq!(r.field("key"), Some("kup_test"));
        assert_eq!(r.field("systemLabel"), Some(system), "{}", c.name);
        assert_eq!(r.field("site"), Some(site), "{}", c.name);
        assert_eq!(r.field("talkgroup"), Some(tg), "{}", c.name);
        assert_eq!(r.field("talkgroupLabel"), Some(c.name));
        assert_eq!(r.field("frequency"), Some(freq.as_str()));
        assert_eq!(r.field("mode"), Some("nbfm"), "{}", c.name);
        assert_eq!(r.field("decoder"), Some("NBFM"), "{}", c.name);
        assert_eq!(r.field("tone"), c.configured, "{}: configured tone", c.name);
        assert_eq!(r.field("toneDetected"), c.sent, "{}: tone heard", c.name);
        assert_eq!(
            r.field("squelchReason"),
            Some("squelch_closed"),
            "{}: the carrier dropped well before EOF",
            c.name
        );
        assert_eq!(r.field("recorder"), Some("trunkd"));
        assert_eq!(r.field("audioType"), Some("audio/mpeg"));
        assert_eq!(r.field("encrypted"), Some("0"));
        // Analog carries no radio ids: an empty list and no `source`.
        assert_eq!(r.field("sources"), Some("[]"), "{}", c.name);
        assert_eq!(r.field("source"), None, "{}", c.name);
        let freqs: serde_json::Value =
            serde_json::from_str(r.field("frequencies").expect("frequencies")).expect("JSON");
        let freqs = freqs.as_array().expect("frequencies array");
        assert_eq!(freqs.len(), 1, "{}: one frequency segment", c.name);
        assert_eq!(freqs[0]["freq"].as_u64(), Some(c.frequency));
        let snr: f64 = r
            .field("snrDb")
            .and_then(|v| v.parse().ok())
            .expect("snrDb");
        assert!(snr > 20.0, "{}: a strong carrier, snrDb {snr}", c.name);

        // The call's length, and the MP3's playing time, match the time the
        // carrier was keyed: the tone gate's buffer keeps the audio from
        // before the tone locked.
        let keyed = key_s(c);
        let duration_ms: f64 = r
            .field("durationMs")
            .and_then(|v| v.parse().ok())
            .expect("durationMs");
        assert!(
            (duration_ms / 1000.0 - keyed).abs() <= 0.1 * keyed,
            "{}: keyed {keyed:.2} s, durationMs {duration_ms}",
            c.name
        );
        let (name, ctype, bytes) = r.audio.as_ref().expect("the call carries audio");
        assert!(name.ends_with(".mp3"), "audio is named {name}");
        assert_eq!(ctype, "audio/mpeg");
        let played = mp3_duration_s(bytes);
        assert!(
            (played - keyed).abs() <= 0.1 * keyed,
            "{}: keyed {keyed:.2} s, MP3 plays {played:.2} s",
            c.name
        );
        let speech: f64 = r
            .field("speechS")
            .and_then(|v| v.parse().ok())
            .expect("speechS");
        assert!(
            speech > 0.8 * keyed,
            "{}: the voice tone is audible ({speech:.2} s of {keyed:.2} s)",
            c.name
        );
    }

    let elapsed = begin.elapsed();
    eprintln!("total {elapsed:.1?}");
    assert!(
        elapsed < Duration::from_secs(60),
        "the end-to-end test stays under a minute ({elapsed:.1?})"
    );
}

/// `trunkd check` — the real binary — accepts a config with channels and no
/// `[[site]]`, and refuses one whose channel sits on the capture centre.
#[test]
fn check_accepts_a_channels_only_config() {
    let dir = TempDir::new("conv-check");
    let capture = dir.0.join("vhf.cf32");
    std::fs::write(&capture, []).expect("an empty capture file");
    let spool = dir.0.join("spool");
    let good = dir.0.join("channels.toml");
    let text = config_toml(&capture, &spool, "");
    std::fs::write(&good, &text).expect("write config");

    let check = |path: &Path| {
        let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_trunkd"));
        cmd.arg("check").arg("--config").arg(path);
        // `Config::load` layers `TRUNKD_*` over the file; keep the test's
        // environment (`TRUNKD_FIXTURES`, ...) out of it.
        for (k, _) in std::env::vars_os() {
            if k.to_string_lossy().starts_with("TRUNKD_") {
                cmd.env_remove(k);
            }
        }
        cmd.output().expect("run trunkd check")
    };

    let out = check(&good);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "check refused a channels-only config:\n{stdout}\n{stderr}"
    );
    assert!(stdout.contains("configuration: OK"), "{stdout}");
    assert!(
        stdout.contains("sites=0") && stdout.contains("channels=4 (4 enabled, 0 disabled)"),
        "{stdout}"
    );

    // The same config with Mutual Aid moved onto the capture centre (the DC
    // bin, which the channelizer refuses).
    let bad = dir.0.join("dc-bin.toml");
    let broken = text.replace(
        &format!("frequency = {}", MUTUAL_AID.frequency),
        &format!("frequency = {CENTER_HZ}"),
    );
    assert_ne!(broken, text);
    std::fs::write(&bad, broken).expect("write config");
    let out = check(&bad);
    assert!(
        !out.status.success(),
        "check accepted a channel on the DC bin:\n{}",
        String::from_utf8_lossy(&out.stdout)
    );
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        said.contains(MUTUAL_AID.name),
        "the error names the channel: {said}"
    );
}
