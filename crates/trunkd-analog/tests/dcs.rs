// SPDX-License-Identifier: GPL-3.0-or-later
//! Golay(23,12) and DCS detector acceptance tests, on synthetic signals only
//! (`testsig`).
//!
//! The receive chain in front of the detector is modelled here the way the
//! engine builds it: channel low-pass, polar discriminator, 300 Hz low-pass,
//! decimate by 10 to 2.5 ksps, 1 Hz DC tracker.

use std::f64::consts::TAU;

use num_complex::Complex;
use trunkd_analog::dcs::{DcsDecision, DcsDetector};
use trunkd_analog::golay;
use trunkd_analog::testsig::{Audio, NbfmSignal, RATE_HZ};
use trunkd_analog::tone::{Tone, CTCSS_TONES, DCS_CODES};
use trunkd_analog::SUBAUDIO_RATE_HZ;

// ---------------------------------------------------------------- Golay

#[test]
fn golay_round_trip_all_4096_words() {
    for data in 0..4096u16 {
        let w = golay::encode(data);
        assert_eq!(golay::syndrome(w), 0);
        for max in 0..=3 {
            let d = golay::decode(w, max).unwrap();
            assert_eq!((d.codeword, d.data, d.errors), (w, data, 0));
        }
    }
}

#[test]
fn golay_corrects_every_single_bit_error() {
    for data in 0..4096u16 {
        let w = golay::encode(data);
        for bit in 0..23 {
            let bad = w ^ (1 << bit);
            assert_ne!(golay::syndrome(bad), 0);
            let d = golay::decode(bad, 1).unwrap();
            assert_eq!(
                (d.codeword, d.data, d.errors),
                (w, data, 1),
                "{data:#x} bit {bit}"
            );
            assert_eq!(
                golay::decode(bad, 0),
                None,
                "strict refuses a corrected word"
            );
        }
    }
}

#[test]
fn golay_refuses_two_bit_errors_with_a_one_bit_budget_and_fixes_three() {
    for data in (0..4096u16).step_by(7) {
        let w = golay::encode(data);
        for a in 0..23 {
            for b in a + 1..23 {
                let bad = w ^ (1 << a) ^ (1 << b);
                assert_eq!(golay::decode(bad, 1), None);
                assert_eq!(golay::decode(bad, 2).unwrap().codeword, w);
                let c = (b + 5) % 23;
                if c != a && c != b {
                    let bad3 = bad ^ (1 << c);
                    let d = golay::decode(bad3, 3).unwrap();
                    assert_eq!((d.codeword, d.errors), (w, 3));
                }
            }
        }
    }
    // Bits above the 23-bit word are ignored.
    let w = golay::encode(0x8A5);
    assert_eq!(golay::decode(w | 0xFF80_0000, 0).unwrap().codeword, w);
}

// ------------------------------------------------------- receive chain

/// RBJ Butterworth low-pass biquad, f64.
struct Lpf {
    b: [f64; 3],
    a: [f64; 2],
    z: [f64; 2],
}

impl Lpf {
    fn new(fc: f64, fs: f64) -> Lpf {
        let w = TAU * fc / fs;
        let (sin, cos) = w.sin_cos();
        let alpha = sin / (2.0 * std::f64::consts::FRAC_1_SQRT_2);
        let a0 = 1.0 + alpha;
        let b1 = (1.0 - cos) / a0;
        Lpf {
            b: [b1 / 2.0, b1, b1 / 2.0],
            a: [-2.0 * cos / a0, (1.0 - alpha) / a0],
            z: [0.0; 2],
        }
    }

    fn run(&mut self, x: f64) -> f64 {
        let y = self.b[0] * x + self.z[0];
        self.z[0] = self.b[1] * x - self.a[0] * y + self.z[1];
        self.z[1] = self.b[2] * x - self.a[1] * y;
        y
    }
}

