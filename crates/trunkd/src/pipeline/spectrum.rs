// SPDX-License-Identifier: GPL-3.0-or-later
//! Opt-in wideband FFT spectrum frames (W4, trunkd half).
//!
//! trunkd stays **outbound-only**: when keyup asks a tuner for spectrum (the
//! per-tuner `spectrum` desired-state flag, W2), the control loop flips that
//! tuner's [`SpectrumHook::enabled`] atomic and the DSP thread starts, on a slow
//! duty cycle, computing one windowed FFT over a raw wideband block, quantizing
//! it to a compact `i8` dBFS array, and handing the frame to the control thread
//! (over a bounded, drop-oldest channel) which POSTs it to
//! `{keyup}/api/recorder/spectrum`.
//!
//! # Zero cost when off (hard requirement)
//!
//! [`SpectrumTap::observe`] is called once per DSP block. When the tuner's
//! spectrum flag is off it does exactly one relaxed atomic load and returns —
//! no FFT, no allocation, no clock read. The FFT machinery ([`SpectrumDsp`]) is
//! built lazily on the first enabled capture, so a tuner that never turns
//! spectrum on never even allocates the plan or the buffers.
//!
//! # When on
//!
//! At most one capture every [`CAPTURE_INTERVAL`] (~3.3 Hz, never every block):
//! a Hann-windowed [`FFT_SIZE`]-point FFT over the head of the raw block,
//! magnitude → dBFS normalized so a full-scale tone reads ~0 dBFS, fftshifted
//! into ascending-frequency order, peak-hold decimated to [`OUTPUT_BINS`]
//! values (a narrow carrier is never averaged away), each clamped to
//! `DB_FLOOR..=DB_CEIL` and quantized to `i8` — the byte *is* the dBFS. A few
//! hundred k ops a few times a second, and only while enabled.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use num_complex::Complex;
use rustfft::{Fft, FftPlanner};
use serde_json::{json, Value};

use trunkd_core::chan::BoundedSender;

/// FFT size over one raw wideband block (within the 4096-8192 the plot wants).
pub(crate) const FFT_SIZE: usize = 4096;

/// Bins delivered to keyup after peak-hold decimation. Never more than a plot
/// needs; 4096 → 512 is a group of 8 per output bin.
pub(crate) const OUTPUT_BINS: usize = 512;

/// dBFS clamp floor. The quantized `i8` value *is* the clamped dBFS.
pub(crate) const DB_FLOOR: i32 = -110;

/// dBFS clamp ceiling.
pub(crate) const DB_CEIL: i32 = -10;

/// Duty cycle: grab at most one block every ~300 ms (~3.3 Hz), never per block.
pub(crate) const CAPTURE_INTERVAL: Duration = Duration::from_millis(300);

// ---------------------------------------------------------------------------
// Pure math (table-testable)
// ---------------------------------------------------------------------------

/// Clamp a dBFS value to the fixed window and quantize to `i8` (1 dB steps).
///
/// The clamp range fits inside `i8`, so the returned byte is directly the
/// clamped, rounded dBFS — no separate scale factor for the reader to undo.
pub(crate) fn quantize_db(db: f64) -> i8 {
    let rounded = if db.is_finite() {
        db.round() as i32
    } else {
        DB_FLOOR
    };
    rounded.clamp(DB_FLOOR, DB_CEIL) as i8
}

/// Peak-hold decimate `src` to exactly `target` values.
///
/// Each output is the maximum over its contiguous group of inputs, so a narrow
/// carrier that occupies one FFT bin still shows up after decimation rather
/// than being averaged into the noise. `target == 0` or an empty input yields
/// an empty vector; `target >= src.len()` returns one input per output (still
/// exactly `target` long when `target <= src.len()`).
pub(crate) fn decimate_peak(src: &[f64], target: usize) -> Vec<f64> {
    if target == 0 || src.is_empty() {
        return Vec::new();
    }
    let n = src.len();
    (0..target)
        .map(|i| {
            let lo = i * n / target;
            let hi = (((i + 1) * n / target).max(lo + 1)).min(n);
            src[lo..hi]
                .iter()
                .copied()
                .fold(f64::NEG_INFINITY, f64::max)
        })
        .collect()
}

