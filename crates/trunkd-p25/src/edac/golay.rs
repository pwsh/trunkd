// SPDX-License-Identifier: GPL-3.0-or-later
//! Golay 24/23/18 codes (spec §8.2).
//!
//! Word representation: a `u32` holding the codeword with **message bit 0 as
//! the most significant codeword bit** — for Golay24, bit i of the message is
//! `(w >> (23 - i)) & 1`; data occupies bits 0–11, checksum bits 12–22,
//! overall parity bit 23. Golay23 drops the parity bit (bit i at
//! `(w >> (22 - i)) & 1`).
//!
//! Decode uses the rotate-and-search method over the 23-bit core as
//! described in the spec: try the received word and each single trial-flip;
//! for each, rotate left up to 23 times looking for a low-weight syndrome,
//! apply it to the checksum region, rotate back, and accept only if the total
//! number of flipped bits stays within the correction budget.

/// Shared 23-entry checksum table (generator polynomial 0xC75).
pub const CHECKSUMS: [u16; 23] = [
    0x63A, 0x31D, 0x7B4, 0x3DA, 0x1ED, 0x6CC, 0x366, 0x1B3, 0x6E3, 0x54B, 0x49F, 0x475, 0x400,
    0x200, 0x100, 0x080, 0x040, 0x020, 0x010, 0x008, 0x004, 0x002, 0x001,
];

const MASK23: u32 = 0x7F_FFFF;

/// Syndrome of a 23-bit core word (bit 0 = MSB at position 22):
/// checksum accumulated over the 12 data bits XOR the stored 11-bit checksum.
#[inline]
fn syndrome(core: u32) -> u16 {
    let mut calc: u16 = 0;
    for (i, &c) in CHECKSUMS.iter().enumerate().take(12) {
        if core >> (22 - i) & 1 != 0 {
            calc ^= c;
        }
    }
    let stored = (core & 0x7FF) as u16;
    calc ^ stored
}

#[inline]
fn rotl23(w: u32, n: u32) -> u32 {
    ((w << n) | (w >> (23 - n))) & MASK23
}

/// Rotate-and-search correction of a 23-bit core. Returns the corrected core
/// and the number of errors (0..=3), or `None` if uncorrectable.
fn correct23(core: u32) -> Option<(u32, u32)> {
    if syndrome(core) == 0 {
        return Some((core, 0));
    }
    // Trial 0 = no flip (threshold 3); trials 1..=23 flip bit (t-1) of the
    // core (threshold 2 after a trial flip).
    for trial in 0..=23u32 {
        let (w0, thresh) = if trial == 0 {
            (core, 3)
        } else {
            (core ^ (1 << (23 - trial)), 2)
        };
        let mut w = w0;
        for rot in 0..23u32 {
            let s = syndrome(w);
            if s.count_ones() <= thresh {
                // The syndrome maps directly onto the checksum region.
                let fixed = w ^ u32::from(s);
                let back = rotl23(fixed, (23 - rot) % 23);
                let errors = (back ^ core).count_ones();
                if errors <= 3 {
                    return Some((back, errors));
                }
            }
            w = rotl23(w, 1);
        }
    }
    None
}

/// Golay(23,12,7): correct `word` in place. Returns the error count 0..=3;
/// on failure returns `u32::MAX` and leaves the word intact.
pub fn golay23_check_and_correct(word: &mut u32) -> u32 {
    match correct23(*word & MASK23) {
        Some((fixed, errors)) => {
            *word = fixed;
            errors
        }
        None => u32::MAX,
    }
}

