// SPDX-License-Identifier: GPL-3.0-or-later
//! Speech synthesis, ported from `jmbe.codec.MBESynthesizer` and
//! `jmbe.codec.imbe.IMBESynthesizer`.
//!
//! Each 20 ms frame is the sum of a voiced component — a bank of harmonic
//! oscillators phase-continuous with the previous frame (Algorithms #127
//! through #141) — and an unvoiced component built by notching the voiced
//! harmonics out of a pseudo-random noise spectrum and overlap-adding
//! (Algorithms #117 through #126).
//!
//! Two details are load bearing for bit-exactness with the Java original and
//! are called out inline: Algorithms #133 and #134 accumulate through
//! `double` because `Math.cos` returns a double, whereas #131 and #132 cast
//! back to `float` first; and the previous frame's phase, the previous
//! unvoiced buffer and the noise sequence are *not* advanced on a muted frame.

use crate::fft::{real_forward, real_inverse_scaled, N as FFT_N};
use crate::noise::{NoiseSequence, WhiteNoise};
use crate::params::{ModelParameters, MAX_L};
use crate::tables::SYNTHESIS_WINDOW;
use crate::SAMPLES_PER_FRAME;

const TWO_PI: f32 = 2.0 * core::f32::consts::PI;
const TWO56_OVER_TWO_PI: f32 = 256.0 / TWO_PI;
const AUDIO_SCALAR_16_BITS_SIGNED: f32 = 1.00 / (i16::MAX as f32);
const MAXIMUM_AUDIO_AMPLITUDE: f32 = 0.95;
const WHITE_NOISE_SCALAR: f32 = TWO_PI / 53125.0;

/// Algorithm #121 — the unvoiced scaling coefficient derived from the
/// synthesis and pitch refinement windows. JMBE hard-codes the computed value.
const UNVOICED_SCALING_COEFFICIENT: f32 = 146.176_96;

/// Speech synthesis window coefficient, Annex I, for `n` in -105..=105.
fn synthesis_window(n: i32) -> f32 {
    if !(-105..=105).contains(&n) {
        return 0.0;
    }
    SYNTHESIS_WINDOW[(n + 105) as usize]
}

/// Carries the inter-frame synthesis state: harmonic phases, the tail of the
/// previous unvoiced buffer and the noise generators.
pub(crate) struct Synthesizer {
    previous_phase_o: [f32; MAX_L + 1],
    previous_phase_v: [f32; MAX_L + 1],
    previous_uw: [f32; FFT_N],
    noise: NoiseSequence,
    white_noise: WhiteNoise,
}

impl Synthesizer {
    pub(crate) fn new() -> Self {
        Self {
            previous_phase_o: [0.0; MAX_L + 1],
            previous_phase_v: [0.0; MAX_L + 1],
            previous_uw: [0.0; FFT_N],
            noise: NoiseSequence::new(),
            white_noise: WhiteNoise::new(),
        }
    }

    /// `MBESynthesizer.getWhiteNoise()` — the muted-frame filler.
    pub(crate) fn white_noise(&mut self) -> [f32; SAMPLES_PER_FRAME] {
        self.white_noise.samples(0.003)
    }

    /// `MBESynthesizer.getVoice(parameters)` — 160 samples scaled to -1.0..1.0.
    pub(crate) fn voice(
        &mut self,
        parameters: &ModelParameters,
        previous: &ModelParameters,
    ) -> [f32; SAMPLES_PER_FRAME] {
        // Algorithm #117 - the pseudo-random noise sequence.
        let u = self.noise.next_buffer();

        let unvoiced = self.unvoiced(parameters, &u);
        let voiced = self.voiced(parameters, previous, &u);

        // Algorithm #142 - combine the two components.
        let mut audio = [0.0f32; SAMPLES_PER_FRAME];
        for (index, sample) in audio.iter_mut().enumerate() {
            *sample = clip((voiced[index] + unvoiced[index]) * AUDIO_SCALAR_16_BITS_SIGNED);
        }
        audio
    }

