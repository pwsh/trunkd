// SPDX-License-Identifier: GPL-3.0-or-later
//! Shortened Reed-Solomon codes over GF(2^6) (spec §8.6).
//!
//! RS(24,12,13) (as RS(63,51)), RS(24,16,9) (as RS(63,55)) and RS(63,47,17).
//! Same field as BCH: primitive polynomial x^6 + x + 1, α = 2. Decoder is the
//! classic syndromes → Berlekamp iterative ELP → Chien search → Forney
//! pipeline. Symbols are entered in **reverse order, parity first, then data,
//! then zero padding** — see the packing helpers in the message modules.

use super::gf64::{alpha_pow, inv, mul, ALPHA_TO, INDEX_OF};

const NN: usize = 63;
/// Largest parity count used (RS(63,47)).
pub const MAX_PARITY: usize = 16;

/// Result of an RS decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RsResult {
    /// Total corrected bits (popcount of symbol XORs).
    pub corrected_bits: u32,
    /// Number of corrected symbols.
    pub corrected_symbols: u32,
}

/// Decode a shortened RS(63, 63−`parity`) codeword in place.
///
/// `input` is the full 63-symbol vector (6-bit symbols); the caller packs
/// parity-first/reversed per the spec. Returns `None` if uncorrectable
/// (input left unmodified in that case is *not* guaranteed — callers treat
/// the message as invalid).
///
/// `used_len` is the length of the **shortened** codeword: the number of
/// leading symbols the caller actually filled (24 for RS(24,12)/RS(24,16),
/// 36 for the HDU's RS(63,47)). Symbols at `used_len..63` are known zero on
/// both sides of the link, so an error locator there is provably a
/// miscorrection — the decoder found a valid RS(63,·) codeword that is not a
/// valid *shortened* codeword. Accepting those was the main source of
/// wrong-but-"valid" decodes (e.g. bogus EncryptionSync ALGID/KID/MI): with
/// t+1 random symbol errors over 3000 trials, RS(24,16) accepted 82 wrong
/// words (every one with a locator ≥ 24) and RS(24,12) accepted 2; with the
/// check, both accept 0. See `shortened_region_check_removes_wrong_accepts`.
pub fn decode(input: &mut [u8; NN], parity: usize, used_len: usize) -> Option<RsResult> {
    assert!(
        parity <= MAX_PARITY && parity.is_multiple_of(2),
        "RS parity must be even and <= {MAX_PARITY}, got {parity}"
    );
    assert!(
        used_len > parity && used_len <= NN,
        "RS used_len must be in ({parity}, {NN}], got {used_len}"
    );
    let t = parity / 2;

    // Syndromes s[i] = r(α^i) for i = 1..=parity.
    let mut synd = [0u8; MAX_PARITY + 1];
    let mut any = false;
    for i in 1..=parity {
        let mut s = 0u8;
        for (j, &r) in input.iter().enumerate() {
            if r != 0 {
                s ^= ALPHA_TO[(INDEX_OF[r as usize] as usize + i * j) % 63];
            }
        }
        synd[i] = s;
        any |= s != 0;
    }
    if !any {
        return Some(RsResult {
            corrected_bits: 0,
            corrected_symbols: 0,
        });
    }

    // Berlekamp-Massey for the error-locator polynomial elp(x).
    let mut elp = [0u8; MAX_PARITY + 2];
    let mut prev = [0u8; MAX_PARITY + 2];
    elp[0] = 1;
    prev[0] = 1;
    let mut l = 0usize;
    let mut m = 1usize;
    let mut b = 1u8;
    for n in 1..=parity {
        let mut d = synd[n];
        for i in 1..=l {
            d ^= mul(elp[i], synd[n - i]);
        }
        if d == 0 {
            m += 1;
        } else if 2 * l < n {
            let tmp = elp;
            let coef = mul(d, inv(b));
            for i in 0..=MAX_PARITY + 1 - m {
                elp[i + m] ^= mul(coef, prev[i]);
            }
            l = n - l;
            prev = tmp;
            b = d;
            m = 1;
        } else {
            let coef = mul(d, inv(b));
            for i in 0..=MAX_PARITY + 1 - m {
                elp[i + m] ^= mul(coef, prev[i]);
            }
            m += 1;
        }
    }
    if l > t {
        return None;
    }

    // Chien search: roots of elp; error location loc = 63 − i for a root at
    // α^i (i.e. locator X = α^loc).
    let mut locs = [0usize; MAX_PARITY / 2];
    let mut nroots = 0usize;
    for i in 0..NN {
        let mut v = 0u8;
        for (k, &c) in elp.iter().enumerate().take(l + 1) {
            if c != 0 {
                v ^= ALPHA_TO[(INDEX_OF[c as usize] as usize + k * i) % 63];
            }
        }
        if v == 0 {
            if nroots >= t {
                return None;
            }
            let loc = (63 - i) % 63;
            if loc >= used_len {
                // Error located in the shortened (known-zero) region: this is
                // not a codeword of the shortened code — reject.
                return None;
            }
            locs[nroots] = loc;
            nroots += 1;
        }
    }
    if nroots != l {
        return None;
    }

    // Forney error values. Error evaluator Ω(x) = [S(x)·Λ(x)] mod x^(parity+1)
    // with S(x) = s₁x + s₂x² + …
    let mut omega = [0u8; MAX_PARITY + 1];
    for i in 1..=parity {
        let mut v = synd[i];
        for j in 1..=l.min(i - 1) {
            v ^= mul(elp[j], synd[i - j]);
        }
        omega[i] = v;
    }

    let mut corrected_bits = 0u32;
    for &loc in locs.iter().take(nroots) {
        // Evaluate at y = X^{-1} = α^{63-loc}.
        let y_log = (63 - loc) % 63;
        // num = Σ_{i=1..parity} Ω_i · y^(i-1)
        let mut num = 0u8;
        for (i, &oi) in omega.iter().enumerate().skip(1).take(parity) {
            if oi != 0 {
                num ^= ALPHA_TO[(INDEX_OF[oi as usize] as usize + (i - 1) * y_log) % 63];
            }
        }
        // den = Λ'(y): derivative keeps odd-power terms, Σ_{odd i≤l} Λ_i·y^(i-1).
        let mut den = 0u8;
        let mut i = 1;
        while i <= l {
            if elp[i] != 0 {
                den ^= ALPHA_TO[(INDEX_OF[elp[i] as usize] as usize + (i - 1) * y_log) % 63];
            }
            i += 2;
        }
        if den == 0 {
            return None;
        }
        let e = mul(num, inv(den));
        if e == 0 {
            return None;
        }
        corrected_bits += e.count_ones();
        input[loc] ^= e;
    }

    Some(RsResult {
        corrected_bits,
        corrected_symbols: nroots as u32,
    })
}

