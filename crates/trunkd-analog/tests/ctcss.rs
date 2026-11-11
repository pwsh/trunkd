// SPDX-License-Identifier: GPL-3.0-or-later
//! CTCSS detector accuracy, close-pair separation, adjacent-tone rejection,
//! false-detection, latency and allocation tests.
//!
//! Signals are synthetic (`testsig`): an NBFM transmission at 25 ksps is
//! demodulated by a reference chain matching the engine's sub-audio path
//! (polar discriminator, normalised to the 12.5 kHz channel's ±2.5 kHz full
//! scale, 2nd-order 300 Hz low-pass, decimate by 10 → 2.5 ksps).

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::f64::consts::TAU;

use trunkd_analog::ctcss::{CtcssDetector, ToneDecision, BLOCK_SAMPLES, WINDOW_MS};
use trunkd_analog::testsig::{self, Audio, NbfmSignal};
use trunkd_analog::tone::{Tone, CTCSS_TONES, DCS_CODES};
use trunkd_analog::{Bandwidth, INPUT_RATE_HZ, SUBAUDIO_RATE_HZ};

// ---- allocation counting (this test binary only) -------------------------

struct Counting;

thread_local! {
    static ALLOCS: Cell<u64> = const { Cell::new(0) };
}

// SAFETY: every method forwards to `System` unchanged; the counter is a
// thread-local `Cell` that never allocates.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let _ = ALLOCS.try_with(|c| c.set(c.get() + 1));
        // SAFETY: forwarded with the caller's own layout.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let _ = ALLOCS.try_with(|c| c.set(c.get() + 1));
        // SAFETY: as above.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let _ = ALLOCS.try_with(|c| c.set(c.get() + 1));
        // SAFETY: as above.
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: as above.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

fn allocations_during(f: impl FnOnce()) -> u64 {
    let before = ALLOCS.with(Cell::get);
    f();
    ALLOCS.with(Cell::get) - before
}

// ---- reference sub-audio chain --------------------------------------------

/// 25 ksps samples per 2.5 ksps sub-audio sample.
const DECIM: usize = (INPUT_RATE_HZ / SUBAUDIO_RATE_HZ) as usize;

/// Sub-audio samples → ms.
fn ms(samples: u64) -> u64 {
    samples * 1000 / u64::from(SUBAUDIO_RATE_HZ)
}

/// Discriminator → ±1.0 full scale (narrowband) → 2nd-order Butterworth
/// low-pass at 300 Hz → every 10th sample.
fn subaudio(iq: &[num_complex::Complex<f32>]) -> Vec<f32> {
    let fs = f64::from(INPUT_RATE_HZ);
    let full_scale = Bandwidth::Narrow.max_deviation_hz();
    let f = testsig::instantaneous_frequency_hz(iq, fs);
    let w = TAU * 300.0 / fs;
    let (cos, alpha) = (w.cos(), w.sin() / std::f64::consts::SQRT_2);
    let a0 = 1.0 + alpha;
    let b = [
        (1.0 - cos) / 2.0 / a0,
        (1.0 - cos) / a0,
        (1.0 - cos) / 2.0 / a0,
    ];
    let a = [-2.0 * cos / a0, (1.0 - alpha) / a0];
    let mut z = [0.0f64; 2];
    let mut out = Vec::with_capacity(f.len() / DECIM + 1);
    for (i, &v) in f.iter().enumerate() {
        let x = f64::from(v / full_scale);
        let y = b[0] * x + z[0];
        z[0] = b[1] * x - a[0] * y + z[1];
        z[1] = b[2] * x - a[1] * y;
        if i % DECIM == 0 {
            out.push(y as f32);
        }
    }
    out
}

/// Narrowband NBFM with voice, `tone`, +150 Hz carrier offset at `snr_db`,
/// as sub-audio.
fn transmission(tone: Option<Tone>, snr_db: f32, ms: usize, seed: u64) -> Vec<f32> {
    let mut s = NbfmSignal::new(Audio::Voice, tone);
    s.offset_hz = 150.0;
    s.snr_db = snr_db;
    s.seed = seed;
    subaudio(&s.generate(ms * INPUT_RATE_HZ as usize / 1000))
}

