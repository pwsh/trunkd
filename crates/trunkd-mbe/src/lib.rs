// SPDX-License-Identifier: GPL-3.0-or-later
//! IMBE (P25 Phase 1) voice decoder — a Rust port of [JMBE].
//!
//! # Lineage
//!
//! This crate is an explicit, structure-preserving port of Dennis Sheirer's
//! JMBE (Java Multi-Band Excitation) library, specifically its
//! `jmbe.codec.imbe` package plus the `jmbe.codec`, `jmbe.edac` and
//! `jmbe.binary` pieces the IMBE path depends on. Algorithm numbering in the
//! comments refers to the IMBE vocoder specification that JMBE annotates, so
//! the two implementations can be audited side by side.
//!
//! The port is deliberately faithful rather than idiomatic: Java `float` stays
//! `f32` and `double` stays `f64`, expression shapes are preserved where the
//! two differ (`Math.cos` returning a double silently promotes an accumulation
//! in a couple of places), the Golay/Hamming error counting keeps its exact
//! reporting quirks, and the pseudo-random noise sequence reproduces the Java
//! generator including its all-zero first buffer. Even the 256-point real FFT
//! is a port of the JTransforms kernels JMBE calls, because the unvoiced
//! excitation feeds straight into the output samples. The result is
//! sample-for-sample comparable with JMBE's own output; see
//! `tests/jmbe_oracle.rs`.
//!
//! [JMBE]: https://github.com/DSheirer/jmbe
//!
//! # Licensing and patents
//!
//! JMBE is GPL-3.0, and so is this port — `GPL-3.0-or-later`. The source is
//! the primary distribution; prebuilt trunkd binaries, when a release provides
//! them, are a convenience and are covered by the same license, with the
//! corresponding source at the tagged commit. The US patents on the IMBE
//! full-rate vocoder used by APCO Project 25 Phase 1 (Digital Voice Systems,
//! filed 1989–1997) have all expired, the last of them in 2017, so the
//! algorithm itself is unencumbered. `NOTICE.md` at the repository root lists
//! the patents, the JMBE and JTransforms attributions, and the JTransforms
//! license text.
//!
//! # Frame format
//!
//! [`ImbeDecoder::decode`] takes one 144-bit IMBE voice frame as 18 bytes,
//! MSB-first, exactly as it is carried in a P25 LDU — the raw, still
//! interleaved and still randomised bits that `trunkd-p25` slices out at the
//! vocoder boundary and reports as `P25Event::ImbeFrame`. Deinterleaving,
//! Golay(23,12,7) and Hamming(15,11,3) correction and derandomisation all
//! happen inside the decoder, as they do in JMBE's `IMBEFrame`.
//!
//! Output is 160 samples of 8 kHz mono audio per frame (20 ms), either as
//! `i16` PCM or as `f32` in -1.0..1.0.
//!
//! # State
//!
//! The decoder is stateful: model parameters, harmonic phase, the previous
//! unvoiced buffer and the noise sequence all carry across frames, so frames
//! from one call must be fed in order. [`ImbeDecoder::reset`] clears the
//! previous frame's model parameters at the start of a new call, matching
//! `IMBESynthesizer.reset()`.
//!
//! # Example
//!
//! ```
//! use trunkd_mbe::ImbeDecoder;
//!
//! let mut decoder = ImbeDecoder::new();
//! let frame = [0u8; 18];
//! let pcm = decoder.decode(&frame);
//! assert_eq!(pcm.len(), trunkd_mbe::SAMPLES_PER_FRAME);
//! ```

#![deny(missing_docs)]
// The port keeps JMBE's (and the spec's) 1-based, algorithm-numbered indexing —
// `for l in 1..=L { ... M[l] ... }` — so the Rust reads next to the Java. Most
// of those loops touch several arrays at once anyway.
#![allow(clippy::needless_range_loop)]

mod bits;
mod edac;
mod fft;
mod frame;
mod noise;
mod params;
mod synth;
mod tables;

use frame::ImbeFrame;
use params::ModelParameters;
use synth::Synthesizer;

/// Output sample rate, in hertz.
pub const SAMPLE_RATE: u32 = 8000;

/// Audio samples produced per IMBE frame (20 ms at 8 kHz).
pub const SAMPLES_PER_FRAME: usize = 160;

/// Bytes in one raw IMBE voice frame (144 bits).
pub const FRAME_BYTES: usize = 18;

