// SPDX-License-Identifier: GPL-3.0-or-later
//! Fred-Harris **M2** (2× oversampled) polyphase channelizer.
//!
//! One pass over the wideband capture splits it into `M = sample_rate / 12_500`
//! equally spaced 12.5 kHz bins; each bin is delivered at **twice** its own
//! bandwidth (25 kHz), which is exactly the rate [`crate::P25P1Channel`] wants.
//! The expensive part (mix + filter + FFT) is paid **once for the whole band**
//! and is `O(1)` in the number of subscribed channels, unlike the per-channel
//! [`Ddc`](super::Ddc) whose cost is `O(N)`.
//!
//! Ported from sdrtrunk's `ComplexPolyphaseChannelizerM2` (Harris,
//! *Multirate Signal Processing for Communications Systems*, p230-233):
//! a flat interleaved I/Q filter/sample layout for autovectorization, a
//! serpentine sample buffer advanced by `M/2` samples per block, two static
//! permutation tables selected on alternating blocks, and one `M`-point
//! inverse FFT per block.
//!
//! # Bin convention (differs from sdrtrunk deliberately)
//!
//! sdrtrunk's `ChannelCalculator` labels bins with a *boundary* at DC, which
//! puts every channel on the 12.5 kHz raster onto a bin **seam**. trunkd is not
//! bound to that: this implementation uses the natural `M`-point DFT
//! convention, so **bin `k` is centred at `k · sample_rate / M`** relative to
//! the capture centre (`k >= M/2` meaning `(k - M) · sample_rate / M`, i.e.
//! negative offsets). Because a typical 800 MHz channel plan and the test capture's 855.5 MHz
//! centre are both on the 12.5 kHz raster, every channel lands on a **bin
//! centre** and takes the cheap single-bin path. Two-bin synthesis is still
//! implemented for genuinely off-grid channels. See the
//! `bin_centering_and_isolation` test for the empirical proof.
//!
//! # Derivation of the output phase (why the block alternation works)
//!
//! With `y_p[m]` the `p`-th polyphase arm output at block `m`, the permutation
//! tables reduce to "arm index in natural order" (top block) and "the same,
//! circularly rotated by `M/2`" (middle block). Rotating the FFT *input* by
//! `M/2` multiplies output bin `k` by `(-1)^k`. Working the algebra through
//! with `t_m = (m+1)·M/2 - 1` (index of the newest buffered sample):
//!
//! ```text
//! Z_k[m] = (-1)^k · e^(-j2πk/M) · b_k(t_m)     for every m
//! ```
//!
//! where `b_k` is the ideal "mix bin k to DC, then low-pass with the
//! prototype" signal. The block-to-block phase term cancels exactly — that is
//! the phase-continuity guarantee — leaving only a constant per-bin phase
//! `α_k = (-1)^k · e^(-j2πk/M)`, which is harmless for a PLL/differential
//! demodulator but *is* accounted for when combining two bins.
//!
//! # Two-bin synthesis
//!
//! For a channel centred on the seam between bins `k` and `k+1`, shifting each
//! bin's stream so the seam sits at DC and summing gives (constants dropped):
//!
//! ```text
//! composite[m] = Z_k[m] · (-j)^(m+1)  -  Z_(k+1)[m] · (j)^(m+1)
//! ```
//!
//! Expanding the four phases of `(m+1) mod 4` shows this is exactly sdrtrunk's
//! `TwoChannelSynthesizerM2` chain — a 2-point IFFT (`Z_k ± Z_(k+1)`) whose two
//! outputs alternate block to block, followed by an `Fs/4` down-converter —
//! with the synthesis low-pass omitted: both inputs are already band-limited by
//! the analysis prototype, and the sum of the two `-6.02 dB`-at-the-seam skirts
//! is flat by construction, so no extra interpolation filter is needed.

use std::f64::consts::{PI, TAU};
use std::sync::Arc;

use num_complex::Complex;
use rustfft::{Fft, FftPlanner};

/// Bin spacing: the P25 channel grid. `M = sample_rate / BIN_BANDWIDTH_HZ`.
pub const BIN_BANDWIDTH_HZ: f64 = 12_500.0;

/// Half-width of the usable span for a capture rate, without building the
/// filterbank — the same value [`PolyphaseChannelizerM2::usable_half_span_hz`]
/// reports, so routing decisions agree bit-for-bit with what a bin subscription
/// would accept. Returns `None` when the rate cannot drive the channelizer at
/// all (not a multiple of 12.5 kHz, an odd bin count, or fewer than four bins).
pub fn usable_half_span_for(sample_rate: u32) -> Option<f64> {
    if !f64::from(sample_rate).rem_euclid(BIN_BANDWIDTH_HZ).eq(&0.0) {
        return None;
    }
    let bins = (f64::from(sample_rate) / BIN_BANDWIDTH_HZ) as u32;
    if !bins.is_multiple_of(2) || bins < 4 {
        return None;
    }
    let bin_spacing = f64::from(sample_rate) / f64::from(bins);
    Some(f64::from(sample_rate) / 2.0 - bin_spacing)
}

/// Default prototype length in taps per polyphase arm.
///
/// The FIR cost is `4 · taps_per_channel` real multiply-accumulates per complex
/// input sample regardless of `M`, so this is the one knob that trades CPU for
/// passband flatness near the bin edge.
pub const DEFAULT_TAPS_PER_CHANNEL: usize = 12;

/// Kaiser stopband parameter used for the prototype window (sdrtrunk uses the
/// same 80 dB figure in `FilterFactory.getSincM2Channelizer`).
pub const DEFAULT_STOPBAND_DB: f64 = 80.0;

/// Blocks between per-bin power accumulations (see
/// [`PolyphaseChannelizerM2::bin_power_percentile_dbfs`]).
///
/// The filterbank produces `2 · BIN_BANDWIDTH_HZ` = 25 000 blocks per second
/// whatever the capture rate, so an unstrided accumulator would be `25_000 · M`
/// adds per second — 40 M/s at `M = 1600`. Striding by eight puts it at 5 M/s
/// against the FIR's `4 · taps_per_channel` multiply-accumulates *per input
/// sample* (960 M/s at 20 MS/s and the default 12 taps). Measured on a
/// second's worth of 20 MS/s capture: 1.7 ms of accumulation against 169 ms of
/// channelizing, i.e. 1% of a pass that is already being paid and 0.17% of the
/// DSP thread's real-time budget. A 5 s stats window still averages
/// 15 625 blocks per bin, which is far more than a percentile needs.
const BIN_POWER_STRIDE: u64 = 8;

/// Perfect-reconstruction gain at the bin edge: `20·log10(0.5)`.
pub const PERFECT_RECONSTRUCTION_EDGE_DB: f64 = -6.020_599_913_279_624;

/// Everything that can go wrong configuring a channelizer or a subscription.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ChannelizerError {
    /// `sample_rate` is not an exact multiple of [`BIN_BANDWIDTH_HZ`].
    #[error("sample rate {0} is not an exact multiple of 12500 Hz")]
    RateNotOnGrid(u32),
    /// `M = sample_rate / 12500` came out odd; the M2 structure needs it even.
    #[error("sample rate {rate} yields an odd bin count {bins}; M must be even")]
    OddBinCount {
        /// The offending input rate.
        rate: u32,
        /// The bin count it produced.
        bins: u32,
    },
    /// `M` is too small to be a useful filterbank.
    #[error("sample rate {rate} yields only {bins} bins (minimum 4)")]
    TooFewBins {
        /// The offending input rate.
        rate: u32,
        /// The bin count it produced.
        bins: u32,
    },
    /// `taps_per_channel` must be at least 2.
    #[error("taps per channel {0} is too small (minimum 2)")]
    TooFewTaps(usize),
    /// The requested channel falls outside the usable part of the capture.
    #[error("frequency {freq_hz} Hz is outside the usable band (centre {center_hz} Hz, ±{usable_hz} Hz)")]
    FrequencyOutOfBand {
        /// Requested channel centre.
        freq_hz: f64,
        /// Capture centre.
        center_hz: f64,
        /// Half-width of the usable span.
        usable_hz: f64,
    },
    /// The requested channel lands on the DC bin, which carries the zero-IF
    /// LO/DC spike and is never usable.
    #[error("frequency {freq_hz} Hz lands on the DC bin (bin 0) and is unusable")]
    DcBin {
        /// Requested channel centre.
        freq_hz: f64,
    },
    /// The handle was stale (already unsubscribed) or from another channelizer.
    #[error("channel handle is no longer valid")]
    StaleHandle,
}

