// SPDX-License-Identifier: GPL-3.0-or-later
//! Digital down-converter: complex mix to baseband + multi-stage decimation
//! from a wide capture rate down to a P25 channel rate.
//!
//! This is the wideband front end that sits *ahead* of [`FrontEnd`](super::FrontEnd):
//! a recorder tuned to a wide span feeds raw IQ through a [`Ddc`] (one per
//! channel of interest) to isolate a single P25 carrier, then the channel-rate
//! [`FrontEnd`] + demodulator take over. [`crate::P25P1WidebandChannel`] pairs
//! the two so callers can feed wideband IQ directly.

use crate::P25Error;
use num_complex::Complex;

use crate::dsp::filters::{halfband_taps, kaiser_lowpass_taps, FirDecimator, HalfBandDecimator};

/// Complex mix (recursive NCO) + cheap half-band ÷2 cascade + optional sharp
/// tail decimator, taking a wide input rate down to ~`channel_rate`.
pub struct Ddc {
    /// Running local-oscillator phasor (recursive NCO).
    osc: Complex<f64>,
    /// Per-sample rotation `exp(j·phase_step)`, applied by complex multiply —
    /// avoids a per-sample sin/cos pair over millions of input samples.
    osc_step: Complex<f64>,
    /// Samples since the last magnitude renormalization.
    since_renorm: u32,
    halfbands: Vec<HalfBandDecimator>,
    tail: Option<FirDecimator>,
    output_rate: u32,
    /// Ping/pong scratch for [`Ddc::process_block`]; reused across calls so the
    /// steady state does not allocate.
    mix: Vec<Complex<f32>>,
    pong: Vec<Complex<f32>>,
}

impl Ddc {
    /// Build a DDC that shifts `offset_hz` to baseband and decimates
    /// `sample_rate` down toward `channel_rate`. Uses cheap 15-tap half-band
    /// ÷2 stages while the rate stays comfortably above the channel bandwidth,
    /// then a single sharp Kaiser stage for any leftover integer factor.
    ///
    /// # Panics
    ///
    /// Panics on invalid rates (see [`Ddc::try_new`] for the exact rules); use
    /// [`Ddc::try_new`] to handle them as an error instead.
    pub fn new(sample_rate: u32, channel_rate: u32, offset_hz: f64) -> Self {
        match Self::try_new(sample_rate, channel_rate, offset_hz) {
            Ok(ddc) => ddc,
            Err(e) => panic!("Ddc::new: {e}"),
        }
    }

    /// Fallible [`Ddc::new`]. Requires `channel_rate > 0`, `sample_rate > 0`
    /// and `sample_rate` an exact integer multiple of `channel_rate` (the
    /// decimation cascade has no fractional resampler, and `sample_rate /
    /// channel_rate == 0` would divide by zero / decimate by zero).
    pub fn try_new(sample_rate: u32, channel_rate: u32, offset_hz: f64) -> Result<Self, P25Error> {
        if channel_rate == 0 {
            return Err(P25Error::InvalidChannelRate);
        }
        if sample_rate == 0 {
            return Err(P25Error::InvalidSampleRate);
        }
        if !sample_rate.is_multiple_of(channel_rate) {
            return Err(P25Error::RateNotDivisible {
                sample_rate,
                channel_rate,
            });
        }
        let mut factor = sample_rate / channel_rate;
        let mut halfbands = Vec::new();
        let mut current = f64::from(sample_rate);
        while factor.is_multiple_of(2) && current / 2.0 >= 4.0 * f64::from(channel_rate) {
            halfbands.push(HalfBandDecimator::new(halfband_taps(15, true)));
            factor /= 2;
            current /= 2.0;
        }
        let (tail, output) = if factor > 1 {
            let (pass, stop) = Self::tail_band_edges(f64::from(channel_rate));
            let taps = kaiser_lowpass_taps(current, pass, stop, 60.0);
            (
                Some(FirDecimator::new(taps, factor)),
                current / f64::from(factor),
            )
        } else {
            (None, current)
        };
        let phase_step = -std::f64::consts::TAU * offset_hz / f64::from(sample_rate);
        Ok(Ddc {
            osc: Complex::new(1.0, 0.0),
            osc_step: Complex::new(phase_step.cos(), phase_step.sin()),
            since_renorm: 0,
            halfbands,
            tail,
            output_rate: output.round() as u32,
            mix: Vec::new(),
            pong: Vec::new(),
        })
    }

