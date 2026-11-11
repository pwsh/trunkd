// SPDX-License-Identifier: GPL-3.0-or-later
//! BCH(63,16,23) NID protection with NAC-biased two-pass decode (spec §8.1).
//!
//! The 64-bit NID word is MSB-first: bit 0 (the u64's bit 63) is the NAC MSB.
//! Codeword: 12-bit NAC (bits 0–11), 4-bit DUID (12–15), 47 parity bits
//! (16–62); bit 63 is an overall even-parity bit ignored by the decoder.
//!
//! Syndromes use the reversed bit index: message bit `i` (i in 0..=62) is the
//! coefficient of x^(62−i), so a set bit contributes α^(j·(62−i)) to syndrome
//! s_j. Per spec §8.1 the root-finding algorithm is non-normative as long as
//! the algebra matches; this implementation uses Berlekamp–Massey plus a
//! Chien search.

use super::gf64::{alpha_pow, inv, mul, sq, ALPHA_TO};

const T: usize = 11;
const SYNDROMES: usize = 2 * T; // 22

/// Read message bit `i` (0 = first received / MSB) of the 64-bit NID word.
#[inline]
fn bit(word: u64, i: usize) -> bool {
    (word >> (63 - i)) & 1 != 0
}

#[inline]
fn flip_bit(word: &mut u64, i: usize) {
    *word ^= 1u64 << (63 - i);
}

/// Result of a successful NID decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NidDecode {
    /// 12-bit network access code.
    pub nac: u16,
    /// 4-bit data unit ID.
    pub duid: u8,
    /// Number of corrected bits (0..=11 per pass; the NAC-bias pass may
    /// additionally have rewritten the NAC field).
    pub corrected: u32,
}

/// Decode one pass of BCH(63,16,23) in place. Returns the corrected bit
/// count, or `None` if uncorrectable.
fn decode_pass(word: &mut u64) -> Option<u32> {
    // Syndromes s_1..s_22 (stored at index j-1). Odd syndromes directly from
    // set bits; even syndromes are GF-squares of the corresponding odd ones.
    let mut s = [0u8; SYNDROMES];
    let w = *word;
    for i in 0..63 {
        if bit(w, i) {
            let p = 62 - i;
            let mut j = 1;
            while j <= SYNDROMES {
                s[j - 1] ^= alpha_pow(j * p);
                j += 2;
            }
        }
    }
    let mut j = 2;
    while j <= SYNDROMES {
        s[j - 1] = sq(s[j / 2 - 1]);
        j += 2;
    }

    if s.iter().all(|&x| x == 0) {
        return Some(0);
    }

    // Berlekamp–Massey over GF(64).
    let mut c = [0u8; SYNDROMES + 1];
    let mut b = [0u8; SYNDROMES + 1];
    c[0] = 1;
    b[0] = 1;
    let mut l: usize = 0;
    let mut m: usize = 1;
    let mut bb: u8 = 1;
    for n in 0..SYNDROMES {
        let mut d = s[n];
        for i in 1..=l {
            d ^= mul(c[i], s[n - i]);
        }
        if d == 0 {
            m += 1;
        } else if 2 * l <= n {
            let t = c;
            let coef = mul(d, inv(bb));
            for i in 0..=SYNDROMES - m {
                c[i + m] ^= mul(coef, b[i]);
            }
            l = n + 1 - l;
            b = t;
            bb = d;
            m = 1;
        } else {
            let coef = mul(d, inv(bb));
            for i in 0..=SYNDROMES - m {
                c[i + m] ^= mul(coef, b[i]);
            }
            m += 1;
        }
    }

    if l > T {
        return None;
    }

    // Chien search: roots q of C(α^q) = 0 → error position p = (63 − q) % 63,
    // flip message bit 62 − p.
    let mut roots = 0u32;
    let mut fixed = *word;
    for q in 0..63usize {
        let mut val = 0u8;
        for (i, &ci) in c.iter().enumerate().take(l + 1) {
            if ci != 0 {
                val ^= mul(ci, ALPHA_TO[(i * q) % 63]);
            }
        }
        if val == 0 {
            roots += 1;
            let p = (63 - q) % 63;
            flip_bit(&mut fixed, 62 - p);
        }
    }

    if roots as usize != l {
        return None;
    }
    *word = fixed;
    Some(roots)
}

/// BCH(63,16,23) decoder with the NAC-biased two-pass (spec §8.1).
///
/// If the first pass fails and `tracked_nac` is `Some(n)` with `n > 0` and
/// `n` differs from the received NAC field, the NAC field is overwritten with
/// the tracked NAC and decoding is retried once.
pub fn decode_nid(word: u64, tracked_nac: Option<u16>) -> Option<NidDecode> {
    let mut w = word;
    let corrected = match decode_pass(&mut w) {
        Some(c) => c,
        None => {
            let received_nac = ((word >> 52) & 0xFFF) as u16;
            let tracked = tracked_nac.filter(|&n| n > 0 && n != received_nac)?;
            w = (word & !(0xFFFu64 << 52)) | (u64::from(tracked) << 52);
            decode_pass(&mut w)?
        }
    };
    Some(NidDecode {
        nac: ((w >> 52) & 0xFFF) as u16,
        duid: ((w >> 48) & 0xF) as u8,
        corrected,
    })
}

