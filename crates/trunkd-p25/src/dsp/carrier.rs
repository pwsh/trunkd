// SPDX-License-Identifier: GPL-3.0-or-later
//! Carrier-offset meter: how far a channel's carrier sits from where the
//! tuner was told to put it.
//!
//! # Why this is not [`P25Event::FrequencyError`](crate::P25Event)
//!
//! The demodulator's PLL phase error is a *loop* reading, not a carrier
//! measurement: it is taken at NID validation, on a signal the loop has
//! already pulled in, and it moves for reasons that have nothing to do with
//! the LO. Measured in the field it read -1748 Hz on a board that decodes
//! perfectly and jumped +160 → +570 Hz when the tune moved by 171 Hz. It
//! cannot close a frequency loop; this can.
//!
//! # What it measures
//!
//! The magnitude-weighted mean instantaneous frequency of the channel:
//!
//! ```text
//! d[n] = x[n] · conj(x[n-1])      offset = mean(arg d, weighted by |d|) / 2π · rate
//! ```
//!
//! For a symmetric constellation — C4FM's ±600/±1800 Hz deviations, and LSM,
//! which is the same symbol set with amplitude — the symbol contribution to
//! instantaneous frequency averages to zero over enough random data, so what
//! is left is the carrier offset. One second is enough; the default
//! `min_samples` is exactly that.
//!
//! Weighting by `|d|` is what makes it usable on a channel that is not always
//! transmitting. An unweighted mean gives a noise-only sample the same weight as
//! a full-strength symbol, and noise-only phase differences are uniform, which
//! drags the estimate toward zero — i.e. toward "no correction needed" exactly
//! when the receiver is deaf. Weighting lets the strong samples dominate.
//!
//! Spectral-centroid measurements on real captures (+51, +306 and +901 Hz on
//! three Airspy R2s; only the +901 board failed to lock) validated the
//! quantity this estimates.

use num_complex::Complex;

/// Floor for [`CarrierEstimate::power_dbfs`], matching the recorder's own
/// wideband floor: a silent channel has no dB value, and `-inf` would poison
/// every comparison downstream.
pub const POWER_FLOOR_DBFS: f64 = -120.0;

/// Mean linear power → dBFS, the one conversion every channel-rate power
/// measurement in this crate goes through.
///
/// Public because an SNR is only meaningful when both of its terms were scaled
/// the same way: the channelizer's per-bin noise floor
/// ([`bin_power_percentile_dbfs`]) is subtracted from
/// [`CarrierEstimate::power_dbfs`], so it must not grow a second, subtly
/// different dB conversion of its own.
///
/// [`bin_power_percentile_dbfs`]:
///     crate::dsp::channelizer::PolyphaseChannelizerM2::bin_power_percentile_dbfs
#[must_use]
pub fn power_to_dbfs(mean_power: f64) -> f64 {
    // NaN is tested explicitly rather than by negating a comparison: a NaN
    // power must land on the floor, not slip through as a dB value.
    if mean_power.is_nan() || mean_power <= 0.0 {
        return POWER_FLOOR_DBFS;
    }
    (10.0 * mean_power.log10()).max(POWER_FLOOR_DBFS)
}

/// One meter's reading over the samples it has accumulated.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CarrierEstimate {
    /// Carrier offset from the channel's nominal center, in Hz. Positive means
    /// the carrier is *above* nominal, so the tuner is listening low.
    pub offset_hz: f64,
    /// Mean channel power in dBFS (full scale = unit-magnitude sample),
    /// floored at -120.
    pub power_dbfs: f64,
    /// Samples the estimate is built from.
    pub samples: u64,
}

/// Accumulating carrier-offset meter for one channel.
///
/// Fed with channel-rate samples in whatever blocks the engine happens to
/// produce; the previous sample is carried across calls so no adjacent pair is
/// lost at a block boundary. O(N) per block, no allocation, no state beyond
/// four accumulators.
pub struct CarrierMeter {
    rate: f64,
    min_samples: u64,
    /// Last sample of the previous block, so `d[n]` spans block boundaries.
    prev: Option<Complex<f32>>,
    sum_arg: f64,
    sum_w: f64,
    sum_power: f64,
    samples: u64,
}

impl CarrierMeter {
    /// A meter for a channel running at `rate` samples/sec, needing one second
    /// of samples before it will report.
    pub fn new(rate: u32) -> Self {
        Self::with_min_samples(rate, u64::from(rate).max(1))
    }

    /// A meter with an explicit minimum sample count (tests, and any caller
    /// that wants a faster but noisier reading).
    pub fn with_min_samples(rate: u32, min_samples: u64) -> Self {
        CarrierMeter {
            rate: f64::from(rate),
            min_samples: min_samples.max(1),
            prev: None,
            sum_arg: 0.0,
            sum_w: 0.0,
            sum_power: 0.0,
            samples: 0,
        }
    }