/// 25 ksps IQ → 2.5 ksps sub-audio in Hz, as the engine produces it.
fn subaudio(iq: &[Complex<f32>]) -> Vec<f32> {
    let fs = f64::from(RATE_HZ);
    // 4th-order 6 kHz channel filter on I and Q.
    let lpf = || Lpf::new(6_000.0, fs);
    let (mut i1, mut i2, mut q1, mut q2) = (lpf(), lpf(), lpf(), lpf());
    let mut sub = Lpf::new(300.0, fs);
    let alpha = TAU * 1.0 / f64::from(SUBAUDIO_RATE_HZ);
    let mut dc = 0.0f64;
    let mut last = Complex::new(1.0f64, 0.0);
    let mut out = Vec::with_capacity(iq.len() / 10 + 1);
    for (n, x) in iq.iter().enumerate() {
        let re = i2.run(i1.run(f64::from(x.re)));
        let im = q2.run(q1.run(f64::from(x.im)));
        let z = Complex::new(re, im);
        let d = z * last.conj();
        last = z;
        let hz = d.im.atan2(d.re) * fs / TAU;
        let s = sub.run(hz);
        if n % 10 == 9 {
            dc += alpha * (s - dc);
            out.push((s - dc) as f32);
        }
    }
    out
}

/// Sub-audio of a transmission of `tone`, starting `skip` input samples into
/// the generator's stream (so the word starts at an arbitrary bit and
/// rotation), `seconds` long.
fn received(
    tone: Option<Tone>,
    audio: Audio,
    offset_hz: f32,
    snr_db: f32,
    skip: usize,
    seconds: f32,
    seed: u64,
) -> Vec<f32> {
    let n = (seconds * RATE_HZ as f32) as usize;
    let mut s = NbfmSignal::new(audio, tone);
    s.offset_hz = offset_hz;
    s.snr_db = snr_db;
    s.seed = seed;
    let iq = s.generate(n + skip);
    subaudio(&iq[skip..])
}

/// Feed `x` in 20 ms blocks; the time (ms) of the first `Detected`, and every
/// decision seen.
fn run(det: &mut DcsDetector, x: &[f32]) -> (Option<u32>, Vec<DcsDecision>) {
    let block = SUBAUDIO_RATE_HZ as usize / 50;
    let mut first = None;
    let mut all = Vec::new();
    for (i, chunk) in x.chunks(block).enumerate() {
        if let Some(d) = det.push(chunk) {
            if first.is_none() && matches!(d, DcsDecision::Detected(_)) {
                first = Some(((i * block + chunk.len()) * 1000 / SUBAUDIO_RATE_HZ as usize) as u32);
            }
            all.push(d);
        }
    }
    (first, all)
}

// ------------------------------------------------------------- DCS

#[test]
fn every_code_both_polarities_at_10_db_with_300_hz_offset_within_400_ms() {
    let mut worst = 0;
    for (i, &code) in DCS_CODES.iter().enumerate() {
        for inverted in [false, true] {
            for offset in [300.0f32, -300.0] {
                let tone = Tone::dcs(code, inverted).unwrap();
                let skip = (i * 7_919 + usize::from(inverted) * 3_001 + offset as usize) % 4_300;
                let seed = (i * 4 + usize::from(inverted) * 2 + usize::from(offset > 0.0)) as u64;
                let x = received(Some(tone), Audio::Silence, offset, 10.0, skip, 0.6, seed);
                let mut det = DcsDetector::new(false);
                let (first, all) = run(&mut det, &x);
                let at = first.unwrap_or_else(|| panic!("{tone} {offset:+} Hz: no decision"));
                assert!(at <= 400, "{tone} {offset:+} Hz: detected at {at} ms");
                worst = worst.max(at);
                // Every decision is the transmitted signal, reported as the
                // normal-polarity member of its pair.
                for d in all {
                    let DcsDecision::Detected(got) = d else {
                        panic!("{tone}: unexpected {d:?}");
                    };
                    assert!(got.same_signal(&tone), "{tone} {offset:+} Hz: got {got}");
                    assert_eq!(
                        got,
                        if inverted {
                            tone.dcs_equivalent().unwrap()
                        } else {
                            tone
                        }
                    );
                }
            }
        }
    }
    eprintln!("worst-case DCS lock time {worst} ms");
}