/// Golay(24,12,8) decode status: 0 = clean, 1 = corrected, 2 = uncorrectable.
///
/// Golay(23,12,7) is a *perfect* code, so the inner [`correct23`] always
/// succeeds — every received word lies within distance 3 of exactly one
/// Golay23 codeword. Detection therefore has to come from the extended code:
/// Golay24 has d = 8 and t = 3, so a decode that needs **more than three**
/// 24-bit flips in total (core correction plus overall-parity repair) is a
/// miscorrection of a ≥4-error pattern and must be reported as uncorrectable
/// instead of silently rewriting the data. Every one of the C(24,4) = 10626
/// weight-4 patterns is caught this way (see `golay24_detects_weight_4`).
///
/// On status 2 the word is left **unmodified**.
pub fn golay24_check_and_correct(word: &mut u32) -> u32 {
    let w = *word & 0xFF_FFFF;
    let core = w >> 1; // bits 0..22 (parity bit 23 excluded)
    let out = if syndrome(core) == 0 {
        // Core is a codeword; fix the parity bit if overall cardinality is odd.
        w ^ (w.count_ones() % 2)
    } else {
        match correct23(core) {
            Some((fixed, _errors)) => {
                let mut out = (fixed << 1) | (w & 1);
                if out.count_ones() % 2 == 1 {
                    out ^= 1; // repair overall parity
                }
                out
            }
            // Unreachable for a perfect code, but keep the detection path.
            None => return 2,
        }
    };
    // Count the TOTAL 24-bit flips, parity repair included.
    let flips = (out ^ w).count_ones();
    if flips > 3 {
        return 2; // beyond t = 3 → detected, word untouched
    }
    *word = out;
    u32::from(flips != 0)
}

/// Golay(18,6,8): the 18-bit word sits at Golay24 bit positions 6–23 (upper
/// six data bits zero). Returns the status as Golay24 and, on status 0/1,
/// writes the corrected 18-bit word back.
///
/// The six leading data bits are known zero on the wire, so a "correction"
/// that sets any of them (u32 bits 18–23 here) is provably a miscorrection —
/// it can only happen with ≥4 errors. Masking them off would hide that, so
/// they are treated as an extra detection check: any flip there → status 2.
pub fn golay18_check_and_correct(word: &mut u32) -> u32 {
    let mut w24 = *word & 0x3_FFFF; // occupies low 18 bits = positions 6..23
    let status = golay24_check_and_correct(&mut w24);
    if status == 2 {
        return 2;
    }
    if w24 & !0x3_FFFF != 0 {
        return 2; // a known-zero bit was flipped: bogus correction
    }
    *word = w24;
    status
}

/// Encode 12 data bits into a Golay(23,12) codeword (test helper).
pub fn golay23_encode(data: u16) -> u32 {
    let mut check: u16 = 0;
    for (i, &c) in CHECKSUMS.iter().enumerate().take(12) {
        if data >> (11 - i) & 1 != 0 {
            check ^= c;
        }
    }
    (u32::from(data & 0xFFF) << 11) | u32::from(check & 0x7FF)
}

/// Encode 12 data bits into a Golay(24,12) codeword with even overall parity
/// (test helper).
pub fn golay24_encode(data: u16) -> u32 {
    let mut w = golay23_encode(data) << 1;
    if w.count_ones() % 2 == 1 {
        w |= 1;
    }
    w
}

/// Encode 6 data bits into a Golay(18,6) codeword (test helper).
pub fn golay18_encode(data: u8) -> u32 {
    golay24_encode(u16::from(data & 0x3F)) & 0x3_FFFF
}

#[cfg(test)]
mod tests {
    use super::*;

    /// All error patterns of exactly `weight` bits over the low `n_bits`.
    fn all_error_patterns(n_bits: u32, weight: u32) -> Vec<u32> {
        let mut out = Vec::new();
        let mut idx = vec![0u32; weight as usize];
        fn rec(start: u32, n_bits: u32, left: usize, idx: &mut Vec<u32>, out: &mut Vec<u32>) {
            if left == 0 {
                out.push(idx.iter().fold(0u32, |a, &b| a | (1 << b)));
                return;
            }
            let k = idx.len() - left;
            for a in start..n_bits {
                idx[k] = a;
                rec(a + 1, n_bits, left - 1, idx, out);
            }
        }
        rec(0, n_bits, weight as usize, &mut idx, &mut out);
        out
    }

