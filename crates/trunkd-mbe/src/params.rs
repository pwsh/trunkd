// SPDX-License-Identifier: GPL-3.0-or-later
//! MBE / IMBE model parameters, ported from `jmbe.codec.MBEModelParameters`
//! and `jmbe.codec.imbe.IMBEModelParameters`.
//!
//! This carries the decoded fundamental frequency, voicing decisions and
//! spectral amplitudes for one 20 ms frame, plus the running error statistics
//! that drive spectral-amplitude enhancement (chapter 8), adaptive smoothing,
//! frame repeat and muting.
//!
//! The Java originals hold Java-sized arrays (`L + 1` entries) and lean on that
//! length in a few places; here everything is a fixed `MAX_L + 2` buffer with an
//! explicit logical length, and indexes above `L` are kept zeroed so that the
//! "resize pads with false/zero" behavior falls out for free.

use crate::tables::{FUNDAMENTAL_FREQUENCY, FUNDAMENTAL_L};

/// Largest harmonic count the fundamental-frequency table can produce.
pub(crate) const MAX_L: usize = 56;

/// Spectral-amplitude array length. JMBE's `IMBEFrame.resize()` grows the
/// previous frame's `log2M` to `max(L, previousL) + 2` entries, so buffers are
/// sized for that worst case.
pub(crate) const AMP_LEN: usize = MAX_L + 2;

/// b0 index used for the default (start of sequence / give-up) frame.
///
/// JMBE labels this entry "L = 30 & w0 = 0.2985 * Pi" but that comment is
/// stale — the enum constructor's own formula gives `w0 = 4*pi/173.5` and
/// `L = 39`, which is what the reference decoder actually uses.
pub(crate) const DEFAULT_FUNDAMENTAL: usize = 134;

/// Repeat count above which JMBE stops repeating speech and resets to the
/// default frame (`IMBEModelParameters.MAX_HEADROOM_THRESHOLD`).
const MAX_HEADROOM_THRESHOLD: u32 = 3;

const PI_96: f32 = 0.96 * core::f32::consts::PI;

/// Decoded model parameters for one IMBE frame.
#[derive(Clone)]
pub(crate) struct ModelParameters {
    /// `w0`, in radians per sample, 0.0..0.5.
    pub(crate) fundamental_frequency: f32,
    /// Number of harmonics.
    pub(crate) l: usize,
    /// Set when the b0 index decoded outside 0..=207
    /// (`IMBEFundamentalFrequency.INVALID`).
    pub(crate) invalid: bool,
    /// Voiced/unvoiced flag per harmonic, indexes 1..=L (index 0 unused but
    /// counted by `unvoiced_band_count`, as in Java).
    pub(crate) voicing: [bool; AMP_LEN],
    /// `log2(M[l])`, indexes 1..=L.
    pub(crate) log2_spectral: [f32; AMP_LEN],
    /// Logical length of `log2_spectral` (`L + 1` for a decoded frame).
    pub(crate) log2_len: usize,
    /// `M[l]`, indexes 1..=L.
    pub(crate) spectral: [f32; AMP_LEN],
    /// Logical length of `spectral`.
    pub(crate) spectral_len: usize,
    /// Enhanced `M[l]` (chapter 8), indexes 1..=L.
    pub(crate) enhanced: [f32; AMP_LEN],
    /// Running local energy estimate (Algorithm #111).
    pub(crate) local_energy: f32,
    /// Adaptive amplitude threshold `Tm` (Algorithm #115).
    pub(crate) amplitude_threshold: i32,
    /// Running error rate (Algorithm #96).
    pub(crate) error_rate: f32,
    /// Total bit errors corrected across all coset words.
    pub(crate) error_count_total: u32,
    /// Bit errors corrected in coset word 4.
    pub(crate) error_count4: u32,
    /// Bit errors corrected in coset word 0.
    pub(crate) error_count_coset0: u32,
    /// How many consecutive frames have been repeated.
    pub(crate) repeat_count: u32,
}

impl ModelParameters {
    /// `new IMBEModelParameters(frequency)` — voicing all unvoiced, spectral
    /// amplitudes all 1.0.
    pub(crate) fn new(fundamental_index: usize) -> Self {
        let l = FUNDAMENTAL_L[fundamental_index] as usize;
        let mut spectral = [0.0f32; AMP_LEN];
        for value in spectral.iter_mut().take(l + 1) {
            *value = 1.0;
        }
        Self {
            fundamental_frequency: FUNDAMENTAL_FREQUENCY[fundamental_index],
            l,
            invalid: false,
            voicing: [false; AMP_LEN],
            log2_spectral: [0.0; AMP_LEN],
            log2_len: l + 1,
            spectral,
            spectral_len: l + 1,
            enhanced: spectral,
            local_energy: 75_000.0,
            amplitude_threshold: 20_480,
            error_rate: 0.0,
            error_count_total: 0,
            error_count4: 0,
            error_count_coset0: 0,
            repeat_count: 0,
        }
    }

