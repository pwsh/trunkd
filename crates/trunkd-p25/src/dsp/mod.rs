// SPDX-License-Identifier: GPL-3.0-or-later
//! Front-end DSP: decimation cascade, baseband low-pass, RRC matched filter
//! (spec §3).

pub mod bank;
pub mod carrier;
pub mod channelizer;
pub mod ddc;
pub mod filters;

pub use bank::{BankHandle, ChannelBank, SharedChannelBank};
pub use carrier::{CarrierEstimate, CarrierMeter};
pub use channelizer::{Channelizer, PolyphaseChannelizerM2};
pub use ddc::Ddc;

use crate::P25Error;
use filters::{
    halfband_taps, kaiser_lowpass_taps, rrc_taps_with_half_sps_quirk, ComplexFir, HalfBandDecimator,
};
use num_complex::Complex;

/// Test-only helpers shared by the dsp golden-vector tests.
#[cfg(test)]
pub(crate) mod testutil {
    use num_complex::Complex;

    /// FNV-1a over the raw IEEE-754 bit patterns of a complex f32 stream.
    /// Bit patterns, not values: the point is to catch *any* arithmetic change.
    pub fn hash_complex(v: &[Complex<f32>]) -> u64 {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for c in v {
            for word in [c.re.to_bits(), c.im.to_bits()] {
                for byte in word.to_le_bytes() {
                    h ^= u64::from(byte);
                    h = h.wrapping_mul(0x1000_0000_01b3);
                }
            }
        }
        h
    }

    /// Deterministic two-tone test signal at `sample_rate`: a unit carrier at
    /// `tone_hz` plus a quarter-amplitude out-of-band tone at `tone2_hz` that
    /// the filters are supposed to reject. Phases come from f64 multiplies (not
    /// an accumulator), so the stream is reproducible and drift-free.
    pub fn two_tone(sample_rate: f64, tone_hz: f64, tone2_hz: f64, n: usize) -> Vec<Complex<f32>> {
        let w1 = std::f64::consts::TAU * tone_hz / sample_rate;
        let w2 = std::f64::consts::TAU * tone2_hz / sample_rate;
        (0..n)
            .map(|k| {
                let (a, b) = (w1 * k as f64, w2 * k as f64);
                Complex::new(
                    (a.cos() + 0.25 * b.cos()) as f32,
                    (a.sin() + 0.25 * b.sin()) as f32,
                )
            })
            .collect()
    }
}

/// Demodulator selection — decides the low-pass band edges (spec §3.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DemodMode {
    /// C4FM (5200 / 6500 Hz edges).
    C4fm,
    /// LSM / simulcast — the primary path (7250 / 8000 Hz edges).
    Lsm,
}

/// Compute the power-of-2 decimation for an input rate (spec §3.1):
/// halve while `sr / decimation >= 38400`.
pub fn decimation_for(sample_rate: u32) -> u32 {
    let mut decimation = 1u32;
    while sample_rate / decimation >= 38_400 {
        decimation *= 2;
    }
    decimation
}

/// Front end: half-band decimation cascade → baseband LPF → RRC (I and Q).
pub struct FrontEnd {
    stages: Vec<HalfBandDecimator>,
    lpf: ComplexFir,
    rrc: ComplexFir,
    /// Output (decimated) sample rate.
    pub decimated_rate: f64,
    /// Samples per symbol at the decimated rate.
    pub samples_per_symbol: f64,
}

