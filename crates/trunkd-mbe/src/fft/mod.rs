// SPDX-License-Identifier: GPL-3.0-or-later
//! A 256-point single-precision real FFT that reproduces JTransforms'
//! `FloatFFT_1D(256)` bit-for-bit.
//!
//! JMBE's [`MBESynthesizer.getUnvoiced()`] runs the windowed noise buffer
//! through `realForward`, scales the half-complex bins per unvoiced band, then
//! `realInverse(a, true)`. Because the unvoiced excitation feeds straight into
//! the output samples, any difference in FFT rounding would show up in the PCM,
//! so this module follows the Java control flow exactly rather than
//! substituting an equivalent transform.
//!
//! For n = 256 JTransforms takes the `SPLIT_RADIX` plan with `nw = 128` and
//! `nc = 64`. `cftfsub` then resolves to
//! `cftf1st(256) -> cftleaf(256, 1) -> bitrv2(256)` and `cftbsub` to
//! `cftb1st(256) -> cftleaf(256, 1) -> bitrv2conj(256)`. The twiddle table
//! (`makewt` + `makect`) and the two bit-reversal permutations are constants
//! lifted from the Java implementation; see `tables.rs`.

mod kernels;
mod tables;
#[cfg(test)]
mod tests;

use kernels::{cftb1st, cftf081, cftf082, cftf1st, cftmdl1, cftmdl2, rftfsub};
use tables::{BITRV2, BITRV2CONJ, BITRV2CONJ_NEGATE, W};

/// Number of points; JMBE only ever uses a 256-point transform.
pub(crate) const N: usize = 256;

const NW: i32 = 128;
const NC: i32 = 64;

/// `CommonUtils.cftleaf(n, isplt, ...)` for n != 512, which is the arm taken
/// for n = 256. `isplt` is always 1 at this call site.
fn cftleaf_256(a: &mut [f32]) {
    cftmdl1(64, a, 0, &W, NW - 32);
    cftf081(a, 0, &W, NW - 8);
    cftf082(a, 16, &W, NW - 8);
    cftf081(a, 32, &W, NW - 8);
    cftf081(a, 48, &W, NW - 8);
    cftmdl2(64, a, 64, &W, NW - 64);
    cftf081(a, 64, &W, NW - 8);
    cftf082(a, 80, &W, NW - 8);
    cftf081(a, 96, &W, NW - 8);
    cftf082(a, 112, &W, NW - 8);
    cftmdl1(64, a, 128, &W, NW - 32);
    cftf081(a, 128, &W, NW - 8);
    cftf082(a, 144, &W, NW - 8);
    cftf081(a, 160, &W, NW - 8);
    cftf081(a, 176, &W, NW - 8);
    // isplt != 0
    cftmdl1(64, a, 192, &W, NW - 32);
    cftf081(a, 240, &W, NW - 8);
    cftf081(a, 192, &W, NW - 8);
    cftf082(a, 208, &W, NW - 8);
    cftf081(a, 224, &W, NW - 8);
}

/// Applies one of the pre-computed bit-reversal permutations. Both `bitrv2`
/// and `bitrv2conj` are pure data movement (the `conj` variant additionally
/// negates selected slots), so they are captured as tables.
fn permute(a: &mut [f32], perm: &[u16; N], negate: Option<&[bool; N]>) {
    let mut out = [0.0f32; N];
    for i in 0..N {
        let v = a[perm[i] as usize];
        out[i] = match negate {
            Some(neg) if neg[i] => -v,
            _ => v,
        };
    }
    a[..N].copy_from_slice(&out);
}

/// `CommonUtils.cftfsub(256, a, 0, ip, 128, w)`.
fn cftfsub(a: &mut [f32]) {
    cftf1st(N as i32, a, 0, &W, NW - (N as i32 >> 2));
    cftleaf_256(a);
    permute(a, &BITRV2, None);
}

/// `CommonUtils.cftbsub(256, a, 0, ip, 128, w)`.
fn cftbsub(a: &mut [f32]) {
    cftb1st(N as i32, a, 0, &W, NW - (N as i32 >> 2));
    cftleaf_256(a);
    permute(a, &BITRV2CONJ, Some(&BITRV2CONJ_NEGATE));
}

/// `FloatFFT_1D.realForward(a)` for n = 256.
///
/// Output layout: `a[0] = Re[0]`, `a[1] = Re[128]`, `a[2k] = Re[k]`,
/// `a[2k + 1] = Im[k]` for `0 < k < 128`.
pub(crate) fn real_forward(a: &mut [f32; N]) {
    cftfsub(a);
    rftfsub(N as i32, a, 0, NC, &W, NW);
    let xi = a[0] - a[1];
    a[0] += a[1];
    a[1] = xi;
}

/// `FloatFFT_1D.realInverse(a, true)` for n = 256 (i.e. with `1 / (n / 2)`
/// scaling applied).
pub(crate) fn real_inverse_scaled(a: &mut [f32; N]) {
    a[1] = 0.5 * (a[0] - a[1]);
    a[0] -= a[1];
    rftfsub(N as i32, a, 0, NC, &W, NW);
    cftbsub(a);
    let scale = 1.0f32 / (N as f32 / 2.0);
    for v in a.iter_mut() {
        *v *= scale;
    }
}
