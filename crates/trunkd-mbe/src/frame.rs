// SPDX-License-Identifier: GPL-3.0-or-later
//! The 144-bit IMBE voice frame, ported from `jmbe.codec.imbe.IMBEFrame`.
//!
//! Decoding a frame is deinterleave, Golay/Hamming EDAC, derandomise, then
//! reconstruction of the model parameters: the fundamental frequency from
//! vector b0, the voicing decisions from b1, the gain from b2 and the spectral
//! amplitude prediction residuals from b3..b(L+1).

use crate::bits::Bits;
use crate::edac::{golay23_check_and_correct, hamming15_check_and_correct};
use crate::params::{ModelParameters, AMP_LEN};
use crate::tables::{GAIN, GAIN_INDEXES, HARMONIC_ALLOCATION, QUANTIZED_VALUE_INDEXES, STEP_SIZES};

/// Bits in one IMBE voice frame.
pub(crate) const FRAME_BITS: usize = 144;

/// Widest J-block in `HarmonicAllocation`, plus one for the 1-based index.
const MAX_J: usize = 12;

/// Deinterleave map from `jmbe.codec.imbe.IMBEInterleave`.
const DEINTERLEAVE: [u8; FRAME_BITS] = [
    0, 24, 48, 72, 96, 120, 25, 1, 73, 49, 121, 97, 2, 26, 50, 74, 98, 122, 27, 3, 75, 51, 123, 99,
    4, 28, 52, 76, 100, 124, 29, 5, 77, 53, 125, 101, 6, 30, 54, 78, 102, 126, 31, 7, 79, 55, 127,
    103, 8, 32, 56, 80, 104, 128, 33, 9, 81, 57, 129, 105, 10, 34, 58, 82, 106, 130, 35, 11, 83,
    59, 131, 107, 12, 36, 60, 84, 108, 132, 37, 13, 85, 61, 133, 109, 14, 38, 62, 86, 110, 134, 39,
    15, 87, 63, 135, 111, 16, 40, 64, 88, 112, 136, 41, 17, 89, 65, 137, 113, 18, 42, 66, 90, 114,
    138, 43, 19, 91, 67, 139, 115, 20, 44, 68, 92, 116, 140, 45, 21, 93, 69, 141, 117, 22, 46, 70,
    94, 118, 142, 47, 23, 95, 71, 143, 119,
];

/// First twelve bits of coset word c0, used to seed the derandomiser.
const RANDOMIZER_SEED: [u8; 12] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];

/// Message bit indexes of information vector b0 (the fundamental frequency).
const VECTOR_B0: [u8; 8] = [0, 1, 2, 3, 4, 5, 141, 142];

/// Message bit index of the voiced/unvoiced decision for each of the L
/// harmonics — harmonic `l` inherits the flag of the K band it falls in.
const VOICE_DECISION_INDEX: [u8; 57] = [
    0, 92, 92, 92, 93, 93, 93, 94, 94, 94, 95, 95, 95, 96, 96, 96, 97, 97, 97, 98, 98, 98, 99, 99,
    99, 100, 100, 100, 101, 101, 101, 102, 102, 102, 107, 107, 107, 107, 107, 107, 107, 107, 107,
    107, 107, 107, 107, 107, 107, 107, 107, 107, 107, 107, 107, 107, 107,
];

/// Coefficient offsets for bit lengths 0..=10: `2^(len - 1) - 0.5`.
const COEFFICIENT_OFFSET: [f32; 11] = [
    0.0, 0.5, 1.5, 3.5, 7.5, 15.5, 31.5, 63.5, 127.5, 255.5, 511.5,
];

/// A deinterleaved, error-corrected IMBE voice frame.
pub(crate) struct ImbeFrame {
    bits: Bits<FRAME_BITS>,
    /// Per-coset error counts, `[c0, c1, c2, c3, c4, c5, c6]`.
    errors: [u32; 7],
    error_count_total: u32,
    /// Decoded b0 index, or `None` when it fell outside 0..=207.
    fundamental_index: Option<usize>,
}