/// Push `x` in 50-sample slices (not aligned with the 125-sample blocks) and
/// collect every decision change.
fn run(det: &mut CtcssDetector, x: &[f32]) -> Vec<ToneDecision> {
    x.chunks(50).filter_map(|c| det.push(c)).collect()
}

fn ctcss(tenths: u16) -> Tone {
    Tone::ctcss(tenths).unwrap()
}

/// Assert `decisions` is exactly one lock onto `tone`, within `max_ms` of
/// sample `from`, and return the latency in ms.
fn assert_single_lock(
    decisions: &[ToneDecision],
    tone: Tone,
    from: u64,
    max_ms: u64,
    what: &str,
) -> u64 {
    assert!(!decisions.is_empty(), "{what}: never detected");
    assert!(
        decisions.iter().all(|d| d.tone == Some(tone)),
        "{what}: wrong decisions {decisions:?}"
    );
    assert_eq!(decisions.len(), 1, "{what}: lock not stable {decisions:?}");
    let latency = ms(decisions[0].at_sample - from);
    assert!(latency <= max_ms, "{what}: locked after {latency} ms");
    latency
}

// ---- accuracy -------------------------------------------------------------

#[test]
fn all_tones_detect_at_20_10_6_db() {
    let mut worst = [0u64; 3];
    for (si, snr) in [20.0f32, 10.0, 6.0].into_iter().enumerate() {
        for &tenths in &CTCSS_TONES {
            for seed in [1u64, 2] {
                let t = ctcss(tenths);
                let x = transmission(Some(t), snr, 2_000, seed + u64::from(tenths));
                let mut det = CtcssDetector::new();
                let d = run(&mut det, &x);
                let what = format!("{t} at {snr} dB seed {seed}");
                let lat = assert_single_lock(&d, t, 0, 500, &what);
                worst[si] = worst[si].max(lat);
                assert_eq!(det.current(), Some(t), "{what}");
            }
        }
    }
    eprintln!("worst lock latency from a fresh detector, 20/10/6 dB: {worst:?} ms");
}

#[test]
fn lock_latency_from_carrier_up_without_reset() {
    // 1 s of noise only (no carrier: full-scale discriminator noise), then the
    // carrier keys up with the tone. The detector is not reset at carrier-up,
    // so its window starts full of noise.
    let mut worst = [0u64; 3];
    for (si, snr) in [20.0f32, 10.0, 6.0].into_iter().enumerate() {
        for (i, &tenths) in CTCSS_TONES.iter().enumerate() {
            let t = ctcss(tenths);
            let mut s = NbfmSignal::new(Audio::Voice, Some(t));
            s.snr_db = snr;
            s.offset_hz = -200.0;
            s.seed = 100 + i as u64;
            let key = INPUT_RATE_HZ as usize; // 1 s
            s.keyed.push(key..3 * key);
            let x = subaudio(&s.generate(3 * key));
            let mut det = CtcssDetector::new();
            let d = run(&mut det, &x);
            let what = format!("{t} at {snr} dB after carrier-up");
            let lat = assert_single_lock(&d, t, SUBAUDIO_RATE_HZ.into(), 500, &what);
            worst[si] = worst[si].max(lat);
        }
    }
    eprintln!("worst lock latency from carrier-up (no reset), 20/10/6 dB: {worst:?} ms");
}

// ---- close pairs (HANDOFF plan correction) --------------------------------

/// The four standard pairs closer than 2.6 Hz.
const CLOSE_PAIRS: [(u16, u16); 4] = [(670, 693), (1598, 1622), (1655, 1679), (1713, 1738)];

