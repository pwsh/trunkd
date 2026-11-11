// SPDX-License-Identifier: GPL-3.0-or-later
//! Noise generators ported from `jmbe.codec.MBENoiseSequenceGenerator` and
//! `jmbe.codec.WhiteNoiseGenerator`.
//!
//! The MBE sequence generator is the spec's Algorithm #117 pseudo-random
//! sequence and is fully deterministic — its state is part of what makes a
//! decode reproducible, so it is reproduced exactly, including the quirk that
//! the very first buffer handed out is all zeros.

/// `jmbe.codec.MBENoiseSequenceGenerator` — `u(n) = (171 * u(n-1) + 11213) mod
/// 53125`, evaluated in `f32` exactly as the Java original does.
pub(crate) struct NoiseSequence {
    sample: f32,
    buffer: [f32; 256],
}

impl NoiseSequence {
    pub(crate) fn new() -> Self {
        Self {
            sample: 3147.0,
            buffer: [0.0; 256],
        }
    }

    fn next(&mut self) -> f32 {
        let next = self.sample;
        self.sample = ((171.0f32 * next) + 11213.0) % 53125.0;
        next
    }

    /// Returns the current 256-sample buffer and refills it: the tail 96
    /// samples slide to the front and indexes 96..256 are regenerated.
    pub(crate) fn next_buffer(&mut self) -> [f32; 256] {
        let copy = self.buffer;
        self.buffer.copy_within(160..256, 0);
        for x in 96..256 {
            self.buffer[x] = self.next();
        }
        copy
    }
}

/// `jmbe.codec.WhiteNoiseGenerator` — used only for muted/erased frames.
///
/// JMBE seeds this from `java.util.Random`'s no-argument constructor, so its
/// output is not reproducible across runs even in Java. This port keeps the
/// same `nextFloat()` arithmetic but pins the seed so that a decode is
/// deterministic; muted frames are audibly just a low-level hiss either way.
pub(crate) struct WhiteNoise {
    seed: u64,
}

/// `java.util.Random`'s multiplier and addend.
const JAVA_RANDOM_MULTIPLIER: u64 = 0x5DEECE66D;
const JAVA_RANDOM_ADDEND: u64 = 0xB;
const JAVA_RANDOM_MASK: u64 = (1 << 48) - 1;

impl WhiteNoise {
    pub(crate) fn new() -> Self {
        Self {
            seed: (0x1BE_u64 ^ JAVA_RANDOM_MULTIPLIER) & JAVA_RANDOM_MASK,
        }
    }

    /// `java.util.Random.next(bits)`.
    fn next_bits(&mut self, bits: u32) -> u32 {
        self.seed = self
            .seed
            .wrapping_mul(JAVA_RANDOM_MULTIPLIER)
            .wrapping_add(JAVA_RANDOM_ADDEND)
            & JAVA_RANDOM_MASK;
        (self.seed >> (48 - bits)) as u32
    }

    /// `java.util.Random.nextFloat()`.
    fn next_float(&mut self) -> f32 {
        self.next_bits(24) as f32 / (1 << 24) as f32
    }

    /// `WhiteNoiseGenerator.getSamples(160, 0.003f)` — the muted-frame filler.
    pub(crate) fn samples(&mut self, gain: f32) -> [f32; crate::SAMPLES_PER_FRAME] {
        let mut out = [0.0f32; crate::SAMPLES_PER_FRAME];
        for sample in out.iter_mut() {
            *sample = (self.next_float() * 2.0 - 1.0) * gain;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_buffer_is_zero_then_the_sequence_starts() {
        let mut generator = NoiseSequence::new();
        assert_eq!(generator.next_buffer(), [0.0f32; 256]);

        let second = generator.next_buffer();
        // Indexes 96..256 of the first refill hold u(0), u(1), ... which start
        // at the seed value 3147.
        assert_eq!(second[96], 3147.0);
        assert_eq!(second[97], (171.0f32 * 3147.0 + 11213.0) % 53125.0);
        // The slide moved the (still zero) tail of the initial buffer down.
        assert_eq!(second[0], 0.0);
    }

    #[test]
    fn sequence_stays_in_range() {
        let mut generator = NoiseSequence::new();
        for _ in 0..64 {
            for value in generator.next_buffer() {
                assert!((0.0..53125.0).contains(&value), "{value}");
            }
        }
    }
}
