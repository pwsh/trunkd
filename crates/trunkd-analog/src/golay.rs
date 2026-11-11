// SPDX-License-Identifier: GPL-3.0-or-later
//! Binary Golay(23,12) code as used by DCS (Digital-Coded Squelch).
//!
//! A DCS codeword is 23 bits: 12 data bits in the low positions (bit 0 first
//! on air) followed by 11 parity bits. The code is cyclic with generator
//! polynomial `g(x) = x^11 + x^10 + x^6 + x^5 + x^4 + x^2 + 1` (`0xC75`); the
//! parity is the remainder of `data(x) · x^11` divided by `g(x)`.
//!
//! The encoder serves the DCS tables ([`crate::tone`]) and the synthetic
//! signal generator; [`syndrome`] and [`decode`] serve the DCS detector
//! ([`crate::dcs`]). The code is perfect: every 23-bit word is within Hamming
//! distance 3 of exactly one codeword, so each of the 2048 syndromes names one
//! error pattern of weight ≤ 3 ([`error_pattern`]). The DCS detector allows at
//! most one corrected bit; [`decode`] takes the budget as a parameter.
//!
//! This is not the P25 crate's Golay: that one stores message bit 0 as the
//! most significant codeword bit and lives behind the P25 framing types; here
//! bit 0 is the least significant bit, matching the order DCS sends on air.

/// Generator polynomial of the cyclic Golay(23,12) code, bit `i` = coefficient
/// of `x^i` (degree 11).
pub const GENERATOR: u32 = 0xC75;

/// Mask of the 23 codeword bits.
pub const CODEWORD_MASK: u32 = (1 << 23) - 1;

/// Mask of the 12 data bits.
pub const DATA_MASK: u32 = (1 << 12) - 1;

/// The 11 parity bits for `data` (only its low 12 bits are used).
pub const fn parity(data: u16) -> u16 {
    let mut r = ((data as u32) & DATA_MASK) << 11;
    let mut bit = 22;
    while bit >= 11 {
        if r & (1 << bit) != 0 {
            r ^= GENERATOR << (bit - 11);
        }
        bit -= 1;
    }
    r as u16
}

/// Systematic codeword for `data`: data in bits 0..=11, parity in bits
/// 12..=22. Only the low 12 bits of `data` are used.
pub const fn encode(data: u16) -> u32 {
    ((parity(data) as u32) << 12) | ((data as u32) & DATA_MASK)
}

/// Rotate a 23-bit word right by `k` bits (the code is cyclic, so every
/// rotation of a codeword is a codeword).
pub const fn rotate_right(word: u32, k: u32) -> u32 {
    let k = k % 23;
    let w = word & CODEWORD_MASK;
    if k == 0 {
        w
    } else {
        ((w >> k) | (w << (23 - k))) & CODEWORD_MASK
    }
}

/// Remainder of the 23-bit `word` (bit `i` = coefficient of `x^i`) divided by
/// [`GENERATOR`]: 0 for a codeword. Bits above bit 22 are ignored.
pub const fn syndrome(word: u32) -> u16 {
    let mut r = word & CODEWORD_MASK;
    let mut bit = 22;
    while bit >= 11 {
        if r & (1 << bit) != 0 {
            r ^= GENERATOR << (bit - 11);
        }
        bit -= 1;
    }
    r as u16
}

/// Error pattern of weight ≤ 3 for every syndrome, indexed by syndrome.
static ERROR_PATTERNS: [u32; 2048] = build_error_patterns();

const fn build_error_patterns() -> [u32; 2048] {
    let mut t = [0u32; 2048];
    let mut a = 0;
    while a < 23 {
        let ea = 1u32 << a;
        t[syndrome(ea) as usize] = ea;
        let mut b = a + 1;
        while b < 23 {
            let eb = ea | 1 << b;
            t[syndrome(eb) as usize] = eb;
            let mut c = b + 1;
            while c < 23 {
                let ec = eb | 1 << c;
                t[syndrome(ec) as usize] = ec;
                c += 1;
            }
            b += 1;
        }
        a += 1;
    }
    t
}

/// The unique error pattern of weight ≤ 3 whose syndrome is `syndrome`
/// (0 for syndrome 0). Only the low 11 bits of `syndrome` are used.
pub fn error_pattern(syndrome: u16) -> u32 {
    ERROR_PATTERNS[usize::from(syndrome & 0x7FF)]
}

/// A received word decoded to the nearest codeword.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Decoded {
    /// The corrected 23-bit codeword.
    pub codeword: u32,
    /// Its 12 data bits (bits 0..=11 of the codeword).
    pub data: u16,
    /// Bits corrected (0..=3).
    pub errors: u32,
}

/// Decode a received 23-bit `word`, correcting at most `max_errors` bit
/// errors (values above 3 act as 3, the code's limit). `None` when the
/// nearest codeword is further than `max_errors` away. Bits above bit 22 are
/// ignored.
pub fn decode(word: u32, max_errors: u32) -> Option<Decoded> {
    let e = error_pattern(syndrome(word));
    let errors = e.count_ones();
    if errors > max_errors {
        return None;
    }
    let codeword = (word & CODEWORD_MASK) ^ e;
    Some(Decoded {
        codeword,
        data: (codeword & DATA_MASK) as u16,
        errors,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dcs_023_reference_word() {
        // Data = fixed bits `100` above octal 023; the DCS reference word for
        // D023N is 0x763813.
        assert_eq!(encode(0x800 | 0o023), 0x76_3813);
    }

    #[test]
    fn every_codeword_is_divisible_by_the_generator() {
        // Polynomial remainder of a full codeword must be zero.
        fn remainder(word: u32) -> u32 {
            let mut r = word;
            let mut bit = 22;
            while bit >= 11 {
                if r & (1 << bit) != 0 {
                    r ^= GENERATOR << (bit - 11);
                }
                bit -= 1;
            }
            r
        }
        for data in 0..4096u16 {
            let w = encode(data);
            assert_eq!(w & DATA_MASK, data as u32);
            assert_eq!(w & !CODEWORD_MASK, 0);
            // Cyclic code: all rotations are codewords too.
            for k in 0..23 {
                assert_eq!(remainder(rotate_right(w, k)), 0, "data {data:#x} rot {k}");
            }
        }
    }

    #[test]
    fn every_syndrome_has_one_pattern_of_weight_at_most_three() {
        assert_eq!(error_pattern(0), 0);
        let mut seen = std::collections::HashSet::new();
        for s in 1..2048u16 {
            let e = error_pattern(s);
            assert!((1..=3).contains(&e.count_ones()), "syndrome {s:#x}");
            assert_eq!(syndrome(e), s);
            assert!(seen.insert(e));
        }
        assert_eq!(syndrome(0x76_3813), 0);
        assert_eq!(syndrome(0xFF80_0000), 0, "bits above 22 are ignored");
    }

    #[test]
    fn minimum_distance_is_seven() {
        let min = (1..4096u16).map(|d| encode(d).count_ones()).min().unwrap();
        assert_eq!(min, 7);
    }
}
