// SPDX-License-Identifier: GPL-3.0-or-later
//! P25 Low-Speed Data (LSD) error-control code.
//!
//! Each LDU carries two LSD octets. Each octet is one codeword of a
//! **(16, 8, 5) shortened cyclic code**: 8 data bits followed by 8 parity
//! bits, generator polynomial **g(x) = x⁸ + x⁵ + x⁴ + x³ + 1** (reduction
//! byte `0x39`). The reference decoder (sdrtrunk) does not check this code —
//! it extracts the data bits raw. This module closes that gap.
//!
//! The polynomial was recovered and cross-checked against the MMDVMHost parity
//! table (`P25LowSpeedData.cpp`): the table generated from g(x) here reproduces
//! that table exactly (verified in tests, including its first 16 and last
//! entries), so the encoding is bit-compatible with on-air LSD.
//!
//! With minimum distance 5 the code corrects up to 2 bit errors per octet
//! (bounded-distance nearest-codeword decoding).

/// Low byte of the generator polynomial x⁸+x⁵+x⁴+x³+1: `x⁸ mod g(x)`.
const GEN_REDUCTION: u16 = 0x39;

/// Guaranteed error-correction capability: floor((d-1)/2) = 2 for d = 5.
const MAX_CORRECTABLE: u32 = 2;

/// Compute the 8 parity bits for an 8-bit data octet (systematic cyclic
/// encoding: parity = (data · x⁸) mod g(x)).
pub fn parity(data: u8) -> u8 {
    // x^(8+i) mod g(x) for each data bit i, accumulated by XOR. Start at
    // x⁸ mod g = GEN_REDUCTION for bit 0 and shift-reduce upward.
    let mut basis = GEN_REDUCTION;
    let mut acc = 0u16;
    for i in 0..8 {
        if data & (1 << i) != 0 {
            acc ^= basis;
        }
        basis <<= 1;
        if basis & 0x100 != 0 {
            basis = (basis ^ 0x100) ^ GEN_REDUCTION;
        }
    }
    acc as u8
}

/// The full 16-bit systematic codeword for a data octet: `data<<8 | parity`.
pub fn encode(data: u8) -> u16 {
    (u16::from(data) << 8) | u16::from(parity(data))
}

/// Result of decoding one LSD octet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LsdOctet {
    /// The 8 data bits: corrected when `valid`, raw as received otherwise.
    pub data: u8,
    /// Number of bit errors actually corrected (0 when `!valid` — nothing was
    /// corrected in that case).
    pub errors: u32,
    /// Whether the received word was within the code's correction capability.
    pub valid: bool,
}