    /// Algorithms #118 through #126 — the unvoiced component.
    fn unvoiced(
        &mut self,
        parameters: &ModelParameters,
        white_noise_samples: &[f32; FFT_N],
    ) -> [f32; SAMPLES_PER_FRAME] {
        // Window the 256 noise samples, treating them as indexes -128..=127.
        let mut uw = [0.0f32; FFT_N];
        for (index, sample) in white_noise_samples.iter().enumerate() {
            uw[index] = sample * synthesis_window(index as i32 - 128);
        }

        let l = parameters.l;
        let multiplier = TWO56_OVER_TWO_PI * parameters.fundamental_frequency;

        // Algorithm #118 - 256-point DFT.
        real_forward(&mut uw);

        // Algorithm #120 - per-band scaling of the unvoiced DFT bins. Bands
        // that are voiced (and any bin outside every band, Algorithm #124) keep
        // a zero scalar and drop out.
        let mut bin_scalar = [0.0f32; 128];

        for index in 1..=l {
            if parameters.voicing[index] {
                continue;
            }

            // Algorithms #122 and #123 - the band's DFT bin edges.
            let a_min = ((index as f32 - 0.5) * multiplier).ceil() as i32;
            let b_max = ((index as f32 + 0.5) * multiplier).ceil() as i32;

            let mut numerator = 0.0f32;
            for n in a_min..b_max {
                if n < 128 {
                    let bin = 2 * n as usize;
                    numerator += uw[bin] * uw[bin];
                    numerator += uw[bin + 1] * uw[bin + 1];
                }
            }

            let denominator = (b_max - a_min) as f32;
            let scalar = UNVOICED_SCALING_COEFFICIENT * parameters.enhanced[index]
                / ((numerator / denominator) as f64).sqrt() as f32;

            for n in a_min..b_max {
                if n < 128 {
                    bin_scalar[n as usize] = scalar;
                }
            }
        }

        // Algorithms #119, #120 and #124 - apply the scalars.
        for bin in 0..128 {
            uw[2 * bin] *= bin_scalar[bin];
            uw[2 * bin + 1] *= bin_scalar[bin];
        }

        // Algorithm #125 - inverse DFT.
        real_inverse_scaled(&mut uw);

        // Algorithm #126 - weighted overlap-add with the previous frame.
        let mut unvoiced = [0.0f32; SAMPLES_PER_FRAME];
        for (n, sample) in unvoiced.iter_mut().enumerate() {
            let previous_window = synthesis_window(n as i32);
            let current_window = synthesis_window(n as i32 - SAMPLES_PER_FRAME as i32);

            // The Uw arrays are indexed 0..=255 but the algorithm wants
            // -128..=127: the previous frame supplies 0..=159 and the current
            // one -160..=-1.
            let previous_uw = if n < 128 {
                self.previous_uw[n + 128]
            } else {
                0.0
            };
            let current_uw = if n >= 32 { uw[n - 32] } else { 0.0 };

            *sample = ((previous_window * previous_uw) + (current_window * current_uw))
                / ((previous_window * previous_window) + (current_window * current_window));
        }

        self.previous_uw = uw;

        unvoiced
    }

