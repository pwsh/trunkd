// SPDX-License-Identifier: GPL-3.0-or-later
//! Hamming(10,6,3) and Hamming(15,11,3) (spec §8.3).
//!
//! Word representation: MSB-first — message bit 0 is the most significant bit
//! of the `u16` word (bit i at `w >> (N-1-i)` for an N-bit word).

/// Per-data-bit checksums for Hamming(10,6,3).
const H10_CHECKSUMS: [u16; 6] = [0xE, 0xD, 0xB, 0x7, 0x3, 0xC];

/// Syndrome → relative bit to flip for Hamming(10,6,3); `NONE` = clean,
/// `BAD` = uncorrectable. Value is the message bit index (0..=9).
const H10_SYNDROME_FLIP: [i8; 16] = [
    -1, // 0: none
    9,  // 1: parity 1
    8,  // 2: parity 2
    4,  // 3: data 2
    7,  // 4: parity 4
    -2, // 5: uncorrectable
    -2, // 6: uncorrectable
    3,  // 7: data 3
    6,  // 8: parity 8
    -2, // 9: uncorrectable
    -2, // 10: uncorrectable
    2,  // 11: data 4
    5,  // 12: data 1
    1,  // 13: data 5
    0,  // 14: data 6
    -2, // 15: uncorrectable
];

/// Hamming(10,6,3): correct the 10-bit word in place.
/// Returns `Some(corrected_bit_count)` (0 or 1) or `None` if uncorrectable.
pub fn hamming10_check_and_correct(word: &mut u16) -> Option<u32> {
    let w = *word & 0x3FF;
    let mut calc: u16 = 0;
    for (i, &c) in H10_CHECKSUMS.iter().enumerate() {
        if w >> (9 - i) & 1 != 0 {
            calc ^= c;
        }
    }
    let syndrome = (w & 0xF) ^ calc;
    match H10_SYNDROME_FLIP[syndrome as usize] {
        -1 => Some(0),
        -2 => None,
        bit => {
            *word = w ^ (1 << (9 - bit as u16));
            Some(1)
        }
    }
}

/// Per-data-bit checksums for Hamming(15,11,3).
const H15_CHECKSUMS: [u16; 15] = [
    0x9, 0xD, 0xF, 0xE, 0x7, 0xA, 0x5, 0xB, 0xC, 0x6, 0x3, 0x8, 0x4, 0x2, 0x1,
];

/// Syndrome → message bit to flip for Hamming(15,11,3); −1 = clean.
const H15_SYNDROME_FLIP: [i8; 16] = [-1, 14, 13, 10, 12, 6, 9, 4, 11, 0, 5, 7, 8, 1, 3, 2];

/// Hamming(15,11,3): correct the 15-bit word in place.
/// Returns the corrected bit count (0 or 1); every syndrome is correctable.
pub fn hamming15_check_and_correct(word: &mut u16) -> u32 {
    let w = *word & 0x7FFF;
    let mut calc: u16 = 0;
    for (i, &c) in H15_CHECKSUMS.iter().enumerate().take(11) {
        if w >> (14 - i) & 1 != 0 {
            calc ^= c;
        }
    }
    let syndrome = (w & 0xF) ^ calc;
    match H15_SYNDROME_FLIP[syndrome as usize] {
        -1 => 0,
        bit => {
            *word = w ^ (1 << (14 - bit as u16));
            1
        }
    }
}

/// Encode 6 data bits into a Hamming(10,6) codeword (test helper).
pub fn hamming10_encode(data: u8) -> u16 {
    let mut check: u16 = 0;
    for (i, &c) in H10_CHECKSUMS.iter().enumerate() {
        if data >> (5 - i) & 1 != 0 {
            check ^= c;
        }
    }
    (u16::from(data & 0x3F) << 4) | check
}

/// Encode 11 data bits into a Hamming(15,11) codeword (test helper).
pub fn hamming15_encode(data: u16) -> u16 {
    let mut check: u16 = 0;
    for (i, &c) in H15_CHECKSUMS.iter().enumerate().take(11) {
        if data >> (10 - i) & 1 != 0 {
            check ^= c;
        }
    }
    ((data & 0x7FF) << 4) | check
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hamming10_all_single_errors() {
        for data in 0..64u8 {
            let cw = hamming10_encode(data);
            let mut clean = cw;
            assert_eq!(hamming10_check_and_correct(&mut clean), Some(0));
            for bit in 0..10 {
                let mut w = cw ^ (1 << bit);
                assert_eq!(hamming10_check_and_correct(&mut w), Some(1), "bit {bit}");
                assert_eq!(w, cw);
            }
        }
    }

    #[test]
    fn hamming15_all_single_errors() {
        for data in [0u16, 0x2AB, 0x7FF, 0x555, 0x123] {
            let cw = hamming15_encode(data);
            let mut clean = cw;
            assert_eq!(hamming15_check_and_correct(&mut clean), 0);
            for bit in 0..15 {
                let mut w = cw ^ (1 << bit);
                assert_eq!(hamming15_check_and_correct(&mut w), 1, "bit {bit}");
                assert_eq!(w, cw);
            }
        }
    }
}