    /// `new IMBEModelParameters()` — the frame used at the start of a sequence.
    pub(crate) fn default_frame() -> Self {
        Self::new(DEFAULT_FUNDAMENTAL)
    }

    /// A frame whose b0 index decoded outside the valid range.
    pub(crate) fn invalid() -> Self {
        // JMBE's INVALID enum entry is constructed with index -1, giving
        // w0 = 4*pi/38.5 and L = 8. Those values are never used, because
        // `repeat_required()` always forces a copy from the previous frame,
        // but the struct still needs something coherent.
        let frequency = (4.0 * core::f64::consts::PI / (-1.0 + 39.5)) as f32;
        let l =
            (0.9254 * (core::f64::consts::PI / frequency as f64 + 0.25).floor()).floor() as usize;
        let mut params = Self::new(DEFAULT_FUNDAMENTAL);
        params.fundamental_frequency = frequency;
        params.l = l;
        params.invalid = true;
        params
    }

    /// `MBEModelParameters.hasVoicedBands()`.
    pub(crate) fn has_voiced_bands(&self) -> bool {
        self.voicing[..=self.l].iter().any(|&voiced| voiced)
    }

    /// `MBEModelParameters.getUnvoicedBandCount()`.
    ///
    /// Java iterates the whole `L + 1` length array, so index 0 — which is
    /// never voiced — is counted; the off-by-one is part of the reference
    /// behavior and feeds Algorithm #140.
    pub(crate) fn unvoiced_band_count(&self) -> usize {
        self.voicing[..=self.l].iter().filter(|&&v| !v).count()
    }

    /// `IMBEModelParameters.requiresMuting()`.
    pub(crate) fn requires_muting(&self) -> bool {
        self.error_rate > 0.0875
    }

    /// `MBEModelParameters.isMaxFrameRepeat()`.
    pub(crate) fn is_max_frame_repeat(&self) -> bool {
        self.repeat_count >= 4
    }

    /// `IMBEModelParameters.repeatRequired()` — Algorithms #97 and #98.
    pub(crate) fn repeat_required(&self) -> bool {
        self.invalid
            || (self.error_count_coset0 >= 2
                && self.error_count_total as f32 >= (10.0 + (40.0 * self.error_rate)))
    }

    /// `IMBEModelParameters.setErrors()` — Algorithm #96.
    pub(crate) fn set_errors(
        &mut self,
        previous_error_rate: f32,
        errors_coset0: u32,
        errors_coset4: u32,
        errors_total: u32,
    ) {
        self.error_count_coset0 = errors_coset0;
        self.error_count_total = errors_total;
        self.error_count4 = errors_coset4;
        self.error_rate = (0.95 * previous_error_rate) + (0.000_365 * errors_total as f32);
    }

    /// Sets the voicing decisions, clearing anything above `L` so that the
    /// implicit zero padding of Java's shorter arrays is preserved.
    pub(crate) fn set_voicing(&mut self, voicing: &[bool; AMP_LEN]) {
        self.voicing = *voicing;
        for value in self.voicing.iter_mut().skip(self.l + 1) {
            *value = false;
        }
    }

    /// `IMBEModelParameters.copy(previous)` — used when the frame must be
    /// repeated because of an invalid fundamental or too many bit errors.
    ///
    /// Returns `true` when the voicing decisions array was shared with the
    /// previous frame. In Java the two frames alias the same `boolean[]`, so an
    /// adaptive-smoothing update writes through to the previous frame as well;
    /// the caller reproduces that by copying the result back.
    pub(crate) fn copy_from(&mut self, previous: &ModelParameters) -> bool {
        if previous.repeat_count > MAX_HEADROOM_THRESHOLD {
            // Avoid continuously repeating speech sounds — reset to defaults.
            let l = FUNDAMENTAL_L[DEFAULT_FUNDAMENTAL] as usize;
            self.fundamental_frequency = FUNDAMENTAL_FREQUENCY[DEFAULT_FUNDAMENTAL];
            self.l = l;
            self.invalid = false;
            self.voicing = [false; AMP_LEN];
            self.log2_spectral = [0.0; AMP_LEN];
            self.log2_len = l + 1;
            let mut spectral = [0.0f32; AMP_LEN];
            for value in spectral.iter_mut().take(l + 1) {
                *value = 1.0;
            }
            let (local_energy, amplitude_threshold) = (self.local_energy, self.amplitude_threshold);
            self.set_spectral_amplitudes(&spectral, l + 1, local_energy, amplitude_threshold);
            false
        } else {
            self.fundamental_frequency = previous.fundamental_frequency;
            self.l = previous.l;
            self.invalid = previous.invalid;
            self.voicing = previous.voicing;
            self.log2_spectral = previous.log2_spectral;
            self.log2_len = previous.log2_len;
            self.set_spectral_amplitudes(
                &previous.spectral,
                previous.spectral_len,
                previous.local_energy,
                previous.amplitude_threshold,
            );
            self.amplitude_threshold = previous.amplitude_threshold;
            self.local_energy = previous.local_energy;
            self.error_count_coset0 = previous.error_count_coset0;
            self.error_count_total = previous.error_count_total;
            self.error_count4 = previous.error_count4;
            self.error_rate = previous.error_rate;
            self.repeat_count = previous.repeat_count + 1;
            true
        }
    }