#[test]
fn close_pairs_are_separated() {
    for (a, b) in CLOSE_PAIRS {
        for (sent, other) in [(a, b), (b, a)] {
            for snr in [20.0f32, 10.0, 6.0] {
                for seed in 1..=4u64 {
                    let t = ctcss(sent);
                    let x = transmission(Some(t), snr, 3_000, seed * 31 + u64::from(sent));
                    let mut det = CtcssDetector::new();
                    let d = run(&mut det, &x);
                    let what = format!("{t} (neighbour {}) at {snr} dB seed {seed}", ctcss(other));
                    assert_single_lock(&d, t, 0, 500, &what);
                    let e = det.last_evaluation().unwrap();
                    assert_eq!(e.best, t, "{what}");
                    assert!(e.runner_up_db >= 6.0, "{what}: margin {e:?}");
                }
            }
        }
    }
}

#[test]
fn close_pair_margin_noiseless() {
    // Noiseless: the neighbour sits near the window's first null.
    for (a, b) in CLOSE_PAIRS {
        for sent in [a, b] {
            let x = transmission(Some(ctcss(sent)), f32::INFINITY, 1_000, 1);
            let mut det = CtcssDetector::new();
            run(&mut det, &x);
            let e = det.last_evaluation().unwrap();
            assert_eq!(e.best, ctcss(sent));
            assert!(e.runner_up_db >= 13.0, "{sent}: {e:?}");
            assert!((e.level - 0.24).abs() < 0.03, "{sent}: level {}", e.level);
        }
    }
}

#[test]
fn off_frequency_encoders_within_0_3_percent() {
    // A transmitter tone 0.3 % off frequency, a carrier-offset DC term and a
    // little white noise, fed straight in as sub-audio (no RF chain). Every
    // tone, including both sides of the close pairs, still locks correctly.
    let fs = f64::from(SUBAUDIO_RATE_HZ);
    for &sent in &CTCSS_TONES {
        for err in [-0.003f64, 0.003] {
            let hz = f64::from(sent) / 10.0 * (1.0 + err);
            let mut rng = 0x1234_5678_u32 ^ u32::from(sent);
            let x: Vec<f32> = (0..2 * SUBAUDIO_RATE_HZ as usize)
                .map(|n| {
                    rng ^= rng << 13;
                    rng ^= rng >> 17;
                    rng ^= rng << 5;
                    let noise = (rng as f32 / u32::MAX as f32 - 0.5) * 0.05;
                    0.24 * (TAU * hz * n as f64 / fs).sin() as f32 + 0.1 + noise
                })
                .collect();
            let mut det = CtcssDetector::new();
            let d = run(&mut det, &x);
            assert_single_lock(&d, ctcss(sent), 0, 500, &format!("{sent} {err:+}"));
        }
    }
}

// ---- adjacent-tone rejection ----------------------------------------------

#[test]
fn adjacent_tones_do_not_match_configured_100_0() {
    let configured = ctcss(1000);
    for sent in [1035u16, 974] {
        for snr in [20.0f32, 10.0, 6.0] {
            for seed in 1..=3u64 {
                let t = ctcss(sent);
                let x = transmission(Some(t), snr, 3_000, seed);
                let mut det = CtcssDetector::new();
                let d = run(&mut det, &x);
                assert!(
                    d.iter().all(|d| d.tone != Some(configured)),
                    "{t} at {snr} dB matched 100.0: {d:?}"
                );
                assert_single_lock(&d, t, 0, 500, &format!("{t} at {snr} dB"));
            }
        }
    }
}

// ---- false detections -----------------------------------------------------

#[test]
fn noise_only_never_detects() {
    // No carrier at all: 2 × 60 s of channel noise.
    for seed in [7u64, 8] {
        let iq = testsig::noise(60 * INPUT_RATE_HZ as usize, 1.0, seed);
        let x = subaudio(&iq);
        let mut det = CtcssDetector::new();
        let d = run(&mut det, &x);
        assert!(d.is_empty(), "seed {seed}: {d:?}");
    }
}