    /// `(pass_hz, stop_hz)` band edges for the sharp tail decimator at output
    /// rate `rate`.
    ///
    /// The tail's only real job is to keep the band the channel-rate
    /// [`FrontEnd`](super::FrontEnd) actually keeps — ±8 kHz, the LSM
    /// low-pass's stopband edge — free of aliases after the decimation to
    /// `rate`. Content at frequency `f` folds to `f mod rate`, so everything
    /// that can land inside ±8 kHz sits at or above `rate − 8000`; a stopband
    /// starting there is sufficient. Content between the passband edge and
    /// there folds to |f| ≥ 8 kHz, where the front end's own low-pass removes
    /// it.
    ///
    /// Two designs, in order of preference:
    ///
    /// 1. **Classic** — 12 kHz pass (comfortably wider than P25's ~7.5 kHz
    ///    occupied bandwidth) against a stop edge at the output Nyquist, so
    ///    nothing above Nyquist survives at all. Used whenever that leaves a
    ///    transition band of at least `MIN_TRANSITION`, i.e. for every rate
    ///    ≥ 41 kHz — which keeps the validated 50 kHz path unchanged tap for
    ///    tap.
    /// 2. **Alias-limited** — 8.5 kHz pass (clear of the 8 kHz the front end
    ///    keeps) against `rate − 8000`. At 25 kHz this is a 8.5 kHz transition
    ///    instead of 500 Hz: ~65 taps at a 150 kHz tail input rather than
    ///    1089, ~17× fewer multiply-accumulates, with the ±8 kHz P25 band
    ///    still ≥60 dB clean.
    ///
    /// Below ~16.5 kHz the 8 kHz band no longer fits under the output Nyquist
    /// at all, so neither design is well-formed (`stop <= pass` would give
    /// `delta_w <= 0` and a `ceil(±inf) as usize` tap count — an overflow
    /// panic in debug, a 1-tap passthrough in release). Those rates fall back
    /// to a passband edge tracking the stop edge at 96 %: sharp and expensive,
    /// but a real anti-alias filter, and not a supported P25 channel rate.
    fn tail_band_edges(rate: f64) -> (f64, f64) {
        /// Half the bandwidth the channel-rate front end keeps (the LSM
        /// low-pass stopband edge). Only this has to be alias-free.
        const P25_HALF_BW: f64 = 8_000.0;
        /// Passband edge of the classic design.
        const NOMINAL_PASS: f64 = 12_000.0;
        /// Passband edge of the alias-limited design.
        const ALIAS_PASS: f64 = 8_500.0;
        /// Narrowest transition band worth designing for; below this the tap
        /// count explodes for no benefit the front end can see.
        const MIN_TRANSITION: f64 = 8_500.0;

        let nyquist = rate / 2.0;
        if nyquist - NOMINAL_PASS >= MIN_TRANSITION {
            (NOMINAL_PASS, nyquist)
        } else if rate - P25_HALF_BW > ALIAS_PASS {
            (ALIAS_PASS, rate - P25_HALF_BW)
        } else {
            (nyquist * 0.96, nyquist)
        }
    }

    /// The actual decimated output rate (≈ `channel_rate`; exact when
    /// `sample_rate` is an integer multiple of it).
    pub fn output_rate(&self) -> u32 {
        self.output_rate
    }

    /// `(half-band stages, tail tap count)` — the shape of the cascade.
    #[cfg(test)]
    fn cascade_shape(&self) -> (usize, Option<usize>) {
        (
            self.halfbands.len(),
            self.tail.as_ref().map(|t| t.num_taps()),
        )
    }

    /// Push one wideband input sample; yields a channel-rate sample when the
    /// decimation cascade produces one.
    #[inline]
    pub fn push(&mut self, s: Complex<f32>) -> Option<Complex<f32>> {
        let osc = Complex::new(self.osc.re as f32, self.osc.im as f32);
        self.osc *= self.osc_step;
        self.since_renorm += 1;
        if self.since_renorm >= 2048 {
            // Cancel accumulated rounding drift so |osc| stays ~1.
            self.osc /= self.osc.norm();
            self.since_renorm = 0;
        }
        let mut v = s * osc;
        for hb in self.halfbands.iter_mut() {
            v = hb.push(v)?;
        }
        match self.tail.as_mut() {
            Some(t) => t.push(v),
            None => Some(v),
        }
    }

