// SPDX-License-Identifier: GPL-3.0-or-later
//! Filter design and streaming FIR primitives (spec §3).
//!
//! Design choices documented per the spec's allowances:
//! - Half-band decimators follow §3.1 exactly (63-tap Hamming ÷2 stage,
//!   23/15-tap Blackman pre-stages).
//! - The baseband low-pass uses a **Kaiser windowed-sinc** design meeting the
//!   §3.2 band edges and the ripple-derived ≈65 dB stopband, instead of a
//!   Parks-McClellan equiripple design. This is an equivalent-design
//!   substitution the implementation notes permit; band edges and
//!   attenuation are honored, exact tap values differ.
//! - The RRC matched filter replicates the GNU-Radio `firdes` formula with
//!   the §3.3 /2 samples-per-symbol quirk exactly.

use num_complex::Complex;

/// Hamming window value at index n of an N-point window.
fn hamming(n: usize, len: usize) -> f64 {
    let x = 2.0 * std::f64::consts::PI * n as f64 / (len - 1) as f64;
    0.54 - 0.46 * x.cos()
}

/// Blackman window value at index n of an N-point window.
///
/// DEVIATION (documented, deliberate): these are the *classic* Blackman
/// coefficients (0.42 / 0.5 / 0.08); the reference's `WindowFactory` uses the
/// "exact" Blackman set (0.4266 / 0.4966 / 0.0768) — a sub-dB sidelobe
/// difference. Where the two implementations overlap, the window only reaches
/// the 15/23-tap half-band *pre*-stages, i.e. decimation factors ≥ 4, which in
/// the reference means channel rates ≥ 100 kHz — not a validated decode path.
/// The other user is [`Ddc`](super::Ddc)'s ÷2 cascade, which has no reference
/// counterpart at all (it is a trunkd extension), so no bit-exactness
/// obligation applies there either. Kept classic so the tap tables stay
/// reproducible straight from the textbook formula.
fn blackman(n: usize, len: usize) -> f64 {
    let x = 2.0 * std::f64::consts::PI * n as f64 / (len - 1) as f64;
    0.42 - 0.5 * x.cos() + 0.08 * (2.0 * x).cos()
}

/// Half-band low-pass taps (spec §3.1): odd length, center tap 0.5, nonzero
/// only at odd offsets from center: `sin(k·π/2)/(k·π) · window`.
pub fn halfband_taps(len: usize, blackman_window: bool) -> Vec<f32> {
    assert!(len % 2 == 1);
    let center = len / 2;
    let mut taps = vec![0.0f32; len];
    for (i, t) in taps.iter_mut().enumerate() {
        let k = i as i64 - center as i64;
        if k == 0 {
            *t = 0.5;
        } else if k % 2 != 0 {
            let kf = k as f64;
            let sinc = (kf * std::f64::consts::FRAC_PI_2).sin() / (kf * std::f64::consts::PI);
            let w = if blackman_window {
                blackman(i, len)
            } else {
                hamming(i, len)
            };
            *t = (sinc * w) as f32;
        }
    }
    taps
}

/// Zeroth-order modified Bessel function of the first kind (series).
fn bessel_i0(x: f64) -> f64 {
    let mut sum = 1.0;
    let mut term = 1.0;
    let half = x / 2.0;
    for k in 1..50 {
        term *= (half / k as f64) * (half / k as f64);
        sum += term;
        if term < 1e-12 * sum {
            break;
        }
    }
    sum
}