#[test]
fn carrier_without_tone_never_detects() {
    for audio in [Audio::Voice, Audio::Noise, Audio::Silence] {
        for snr in [f32::INFINITY, 20.0, 6.0, 3.0] {
            let mut s = NbfmSignal::new(audio.clone(), None);
            s.snr_db = snr;
            s.offset_hz = 300.0;
            let x = subaudio(&s.generate(20 * INPUT_RATE_HZ as usize));
            let mut det = CtcssDetector::new();
            let d = run(&mut det, &x);
            assert!(d.is_empty(), "{audio:?} at {snr} dB: {d:?}");
        }
    }
}

#[test]
fn dcs_never_detects_as_ctcss() {
    for &code in &DCS_CODES {
        for inverted in [false, true] {
            let t = Tone::dcs(code, inverted).unwrap();
            let x = transmission(Some(t), 20.0, 2_000, u64::from(code));
            let mut det = CtcssDetector::new();
            let d = run(&mut det, &x);
            assert!(d.is_empty(), "{t}: {d:?}");
        }
    }
}

// ---- loss, switch, timing ---------------------------------------------------

#[test]
fn tone_loss_and_switch_while_carrier_stays_up() {
    let a = ctcss(1679);
    let b = ctcss(1655);
    for snr in [20.0f32, 6.0] {
        // 2 s of A, then 2 s with no tone, then 2 s of B (carrier throughout;
        // the segments are demodulated together, one phase step at each join).
        let n = 2 * INPUT_RATE_HZ as usize;
        let mut iq = Vec::new();
        for (i, tone) in [Some(a), None, Some(b)].into_iter().enumerate() {
            let mut s = NbfmSignal::new(Audio::Voice, tone);
            s.snr_db = snr;
            s.seed = 50 + i as u64;
            iq.extend(s.generate(n));
        }
        let x = subaudio(&iq);
        let mut det = CtcssDetector::new();
        let d = run(&mut det, &x);
        assert_eq!(d.len(), 3, "{snr} dB: {d:?}");
        let sec = u64::from(SUBAUDIO_RATE_HZ);
        assert_eq!(d[0].tone, Some(a));
        assert_eq!(d[1].tone, None);
        let drop = ms(d[1].at_sample - 2 * sec);
        assert!(drop <= 500, "{snr} dB: lost after {drop} ms");
        assert_eq!(d[2].tone, Some(b));
        let lock = ms(d[2].at_sample - 4 * sec);
        assert!(lock <= 500, "{snr} dB: B locked after {lock} ms");
        eprintln!("{snr} dB: drop {drop} ms, relock {lock} ms");
    }
    // Direct switch A → B (another user on the frequency): no Lost needed.
    let n = 2 * INPUT_RATE_HZ as usize;
    let mut iq = NbfmSignal::new(Audio::Voice, Some(a)).generate(n);
    iq.extend(NbfmSignal::new(Audio::Voice, Some(b)).generate(n));
    let mut det = CtcssDetector::new();
    let d = run(&mut det, &subaudio(&iq));
    assert_eq!(d.first().and_then(|d| d.tone), Some(a), "{d:?}");
    assert_eq!(d.last().and_then(|d| d.tone), Some(b), "{d:?}");
    assert_eq!(det.current(), Some(b));
}

/// Low-deviation tone (250 Hz) with voice at 15 dB SNR, as sub-audio, plus
/// `extra(t)` added per sample (`t` in seconds).
fn low_tone_with(tone: Tone, secs: usize, extra: impl Fn(f64) -> f64) -> Vec<f32> {
    let mut s = NbfmSignal::new(Audio::Voice, Some(tone));
    s.deviation.subaudio_hz = 250.0;
    s.snr_db = 15.0;
    s.seed = 77;
    let mut x = subaudio(&s.generate(secs * INPUT_RATE_HZ as usize));
    let fs = f64::from(SUBAUDIO_RATE_HZ);
    for (i, v) in x.iter_mut().enumerate() {
        *v += extra(i as f64 / fs) as f32;
    }
    x
}