/// A Hann window of length `n`.
fn hann(n: usize) -> Vec<f32> {
    if n <= 1 {
        return vec![1.0; n];
    }
    (0..n)
        .map(|i| {
            let w = 0.5 - 0.5 * (std::f64::consts::TAU * i as f64 / (n - 1) as f64).cos();
            w as f32
        })
        .collect()
}

// ---------------------------------------------------------------------------
// The FFT engine (lazily built)
// ---------------------------------------------------------------------------

/// The heavy, reusable spectrum state: the FFT plan, the window, and the
/// preallocated buffers. Built once, on the first enabled capture, and reused
/// for every capture after — nothing here allocates in the steady state.
pub(crate) struct SpectrumDsp {
    fft: Arc<dyn Fft<f32>>,
    window: Vec<f32>,
    /// Coherent gain of the window: dividing the bin magnitude by this makes a
    /// full-scale complex sinusoid read ~0 dBFS.
    norm: f64,
    buf: Vec<Complex<f32>>,
    scratch: Vec<Complex<f32>>,
    shifted: Vec<f64>,
}

impl SpectrumDsp {
    /// Build the FFT plan, Hann window and buffers for one [`FFT_SIZE`] block.
    pub(crate) fn new() -> Self {
        let mut planner = FftPlanner::<f32>::new();
        let fft = planner.plan_fft_forward(FFT_SIZE);
        let scratch = vec![Complex::new(0.0, 0.0); fft.get_inplace_scratch_len()];
        let window = hann(FFT_SIZE);
        let norm = window.iter().map(|&w| f64::from(w)).sum::<f64>().max(1e-12);
        SpectrumDsp {
            fft,
            window,
            norm,
            buf: vec![Complex::new(0.0, 0.0); FFT_SIZE],
            scratch,
            shifted: vec![0.0; FFT_SIZE],
        }
    }

    /// Compute the decimated, quantized dBFS spectrum for one raw block.
    ///
    /// Uses the first [`FFT_SIZE`] samples; returns [`OUTPUT_BINS`] `i8` values
    /// in ascending-frequency order (the capture centre in the middle).
    /// `None` when the block is shorter than one FFT (a ring-wrap tail): the
    /// caller simply waits for the next block.
    pub(crate) fn compute(&mut self, block: &[Complex<f32>]) -> Option<Vec<i8>> {
        if block.len() < FFT_SIZE {
            return None;
        }
        for (dst, (s, w)) in self
            .buf
            .iter_mut()
            .zip(block.iter().zip(self.window.iter()))
        {
            *dst = *s * *w;
        }
        self.fft
            .process_with_scratch(&mut self.buf, &mut self.scratch);

        // Magnitude → dBFS, fftshifted so negative frequencies come first and
        // the array reads from (centre − span/2) up to (centre + span/2).
        let half = FFT_SIZE / 2;
        for k in 0..FFT_SIZE {
            let src = if k < half { k + half } else { k - half };
            let mag = f64::from(self.buf[src].norm()) / self.norm;
            self.shifted[k] = 20.0 * mag.max(1e-12).log10();
        }
        let decimated = decimate_peak(&self.shifted, OUTPUT_BINS);
        Some(decimated.into_iter().map(quantize_db).collect())
    }
}

impl Default for SpectrumDsp {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// The frame and its wire form
// ---------------------------------------------------------------------------

/// One spectrum frame, handed from the DSP thread to the control thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SpectrumFrame {
    /// The tuner (device id) this frame is for.
    pub tuner_key: String,
    /// Wall-clock capture time in unix milliseconds.
    pub ts_ms: i64,
    /// Capture centre frequency in Hz.
    pub center_hz: u64,
    /// Captured span in Hz (the whole FFT width = the sample rate).
    pub span_hz: u64,
    /// Quantized dBFS bins, ascending frequency. `db.len()` is the bin count.
    pub db: Vec<i8>,
}