impl ImbeFrame {
    /// Deinterleaves, error-corrects and derandomises one 144-bit frame.
    pub(crate) fn new(data: &[u8; 18]) -> Self {
        let source: Bits<FRAME_BITS> = Bits::from_bytes(data);

        // IMBEInterleave.deinterleave
        let mut bits = Bits::<FRAME_BITS>::zeroed();
        let mut deinterleaved = [false; FRAME_BITS];
        for (index, &target) in DEINTERLEAVE.iter().enumerate() {
            if source.get(index) {
                deinterleaved[target as usize] = true;
            }
        }
        for (index, &value) in deinterleaved.iter().enumerate() {
            if value {
                bits.flip(index);
            }
        }

        let mut errors = [0u32; 7];
        let mut error_count_total = 0u32;

        errors[0] = golay23_check_and_correct(&mut bits, 0);
        error_count_total += errors[0];

        derandomize(&mut bits);

        errors[1] = golay23_check_and_correct(&mut bits, 23);
        error_count_total += errors[1];
        errors[2] = golay23_check_and_correct(&mut bits, 46);
        error_count_total += errors[2];
        errors[3] = golay23_check_and_correct(&mut bits, 69);
        error_count_total += errors[3];
        errors[4] = hamming15_check_and_correct(&mut bits, 92);
        error_count_total += errors[4];
        errors[5] = hamming15_check_and_correct(&mut bits, 107);
        error_count_total += errors[5];
        errors[6] = hamming15_check_and_correct(&mut bits, 122);
        error_count_total += errors[6];

        let b0 = bits.get_int_indexes(&VECTOR_B0) as usize;
        let fundamental_index = (b0 <= 207).then_some(b0);

        Self {
            bits,
            errors,
            error_count_total,
            fundamental_index,
        }
    }

    /// Total bit errors detected/corrected across the frame's coset words.
    pub(crate) fn error_count_total(&self) -> u32 {
        self.error_count_total
    }

    /// `IMBEFrame.getModelParameters(previous)`.
    ///
    /// Returns the parameters plus a flag indicating that the voicing decisions
    /// are shared with `previous` (see `ModelParameters::copy_from`).
    pub(crate) fn model_parameters(&self, previous: &ModelParameters) -> (ModelParameters, bool) {
        let mut parameters = match self.fundamental_index {
            Some(index) => ModelParameters::new(index),
            None => ModelParameters::invalid(),
        };

        parameters.set_errors(
            previous.error_rate,
            self.errors[0],
            self.errors[4],
            self.error_count_total,
        );

        // Too many errors, or an invalid fundamental frequency: repeat the
        // previous frame's model parameters.
        if parameters.repeat_required() {
            let shared_voicing = parameters.copy_from(previous);
            return (parameters, shared_voicing);
        }

        parameters.set_voicing(&self.voicing_decisions(parameters.l));

        let (log2_spectral, log2_len) = self.log2_spectral_amplitudes(&parameters, previous);
        parameters.log2_spectral = log2_spectral;
        parameters.log2_len = log2_len;

        // Algorithm #104 - M[l] = 2 ^ log2M[l].
        let mut spectral = [0.0f32; AMP_LEN];
        for index in 0..log2_len {
            spectral[index] = 2.0f64.powf(log2_spectral[index] as f64) as f32;
        }
        parameters.set_spectral_amplitudes(
            &spectral,
            log2_len,
            previous.local_energy,
            previous.amplitude_threshold,
        );

        (parameters, false)
    }

    /// `IMBEFrame.getVoicingDecisions()`.
    fn voicing_decisions(&self, l: usize) -> [bool; AMP_LEN] {
        let mut decisions = [false; AMP_LEN];
        for index in 1..=l {
            decisions[index] = self.bits.get(VOICE_DECISION_INDEX[index] as usize);
        }
        decisions
    }

