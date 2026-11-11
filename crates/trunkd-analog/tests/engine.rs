// SPDX-License-Identifier: GPL-3.0-or-later
//! `NbfmChannel` end to end on synthetic signals: squelch timing, false
//! opens on noise, tone gating with audio continuity, min/max call, stats
//! and the event contract. Most tests use test doubles (a small Goertzel
//! CTCSS bank and a scripted detector) behind [`trunkd_analog::ToneDetector`]
//! to pin the engine's own timing; the `real_*` tests and the noise runs use
//! the crate's CTCSS/DCS detectors ([`trunkd_analog::default_detectors`]).

// `NbfmSignal::keyed` is a list of ranges; one range is a normal value.
#![allow(clippy::single_range_in_vec_init)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use num_complex::Complex;
use trunkd_analog::testsig::{self, Audio, Deviation, NbfmSignal};
use trunkd_analog::tone::CTCSS_TONES;
use trunkd_analog::{
    Bandwidth, CallEnd, CallStart, ChannelEvent, ChannelParams, ChannelState, EndReason,
    NbfmChannel, Tone, ToneDetector, FRAME_SAMPLES, PCM_BATCH_SAMPLES, SUBAUDIO_RATE_HZ,
};

const FS: usize = 25_000;

fn samples(ms: usize) -> usize {
    ms * FS / 1000
}

fn tone(s: &str) -> Tone {
    s.parse().unwrap()
}

// ---------------------------------------------------------------------------
// Test doubles
// ---------------------------------------------------------------------------

/// Minimal CTCSS detector: Goertzel over all 50 tones on a 400 ms window,
/// evaluated every 100 ms; locks when the best tone holds ≥ 50 % of the
/// window's energy and is ≥ 6 dB over the runner-up, drops after two misses.
/// Records the sub-audio sample count of its first lock.
struct GoertzelDouble {
    buf: Vec<f32>,
    pos: usize,
    filled: usize,
    since_eval: usize,
    seen: u64,
    locked: Option<Tone>,
    misses: u32,
    first_lock: Arc<AtomicU64>,
}

const WIN: usize = 1_000;
const HOP: usize = 250;

impl GoertzelDouble {
    fn new() -> (Self, Arc<AtomicU64>) {
        let first = Arc::new(AtomicU64::new(u64::MAX));
        (
            GoertzelDouble {
                buf: vec![0.0; WIN],
                pos: 0,
                filled: 0,
                since_eval: 0,
                seen: 0,
                locked: None,
                misses: 0,
                first_lock: first.clone(),
            },
            first,
        )
    }

    fn evaluate(&mut self) {
        let fs = SUBAUDIO_RATE_HZ as f32;
        let energy: f32 = self.buf.iter().map(|x| x * x).sum();
        let mut best = (0usize, 0.0f32);
        let mut second = 0.0f32;
        for (i, &tenths) in CTCSS_TONES.iter().enumerate() {
            let w = std::f32::consts::TAU * tenths as f32 / 10.0 / fs;
            let coeff = 2.0 * w.cos();
            let (mut s1, mut s2) = (0.0f32, 0.0f32);
            for k in 0..WIN {
                let x = self.buf[(self.pos + k) % WIN];
                let s0 = x + coeff * s1 - s2;
                s2 = s1;
                s1 = s0;
            }
            let p = s1 * s1 + s2 * s2 - coeff * s1 * s2;
            if p > best.1 {
                second = best.1;
                best = (i, p);
            } else if p > second {
                second = p;
            }
        }
        let frac = best.1 / (energy.max(1e-30) * WIN as f32 / 2.0);
        let hit = frac > 0.5 && best.1 > 4.0 * second;
        if hit {
            let t = Tone::Ctcss(CTCSS_TONES[best.0]);
            if self.locked.is_none() {
                let _ = self.first_lock.compare_exchange(
                    u64::MAX,
                    self.seen,
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                );
            }
            self.locked = Some(t);
            self.misses = 0;
        } else if self.locked.is_some() {
            self.misses += 1;
            if self.misses >= 2 {
                self.locked = None;
                self.misses = 0;
            }
        }
    }
}

impl ToneDetector for GoertzelDouble {
    fn push(&mut self, x: &[f32]) {
        for &v in x {
            self.buf[self.pos] = v;
            self.pos = (self.pos + 1) % WIN;
            self.filled = (self.filled + 1).min(WIN);
            self.seen += 1;
            self.since_eval += 1;
            if self.filled == WIN && self.since_eval >= HOP {
                self.since_eval = 0;
                self.evaluate();
            }
        }
    }
    fn current(&self) -> Option<Tone> {
        self.locked
    }
    fn reset(&mut self) {
        self.buf.iter_mut().for_each(|x| *x = 0.0);
        self.filled = 0;
        self.since_eval = 0;
        self.locked = None;
        self.misses = 0;
    }
}

/// Reports `tone` while the sub-audio sample count is inside `on` (ignores
/// the signal; `reset` does not rewind the clock).
struct Scripted {
    tone: Tone,
    on: std::ops::Range<u64>,
    seen: u64,
}