#[test]
fn d445n_and_d445i() {
    let d445n: Tone = "D445N".parse().unwrap();
    let d445i: Tone = "D445I".parse().unwrap();
    let d043n: Tone = "D043N".parse().unwrap();

    let x = received(Some(d445n), Audio::Silence, 300.0, 10.0, 1_234, 3.0, 9);
    let mut det = DcsDetector::new(false);
    let (first, all) = run(&mut det, &x);
    assert!(first.unwrap() <= 400);
    assert_eq!(all, vec![DcsDecision::Detected(d445n)], "held for 3 s");
    assert_eq!(det.current(), Some(d445n));
    assert!(!det.current().unwrap().same_signal(&d445i));

    // D445I is D043N on air; it is reported as D043N and matches a D445I
    // configuration only through same_signal().
    let x = received(Some(d445i), Audio::Silence, -300.0, 10.0, 77, 1.0, 10);
    let mut det = DcsDetector::new(false);
    let (first, all) = run(&mut det, &x);
    assert!(first.unwrap() <= 400);
    assert_eq!(all, vec![DcsDecision::Detected(d043n)]);
    assert!(det.current().unwrap().same_signal(&d445i));
    assert!(!det.current().unwrap().same_signal(&d445n));

    // Strict mode on a clean signal.
    let x = received(Some(d445n), Audio::Silence, 0.0, 30.0, 500, 1.0, 11);
    let mut det = DcsDetector::new(true);
    assert_eq!(run(&mut det, &x).1, vec![DcsDecision::Detected(d445n)]);
}

#[test]
fn ctcss_only_never_yields_a_dcs_decision() {
    for (i, &tenths) in CTCSS_TONES.iter().enumerate() {
        let tone = Tone::ctcss(tenths).unwrap();
        for (j, snr) in [f32::INFINITY, 20.0, 10.0, 6.0].into_iter().enumerate() {
            let offset = if j % 2 == 0 { 300.0 } else { -300.0 };
            let x = received(
                Some(tone),
                Audio::Silence,
                offset,
                snr,
                i * 31,
                4.0,
                (i * 4 + j) as u64,
            );
            for strict in [false, true] {
                let mut det = DcsDetector::new(strict);
                let (_, all) = run(&mut det, &x);
                assert!(all.is_empty(), "{tone} at {snr} dB: {all:?}");
            }
        }
    }
}

#[test]
fn noise_and_plain_carrier_never_yield_a_decision() {
    for seed in 0..4u64 {
        // Noise only (no carrier).
        let mut s = NbfmSignal::new(Audio::Silence, None);
        s.snr_db = 0.0;
        s.keyed.push(0..0);
        s.seed = seed;
        let x = subaudio(&s.generate(25_000 * 20));
        let mut det = DcsDetector::new(false);
        assert!(run(&mut det, &x).1.is_empty(), "noise seed {seed}");
        // Carrier with voice-band noise audio, no tone.
        let x = received(None, Audio::Noise, 300.0, 10.0, 0, 10.0, seed);
        let mut det = DcsDetector::new(false);
        assert!(run(&mut det, &x).1.is_empty(), "carrier seed {seed}");
    }
}

#[test]
fn locks_after_noise_and_drops_after_the_carrier() {
    let tone: Tone = "D445N".parse().unwrap();
    let fs = RATE_HZ as usize;
    let mut s = NbfmSignal::new(Audio::Silence, Some(tone));
    s.snr_db = 10.0;
    s.offset_hz = -300.0;
    s.keyed.push(fs..3 * fs);
    let x = subaudio(&s.generate(5 * fs));
    let sub = SUBAUDIO_RATE_HZ as usize;

    // Left running on noise: locks, a little slower than from a reset.
    let mut det = DcsDetector::new(false);
    let (first, all) = run(&mut det, &x);
    let at = first.expect("locks") - 1000;
    assert!(at <= 600, "lock {at} ms after carrier-up");
    assert_eq!(all, vec![DcsDecision::Detected(tone), DcsDecision::Lost]);

    // Reset at carrier-up (what the engine does): within 400 ms.
    let mut det = DcsDetector::new(false);
    assert!(run(&mut det, &x[..sub]).1.is_empty());
    det.reset();
    let (first, _) = run(&mut det, &x[sub..3 * sub]);
    assert!(first.unwrap() <= 400);
    // Lost within three words of the carrier dropping.
    let (_, after) = run(&mut det, &x[3 * sub..3 * sub + sub * 52 / 100]);
    assert_eq!(after, vec![DcsDecision::Lost]);
    assert_eq!(det.current(), None);
}

#[test]
fn locks_under_voice_audio() {
    let tone: Tone = "D023N".parse().unwrap();
    let x = received(Some(tone), Audio::Voice, 300.0, 20.0, 999, 2.0, 3);
    let mut det = DcsDetector::new(false);
    let (first, all) = run(&mut det, &x);
    assert!(first.unwrap() <= 600, "{first:?}");
    assert!(
        all.iter().all(|d| *d == DcsDecision::Detected(tone)),
        "{all:?}"
    );
}
