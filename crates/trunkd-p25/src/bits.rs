// SPDX-License-Identifier: GPL-3.0-or-later
//! Bit-level primitives: dibits and fixed-capacity MSB-first bit buffers.
//!
//! Conventions (spec §1): MSB-first everywhere; bit index 0 is the first
//! received / most significant bit. Dibit N occupies bits 2N (MSB) and 2N+1.

/// A 4-level symbol: two bits, `value = bit1*2 + bit2`.
///
/// Phase mapping (π/4-DQPSK differential phase per symbol):
/// D01 → +3π/4, D00 → +π/4, D10 → −π/4, D11 → −3π/4.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Dibit {
    /// bits 00, value 0, +π/4
    D00Plus1 = 0,
    /// bits 01, value 1, +3π/4
    D01Plus3 = 1,
    /// bits 10, value 2, −π/4
    D10Minus1 = 2,
    /// bits 11, value 3, −3π/4
    D11Minus3 = 3,
}

impl Dibit {
    /// Numeric value 0..=3 (bit1*2 + bit2).
    #[inline]
    pub fn value(self) -> u8 {
        self as u8
    }

    /// Construct from a value 0..=3 (upper bits ignored).
    #[inline]
    pub fn from_value(v: u8) -> Self {
        match v & 3 {
            0 => Dibit::D00Plus1,
            1 => Dibit::D01Plus3,
            2 => Dibit::D10Minus1,
            _ => Dibit::D11Minus3,
        }
    }

    /// First (most significant) bit.
    #[inline]
    pub fn bit1(self) -> bool {
        (self as u8) & 2 != 0
    }

    /// Second (least significant) bit.
    #[inline]
    pub fn bit2(self) -> bool {
        (self as u8) & 1 != 0
    }

    /// Ideal differential phase for this dibit, in radians.
    #[inline]
    pub fn ideal_phase(self) -> f32 {
        use std::f32::consts::PI;
        match self {
            Dibit::D00Plus1 => PI / 4.0,
            Dibit::D01Plus3 => 3.0 * PI / 4.0,
            Dibit::D10Minus1 => -PI / 4.0,
            Dibit::D11Minus3 => -3.0 * PI / 4.0,
        }
    }

    /// Bit-error distance to another dibit: 0 if equal, 2 if xor == 3, else 1.
    #[inline]
    pub fn bit_error_distance(self, other: Dibit) -> u8 {
        let x = (self as u8) ^ (other as u8);
        match x {
            0 => 0,
            3 => 2,
            _ => 1,
        }
    }

    /// Slice a soft symbol (differential phase, radians) into a dibit.
    ///
    /// Decision boundaries 0 and ±π/2 (spec §4.1).
    #[inline]
    pub fn slice(phase: f32) -> Dibit {
        use std::f32::consts::FRAC_PI_2;
        if phase > 0.0 {
            if phase > FRAC_PI_2 {
                Dibit::D01Plus3
            } else {
                Dibit::D00Plus1
            }
        } else if phase < -FRAC_PI_2 {
            Dibit::D11Minus3
        } else {
            Dibit::D10Minus1
        }
    }
}

/// Read a big-endian (MSB-first) bit field `[start, start+width)` from a byte
/// slice (spec §1: bit 0 is the MSB of byte 0; bit `start` becomes the MSB of
/// the result). `width` ≤ 64.
///
/// This is the single MSB-first field extractor used across the crate:
/// [`BitBuf::get_u64`] (and therefore the TSBK/PDU parsers in
/// [`crate::message`]) and the voice-payload parsers in [`crate::payload`] all
/// funnel through it, so a field read is bit-identical wherever it happens.
#[inline]
pub fn bits_msb_first(bytes: &[u8], start: usize, width: usize) -> u64 {
    debug_assert!(width <= 64);
    let mut v = 0u64;
    for i in start..start + width {
        let bit = (bytes[i / 8] >> (7 - (i % 8))) & 1;
        v = (v << 1) | u64::from(bit);
    }
    v
}

/// Maximum message size we ever assemble (PLACE_HOLDER = 1572 bits) rounded up.
pub const MAX_MESSAGE_BITS: usize = 1600;

/// A fixed-capacity, heap-free, MSB-first bit buffer.
///
/// Bit `i` is stored in byte `i / 8`, mask `0x80 >> (i % 8)` — so byte 0's MSB
/// is message bit 0, matching the spec's MSB-first convention.
#[derive(Clone)]
pub struct BitBuf {
    bytes: [u8; MAX_MESSAGE_BITS / 8],
    len: usize,
}

impl Default for BitBuf {
    fn default() -> Self {
        Self::new()
    }
}

impl BitBuf {
    /// An empty buffer.
    pub const fn new() -> Self {
        BitBuf {
            bytes: [0; MAX_MESSAGE_BITS / 8],
            len: 0,
        }
    }

    /// Number of bits currently held.
    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    /// True when no bits are held.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Clear to zero length (and zero content).
    pub fn clear(&mut self) {
        self.bytes = [0; MAX_MESSAGE_BITS / 8];
        self.len = 0;
    }