    /// Algorithms #68 through #74 — reconstructs the spectral amplitude
    /// prediction residuals `T[1..=L]`.
    fn spectral_amplitude_prediction_residuals(&self, l: usize) -> [f32; AMP_LEN] {
        let table = l - 9;

        // Algorithm #67 - the gain value b2.
        let gain_index = self.bits.get_int_indexes(&GAIN_INDEXES[table]) as usize;
        let mut g = [0.0f32; 7];
        g[1] = GAIN[gain_index];

        let step_sizes = STEP_SIZES[table];
        let indexes = QUANTIZED_VALUE_INDEXES[table];

        // Algorithm #68 - decode the gain vector G.
        for m in 3..=7usize {
            let index_set = indexes[m - 3];
            if !index_set.is_empty() {
                let b = self.bits.get_int_indexes(index_set);
                g[m - 1] = step_sizes[m - 3] * (b as f32 - COEFFICIENT_OFFSET[index_set.len()]);
            }
        }

        let allocations = &HARMONIC_ALLOCATION[table];
        let mut c = [[0.0f32; MAX_J + 1]; 7];

        // Algorithms #69 and #70 - gain vector R as the inverse DCT of G.
        for i in 1..=6usize {
            c[i][1] = g[1];
            for m in 2..=6usize {
                c[i][1] += 2.0
                    * g[m]
                    * ((core::f64::consts::PI * (m - 1) as f32 as f64 * (i as f32 - 0.5) as f64)
                        / 6.0f32 as f64)
                        .cos() as f32;
            }
        }

        // Algorithms #71 and #72 - the higher order DCT coefficients.
        for i in 1..=6usize {
            let harmonics = allocations[i - 1];
            if harmonics.len() > 1 {
                for j in 2..=harmonics.len() {
                    let m = harmonics[j - 1] as usize;
                    let index_set = indexes[m - 3];
                    if !index_set.is_empty() {
                        let b = self.bits.get_int_indexes(index_set);
                        c[i][j] =
                            step_sizes[m - 3] * (b as f32 - COEFFICIENT_OFFSET[index_set.len()]);
                    }
                }
            }
        }

        // Algorithms #73 and #74 - inverse DCT of C into T.
        let mut t = [0.0f32; AMP_LEN];
        let mut index = 1usize;
        for i in 1..=6usize {
            let ji = allocations[i - 1].len();
            for j in 1..=ji {
                t[index] = c[i][1];
                if ji >= 2 {
                    for k in 2..=ji {
                        t[index] += 2.0
                            * c[i][k]
                            * ((core::f64::consts::PI
                                * (k - 1) as f32 as f64
                                * (j as f32 - 0.5) as f64)
                                / ji as f32 as f64)
                                .cos() as f32;
                    }
                }
                index += 1;
            }
        }

        t
    }

    /// Algorithms #75 through #79 — the current frame's `log2M` amplitudes,
    /// predicted from the previous frame's with interpolation for a change in
    /// L. Returns the array and its logical length (`L + 1`).
    fn log2_spectral_amplitudes(
        &self,
        parameters: &ModelParameters,
        previous: &ModelParameters,
    ) -> ([f32; AMP_LEN], usize) {
        let l = parameters.l;
        let l_float = l as f32;
        let l_plus_1 = l + 1;
        let previous_l = previous.l;

        // Resize the previous frame's log2M, padding new indexes with the
        // previously highest value (Algorithm #79).
        let mut previous_log2 = previous.log2_spectral;
        let next_l = previous_l.max(l) + 1;
        if next_l > previous.log2_len - 1 {
            let highest = previous_log2[previous.log2_len - 1];
            for value in previous_log2
                .iter_mut()
                .take(next_l + 1)
                .skip(previous.log2_len)
            {
                *value = highest;
            }
        }

        let t = self.spectral_amplitude_prediction_residuals(l);

        let scale = previous_l as f32 / l_float;

        let mut kl_floor = [0usize; AMP_LEN];
        let mut sl = [0.0f32; AMP_LEN];
        for index in 1..l_plus_1 {
            // Algorithm #75
            let kl = index as f32 * scale;
            kl_floor[index] = kl.floor() as usize;
            // Algorithm #76
            sl[index] = kl - kl_floor[index] as f32;
        }

        // Algorithm #77 (partial) - the summation term.
        let mut sum = 0.0f32;
        for index in 1..l_plus_1 {
            sum += ((1.0 - sl[index]) * previous_log2[kl_floor[index]])
                + (sl[index] * previous_log2[kl_floor[index] + 1]);
        }

        // Algorithm #55 - prediction coefficient.
        let p = if l_float <= 15.0 {
            0.4
        } else if l_float <= 24.0 {
            0.03 * l_float - 0.05
        } else {
            0.7
        };

        let pl_sum = p / l_float * sum;

        let mut log2m = [0.0f32; AMP_LEN];
        for index in 1..=l {
            log2m[index] = t[index]
                + (p * (1.0 - sl[index]) * previous_log2[kl_floor[index]])
                + (p * sl[index] * previous_log2[kl_floor[index] + 1])
                - pl_sum;
        }

        (log2m, l_plus_1)
    }
}