/// NAC tracker (spec §8.1): map NAC → {count, timestamp}; max 3 entries with
/// oldest-timestamp eviction; a NAC is "tracked" only once its count ≥ 3.
#[derive(Debug, Default)]
pub struct NacTracker {
    entries: [(u16, u32, u64); 3], // (nac, count, timestamp_ms); nac 0 = empty
}

impl NacTracker {
    /// Create an empty tracker.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record an observed NAC at time `now_ms`.
    pub fn observe(&mut self, nac: u16, now_ms: u64) {
        if nac == 0 {
            return;
        }
        if let Some(e) = self.entries.iter_mut().find(|e| e.0 == nac) {
            e.1 = e.1.saturating_add(1);
            e.2 = now_ms;
            return;
        }
        // Insert into an empty slot, else evict the oldest timestamp.
        let slot = match self.entries.iter().position(|e| e.0 == 0) {
            Some(i) => i,
            None => self
                .entries
                .iter()
                .enumerate()
                .min_by_key(|(_, e)| e.2)
                .map(|(i, _)| i)
                .unwrap_or(0),
        };
        self.entries[slot] = (nac, 1, now_ms);
    }

    /// The tracked NAC (count ≥ 3), most recently seen first, if any.
    pub fn tracked(&self) -> Option<u16> {
        self.entries
            .iter()
            .filter(|e| e.0 != 0 && e.1 >= 3)
            .max_by_key(|e| e.2)
            .map(|e| e.0)
    }

    /// Reset all tracking (on sync loss).
    pub fn reset(&mut self) {
        self.entries = [(0, 0, 0); 3];
    }
}

/// Build the BCH(63,16) generator polynomial (degree 47) as a bit mask with
/// bit k = coefficient of x^k. Used by the test encoder.
#[allow(dead_code)]
fn generator() -> u64 {
    // LCM of minimal polynomials of α^1..α^22 — collect conjugacy classes.
    let mut covered = [false; 63];
    let mut g: u128 = 1;
    for j in 1..=SYNDROMES {
        if covered[j % 63] {
            continue;
        }
        // Minimal polynomial of α^j: Π (x + α^(j·2^k)) over the conjugacy class.
        let mut poly: [u8; 8] = [0; 8]; // coefficients in GF(64), poly[0] = const
        poly[0] = 1;
        let mut deg = 0usize;
        let mut e = j % 63;
        loop {
            covered[e] = true;
            // Multiply poly by (x + α^e).
            let root = ALPHA_TO[e];
            let mut next = [0u8; 8];
            for (i, &pi) in poly.iter().enumerate().take(deg + 1) {
                next[i + 1] ^= pi; // x * poly
                next[i] ^= mul(pi, root);
            }
            poly = next;
            deg += 1;
            e = (e * 2) % 63;
            if e == j % 63 {
                break;
            }
        }
        // Coefficients of a minimal polynomial over GF(2) are 0/1.
        let mut m: u128 = 0;
        for (i, &pi) in poly.iter().enumerate().take(deg + 1) {
            assert!(pi <= 1, "minimal polynomial coefficient not binary");
            m |= (pi as u128) << i;
        }
        // g *= m (GF(2) polynomial multiply).
        let mut prod: u128 = 0;
        let mut mm = m;
        let mut shift = 0;
        while mm != 0 {
            if mm & 1 != 0 {
                prod ^= g << shift;
            }
            mm >>= 1;
            shift += 1;
        }
        g = prod;
    }
    assert_eq!(128 - g.leading_zeros() - 1, 47, "generator degree");
    g as u64
}