#[test]
fn mains_harmonic_at_180_hz_does_not_split_a_179_9_call() {
    // A strong 180 Hz mains harmonic (3 x 60 Hz, 0.1 Hz from the 179.9 Hz
    // tone, so it beats with it every 10 s) over a low-deviation 179.9 tone:
    // one lock, never lost, for 20 s.
    let t = ctcss(1799);
    for hum in [0.15, 0.25] {
        let x = low_tone_with(t, 20, |s| hum * (TAU * 180.0 * s).sin());
        let mut det = CtcssDetector::new();
        let d = run(&mut det, &x);
        assert_eq!(d.len(), 1, "hum {hum}: {d:?}");
        assert_eq!(d[0].tone, Some(t), "hum {hum}: {d:?}");
    }
}

#[test]
fn a_brief_competing_line_does_not_split_a_call() {
    // Voice harmonics: a 186.2 Hz line (the next tone up) in 300 ms bursts
    // every second, up to 5 dB stronger than the locked 179.9 Hz tone. The
    // lock holds: a different winner must beat it by 6 dB for two hops.
    let t = ctcss(1799);
    let tone_amp = 250.0 / f64::from(Bandwidth::Narrow.max_deviation_hz());
    let burst = tone_amp * 10f64.powf(5.0 / 20.0);
    let x = low_tone_with(t, 10, |s| {
        if s.fract() < 0.3 && s > 1.0 {
            burst * (TAU * 186.2 * s).sin()
        } else {
            0.0
        }
    });
    let mut det = CtcssDetector::new();
    let d = run(&mut det, &x);
    assert_eq!(d.len(), 1, "{d:?}");
    assert_eq!(d[0].tone, Some(t));

    // A line 10 dB stronger that stays drops the lock and is taken over.
    let other = ctcss(1862);
    let strong = tone_amp * 10f64.powf(10.0 / 20.0);
    let x = low_tone_with(t, 6, |s| {
        if s > 3.0 {
            strong * (TAU * 186.2 * s).sin()
        } else {
            0.0
        }
    });
    let mut det = CtcssDetector::new();
    let d = run(&mut det, &x);
    assert_eq!(d.first().and_then(|d| d.tone), Some(t), "{d:?}");
    assert_eq!(det.current(), Some(other), "{d:?}");
}

#[test]
fn slicing_does_not_change_decisions() {
    let x = transmission(Some(ctcss(1035)), 10.0, 3_000, 9);
    let collect = |size: usize| -> Vec<ToneDecision> {
        let mut det = CtcssDetector::new();
        x.chunks(size).filter_map(|c| det.push(c)).collect()
    };
    let one = collect(1);
    assert_eq!(one.len(), 1);
    assert_eq!(one, collect(BLOCK_SAMPLES));
    assert_eq!(one, collect(77));
    // One big push returns the last change.
    let mut det = CtcssDetector::new();
    assert_eq!(det.push(&x), one.last().copied());
    assert_eq!(one[0].at_ms(), u64::from(WINDOW_MS) + 50);
}

#[test]
fn reset_forgets_the_lock() {
    let t = ctcss(2541);
    let x = transmission(Some(t), 20.0, 1_000, 3);
    let mut det = CtcssDetector::new();
    run(&mut det, &x);
    assert_eq!(det.current(), Some(t));
    det.reset();
    assert_eq!(det.current(), None);
    assert_eq!(det.samples(), 0);
    assert_eq!(det.last_evaluation(), None);
    let d = run(&mut det, &x);
    assert_single_lock(&d, t, 0, 500, "after reset");
}

#[test]
fn no_allocation_after_new() {
    let x = transmission(Some(ctcss(1273)), 10.0, 10_000, 4);
    let mut det = CtcssDetector::new();
    let mut changes = 0;
    let n = allocations_during(|| {
        for c in x.chunks(37) {
            if det.push(c).is_some() {
                changes += 1;
            }
        }
        det.reset();
        for c in x.chunks(BLOCK_SAMPLES) {
            if det.push(c).is_some() {
                changes += 1;
            }
        }
    });
    assert_eq!(changes, 2);
    assert_eq!(n, 0, "allocations in push/reset");
}
