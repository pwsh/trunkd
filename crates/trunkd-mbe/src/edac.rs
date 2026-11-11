// SPDX-License-Identifier: GPL-3.0-or-later
//! Error detection and correction, ported from `jmbe.edac`.
//!
//! An IMBE voice frame protects coset words `c0`..`c3` with Golay(23,12,7) and
//! `c4`..`c6` with Hamming(15,11,3). The returned error counts are what JMBE
//! feeds into the frame-repeat and muting logic, so the exact counting
//! behavior (including Golay's "give up and call it 4") matters as much as the
//! corrections themselves.

use crate::bits::Bits;

/// Golay(23,12,7) checksums; `CRCUtil.generate(12, 11, 0xC75, 0x0, true)` in
/// JMBE's comments. Shared by `Golay23` and `Golay24`.
const GOLAY_CHECKSUMS: [u32; 23] = [
    0x63A, 0x31D, 0x7B4, 0x3DA, 0x1ED, 0x6CC, 0x366, 0x1B3, 0x6E3, 0x54B, 0x49F, 0x475, 0x400,
    0x200, 0x100, 0x080, 0x040, 0x020, 0x010, 0x008, 0x004, 0x002, 0x001,
];

const GOLAY_MAX_CORRECTABLE_ERRORS: u32 = 3;

fn golay_checksum<const N: usize>(frame: &Bits<N>, start: usize) -> u32 {
    let mut calculated = 0u32;
    for i in 0..12 {
        if frame.get(start + i) {
            calculated ^= GOLAY_CHECKSUMS[i];
        }
    }
    calculated
}

fn golay_syndrome<const N: usize>(frame: &Bits<N>, start: usize) -> u32 {
    golay_checksum(frame, start) ^ frame.get_int_range(start + 12, start + 22)
}

/// `jmbe.edac.Golay23.checkAndCorrect` — returns the number of detected bit
/// errors. Up to three errors are corrected in place; anything worse leaves the
/// frame untouched and reports a count greater than three.
pub(crate) fn golay23_check_and_correct<const N: usize>(frame: &mut Bits<N>, start: usize) -> u32 {
    if golay_syndrome(frame, start) == 0 {
        return 0;
    }

    let mut copy: Bits<23> = frame.sub(start);

    let mut index: i32 = -1;
    let mut syndrome_weight = GOLAY_MAX_CORRECTABLE_ERRORS;

    while index < 23 {
        if index != -1 {
            // Restore the previously flipped bit.
            if index > 0 {
                copy.flip(index as usize - 1);
            }
            copy.flip(index as usize);
            syndrome_weight = GOLAY_MAX_CORRECTABLE_ERRORS - 1;
        }

        let mut syndrome = golay_syndrome(&copy, 0);

        if syndrome > 0 {
            for i in 0..23 {
                let errors = syndrome.count_ones();

                if errors <= syndrome_weight {
                    copy.xor_value(12, 11, syndrome);
                    copy.rotate_right(i, 0, 22);

                    let corrected = copy.get_int_range(0, 22);
                    let original = frame.get_int_range(start, start + 22);
                    let error_count = (original ^ corrected).count_ones();

                    if error_count <= 3 {
                        frame.load(start, 23, corrected);
                    }

                    return error_count;
                }

                copy.rotate_left(0, 22);
                syndrome = golay_syndrome(&copy, 0);
            }

            index += 1;
        }
    }

    // Correction attempt failed.
    4
}

/// `jmbe.edac.Golay24.checkAndCorrect` — the Golay(24,12,7) variant, kept for
/// parity with the reference library. The IMBE frame format does not use it
/// (its cosets are the 23-bit flavour), so this is exercised only by tests.
#[allow(dead_code)]
pub(crate) fn golay24_check_and_correct<const N: usize>(
    message: &mut Bits<N>,
    start: usize,
) -> u32 {
    let parity_error = (0..N).filter(|&i| message.get(i)).count() % 2 != 0;

    let mut syndrome = golay_syndrome(message, start);

    if syndrome == 0 {
        if parity_error {
            message.flip(start + 23);
            return 1;
        }
        return 0;
    }

    let original = message.get_int_range(0, 22);

    let mut index: i32 = -1;
    let mut syndrome_weight = 3u32;

    while index < 23 {
        if index != -1 {
            if index > 0 {
                message.flip(index as usize - 1);
            }
            message.flip(index as usize);
            syndrome_weight = 2;
        }

        syndrome = golay_syndrome(message, start);

        if syndrome > 0 {
            for i in 0..23 {
                let mut errors = syndrome.count_ones();

                if errors <= syndrome_weight {
                    message.xor_value(12, 11, syndrome);
                    message.rotate_right(i, start, start + 22);

                    if index >= 0 {
                        errors += 1;
                    }

                    let corrected = message.get_int_range(0, 22);

                    if (original ^ corrected).count_ones() > 3 {
                        return 4;
                    }

                    return errors;
                }

                message.rotate_left(start, start + 22);
                syndrome = golay_syndrome(message, start);
            }

            index += 1;
        }
    }

    2
}

/// Hamming(15,11,3) parity lookup for data bits 11 down to 1.
const HAMMING_CHECKSUMS: [u32; 11] = [0xF, 0xE, 0xD, 0xC, 0xB, 0xA, 0x9, 0x7, 0x6, 0x5, 0x3];