/// Encode a 16-bit (NAC<<4 | DUID) value into a 64-bit NID codeword
/// (systematic; bit 63 = overall even parity). Test/verification helper.
#[allow(dead_code)]
pub fn encode_nid(nac: u16, duid: u8) -> u64 {
    let g = generator();
    let data = (u64::from(nac & 0xFFF) << 4) | u64::from(duid & 0xF);
    // Message polynomial: data bits are coefficients x^62..x^47.
    let mut rem: u64 = 0; // remainder register, degree < 47
    for k in (0..16).rev() {
        let inbit = (data >> k) & 1;
        let top = (rem >> 46) & 1;
        rem = (rem << 1) & ((1u64 << 47) - 1);
        if top ^ inbit != 0 {
            rem ^= g & ((1u64 << 47) - 1);
        }
    }
    // Assemble MSB-first: bits 0..15 data, 16..62 parity (x^46..x^0).
    let mut w: u64 = data << 48;
    for k in 0..47 {
        if (rem >> (46 - k)) & 1 != 0 {
            w |= 1u64 << (63 - (16 + k));
        }
    }
    if w.count_ones() % 2 == 1 {
        w |= 1; // overall even parity in bit 63
    }
    w
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_clean() {
        let w = encode_nid(0x293, 0x7);
        let d = decode_nid(w, None).expect("clean decode");
        assert_eq!(d.nac, 0x293);
        assert_eq!(d.duid, 0x7);
        assert_eq!(d.corrected, 0);
    }

    #[test]
    fn corrects_up_to_11_errors() {
        let w = encode_nid(0xABC, 0x5);
        // Flip up to 11 bits at spread positions within bits 0..62.
        let positions = [0usize, 5, 11, 17, 23, 30, 38, 44, 50, 56, 62];
        for n in 1..=11 {
            let mut e = w;
            for &p in positions.iter().take(n) {
                e ^= 1u64 << (63 - p);
            }
            let d = decode_nid(e, None).unwrap_or_else(|| panic!("{n} errors"));
            assert_eq!(d.nac, 0xABC);
            assert_eq!(d.duid, 0x5);
            assert_eq!(d.corrected as usize, n);
        }
    }

    #[test]
    fn rejects_gross_corruption() {
        let w = encode_nid(0x123, 0x0);
        // 23 flipped bits is beyond design distance; expect failure (or at
        // minimum, no false 0-corrections claim).
        let e = w ^ 0x7FFF_FFFF_FFFF_0000u64 & !0x1;
        if let Some(d) = decode_nid(e, None) {
            assert!(d.corrected > 0);
        }
    }

    /// t+1 = 12 errors is past the guaranteed radius of BCH(63,16,23). A
    /// bounded-distance decoder may reject *or* miscorrect there, and which
    /// one happens is a property of the code, not a bug — so this measures the
    /// split over random 12-error patterns instead of asserting a vacuous
    /// `Some`/`None`. What must hold: an *accepted* decode is either the true
    /// NAC/DUID or a decode that claims corrections (never a silent "clean"),
    /// and the miscorrection rate stays in the measured band.
    #[test]
    fn twelve_errors_rejected_or_miscorrected_with_known_rate() {
        const TRIALS: usize = 400;
        let w = encode_nid(0xABC, 0x5);
        let mut rng = 0x5eed_1234u64;
        let mut next = move || {
            rng = rng
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            rng >> 11
        };
        let (mut rejected, mut miscorrected, mut recovered) = (0usize, 0usize, 0usize);
        for _ in 0..TRIALS {
            let mut e = w;
            let mut placed = 0;
            let mut mask = 0u64;
            while placed < 12 {
                let p = (next() % 63) as usize;
                if mask >> p & 1 == 0 {
                    mask |= 1 << p;
                    e ^= 1u64 << (63 - p);
                    placed += 1;
                }
            }
            match decode_nid(e, None) {
                None => rejected += 1,
                Some(d) => {
                    assert!(d.corrected > 0, "12 errors cannot decode as clean");
                    if d.nac == 0xABC && d.duid == 0x5 {
                        recovered += 1;
                    } else {
                        miscorrected += 1;
                    }
                }
            }
        }
        assert_eq!(rejected + miscorrected + recovered, TRIALS);
        // Measured on this fixed seed: the vast majority of 12-error patterns
        // are rejected outright; the rest miscorrect into a different NID.
        // (The code cannot correct 12 errors, so `recovered` must be 0.)
        assert_eq!(recovered, 0, "t+1 errors must not decode back to the NID");
        assert!(
            rejected * 2 > TRIALS,
            "rejected {rejected}/{TRIALS}, miscorrected {miscorrected}"
        );
    }

    #[test]
    fn nac_bias_second_pass() {
        let w = encode_nid(0x293, 0x7);
        // Corrupt heavily but only in the NAC field + a few parity bits so the
        // biased pass (with NAC restored) succeeds.
        let mut e = w ^ (0xFFFu64 << 52); // trash entire NAC field (12 errors)
        e ^= 1u64 << 20; // plus one parity-region error → 13 total, > t
        assert!(decode_nid(e, None).is_none());
        let d = decode_nid(e, Some(0x293)).expect("biased decode");
        assert_eq!(d.nac, 0x293);
        assert_eq!(d.duid, 0x7);
    }

    #[test]
    fn nac_tracker_policy() {
        let mut t = NacTracker::new();
        t.observe(0x293, 1);
        t.observe(0x293, 2);
        assert_eq!(t.tracked(), None);
        t.observe(0x293, 3);
        assert_eq!(t.tracked(), Some(0x293));
        // Eviction: three other NACs push by oldest timestamp.
        t.observe(0x111, 4);
        t.observe(0x222, 5);
        t.observe(0x333, 6); // evicts 0x293 (oldest ts=3)
        assert_eq!(t.tracked(), None);
        t.reset();
        assert_eq!(t.tracked(), None);
    }
}