/// Kaiser windowed-sinc low-pass meeting (pass_hz, stop_hz) edges at
/// `sample_rate`, ≈`atten_db` stopband. Unity DC gain.
pub fn kaiser_lowpass_taps(
    sample_rate: f64,
    pass_hz: f64,
    stop_hz: f64,
    atten_db: f64,
) -> Vec<f32> {
    let cutoff = (pass_hz + stop_hz) / 2.0 / sample_rate; // normalized (cycles/sample)
    let delta_w = 2.0 * std::f64::consts::PI * (stop_hz - pass_hz) / sample_rate;
    let beta = if atten_db > 50.0 {
        0.1102 * (atten_db - 8.7)
    } else if atten_db >= 21.0 {
        0.5842 * (atten_db - 21.0).powf(0.4) + 0.07886 * (atten_db - 21.0)
    } else {
        0.0
    };
    let mut n = ((atten_db - 7.95) / (2.285 * delta_w)).ceil() as usize + 1;
    if n.is_multiple_of(2) {
        n += 1;
    }
    let center = (n - 1) as f64 / 2.0;
    let i0b = bessel_i0(beta);
    let mut taps = vec![0.0f32; n];
    let mut sum = 0.0f64;
    for (i, t) in taps.iter_mut().enumerate() {
        let k = i as f64 - center;
        let sinc = if k == 0.0 {
            2.0 * cutoff
        } else {
            (2.0 * std::f64::consts::PI * cutoff * k).sin() / (std::f64::consts::PI * k)
        };
        let r = k / center;
        let w = bessel_i0(beta * (1.0 - r * r).max(0.0).sqrt()) / i0b;
        let v = sinc * w;
        sum += v;
        *t = v as f32;
    }
    for t in taps.iter_mut() {
        *t = (f64::from(*t) / sum) as f32;
    }
    taps
}

/// Root-raised-cosine taps per the GNU-Radio `firdes` formula, with the
/// reference's **/2 samples-per-symbol quirk** (spec §3.3): the design uses
/// `sps/2`, and the tap count is `round(sps/2 × span)` (not forced odd).
/// Sum-normalized to unity DC gain.
///
/// DEVIATION (documented, no effect on the validated configurations): the
/// centering index below is the integer `ntaps / 2`, following GNU Radio's
/// `firdes` (which the spec's §3.3 wording tracks); the reference centers on
/// the float `taps / 2.0`. The two agree whenever `ntaps` is even, which
/// covers the 42-tap 25 kHz design used by every validated path. They differ
/// by half a sample for odd tap counts (e.g. the 63-tap 37.5 kHz design),
/// where this implementation stays with the GNU Radio convention.
pub fn rrc_taps_with_half_sps_quirk(sps: f64, span: f64, alpha: f64) -> Vec<f32> {
    let spb = sps / 2.0; // the quirk
    let ntaps = (spb * span).round() as usize;
    let mut taps = vec![0.0f64; ntaps];
    let mut scale = 0.0f64;
    for (i, t) in taps.iter_mut().enumerate() {
        let xindx = i as f64 - (ntaps / 2) as f64;
        let x1 = std::f64::consts::PI * xindx / spb;
        let x2 = 4.0 * alpha * xindx / spb;
        let x3 = x2 * x2 - 1.0;
        let v = if x3.abs() >= 1e-6 {
            let num = if xindx == 0.0 {
                ((1.0 + alpha) * x1).cos() + (1.0 - alpha) * std::f64::consts::PI / (4.0 * alpha)
            } else {
                ((1.0 + alpha) * x1).cos()
                    + ((1.0 - alpha) * x1).sin() / (4.0 * alpha * xindx / spb)
            };
            let den = x3 * std::f64::consts::PI;
            4.0 * alpha * num / den
        } else {
            // Near-singular points |4α·x/spb| ≈ 1.
            if alpha == 1.0 {
                -1.0
            } else {
                let x3b = (1.0 - alpha) * x1;
                let x4 = (1.0 + alpha) * x1;
                let num = x4.sin() * (1.0 + alpha) * std::f64::consts::PI
                    - x3b.cos() * ((1.0 - alpha) * std::f64::consts::PI * spb)
                        / (4.0 * alpha * xindx)
                    + x3b.sin() * spb * spb / (4.0 * alpha * xindx * xindx);
                let den = -32.0 * std::f64::consts::PI * alpha * alpha * xindx / spb;
                4.0 * alpha * num / den
            }
        };
        *t = v;
        scale += v;
    }
    taps.iter().map(|&t| (t / scale) as f32).collect()
}