/// Stateful IMBE voice decoder.
///
/// Feed frames in order with [`decode`](Self::decode) or
/// [`decode_f32`](Self::decode_f32); call [`reset`](Self::reset) between calls.
pub struct ImbeDecoder {
    synthesizer: Synthesizer,
    previous: ModelParameters,
    last_frame_errors: u32,
    last_frame_muted: bool,
}

impl ImbeDecoder {
    /// Creates a decoder primed with JMBE's default (start of sequence) frame.
    pub fn new() -> Self {
        Self {
            synthesizer: Synthesizer::new(),
            previous: ModelParameters::default_frame(),
            last_frame_errors: 0,
            last_frame_muted: false,
        }
    }

    /// Forgets the previous frame's model parameters.
    ///
    /// This mirrors `IMBESynthesizer.reset()`, which replaces the model
    /// parameters only: the harmonic phase accumulators, the previous unvoiced
    /// buffer and the noise sequence deliberately keep running, so a reset does
    /// not introduce a discontinuity in the noise excitation.
    pub fn reset(&mut self) {
        self.previous = ModelParameters::default_frame();
        self.last_frame_errors = 0;
        self.last_frame_muted = false;
    }

    /// Decodes one 144-bit IMBE frame into 20 ms of `f32` PCM in -1.0..1.0.
    pub fn decode_f32(&mut self, frame: &[u8; FRAME_BYTES]) -> [f32; SAMPLES_PER_FRAME] {
        let imbe = ImbeFrame::new(frame);
        let (mut parameters, shared_voicing) = imbe.model_parameters(&self.previous);

        // JMBE aliases the voicing decisions array across the two frames when a
        // repeat copies it, so adaptive smoothing during the copy writes
        // through to the previous frame too. Reproduce that before synthesis
        // reads the previous frame's decisions.
        if shared_voicing {
            self.previous.voicing = parameters.voicing;
        }

        self.last_frame_errors = imbe.error_count_total();
        self.last_frame_muted = parameters.is_max_frame_repeat() || parameters.requires_muting();

        let audio = if self.last_frame_muted {
            self.synthesizer.white_noise()
        } else {
            self.synthesizer.voice(&parameters, &self.previous)
        };

        core::mem::swap(&mut self.previous, &mut parameters);

        audio
    }

    /// Decodes one 144-bit IMBE frame into 20 ms of 16-bit PCM.
    ///
    /// Scaling matches JMBE's callers: `(short)(sample * Short.MAX_VALUE)`,
    /// i.e. truncation toward zero. The synthesizer clips to 0.95 full scale
    /// first, so this never saturates.
    pub fn decode(&mut self, frame: &[u8; FRAME_BYTES]) -> [i16; SAMPLES_PER_FRAME] {
        let audio = self.decode_f32(frame);
        let mut pcm = [0i16; SAMPLES_PER_FRAME];
        for (out, sample) in pcm.iter_mut().zip(audio.iter()) {
            *out = (sample * i16::MAX as f32) as i16;
        }
        pcm
    }

    /// Bit errors detected and corrected in the most recently decoded frame,
    /// summed over its seven coset words (JMBE's `mErrorCountTotal`).
    pub fn last_frame_errors(&self) -> u32 {
        self.last_frame_errors
    }

    /// Whether the most recently decoded frame was muted — replaced with low
    /// level noise because the running error rate was too high or the frame had
    /// been repeated too many times.
    pub fn last_frame_muted(&self) -> bool {
        self.last_frame_muted
    }
}

impl Default for ImbeDecoder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_a_frame_without_panicking() {
        let mut decoder = ImbeDecoder::new();
        let pcm = decoder.decode(&[0u8; 18]);
        assert_eq!(pcm.len(), SAMPLES_PER_FRAME);
    }

    #[test]
    fn reset_restores_the_default_frame() {
        let mut decoder = ImbeDecoder::new();
        for _ in 0..8 {
            decoder.decode(&[0xAA; 18]);
        }
        decoder.reset();
        assert_eq!(decoder.last_frame_errors(), 0);
        assert!(!decoder.last_frame_muted());
    }

    /// Every reachable value of L must decode without an out-of-range index;
    /// drive the decoder with pseudo-random frames to shake the tables out.
    #[test]
    fn random_frames_decode_without_panicking() {
        let mut decoder = ImbeDecoder::new();
        let mut seed = 0x1234_5678u32;
        for _ in 0..2000 {
            let mut frame = [0u8; 18];
            for byte in frame.iter_mut() {
                seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                *byte = (seed >> 16) as u8;
            }
            decoder.decode(&frame);
        }
    }
}