impl FrontEnd {
    /// Build the front end for `sample_rate` and mode.
    ///
    /// The rate must be > 9600 *and* leave the decimated rate at or above
    /// twice the mode's stopband edge (16 kHz for LSM, 13 kHz for C4FM), or
    /// the baseband low-pass would alias rather than filter.
    pub fn new(sample_rate: u32, mode: DemodMode) -> Result<Self, P25Error> {
        if sample_rate <= 9600 {
            return Err(P25Error::SampleRateTooLow {
                rate: sample_rate,
                min: 9600,
            });
        }
        let decimation = decimation_for(sample_rate);
        let decimated_rate = f64::from(sample_rate) / f64::from(decimation);

        // Baseband low-pass band edges (spec §3.2).
        let (pass, stop) = match mode {
            DemodMode::C4fm => (5200.0, 6500.0),
            DemodMode::Lsm => (7250.0, 8000.0),
        };
        // The windowed-sinc cutoff is (pass + stop)/2; the design is only
        // meaningful while the stopband edge stays below Nyquist. Rates just
        // above the 9600 floor (up to 16 kHz for LSM, 13 kHz for C4FM) put the
        // cutoff past 0.5 cycles/sample, so the "low-pass" aliases instead of
        // filtering. The reference throws for such rates; reject them here.
        if decimated_rate < 2.0 * stop {
            return Err(P25Error::RateBelowNyquist {
                decimated_rate: decimated_rate as u32,
                required: (2.0 * stop) as u32,
            });
        }

        // Decimation cascade (spec §3.1): ÷2 = 63-tap Hamming half-band;
        // ÷4 prepends a 23-tap Blackman stage; ÷8 prepends a 15-tap Blackman
        // stage. Deeper factors repeat the 15-tap pre-stage (extension: the
        // reference tops out at ÷8 for its supported rates).
        let mut stages = Vec::new();
        let mut remaining = decimation;
        while remaining > 8 {
            stages.push(HalfBandDecimator::new(halfband_taps(15, true)));
            remaining /= 2;
        }
        if remaining == 8 {
            stages.push(HalfBandDecimator::new(halfband_taps(15, true)));
            remaining /= 2;
        }
        if remaining == 4 {
            stages.push(HalfBandDecimator::new(halfband_taps(23, true)));
            remaining /= 2;
        }
        if remaining == 2 {
            stages.push(HalfBandDecimator::new(halfband_taps(63, false)));
        }

        // Baseband low-pass (spec §3.2 band edges, chosen above; Kaiser
        // equivalent design, see filters.rs docs). Ripple 0.01 dB ⇒ ripple
        // amplitude (10^(0.01/20)−1)/(10^(0.01/20)+1) ≈ 5.76e-4 ⇒ ≈64.8 dB.
        let lpf = ComplexFir::new(kaiser_lowpass_taps(decimated_rate, pass, stop, 64.8));

        // RRC matched filter with the /2 quirk (spec §3.3).
        let sps = decimated_rate / 4800.0;
        let rrc = ComplexFir::new(rrc_taps_with_half_sps_quirk(sps, 16.0, 0.2));

        Ok(FrontEnd {
            stages,
            lpf,
            rrc,
            decimated_rate,
            samples_per_symbol: sps,
        })
    }

    /// Push one channel-rate sample; yields a filtered decimated-rate sample
    /// when the cascade produces one.
    #[inline]
    pub fn push(&mut self, s: Complex<f32>) -> Option<Complex<f32>> {
        let mut v = s;
        for stage in self.stages.iter_mut() {
            v = stage.push(v)?;
        }
        Some(self.rrc.push(self.lpf.push(v)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decimation_rule() {
        assert_eq!(decimation_for(25_000), 1);
        assert_eq!(decimation_for(50_000), 2);
        assert_eq!(decimation_for(100_000), 4);
        assert_eq!(decimation_for(2_400_000), 64); // 2.4M/64 = 37500 < 38400
        assert_eq!(decimation_for(38_400), 2);
        assert_eq!(decimation_for(38_399), 1);
    }

    #[test]
    fn frontend_50k() {
        let fe = FrontEnd::new(50_000, DemodMode::Lsm).unwrap();
        assert!((fe.decimated_rate - 25_000.0).abs() < 1e-9);
        assert!((fe.samples_per_symbol - 5.208333).abs() < 1e-3);
    }

    #[test]
    fn rejects_low_rate() {
        assert!(FrontEnd::new(9600, DemodMode::Lsm).is_err());
    }

    /// Rates just above the 9600 floor put the windowed-sinc cutoff past
    /// 0.5 cycles/sample, so the "low-pass" aliases instead of filtering.
    #[test]
    fn rejects_rates_below_the_stopband_nyquist() {
        // LSM stops at 8 kHz → needs ≥ 16 kHz decimated.
        assert!(FrontEnd::new(12_000, DemodMode::Lsm).is_err());
        assert!(FrontEnd::new(15_999, DemodMode::Lsm).is_err());
        assert!(FrontEnd::new(16_000, DemodMode::Lsm).is_ok());
        // C4FM stops at 6.5 kHz → needs ≥ 13 kHz decimated.
        assert!(FrontEnd::new(12_999, DemodMode::C4fm).is_err());
        assert!(FrontEnd::new(13_000, DemodMode::C4fm).is_ok());
        // The validated rates are unaffected.
        assert!(FrontEnd::new(25_000, DemodMode::Lsm).is_ok());
        assert!(FrontEnd::new(50_000, DemodMode::Lsm).is_ok());
    }
}

/// Golden vectors for the channel-rate front end (bit-exactness guard).
#[cfg(test)]
mod golden {
    use super::testutil::{hash_complex, two_tone};
    use super::*;

    /// Exact output stream of the 50 kHz LSM front end (the control fixture's
    /// configuration: ÷2 half-band → 25 kHz, Kaiser LPF, RRC) over a
    /// deterministic two-tone input.
    #[test]
    fn frontend_output_is_pinned() {
        let mut fe = FrontEnd::new(50_000, DemodMode::Lsm).unwrap();
        let input = two_tone(50_000.0, 1_700.0, 11_300.0, 20_000);
        let mut out = Vec::new();
        for s in input {
            if let Some(c) = fe.push(s) {
                out.push(c);
            }
        }
        assert_eq!(out.len(), 10_000);
        assert_eq!(
            hash_complex(&out),
            3_338_919_831_707_029_804,
            "FrontEnd output changed; first: {:?}",
            &out[..4]
        );
    }
}