/// Render the spectrum POST body, matching the keyup frame contract exactly:
/// `{instance_id, tuner_key, ts_ms, center_hz, span_hz, bins, db}` with
/// `bins == db.len()`.
pub(crate) fn spectrum_body(instance_id: &str, f: &SpectrumFrame) -> Value {
    json!({
        "instance_id": instance_id,
        "tuner_key": f.tuner_key,
        "ts_ms": f.ts_ms,
        "center_hz": f.center_hz,
        "span_hz": f.span_hz,
        "bins": f.db.len(),
        "db": f.db,
    })
}

// ---------------------------------------------------------------------------
// The DSP-thread tap
// ---------------------------------------------------------------------------

/// What the recorder wiring hands a `DeviceUnit` so it *can* emit spectrum: the
/// live on/off flag (set by the control loop when keyup's desired `spectrum`
/// flips) and the drop-oldest frame sink into the control thread.
#[derive(Clone)]
pub(crate) struct SpectrumHook {
    /// Set/cleared by the control loop; read once per block by the DSP thread.
    pub enabled: Arc<AtomicBool>,
    /// Bounded drop-oldest sink; a failed/slow POST costs frames, never the DSP.
    pub tx: Arc<BoundedSender<SpectrumFrame>>,
}

/// The DSP-thread spectrum tap. Cheap to hold and to poll; does real work only
/// while its flag is on, and then only on the duty cycle.
pub(crate) struct SpectrumTap {
    enabled: Arc<AtomicBool>,
    tx: Arc<BoundedSender<SpectrumFrame>>,
    tuner_key: String,
    center_hz: u64,
    span_hz: u64,
    last: Option<Instant>,
    dsp: Option<SpectrumDsp>,
    /// Count of frames actually computed. Stays 0 while disabled — the
    /// zero-cost-when-off proof the tests assert on.
    computed: trunkd_core::Metric,
}

impl SpectrumTap {
    /// Build a tap for one tuner. The FFT machinery is not built here.
    pub(crate) fn new(
        hook: SpectrumHook,
        tuner_key: String,
        center_hz: u64,
        span_hz: u64,
        computed: trunkd_core::Metric,
    ) -> Self {
        SpectrumTap {
            enabled: hook.enabled,
            tx: hook.tx,
            tuner_key,
            center_hz,
            span_hz,
            last: None,
            dsp: None,
            computed,
        }
    }