impl ToneDetector for Scripted {
    fn push(&mut self, x: &[f32]) {
        self.seen += x.len() as u64;
    }
    fn current(&self) -> Option<Tone> {
        self.on.contains(&self.seen).then_some(self.tone)
    }
    fn reset(&mut self) {}
}

fn goertzel_engine(p: ChannelParams) -> (NbfmChannel, Arc<AtomicU64>) {
    let (d, first) = GoertzelDouble::new();
    (NbfmChannel::with_detectors(p, vec![Box::new(d)]), first)
}

// ---------------------------------------------------------------------------
// Driving and inspecting
// ---------------------------------------------------------------------------

/// Events with the stream time of the chunk that produced them, ms.
struct Run {
    events: Vec<(u64, ChannelEvent)>,
    /// (stream ms at the end of the chunk, state) on every state change.
    states: Vec<(u64, ChannelState)>,
}

fn drive(engine: &mut NbfmChannel, iq: &[Complex<f32>], chunk: usize, t0_samples: usize) -> Run {
    let mut run = Run {
        events: Vec::new(),
        states: Vec::new(),
    };
    let mut last = engine.stats().state;
    let mut off = 0;
    while off < iq.len() {
        let end = (off + chunk).min(iq.len());
        let at = ((t0_samples + off) * 1000 / FS) as u64;
        let now = ((t0_samples + end) * 1000 / FS) as u64;
        let ev = &mut run.events;
        engine.process(&iq[off..end], at, &mut |e| ev.push((now, e)));
        let st = engine.stats().state;
        if st != last {
            run.states.push((now, st));
            last = st;
        }
        off = end;
    }
    run
}

impl Run {
    fn starts(&self) -> Vec<CallStart> {
        self.events
            .iter()
            .filter_map(|(_, e)| match e {
                ChannelEvent::Started(s) => Some(*s),
                _ => None,
            })
            .collect()
    }
    fn ends(&self) -> Vec<CallEnd> {
        self.events
            .iter()
            .filter_map(|(_, e)| match e {
                ChannelEvent::Ended(s) => Some(*s),
                _ => None,
            })
            .collect()
    }
    /// PCM of each call.
    fn calls_pcm(&self) -> Vec<Vec<i16>> {
        let mut out: Vec<Vec<i16>> = Vec::new();
        for (_, e) in &self.events {
            match e {
                ChannelEvent::Started(_) => out.push(Vec::new()),
                ChannelEvent::Pcm(p) => out.last_mut().unwrap().extend_from_slice(p),
                ChannelEvent::Ended(_) => {}
            }
        }
        out
    }
    fn first_state(&self, s: ChannelState) -> Option<u64> {
        self.states.iter().find(|x| x.1 == s).map(|x| x.0)
    }
}

/// The engine's event contract: `(Started Pcm* Ended)*`, PCM in whole frames of at
/// most 800 samples, `Ended.at_ms = Started.at_ms + duration_ms` = PCM/8.
fn check_contract(events: &[(u64, ChannelEvent)]) {
    let mut open: Option<(CallStart, u64)> = None;
    for (_, e) in events {
        match e {
            ChannelEvent::Started(s) => {
                assert!(open.is_none(), "Started while a call is open");
                open = Some((*s, 0));
            }
            ChannelEvent::Pcm(p) => {
                let (_, n) = open.as_mut().expect("Pcm outside a call");
                assert!(!p.is_empty() && p.len() <= PCM_BATCH_SAMPLES);
                assert_eq!(p.len() % FRAME_SAMPLES, 0);
                *n += p.len() as u64;
            }
            ChannelEvent::Ended(end) => {
                let (s, n) = open.take().expect("Ended outside a call");
                assert_eq!(end.duration_ms, n / 8);
                assert_eq!(end.at_ms, s.at_ms + end.duration_ms);
            }
        }
    }
}

/// Amplitude of `hz` in `x` at 8 kHz, as a fraction of i16 full scale.
fn amp8k(x: &[i16], hz: f32) -> f32 {
    let w = std::f32::consts::TAU * hz / 8_000.0;
    let (mut re, mut im) = (0.0f32, 0.0f32);
    for (n, &v) in x.iter().enumerate() {
        re += v as f32 * (w * n as f32).cos();
        im -= v as f32 * (w * n as f32).sin();
    }
    2.0 * (re * re + im * im).sqrt() / x.len() as f32 / 32_767.0
}