    /// Read bit `i` (must be < capacity; bits past `len` read as written/zero).
    #[inline]
    pub fn get(&self, i: usize) -> bool {
        debug_assert!(i < MAX_MESSAGE_BITS);
        self.bytes[i / 8] & (0x80 >> (i % 8)) != 0
    }

    /// Write bit `i` without changing `len`.
    #[inline]
    pub fn set(&mut self, i: usize, v: bool) {
        debug_assert!(i < MAX_MESSAGE_BITS);
        let mask = 0x80 >> (i % 8);
        if v {
            self.bytes[i / 8] |= mask;
        } else {
            self.bytes[i / 8] &= !mask;
        }
    }

    /// Flip bit `i` in place.
    #[inline]
    pub fn flip(&mut self, i: usize) {
        self.bytes[i / 8] ^= 0x80 >> (i % 8);
    }

    /// Append one bit.
    #[inline]
    pub fn push(&mut self, v: bool) {
        let i = self.len;
        self.len += 1;
        self.set(i, v);
    }

    /// Append a dibit (bit1 then bit2).
    #[inline]
    pub fn push_dibit(&mut self, d: Dibit) {
        self.push(d.bit1());
        self.push(d.bit2());
    }

    /// Extract bits `[start, start+width)` as a u64, MSB-first (bit `start` is
    /// the MSB of the result). `width` ≤ 64.
    #[inline]
    pub fn get_u64(&self, start: usize, width: usize) -> u64 {
        bits_msb_first(&self.bytes, start, width)
    }

    /// Extract bits `[start, start+width)` as a u32, MSB-first. `width` ≤ 32.
    #[inline]
    pub fn get_u32(&self, start: usize, width: usize) -> u32 {
        self.get_u64(start, width) as u32
    }

    /// Write `width` bits of `v` (MSB-first) at `[start, start+width)`.
    pub fn set_u64(&mut self, start: usize, width: usize, v: u64) {
        debug_assert!(width <= 64);
        for k in 0..width {
            self.set(start + k, (v >> (width - 1 - k)) & 1 != 0);
        }
    }

    /// Copy bits `[start, start+n)` into `out` bytes, MSB-first, zero-padding
    /// the final byte. `out` must hold at least `(n+7)/8` bytes.
    pub fn copy_bits_to_bytes(&self, start: usize, n: usize, out: &mut [u8]) {
        for b in out.iter_mut().take(n.div_ceil(8)) {
            *b = 0;
        }
        for k in 0..n {
            if self.get(start + k) {
                out[k / 8] |= 0x80 >> (k % 8);
            }
        }
    }
}

impl std::fmt::Debug for BitBuf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "BitBuf[{} bits]", self.len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dibit_distance() {
        assert_eq!(Dibit::D00Plus1.bit_error_distance(Dibit::D00Plus1), 0);
        assert_eq!(Dibit::D00Plus1.bit_error_distance(Dibit::D11Minus3), 2);
        assert_eq!(Dibit::D00Plus1.bit_error_distance(Dibit::D01Plus3), 1);
        assert_eq!(Dibit::D10Minus1.bit_error_distance(Dibit::D01Plus3), 2);
    }

    #[test]
    fn slicing_boundaries() {
        assert_eq!(Dibit::slice(0.8), Dibit::D00Plus1);
        assert_eq!(Dibit::slice(2.0), Dibit::D01Plus3);
        assert_eq!(Dibit::slice(-0.8), Dibit::D10Minus1);
        assert_eq!(Dibit::slice(-2.0), Dibit::D11Minus3);
        assert_eq!(Dibit::slice(0.0), Dibit::D10Minus1); // φ <= 0 branch
    }

    #[test]
    fn bitbuf_fields() {
        let mut b = BitBuf::new();
        for _ in 0..64 {
            b.push(false);
        }
        b.set_u64(4, 12, 0xABC);
        assert_eq!(b.get_u32(4, 12), 0xABC);
        assert_eq!(b.get_u32(4, 4), 0xA);
        b.push_dibit(Dibit::D10Minus1);
        assert!(b.get(64));
        assert!(!b.get(65));
    }

    #[test]
    fn msb_first_extractor_matches_bitbuf() {
        // The free function and BitBuf::get_u64 must agree bit-for-bit at every
        // (start, width) — they are the same reader used by message.rs and
        // payload.rs.
        let bytes: [u8; 8] = [0x9C, 0x37, 0xF0, 0x0F, 0xA5, 0x5A, 0xDE, 0xAD];
        let mut b = BitBuf::new();
        for byte in bytes {
            for k in (0..8).rev() {
                b.push((byte >> k) & 1 != 0);
            }
        }
        for start in 0..64 {
            for width in 0..=(64 - start) {
                assert_eq!(
                    bits_msb_first(&bytes, start, width),
                    b.get_u64(start, width),
                    "mismatch at start={start} width={width}"
                );
            }
        }
        // Spot-check the MSB-first convention itself.
        assert_eq!(bits_msb_first(&bytes, 0, 8), 0x9C);
        assert_eq!(bits_msb_first(&bytes, 4, 8), 0xC3);
        assert_eq!(bits_msb_first(&bytes, 0, 0), 0);
    }
}