/// Outputs computed per block iteration by the `process_block` paths.
///
/// Each lane is an *independent* output with its own accumulator, and every
/// accumulator sums its taps in exactly the order the scalar loop used — so
/// the lanes give the compiler four independent dependency chains to
/// interleave without ever reassociating the arithmetic of a single output.
/// That is what makes the block path bit-for-bit identical to `push`.
const LANES: usize = 4;

/// Streaming real-coefficient FIR over complex samples.
///
/// The delay line is a **mirrored contiguous buffer**: `2·w` slots, with every
/// inserted sample written twice — at `pos` and at `pos + w`. Any window of up
/// to `w` consecutive samples is then a plain descending walk of contiguous
/// memory starting at `pos + w - 1`, so the tap loop needs no `%` and no
/// per-tap wrap branch. Zero taps are skipped by iterating a precomputed
/// (lag, coefficient) list built in the same order the original scalar loop
/// visited the taps, which keeps the summation order — and therefore the
/// f32 rounding — unchanged.
pub struct ComplexFir {
    taps: Vec<f32>,
    /// Lag from the newest sample for each nonzero tap, in scalar-loop order.
    nz_lag: Vec<u32>,
    /// Coefficients matching `nz_lag`, same order.
    nz_tap: Vec<f32>,
    /// Mirrored delay line, `2 * w` slots.
    buf: Vec<Complex<f32>>,
    /// Next write index, in `0..w`.
    pos: usize,
    /// Mirror width: `taps.len() + lookback`.
    w: usize,
}

impl ComplexFir {
    /// New filter from taps, able to compute only the current output.
    ///
    /// # Panics
    ///
    /// Panics if `taps` is empty: the delay line would be zero-length and the
    /// first `insert_only` would index out of bounds, so an empty design is
    /// caught here rather than on first use.
    pub fn new(taps: Vec<f32>) -> Self {
        Self::with_lookback(taps, 0)
    }

    /// New filter that can also recompute outputs up to `lookback` samples
    /// behind the newest one (see [`Self::compute_lanes`]). The mirror is
    /// widened to `taps.len() + lookback` so those older windows stay inside
    /// the buffer — a plain `2n` mirror would underflow.
    ///
    /// # Panics
    ///
    /// Panics if `taps` is empty.
    pub fn with_lookback(taps: Vec<f32>, lookback: usize) -> Self {
        assert!(!taps.is_empty(), "ComplexFir requires at least one tap");
        let w = taps.len() + lookback;
        let mut nz_lag = Vec::new();
        let mut nz_tap = Vec::new();
        for (i, &t) in taps.iter().enumerate() {
            if t != 0.0 {
                nz_lag.push(i as u32);
                nz_tap.push(t);
            }
        }
        ComplexFir {
            taps,
            nz_lag,
            nz_tap,
            buf: vec![Complex::new(0.0, 0.0); 2 * w],
            pos: 0,
            w,
        }
    }

    /// Number of taps (filter length).
    pub fn num_taps(&self) -> usize {
        self.taps.len()
    }

    /// Push one sample, get the filtered output.
    #[inline]
    pub fn push(&mut self, s: Complex<f32>) -> Complex<f32> {
        self.insert_only(s);
        self.compute_current()
    }

    /// Insert one sample into the delay line and advance, WITHOUT computing the
    /// output. Pair with [`Self::compute_current`] on the samples whose output
    /// is actually needed — a decimator can skip the dot product on the inputs
    /// whose output it discards (the delay-line state is identical either way,
    /// so the computed outputs are bit-for-bit the same as `push`).
    #[inline]
    pub fn insert_only(&mut self, s: Complex<f32>) {
        let w = self.w;
        self.buf[self.pos] = s;
        self.buf[self.pos + w] = s;
        self.pos += 1;
        if self.pos == w {
            self.pos = 0;
        }
    }