/// Removes the randomiser: a pseudo-random sequence seeded from the first
/// twelve bits of coset word c0 is XORed over coset words c1..c6
/// (Algorithms #52 through #54).
fn derandomize(bits: &mut Bits<FRAME_BITS>) {
    const OFFSET: usize = 23;

    let seed = bits.get_int_indexes(&RANDOMIZER_SEED);

    // Algorithm #52
    let mut pr = 16u32.wrapping_mul(seed);

    for x in 0..114usize {
        // Algorithm #53
        pr = (173u32.wrapping_mul(pr).wrapping_add(13849)) % 65536;

        // Algorithm #54 - values 32768 and above contribute a 1 bit.
        if pr >= 32768 {
            bits.flip(x + OFFSET);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tables::{FUNDAMENTAL_FREQUENCY, FUNDAMENTAL_L};

    /// Look-up of the generated fundamental frequency table.
    fn fundamental(index: usize) -> (f32, usize) {
        (FUNDAMENTAL_FREQUENCY[index], FUNDAMENTAL_L[index] as usize)
    }

    #[test]
    fn deinterleave_is_a_permutation() {
        let mut seen = [false; FRAME_BITS];
        for &target in DEINTERLEAVE.iter() {
            assert!(!seen[target as usize], "duplicate target {target}");
            seen[target as usize] = true;
        }
        assert!(seen.iter().all(|&s| s));
    }

    /// The deinterleave map moves interleaved bit `i` to frame bit
    /// `DEINTERLEAVE[i]`; these are the first and last few entries taken
    /// straight from `IMBEInterleave.DEINTERLEAVE`.
    #[test]
    fn deinterleave_matches_the_reference_table() {
        assert_eq!(&DEINTERLEAVE[..7], &[0, 24, 48, 72, 96, 120, 25]);
        assert_eq!(&DEINTERLEAVE[FRAME_BITS - 4..], &[95, 71, 143, 119]);
    }

    #[test]
    fn fundamental_table_matches_the_spec_formula() {
        for index in 0..208usize {
            let expected_frequency = (4.0 * core::f64::consts::PI / (index as f64 + 39.5)) as f32;
            let expected_l = (0.9254
                * (core::f64::consts::PI / expected_frequency as f64 + 0.25).floor())
            .floor() as usize;
            let (frequency, l) = fundamental(index);
            assert_eq!(frequency, expected_frequency, "index {index}");
            assert_eq!(l, expected_l, "index {index}");
        }
        // The default (start of sequence) frame is b0 index 134. JMBE's
        // source comments it as "L = 30 & w0 = 0.2985 * Pi", but that comment
        // is stale: the constructor's own formula yields L = 39.
        assert_eq!(fundamental(134).1, 39);
    }

    #[test]
    fn harmonic_allocations_cover_l_harmonics() {
        for l in 9..=56usize {
            let total: usize = HARMONIC_ALLOCATION[l - 9].iter().map(|b| b.len()).sum();
            assert_eq!(total, l, "L = {l}");
            assert!(HARMONIC_ALLOCATION[l - 9].iter().all(|b| b.len() <= MAX_J));
            assert_eq!(QUANTIZED_VALUE_INDEXES[l - 9].len(), l - 1, "L = {l}");
            assert_eq!(STEP_SIZES[l - 9].len(), l - 1, "L = {l}");
        }
    }
}