/// Opaque subscription handle. Internally a slot index plus a generation
/// counter so a stale handle can never alias a recycled slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ChannelHandle {
    slot: u32,
    generation: u32,
}

/// Which bin(s) a subscription reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinSelection {
    /// The channel sits (near enough) on one bin's centre.
    One(u32),
    /// The channel straddles the seam between two adjacent bins.
    Two(u32, u32),
}

/// A wideband splitter: feed blocks of capture-rate IQ, pull per-channel
/// streams at a fixed, lower output rate.
///
/// Implementations must not allocate in the steady state.
pub trait Channelizer {
    /// Output sample rate shared by every subscription, in Hz.
    fn output_rate(&self) -> u32;

    /// Feed one block of wideband complex samples. Any length is accepted;
    /// partial internal blocks are carried over.
    fn process_block(&mut self, wideband: &[Complex<f32>]);

    /// Subscribe to an absolute centre frequency (Hz).
    fn subscribe(&mut self, center_hz: f64) -> Result<ChannelHandle, ChannelizerError>;

    /// Move an existing subscription to a new centre frequency without
    /// touching the shared filterbank.
    fn retune(&mut self, handle: ChannelHandle, center_hz: f64) -> Result<(), ChannelizerError>;

    /// Drop a subscription. Returns `false` for a stale handle.
    fn unsubscribe(&mut self, handle: ChannelHandle) -> bool;

    /// Append this subscription's freshly produced samples to `out` and clear
    /// the internal buffer; returns the number appended.
    fn drain(&mut self, handle: ChannelHandle, out: &mut Vec<Complex<f32>>) -> usize;
}

// ---------------------------------------------------------------------------
// Prototype filter design
// ---------------------------------------------------------------------------

/// Zeroth-order modified Bessel function of the first kind (series).
fn bessel_i0(x: f64) -> f64 {
    let mut sum = 1.0;
    let mut term = 1.0;
    let half = x / 2.0;
    for k in 1..60 {
        term *= (half / k as f64) * (half / k as f64);
        sum += term;
        if term < 1e-15 * sum {
            break;
        }
    }
    sum
}

/// Kaiser `beta` for a target stopband attenuation.
fn kaiser_beta(atten_db: f64) -> f64 {
    if atten_db > 50.0 {
        0.1102 * (atten_db - 8.7)
    } else if atten_db >= 21.0 {
        0.5842 * (atten_db - 21.0).powf(0.4) + 0.078_86 * (atten_db - 21.0)
    } else {
        0.0
    }
}

/// Magnitude response in dB at `cycles_per_sample` of a real FIR.
pub fn response_db(taps: &[f64], cycles_per_sample: f64) -> f64 {
    let w = TAU * cycles_per_sample;
    let mut re = 0.0f64;
    let mut im = 0.0f64;
    for (n, &h) in taps.iter().enumerate() {
        let p = w * n as f64;
        re += h * p.cos();
        im -= h * p.sin();
    }
    20.0 * re.hypot(im).log10()
}

/// Windowed-sinc low pass, odd length, `cutoff` in cycles/sample, using a
/// pre-computed Kaiser window.
fn sinc_with_window(window: &[f64], cutoff: f64) -> Vec<f64> {
    let len = window.len();
    let half = (len / 2) as f64;
    window
        .iter()
        .enumerate()
        .map(|(i, &w)| {
            let k = i as f64 - half;
            let sinc = if k == 0.0 {
                2.0 * cutoff
            } else {
                (TAU * cutoff * k).sin() / (PI * k)
            };
            sinc * w
        })
        .collect()
}

/// Design the M2 channelizer prototype for `bins` bins and `taps_per_channel`
/// taps per polyphase arm.
///
/// The length is fixed at `bins · taps_per_channel - 1` (odd) and the cutoff is
/// bisected until the response at the bin edge (`1/(2·bins)` cycles/sample,
/// i.e. half a bin) is [`PERFECT_RECONSTRUCTION_EDGE_DB`] relative to DC — the
/// perfect-reconstruction condition two adjacent bins need in order to sum to
/// a flat passband across their shared seam. A single zero tap is then
/// pre-padded so the length is exactly `bins · taps_per_channel`, as the
/// polyphase decomposition requires.
///
/// Returned taps are normalized to unity DC gain, which (with an unnormalized
/// inverse FFT of size `bins`) gives every bin unity gain for a tone at its
/// centre — no `channelCount` output scaling is needed, unlike sdrtrunk, whose
/// JTransforms IFFT divides by `M`.
pub fn design_m2_prototype(bins: usize, taps_per_channel: usize, atten_db: f64) -> Vec<f32> {
    let len = bins * taps_per_channel - 1;
    let beta = kaiser_beta(atten_db);
    let i0b = bessel_i0(beta);
    let half = (len / 2) as f64;
    let window: Vec<f64> = (0..len)
        .map(|i| {
            let r = (i as f64 - half) / half;
            bessel_i0(beta * (1.0 - r * r).max(0.0).sqrt()) / i0b
        })
        .collect();

    let band_edge = 1.0 / (2.0 * bins as f64);
    // Response at the bin edge rises monotonically with cutoff; bisect.
    let mut lo = band_edge * 0.25;
    let mut hi = band_edge * 2.0;
    let mut taps = sinc_with_window(&window, band_edge);
    for _ in 0..64 {
        let mid = 0.5 * (lo + hi);
        taps = sinc_with_window(&window, mid);
        let db = response_db(&taps, band_edge) - response_db(&taps, 0.0);
        if (db - PERFECT_RECONSTRUCTION_EDGE_DB).abs() < 1e-5 {
            break;
        }
        if db < PERFECT_RECONSTRUCTION_EDGE_DB {
            lo = mid;
        } else {
            hi = mid;
        }
    }

    let sum: f64 = taps.iter().sum();
    let mut out = Vec::with_capacity(bins * taps_per_channel);
    out.push(0.0f32); // pre-pad so the length is an exact multiple of `bins`
    out.extend(taps.iter().map(|&t| (t / sum) as f32));
    out
}

// ---------------------------------------------------------------------------
// Subscriptions
// ---------------------------------------------------------------------------

struct Subscription {
    bins: BinSelection,
    /// Residual offset from the bin centre / seam, in Hz (usually 0).
    residual_hz: f64,
    /// Running residual-offset phasor and its per-output-sample step.
    nco: Complex<f64>,
    nco_step: Complex<f64>,
    since_renorm: u32,
    center_hz: f64,
    out: Vec<Complex<f32>>,
}

impl Subscription {
    #[inline]
    fn emit(&mut self, spectrum: &[Complex<f32>], rot: Complex<f32>) {
        let raw = match self.bins {
            BinSelection::One(b) => spectrum[b as usize],
            // composite[m] = Z_lo·(-j)^(m+1) − Z_hi·(j)^(m+1); `rot` is
            // (j)^((m+1) mod 4) and its conjugate is (-j)^(m+1).
            BinSelection::Two(lo, hi) => {
                spectrum[lo as usize] * rot.conj() - spectrum[hi as usize] * rot
            }
        };
        let v = if self.residual_hz == 0.0 {
            raw
        } else {
            let osc = Complex::new(self.nco.re as f32, self.nco.im as f32);
            self.nco *= self.nco_step;
            self.since_renorm += 1;
            if self.since_renorm >= 2048 {
                self.nco /= self.nco.norm();
                self.since_renorm = 0;
            }
            raw * osc
        };
        self.out.push(v);
    }
}