    /// Dot product of the taps against the delay line as of the most recent
    /// [`Self::insert_only`] — equals what [`Self::push`] would have returned
    /// for that sample. Skips zero taps (half-band filters are ~half zeros).
    #[inline]
    pub fn compute_current(&self) -> Complex<f32> {
        // Newest sample sits at pos-1 mod w; `pos + w - 1` is the same slot in
        // the mirror, and every lag below it is a straight subtraction.
        let head = self.pos + self.w - 1;
        let mut re = 0.0f32;
        let mut im = 0.0f32;
        for (&lag, &t) in self.nz_lag.iter().zip(self.nz_tap.iter()) {
            let s = self.buf[head - lag as usize];
            re += s.re * t;
            im += s.im * t;
        }
        Complex::new(re, im)
    }

    /// Compute [`LANES`] outputs at once: lane `j` is the output for the sample
    /// `j * stride` behind the newest, so `out[LANES-1]` is the newest.
    ///
    /// Requires `stride * (LANES - 1) <= lookback` (see
    /// [`Self::with_lookback`]). Each lane sums its taps in the scalar order,
    /// so every lane equals what [`Self::compute_current`] would have returned
    /// at that point in the stream, bit for bit.
    #[inline]
    pub fn compute_lanes(&self, stride: usize) -> [Complex<f32>; LANES] {
        debug_assert!(stride * (LANES - 1) + self.taps.len() <= self.w);
        let head = self.pos + self.w - 1;
        let mut re = [0.0f32; LANES];
        let mut im = [0.0f32; LANES];
        for (&lag, &t) in self.nz_lag.iter().zip(self.nz_tap.iter()) {
            let base = head - lag as usize;
            for j in 0..LANES {
                let s = self.buf[base - j * stride];
                re[j] += s.re * t;
                im[j] += s.im * t;
            }
        }
        std::array::from_fn(|j| Complex::new(re[j], im[j]))
    }
}

/// Decimate-by-2 half-band stage.
pub struct HalfBandDecimator {
    fir: ComplexFir,
    toggle: bool,
}

impl HalfBandDecimator {
    /// New stage with the given half-band taps.
    pub fn new(taps: Vec<f32>) -> Self {
        HalfBandDecimator {
            // Blocked output needs to look back over LANES outputs, i.e.
            // 2*(LANES-1) input samples.
            fir: ComplexFir::with_lookback(taps, 2 * (LANES - 1)),
            toggle: false,
        }
    }

    /// Push one input sample; every second sample yields an output. The dot
    /// product runs only on output samples (the discarded ones just advance the
    /// delay line), halving the arithmetic versus filtering at the input rate.
    #[inline]
    pub fn push(&mut self, s: Complex<f32>) -> Option<Complex<f32>> {
        self.fir.insert_only(s);
        self.toggle = !self.toggle;
        if self.toggle {
            None
        } else {
            Some(self.fir.compute_current())
        }
    }

    /// Filter a whole block, appending the decimated outputs to `out`.
    ///
    /// Bit-for-bit identical to calling [`Self::push`] on each sample and
    /// collecting the `Some` results — including across arbitrary block
    /// boundaries, since the leading odd sample and the trailing remainder are
    /// run through `push` itself and only the aligned middle uses the
    /// [`LANES`]-wide path.
    pub fn process_block(&mut self, input: &[Complex<f32>], out: &mut Vec<Complex<f32>>) {
        let mut i = 0;
        // Re-align to an even phase: if the next sample would produce an
        // output, take it the scalar way first.
        if self.toggle && i < input.len() {
            if let Some(y) = self.push(input[i]) {
                out.push(y);
            }
            i += 1;
        }
        // Aligned middle: 2*LANES inputs → LANES outputs per iteration.
        const GROUP: usize = 2 * LANES;
        while i + GROUP <= input.len() {
            for &s in &input[i..i + GROUP] {
                self.fir.insert_only(s);
            }
            let lanes = self.fir.compute_lanes(2);
            // Lane j is 2*j samples back, so stream order is newest last.
            for y in lanes.iter().rev() {
                out.push(*y);
            }
            i += GROUP;
        }
        for &s in &input[i..] {
            if let Some(y) = self.push(s) {
                out.push(y);
            }
        }
    }
}

