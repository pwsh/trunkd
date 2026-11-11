// SPDX-License-Identifier: GPL-3.0-or-later
//! GF(2^6) arithmetic shared by BCH(63,16) and the Reed-Solomon codes.
//!
//! Primitive polynomial x^6 + x + 1 (0x43); α = 2. Tables built at compile
//! time via the spec's recurrence: `x <<= 1; if (x & 64) x ^= 0x43`
//! (spec §8.1, §8.6 — both describe the same field).

/// Antilog table: `ALPHA_TO[i] = α^i` for i in 0..63; `ALPHA_TO[63] = 1`.
///
/// Sequence begins 1,2,4,8,16,32,3,6,12,24,48,35,… (spec §8.1).
pub const ALPHA_TO: [u8; 64] = build_alpha();

/// Log table: `INDEX_OF[α^i] = i`; `INDEX_OF[0] = 0` sentinel (spec: a_log[0]
/// is a sentinel — callers must test for zero before using logs).
pub const INDEX_OF: [u8; 64] = build_index();

const fn build_alpha() -> [u8; 64] {
    let mut t = [0u8; 64];
    let mut x: u16 = 1;
    let mut i = 0;
    while i < 63 {
        t[i] = x as u8;
        x <<= 1;
        if x & 64 != 0 {
            x ^= 0x43;
        }
        i += 1;
    }
    t[63] = 1;
    t
}

const fn build_index() -> [u8; 64] {
    let a = build_alpha();
    let mut t = [0u8; 64];
    let mut i = 0;
    while i < 63 {
        t[a[i] as usize] = i as u8;
        i += 1;
    }
    t[0] = 0; // sentinel
    t
}

/// GF(64) multiply.
#[inline]
pub fn mul(a: u8, b: u8) -> u8 {
    if a == 0 || b == 0 {
        0
    } else {
        ALPHA_TO[(INDEX_OF[a as usize] as usize + INDEX_OF[b as usize] as usize) % 63]
    }
}

/// GF(64) inverse of a nonzero element.
#[inline]
pub fn inv(a: u8) -> u8 {
    debug_assert!(a != 0);
    ALPHA_TO[(63 - INDEX_OF[a as usize] as usize) % 63]
}

/// GF(64) square.
#[inline]
pub fn sq(a: u8) -> u8 {
    mul(a, a)
}

/// α^e for any non-negative exponent.
#[inline]
pub fn alpha_pow(e: usize) -> u8 {
    ALPHA_TO[e % 63]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_basics() {
        assert_eq!(
            &ALPHA_TO[0..12],
            &[1, 2, 4, 8, 16, 32, 3, 6, 12, 24, 48, 35]
        );
        assert_eq!(ALPHA_TO[63], 1);
        // α^63 = 1 via multiplication too.
        let mut x = 1u8;
        for _ in 0..63 {
            x = mul(x, 2);
        }
        assert_eq!(x, 1);
        for a in 1..64u8 {
            assert_eq!(mul(a, inv(a)), 1);
            assert_eq!(sq(a), mul(a, a));
        }
    }
}
