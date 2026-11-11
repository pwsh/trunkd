// SPDX-License-Identifier: GPL-3.0-or-later
//! Fixed-size bit buffer with the accessors JMBE's `jmbe.binary.BinaryFrame`
//! exposes to the IMBE decoder.
//!
//! `BinaryFrame` extends `java.util.BitSet` and adds MSB-first integer
//! accessors plus in-place rotations, which the Golay(23,12,7) decoder leans
//! on. Only the operations the IMBE path actually uses are ported, and the
//! semantics (bit 0 is the MSB of byte 0, `getInt` treats index 0 as the MSB)
//! are preserved exactly.

/// A bit buffer of `N` bits, indexed MSB-first.
#[derive(Clone, Copy)]
pub(crate) struct Bits<const N: usize> {
    bits: [bool; N],
}

impl<const N: usize> Bits<N> {
    /// An all-zero buffer.
    pub(crate) fn zeroed() -> Self {
        Self { bits: [false; N] }
    }

    /// `BinaryFrame.fromBytes(data, ByteOrder.LITTLE_ENDIAN)`: each byte is
    /// unpacked MSB-first into consecutive bit indexes.
    pub(crate) fn from_bytes(data: &[u8]) -> Self {
        debug_assert_eq!(data.len() * 8, N);
        let mut out = Self::zeroed();
        for (x, byte) in data.iter().enumerate() {
            for bit in 0..8 {
                out.bits[x * 8 + bit] = (byte >> (7 - bit)) & 1 == 1;
            }
        }
        out
    }

    #[inline]
    pub(crate) fn get(&self, index: usize) -> bool {
        self.bits[index]
    }

    #[inline]
    pub(crate) fn flip(&mut self, index: usize) {
        self.bits[index] = !self.bits[index];
    }

    /// `BinaryFrame.getInt(int[] bits)` — the listed indexes are concatenated
    /// with index 0 as the MSB.
    pub(crate) fn get_int_indexes(&self, indexes: &[u8]) -> u32 {
        let mut value = 0u32;
        for &index in indexes {
            value <<= 1;
            if self.bits[index as usize] {
                value |= 1;
            }
        }
        value
    }

    /// `BinaryFrame.getInt(start, end)` for `start <= end` — bit `start` is the
    /// MSB, bit `end` the LSB (both inclusive).
    pub(crate) fn get_int_range(&self, start: usize, end: usize) -> u32 {
        let mut value = 0u32;
        for index in start..=end {
            value <<= 1;
            if self.bits[index] {
                value |= 1;
            }
        }
        value
    }

    /// `BinaryFrame.load(offset, width, value)` — writes `value` MSB-first
    /// starting at `offset`.
    pub(crate) fn load(&mut self, offset: usize, width: usize, value: u32) {
        for x in 0..width {
            self.bits[offset + x] = (value >> (width - x - 1)) & 1 == 1;
        }
    }

    /// `BinaryFrame.xor(offset, width, value)` — exclusive-or of `value`,
    /// MSB-first, into the `width` bits starting at `offset`.
    pub(crate) fn xor_value(&mut self, offset: usize, width: usize, value: u32) {
        for x in 0..width {
            if (value >> (width - x - 1)) & 1 == 1 {
                self.bits[offset + x] = !self.bits[offset + x];
            }
        }
    }

    /// `BinaryFrame.rotateLeft(startIndex, endIndex)` (inclusive range).
    pub(crate) fn rotate_left(&mut self, start: usize, end: usize) {
        let wrap = self.bits[start];
        for x in start..end {
            self.bits[x] = self.bits[x + 1];
        }
        self.bits[end] = wrap;
    }

    /// `BinaryFrame.rotateRight(places, startIndex, endIndex)`.
    pub(crate) fn rotate_right(&mut self, places: usize, start: usize, end: usize) {
        for _ in 0..places {
            let wrap = self.bits[end];
            for x in (start..end).rev() {
                self.bits[x + 1] = self.bits[x];
            }
            self.bits[start] = wrap;
        }
    }

    /// `BinaryFrame.getSubMessage(start, start + M)` — the `M` bits starting at
    /// `start`, shifted down to index 0.
    pub(crate) fn sub<const M: usize>(&self, start: usize) -> Bits<M> {
        let mut out = Bits::<M>::zeroed();
        out.bits[..M].copy_from_slice(&self.bits[start..start + M]);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_unpack_msb_first() {
        let bits = Bits::<16>::from_bytes(&[0b1000_0001, 0b0100_0000]);
        assert!(bits.get(0));
        assert!(bits.get(7));
        assert!(bits.get(9));
        assert!(!bits.get(1));
        assert_eq!(bits.get_int_range(0, 7), 0x81);
    }

    #[test]
    fn load_and_get_round_trip() {
        let mut bits = Bits::<32>::zeroed();
        bits.load(5, 11, 0x5A3);
        assert_eq!(bits.get_int_range(5, 15), 0x5A3);
    }

    #[test]
    fn rotations_are_inverses() {
        let mut bits = Bits::<23>::zeroed();
        bits.load(0, 23, 0x12345);
        let before = bits.get_int_range(0, 22);
        bits.rotate_left(0, 22);
        assert_ne!(bits.get_int_range(0, 22), before);
        bits.rotate_right(1, 0, 22);
        assert_eq!(bits.get_int_range(0, 22), before);
    }

    #[test]
    fn get_int_indexes_is_msb_first() {
        let mut bits = Bits::<16>::zeroed();
        bits.load(0, 16, 0b1010_0000_0000_0001);
        assert_eq!(bits.get_int_indexes(&[0, 2, 15]), 0b111);
        assert_eq!(bits.get_int_indexes(&[1, 3]), 0b00);
    }
}