    /// Block form of [`Ddc::push`]: mix and decimate a whole slice, appending
    /// the channel-rate samples to `out`.
    ///
    /// Bit-for-bit identical to feeding the same stream through [`Ddc::push`]
    /// one sample at a time, for any splitting of the stream into blocks. The
    /// NCO keeps its f64 recursive-phasor semantics and its 2048-sample
    /// renormalization cadence exactly; each decimation stage runs its
    /// `process_block` path, whose lanes never reassociate a single output's
    /// tap sum.
    pub fn process_block(&mut self, input: &[Complex<f32>], out: &mut Vec<Complex<f32>>) {
        // Work in chunks so the intermediate stage buffers stay cache-resident
        // no matter how large a block the caller hands us.
        const CHUNK: usize = 8192;
        let Ddc {
            osc,
            osc_step,
            since_renorm,
            halfbands,
            tail,
            mix,
            pong,
            ..
        } = self;
        for chunk in input.chunks(CHUNK) {
            mix.clear();
            mix.reserve(chunk.len());
            for &s in chunk {
                let o = Complex::new(osc.re as f32, osc.im as f32);
                *osc *= *osc_step;
                *since_renorm += 1;
                if *since_renorm >= 2048 {
                    // Cancel accumulated rounding drift so |osc| stays ~1.
                    *osc /= osc.norm();
                    *since_renorm = 0;
                }
                mix.push(s * o);
            }
            for hb in halfbands.iter_mut() {
                pong.clear();
                hb.process_block(mix, pong);
                std::mem::swap(mix, pong);
            }
            match tail.as_mut() {
                Some(t) => t.process_block(mix, out),
                None => out.extend_from_slice(mix),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_rate_exact_for_divisible() {
        // 2.4 Msps → 25 kHz is ÷96; the cascade must land exactly on 25 kHz.
        let ddc = Ddc::new(2_400_000, 25_000, 400_500.0);
        assert_eq!(ddc.output_rate(), 25_000);
    }

    #[test]
    fn mixes_offset_tone_to_dc() {
        // A pure complex tone at +offset should mix to ~0 Hz: the decimated
        // output settles to a roughly constant phasor (near-zero frequency).
        let rate = 2_400_000u32;
        let offset = 100_000.0f64;
        let mut ddc = Ddc::new(rate, 25_000, offset);
        let mut out = Vec::new();
        let w = std::f64::consts::TAU * offset / f64::from(rate);
        for n in 0..200_000 {
            let ph = w * n as f64;
            let s = Complex::new(ph.cos() as f32, ph.sin() as f32);
            if let Some(c) = ddc.push(s) {
                out.push(c);
            }
        }
        // Take two well-separated settled outputs; after mixing to DC their
        // phase difference should be tiny (residual frequency ≈ 0).
        assert!(out.len() > 100);
        let a = out[out.len() - 60];
        let b = out[out.len() - 10];
        let dphase = (b * a.conj()).arg().abs();
        assert!(dphase < 0.05, "residual phase drift too large: {dphase}");
    }

    /// `process_block` must reproduce the per-sample `push` stream exactly,
    /// for any block splitting — the NCO, every half-band stage and the sharp
    /// tail all have to carry their phase across a block boundary.
    #[test]
    fn process_block_matches_push_bit_for_bit() {
        use crate::dsp::testutil::two_tone;
        let input = two_tone(2_400_000.0, 402_800.0, 539_500.0, 60_000);
        for channel_rate in [50_000u32, 25_000] {
            let mut ddc = Ddc::new(2_400_000, channel_rate, 400_500.0);
            let mut reference = Vec::new();
            for &s in &input {
                if let Some(c) = ddc.push(s) {
                    reference.push(c);
                }
            }
            assert!(!reference.is_empty());
            for size in [1usize, 7, 64, 1000, input.len()] {
                let mut b = Ddc::new(2_400_000, channel_rate, 400_500.0);
                let mut got = Vec::new();
                for chunk in input.chunks(size) {
                    b.process_block(chunk, &mut got);
                }
                assert_eq!(got.len(), reference.len(), "{channel_rate} block {size}");
                for (i, (a, r)) in got.iter().zip(reference.iter()).enumerate() {
                    assert_eq!(
                        (a.re.to_bits(), a.im.to_bits()),
                        (r.re.to_bits(), r.im.to_bits()),
                        "{channel_rate} block {size} output {i}"
                    );
                }
            }
        }
    }

    /// The 20 M → 50 k cascade (4 half-bands + a ÷25 tail) is the wideband
    /// path; check block/push equivalence there too, and across a chunk
    /// boundary larger than the internal 8192-sample chunk.
    #[test]
    fn process_block_matches_push_on_the_wideband_cascade() {
        use crate::dsp::testutil::two_tone;
        let input = two_tone(20_000_000.0, -1_062_500.0, 3_000_000.0, 40_000);
        let mut ddc = Ddc::new(20_000_000, 50_000, -1_062_500.0);
        let mut reference = Vec::new();
        for &s in &input {
            if let Some(c) = ddc.push(s) {
                reference.push(c);
            }
        }
        assert!(!reference.is_empty());
        for size in [3usize, 8192, 10_000, input.len()] {
            let mut b = Ddc::new(20_000_000, 50_000, -1_062_500.0);
            let mut got = Vec::new();
            for chunk in input.chunks(size) {
                b.process_block(chunk, &mut got);
            }
            assert_eq!(got.len(), reference.len(), "block {size}");
            for (a, r) in got.iter().zip(reference.iter()) {
                assert_eq!(
                    (a.re.to_bits(), a.im.to_bits()),
                    (r.re.to_bits(), r.im.to_bits())
                );
            }
        }
    }

    #[test]
    fn rejects_invalid_rates() {
        // channel_rate 0 would divide by zero building the decimation factor.
        assert!(Ddc::try_new(2_400_000, 0, 0.0).is_err());
        assert!(Ddc::try_new(0, 25_000, 0.0).is_err());
        // No fractional resampler: the rate must divide exactly.
        assert!(Ddc::try_new(2_400_000, 11_000, 0.0).is_err());
        assert!(Ddc::try_new(25_000, 50_000, 0.0).is_err());
        assert!(Ddc::try_new(2_400_000, 50_000, 0.0).is_ok());
    }

    #[test]
    #[should_panic(expected = "Ddc::new")]
    fn new_panics_on_invalid_rates() {
        let _ = Ddc::new(2_400_000, 0, 0.0);
    }

    /// Before the passband-edge fix a channel rate at or below 24 kHz gave the
    /// Kaiser tail `stop <= pass`, i.e. `delta_w <= 0` and a `ceil(inf)` tap
    /// count: an overflow panic in debug, a 1-tap passthrough (no
    /// anti-aliasing at all) in release.
    ///
    /// The tail is designed against the *pre-decimation* rate, so a big
    /// leftover odd factor still buys a narrow relative transition band and a
    /// large tap count. The tail redesign (see `Ddc::tail_band_edges`) cut
    /// that hard for the rates where the ±8 kHz P25 band actually fits under
    /// the output Nyquist — 24 kHz and 20 kHz here, and the validated 25 kHz
    /// path — while 12.5 kHz and 9.6 kHz keep the sharp fallback design.
    /// What matters here is that a real filter exists at every rate.
    #[test]
    fn low_channel_rates_still_design_a_real_filter() {
        for channel_rate in [24_000u32, 20_000, 12_500, 9_600] {
            let ddc = Ddc::try_new(2_400_000, channel_rate, 0.0).unwrap();
            let (_, tail) = ddc.cascade_shape();
            let taps = tail.expect("a leftover factor must give a tail filter");
            assert!(
                taps >= 17 && !taps.is_multiple_of(2),
                "{channel_rate} Hz tail is {taps} taps — not a usable anti-alias filter"
            );
            assert_eq!(ddc.output_rate(), channel_rate);
        }
    }
}

/// Golden vectors for the DDC cascade (bit-exactness guard).
///
/// The pinned hashes cover f32 *bit patterns*, so they also depend on the
/// platform's `sin`/`cos`/`powf` for the tap design. They are meant to catch
/// unintended changes to this crate's arithmetic (notably the planned FIR
/// block/SIMD rework); a libm difference on an exotic target would show up
/// here too and should be re-pinned deliberately.
#[cfg(test)]
mod golden {
    use super::*;
    use crate::dsp::testutil::{hash_complex, two_tone};

    /// The cascade shape (half-band stage count and sharp-tail tap count) for
    /// every configuration on a validated decode path. These numbers must not
    /// move: the tail's Kaiser design is what the control-fixture output is
    /// built on.
    #[test]
    fn cascade_shapes_are_pinned() {
        assert_eq!(
            Ddc::new(20_000_000, 50_000, 0.0).cascade_shape(),
            (4, Some(351))
        );
        assert_eq!(
            Ddc::new(2_400_000, 50_000, 0.0).cascade_shape(),
            (3, Some(85))
        );
        // DELIBERATE RE-PIN (tail redesign): 1089 → 65. The 25 kHz tail used
        // to be designed for a 12 kHz passband against a 12.5 kHz stop — a
        // 500 Hz transition, and 1089 taps at the 150 kHz tail input rate —
        // even though only the ±8 kHz the channel-rate front end keeps has to
        // be alias-free. The alias-limited design (8.5 kHz pass, 17 kHz stop,
        // see `Ddc::tail_band_edges`) protects exactly that band at the same
        // 60 dB with ~17× fewer multiply-accumulates. NOT bit-exact with the
        // old 25 kHz output; the 50 kHz designs above are unchanged.
        assert_eq!(
            Ddc::new(2_400_000, 25_000, 0.0).cascade_shape(),
            (4, Some(65))
        );
        // Same redesign at the wideband rate: the ÷25 tail runs at 625 kHz,
        // where the old design was 4529 taps.
        assert_eq!(
            Ddc::new(20_000_000, 25_000, 0.0).cascade_shape(),
            (5, Some(269))
        );
    }

    /// The redesigned tail must still put ≥60 dB on everything that could
    /// alias into the ±8 kHz band the channel-rate front end keeps.
    #[test]
    fn tail_protects_the_p25_band_from_aliases() {
        for (rate, tail_in) in [(25_000.0f64, 150_000.0f64), (50_000.0, 300_000.0)] {
            let (pass, stop) = Ddc::tail_band_edges(rate);
            assert!(pass >= 8_000.0, "{rate}: passband edge {pass} clips P25");
            assert!(
                stop <= rate - 8_000.0,
                "{rate}: stopband starts at {stop}, above the {} Hz alias edge",
                rate - 8_000.0
            );
            let taps = kaiser_lowpass_taps(tail_in, pass, stop, 60.0);
            let resp = |f_hz: f64| -> f64 {
                let (mut re, mut im) = (0.0f64, 0.0f64);
                for (i, &t) in taps.iter().enumerate() {
                    let w = std::f64::consts::TAU * f_hz / tail_in * i as f64;
                    re += f64::from(t) * w.cos();
                    im -= f64::from(t) * w.sin();
                }
                20.0 * (re * re + im * im).sqrt().log10()
            };
            // In band: essentially untouched.
            assert!(resp(0.0).abs() < 0.05, "{rate}: DC gain {}", resp(0.0));
            assert!(resp(6_000.0) > -0.5, "{rate}: 6 kHz {}", resp(6_000.0));
            // Every image band that folds into ±8 kHz is well down.
            let mut k = 1.0;
            while k * rate + 8_000.0 < tail_in / 2.0 {
                for f in [k * rate - 8_000.0, k * rate, k * rate + 8_000.0] {
                    assert!(resp(f) < -55.0, "{rate}: alias at {f} Hz is {}", resp(f));
                }
                k += 1.0;
            }
        }
    }

    /// Exact output stream of the 2.4 M → 50 kHz DDC (the control fixture's
    /// configuration) over a deterministic two-tone input.
    #[test]
    fn decimated_output_is_pinned() {
        let mut ddc = Ddc::new(2_400_000, 50_000, 400_500.0);
        let input = two_tone(2_400_000.0, 402_800.0, 539_500.0, 240_000);
        let mut out = Vec::new();
        for s in input {
            if let Some(c) = ddc.push(s) {
                out.push(c);
            }
        }
        assert_eq!(out.len(), 5000);
        assert_eq!(
            hash_complex(&out),
            9_553_718_538_905_167_246,
            "DDC output changed; first: {:?}",
            &out[..4]
        );
    }
}