    /// Accumulate one block of channel-rate samples.
    pub fn feed(&mut self, block: &[Complex<f32>]) {
        let mut prev = self.prev;
        for &x in block {
            let (xr, xi) = (f64::from(x.re), f64::from(x.im));
            self.sum_power += xr * xr + xi * xi;
            if let Some(p) = prev {
                // d = x[n] · conj(x[n-1]); its argument is the phase advanced
                // over one sample and its magnitude is |x[n]|·|x[n-1]|, which
                // is the weight that keeps noise-only pairs from dominating.
                let (pr, pi) = (f64::from(p.re), f64::from(p.im));
                let (dr, di) = (xr * pr + xi * pi, xi * pr - xr * pi);
                let mag = (dr * dr + di * di).sqrt();
                if mag > 0.0 {
                    self.sum_arg += di.atan2(dr) * mag;
                    self.sum_w += mag;
                }
            }
            prev = Some(x);
        }
        self.samples += block.len() as u64;
        self.prev = prev;
    }

    /// The reading, or `None` until `min_samples` have been accumulated.
    pub fn estimate(&self) -> Option<CarrierEstimate> {
        if self.samples < self.min_samples || self.sum_w <= 0.0 {
            return None;
        }
        let power_dbfs = power_to_dbfs(self.sum_power / self.samples as f64);
        Some(CarrierEstimate {
            offset_hz: (self.sum_arg / self.sum_w) / std::f64::consts::TAU * self.rate,
            power_dbfs,
            samples: self.samples,
        })
    }

    /// Discard everything accumulated so far, including the carried sample, so
    /// the next window is entirely independent of this one. Used both to roll
    /// the stats window and to drop pre-retune samples that would otherwise
    /// argue for a correction that has already been applied.
    pub fn reset(&mut self) {
        self.prev = None;
        self.sum_arg = 0.0;
        self.sum_w = 0.0;
        self.sum_power = 0.0;
        self.samples = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::TAU;

    const RATE: u32 = 25_000;

    /// A pure complex tone at `hz`, phase from an f64 multiply so the test
    /// signal is drift-free and reproducible.
    fn tone(hz: f64, n: usize) -> Vec<Complex<f32>> {
        let w = TAU * hz / f64::from(RATE);
        (0..n)
            .map(|k| {
                let a = w * k as f64;
                Complex::new(a.cos() as f32, a.sin() as f32)
            })
            .collect()
    }

    /// Deterministic 32-bit LCG (numerical recipes constants). No rand crate:
    /// a DSP test that cannot be replayed byte-for-byte is not a test.
    struct Lcg(u32);
    impl Lcg {
        fn next_symbol(&mut self) -> f64 {
            self.0 = self.0.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            match (self.0 >> 16) & 0b11 {
                0 => -1800.0,
                1 => -600.0,
                2 => 600.0,
                _ => 1800.0,
            }
        }
    }

    #[test]
    fn a_positive_tone_reads_its_own_offset() {
        let mut m = CarrierMeter::new(RATE);
        // Fed in ragged blocks: the carried sample must bridge the boundaries.
        let sig = tone(900.0, RATE as usize * 2);
        for chunk in sig.chunks(777) {
            m.feed(chunk);
        }
        let e = m.estimate().expect("two seconds is past min_samples");
        assert!((e.offset_hz - 900.0).abs() < 5.0, "{e:?}");
        // A unit-magnitude tone is full scale.
        assert!(e.power_dbfs.abs() < 0.1, "{e:?}");
        assert_eq!(e.samples, RATE as u64 * 2);
    }

    #[test]
    fn a_negative_tone_reads_a_negative_offset() {
        let mut m = CarrierMeter::new(RATE);
        m.feed(&tone(-400.0, RATE as usize * 2));
        let e = m.estimate().expect("past min_samples");
        assert!((e.offset_hz + 400.0).abs() < 5.0, "{e:?}");
    }

    /// The case the loop actually runs on: random C4FM symbols, whose own
    /// contribution to instantaneous frequency is symmetric about zero, riding
    /// on a carrier that is 900 Hz high.
    #[test]
    fn four_level_fsk_reads_the_carrier_not_the_symbols() {
        const OFFSET_HZ: f64 = 900.0;
        const BAUD: f64 = 4800.0;
        let sps = f64::from(RATE) / BAUD;
        let n = RATE as usize * 2;

        let mut rng = Lcg(0x1234_5678);
        let mut phase = 0.0f64;
        let mut sym_hz = rng.next_symbol();
        let mut sig = Vec::with_capacity(n);
        for k in 0..n {
            if (k as f64 % sps) < 1.0 && k > 0 {
                sym_hz = rng.next_symbol();
            }
            phase += TAU * (sym_hz + OFFSET_HZ) / f64::from(RATE);
            sig.push(Complex::new(phase.cos() as f32, phase.sin() as f32));
        }

        let mut m = CarrierMeter::new(RATE);
        for chunk in sig.chunks(4096) {
            m.feed(chunk);
        }
        let e = m.estimate().expect("two seconds is past min_samples");
        assert!((e.offset_hz - OFFSET_HZ).abs() < 150.0, "{e:?}");
    }

    #[test]
    fn nothing_is_reported_below_min_samples() {
        let mut m = CarrierMeter::new(RATE);
        m.feed(&tone(900.0, RATE as usize - 1));
        assert_eq!(m.estimate(), None);
        m.feed(&tone(900.0, 1));
        assert!(m.estimate().is_some());
        // And a reset puts it back below the threshold.
        m.reset();
        assert_eq!(m.estimate(), None);
    }
}