struct Slot {
    generation: u32,
    sub: Option<Subscription>,
}

/// One noise-floor measurement window: summed power per bin since its last
/// reset. A channelizer carries one by default; a [`ChannelBank`] shared by
/// several site engines opens one per engine, because each engine rolls its
/// window on its own stats cadence and after its own retunes.
///
/// [`ChannelBank`]: super::bank::ChannelBank
struct PowerWindow {
    /// Summed power per bin, in the same linear units a subscription's own
    /// samples carry. Length `M`.
    sums: Vec<f64>,
    /// Blocks folded into `sums` since the last reset (one every
    /// [`BIN_POWER_STRIDE`]), so the sums can be turned into means.
    blocks: u64,
    /// A closed window is skipped by the accumulation (and can be reopened).
    open: bool,
}

thread_local! {
    /// Filterbanks built on this thread (see [`channelizers_built_on_this_thread`]).
    static BUILT: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// How many [`PolyphaseChannelizerM2`]s have been built on the calling thread.
///
/// Instrumentation: a device's engines are all built on its DSP thread, so the
/// difference across building them is how many filterbanks that device will
/// run — one when its sites share a [`ChannelBank`](super::bank::ChannelBank).
/// Per thread rather than global so parallel tests cannot see each other.
pub fn channelizers_built_on_this_thread() -> u64 {
    BUILT.with(std::cell::Cell::get)
}

// ---------------------------------------------------------------------------
// The channelizer
// ---------------------------------------------------------------------------

/// Fred-Harris M2 polyphase channelizer: `M = sample_rate / 12_500` bins, each
/// delivered at `2 × 12.5 kHz = 25 kHz`.
pub struct PolyphaseChannelizerM2 {
    sample_rate: u32,
    center_hz: f64,
    /// `M`.
    bins: usize,
    taps_per_channel: usize,
    /// Interleaved I/Q prototype taps (each real tap duplicated), block-mirrored
    /// so the run-time loop never reverses an index. Length `2·M·T`.
    filter: Vec<f32>,
    /// Serpentine sample history as interleaved floats, double-length so the
    /// write pointer can walk backwards and only memmove once every `2·T`
    /// blocks. Length `2 · 2·M·T`.
    history: Vec<f32>,
    /// Complex-sample offset of the logical history start inside `history`.
    pos: usize,
    /// Complex samples buffered toward the current (`M/2`-sample) block.
    pending: usize,
    /// Per-sub-channel accumulator, interleaved I/Q. Length `2·M`.
    acc: Vec<f32>,
    top_block_map: Vec<u32>,
    middle_block_map: Vec<u32>,
    top_block: bool,
    /// Completed block counter, drives the two-bin synthesis rotation.
    blocks: u64,
    fft: Arc<dyn Fft<f32>>,
    fft_buffer: Vec<Complex<f32>>,
    fft_scratch: Vec<Complex<f32>>,
    slots: Vec<Slot>,
    /// Noise-floor windows. Window 0 always exists and is the one the
    /// single-window API (`bin_power_*`, [`reset_bin_power`](Self::reset_bin_power))
    /// reads; more are opened with [`open_power_window`](Self::open_power_window).
    power_windows: Vec<PowerWindow>,
    /// Scratch for the percentile selection, preallocated to `M` so reading the
    /// floor never allocates. Length is only ever `0` or `M`.
    bin_power_scratch: Vec<f64>,
}

impl PolyphaseChannelizerM2 {
    /// Build a channelizer for a capture of `sample_rate` centred at
    /// `center_hz`, using [`DEFAULT_TAPS_PER_CHANNEL`].
    pub fn new(sample_rate: u32, center_hz: f64) -> Result<Self, ChannelizerError> {
        Self::with_taps(
            sample_rate,
            center_hz,
            DEFAULT_TAPS_PER_CHANNEL,
            DEFAULT_STOPBAND_DB,
        )
    }

    /// Build a channelizer with an explicit prototype length and Kaiser
    /// stopband parameter.
    ///
    /// `sample_rate` must be an exact multiple of 12.5 kHz and yield an even
    /// bin count (20 MHz → 1600, 12.5 MHz → 1000, 10 MHz → 800 all qualify).
    pub fn with_taps(
        sample_rate: u32,
        center_hz: f64,
        taps_per_channel: usize,
        atten_db: f64,
    ) -> Result<Self, ChannelizerError> {
        if !f64::from(sample_rate).rem_euclid(BIN_BANDWIDTH_HZ).eq(&0.0) {
            return Err(ChannelizerError::RateNotOnGrid(sample_rate));
        }
        let bins = (f64::from(sample_rate) / BIN_BANDWIDTH_HZ) as u32;
        if !bins.is_multiple_of(2) {
            return Err(ChannelizerError::OddBinCount {
                rate: sample_rate,
                bins,
            });
        }
        if bins < 4 {
            return Err(ChannelizerError::TooFewBins {
                rate: sample_rate,
                bins,
            });
        }
        if taps_per_channel < 2 {
            return Err(ChannelizerError::TooFewTaps(taps_per_channel));
        }
        let m = bins as usize;
        let t = taps_per_channel;
        let half = m / 2;

        let proto = design_m2_prototype(m, t, atten_db);
        debug_assert_eq!(proto.len(), m * t);

        // `getAlignedFilter`: duplicate each tap for the I and Q lane, then
        // mirror each `M/2`-complex-sample block — the polyphase commutator's
        // serpentine order, baked into the tap layout.
        let mut filter = vec![0.0f32; 2 * m * t];
        for (blk, chunk) in proto.chunks(half).enumerate() {
            for (i, &v) in chunk.iter().enumerate() {
                let j = blk * half + (half - 1 - i);
                filter[2 * j] = v;
                filter[2 * j + 1] = v;
            }
        }

        // `getTopBlockMap` / `getMiddleBlockMap`, expressed on complex indices:
        // the top map reverses each half, the middle map reverses the whole
        // array (= the top map rotated by M/2, which multiplies bin k by
        // (-1)^k and is exactly the phase correction the M/2 advance needs).
        let top_block_map: Vec<u32> = (0..m)
            .map(|x| if x < half { half - 1 - x } else { 3 * half - 1 - x } as u32)
            .collect();
        let middle_block_map: Vec<u32> = (0..m).map(|x| (m - 1 - x) as u32).collect();

        let mut planner = FftPlanner::<f32>::new();
        let fft = planner.plan_fft_inverse(m);
        let scratch_len = fft.get_inplace_scratch_len();

        BUILT.with(|b| b.set(b.get() + 1));
        Ok(PolyphaseChannelizerM2 {
            sample_rate,
            center_hz,
            bins: m,
            taps_per_channel: t,
            filter,
            history: vec![0.0f32; 4 * m * t],
            pos: m * t,
            pending: 0,
            acc: vec![0.0f32; 2 * m],
            top_block_map,
            middle_block_map,
            top_block: true,
            blocks: 0,
            fft,
            fft_buffer: vec![Complex::new(0.0, 0.0); m],
            fft_scratch: vec![Complex::new(0.0, 0.0); scratch_len],
            slots: Vec::new(),
            power_windows: vec![PowerWindow {
                sums: vec![0.0; m],
                blocks: 0,
                open: true,
            }],
            bin_power_scratch: Vec::with_capacity(m),
        })
    }

    /// Filterbank blocks run so far (one per `M/2` input samples) — the unit
    /// the channelizer's cost is paid in.
    pub fn blocks_run(&self) -> u64 {
        self.blocks
    }

    /// Number of bins `M`.
    pub fn bin_count(&self) -> usize {
        self.bins
    }

    /// Taps per polyphase arm.
    pub fn taps_per_channel(&self) -> usize {
        self.taps_per_channel
    }

    /// Bin spacing in Hz (`sample_rate / M`).
    pub fn bin_spacing_hz(&self) -> f64 {
        f64::from(self.sample_rate) / self.bins as f64
    }

    /// Capture centre frequency in Hz.
    pub fn center_hz(&self) -> f64 {
        self.center_hz
    }

    /// Half-width of the usable span: one bin short of Nyquist on each side.
    pub fn usable_half_span_hz(&self) -> f64 {
        f64::from(self.sample_rate) / 2.0 - self.bin_spacing_hz()
    }

    /// Which bin(s) a channel centre maps to, plus the leftover offset the
    /// output-side NCO has to remove.
    ///
    /// A channel within ±1/8 bin (±1.5625 kHz) of a bin centre takes the
    /// single-bin path; anything further out straddles a seam and is
    /// reconstructed from the two neighbouring bins.
    pub fn bin_for(&self, center_hz: f64) -> Result<(BinSelection, f64), ChannelizerError> {
        let spacing = self.bin_spacing_hz();
        let offset = center_hz - self.center_hz;
        if offset.abs() > self.usable_half_span_hz() {
            return Err(ChannelizerError::FrequencyOutOfBand {
                freq_hz: center_hz,
                center_hz: self.center_hz,
                usable_hz: self.usable_half_span_hz(),
            });
        }
        let m = self.bins as i64;
        let exact = offset / spacing;
        let k = exact.round();
        let residual = (exact - k) * spacing;
        let wrap = |i: i64| -> u32 { i.rem_euclid(m) as u32 };
        if residual.abs() <= spacing / 8.0 {
            let bin = wrap(k as i64);
            if bin == 0 {
                return Err(ChannelizerError::DcBin { freq_hz: center_hz });
            }
            Ok((BinSelection::One(bin), residual))
        } else {
            // Straddles the seam between `lo` and `lo + 1`.
            let lo_f = (exact - 0.5).round();
            let residual = (exact - (lo_f + 0.5)) * spacing;
            let lo = wrap(lo_f as i64);
            let hi = wrap(lo_f as i64 + 1);
            if lo == 0 || hi == 0 {
                return Err(ChannelizerError::DcBin { freq_hz: center_hz });
            }
            Ok((BinSelection::Two(lo, hi), residual))
        }
    }

    fn make_subscription(&self, center_hz: f64) -> Result<Subscription, ChannelizerError> {
        let (bins, residual_hz) = self.bin_for(center_hz)?;
        let step = -TAU * residual_hz / f64::from(self.output_rate_u32());
        Ok(Subscription {
            bins,
            residual_hz,
            nco: Complex::new(1.0, 0.0),
            nco_step: Complex::new(step.cos(), step.sin()),
            since_renorm: 0,
            center_hz,
            out: Vec::with_capacity(256),
        })
    }

    fn output_rate_u32(&self) -> u32 {
        (2.0 * BIN_BANDWIDTH_HZ) as u32
    }

    fn slot_mut(&mut self, h: ChannelHandle) -> Option<&mut Subscription> {
        let s = self.slots.get_mut(h.slot as usize)?;
        if s.generation != h.generation {
            return None;
        }
        s.sub.as_mut()
    }

    /// Bin selection currently in force for a subscription (for tests/debug).
    pub fn selection(&self, h: ChannelHandle) -> Option<BinSelection> {
        let s = self.slots.get(h.slot as usize)?;
        if s.generation != h.generation {
            return None;
        }
        s.sub.as_ref().map(|s| s.bins)
    }

    /// Centre frequency of a subscription.
    pub fn subscribed_frequency(&self, h: ChannelHandle) -> Option<f64> {
        let s = self.slots.get(h.slot as usize)?;
        if s.generation != h.generation {
            return None;
        }
        s.sub.as_ref().map(|s| s.center_hz)
    }

    /// Number of live subscriptions.
    pub fn subscription_count(&self) -> usize {
        self.slots.iter().filter(|s| s.sub.is_some()).count()
    }

    /// The `percentile`-th quietest bin's mean power over the current
    /// measurement window, in dBFS — the capture's noise floor.
    ///
    /// `None` until at least one block has been folded in (a freshly reset or
    /// freshly built channelizer has nothing to report; a fabricated `-120` is
    /// not an answer).
    ///
    /// # Why this is directly comparable to a channel's own power
    ///
    /// A single-bin subscription emits `spectrum[b]` verbatim (times a
    /// unit-magnitude residual NCO), and the prototype is normalized to unity
    /// DC gain, so bin power *is* channel power in the same linear units —
    /// which is then put through the same
    /// [`power_to_dbfs`](super::carrier::power_to_dbfs) as
    /// [`CarrierEstimate::power_dbfs`](super::carrier::CarrierEstimate::power_dbfs).
    /// Subtracting the two therefore gives a real SNR in dB. A channel on the
    /// two-bin synthesis path is within a fraction of a dB of that: the
    /// perfect-reconstruction skirts sum to one flat 12.5 kHz passband, so the
    /// composite's noise bandwidth is a bin's.
    ///
    /// A percentile rather than a mean because the point is to measure what is
    /// *not* signal: the DC bin's LO spike, the control channel itself and
    /// every busy traffic channel all sit far above the 20th percentile of a
    /// few hundred bins and cannot drag it.
    ///
    /// `&mut self` because the selection runs in a preallocated scratch buffer;
    /// nothing observable changes, and the window keeps accumulating until
    /// [`reset_bin_power`](Self::reset_bin_power).
    pub fn bin_power_percentile_dbfs(&mut self, percentile: f64) -> Option<f64> {
        self.window_percentile_dbfs(0, percentile)
    }

    /// Blocks folded into the per-bin power window so far (tests, and anyone
    /// asking how much the floor is averaged over).
    pub fn bin_power_blocks(&self) -> u64 {
        self.window_blocks(0)
    }

    /// Start a fresh noise-floor window.
    ///
    /// Called once per stats window so each floor is an independent
    /// measurement, and after a retune, whose pre-retune samples describe a
    /// band the tuner is no longer looking at.
    pub fn reset_bin_power(&mut self) {
        self.reset_window(0);
    }

    /// Open another noise-floor window (or reopen a closed one) and return its
    /// index. Each open window costs one strided pass over the FFT output — the
    /// same pass window 0 already pays — and nothing else.
    pub fn open_power_window(&mut self) -> usize {
        let idx = match self.power_windows.iter().position(|w| !w.open) {
            Some(i) => i,
            None => {
                self.power_windows.push(PowerWindow {
                    sums: vec![0.0; self.bins],
                    blocks: 0,
                    open: false,
                });
                self.power_windows.len() - 1
            }
        };
        self.power_windows[idx].open = true;
        self.reset_window(idx);
        idx
    }

    /// Stop accumulating window `w` until it is reopened. A no-op for an index
    /// that does not exist.
    pub fn close_power_window(&mut self, w: usize) {
        if let Some(win) = self.power_windows.get_mut(w) {
            win.open = false;
            win.sums.fill(0.0);
            win.blocks = 0;
        }
    }

    /// [`bin_power_percentile_dbfs`](Self::bin_power_percentile_dbfs) for
    /// window `w`. `None` for a window with no blocks yet, or no such window.
    pub fn window_percentile_dbfs(&mut self, w: usize, percentile: f64) -> Option<f64> {
        let win = self.power_windows.get(w)?;
        let n = win.sums.len();
        if win.blocks == 0 || n == 0 {
            return None;
        }
        let blocks = win.blocks as f64;
        self.bin_power_scratch.clear();
        self.bin_power_scratch
            .extend(win.sums.iter().map(|p| p / blocks));
        let rank = (percentile.clamp(0.0, 100.0) / 100.0 * (n - 1) as f64).round() as usize;
        let rank = rank.min(n - 1);
        let (_, nth, _) = self
            .bin_power_scratch
            .select_nth_unstable_by(rank, f64::total_cmp);
        Some(super::carrier::power_to_dbfs(*nth))
    }

    /// Blocks folded into window `w` since its last reset (0 for no such
    /// window).
    pub fn window_blocks(&self, w: usize) -> u64 {
        self.power_windows.get(w).map_or(0, |win| win.blocks)
    }

    /// Start a fresh measurement in window `w`.
    pub fn reset_window(&mut self, w: usize) {
        if let Some(win) = self.power_windows.get_mut(w) {
            win.sums.fill(0.0);
            win.blocks = 0;
        }
    }

    /// The samples a subscription has buffered, without taking them — the
    /// read a [`ChannelBank`](super::bank::ChannelBank) fans out to every
    /// consumer of one channel. Pair with [`clear_outputs`](Self::clear_outputs).
    pub fn peek(&self, handle: ChannelHandle) -> Option<&[Complex<f32>]> {
        let s = self.slots.get(handle.slot as usize)?;
        if s.generation != handle.generation {
            return None;
        }
        s.sub.as_ref().map(|s| s.out.as_slice())
    }

    /// Empty every subscription's output buffer (keeping its capacity).
    pub fn clear_outputs(&mut self) {
        for sub in self.slots.iter_mut().filter_map(|s| s.sub.as_mut()) {
            sub.out.clear();
        }
    }

    /// Run one filterbank block: FIR over the whole history, permute into FFT
    /// order, one `M`-point inverse FFT, then hand each subscriber its sample.
    fn run_block(&mut self) {
        let m = self.bins;
        let n = 2 * m * self.taps_per_channel;

        // Fused multiply-accumulate over the flat interleaved arrays; every
        // one of the `M` polyphase sub-filters is evaluated by this one loop.
        self.acc.fill(0.0);
        let hist = &self.history[2 * self.pos..2 * self.pos + n];
        for tap in 0..self.taps_per_channel {
            let base = tap * 2 * m;
            let hs = &hist[base..base + 2 * m];
            let fs = &self.filter[base..base + 2 * m];
            for (a, (h, f)) in self.acc.iter_mut().zip(hs.iter().zip(fs.iter())) {
                *a += h * f;
            }
        }

        let map = if self.top_block {
            &self.top_block_map
        } else {
            &self.middle_block_map
        };
        for (x, &src) in map.iter().enumerate() {
            let i = 2 * src as usize;
            self.fft_buffer[x] = Complex::new(self.acc[i], self.acc[i + 1]);
        }
        self.top_block = !self.top_block;

        self.fft
            .process_with_scratch(&mut self.fft_buffer, &mut self.fft_scratch);

        self.blocks += 1;
        // Per-bin power for the noise floor. Strided (see `BIN_POWER_STRIDE`),
        // no allocation, no branch beyond the stride test, and it reads the FFT
        // output while it is still hot in cache.
        if self.blocks.is_multiple_of(BIN_POWER_STRIDE) {
            for win in self.power_windows.iter_mut().filter(|w| w.open) {
                for (acc, z) in win.sums.iter_mut().zip(self.fft_buffer.iter()) {
                    *acc += f64::from(z.norm_sqr());
                }
                win.blocks += 1;
            }
        }
        // (j)^((m+1) mod 4), with m the zero-based block index just produced.
        let rot = match (self.blocks & 3) as u8 {
            0 => Complex::new(1.0f32, 0.0),
            1 => Complex::new(0.0f32, 1.0),
            2 => Complex::new(-1.0f32, 0.0),
            _ => Complex::new(0.0f32, -1.0),
        };
        let spectrum = &self.fft_buffer;
        for slot in self.slots.iter_mut() {
            if let Some(sub) = slot.sub.as_mut() {
                sub.emit(spectrum, rot);
            }
        }
    }

    /// Start a new `M/2`-sample block: walk the write pointer backwards,
    /// memmoving the history to the top of the buffer only when it would run
    /// off the front (once every `2·T` blocks).
    fn begin_block(&mut self) {
        let m = self.bins;
        let t = self.taps_per_channel;
        let half = m / 2;
        if self.pos < half {
            self.history
                .copy_within(2 * self.pos..2 * (self.pos + m * t), 2 * m * t);
            self.pos = m * t;
        }
        self.pos -= half;
    }
}

impl Channelizer for PolyphaseChannelizerM2 {
    fn output_rate(&self) -> u32 {
        self.output_rate_u32()
    }

    fn process_block(&mut self, wideband: &[Complex<f32>]) {
        let half = self.bins / 2;
        let mut i = 0usize;
        while i < wideband.len() {
            if self.pending == 0 {
                self.begin_block();
            }
            let take = (half - self.pending).min(wideband.len() - i);
            let base = 2 * (self.pos + self.pending);
            for (j, s) in wideband[i..i + take].iter().enumerate() {
                self.history[base + 2 * j] = s.re;
                self.history[base + 2 * j + 1] = s.im;
            }
            self.pending += take;
            i += take;
            if self.pending == half {
                self.run_block();
                self.pending = 0;
            }
        }
    }

    fn subscribe(&mut self, center_hz: f64) -> Result<ChannelHandle, ChannelizerError> {
        let sub = self.make_subscription(center_hz)?;
        if let Some(idx) = self.slots.iter().position(|s| s.sub.is_none()) {
            self.slots[idx].sub = Some(sub);
            Ok(ChannelHandle {
                slot: idx as u32,
                generation: self.slots[idx].generation,
            })
        } else {
            self.slots.push(Slot {
                generation: 0,
                sub: Some(sub),
            });
            Ok(ChannelHandle {
                slot: (self.slots.len() - 1) as u32,
                generation: 0,
            })
        }
    }

    fn retune(&mut self, handle: ChannelHandle, center_hz: f64) -> Result<(), ChannelizerError> {
        let fresh = self.make_subscription(center_hz)?;
        let Some(sub) = self.slot_mut(handle) else {
            return Err(ChannelizerError::StaleHandle);
        };
        sub.bins = fresh.bins;
        sub.residual_hz = fresh.residual_hz;
        sub.nco = fresh.nco;
        sub.nco_step = fresh.nco_step;
        sub.since_renorm = 0;
        sub.center_hz = center_hz;
        sub.out.clear();
        Ok(())
    }

    fn unsubscribe(&mut self, handle: ChannelHandle) -> bool {
        let Some(slot) = self.slots.get_mut(handle.slot as usize) else {
            return false;
        };
        if slot.generation != handle.generation || slot.sub.is_none() {
            return false;
        }
        slot.sub = None;
        slot.generation = slot.generation.wrapping_add(1);
        true
    }

    fn drain(&mut self, handle: ChannelHandle, out: &mut Vec<Complex<f32>>) -> usize {
        let Some(sub) = self.slot_mut(handle) else {
            return 0;
        };
        let n = sub.out.len();
        out.extend_from_slice(&sub.out);
        sub.out.clear();
        n
    }
}

#[cfg(test)]
mod tests {
    use super::super::carrier::{CarrierEstimate, CarrierMeter};
    use super::*;

    const RATE: u32 = 20_000_000;
    const CENTER: f64 = 855_500_000.0;

    fn tone_block(
        out: &mut Vec<Complex<f32>>,
        freqs: &[(f64, f32)],
        rate: f64,
        start: u64,
        count: usize,
    ) {
        out.clear();
        for n in 0..count {
            let t = (start + n as u64) as f64;
            let mut v = Complex::new(0.0f32, 0.0f32);
            for &(f, a) in freqs {
                let ph = TAU * f * t / rate;
                v += Complex::new(a * ph.cos() as f32, a * ph.sin() as f32);
            }
            out.push(v);
        }
    }

    /// RMS magnitude of the settled tail of a stream.
    fn rms_tail(v: &[Complex<f32>]) -> f64 {
        let tail = &v[v.len() / 2..];
        (tail.iter().map(|c| f64::from(c.norm_sqr())).sum::<f64>() / tail.len() as f64).sqrt()
    }

    /// Feed `count` samples of `freqs` and return each subscription's stream.
    fn run(
        ch: &mut PolyphaseChannelizerM2,
        handles: &[ChannelHandle],
        freqs: &[(f64, f32)],
        count: usize,
    ) -> Vec<Vec<Complex<f32>>> {
        let mut outs = vec![Vec::new(); handles.len()];
        let mut buf = Vec::with_capacity(8192);
        let mut done = 0u64;
        while (done as usize) < count {
            let n = 8192.min(count - done as usize);
            tone_block(&mut buf, freqs, f64::from(RATE), done, n);
            ch.process_block(&buf);
            for (i, &h) in handles.iter().enumerate() {
                ch.drain(h, &mut outs[i]);
            }
            done += n as u64;
        }
        outs
    }

    // (a) Prototype filter response.
    #[test]
    fn prototype_response_meets_m2_objectives() {
        let m = 1600usize;
        let t = DEFAULT_TAPS_PER_CHANNEL;
        let taps: Vec<f64> = design_m2_prototype(m, t, DEFAULT_STOPBAND_DB)
            .iter()
            .map(|&v| f64::from(v))
            .collect();
        assert_eq!(taps.len(), m * t);

        let dc = response_db(&taps, 0.0);
        assert!(dc.abs() < 1e-3, "DC gain should be unity, got {dc} dB");

        // Bin edge (6.25 kHz at 20 Msps / 1600 bins) must be -6.02 dB so two
        // adjacent bins reconstruct a flat passband across their shared seam.
        let hz = |f: f64| f / f64::from(RATE);
        let edge = response_db(&taps, hz(6_250.0));
        assert!(
            (edge - PERFECT_RECONSTRUCTION_EDGE_DB).abs() < 0.02,
            "band edge {edge} dB, want {PERFECT_RECONSTRUCTION_EDGE_DB} dB"
        );

        // One full bin out (the adjacent bin's centre) must be deep in the
        // stopband: that is the bin-isolation / anti-alias guarantee.
        let adj = response_db(&taps, hz(12_500.0));
        assert!(adj < -60.0, "response at 12.5 kHz is {adj} dB, want <= -60");
        for f in [12_500.0, 18_750.0, 25_000.0, 50_000.0, 100_000.0] {
            let r = response_db(&taps, hz(f));
            assert!(r < -60.0, "response at {f} Hz is {r} dB, want <= -60");
        }

        // Passband: flat where P25 actually has energy. The -6.02 dB
        // constraint at 6.25 kHz makes true flatness all the way to the bin
        // edge impossible for any realizable filter, so the achievable
        // objective is "flat over the occupied bandwidth, rolling into -6 dB
        // at the edge". Record what this design achieves.
        let d2 = response_db(&taps, hz(2_000.0));
        let d3 = response_db(&taps, hz(3_000.0));
        let d4 = response_db(&taps, hz(4_000.0));
        assert!(d2 > -0.15, "2 kHz droop {d2} dB");
        assert!(d3 > -0.5, "3 kHz droop {d3} dB");
        assert!(d4 > -1.5, "4 kHz droop {d4} dB");
    }

    // (b) THE BIN-CENTERING PROOF: a tone at exactly k·12.5 kHz from the
    // capture centre must come out at DC of bin k (not at ±6.25 kHz), with
    // >=60 dB isolation in the neighbouring bins.
    #[test]
    fn bin_centering_and_isolation() {
        let mut ch = PolyphaseChannelizerM2::new(RATE, CENTER).unwrap();
        let spacing = ch.bin_spacing_hz();
        assert_eq!(ch.bin_count(), 1600);
        assert!((spacing - 12_500.0).abs() < 1e-9);
        assert_eq!(ch.output_rate(), 25_000);

        // Three on-grid channels (the test capture's control channel among them)
        // plus one deliberately off-grid, half-bin (seam) tone.
        let on_grid = [851_050_000.0f64, 854_437_500.0, 859_987_500.0];
        let off_grid = 857_106_250.0f64; // 855.5 MHz + 128.5 bins -> a seam

        let mut freqs: Vec<(f64, f32)> = on_grid
            .iter()
            .map(|&f| (f - CENTER, 1.0f32))
            .collect::<Vec<_>>();
        freqs.push((off_grid - CENTER, 1.0));

        let mut handles = Vec::new();
        let mut labels = Vec::new();
        for &f in &on_grid {
            let (sel, res) = ch.bin_for(f).unwrap();
            assert!(
                matches!(sel, BinSelection::One(_)),
                "{f} Hz must land on a bin CENTRE, got {sel:?}"
            );
            assert!(res.abs() < 1e-6, "{f} Hz residual {res} should be zero");
            let BinSelection::One(k) = sel else {
                unreachable!()
            };
            // Host bin plus both neighbours, so isolation is measurable.
            for b in [k - 1, k, k + 1] {
                let hz = CENTER + f64::from(b as i32 - if b > 800 { 1600 } else { 0 }) * spacing;
                handles.push(ch.subscribe(hz).unwrap());
                labels.push((f, b, b == k));
            }
        }
        let seam = ch.subscribe(off_grid).unwrap();
        assert!(matches!(
            ch.selection(seam).unwrap(),
            BinSelection::Two(_, _)
        ));
        handles.push(seam);

        let outs = run(&mut ch, &handles, &freqs, 400 * 800);

        for (i, &(f, b, hosting)) in labels.iter().enumerate() {
            let mag = rms_tail(&outs[i]);
            if hosting {
                assert!(
                    (mag - 1.0).abs() < 0.05,
                    "{f} Hz in host bin {b}: |y| = {mag}, want ~1.0"
                );
                // The tone must be at DC of its bin: consecutive settled
                // output samples share a phase (no residual +/-6.25 kHz).
                let tail = &outs[i][outs[i].len() * 3 / 4..];
                let mut worst = 0.0f64;
                for w in tail.windows(2) {
                    let d = f64::from((w[1] * w[0].conj()).arg()).abs();
                    worst = worst.max(d);
                }
                assert!(
                    worst < 0.02,
                    "{f} Hz in bin {b} is not at DC: max phase step {worst} rad"
                );
            } else {
                let db = 20.0 * mag.log10();
                assert!(
                    db < -60.0,
                    "{f} Hz leaks into neighbour bin {b} at {db} dB, want <= -60"
                );
            }
        }

        // The half-bin (seam) tone is reconstructed at DC and at unity gain by
        // the two-bin synthesis path.
        let seam_out = &outs[outs.len() - 1];
        let mag = rms_tail(seam_out);
        assert!(
            (mag - 1.0).abs() < 0.08,
            "seam tone reconstructed at |y| = {mag}, want ~1.0"
        );
        let tail = &seam_out[seam_out.len() * 3 / 4..];
        let mut worst = 0.0f64;
        for w in tail.windows(2) {
            worst = worst.max(f64::from((w[1] * w[0].conj()).arg()).abs());
        }
        assert!(worst < 0.05, "seam tone not at DC: max phase step {worst}");
    }

    // A tone offset by +6.25 kHz from a bin centre must appear at +6.25 kHz in
    // that bin's 25 kHz output (a quarter turn per sample), proving the output
    // is a true baseband translate of the bin, not a re-centred one.
    #[test]
    fn half_bin_offset_appears_at_quarter_rate() {
        let mut ch = PolyphaseChannelizerM2::new(RATE, CENTER).unwrap();
        let bin_hz = CENTER + 100.0 * 12_500.0;
        let tone = bin_hz + 6_250.0;
        let h = ch.subscribe(bin_hz).unwrap();
        let outs = run(&mut ch, &[h], &[(tone - CENTER, 1.0)], 400 * 800);
        let tail = &outs[0][outs[0].len() * 3 / 4..];
        let mut sum = Complex::new(0.0f64, 0.0);
        for w in tail.windows(2) {
            let d = w[1] * w[0].conj();
            sum += Complex::new(f64::from(d.re), f64::from(d.im));
        }
        let step = sum.arg();
        // +6250 Hz at a 25 kHz output rate = +pi/2 rad/sample.
        assert!(
            (step - std::f64::consts::FRAC_PI_2).abs() < 0.01,
            "phase step {step} rad/sample, want pi/2"
        );
    }

    // (c) Phase continuity across block boundaries: the top/middle block-map
    // alternation must leave no phase glitch at the 25 kHz block rate.
    #[test]
    fn phase_continuous_across_blocks() {
        let mut ch = PolyphaseChannelizerM2::new(RATE, CENTER).unwrap();
        let bin_hz = CENTER + 200.0 * 12_500.0;
        // 1500 Hz inside the bin: a clean constant-frequency output.
        let tone = bin_hz + 1_500.0;
        let h = ch.subscribe(bin_hz).unwrap();
        // Deliberately ragged input block sizes so internal block boundaries
        // land at every phase relative to the caller's buffers.
        let mut out = Vec::new();
        let mut buf = Vec::new();
        let mut n = 0u64;
        let sizes = [1usize, 7, 800, 801, 1600, 13, 4096, 799];
        for i in 0..600 {
            let c = sizes[i % sizes.len()];
            tone_block(&mut buf, &[(tone - CENTER, 1.0)], f64::from(RATE), n, c);
            ch.process_block(&buf);
            ch.drain(h, &mut out);
            n += c as u64;
        }
        let tail = &out[out.len() / 2..];
        let want = TAU * 1_500.0 / 25_000.0;
        let mut worst = 0.0f64;
        for w in tail.windows(2) {
            let d = f64::from((w[1] * w[0].conj()).arg());
            worst = worst.max((d - want).abs());
        }
        assert!(
            worst < 1e-3,
            "phase step deviates by {worst} rad at some block boundary"
        );
        // ... and the amplitude must not modulate at the block rate either.
        let mut lo = f64::INFINITY;
        let mut hi = 0.0f64;
        for c in tail {
            let m = f64::from(c.norm());
            lo = lo.min(m);
            hi = hi.max(m);
        }
        assert!(hi / lo < 1.001, "amplitude ripple {lo}..{hi}");
    }

    // (d) Subscribe / unsubscribe / retune at run time never touches the
    // shared filterbank.
    #[test]
    fn subscription_lifecycle_is_runtime_safe() {
        let mut ch = PolyphaseChannelizerM2::new(RATE, CENTER).unwrap();
        let filter_before = ch.filter.clone();
        let fft_before = Arc::as_ptr(&ch.fft) as *const () as usize;

        let a = ch.subscribe(CENTER + 12_500.0).unwrap();
        let b = ch.subscribe(CENTER - 25_000.0).unwrap();
        assert_eq!(ch.subscription_count(), 2);

        let mut buf = Vec::new();
        tone_block(&mut buf, &[(12_500.0, 1.0)], f64::from(RATE), 0, 8_000);
        ch.process_block(&buf);
        let mut out = Vec::new();
        assert_eq!(ch.drain(a, &mut out), 10);
        assert_eq!(ch.drain(a, &mut out), 0);

        assert!(ch.unsubscribe(b));
        assert!(!ch.unsubscribe(b), "double unsubscribe must fail");
        assert_eq!(ch.drain(b, &mut out), 0, "stale handle drains nothing");
        assert_eq!(ch.subscription_count(), 1);

        // The freed slot is recycled but the old handle stays invalid.
        let c = ch.subscribe(CENTER + 37_500.0).unwrap();
        assert_eq!(c.slot, b.slot);
        assert_ne!(c.generation, b.generation);
        assert_eq!(ch.selection(b), None);

        ch.retune(c, CENTER - 62_500.0).unwrap();
        assert_eq!(ch.subscribed_frequency(c), Some(CENTER - 62_500.0));
        assert_eq!(ch.selection(c), Some(BinSelection::One(1600 - 5)));
        assert!(ch.retune(b, CENTER + 12_500.0).is_err());

        assert_eq!(ch.filter, filter_before, "filterbank must not be rebuilt");
        assert_eq!(Arc::as_ptr(&ch.fft) as *const () as usize, fft_before);
    }

    // (e) Invalid rates and frequencies.
    #[test]
    fn rejects_invalid_rates_and_frequencies() {
        assert_eq!(
            PolyphaseChannelizerM2::new(2_048_000, CENTER)
                .err()
                .unwrap(),
            ChannelizerError::RateNotOnGrid(2_048_000)
        );
        assert_eq!(
            PolyphaseChannelizerM2::new(1_012_500, CENTER)
                .err()
                .unwrap(),
            ChannelizerError::OddBinCount {
                rate: 1_012_500,
                bins: 81
            }
        );
        assert_eq!(
            PolyphaseChannelizerM2::new(25_000, CENTER).err().unwrap(),
            ChannelizerError::TooFewBins {
                rate: 25_000,
                bins: 2
            }
        );
        assert!(matches!(
            PolyphaseChannelizerM2::with_taps(RATE, CENTER, 1, 80.0),
            Err(ChannelizerError::TooFewTaps(1))
        ));
        // The three supported capture rates all work and all give 25 kHz bins.
        for (rate, bins) in [
            (20_000_000u32, 1600usize),
            (12_500_000, 1000),
            (10_000_000, 800),
        ] {
            let ch = PolyphaseChannelizerM2::with_taps(rate, CENTER, 4, 80.0).unwrap();
            assert_eq!(ch.bin_count(), bins);
            assert_eq!(ch.output_rate(), 25_000);
        }

        let mut ch = PolyphaseChannelizerM2::with_taps(RATE, CENTER, 4, 80.0).unwrap();
        assert!(matches!(
            ch.subscribe(CENTER + 9_999_000.0),
            Err(ChannelizerError::FrequencyOutOfBand { .. })
        ));
        assert!(matches!(
            ch.subscribe(CENTER - 9_999_000.0),
            Err(ChannelizerError::FrequencyOutOfBand { .. })
        ));
        assert!(matches!(
            ch.subscribe(CENTER),
            Err(ChannelizerError::DcBin { .. })
        ));
        assert!(matches!(
            ch.subscribe(CENTER + 200.0),
            Err(ChannelizerError::DcBin { .. })
        ));
        assert!(ch.subscribe(CENTER + 12_500.0).is_ok());
    }

    // A small-M instance behaves the same way — guards the bin math against
    // hard-coded 1600s.
    #[test]
    fn small_bank_bin_math() {
        let ch = PolyphaseChannelizerM2::with_taps(1_000_000, 100_000_000.0, 6, 80.0).unwrap();
        assert_eq!(ch.bin_count(), 80);
        assert_eq!(
            ch.bin_for(100_000_000.0 + 12_500.0).unwrap().0,
            BinSelection::One(1)
        );
        assert_eq!(
            ch.bin_for(100_000_000.0 - 12_500.0).unwrap().0,
            BinSelection::One(79)
        );
        let (sel, res) = ch.bin_for(100_000_000.0 + 18_750.0).unwrap();
        assert_eq!(sel, BinSelection::Two(1, 2));
        assert!(res.abs() < 1e-9);
        let (sel, res) = ch.bin_for(100_000_000.0 + 25_400.0).unwrap();
        assert_eq!(sel, BinSelection::One(2));
        assert!((res - 400.0).abs() < 1e-9);
    }

    // An off-grid channel needing a residual shift lands at DC after the
    // output-side NCO.
    #[test]
    fn residual_offset_is_removed() {
        let mut ch = PolyphaseChannelizerM2::new(RATE, CENTER).unwrap();
        let tone = CENTER + 300.0 * 12_500.0 + 900.0;
        let h = ch.subscribe(tone).unwrap();
        assert_eq!(ch.selection(h), Some(BinSelection::One(300)));
        let outs = run(&mut ch, &[h], &[(tone - CENTER, 1.0)], 400 * 800);
        let tail = &outs[0][outs[0].len() * 3 / 4..];
        let mut sum = Complex::new(0.0f64, 0.0);
        for w in tail.windows(2) {
            let d = w[1] * w[0].conj();
            sum += Complex::new(f64::from(d.re), f64::from(d.im));
        }
        assert!(
            sum.arg().abs() < 1e-3,
            "residual not removed: {}",
            sum.arg()
        );
    }

    // -- the noise floor ------------------------------------------------------

    /// Deterministic white noise. xorshift32 rather than an LCG because its
    /// low-order structure is what a spectral measurement would trip over, and
    /// a DSP test that cannot be replayed byte-for-byte is not a test.
    struct Noise(u32);

    impl Noise {
        /// Uniform on `[-1, 1)` from the top 24 bits.
        fn next(&mut self) -> f32 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 17;
            self.0 ^= self.0 << 5;
            (self.0 >> 8) as f32 / 8_388_608.0 - 1.0
        }
    }

    /// A small filterbank (80 bins) fed white noise plus one full-strength tone
    /// dead centre in bin 10, drained through a subscription on the tone's bin
    /// and one on an empty bin.
    fn noisy_capture() -> (PolyphaseChannelizerM2, CarrierEstimate, CarrierEstimate) {
        const NOISY_RATE: u32 = 1_000_000; // 80 bins of 12.5 kHz
        const NOISY_CENTER: f64 = 852_000_000.0;
        const SIGNAL_HZ: f64 = 125_000.0; // bin 10
        const EMPTY_HZ: f64 = 375_000.0; // bin 30, nothing in it
        const SAMPLES: usize = 400_000; // 10 000 filterbank blocks

        let mut ch = PolyphaseChannelizerM2::new(NOISY_RATE, NOISY_CENTER).unwrap();
        let sig = ch.subscribe(NOISY_CENTER + SIGNAL_HZ).unwrap();
        let empty = ch.subscribe(NOISY_CENTER + EMPTY_HZ).unwrap();
        assert_eq!(ch.selection(sig), Some(BinSelection::One(10)));
        assert_eq!(ch.selection(empty), Some(BinSelection::One(30)));

        // `min_samples` of 1: the point is the power, and the default one
        // second of output would need 25× the input.
        let mut sig_meter = CarrierMeter::with_min_samples(ch.output_rate(), 1);
        let mut empty_meter = CarrierMeter::with_min_samples(ch.output_rate(), 1);
        let mut rng = Noise(0xfeed_1234);
        let mut block = Vec::with_capacity(4096);
        let mut drained = Vec::with_capacity(4096);
        let w = TAU * SIGNAL_HZ / f64::from(NOISY_RATE);
        let mut done = 0usize;
        while done < SAMPLES {
            let n = 4096.min(SAMPLES - done);
            block.clear();
            for k in 0..n {
                let a = w * (done + k) as f64;
                block.push(Complex::new(
                    TONE_AMPLITUDE * a.cos() as f32 + rng.next(),
                    TONE_AMPLITUDE * a.sin() as f32 + rng.next(),
                ));
            }
            ch.process_block(&block);
            drained.clear();
            ch.drain(sig, &mut drained);
            sig_meter.feed(&drained);
            drained.clear();
            ch.drain(empty, &mut drained);
            empty_meter.feed(&drained);
            done += n;
        }
        (
            ch,
            sig_meter.estimate().expect("min_samples is 1"),
            empty_meter.estimate().expect("min_samples is 1"),
        )
    }

    /// A tone at unity gain in a centred bin contributes exactly `A²`.
    const TONE_AMPLITUDE: f32 = 0.9;

    /// The measurement the tuner-status SNR is built out of: the 20th-percentile
    /// bin power is the capture's noise floor, and subtracting it from a
    /// channel's own [`CarrierEstimate::power_dbfs`] gives that channel's SNR.
    ///
    /// Truth comes from an *empty* bin of the same capture, measured through a
    /// subscription — a completely independent path to the same quantity.
    #[test]
    fn the_noise_floor_recovers_a_synthetic_tones_snr() {
        let (mut ch, signal, empty) = noisy_capture();
        let floor = ch
            .bin_power_percentile_dbfs(20.0)
            .expect("blocks were folded in");

        // The percentile really is an unoccupied bin's power: one is measured
        // across the whole filterbank, the other on one subscription's stream.
        assert!(
            (floor - empty.power_dbfs).abs() < 1.0,
            "floor {floor} dBFS vs an empty bin's {} dBFS",
            empty.power_dbfs
        );

        // The tone passes a centred bin at unity gain, so its own contribution
        // is `A²`; the noise sharing that bin is what the empty bin measured.
        let noise = 10f64.powf(empty.power_dbfs / 10.0);
        let truth = 10.0 * (f64::from(TONE_AMPLITUDE).powi(2) / noise).log10();
        let measured = signal.power_dbfs - floor;
        assert!(
            truth > 10.0,
            "the test signal has to stand out of the noise to test anything: {truth} dB"
        );
        assert!(
            (measured - truth).abs() < 1.5,
            "SNR read {measured} dB, truth {truth} dB (floor {floor}, signal {})",
            signal.power_dbfs
        );
    }

    /// The percentile is what keeps the occupied bins out of the floor: the
    /// top of the distribution is the tone, the bottom is noise, and they are
    /// twenty-odd dB apart.
    #[test]
    fn the_percentile_separates_the_signal_from_the_floor() {
        let (mut ch, signal, _) = noisy_capture();
        let floor = ch.bin_power_percentile_dbfs(20.0).unwrap();
        let loudest = ch.bin_power_percentile_dbfs(100.0).unwrap();
        assert!(
            (loudest - signal.power_dbfs).abs() < 1.0,
            "the loudest bin is the tone: {loudest} vs {}",
            signal.power_dbfs
        );
        assert!(loudest - floor > 10.0, "{loudest} dBFS over {floor} dBFS");
        // Every percentile below the one occupied bin in eighty is noise, so
        // the floor barely moves across them.
        let median = ch.bin_power_percentile_dbfs(50.0).unwrap();
        assert!((median - floor).abs() < 1.0, "{median} vs {floor}");
    }

    /// Nothing measured is reported as nothing, not as the dB floor, and a
    /// reset puts the window back to nothing.
    #[test]
    fn no_blocks_means_no_floor() {
        let mut ch = PolyphaseChannelizerM2::new(1_000_000, 852_000_000.0).unwrap();
        assert_eq!(ch.bin_power_percentile_dbfs(20.0), None);
        assert_eq!(ch.bin_power_blocks(), 0);
        // One accumulation every `BIN_POWER_STRIDE` blocks of `M/2` samples.
        ch.process_block(&vec![Complex::new(0.5, 0.0); 40 * 8 * 3]);
        assert_eq!(ch.bin_power_blocks(), 3);
        assert!(ch.bin_power_percentile_dbfs(20.0).is_some());
        ch.reset_bin_power();
        assert_eq!(ch.bin_power_blocks(), 0);
        assert_eq!(ch.bin_power_percentile_dbfs(20.0), None);
    }
}