    /// Offer one raw wideband block to the tap.
    ///
    /// **Zero cost when off**: a single relaxed atomic load, then return. When
    /// on, at most one windowed FFT every [`CAPTURE_INTERVAL`]; the resulting
    /// frame is handed to the (drop-oldest) channel and never blocks this thread.
    #[inline]
    pub(crate) fn observe(&mut self, block: &[Complex<f32>]) {
        // The one-and-only cost on the off path.
        if !self.enabled.load(Ordering::Relaxed) {
            return;
        }
        let now = Instant::now();
        if self
            .last
            .is_some_and(|t| now.duration_since(t) < CAPTURE_INTERVAL)
        {
            return;
        }
        // Build (once) on first real use so a never-enabled tuner never allocates.
        let dsp = self.dsp.get_or_insert_with(SpectrumDsp::new);
        let Some(db) = dsp.compute(block) else {
            return; // ring-wrap tail shorter than one FFT; try the next block
        };
        self.last = Some(now);
        self.computed.inc();
        let frame = SpectrumFrame {
            tuner_key: self.tuner_key.clone(),
            ts_ms: trunkd_core::Timestamp::now().unix_millis(),
            center_hz: self.center_hz,
            span_hz: self.span_hz,
            db,
        };
        self.tx.send(frame);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use trunkd_core::chan;
    use trunkd_core::Metrics;

    // -- quantization: magnitude → dB → i8 ----------------------------------

    #[test]
    fn quantize_clamps_to_the_floor_and_ceiling() {
        assert_eq!(quantize_db(0.0), DB_CEIL as i8, "above ceiling clamps down");
        assert_eq!(quantize_db(-5.0), DB_CEIL as i8);
        assert_eq!(quantize_db(-200.0), DB_FLOOR as i8, "below floor clamps up");
        // Inside the window the byte is the rounded dBFS itself.
        assert_eq!(quantize_db(-42.4), -42);
        assert_eq!(quantize_db(-42.6), -43);
        assert_eq!(quantize_db(-10.0), -10);
        assert_eq!(quantize_db(-110.0), -110);
        // Non-finite (log10 of a zero-magnitude bin) pins to the floor.
        assert_eq!(quantize_db(f64::NEG_INFINITY), DB_FLOOR as i8);
        assert_eq!(quantize_db(f64::NAN), DB_FLOOR as i8);
    }

    // -- decimation: bin count → target width -------------------------------

    #[test]
    fn decimate_reduces_to_exactly_the_target_width() {
        let src: Vec<f64> = (0..4096).map(|i| i as f64).collect();
        let out = decimate_peak(&src, 512);
        assert_eq!(out.len(), 512, "output width is the target");
        // Peak-hold: each group's max is its last element (monotone input).
        assert_eq!(out[0], 7.0, "first group max of 0..8");
        assert_eq!(out[511], 4095.0, "last group max");
    }

    #[test]
    fn decimate_peak_keeps_a_narrow_spike() {
        // A single tall bin in an otherwise flat spectrum must survive
        // decimation (peak-hold, not averaging).
        let mut src = vec![-100.0f64; 4096];
        src[1000] = -12.0;
        let out = decimate_peak(&src, 512);
        assert!(
            out.iter().any(|&v| (v - -12.0).abs() < 1e-9),
            "the spike must be preserved by peak-hold"
        );
    }

    #[test]
    fn decimate_edge_cases_do_not_panic() {
        assert!(decimate_peak(&[], 512).is_empty());
        assert!(decimate_peak(&[1.0, 2.0], 0).is_empty());
        // target > len still yields exactly target values, none skipped.
        assert_eq!(decimate_peak(&[1.0, 2.0, 3.0], 6).len(), 6);
    }

    // -- the full FFT path: frame shape + a tone in the right half ----------

    fn tone(freq_hz: f64, rate: f64, n: usize) -> Vec<Complex<f32>> {
        (0..n)
            .map(|i| {
                let ph = std::f64::consts::TAU * freq_hz * i as f64 / rate;
                Complex::new(ph.cos() as f32, ph.sin() as f32)
            })
            .collect()
    }

    #[test]
    fn compute_returns_exactly_output_bins() {
        let mut dsp = SpectrumDsp::new();
        let block = tone(1_000_000.0, 20_000_000.0, FFT_SIZE);
        let db = dsp.compute(&block).expect("a full block computes");
        assert_eq!(db.len(), OUTPUT_BINS, "db length == bins");
    }

    #[test]
    fn compute_needs_a_full_fft_block() {
        let mut dsp = SpectrumDsp::new();
        assert!(dsp.compute(&tone(0.0, 20e6, FFT_SIZE - 1)).is_none());
    }

    #[test]
    fn a_full_scale_tone_reads_near_zero_dbfs_in_its_bin() {
        // Coherent-gain normalization: a unit complex sinusoid should peak at
        // ~0 dBFS (clamped ceiling is -10, so it saturates to the ceiling).
        let mut dsp = SpectrumDsp::new();
        let db = dsp
            .compute(&tone(5_000_000.0, 20_000_000.0, FFT_SIZE))
            .unwrap();
        let peak = *db.iter().max().unwrap();
        assert_eq!(
            peak, DB_CEIL as i8,
            "a full-scale tone saturates the ceiling"
        );
        // A positive-frequency tone lands in the upper half of the shifted array.
        let peak_idx = db.iter().position(|&v| v == peak).unwrap();
        assert!(
            peak_idx > OUTPUT_BINS / 2,
            "positive tone in the upper half"
        );
    }

    // -- the wire contract --------------------------------------------------

    #[test]
    fn spectrum_body_matches_the_contract_shape() {
        let frame = SpectrumFrame {
            tuner_key: "ctrl".into(),
            ts_ms: 1_700_000_000_123,
            center_hz: 855_500_000,
            span_hz: 20_000_000,
            db: vec![-110, -55, -10],
        };
        let body = spectrum_body("rec-01", &frame);
        assert_eq!(body["instance_id"], "rec-01");
        assert_eq!(body["tuner_key"], "ctrl");
        assert_eq!(body["ts_ms"], 1_700_000_000_123i64);
        assert_eq!(body["center_hz"], 855_500_000u64);
        assert_eq!(body["span_hz"], 20_000_000u64);
        assert_eq!(body["bins"], 3);
        assert_eq!(body["db"], json!([-110, -55, -10]));
        // The contract's invariant: bins == db length.
        assert_eq!(
            body["bins"].as_u64().unwrap() as usize,
            body["db"].as_array().unwrap().len()
        );
    }

    // -- the duty-cycle gate: no work when disabled -------------------------

    fn tap_with(enabled: bool) -> (SpectrumTap, Metrics, chan::BoundedReceiver<SpectrumFrame>) {
        let metrics = Metrics::new();
        let (tx, rx) = chan::bounded::<SpectrumFrame>(8);
        let hook = SpectrumHook {
            enabled: Arc::new(AtomicBool::new(enabled)),
            tx: Arc::new(tx),
        };
        let counter = metrics.counter("device.d0.spectrum_frames");
        let tap = SpectrumTap::new(hook, "d0".into(), 855_500_000, 20_000_000, counter);
        (tap, metrics, rx)
    }

    #[test]
    fn disabled_tap_does_no_work() {
        let (mut tap, metrics, rx) = tap_with(false);
        let block = tone(1_000_000.0, 20_000_000.0, FFT_SIZE);
        for _ in 0..100 {
            tap.observe(&block);
        }
        assert_eq!(
            metrics.counter("device.d0.spectrum_frames").get(),
            0,
            "a disabled tuner computes no frames"
        );
        assert!(rx.try_recv().is_err(), "and emits nothing");
        // It never even built the FFT machinery.
        assert!(tap.dsp.is_none(), "no allocation while disabled");
    }

    #[test]
    fn enabled_tap_emits_one_frame_then_the_duty_cycle_gates() {
        let (mut tap, metrics, rx) = tap_with(true);
        let block = tone(1_000_000.0, 20_000_000.0, FFT_SIZE);
        // First block computes a frame; the immediate next is gated by the duty
        // cycle (well under CAPTURE_INTERVAL), so the count stays at one.
        for _ in 0..50 {
            tap.observe(&block);
        }
        assert_eq!(
            metrics.counter("device.d0.spectrum_frames").get(),
            1,
            "one frame per duty-cycle window, not one per block"
        );
        let frame = rx.try_recv().expect("a frame was emitted");
        assert_eq!(frame.tuner_key, "d0");
        assert_eq!(frame.center_hz, 855_500_000);
        assert_eq!(frame.span_hz, 20_000_000);
        assert_eq!(frame.db.len(), OUTPUT_BINS);
    }

    #[test]
    fn flipping_the_flag_off_stops_the_work() {
        let (mut tap, metrics, _rx) = tap_with(true);
        let block = tone(1_000_000.0, 20_000_000.0, FFT_SIZE);
        tap.observe(&block);
        assert_eq!(metrics.counter("device.d0.spectrum_frames").get(), 1);
        tap.enabled.store(false, Ordering::Relaxed);
        for _ in 0..100 {
            tap.observe(&block);
        }
        assert_eq!(
            metrics.counter("device.d0.spectrum_frames").get(),
            1,
            "no further work once the flag is cleared"
        );
    }
}