/// Encode: append `parity` parity symbols to `data` (63−parity data symbols,
/// polynomial form: index = power of x). Generator = Π (X + α^i), i = 1..=parity
/// (narrow-sense, first root α¹). Test helper.
pub fn encode(codeword: &mut [u8; NN], parity: usize) {
    // Build generator polynomial g(x), degree `parity`, g[0..=parity].
    let mut g = [0u8; MAX_PARITY + 1];
    g[0] = 1;
    for (deg, i) in (1..=parity).enumerate() {
        let root = alpha_pow(i);
        let mut next = [0u8; MAX_PARITY + 1];
        for k in 0..=deg {
            next[k + 1] ^= g[k];
            next[k] ^= mul(g[k], root);
        }
        g = next;
    }
    // Systematic encode: data occupies positions parity..NN-1; parity goes in
    // positions 0..parity. Compute remainder of data(x)·1 divided by g(x)
    // using synthetic division (data already placed at high positions).
    let mut rem = [0u8; MAX_PARITY];
    for j in (parity..NN).rev() {
        let feedback = codeword[j] ^ if parity > 0 { rem[parity - 1] } else { 0 };
        for k in (1..parity).rev() {
            rem[k] = rem[k - 1] ^ mul(g[k], feedback);
        }
        if parity > 0 {
            rem[0] = mul(g[0], feedback);
        }
    }
    codeword[..parity].copy_from_slice(&rem[..parity]);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic LCG for reproducible random tests.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.0 >> 11
        }
        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    /// Build a random shortened codeword: data in `parity..used_len`, parity
    /// symbols in `0..parity`, zeros above `used_len`.
    fn random_codeword(parity: usize, used_len: usize, rng: &mut Rng) -> [u8; NN] {
        let mut cw = [0u8; NN];
        for sym in cw.iter_mut().take(used_len).skip(parity) {
            *sym = (rng.next() & 0x3F) as u8;
        }
        encode_shortened(&mut cw, parity, used_len);
        cw
    }

    /// Systematic encode of a *shortened* codeword: the symbols above
    /// `used_len` stay zero, which is exactly how the link uses these codes.
    fn encode_shortened(cw: &mut [u8; NN], parity: usize, used_len: usize) {
        for sym in cw.iter_mut().skip(used_len) {
            *sym = 0;
        }
        encode(cw, parity);
    }

    fn check_code(parity: usize, used_len: usize, seed: u64) {
        let t = parity / 2;
        let mut cw = [0u8; NN];
        let mut s = seed;
        for sym in cw.iter_mut().take(used_len).skip(parity) {
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1);
            *sym = ((s >> 58) & 0x3F) as u8;
        }
        encode(&mut cw, parity);

        // Verify all syndromes are zero on the clean codeword.
        let mut clean = cw;
        let r = decode(&mut clean, parity, used_len).expect("clean");
        assert_eq!(r.corrected_symbols, 0);
        assert_eq!(clean, cw);

        // Inject up to t symbol errors at spread positions inside the used
        // region.
        for n in 1..=t {
            let mut e = cw;
            for k in 0..n {
                let pos = (k * 7 + 5) % used_len;
                e[pos] ^= (0x15 + k as u8) & 0x3F | 1;
            }
            let r = decode(&mut e, parity, used_len)
                .unwrap_or_else(|| panic!("{n} errors parity {parity}"));
            assert_eq!(e, cw, "{n} errors parity {parity}");
            assert_eq!(r.corrected_symbols as usize, n);
            assert!(r.corrected_bits >= n as u32);
        }
    }

    #[test]
    fn rs_24_12_13() {
        check_code(12, 24, 11);
    }

    #[test]
    fn rs_24_16_9() {
        check_code(8, 24, 22);
    }

    #[test]
    fn rs_63_47_17() {
        check_code(16, 36, 33);
    }

    /// A "correction" landing outside the shortened region is provably wrong:
    /// those symbols are known zero at both ends. Verify that no accepted
    /// decode ever writes there, and that the wrong-accept rate at t+1 random
    /// symbol errors is now negligible (RS(24,16) previously accepted 94/3000
    /// such words, 93 of them with a locator ≥ 24).
    #[test]
    fn beyond_t_errors_are_rejected_not_miscorrected() {
        const TRIALS: usize = 3000;
        for (parity, used_len, max_accept) in [(12usize, 24usize, 3usize), (8, 24, 3), (16, 36, 3)]
        {
            let t = parity / 2;
            let mut rng = Rng(0x1234_5678 ^ (parity as u64) << 32);
            let mut accepted = 0usize;
            let mut exact = 0usize;
            for _ in 0..TRIALS {
                let cw = random_codeword(parity, used_len, &mut rng);
                let mut e = cw;
                let mut placed = 0;
                while placed <= t {
                    let pos = rng.below(used_len);
                    let v = ((rng.next() & 0x3F) as u8) | 1;
                    if e[pos] == cw[pos] {
                        e[pos] ^= v;
                        placed += 1;
                    }
                }
                if let Some(r) = decode(&mut e, parity, used_len) {
                    accepted += 1;
                    if e == cw {
                        exact += 1;
                    }
                    // Never touch the known-zero region.
                    assert!(
                        e[used_len..].iter().all(|&x| x == 0),
                        "parity {parity}: correction outside the shortened region"
                    );
                    assert!(r.corrected_symbols as usize <= t);
                }
            }
            assert_eq!(exact, 0, "t+1 errors cannot decode back to the codeword");
            assert!(
                accepted <= max_accept,
                "parity {parity}: {accepted}/{TRIALS} wrong-accepts at t+1 errors \
                 (expected <= {max_accept})"
            );
        }
    }

    /// Errors injected in the known-zero region must always be rejected: any
    /// such word is not a codeword of the shortened code.
    #[test]
    fn errors_in_shortened_region_rejected() {
        for (parity, used_len) in [(12usize, 24usize), (8, 24), (16, 36)] {
            let mut rng = Rng(99 + parity as u64);
            let cw = random_codeword(parity, used_len, &mut rng);
            for pos in used_len..NN {
                let mut e = cw;
                e[pos] ^= 0x2A;
                assert!(
                    decode(&mut e, parity, used_len).is_none(),
                    "parity {parity}: accepted an error at shortened position {pos}"
                );
            }
        }
    }

    /// Direct before/after for the shortened-region check: passing
    /// `used_len = 63` reproduces the old behavior (Chien over all 63
    /// positions), passing the real `used_len` is the fix. Measured here with
    /// t+1 random symbol errors over 3000 trials per code:
    ///
    /// | code       | legacy wrong-accepts | with `used_len` |
    /// |------------|----------------------|-----------------|
    /// | RS(24,16)  | 82/3000 (all with a locator ≥ 24) | 0/3000 |
    /// | RS(24,12)  | 2/3000                            | 0/3000 |
    /// | RS(63,47) shortened to 36 | 0/3000             | 0/3000 |
    #[test]
    fn shortened_region_check_removes_wrong_accepts() {
        const TRIALS: usize = 3000;
        for (parity, used_len, legacy_expect) in
            [(12usize, 24usize, 2usize), (8, 24, 82), (16, 36, 0)]
        {
            let t = parity / 2;
            for (effective, expect) in [(NN, legacy_expect), (used_len, 0)] {
                let mut rng = Rng(0x1234_5678 ^ (parity as u64) << 32);
                let mut accepted = 0usize;
                let mut outside = 0usize;
                for _ in 0..TRIALS {
                    let cw = random_codeword(parity, used_len, &mut rng);
                    let mut e = cw;
                    let mut placed = 0;
                    while placed <= t {
                        let pos = rng.below(used_len);
                        let v = ((rng.next() & 0x3F) as u8) | 1;
                        if e[pos] == cw[pos] {
                            e[pos] ^= v;
                            placed += 1;
                        }
                    }
                    if decode(&mut e, parity, effective).is_some() {
                        accepted += 1;
                        // Every accepted decode here is wrong (t+1 errors), and
                        // legacy ones put corrections in the known-zero region.
                        assert_ne!(e, cw);
                        if e[used_len..].iter().any(|&x| x != 0) {
                            outside += 1;
                        }
                    }
                }
                assert_eq!(
                    accepted, expect,
                    "parity {parity}, used_len {effective}: {accepted}/{TRIALS} accepted"
                );
                if effective == used_len {
                    assert_eq!(outside, 0);
                } else {
                    assert_eq!(
                        outside, accepted,
                        "legacy accepts all came from the shortened region"
                    );
                }
            }
        }
    }
}