/// Streaming FIR decimator (arbitrary integer factor) — the sharp tail stage
/// of [`Ddc`](super::Ddc), used by [`crate::P25P1WidebandChannel`].
pub struct FirDecimator {
    fir: ComplexFir,
    factor: u32,
    count: u32,
}

impl FirDecimator {
    /// New decimator.
    pub fn new(taps: Vec<f32>, factor: u32) -> Self {
        let lookback = (factor as usize).saturating_mul(LANES - 1);
        FirDecimator {
            fir: ComplexFir::with_lookback(taps, lookback),
            factor,
            count: 0,
        }
    }

    /// Number of taps in the decimating filter.
    pub fn num_taps(&self) -> usize {
        self.fir.num_taps()
    }

    /// Push one input sample; every `factor`-th sample yields an output. The dot
    /// product runs only on output samples; the rest just advance the delay line.
    #[inline]
    pub fn push(&mut self, s: Complex<f32>) -> Option<Complex<f32>> {
        self.fir.insert_only(s);
        self.count += 1;
        if self.count == self.factor {
            self.count = 0;
            Some(self.fir.compute_current())
        } else {
            None
        }
    }

    /// Filter a whole block, appending the decimated outputs to `out`.
    ///
    /// Bit-for-bit identical to per-sample [`Self::push`], for any block
    /// splitting (see [`HalfBandDecimator::process_block`]).
    pub fn process_block(&mut self, input: &[Complex<f32>], out: &mut Vec<Complex<f32>>) {
        let f = self.factor as usize;
        if f < 2 {
            // factor 0 has no output phase at all (and would make the aligned
            // loop below spin forever); factor 1 has nothing to batch.
            for &s in input {
                if let Some(y) = self.push(s) {
                    out.push(y);
                }
            }
            return;
        }
        let mut i = 0;
        // Re-align to a phase boundary the scalar way.
        while self.count != 0 && i < input.len() {
            if let Some(y) = self.push(input[i]) {
                out.push(y);
            }
            i += 1;
        }
        let group = f * LANES;
        while i + group <= input.len() {
            for &s in &input[i..i + group] {
                self.fir.insert_only(s);
            }
            let lanes = self.fir.compute_lanes(f);
            for y in lanes.iter().rev() {
                out.push(*y);
            }
            i += group;
        }
        for &s in &input[i..] {
            if let Some(y) = self.push(s) {
                out.push(y);
            }
        }
    }
}

/// Golden tap vectors (spec §3, bit-exactness guard).
///
/// These pin the *exact* f32 tap values of every filter design on a validated
/// decode path. Any change to a windowing formula, a normalization, or the
/// RRC centering convention shows up here as a hard failure rather than as a
/// silent drift in decoded output — which is what protects the planned FIR
/// block/SIMD rework from changing what the decoder emits.
#[cfg(test)]
mod golden {
    use super::*;

    /// RRC at 25 kHz (decimated rate for a 50 kHz channel): 42 taps.
    const RRC_25K: [f32; 42] = [
        -0.0016339162,
        0.001877109,
        0.0023670585,
        -0.0013910498,
        -0.0027590047,
        0.001412811,
        0.0036505663,
        -0.0019015896,
        -0.006488133,
        0.0010319385,
        0.011715453,
        0.004690923,
        -0.016712232,
        -0.018499415,
        0.0147339655,
        0.04050836,
        0.004972759,
        -0.06602427,
        -0.062497813,
        0.08675955,
        0.30141476,
        0.40391046,
        0.30141476,
        0.08675955,
        -0.062497813,
        -0.06602427,
        0.004972759,
        0.04050836,
        0.0147339655,
        -0.018499415,
        -0.016712232,
        0.004690923,
        0.011715453,
        0.0010319385,
        -0.006488133,
        -0.0019015896,
        0.0036505663,
        0.001412811,
        -0.0027590047,
        -0.0013910498,
        0.0023670585,
        0.001877109,
    ];