    /// Algorithms #127 through #141 — the voiced component.
    fn voiced(
        &mut self,
        current: &ModelParameters,
        previous: &ModelParameters,
        u: &[f32; FFT_N],
    ) -> [f32; SAMPLES_PER_FRAME] {
        let current_frequency = current.fundamental_frequency;
        let previous_frequency = previous.fundamental_frequency;
        let average_frequency = (previous_frequency + current_frequency) / 2.0;
        let phase_offset_per_frame = average_frequency * SAMPLES_PER_FRAME as f32;

        // Algorithm #139 - advance the per-harmonic phase.
        let mut current_phase_v = [0.0f32; MAX_L + 1];
        for l in 1..=MAX_L {
            // Unwrap the previous phase before updating to avoid overflow.
            self.previous_phase_v[l] %= TWO_PI;
            current_phase_v[l] = self.previous_phase_v[l] + (phase_offset_per_frame * l as f32);
        }

        if !previous.has_voiced_bands() && !current.has_voiced_bands() {
            self.previous_phase_v = current_phase_v;
            return [0.0; SAMPLES_PER_FRAME];
        }

        let current_l = current.l;
        let previous_l = previous.l;
        let max_l = current_l.max(previous_l);

        // Algorithm #140 (partial) - the unvoiced band count of this frame.
        let unvoiced_band_count = current.unvoiced_band_count();

        // Algorithm #140 - the excitation phase.
        let mut current_phase_o = [0.0f32; MAX_L + 1];
        let threshold = (current_l as f32 / 4.0).floor() as usize;
        for l in 1..=MAX_L {
            if l <= threshold {
                current_phase_o[l] = current_phase_v[l];
            } else if l <= max_l {
                let pl = WHITE_NOISE_SCALAR * u[l] - core::f32::consts::PI;
                current_phase_o[l] =
                    current_phase_v[l] + ((unvoiced_band_count as f32 * pl) / current_l as f32);
            }
        }

        let current_m = &current.enhanced;
        let previous_m = &previous.enhanced;
        let mut voiced = [0.0f32; SAMPLES_PER_FRAME];

        // Algorithm #127 - four cases, selected by the voicing decisions of the
        // current and previous frames for each harmonic.
        let exceeds_threshold =
            (current_frequency - previous_frequency).abs() as f64 >= 0.1 * current_frequency as f64;

        for n in 0..SAMPLES_PER_FRAME {
            for l in 1..=max_l {
                let current_voiced = current.voicing[l];
                let previous_voiced = previous.voicing[l];

                if current_voiced && previous_voiced {
                    if l >= 8 || exceeds_threshold {
                        // Algorithm #133. `Math.cos` returns a double in Java,
                        // so the whole term — and the accumulation — is
                        // evaluated in double precision.
                        let previous_phase =
                            self.previous_phase_o[l] + (previous_frequency * n as f32 * l as f32);
                        voiced[n] = (voiced[n] as f64
                            + 2.0f32 as f64
                                * ((synthesis_window(n as i32) * previous_m[l]) as f64
                                    * (previous_phase as f64).cos()))
                            as f32;

                        let current_phase = current_phase_o[l]
                            + (current_frequency
                                * (n as i32 - SAMPLES_PER_FRAME as i32) as f32
                                * l as f32);
                        voiced[n] = (voiced[n] as f64
                            + 2.0f32 as f64
                                * ((synthesis_window(n as i32 - SAMPLES_PER_FRAME as i32)
                                    * current_m[l]) as f64
                                    * (current_phase as f64).cos()))
                            as f32;
                    } else {
                        // Algorithm #135 - linear amplitude interpolation.
                        let amplitude = previous_m[l]
                            + ((n as f32 / SAMPLES_PER_FRAME as f32)
                                * (current_m[l] - previous_m[l]));

                        // Algorithm #137
                        let ol = current_phase_o[l]
                            - self.previous_phase_o[l]
                            - (phase_offset_per_frame * l as f32);

                        // Algorithm #138
                        let wl = (ol - (TWO_PI * ((ol + core::f32::consts::PI) / TWO_PI).floor()))
                            / 160.0;

                        // Algorithm #136
                        let phase = self.previous_phase_o[l]
                            + (((previous_frequency * l as f32) + wl) * n as f32)
                            + ((current_frequency - previous_frequency)
                                * ((l * n * n) as f32 / 320.0));

                        // Algorithm #134 - again a double accumulation.
                        voiced[n] = (voiced[n] as f64
                            + 2.0f32 as f64 * (amplitude as f64 * (phase as f64).cos()))
                            as f32;
                    }
                } else if !current_voiced && previous_voiced {
                    // Algorithm #131 - cast back to float before accumulating.
                    voiced[n] += 2.0
                        * (synthesis_window(n as i32)
                            * previous_m[l]
                            * ((self.previous_phase_o[l]
                                + (previous_frequency * n as f32 * l as f32))
                                as f64)
                                .cos() as f32);
                } else if current_voiced && !previous_voiced {
                    // Algorithm #132
                    voiced[n] += 2.0
                        * (synthesis_window(n as i32 - SAMPLES_PER_FRAME as i32)
                            * current_m[l]
                            * ((current_phase_o[l]
                                + (current_frequency
                                    * (n as i32 - SAMPLES_PER_FRAME as i32) as f32
                                    * l as f32)) as f64)
                                .cos() as f32);
                }

                // Algorithm #130 - harmonics unvoiced in both frames
                // contribute nothing.
            }
        }

        self.previous_phase_v = current_phase_v;
        self.previous_phase_o = current_phase_o;

        voiced
    }
}

/// Clips to +/- 0.95 full scale, as `MBESynthesizer.clip()` does.
fn clip(value: f32) -> f32 {
    // `clamp` matches JMBE's comparison chain, NaN included: an out-of-range
    // comparison against NaN is false there, and `clamp` passes NaN through.
    value.clamp(-MAXIMUM_AUDIO_AMPLITUDE, MAXIMUM_AUDIO_AMPLITUDE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synthesis_window_is_zero_outside_its_support() {
        assert_eq!(synthesis_window(-106), 0.0);
        assert_eq!(synthesis_window(106), 0.0);
        assert_eq!(synthesis_window(-105), 0.0);
        assert_eq!(synthesis_window(105), 0.0);
        assert_eq!(synthesis_window(0), 1.0);
    }

    #[test]
    fn overlap_add_denominator_is_never_zero() {
        for n in 0..SAMPLES_PER_FRAME as i32 {
            let previous = synthesis_window(n);
            let current = synthesis_window(n - SAMPLES_PER_FRAME as i32);
            assert!(
                previous * previous + current * current > 0.0,
                "n = {n} has no window support"
            );
        }
    }
}