/// Decode one received 16-bit LSD codeword by nearest-codeword search over the
/// 256 valid words.
///
/// Within the correction radius (distance ≤ 2 for d = 5) the nearest codeword
/// is the unique correct one: `data` is the corrected octet and `errors` is
/// the number of bits flipped. Beyond it, the nearest codeword is arbitrary —
/// several codewords may be equidistant and none is more likely than the
/// received word itself — so nothing is corrected: `data` is the **raw**
/// received data byte, `errors` is 0, and `valid` is false. Consumers must
/// check `valid` before using `data`.
pub fn decode(received: u16) -> LsdOctet {
    let raw = (received >> 8) as u8;
    let mut best_data = raw;
    let mut best_dist = u32::MAX;
    for d in 0u16..256 {
        let dist = (encode(d as u8) ^ received).count_ones();
        if dist < best_dist {
            best_dist = dist;
            best_data = d as u8;
            if dist == 0 {
                break;
            }
        }
    }
    if best_dist <= MAX_CORRECTABLE {
        LsdOctet {
            data: best_data,
            errors: best_dist,
            valid: true,
        }
    } else {
        LsdOctet {
            data: raw,
            errors: 0,
            valid: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The parity table generated from g(x) must match MMDVMHost's CCS_PARITY
    /// table (P25LowSpeedData.cpp) — the authoritative on-air reference.
    #[test]
    fn parity_matches_reference_table() {
        // First 16 entries of CCS_PARITY (indices 0..16).
        let head = [
            0x00u8, 0x39, 0x72, 0x4B, 0xE4, 0xDD, 0x96, 0xAF, 0xF1, 0xC8, 0x83, 0xBA, 0x15, 0x2C,
            0x67, 0x5E,
        ];
        for (d, &p) in head.iter().enumerate() {
            assert_eq!(parity(d as u8), p, "parity({d:#04x})");
        }
        // Next-row anchors (indices 16..18) and the final entry (255).
        assert_eq!(parity(16), 0xDB);
        assert_eq!(parity(17), 0xE2);
        assert_eq!(parity(0xFF), 0x63);
    }

    #[test]
    fn parity_is_linear() {
        // Cyclic-code parity is linear over the data bits.
        for a in 0u16..256 {
            for b in [0u8, 1, 2, 4, 8, 0x55, 0xAA, 0xFF] {
                assert_eq!(parity(a as u8 ^ b), parity(a as u8) ^ parity(b));
            }
        }
    }

    #[test]
    fn clean_codeword_decodes_zero_errors() {
        for d in 0u16..256 {
            let r = decode(encode(d as u8));
            assert_eq!(r.data, d as u8);
            assert_eq!(r.errors, 0);
            assert!(r.valid);
        }
    }

    #[test]
    fn corrects_one_and_two_bit_errors() {
        let data = 0xA5u8;
        let cw = encode(data);
        // Every single-bit error is corrected.
        for i in 0..16 {
            let r = decode(cw ^ (1 << i));
            assert_eq!(r.data, data, "1-bit at {i}");
            assert_eq!(r.errors, 1);
            assert!(r.valid);
        }
        // Every two-bit error is corrected (d=5 => t=2).
        for i in 0..16 {
            for j in (i + 1)..16 {
                let r = decode(cw ^ (1 << i) ^ (1 << j));
                assert_eq!(r.data, data, "2-bit at {i},{j}");
                assert_eq!(r.errors, 2);
                assert!(r.valid);
            }
        }
    }

    /// Brute-force minimum Hamming distance from `r` to any codeword.
    fn min_dist(r: u16) -> u32 {
        (0u16..256)
            .map(|d| (encode(d as u8) ^ r).count_ones())
            .min()
            .unwrap()
    }

    #[test]
    fn valid_flag_and_errors_match_true_distance() {
        // Across the whole 16-bit received space, `valid` must be exactly
        // (min distance <= 2). For valid words `errors` is the true distance
        // and `data` the corrected octet; for invalid words nothing is
        // corrected, so `errors` is 0 and `data` is the raw received byte.
        let mut saw_uncorrectable = false;
        for r in 0u32..=0xFFFF {
            let r = r as u16;
            let md = min_dist(r);
            let out = decode(r);
            assert_eq!(out.valid, md <= MAX_CORRECTABLE, "valid for {r:#06x}");
            if out.valid {
                assert_eq!(out.errors, md, "distance for {r:#06x}");
                assert_eq!(decode(encode(out.data)).data, out.data);
                assert_eq!((encode(out.data) ^ r).count_ones(), md);
            } else {
                assert_eq!(out.errors, 0, "uncorrectable words correct nothing");
                assert_eq!(out.data, (r >> 8) as u8, "raw data byte for {r:#06x}");
                saw_uncorrectable = true;
            }
        }
        assert!(saw_uncorrectable, "code should have uncorrectable cosets");
    }

    #[test]
    fn uncorrectable_word_is_passed_through_raw() {
        // Pick a received word genuinely outside every correction radius.
        let recv = (0u32..=0xFFFF)
            .map(|r| r as u16)
            .find(|&r| min_dist(r) > MAX_CORRECTABLE)
            .expect("an uncorrectable word exists");
        let out = decode(recv);
        assert!(!out.valid);
        assert_eq!(out.errors, 0);
        assert_eq!(out.data, (recv >> 8) as u8);
    }
}