fn keyed(audio: Audio, tone: Option<Tone>, snr_db: f32, on: std::ops::Range<usize>) -> NbfmSignal {
    NbfmSignal {
        snr_db,
        keyed: vec![on],
        ..NbfmSignal::new(audio, tone)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn squelch_opens_within_60ms_and_closes_hang_after_drop() {
    let key = samples(1_000);
    let drop = samples(3_000);
    let iq = keyed(Audio::Voice, None, 10.0, key..drop).generate(samples(5_000));
    let mut e = NbfmChannel::new(ChannelParams::default());
    let run = drive(&mut e, &iq, samples(5), 0);
    check_contract(&run.events);

    let opened = run
        .first_state(ChannelState::Opening)
        .expect("never opened");
    assert!((1_000..=1_060).contains(&opened), "opened at {opened} ms");

    let s = run.starts();
    let en = run.ends();
    assert_eq!((s.len(), en.len()), (1, 1));
    assert!(s[0].at_ms.abs_diff(1_000) <= 10, "start {}", s[0].at_ms);
    assert!(
        s[0].snr_db > 8.0 && s[0].snr_db < 14.0,
        "snr {}",
        s[0].snr_db
    );
    assert_eq!(en[0].reason, EndReason::SquelchClosed);
    // The call ends at the carrier drop (hang tail trimmed) ...
    assert!(en[0].at_ms.abs_diff(3_000) <= 25, "end {}", en[0].at_ms);
    // ... and the squelch closes hang_ms (1 s) after it.
    let closed = run
        .states
        .iter()
        .filter(|x| x.1 == ChannelState::Closed)
        .map(|x| x.0)
        .next_back()
        .unwrap();
    assert!(closed.abs_diff(4_000) <= 25, "closed at {closed} ms");
    let (ended_at, _) = run
        .events
        .iter()
        .find(|(_, e)| matches!(e, ChannelEvent::Ended(_)))
        .unwrap();
    assert!(
        ended_at.abs_diff(4_000) <= 25,
        "Ended emitted at {ended_at} ms"
    );
    assert_eq!(e.stats().calls, 1);
    assert_eq!(e.stats().false_opens, 0);
}

#[test]
fn hang_bridges_a_short_drop() {
    let iq = NbfmSignal {
        snr_db: 15.0,
        keyed: vec![samples(500)..samples(2_000), samples(2_400)..samples(4_000)],
        ..NbfmSignal::new(Audio::Voice, None)
    }
    .generate(samples(6_000));
    let mut e = NbfmChannel::new(ChannelParams::default());
    let run = drive(&mut e, &iq, 999, 0);
    check_contract(&run.events);
    let en = run.ends();
    assert_eq!(en.len(), 1);
    assert!(
        en[0].duration_ms.abs_diff(3_500) <= 30,
        "{}",
        en[0].duration_ms
    );
    // The 400 ms gap is silence, not discriminator noise.
    let pcm = &run.calls_pcm()[0];
    let gap = &pcm[8 * 1_550..8 * 1_850];
    assert!(gap.iter().all(|&x| x == 0));
}

#[test]
fn tone_gate_keeps_audio_from_key_up_not_from_tone_lock() {
    // 1 kHz keyed at t = 1 s with CTCSS 100.0; the double needs > 400 ms to
    // lock, the call must still start at key-up with the 1 kHz in it.
    let t = tone("100.0");
    let iq = keyed(
        Audio::Sine { hz: 1_000.0 },
        Some(t),
        20.0,
        samples(1_000)..samples(4_000),
    )
    .generate(samples(6_000));
    let (mut e, first_lock) = goertzel_engine(ChannelParams::with_tone(Some(t)));
    let run = drive(&mut e, &iq, 640, 0);
    check_contract(&run.events);

    let s = run.starts();
    assert_eq!(s.len(), 1);
    assert_eq!(s[0].tone_detected, Some(t));
    assert!(s[0].at_ms.abs_diff(1_000) <= 10, "start {}", s[0].at_ms);
    // Tone lock came well after key-up (sub-audio clock, 2.5 ksps).
    let lock_ms = first_lock.load(Ordering::SeqCst) * 1000 / u64::from(SUBAUDIO_RATE_HZ);
    assert!((1_300..1_750).contains(&lock_ms), "lock at {lock_ms} ms");

    let pcm = &run.calls_pcm()[0];
    let steady = amp8k(&pcm[8 * 1_000..8 * 1_400], 1_000.0);
    assert!(steady > 0.5, "steady 1 kHz level {steady}");
    // Every 20 ms frame from the second on (20–40 ms after key-up) already
    // carries the 1 kHz at nearly full level: nothing before tone lock was
    // lost.
    for f in 1..((lock_ms as usize - 1_000) / 20) {
        let a = amp8k(&pcm[f * 160..(f + 1) * 160], 1_000.0);
        assert!(a > 0.8 * steady, "frame {f}: {a} vs {steady}");
    }
    assert!(amp8k(&pcm[0..160], 1_000.0) > 0.3 * steady);
    let en = run.ends();
    assert!(en[0].duration_ms.abs_diff(3_000) <= 30);
    assert_eq!(en[0].tone_detected, Some(t));
    assert_eq!(e.stats().tone_mismatch, 0);
}

#[test]
fn wrong_tone_carrier_is_discarded_then_right_tone_opens() {
    let want = tone("100.0");
    let mut iq = keyed(
        Audio::Voice,
        Some(tone("131.8")),
        20.0,
        samples(500)..samples(3_500),
    )
    .generate(samples(5_000));
    iq.extend(
        keyed(Audio::Voice, Some(want), 20.0, samples(500)..samples(3_500))
            .generate(samples(5_000)),
    );
    let (mut e, _) = goertzel_engine(ChannelParams::with_tone(Some(want)));
    let run = drive(&mut e, &iq, 1_250, 0);
    check_contract(&run.events);
    let s = run.starts();
    assert_eq!(s.len(), 1, "only the right-tone call");
    assert!(s[0].at_ms.abs_diff(5_500) <= 10, "{}", s[0].at_ms);
    let st = e.stats();
    assert_eq!(st.tone_mismatch, 1);
    assert_eq!(st.calls, 1);
    assert_eq!(st.calls_too_short, 0);
}

#[test]
fn dcs_gate_with_scripted_detector_and_polarity_pair() {
    // D445I is on air the same as its normal-polarity partner, which is what
    // a DCS detector may report.
    let want = tone("D445I");
    let heard = want.dcs_equivalent().unwrap();
    let iq = keyed(Audio::Voice, None, 20.0, samples(500)..samples(2_500)).generate(samples(4_000));
    // Lock 300 ms after key-up, for 1.5 s.
    let d = Scripted {
        tone: heard,
        on: 2_000..5_750,
        seen: 0,
    };
    let mut e =
        NbfmChannel::with_detectors(ChannelParams::with_tone(Some(want)), vec![Box::new(d)]);
    let run = drive(&mut e, &iq, 500, 0);
    check_contract(&run.events);
    let s = run.starts();
    assert_eq!(s.len(), 1);
    assert!(s[0].at_ms.abs_diff(500) <= 10);
    assert_eq!(s[0].tone_detected, Some(want));
    // Tone gone at 2.3 s (a DCS turn-off), carrier dropped at 2.5 s: the
    // tone's hang runs out first, and the call ends where the tone went.
    let en = run.ends();
    assert_eq!(en[0].reason, EndReason::ToneLost);
    assert!(en[0].at_ms.abs_diff(2_300) <= 15, "{}", en[0].at_ms);
}

#[test]
fn tone_lost_on_continuing_carrier() {
    let want = tone("D023N");
    let iq = keyed(Audio::Voice, None, 20.0, samples(500)..samples(6_000)).generate(samples(7_000));
    let d = Scripted {
        tone: want,
        on: 1_500..5_000, // 0.6 s .. 2.0 s
        seen: 0,
    };
    let mut e =
        NbfmChannel::with_detectors(ChannelParams::with_tone(Some(want)), vec![Box::new(d)]);
    let run = drive(&mut e, &iq, 500, 0);
    check_contract(&run.events);
    let en = run.ends();
    assert_eq!(en.len(), 1);
    assert_eq!(en[0].reason, EndReason::ToneLost);
    assert!(en[0].at_ms.abs_diff(2_000) <= 15, "{}", en[0].at_ms);
    assert_eq!(e.stats().state, ChannelState::Closed);
}

// --- Real CTCSS / DCS detectors (default_detectors) ------------------------

/// Run a tone-gated engine with the real detectors on one transmission
/// keyed 1.0–4.0 s at `snr_db` with `sent`; returns the events.
fn real_gate(want: Tone, sent: Option<Tone>, snr_db: f32, offset_hz: f32) -> (Run, NbfmChannel) {
    let iq = NbfmSignal {
        snr_db,
        offset_hz,
        keyed: vec![samples(1_000)..samples(4_000)],
        ..NbfmSignal::new(Audio::Sine { hz: 1_000.0 }, sent)
    }
    .generate(samples(6_000));
    let mut e = NbfmChannel::new(ChannelParams::with_tone(Some(want)));
    let run = drive(&mut e, &iq, 500, 0);
    check_contract(&run.events);
    (run, e)
}

#[test]
fn real_ctcss_gate_keeps_audio_from_key_up() {
    let t = tone("167.9");
    for snr in [10.0, 20.0] {
        let (run, e) = real_gate(t, Some(t), snr, 0.0);
        let s = run.starts();
        assert_eq!(s.len(), 1, "{snr} dB");
        assert!(
            s[0].at_ms.abs_diff(1_000) <= 10,
            "{snr} dB start {}",
            s[0].at_ms
        );
        assert_eq!(s[0].tone_detected, Some(t));
        let pcm = &run.calls_pcm()[0];
        let steady = amp8k(&pcm[8 * 1_000..8 * 1_400], 1_000.0);
        // The first 400 ms (before the detector's ~450 ms lock) is there.
        for f in 1..20 {
            let a = amp8k(&pcm[f * 160..(f + 1) * 160], 1_000.0);
            assert!(a > 0.8 * steady, "{snr} dB frame {f}: {a} vs {steady}");
        }
        let en = run.ends()[0];
        assert!(en.duration_ms.abs_diff(3_000) <= 30, "{}", en.duration_ms);
        assert_eq!(en.tone_detected, Some(t));
        assert_eq!(e.stats().tone_mismatch, 0);
    }
}

#[test]
fn real_ctcss_gate_rejects_other_and_adjacent_tones() {
    let want = tone("100.0");
    for sent in ["103.5", "97.4", "131.8"] {
        let (run, e) = real_gate(want, Some(tone(sent)), 20.0, 0.0);
        assert!(run.events.is_empty(), "opened on {sent}");
        assert_eq!(e.stats().tone_mismatch, 1, "{sent}");
    }
    // No tone at all on a tone-gated channel.
    let (run, e) = real_gate(want, None, 20.0, 0.0);
    assert!(run.events.is_empty());
    assert_eq!(e.stats().tone_mismatch, 1);
}

#[test]
fn real_dcs_gate_both_polarities_with_offset() {
    for code in ["D445N", "D445I", "D023N"] {
        let t = tone(code);
        for offset in [-300.0, 300.0] {
            let (run, _) = real_gate(t, Some(t), 10.0, offset);
            let s = run.starts();
            assert_eq!(s.len(), 1, "{code} {offset}");
            assert!(
                s[0].at_ms.abs_diff(1_000) <= 10,
                "{code} start {}",
                s[0].at_ms
            );
            assert_eq!(s[0].tone_detected, Some(t));
            let en = run.ends()[0];
            assert!(
                en.duration_ms.abs_diff(3_000) <= 30,
                "{code} {}",
                en.duration_ms
            );
            assert!(
                (en.offset_hz - offset).abs() < 40.0,
                "{code} offset {}",
                en.offset_hz
            );
        }
    }
    // A CTCSS carrier and a different code do not open a DCS channel.
    let want = tone("D445N");
    for sent in ["167.9", "D023N"] {
        let (run, e) = real_gate(want, Some(tone(sent)), 20.0, 0.0);
        assert!(run.events.is_empty(), "opened on {sent}");
        assert_eq!(e.stats().tone_mismatch, 1);
    }
}

#[test]
fn real_detectors_report_tone_on_carrier_squelch() {
    for sent in ["136.5", "D445N"] {
        let t = tone(sent);
        let iq = keyed(Audio::Voice, Some(t), 15.0, samples(500)..samples(3_500))
            .generate(samples(5_000));
        let mut e = NbfmChannel::new(ChannelParams::default());
        let run = drive(&mut e, &iq, 700, 0);
        check_contract(&run.events);
        let en = run.ends();
        assert_eq!(en.len(), 1, "{sent}");
        let heard = en[0].tone_detected.expect("no tone reported");
        assert!(t.same_signal(&heard), "{sent}: heard {heard}");
    }
}

/// Amplitude of `hz` in `x` at 8 kHz, Hann-windowed, as a fraction of i16
/// full scale (a strong 1 kHz tone does not leak into a weak tone's bin).
fn amp8k_hann(x: &[i16], hz: f32) -> f32 {
    let n = x.len() as f32;
    let w = std::f32::consts::TAU * hz / 8_000.0;
    let (mut re, mut im) = (0.0f64, 0.0f64);
    for (i, &v) in x.iter().enumerate() {
        let hann = 0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / n).cos();
        let v = f64::from(hann * f32::from(v));
        re += v * f64::from((w * i as f32).cos());
        im -= v * f64::from((w * i as f32).sin());
    }
    (4.0 * (re * re + im * im).sqrt() / f64::from(n) / 32_767.0) as f32
}

/// Every CTCSS tone through the whole engine, carrier squelch, default
/// params (750 µs de-emphasis) and with de-emphasis off: in the recorded PCM
/// each tone is at least 35 dB below a 1 kHz tone of the same deviation (the
/// tone is sent at 600 Hz deviation, the 1 kHz audio at 2 kHz). With the old
/// 4th-order Butterworth high-pass the worst tone (254.1 Hz) was +3.0 dB with
/// de-emphasis and −6.7 dB without; now −47.6 and −59.9 dB.
#[test]
fn recorded_audio_has_no_ctcss_hum() {
    let dev = Deviation::NARROW;
    let same_deviation = 20.0 * (dev.audio_hz / dev.subaudio_hz).log10();
    for deemphasis_us in [750, 0] {
        let mut worst = f32::MIN;
        for &tenths in CTCSS_TONES.iter() {
            let t = Tone::Ctcss(tenths);
            let hz = f32::from(tenths) / 10.0;
            let iq = keyed(
                Audio::Sine { hz: 1_000.0 },
                Some(t),
                40.0,
                samples(100)..samples(2_500),
            )
            .generate(samples(2_600));
            let p = ChannelParams {
                deemphasis_us,
                ..ChannelParams::default()
            };
            let mut e = NbfmChannel::new(p);
            let run = drive(&mut e, &iq, 1_000, 0);
            let pcm = &run.calls_pcm()[0];
            // One settled second from the middle of the call.
            let seg = &pcm[8 * 800..8 * 1_800];
            let voice = amp8k_hann(seg, 1_000.0);
            assert!(voice > 0.5, "{t}: 1 kHz level {voice}");
            let rel = 20.0 * (amp8k_hann(seg, hz) / voice).log10() + same_deviation;
            worst = worst.max(rel);
            assert!(
                rel <= -35.0,
                "de-emphasis {deemphasis_us} µs, {t}: {rel:.1} dB"
            );
        }
        eprintln!("de-emphasis {deemphasis_us} µs: worst CTCSS {worst:.1} dB re 1 kHz");
    }
}

/// A strong VHF neighbour 7.5 kHz away (+30 dB over the noise, voice or
/// band-limited noise at ±2.5 kHz peak deviation, either side), keyed after
/// the floor has settled, must not open a carrier-squelch channel. The
/// 12.5 kHz channel filter (passband 4 kHz, stopband 6.5 kHz) keeps it below
/// `open_db`; with the old 5.5 / 8 kHz filter voice opened at +30 dB.
/// (Limit: a *steady* full-deviation test tone on the neighbour still opens
/// from about +25 dB — its Bessel sidebands land inside this channel's own
/// occupied band, where no channel filter can remove them.)
#[test]
fn strong_neighbour_7_5_khz_away_does_not_open_carrier_squelch() {
    for offset_hz in [7_500.0, -7_500.0] {
        for audio in [Audio::Voice, Audio::Noise] {
            let iq = NbfmSignal {
                snr_db: 30.0,
                offset_hz,
                deviation: Deviation {
                    audio_hz: 1_900.0,
                    subaudio_hz: 600.0,
                },
                seed: 7,
                // Noise alone first, so the floor settles on it.
                keyed: vec![samples(3_000)..samples(10_000)],
                ..NbfmSignal::new(audio.clone(), Some(tone("100.0")))
            }
            .generate(samples(10_000));
            let mut e = NbfmChannel::new(ChannelParams::default());
            let run = drive(&mut e, &iq, 1_000, 0);
            let st = e.stats();
            eprintln!(
                "neighbour {offset_hz} Hz {audio:?}: snr {:.1} dB, events {}, false opens {}",
                st.snr_db,
                run.events.len(),
                st.false_opens
            );
            assert!(
                run.starts().is_empty(),
                "{offset_hz} Hz {audio:?} opened: {:?}",
                run.starts()
            );
        }
    }
}

/// On-channel voice at the full ±2.5 kHz deviation (1.9 kHz audio + 600 Hz
/// tone) passes the channel filter undistorted: the peak-deviation
/// measurement stays within 10 % and the PCM's 1 kHz tone has no harmonic
/// above −30 dB.
#[test]
fn full_deviation_on_channel_is_not_distorted() {
    let iq = NbfmSignal {
        snr_db: 30.0,
        deviation: Deviation {
            audio_hz: 1_900.0,
            subaudio_hz: 600.0,
        },
        keyed: vec![samples(200)..samples(3_200)],
        ..NbfmSignal::new(Audio::Sine { hz: 1_000.0 }, Some(tone("100.0")))
    }
    .generate(samples(4_500));
    let mut e = NbfmChannel::new(ChannelParams::default());
    let run = drive(&mut e, &iq, 1_000, 0);
    let en = run.ends();
    assert_eq!(en.len(), 1);
    let peak = en[0].peak_deviation_hz;
    let pcm = &run.calls_pcm()[0];
    let seg = &pcm[8 * 1_000..8 * 2_000];
    let fund = amp8k_hann(seg, 1_000.0);
    let h2 = 20.0 * (amp8k_hann(seg, 2_000.0) / fund).log10();
    let h3 = 20.0 * (amp8k_hann(seg, 3_000.0) / fund).log10();
    eprintln!("peak deviation {peak:.0} Hz, 1 kHz {fund:.3}, H2 {h2:.1} dB, H3 {h3:.1} dB");
    assert!((peak - 2_500.0).abs() <= 250.0, "peak deviation {peak}");
    assert!(h2 < -30.0 && h3 < -30.0, "H2 {h2} dB, H3 {h3} dB");
}

/// 10 minutes of noise in 1 s pieces through a carrier-squelch engine and a
/// tone-gated one. `burst_db`: every 10 s, 2 s of noise this much louder.
fn noise_run(
    burst_db: f32,
) -> (
    trunkd_analog::ChannelStats,
    trunkd_analog::ChannelStats,
    usize,
    usize,
) {
    // Real detectors (CTCSS + DCS on carrier squelch, CTCSS on the gated one).
    let mut carrier = NbfmChannel::new(ChannelParams::default());
    let mut gated = NbfmChannel::new(ChannelParams::with_tone(Some(tone("100.0"))));
    let (mut ev_c, mut ev_g) = (0usize, 0usize);
    let base = 1e-4f32;
    for sec in 0..600u64 {
        let p = if sec % 10 >= 8 {
            base * 10f32.powf(burst_db / 10.0)
        } else {
            base
        };
        let iq = testsig::noise(FS, p, 0x5EED + sec);
        carrier.process(&iq, sec * 1000, &mut |_| ev_c += 1);
        gated.process(&iq, sec * 1000, &mut |_| ev_g += 1);
    }
    (carrier.stats(), gated.stats(), ev_c, ev_g)
}

#[test]
fn no_false_opens_on_ten_minutes_of_steady_noise() {
    let (c, g, ev_c, ev_g) = noise_run(0.0);
    assert!(c.calls <= 1, "carrier squelch opened {} times", c.calls);
    assert_eq!(g.calls, 0);
    assert_eq!(ev_g, 0);
    assert!(ev_c == 0 || c.calls == 1);
    assert_eq!(c.state, ChannelState::Closed);
}

#[test]
fn noise_bursts_are_vetoed_not_recorded() {
    // +3 dB bursts stay under open_db; +12 dB bursts pass the power test and
    // must be caught by the discriminator noise veto.
    for burst in [3.0, 12.0] {
        let (c, g, ev_c, ev_g) = noise_run(burst);
        assert!(
            c.calls <= 1,
            "{burst} dB: carrier squelch {} calls",
            c.calls
        );
        assert!(ev_c == 0 || c.calls == 1);
        assert_eq!((g.calls, ev_g), (0, 0), "{burst} dB: tone-gated opened");
        if burst > 10.0 {
            assert!(c.false_opens >= 30, "{burst} dB: {} vetoes", c.false_opens);
        } else {
            assert_eq!(c.false_opens, 0);
        }
    }
}

#[test]
fn kerchunk_is_dropped_and_min_length_call_kept() {
    let mut iq =
        keyed(Audio::Voice, None, 20.0, samples(500)..samples(800)).generate(samples(3_000));
    iq.extend(
        keyed(Audio::Voice, None, 20.0, samples(500)..samples(1_200)).generate(samples(3_000)),
    );
    let mut e = NbfmChannel::new(ChannelParams::default());
    let run = drive(&mut e, &iq, 300, 0);
    check_contract(&run.events);
    assert_eq!(e.stats().calls_too_short, 1);
    let s = run.starts();
    assert_eq!(s.len(), 1);
    assert!(s[0].at_ms.abs_diff(3_500) <= 10);
    assert!(run.ends()[0].duration_ms.abs_diff(700) <= 25);
}

#[test]
fn max_call_splits_without_losing_audio() {
    let mut p = ChannelParams::default();
    p.squelch.max_call_s = 2;
    let iq = keyed(
        Audio::Sine { hz: 1_000.0 },
        None,
        20.0,
        samples(500)..samples(5_500),
    )
    .generate(samples(7_000));
    let mut e = NbfmChannel::new(p);
    let run = drive(&mut e, &iq, 2_000, 0);
    check_contract(&run.events);
    let s = run.starts();
    let en = run.ends();
    assert_eq!((s.len(), en.len()), (3, 3));
    assert_eq!(en[0].reason, EndReason::MaxDuration);
    assert_eq!(en[1].reason, EndReason::MaxDuration);
    assert_eq!(en[2].reason, EndReason::SquelchClosed);
    assert_eq!(en[0].duration_ms, 2_000);
    assert_eq!(en[1].duration_ms, 2_000);
    assert_eq!(s[1].at_ms, en[0].at_ms);
    assert_eq!(s[2].at_ms, en[1].at_ms);
    let total: u64 = en.iter().map(|x| x.duration_ms).sum();
    assert!(total.abs_diff(5_000) <= 30, "{total}");
    // The 1 kHz runs straight across the split.
    let pcm = run.calls_pcm();
    let tail = &pcm[0][pcm[0].len() - 160..];
    let head = &pcm[1][..160];
    let a = amp8k(tail, 1_000.0);
    assert!(a > 0.5 && (amp8k(head, 1_000.0) - a).abs() < 0.05);
    assert_eq!(e.stats().calls, 3);
}

#[test]
fn stats_and_call_measurements() {
    let t = tone("167.9");
    let iq = NbfmSignal {
        snr_db: 20.0,
        offset_hz: 400.0,
        keyed: vec![samples(1_000)..samples(4_000)],
        ..NbfmSignal::new(Audio::Sine { hz: 1_000.0 }, Some(t))
    }
    .generate(samples(6_000));
    let (mut e, _) = goertzel_engine(ChannelParams::default());
    let before = e.stats();
    assert_eq!(before.state, ChannelState::Closed);
    assert!(!before.is_open());
    let run = drive(&mut e, &iq[..samples(3_000)], 500, 0);
    let mid = e.stats();
    assert!(mid.is_open());
    assert_eq!(mid.state, ChannelState::Open);
    assert!((mid.snr_db - 20.0).abs() < 3.0, "snr {}", mid.snr_db);
    assert!(mid.noise_floor_dbfs < mid.power_dbfs - 15.0);
    assert!(
        (mid.power_dbfs - 0.0).abs() < 1.0,
        "power {}",
        mid.power_dbfs
    );
    assert_eq!(mid.tone_detected, Some(t));
    assert_eq!(mid.samples_in, samples(3_000) as u64);
    assert_eq!(run.starts()[0].tone_detected, Some(t));

    let run2 = drive(&mut e, &iq[samples(3_000)..], 500, samples(3_000));
    let end = run2.ends()[0];
    let st = e.stats();
    assert!(!st.is_open());
    assert_eq!(st.calls, 1);
    assert_eq!(st.last_call_end_ms, Some(end.at_ms));
    assert!(
        (st.offset_hz - 400.0).abs() < 30.0,
        "offset {}",
        st.offset_hz
    );
    assert!((end.offset_hz - 400.0).abs() < 30.0);
    assert_eq!(end.tone_detected, Some(t));
    // 2 kHz audio + 600 Hz sub-audio, offset removed.
    assert!(
        (2_300.0..3_000.0).contains(&end.peak_deviation_hz),
        "peak {}",
        end.peak_deviation_hz
    );
    assert!((end.snr_db - 20.0).abs() < 3.0);
}

#[test]
fn chunking_does_not_change_the_events() {
    let iq = NbfmSignal {
        snr_db: 12.0,
        // A call, a dropped kerchunk, a second call bridging a 400 ms drop.
        keyed: vec![
            samples(300)..samples(1_900),
            samples(3_000)..samples(3_300),
            samples(4_500)..samples(5_300),
            samples(5_700)..samples(6_500),
        ],
        ..NbfmSignal::new(Audio::Voice, Some(tone("123.0")))
    }
    .generate(samples(8_000));
    let events = |chunk: usize| {
        let (mut e, _) = goertzel_engine(ChannelParams::default());
        let run = drive(&mut e, &iq, chunk, 0);
        check_contract(&run.events);
        run.events.into_iter().map(|(_, e)| e).collect::<Vec<_>>()
    };
    let a = events(77);
    assert_eq!(
        a.iter()
            .filter(|e| matches!(e, ChannelEvent::Started(_)))
            .count(),
        2
    );
    assert_eq!(a, events(4_096));
    assert_eq!(a, events(1));
}

#[test]
fn finish_ends_an_open_call_with_shutdown() {
    let iq =
        keyed(Audio::Voice, None, 20.0, samples(500)..samples(10_000)).generate(samples(2_000));
    let mut e = NbfmChannel::new(ChannelParams::default());
    let mut run = drive(&mut e, &iq, 1_000, 0);
    let ev = &mut run.events;
    e.finish(2_000, &mut |x| ev.push((2_000, x)));
    check_contract(&run.events);
    let en = run.ends();
    assert_eq!(en.len(), 1);
    assert_eq!(en[0].reason, EndReason::Shutdown);
    assert!(en[0].duration_ms.abs_diff(1_500) <= 25);
    // Nothing more after finish on a closed channel.
    let mut n = 0;
    e.finish(2_000, &mut |_| n += 1);
    assert_eq!(n, 0);
}

#[test]
fn wide_channel_and_no_deemphasis() {
    let mut p = ChannelParams {
        bandwidth: Bandwidth::Wide,
        deemphasis_us: 0,
        ..ChannelParams::default()
    };
    p.squelch.hang_ms = 500;
    let iq = NbfmSignal {
        snr_db: 15.0,
        deviation: Deviation::WIDE,
        keyed: vec![samples(500)..samples(2_500)],
        ..NbfmSignal::new(Audio::Sine { hz: 1_000.0 }, None)
    }
    .generate(samples(4_000));
    let mut e = NbfmChannel::new(p);
    let run = drive(&mut e, &iq, 1_000, 0);
    check_contract(&run.events);
    let en = run.ends();
    assert_eq!(en.len(), 1);
    assert!(en[0].duration_ms.abs_diff(2_000) <= 25);
    let pcm = &run.calls_pcm()[0];
    // 4 kHz of 5 kHz full scale at 0.8 PCM gain: ≈ 0.64.
    let a = amp8k(&pcm[8 * 500..8 * 1_500], 1_000.0);
    assert!((a - 0.64).abs() < 0.06, "level {a}");
}

/// Per-channel cost: `cargo test --release -p trunkd-analog --test engine
/// bench_channels -- --ignored --nocapture`. 60 s of a busy channel (voice +
/// CTCSS, keyed 2/3 of the time, 12 dB SNR) through one engine: without
/// detectors (the FM chain, squelch and segmenter alone), with the CTCSS
/// detector (a CTCSS-gated channel), and with CTCSS + DCS (carrier squelch,
/// the most expensive case).
#[test]
#[ignore]
fn bench_channels() {
    let secs = 60usize;
    let mut keyed_ranges = Vec::new();
    let mut t = 0;
    while t + 3 * FS <= secs * FS {
        keyed_ranges.push(t..t + 2 * FS);
        t += 3 * FS;
    }
    let iq = NbfmSignal {
        snr_db: 12.0,
        keyed: keyed_ranges,
        ..NbfmSignal::new(Audio::Voice, Some(tone("167.9")))
    }
    .generate(secs * FS);
    for case in ["chain only", "ctcss-gated", "carrier (ctcss+dcs)"] {
        let mut e = match case {
            "chain only" => NbfmChannel::with_detectors(ChannelParams::default(), Vec::new()),
            "ctcss-gated" => NbfmChannel::new(ChannelParams::with_tone(Some(tone("167.9")))),
            _ => NbfmChannel::new(ChannelParams::default()),
        };
        let mut n = 0usize;
        let t0 = std::time::Instant::now();
        for (i, c) in iq.chunks(2_500).enumerate() {
            e.process(c, (i * 100) as u64, &mut |_| n += 1);
        }
        let el = t0.elapsed();
        let us_per_s = el.as_secs_f64() * 1e6 / secs as f64;
        println!(
            "bench_channels: {case}: {us_per_s:.0} µs per channel per second of signal \
             ({:.3} % of one core), {} events, {} calls",
            us_per_s / 1e4,
            n,
            e.stats().calls
        );
        assert!(e.stats().calls >= 15);
    }
}