    /// `MBEModelParameters.setSpectralAmplitudes()` — stores the amplitudes and
    /// immediately derives the enhanced set.
    pub(crate) fn set_spectral_amplitudes(
        &mut self,
        spectral: &[f32; AMP_LEN],
        spectral_len: usize,
        previous_local_energy: f32,
        previous_amplitude_threshold: i32,
    ) {
        self.spectral = *spectral;
        self.spectral_len = spectral_len;
        self.enhance_spectral_amplitudes(previous_local_energy, previous_amplitude_threshold);
    }

    /// Chapter 8 spectral amplitude enhancement — Algorithms #105 through #111.
    fn enhance_spectral_amplitudes(
        &mut self,
        previous_local_energy: f32,
        previous_amplitude_threshold: i32,
    ) {
        let l = self.l;
        let w0 = self.fundamental_frequency;

        // Algorithms #105 and #106 - RM0 and RM1.
        let mut rm0 = 0.0f32;
        let mut rm1 = 0.0f32;
        for index in 1..=l {
            let amplitudes_squared = self.spectral[index] * self.spectral[index];
            rm0 += amplitudes_squared;
            // Java: float += (float * double) -> the product is evaluated in
            // double and the accumulation narrows back to float.
            rm1 = (rm1 as f64 + amplitudes_squared as f64 * ((w0 * index as f32) as f64).cos())
                as f32;
        }

        let mut enhanced = [0.0f32; AMP_LEN];

        if rm0 == 0.0 {
            self.enhanced = enhanced;
            return;
        }

        let rm0_squared = rm0 * rm0;
        let rm1_squared = rm1 * rm1;

        // Algorithm #107 - enhancement weights.
        let mut weights = [0.0f32; AMP_LEN];
        for index in 1..=l {
            let temp = (PI_96
                * (rm0_squared + rm1_squared
                    - (2.0 * rm0 * rm1 * (((w0 * index as f32) as f64).cos() as f32))))
                / (w0 * rm0 * (rm0_squared - rm1_squared));
            weights[index] =
                ((self.spectral[index] as f64).sqrt() * (temp as f64).powf(0.25)) as f32;
        }

        // Algorithm #108 - apply the weights.
        for index in 1..=l {
            enhanced[index] = if (8 * index) <= l {
                self.spectral[index]
            } else if weights[index] > 1.2 {
                self.spectral[index] * 1.2
            } else if weights[index] < 0.5 {
                self.spectral[index] * 0.5
            } else {
                self.spectral[index] * weights[index]
            };
        }

        // Algorithms #109 and #110 - remove the energy differential.
        let mut denominator = 0.0f32;
        for index in 1..=l {
            denominator += enhanced[index] * enhanced[index];
        }
        let y = ((rm0 / denominator) as f64).sqrt() as f32;
        for index in 1..=l {
            enhanced[index] *= y;
        }

        // Algorithm #111 - local energy.
        self.local_energy = (0.95 * previous_local_energy) + (0.05 * rm0);
        if self.local_energy < 10_000.0 {
            self.local_energy = 10_000.0;
        }

        self.enhanced = enhanced;
        self.apply_adaptive_smoothing(previous_amplitude_threshold);
    }

    /// Algorithms #112 through #116 — adaptive smoothing in the presence of
    /// uncorrected bit errors.
    fn apply_adaptive_smoothing(&mut self, previous_amplitude_threshold: i32) {
        let l = self.l;

        // Algorithm #112 - adaptive threshold.
        // Note: JMBE compares against the *double* literal 0.005 here, so the
        // float error rate is widened first; 0.0125f below is a float literal.
        if !((self.error_rate as f64) <= 0.005 && self.error_count_total <= 4) {
            let energy = (self.local_energy as f64).powf(0.375) as f32;

            let vm = if self.error_rate <= 0.0125 && self.error_count4 == 0 {
                (45.255 * energy) / ((277.26_f32 * self.error_rate) as f64).exp() as f32
            } else {
                1.414 * energy
            };

            // Algorithm #113 - voicing decisions are only smoothed when there
            // are errors to smooth over.
            for index in 1..=l {
                if self.enhanced[index] > vm {
                    self.voicing[index] = true;
                }
            }
        }

        // Algorithm #114 - amplitude measure.
        let mut am = 0.0f32;
        for index in 1..=l {
            am += self.enhanced[index];
        }

        // Algorithm #115 - amplitude threshold.
        let tm = if (self.error_rate as f64) <= 0.005 && self.error_count_total <= 6 {
            20_480
        } else {
            6000 - (300 * self.error_count_total as i32) + previous_amplitude_threshold
        };
        self.amplitude_threshold = tm;

        // Algorithm #116 - scale back if the amplitude measure runs hot.
        if am > tm as f32 {
            let scale = tm as f32 / am;
            for index in 1..=l {
                self.enhanced[index] *= scale;
            }
        }
    }
}