/// Bit offset (relative to the start of the word) corrected for each non-zero
/// syndrome value 1..=15, matching `jmbe.edac.Hamming15`'s switch.
const HAMMING_CORRECTION: [usize; 15] = [14, 13, 10, 12, 9, 8, 7, 11, 6, 5, 4, 3, 2, 1, 0];

/// `jmbe.edac.Hamming15.checkAndCorrect` — corrects any single-bit error and
/// returns 0 (clean) or 1 (corrected).
pub(crate) fn hamming15_check_and_correct<const N: usize>(
    frame: &mut Bits<N>,
    start: usize,
) -> u32 {
    let mut calculated = 0u32;
    for i in 0..11 {
        if frame.get(start + i) {
            calculated ^= HAMMING_CHECKSUMS[i];
        }
    }
    let syndrome = calculated ^ frame.get_int_range(start + 11, start + 14);

    if syndrome == 0 {
        return 0;
    }

    frame.flip(start + HAMMING_CORRECTION[syndrome as usize - 1]);
    1
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encodes an 11-bit value into a Hamming(15,11,3) word using the same
    /// checksum table the decoder uses.
    fn hamming15_encode(data: u32) -> Bits<15> {
        let mut word = Bits::<15>::zeroed();
        word.load(0, 11, data);
        let mut parity = 0u32;
        for i in 0..11 {
            if word.get(i) {
                parity ^= HAMMING_CHECKSUMS[i];
            }
        }
        word.load(11, 4, parity);
        word
    }

    #[test]
    fn hamming15_accepts_clean_words() {
        for data in 0..2048u32 {
            let mut word = hamming15_encode(data);
            assert_eq!(hamming15_check_and_correct(&mut word, 0), 0);
            assert_eq!(word.get_int_range(0, 10), data);
        }
    }

    #[test]
    fn hamming15_corrects_every_single_bit_error() {
        for data in [0u32, 1, 0x2AA, 0x555, 0x7FF] {
            for bit in 0..15 {
                let mut word = hamming15_encode(data);
                word.flip(bit);
                assert_eq!(hamming15_check_and_correct(&mut word, 0), 1, "bit {bit}");
                assert_eq!(word.get_int_range(0, 10), data, "bit {bit}");
            }
        }
    }

    /// Encodes a 12-bit value into a Golay(23,12,7) codeword.
    fn golay23_encode(data: u32) -> Bits<23> {
        let mut word = Bits::<23>::zeroed();
        word.load(0, 12, data);
        let mut checksum = 0u32;
        for i in 0..12 {
            if word.get(i) {
                checksum ^= GOLAY_CHECKSUMS[i];
            }
        }
        word.load(12, 11, checksum);
        word
    }

    #[test]
    fn golay23_accepts_clean_words() {
        for data in 0..4096u32 {
            let mut word = golay23_encode(data);
            assert_eq!(golay23_check_and_correct(&mut word, 0), 0);
        }
    }

    #[test]
    fn golay23_corrects_up_to_three_errors() {
        for data in [0u32, 1, 0xAAA, 0x555, 0xFFF, 0x123] {
            for a in 0..23 {
                for b in (a + 1)..23 {
                    for c in (b + 1)..23 {
                        let clean = golay23_encode(data);
                        let mut word = clean;
                        word.flip(a);
                        word.flip(b);
                        word.flip(c);
                        let errors = golay23_check_and_correct(&mut word, 0);
                        assert_eq!(errors, 3, "data {data:#x} bits {a},{b},{c}");
                        assert_eq!(
                            word.get_int_range(0, 22),
                            clean.get_int_range(0, 22),
                            "data {data:#x} bits {a},{b},{c}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn golay23_miscorrects_four_errors_to_a_valid_codeword() {
        // Golay(23,12,7) is a perfect code, so a word with four errors still
        // lies within distance three of some (wrong) codeword. JMBE reports
        // three corrections and rewrites the frame; what it must never do is
        // leave a word that fails its own syndrome check.
        let clean = golay23_encode(0x123);
        let mut word = clean;
        for bit in [0, 5, 11, 17] {
            word.flip(bit);
        }
        let errors = golay23_check_and_correct(&mut word, 0);
        assert_eq!(errors, 3);
        assert_eq!(golay_syndrome(&word, 0), 0);
        assert_ne!(word.get_int_range(0, 22), clean.get_int_range(0, 22));
    }

    #[test]
    fn golay23_operates_at_an_offset() {
        // The IMBE frame calls this at bit offsets 0, 23, 46 and 69.
        let clean = golay23_encode(0x5A5);
        let mut frame = Bits::<144>::zeroed();
        frame.load(46, 23, clean.get_int_range(0, 22));
        frame.flip(46 + 3);
        frame.flip(46 + 19);
        assert_eq!(golay23_check_and_correct(&mut frame, 46), 2);
        assert_eq!(frame.get_int_range(46, 68), clean.get_int_range(0, 22));
    }

    #[test]
    fn golay24_flags_parity_only_errors() {
        let clean = golay23_encode(0x321);
        let mut word = Bits::<24>::zeroed();
        word.load(0, 23, clean.get_int_range(0, 22));
        // Even parity over the whole 24-bit word: set bit 23 if needed.
        let ones = (0..24).filter(|&i| word.get(i)).count();
        if ones % 2 != 0 {
            word.flip(23);
        }
        assert_eq!(golay24_check_and_correct(&mut word, 0), 0);
        word.flip(23);
        assert_eq!(golay24_check_and_correct(&mut word, 0), 1);
    }
}