    /// RRC at 37.5 kHz: 63 taps (the odd-length case, where the GNU Radio
    /// integer centering documented on `rrc_taps_with_half_sps_quirk` differs
    /// from the reference's float centering).
    const RRC_37K5: [f32; 63] = [
        -0.00035192954,
        0.0012506627,
        0.0018791608,
        0.00090282666,
        -0.00092681573,
        -0.0019933488,
        -0.0011955558,
        0.0009413145,
        0.002487934,
        0.0016954814,
        -0.0012669733,
        -0.0040028645,
        -0.0036082696,
        0.00068755035,
        0.0061488897,
        0.007993634,
        0.0031254243,
        -0.006542743,
        -0.014168478,
        -0.0123256175,
        0.0008583647,
        0.01833163,
        0.02698953,
        0.016231587,
        -0.012809119,
        -0.04399004,
        -0.05256353,
        -0.018950546,
        0.057805344,
        0.15552254,
        0.2372871,
        0.2691137,
        0.2372871,
        0.15552254,
        0.057805344,
        -0.018950546,
        -0.05256353,
        -0.04399004,
        -0.012809119,
        0.016231587,
        0.02698953,
        0.01833163,
        0.0008583647,
        -0.0123256175,
        -0.014168478,
        -0.006542743,
        0.0031254243,
        0.007993634,
        0.0061488897,
        0.00068755035,
        -0.0036082696,
        -0.0040028645,
        -0.0012669733,
        0.0016954814,
        0.002487934,
        0.0009413145,
        -0.0011955558,
        -0.0019933488,
        -0.00092681573,
        0.00090282666,
        0.0018791608,
        0.0012506627,
        -0.00035192954,
    ];

    /// 15-tap Blackman half-band (DDC ÷2 cascade, deep FrontEnd pre-stages).
    const HB15: [f32; 15] = [
        6.3106245e-19,
        0.0,
        0.005758444,
        0.0,
        -0.048720825,
        0.0,
        0.29296085,
        0.5,
        0.29296085,
        0.0,
        -0.048720825,
        0.0,
        0.005758444,
        0.0,
        6.3106245e-19,
    ];

    /// 23-tap Blackman half-band (FrontEnd ÷4 pre-stage).
    const HB23: [f32; 23] = [
        4.0158518e-19,
        0.0,
        0.0011532167,
        0.0,
        -0.0072712726,
        0.0,
        0.026381396,
        0.0,
        -0.07809682,
        0.0,
        0.30782056,
        0.5,
        0.30782056,
        0.0,
        -0.07809682,
        0.0,
        0.026381396,
        0.0,
        -0.0072712726,
        0.0,
        0.0011532167,
        0.0,
        4.0158518e-19,
    ];

    /// Bit-exact comparison: `==` on f32 is the point here, not an epsilon.
    fn assert_exact(name: &str, got: &[f32], want: &[f32]) {
        assert_eq!(got.len(), want.len(), "{name}: tap count changed");
        for (i, (g, w)) in got.iter().zip(want).enumerate() {
            assert_eq!(
                g.to_bits(),
                w.to_bits(),
                "{name}: tap {i} changed: {g:?} != {w:?}"
            );
        }
    }