    #[test]
    fn golay23_corrects_up_to_3() {
        let cw = golay23_encode(0x8B5);
        for wt in 1..=3 {
            for pat in all_error_patterns(23, wt) {
                let mut w = cw ^ pat;
                let e = golay23_check_and_correct(&mut w);
                assert_eq!(e, wt, "pattern {pat:#x}");
                assert_eq!(w, cw);
            }
        }
        let mut clean = cw;
        assert_eq!(golay23_check_and_correct(&mut clean), 0);
    }

    #[test]
    fn golay24_corrects_up_to_3() {
        let cw = golay24_encode(0x123);
        for wt in 1..=3 {
            let pats = all_error_patterns(24, wt);
            assert_eq!(pats.len(), [24, 276, 2024][wt as usize - 1]);
            for pat in pats {
                let mut w = cw ^ pat;
                let s = golay24_check_and_correct(&mut w);
                assert_eq!(s, 1, "pattern {pat:#x} status {s}");
                // The full 24-bit codeword must be restored, not just the data.
                assert_eq!(w, cw, "pattern {pat:#x}");
            }
        }
        let mut clean = cw;
        assert_eq!(golay24_check_and_correct(&mut clean), 0);
        assert_eq!(clean, cw);
    }

    /// d = 8 ⇒ every weight-4 pattern is detectable. Before the total-flip
    /// accounting these all decoded as "corrected" (status 1), 8855 of them
    /// with wrong data bits.
    #[test]
    fn golay24_detects_weight_4() {
        for data in [0x123u16, 0x5A3, 0x000, 0xFFF] {
            let cw = golay24_encode(data);
            let pats = all_error_patterns(24, 4);
            assert_eq!(pats.len(), 10626);
            for pat in pats {
                let recv = cw ^ pat;
                let mut w = recv;
                let s = golay24_check_and_correct(&mut w);
                assert_eq!(s, 2, "data {data:#x} pattern {pat:#x}");
                assert_eq!(w, recv, "status 2 must leave the word untouched");
            }
        }
    }

    #[test]
    fn golay18_roundtrip() {
        for data in [0u8, 0x15, 0x3F, 0x2A] {
            let cw = golay18_encode(data);
            for pat in all_error_patterns(18, 3) {
                let mut w = cw ^ pat;
                let s = golay18_check_and_correct(&mut w);
                assert!(s <= 1, "pattern {pat:#x}");
                assert_eq!(w, cw, "pattern {pat:#x}");
                assert_eq!(w >> 12, u32::from(data), "pattern {pat:#x}");
            }
            // Weight 4 is detectable in the shortened code too.
            for pat in all_error_patterns(18, 4) {
                let recv = cw ^ pat;
                let mut w = recv;
                assert_eq!(golay18_check_and_correct(&mut w), 2, "pattern {pat:#x}");
                assert_eq!(w, recv);
            }
        }
    }

    /// The six known-zero leading data bits are a real extra detection check:
    /// at weight 5 some patterns survive the Golay24 flip budget but "correct"
    /// into a word with those bits set. Those must be rejected, and nothing
    /// accepted may carry a nonzero known-zero region.
    #[test]
    fn golay18_known_zero_bits_are_checked() {
        let cw = golay18_encode(0x2A);
        let mut caught_by_zero_check = 0usize;
        for pat in all_error_patterns(18, 5) {
            let mut w24 = cw ^ pat;
            let s24 = golay24_check_and_correct(&mut w24);
            let mut w18 = cw ^ pat;
            let s18 = golay18_check_and_correct(&mut w18);
            if s18 != 2 {
                assert_eq!(w18 & !0x3_FFFF, 0, "accepted word has stray bits");
            }
            if s24 != 2 && w24 & !0x3_FFFF != 0 {
                assert_eq!(s18, 2, "pattern {pat:#x} should be rejected");
                caught_by_zero_check += 1;
            }
        }
        assert!(
            caught_by_zero_check > 0,
            "known-zero check never fired — test would be vacuous"
        );
    }
}
