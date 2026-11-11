// SPDX-License-Identifier: GPL-3.0-or-later
//! 196-bit data-block deinterleaver (spec §9).
//!
//! Applied to every TSBK block, PDU header and PDU data block before trellis
//! decoding. Mapping semantics: for each set source bit `i`, set destination
//! bit `PATTERN[i]` (source index → destination index). HDU/LDU do not use
//! this interleaver.

use crate::bits::BitBuf;

/// Receive-direction deinterleave pattern (the one applied to the received
/// 196 bits).
pub const DATA_DEINTERLEAVE: [u8; 196] = [
    0, 1, 2, 3, 16, 17, 18, 19, 32, 33, 34, 35, 48, 49, 50, 51, 64, 65, 66, 67, 80, 81, 82, 83, 96,
    97, 98, 99, 112, 113, 114, 115, 128, 129, 130, 131, 144, 145, 146, 147, 160, 161, 162, 163,
    176, 177, 178, 179, 192, 193, 194, 195, 4, 5, 6, 7, 20, 21, 22, 23, 36, 37, 38, 39, 52, 53, 54,
    55, 68, 69, 70, 71, 84, 85, 86, 87, 100, 101, 102, 103, 116, 117, 118, 119, 132, 133, 134, 135,
    148, 149, 150, 151, 164, 165, 166, 167, 180, 181, 182, 183, 8, 9, 10, 11, 24, 25, 26, 27, 40,
    41, 42, 43, 56, 57, 58, 59, 72, 73, 74, 75, 88, 89, 90, 91, 104, 105, 106, 107, 120, 121, 122,
    123, 136, 137, 138, 139, 152, 153, 154, 155, 168, 169, 170, 171, 184, 185, 186, 187, 12, 13,
    14, 15, 28, 29, 30, 31, 44, 45, 46, 47, 60, 61, 62, 63, 76, 77, 78, 79, 92, 93, 94, 95, 108,
    109, 110, 111, 124, 125, 126, 127, 140, 141, 142, 143, 156, 157, 158, 159, 172, 173, 174, 175,
    188, 189, 190, 191,
];

/// Deinterleave 196 bits at `[start, start+196)` of `src` into a fresh
/// 196-bit buffer.
pub fn deinterleave(src: &BitBuf, start: usize) -> BitBuf {
    let mut out = BitBuf::new();
    for _ in 0..196 {
        out.push(false);
    }
    for (i, &dst) in DATA_DEINTERLEAVE.iter().enumerate() {
        if src.get(start + i) {
            out.set(dst as usize, true);
        }
    }
    out
}

/// Interleave (transmit direction — the permutation's inverse). Test helper.
pub fn interleave(src: &BitBuf) -> BitBuf {
    let mut out = BitBuf::new();
    for _ in 0..196 {
        out.push(false);
    }
    for (i, &dst) in DATA_DEINTERLEAVE.iter().enumerate() {
        if src.get(dst as usize) {
            out.set(i, true);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pattern_is_a_permutation() {
        let mut seen = [false; 196];
        for &d in DATA_DEINTERLEAVE.iter() {
            assert!(!seen[d as usize], "duplicate destination {d}");
            seen[d as usize] = true;
        }
        assert!(seen.iter().all(|&s| s));
    }

    #[test]
    fn deinterleave_inverts_interleave() {
        let mut b = BitBuf::new();
        let mut s = 99u64;
        for _ in 0..196 {
            s = s.wrapping_mul(6364136223846793005).wrapping_add(7);
            b.push((s >> 62) & 1 != 0);
        }
        let tx = interleave(&b);
        let rx = deinterleave(&tx, 0);
        for i in 0..196 {
            assert_eq!(rx.get(i), b.get(i), "bit {i}");
        }
    }
}