    #[test]
    fn rrc_taps_are_pinned() {
        assert_exact(
            "rrc 25k",
            &rrc_taps_with_half_sps_quirk(25000.0 / 4800.0, 16.0, 0.2),
            &RRC_25K,
        );
        assert_exact(
            "rrc 37.5k",
            &rrc_taps_with_half_sps_quirk(37500.0 / 4800.0, 16.0, 0.2),
            &RRC_37K5,
        );
    }

    #[test]
    fn halfband_taps_are_pinned() {
        assert_exact("halfband 15", &halfband_taps(15, true), &HB15);
        assert_exact("halfband 23", &halfband_taps(23, true), &HB23);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn halfband_structure() {
        let t = halfband_taps(63, false);
        assert_eq!(t.len(), 63);
        assert!((t[31] - 0.5).abs() < 1e-6);
        // Even offsets are exactly zero.
        for k in (2..31).step_by(2) {
            assert_eq!(t[31 + k], 0.0);
            assert_eq!(t[31 - k], 0.0);
        }
        // DC gain ≈ 1.
        let sum: f32 = t.iter().sum();
        assert!((sum - 1.0).abs() < 0.02, "dc gain {sum}");
    }

    #[test]
    fn rrc_quirk_tap_count() {
        // decimatedRate 25000 → sps 5.20833… → design sps 2.604166 →
        // round(2.604166 × 16) = 42 taps (spec §3.3).
        let t = rrc_taps_with_half_sps_quirk(25000.0 / 4800.0, 16.0, 0.2);
        assert_eq!(t.len(), 42);
        let sum: f32 = t.iter().sum();
        assert!((sum - 1.0).abs() < 1e-4);
    }

    #[test]
    fn kaiser_lowpass_meets_edges() {
        let taps = kaiser_lowpass_taps(25000.0, 7250.0, 8000.0, 65.0);
        // Frequency response at DC ≈ 1, in stopband ≤ -55 dB.
        let resp = |f_hz: f64| -> f64 {
            let mut re = 0.0f64;
            let mut im = 0.0f64;
            for (i, &t) in taps.iter().enumerate() {
                let w = 2.0 * std::f64::consts::PI * f_hz / 25000.0 * i as f64;
                re += f64::from(t) * w.cos();
                im -= f64::from(t) * w.sin();
            }
            (re * re + im * im).sqrt()
        };
        assert!((resp(0.0) - 1.0).abs() < 1e-3);
        assert!(resp(3000.0) > 0.98);
        assert!(
            20.0 * resp(8500.0).log10() < -55.0,
            "{}",
            20.0 * resp(8500.0).log10()
        );
        assert!(20.0 * resp(10000.0).log10() < -55.0);
    }

    #[test]
    fn halfband_decimator_passes_dc() {
        let mut d = HalfBandDecimator::new(halfband_taps(63, false));
        let mut out = Vec::new();
        for _ in 0..500 {
            if let Some(y) = d.push(Complex::new(1.0, 0.5)) {
                out.push(y);
            }
        }
        assert_eq!(out.len(), 250);
        let last = out.last().unwrap();
        assert!((last.re - 1.0).abs() < 0.01);
        assert!((last.im - 0.5).abs() < 0.01);
    }

    /// Deterministic pseudo-random complex stream (xorshift, so no dev-deps).
    fn noise(n: usize) -> Vec<Complex<f32>> {
        let mut x = 0x2545_F491_4F6C_DD1Du64;
        let mut next = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            ((x >> 40) as f32 / 8_388_608.0) - 1.0
        };
        (0..n).map(|_| Complex::new(next(), next())).collect()
    }

    /// Feed `input` through `make()` in blocks of `size` and collect outputs.
    fn blocked<T, F, B>(
        input: &[Complex<f32>],
        size: usize,
        mut make: F,
        mut run: B,
    ) -> Vec<Complex<f32>>
    where
        F: FnMut() -> T,
        B: FnMut(&mut T, &[Complex<f32>], &mut Vec<Complex<f32>>),
    {
        let mut f = make();
        let mut out = Vec::new();
        for c in input.chunks(size) {
            run(&mut f, c, &mut out);
        }
        out
    }

    /// The block path must reproduce the per-sample `push` stream exactly, for
    /// every block size — including sizes that straddle the lane group and the
    /// decimation phase.
    #[test]
    fn halfband_block_matches_push_bit_for_bit() {
        let input = noise(20_000);
        for len in [15usize, 23, 63] {
            let mut d = HalfBandDecimator::new(halfband_taps(len, true));
            let mut reference = Vec::new();
            for &s in &input {
                if let Some(y) = d.push(s) {
                    reference.push(y);
                }
            }
            assert_eq!(reference.len(), input.len() / 2);
            for size in [1usize, 7, 64, 1000, input.len()] {
                let got = blocked(
                    &input,
                    size,
                    || HalfBandDecimator::new(halfband_taps(len, true)),
                    |f, c, o| f.process_block(c, o),
                );
                assert_eq!(got.len(), reference.len(), "len {len} block {size}");
                for (i, (a, b)) in got.iter().zip(reference.iter()).enumerate() {
                    assert_eq!(
                        (a.re.to_bits(), a.im.to_bits()),
                        (b.re.to_bits(), b.im.to_bits()),
                        "len {len} block {size} output {i}"
                    );
                }
            }
        }
    }

    #[test]
    fn fir_decimator_block_matches_push_bit_for_bit() {
        let input = noise(20_000);
        let taps = kaiser_lowpass_taps(150_000.0, 12_000.0, 25_000.0, 60.0);
        for factor in [2u32, 3, 5, 6, 25] {
            let mut d = FirDecimator::new(taps.clone(), factor);
            let mut reference = Vec::new();
            for &s in &input {
                if let Some(y) = d.push(s) {
                    reference.push(y);
                }
            }
            assert_eq!(reference.len(), input.len() / factor as usize);
            for size in [1usize, 7, 64, 1000, input.len()] {
                let got = blocked(
                    &input,
                    size,
                    || FirDecimator::new(taps.clone(), factor),
                    |f, c, o| f.process_block(c, o),
                );
                assert_eq!(got.len(), reference.len(), "factor {factor} block {size}");
                for (i, (a, b)) in got.iter().zip(reference.iter()).enumerate() {
                    assert_eq!(
                        (a.re.to_bits(), a.im.to_bits()),
                        (b.re.to_bits(), b.im.to_bits()),
                        "factor {factor} block {size} output {i}"
                    );
                }
            }
        }
    }

    /// Blocks of *varying* size in one stream — the phase carried across a
    /// boundary is where an off-by-one would hide.
    #[test]
    fn halfband_block_matches_push_across_ragged_blocks() {
        let input = noise(9_999);
        let mut d = HalfBandDecimator::new(halfband_taps(15, true));
        let mut reference = Vec::new();
        for &s in &input {
            if let Some(y) = d.push(s) {
                reference.push(y);
            }
        }
        let mut f = HalfBandDecimator::new(halfband_taps(15, true));
        let mut got = Vec::new();
        let sizes = [1usize, 2, 3, 5, 8, 13, 64, 1000, 7];
        let mut i = 0;
        let mut k = 0;
        while i < input.len() {
            let n = sizes[k % sizes.len()].min(input.len() - i);
            f.process_block(&input[i..i + n], &mut got);
            i += n;
            k += 1;
        }
        assert_eq!(got.len(), reference.len());
        for (a, b) in got.iter().zip(reference.iter()) {
            assert_eq!(
                (a.re.to_bits(), a.im.to_bits()),
                (b.re.to_bits(), b.im.to_bits())
            );
        }
    }

    #[test]
    #[should_panic(expected = "at least one tap")]
    fn empty_taps_rejected() {
        // Previously this constructed fine and panicked (index out of bounds /
        // divide by zero) on the first `push`.
        let _ = ComplexFir::new(Vec::new());
    }
}
