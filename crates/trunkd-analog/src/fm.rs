// SPDX-License-Identifier: GPL-3.0-or-later
//! FM chain: channel filter, polar discriminator, audio and sub-audio
//! filters, de-emphasis, 25 k -> 8 k resampler.
//!
//! Everything here runs at [`INPUT_RATE_HZ`] (25 ksps) except the sub-audio
//! output ([`SUBAUDIO_RATE_HZ`]) and the resampler output
//! ([`AUDIO_RATE_HZ`]). Discriminator output is in *normalised* units: ±1.0
//! is the channel's [`Bandwidth::max_deviation_hz`].
//!
//! The pieces are allocation-free after construction and stream sample by
//! sample, so the engine can feed them blocks of any length.

use num_complex::Complex;
use trunkd_p25::dsp::filters::{kaiser_lowpass_taps, ComplexFir};

use crate::{Bandwidth, AUDIO_RATE_HZ, INPUT_RATE_HZ, SUBAUDIO_RATE_HZ};

/// Butterworth 4th order as two biquads: their quality factors.
const BUTTER4_Q: [f32; 2] = [0.541_196_1, 1.306_563];

/// Butterworth 2nd order quality factor.
const BUTTER2_Q: f32 = std::f32::consts::FRAC_1_SQRT_2;

/// Channel filter stopband attenuation, dB.
const CHANNEL_ATTEN_DB: f64 = 75.0;

/// Channel filter transition width, Hz (passband edge to stopband edge).
const CHANNEL_TRANSITION_HZ: f64 = 2_500.0;

/// Values this small in an IIR state are flushed to zero (avoids denormal
/// arithmetic, which is very slow on x86, while the input is silent).
const DENORMAL_FLUSH: f32 = 1e-25;

/// Taps of the post-bank channel filter for `bw` at 25 ksps: Kaiser low-pass,
/// passband [`Bandwidth::passband_hz`], stopband 2.5 kHz beyond it, 75 dB.
/// About 48 taps for a 12.5 kHz channel.
pub fn channel_filter_taps(bw: Bandwidth) -> Vec<f32> {
    let pass = f64::from(bw.passband_hz());
    kaiser_lowpass_taps(
        f64::from(INPUT_RATE_HZ),
        pass,
        pass + CHANNEL_TRANSITION_HZ,
        CHANNEL_ATTEN_DB,
    )
}

/// The post-bank channel filter (complex FIR, real taps).
pub struct ChannelFilter {
    fir: ComplexFir,
}

impl ChannelFilter {
    /// Filter for a channel of bandwidth `bw`.
    pub fn new(bw: Bandwidth) -> Self {
        ChannelFilter {
            fir: ComplexFir::new(channel_filter_taps(bw)),
        }
    }

    /// Number of taps.
    pub fn num_taps(&self) -> usize {
        self.fir.num_taps()
    }

    /// Filter one sample.
    #[inline]
    pub fn push(&mut self, x: Complex<f32>) -> Complex<f32> {
        self.fir.push(x)
    }
}

/// Polar discriminator: `arg(x[n] · conj(x[n-1]))`, scaled so a deviation of
/// `max_deviation_hz` gives ±1.0.
#[derive(Clone, Debug)]
pub struct Discriminator {
    prev: Complex<f32>,
    scale: f32,
}

impl Discriminator {
    /// Discriminator at 25 ksps with full scale `max_deviation_hz`.
    pub fn new(max_deviation_hz: f32) -> Self {
        let rad_per_sample = std::f32::consts::TAU * max_deviation_hz / INPUT_RATE_HZ as f32;
        Discriminator {
            prev: Complex::new(0.0, 0.0),
            scale: 1.0 / rad_per_sample,
        }
    }

    /// Instantaneous frequency of `x`, normalised (±1.0 = full scale).
    #[inline]
    pub fn push(&mut self, x: Complex<f32>) -> f32 {
        let p = self.prev;
        self.prev = x;
        // x · conj(p)
        let re = x.re * p.re + x.im * p.im;
        let im = x.im * p.re - x.re * p.im;
        im.atan2(re) * self.scale
    }
}

/// Biquad section (RBJ cookbook), transposed direct form II.
#[derive(Clone, Copy, Debug)]
pub struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    z1: f32,
    z2: f32,
}

impl Biquad {
    fn from_coeffs(b: [f64; 3], a: [f64; 3]) -> Self {
        Biquad {
            b0: (b[0] / a[0]) as f32,
            b1: (b[1] / a[0]) as f32,
            b2: (b[2] / a[0]) as f32,
            a1: (a[1] / a[0]) as f32,
            a2: (a[2] / a[0]) as f32,
            z1: 0.0,
            z2: 0.0,
        }
    }

    fn w0(fs: f32, f0: f32) -> (f64, f64) {
        let w0 = std::f64::consts::TAU * f64::from(f0) / f64::from(fs);
        (w0.cos(), w0.sin())
    }

    /// Low-pass at `f0` Hz, quality `q`, sample rate `fs`.
    pub fn lowpass(fs: f32, f0: f32, q: f32) -> Self {
        let (c, s) = Self::w0(fs, f0);
        let alpha = s / (2.0 * f64::from(q));
        Self::from_coeffs(
            [(1.0 - c) / 2.0, 1.0 - c, (1.0 - c) / 2.0],
            [1.0 + alpha, -2.0 * c, 1.0 - alpha],
        )
    }

    /// High-pass at `f0` Hz, quality `q`, sample rate `fs`.
    pub fn highpass(fs: f32, f0: f32, q: f32) -> Self {
        let (c, s) = Self::w0(fs, f0);
        let alpha = s / (2.0 * f64::from(q));
        Self::from_coeffs(
            [(1.0 + c) / 2.0, -(1.0 + c), (1.0 + c) / 2.0],
            [1.0 + alpha, -2.0 * c, 1.0 - alpha],
        )
    }

    /// Filter one sample.
    #[inline]
    pub fn push(&mut self, x: f32) -> f32 {
        let y = self.b0 * x + self.z1;
        self.z1 = self.b1 * x - self.a1 * y + self.z2;
        self.z2 = self.b2 * x - self.a2 * y;
        y
    }

    /// Zero state values too small to matter (below 1e-25): avoids slow
    /// denormal arithmetic on x86 while the input is silent.
    pub fn flush_denormals(&mut self) {
        if self.z1.abs() < DENORMAL_FLUSH {
            self.z1 = 0.0;
        }
        if self.z2.abs() < DENORMAL_FLUSH {
            self.z2 = 0.0;
        }
    }
}

/// A cascade of `N` biquads.
#[derive(Clone, Copy, Debug)]
pub struct Cascade<const N: usize>(pub [Biquad; N]);

impl<const N: usize> Cascade<N> {
    /// Filter one sample through every section.
    #[inline]
    pub fn push(&mut self, mut x: f32) -> f32 {
        for s in self.0.iter_mut() {
            x = s.push(x);
        }
        x
    }

    /// See [`Biquad::flush_denormals`].
    pub fn flush_denormals(&mut self) {
        for s in self.0.iter_mut() {
            s.flush_denormals();
        }
    }
}

/// 4th-order Butterworth low-pass at `f0`.
pub fn butter4_lowpass(fs: f32, f0: f32) -> Cascade<2> {
    Cascade(BUTTER4_Q.map(|q| Biquad::lowpass(fs, f0, q)))
}

/// 4th-order Butterworth high-pass at `f0`.
pub fn butter4_highpass(fs: f32, f0: f32) -> Cascade<2> {
    Cascade(BUTTER4_Q.map(|q| Biquad::highpass(fs, f0, q)))
}

/// 2nd-order Butterworth low-pass at `f0`.
pub fn butter2_lowpass(fs: f32, f0: f32) -> Biquad {
    Biquad::lowpass(fs, f0, BUTTER2_Q)
}

/// One-pole de-emphasis with time constant `tau_us`, gain normalised to 0 dB
/// at 1 kHz (the land-mobile reference frequency), so voice keeps its level
/// whether de-emphasis is on or off.
#[derive(Clone, Copy, Debug)]
pub struct Deemphasis {
    a: f32,
    gain: f32,
    y: f32,
}

impl Deemphasis {
    /// De-emphasis at sample rate `fs`; `None` when `tau_us == 0`
    /// (disabled).
    pub fn new(fs: f32, tau_us: u32) -> Option<Self> {
        if tau_us == 0 {
            return None;
        }
        let tau = f64::from(tau_us) * 1e-6;
        let a = 1.0 - (-1.0 / (f64::from(fs) * tau)).exp();
        // |H(1 kHz)| of y[n] = y[n-1] + a (x[n] - y[n-1]).
        let w = std::f64::consts::TAU * 1000.0 / f64::from(fs);
        let den = Complex::new(1.0 - (1.0 - a) * w.cos(), (1.0 - a) * w.sin());
        let h = a / den.norm();
        Some(Deemphasis {
            a: a as f32,
            gain: (1.0 / h) as f32,
            y: 0.0,
        })
    }

    /// Filter one sample.
    #[inline]
    pub fn push(&mut self, x: f32) -> f32 {
        self.y += self.a * (x - self.y);
        self.y * self.gain
    }

    /// See [`Biquad::flush_denormals`].
    pub fn flush_denormals(&mut self) {
        if self.y.abs() < DENORMAL_FLUSH {
            self.y = 0.0;
        }
    }
}

/// Second-order sections of the voice high-pass: `[b0, b1, b2, a1, a2]`
/// (`a0 = 1`), at 25 ksps. An 8th-order elliptic high-pass, passband edge
/// 300 Hz with 0.1 dB ripple, stopband ≥ 60 dB from 254.1 Hz (the highest
/// CTCSS tone) down to DC; gain normalised to 0 dB at 1 kHz. Designed with
/// `scipy.signal.ellip(8, 0.1, 60, 300, btype="highpass", fs=25000,
/// output="sos")`, first section scaled by 1/|H(1 kHz)|.
const VOICE_HPF_SOS: [[f64; 5]; 4] = [
    [
        0.841_413_029_070_883_3,
        -1.682_597_286_492_18,
        0.841_413_029_070_883_7,
        -1.771_053_815_466_473_4,
        0.789_677_226_602_223_6,
    ],
    [
        1.0,
        -1.998_151_517_126_98,
        1.000_000_000_000_000_4,
        -1.929_813_190_785_404_1,
        0.938_709_224_289_167_6,
    ],
    [
        1.0,
        -1.996_684_829_156_401_9,
        0.999_999_999_999_999_7,
        -1.975_087_065_437_595,
        0.981_276_973_478_553,
    ],
    [
        1.0,
        -1.995_995_436_377_59,
        0.999_999_999_999_999_8,
        -1.990_131_439_760_763,
        0.995_601_647_478_497_2,
    ],
];

// The coefficients above are for this rate only.
const _: () = assert!(INPUT_RATE_HZ == 25_000);

/// One sample through a cascade of `f64` second-order sections
/// (`[b0, b1, b2, a1, a2]`, transposed direct form II state `[z1, z2]`).
#[inline]
fn sos64_push<const N: usize>(coef: &[[f64; 5]; N], state: &mut [[f64; 2]; N], x: f32) -> f32 {
    let mut v = f64::from(x);
    for (c, z) in coef.iter().zip(state.iter_mut()) {
        let y = c[0] * v + z[0];
        z[0] = c[1] * v - c[3] * y + z[1];
        z[1] = c[2] * v - c[4] * y;
        v = y;
    }
    v as f32
}

/// See [`Biquad::flush_denormals`].
fn sos64_flush_denormals<const N: usize>(state: &mut [[f64; 2]; N]) {
    for z in state.iter_mut().flatten() {
        if z.abs() < f64::from(DENORMAL_FLUSH) {
            *z = 0.0;
        }
    }
}

/// Voice high-pass of the audio path: takes out CTCSS tones and DCS before
/// the audio is recorded.
///
/// A 4th-order Butterworth at 300 Hz was not enough: de-emphasis (0 dB at
/// 1 kHz) lifts everything below 300 Hz by 9–14 dB, so a 254.1 Hz tone came
/// out *above* a 1 kHz tone of the same deviation, and 192.8 Hz only ~4 dB
/// below it — an audible hum. The ordering of the filters does not matter
/// (they are linear), and a Butterworth would need ~36th order to drop
/// 254.1 Hz by 49 dB while staying within 1.5 dB at 300 Hz; a notch at the
/// configured tone would miss carrier-squelch channels, which hear any tone.
/// So the high-pass is elliptic: ≥ 60 dB down at every CTCSS tone and
/// across the DCS band, flat within 0.1 dB from 300 Hz up. With de-emphasis
/// every sub-audio frequency ends at least 46 dB below a 1 kHz tone of the
/// same deviation. Its poles sit close to the unit circle (radius 0.998), so
/// it runs in `f64`: four sections per input sample, negligible next to the
/// channel filter.
#[derive(Clone, Copy, Debug)]
pub struct VoiceHighpass {
    /// Per section: `[b0, b1, b2, a1, a2]`.
    coef: [[f64; 5]; 4],
    /// Per section: transposed direct form II state `[z1, z2]`.
    state: [[f64; 2]; 4],
}

impl VoiceHighpass {
    /// The filter at [`INPUT_RATE_HZ`], with empty state.
    pub fn new() -> Self {
        VoiceHighpass {
            coef: VOICE_HPF_SOS,
            state: [[0.0; 2]; 4],
        }
    }

    /// Filter one sample.
    #[inline]
    pub fn push(&mut self, x: f32) -> f32 {
        sos64_push(&self.coef, &mut self.state, x)
    }

    /// See [`Biquad::flush_denormals`].
    pub fn flush_denormals(&mut self) {
        sos64_flush_denormals(&mut self.state);
    }
}

impl Default for VoiceHighpass {
    fn default() -> Self {
        VoiceHighpass::new()
    }
}

/// Second-order sections of the voice low-pass: `[b0, b1, b2, a1, a2]`
/// (`a0 = 1`), at 25 ksps. A 6th-order elliptic low-pass, passband edge
/// 3.2 kHz with 0.1 dB ripple, stopband ≥ 45 dB from 4 kHz up to Nyquist;
/// gain normalised to 0 dB at 1 kHz. Designed with
/// `scipy.signal.ellip(6, 0.1, 45, 3200, btype="lowpass", fs=25000,
/// output="sos")`, first section scaled by 1/|H(1 kHz)|.
const VOICE_LPF_SOS: [[f64; 5]; 3] = [
    [
        0.016_080_934_718_983_683,
        0.015_564_098_489_428_213,
        0.016_080_934_718_983_687,
        -1.207_666_270_015_192_1,
        0.413_924_821_342_932_4,
    ],
    [
        1.0,
        -0.714_563_824_498_278_9,
        1.0,
        -1.237_770_874_604_212_2,
        0.687_183_537_710_095,
    ],
    [
        1.0,
        -1.064_363_548_331_712_2,
        1.000_000_000_000_000_2,
        -1.287_547_191_001_293_4,
        0.913_857_655_856_373_4,
    ],
];

/// Voice low-pass of the audio path: bounds the audio band before the
/// 25 k -> 8 k resampler.
///
/// It replaces a 2nd-order Butterworth at 3 kHz, which was −3 dB at 3 kHz
/// (so the chain left the de-emphasis curve by more than 1.5 dB above
/// 2.4 kHz) and only ~7 dB down at 4 kHz, leaving the resampler alone to
/// stop content that folds back into the 8 ksps output. A Butterworth
/// cannot be flat to 3 kHz and 40 dB down at 4 kHz at any practical order,
/// so this one is elliptic: flat within 0.1 dB to 3.2 kHz (−3 dB near
/// 3.35 kHz) and ≥ 45 dB down from 4 kHz up, before the resampler's own
/// filter adds its stopband. Runs in `f64` like [`VoiceHighpass`]: three
/// sections per input sample.
#[derive(Clone, Copy, Debug)]
pub struct VoiceLowpass {
    /// Per section: `[b0, b1, b2, a1, a2]`.
    coef: [[f64; 5]; 3],
    /// Per section: transposed direct form II state `[z1, z2]`.
    state: [[f64; 2]; 3],
}

impl VoiceLowpass {
    /// The filter at [`INPUT_RATE_HZ`], with empty state.
    pub fn new() -> Self {
        VoiceLowpass {
            coef: VOICE_LPF_SOS,
            state: [[0.0; 2]; 3],
        }
    }

    /// Filter one sample.
    #[inline]
    pub fn push(&mut self, x: f32) -> f32 {
        sos64_push(&self.coef, &mut self.state, x)
    }

    /// See [`Biquad::flush_denormals`].
    pub fn flush_denormals(&mut self) {
        sos64_flush_denormals(&mut self.state);
    }
}

impl Default for VoiceLowpass {
    fn default() -> Self {
        VoiceLowpass::new()
    }
}

/// Rational resampler 25 ksps -> 8 ksps (L = 8, M = 25), polyphase FIR.
///
/// Prototype: Kaiser low-pass at 200 kHz (the virtual upsampled rate),
/// passband 3.2 kHz, stopband 4.8 kHz, 70 dB. Content between 4 and 4.8 kHz
/// folds to 3.2–4 kHz, above the audio band, and the audio path's voice
/// low-pass ([`VoiceLowpass`]) has already taken it ≥ 45 dB down.
pub struct Resampler25to8 {
    /// Phase-major taps: `taps[p * per_phase + j] = h[p + j L] · L`.
    taps: Vec<f32>,
    per_phase: usize,
    /// Mirrored delay line (`2 * per_phase`), newest at `pos + per_phase - 1`.
    buf: Vec<f32>,
    pos: usize,
    /// Virtual time of the next output relative to the newest input, in
    /// upsampled samples.
    rel: usize,
}

impl Resampler25to8 {
    /// Interpolation factor.
    pub const L: usize = 8;
    /// Decimation factor.
    pub const M: usize = 25;

    /// New resampler with an empty history.
    pub fn new() -> Self {
        debug_assert_eq!(
            INPUT_RATE_HZ as usize * Self::L,
            AUDIO_RATE_HZ as usize * Self::M
        );
        let fs_up = f64::from(INPUT_RATE_HZ) * Self::L as f64;
        let proto = kaiser_lowpass_taps(fs_up, 3_200.0, 4_800.0, 70.0);
        let per_phase = proto.len().div_ceil(Self::L);
        let mut taps = vec![0.0f32; per_phase * Self::L];
        for (i, &h) in proto.iter().enumerate() {
            let (p, j) = (i % Self::L, i / Self::L);
            taps[p * per_phase + j] = h * Self::L as f32;
        }
        Resampler25to8 {
            taps,
            per_phase,
            buf: vec![0.0; 2 * per_phase],
            pos: 0,
            rel: 0,
        }
    }

    /// Taps per polyphase branch.
    pub fn taps_per_phase(&self) -> usize {
        self.per_phase
    }

    /// Push one input sample; calls `out` for each output sample it
    /// completes (8 outputs per 25 inputs, at most one per input).
    #[inline]
    pub fn push(&mut self, x: f32, mut out: impl FnMut(f32)) {
        let w = self.per_phase;
        self.buf[self.pos] = x;
        self.buf[self.pos + w] = x;
        self.pos += 1;
        if self.pos == w {
            self.pos = 0;
        }
        while self.rel < Self::L {
            let p = self.rel;
            let h = &self.taps[p * w..(p + 1) * w];
            // Newest sample at pos + w - 1; h[j] multiplies x[k - j].
            let newest = self.pos + w - 1;
            let hist = &self.buf[newest + 1 - w..=newest];
            let mut acc = 0.0f32;
            for (hj, xj) in h.iter().zip(hist.iter().rev()) {
                acc += hj * xj;
            }
            out(acc);
            self.rel += Self::M;
        }
        self.rel -= Self::L;
    }
}

impl Default for Resampler25to8 {
    fn default() -> Self {
        Resampler25to8::new()
    }
}

/// Audio path: 300 Hz voice high-pass ([`VoiceHighpass`], removes CTCSS/DCS)
/// -> optional de-emphasis -> voice low-pass ([`VoiceLowpass`], flat to
/// 3.2 kHz, ≥ 45 dB down from 4 kHz) -> 25 k -> 8 k resampler -> soft
/// limiter -> i16.
pub struct AudioChain {
    hpf: VoiceHighpass,
    deemph: Option<Deemphasis>,
    lpf: VoiceLowpass,
    resampler: Resampler25to8,
}

/// PCM full scale per normalised discriminator unit: a full-scale deviation
/// at 1 kHz comes out at 0.8 of i16 full scale.
pub const PCM_GAIN: f32 = 0.8;

/// The soft limiter is linear up to this fraction of full scale.
const LIMIT_KNEE: f32 = 0.8;

/// Soft limiter: linear to 0.8 of full scale, then a tanh knee that approaches
/// full scale; returns i16.
#[inline]
pub fn soft_limit_i16(x: f32) -> i16 {
    let a = x.abs();
    let y = if a <= LIMIT_KNEE {
        a
    } else {
        let span = 1.0 - LIMIT_KNEE;
        LIMIT_KNEE + span * ((a - LIMIT_KNEE) / span).tanh()
    };
    (y.copysign(x) * i16::MAX as f32) as i16
}

impl AudioChain {
    /// Audio path at 25 ksps; `deemphasis_us == 0` disables de-emphasis.
    pub fn new(deemphasis_us: u32) -> Self {
        let fs = INPUT_RATE_HZ as f32;
        AudioChain {
            hpf: VoiceHighpass::new(),
            deemph: Deemphasis::new(fs, deemphasis_us),
            lpf: VoiceLowpass::new(),
            resampler: Resampler25to8::new(),
        }
    }

    /// Filtered audio before resampling (for response tests).
    #[inline]
    pub fn filter(&mut self, d: f32) -> f32 {
        let mut x = self.hpf.push(d);
        if let Some(de) = self.deemph.as_mut() {
            x = de.push(x);
        }
        self.lpf.push(x)
    }

    /// Push one discriminator sample; appends 0 or 1 PCM samples to `out`.
    #[inline]
    pub fn push(&mut self, d: f32, out: &mut Vec<i16>) {
        let x = self.filter(d);
        self.resampler
            .push(x, |y| out.push(soft_limit_i16(y * PCM_GAIN)));
    }

    /// See [`Biquad::flush_denormals`].
    pub fn flush_denormals(&mut self) {
        self.hpf.flush_denormals();
        if let Some(d) = self.deemph.as_mut() {
            d.flush_denormals();
        }
        self.lpf.flush_denormals();
    }
}

/// Decimation from the input rate to the sub-audio rate.
pub const SUBAUDIO_DECIMATION: usize = (INPUT_RATE_HZ / SUBAUDIO_RATE_HZ) as usize;

/// Sub-audio path: 300 Hz low-pass (4th-order Butterworth) -> decimate by 10
/// -> 1 Hz DC tracker (one-pole high-pass). Output at 2.5 ksps in normalised
/// units, for the CTCSS/DCS detectors.
#[derive(Clone, Copy, Debug)]
pub struct SubaudioChain {
    lpf: Cascade<2>,
    phase: usize,
    dc: f32,
    dc_alpha: f32,
}

impl SubaudioChain {
    /// New sub-audio path.
    pub fn new() -> Self {
        let fs = INPUT_RATE_HZ as f32;
        let dc_alpha = 1.0 - (-std::f32::consts::TAU * 1.0 / SUBAUDIO_RATE_HZ as f32).exp();
        SubaudioChain {
            lpf: butter4_lowpass(fs, 300.0),
            phase: 0,
            dc: 0.0,
            dc_alpha,
        }
    }

    /// Push one discriminator sample; every 10th call returns a sub-audio
    /// sample.
    #[inline]
    pub fn push(&mut self, d: f32) -> Option<f32> {
        let y = self.lpf.push(d);
        self.phase += 1;
        if self.phase < SUBAUDIO_DECIMATION {
            return None;
        }
        self.phase = 0;
        self.dc += self.dc_alpha * (y - self.dc);
        Some(y - self.dc)
    }

    /// See [`Biquad::flush_denormals`].
    pub fn flush_denormals(&mut self) {
        self.lpf.flush_denormals();
        if self.dc.abs() < DENORMAL_FLUSH {
            self.dc = 0.0;
        }
    }
}

impl Default for SubaudioChain {
    fn default() -> Self {
        SubaudioChain::new()
    }
}

/// Discriminator noise meter: power of the discriminator output in the
/// 6–10 kHz band (4th-order high-pass at 6 kHz, 2nd-order low-pass at
/// 10 kHz), above any modulation. A real FM carrier leaves this band
/// quiet; noise, splatter and intermod with no FM structure fill it.
#[derive(Clone, Copy, Debug)]
pub struct NoiseMeter {
    hpf: Cascade<2>,
    lpf: Biquad,
    acc: f32,
    n: u32,
}

impl NoiseMeter {
    /// New meter at 25 ksps.
    pub fn new() -> Self {
        let fs = INPUT_RATE_HZ as f32;
        NoiseMeter {
            hpf: butter4_highpass(fs, 6_000.0),
            lpf: butter2_lowpass(fs, 10_000.0),
            acc: 0.0,
            n: 0,
        }
    }

    /// Accumulate one discriminator sample.
    #[inline]
    pub fn push(&mut self, d: f32) {
        let y = self.lpf.push(self.hpf.push(d));
        self.acc += y * y;
        self.n += 1;
    }

    /// RMS since the last call (normalised units), and restart.
    pub fn take_rms(&mut self) -> f32 {
        let r = if self.n == 0 {
            0.0
        } else {
            (self.acc / self.n as f32).sqrt()
        };
        self.acc = 0.0;
        self.n = 0;
        self.hpf.flush_denormals();
        self.lpf.flush_denormals();
        r
    }
}

impl Default for NoiseMeter {
    fn default() -> Self {
        NoiseMeter::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FS: f32 = INPUT_RATE_HZ as f32;

    /// Amplitude of the `hz` component of `x` at `fs` (single-bin DFT).
    fn amp(x: &[f32], hz: f32, fs: f32) -> f32 {
        let w = std::f32::consts::TAU * hz / fs;
        let (mut re, mut im) = (0.0f64, 0.0f64);
        for (n, &v) in x.iter().enumerate() {
            let ph = w as f64 * n as f64;
            re += f64::from(v) * ph.cos();
            im -= f64::from(v) * ph.sin();
        }
        (2.0 * (re * re + im * im).sqrt() / x.len() as f64) as f32
    }

    fn sine(hz: f32, n: usize) -> Vec<f32> {
        (0..n)
            .map(|i| (std::f32::consts::TAU * hz * i as f32 / FS).sin())
            .collect()
    }

    fn db(x: f32) -> f32 {
        20.0 * x.log10()
    }

    /// Gain of a 25 ksps real filter at `hz`, after settling.
    fn gain(mut f: impl FnMut(f32) -> f32, hz: f32) -> f32 {
        let x = sine(hz, 25_000);
        let y: Vec<f32> = x.iter().map(|&v| f(v)).collect();
        amp(&y[12_500..], hz, FS)
    }

    fn resample(x: &[f32]) -> Vec<f32> {
        let mut r = Resampler25to8::new();
        let mut out = Vec::new();
        for &v in x {
            r.push(v, |y| out.push(y));
        }
        out
    }

    #[test]
    fn resampler_rate_and_passband() {
        let x = sine(1_000.0, 25_000);
        let y = resample(&x);
        assert_eq!(y.len(), 8_000);
        let tail = &y[4_000..];
        let g = amp(tail, 1_000.0, AUDIO_RATE_HZ as f32);
        assert!((g - 1.0).abs() < 0.01, "1 kHz gain {g}");
        for hz in [300.0, 2_000.0, 3_000.0] {
            let y = resample(&sine(hz, 25_000));
            let g = amp(&y[4_000..], hz, AUDIO_RATE_HZ as f32);
            assert!((g - 1.0).abs() < 0.02, "{hz} Hz gain {g}");
        }
    }

    #[test]
    fn resampler_rejects_aliases() {
        // 5 kHz and 6 kHz would fold to 3 kHz and 2 kHz in the 8 ksps output.
        for (hz, alias) in [(5_000.0, 3_000.0), (6_000.0, 2_000.0), (10_000.0, 2_000.0)] {
            let y = resample(&sine(hz, 25_000));
            let a = amp(&y[4_000..], alias, AUDIO_RATE_HZ as f32);
            assert!(db(a) < -65.0, "{hz} Hz alias at {alias} Hz: {} dB", db(a));
        }
    }

    #[test]
    fn resampler_is_block_invariant() {
        // Streaming: one output per 25/8 inputs regardless of how input is fed.
        let x = sine(700.0, 2_500);
        let whole = resample(&x);
        let mut r = Resampler25to8::new();
        let mut parts = Vec::new();
        for chunk in x.chunks(37) {
            for &v in chunk {
                r.push(v, |y| parts.push(y));
            }
        }
        assert_eq!(whole, parts);
        assert_eq!(whole.len(), 800);
    }

    #[test]
    fn audio_highpass_removes_subaudio() {
        let mut h = butter4_highpass(FS, 300.0);
        assert!(db(gain(|v| h.push(v), 100.0)) < -35.0);
        let mut h = butter4_highpass(FS, 300.0);
        assert!(db(gain(|v| h.push(v), 250.0)) < -5.0);
        let mut h = butter4_highpass(FS, 300.0);
        assert!(db(gain(|v| h.push(v), 1_000.0)).abs() < 0.1);
    }

    #[test]
    fn voice_highpass_response() {
        // >= 58 dB down at every CTCSS tone and anywhere below 254.1 Hz (the
        // DCS band included); flat within 0.15 dB from 300 Hz to 3 kHz.
        let g = |hz: f32| {
            let mut h = VoiceHighpass::new();
            db(gain(|v| h.push(v), hz))
        };
        for &tenths in crate::tone::CTCSS_TONES.iter() {
            let hz = f32::from(tenths) / 10.0;
            assert!(g(hz) < -58.0, "{hz} Hz: {} dB", g(hz));
        }
        for hz in (10..=254).step_by(4) {
            assert!(g(hz as f32) < -58.0, "{hz} Hz: {} dB", g(hz as f32));
        }
        for hz in [300.0, 320.0, 400.0, 600.0, 1_000.0, 2_000.0, 3_000.0] {
            assert!(g(hz).abs() < 0.15, "{hz} Hz: {} dB", g(hz));
        }
    }

    /// PCM amplitude (fraction of full scale) of `hz` at normalised input
    /// amplitude `a` through the whole audio path, measured over the last
    /// second of two (Hann-windowed DFT, so a strong neighbour cannot leak in).
    fn pcm_amp(deemphasis_us: u32, hz: f32, a: f32) -> f32 {
        pcm_amp_at(deemphasis_us, hz, hz, a)
    }

    /// As [`pcm_amp`], but measures the PCM at `at_hz` (an alias) for an
    /// input tone at `hz`.
    fn pcm_amp_at(deemphasis_us: u32, hz: f32, at_hz: f32, a: f32) -> f32 {
        let mut chain = AudioChain::new(deemphasis_us);
        let mut pcm = Vec::new();
        for v in sine(hz, 50_000) {
            chain.push(a * v, &mut pcm);
        }
        let tail = &pcm[pcm.len() - 8_000..];
        let n = tail.len() as f32;
        let w: Vec<f32> = tail
            .iter()
            .enumerate()
            .map(|(i, &v)| {
                let hann = 0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / n).cos();
                hann * f32::from(v) / i16::MAX as f32
            })
            .collect();
        // Hann window coherent gain is 0.5.
        amp(&w, at_hz, AUDIO_RATE_HZ as f32) / 0.5
    }

    #[test]
    fn pcm_has_no_ctcss_or_dcs_hum() {
        // Every CTCSS tone, and the DCS band (its 134.4 bps main lobe and up
        // to the highest tone), at least 35 dB below a 1 kHz tone of the same
        // deviation, with the default de-emphasis and with it off. Measured
        // margin: >= 46 dB with de-emphasis, >= 58 dB without.
        let a = 0.3;
        for deemph in [750, 0] {
            let reference = pcm_amp(deemph, 1_000.0, a);
            assert!(
                (reference - a * PCM_GAIN).abs() < 0.01,
                "1 kHz reference {reference}"
            );
            let mut worst = f32::MIN;
            let tones = crate::tone::CTCSS_TONES
                .iter()
                .map(|&t| f32::from(t) / 10.0);
            let dcs_band = (20..=250).step_by(10).map(|hz| hz as f32);
            for hz in tones.chain(dcs_band) {
                let rel = db(pcm_amp(deemph, hz, a) / reference);
                worst = worst.max(rel);
                assert!(rel <= -35.0, "de-emphasis {deemph} µs, {hz} Hz: {rel} dB");
            }
            eprintln!("de-emphasis {deemph} µs: worst sub-audio {worst:.1} dB re 1 kHz");
        }
    }

    #[test]
    fn voice_band_is_not_hurt_by_the_highpass() {
        // With de-emphasis as intended: the chain equals de-emphasis x the
        // voice low-pass within 0.2 dB from 300 Hz to 3 kHz (the high-pass
        // adds nothing there).
        for hz in [
            300.0, 350.0, 500.0, 1_000.0, 1_500.0, 2_000.0, 2_400.0, 3_000.0,
        ] {
            let mut chain = AudioChain::new(750);
            let total = db(gain(|v| chain.filter(v), hz));
            let mut de = Deemphasis::new(FS, 750).unwrap();
            let de_db = db(gain(|v| de.push(v), hz));
            let mut lpf = VoiceLowpass::new();
            let lpf_db = db(gain(|v| lpf.push(v), hz));
            assert!(
                (total - de_db - lpf_db).abs() < 0.2,
                "{hz} Hz: {total} vs {de_db} + {lpf_db}"
            );
        }
    }

    #[test]
    fn audio_chain_follows_the_deemphasis_curve() {
        // 300 Hz–3 kHz: within ±1.5 dB of the de-emphasis curve with the
        // default 750 µs, and flat within ±1.5 dB with de-emphasis off.
        let hz_list = (300..=3_000).step_by(100).map(|hz| hz as f32);
        for deemph in [750, 0] {
            let mut worst = 0.0f32;
            for hz in hz_list.clone() {
                let mut chain = AudioChain::new(deemph);
                let total = db(gain(|v| chain.filter(v), hz));
                let curve = match Deemphasis::new(FS, deemph) {
                    Some(mut de) => db(gain(|v| de.push(v), hz)),
                    None => 0.0,
                };
                let dev = total - curve;
                if dev.abs() > worst.abs() {
                    worst = dev;
                }
                assert!(
                    dev.abs() <= 1.5,
                    "de-emphasis {deemph} µs, {hz} Hz: {dev} dB"
                );
            }
            eprintln!("de-emphasis {deemph} µs: worst deviation 300 Hz–3 kHz {worst:.2} dB");
        }
    }

    #[test]
    fn audio_lowpass_rejects_above_4khz() {
        // At least 40 dB down from 4 kHz to Nyquist before the resampler, so
        // nothing can fold into the 8 ksps output.
        let mut worst = f32::MIN;
        for hz in (4_000..=12_400).step_by(100) {
            let mut chain = AudioChain::new(0);
            let g = db(gain(|v| chain.filter(v), hz as f32));
            worst = worst.max(g);
            assert!(g <= -40.0, "{hz} Hz: {g} dB");
        }
        eprintln!("audio low-pass: worst 4–12.4 kHz {worst:.1} dB");
    }

    #[test]
    fn pcm_has_no_aliases() {
        // 4.5–6 kHz at the input would fold to 3.5–2 kHz in the 8 ksps
        // output; through the whole path each alias is far below a 1 kHz
        // tone of the same deviation.
        let a = 0.3;
        for deemph in [750, 0] {
            let reference = pcm_amp(deemph, 1_000.0, a);
            let mut worst = f32::MIN;
            for hz in [4_500.0, 5_000.0, 5_500.0, 6_000.0] {
                let alias = AUDIO_RATE_HZ as f32 - hz;
                let rel = db(pcm_amp_at(deemph, hz, alias, a) / reference);
                worst = worst.max(rel);
                assert!(
                    rel <= -60.0,
                    "de-emphasis {deemph} µs, {hz} Hz -> {alias} Hz: {rel} dB"
                );
            }
            eprintln!("de-emphasis {deemph} µs: worst alias {worst:.1} dB re 1 kHz");
        }
    }

    #[test]
    fn audio_chain_response() {
        // No de-emphasis: 0 dB at 1 kHz, within 0.2 dB at 3 kHz, −3 dB
        // between 3.2 and 3.5 kHz, far down above.
        let g = |hz: f32| {
            let mut a = AudioChain::new(0);
            db(gain(|v| a.filter(v), hz))
        };
        assert!(g(1_000.0).abs() < 0.05, "1 kHz {} dB", g(1_000.0));
        assert!(g(3_000.0).abs() < 0.2, "3 kHz {} dB", g(3_000.0));
        assert!(g(3_200.0) > -3.0 && g(3_500.0) < -3.0);
        assert!(g(8_000.0) < -40.0);
    }

    #[test]
    fn deemphasis_response() {
        // 750 µs: corner 212 Hz, -6 dB/octave; 0 dB at 1 kHz by design.
        let g = |hz: f32| {
            let mut d = Deemphasis::new(FS, 750).unwrap();
            db(gain(|v| d.push(v), hz))
        };
        assert!(g(1_000.0).abs() < 0.1);
        let expect = |hz: f32| {
            let wt = |f: f32| (1.0 + (std::f32::consts::TAU * f * 750e-6).powi(2)).sqrt();
            20.0 * (wt(1_000.0) / wt(hz)).log10()
        };
        for hz in [500.0, 2_000.0, 3_000.0] {
            assert!(
                (g(hz) - expect(hz)).abs() < 0.3,
                "{hz}: {} vs {}",
                g(hz),
                expect(hz)
            );
        }
        assert!(Deemphasis::new(FS, 0).is_none());
    }

    #[test]
    fn channel_filter_response() {
        for bw in [Bandwidth::Narrow, Bandwidth::Wide] {
            let taps = channel_filter_taps(bw);
            if bw == Bandwidth::Narrow {
                assert!(taps.len() <= 52, "{} taps", taps.len());
            }
            let resp = |hz: f32| {
                let w = std::f32::consts::TAU * hz / FS;
                let (mut re, mut im) = (0.0f32, 0.0f32);
                for (k, &t) in taps.iter().enumerate() {
                    re += t * (w * k as f32).cos();
                    im -= t * (w * k as f32).sin();
                }
                db((re * re + im * im).sqrt())
            };
            let pass = bw.passband_hz();
            assert!(resp(0.0).abs() < 0.01);
            assert!(resp(pass * 0.8).abs() < 0.1, "{bw:?} passband");
            assert!(resp(pass + 2_500.0) < -70.0, "{bw:?} stop edge");
            assert!(resp(12_499.0) < -70.0, "{bw:?} at 12.5 kHz");
            if bw == Bandwidth::Narrow {
                // A neighbour 7.5 kHz away (VHF offset channel) is far down.
                assert!(resp(7_500.0) < -70.0, "7.5 kHz: {} dB", resp(7_500.0));
            }
        }
    }

    #[test]
    fn discriminator_scale() {
        let mut d = Discriminator::new(2_500.0);
        let f = 1_000.0f32;
        let mut last = 0.0;
        for n in 0..100 {
            let ph = std::f32::consts::TAU * f * n as f32 / FS;
            last = d.push(Complex::new(ph.cos(), ph.sin()));
        }
        assert!((last - 0.4).abs() < 1e-3, "{last}");
        // Negative deviation, wide scale.
        let mut d = Discriminator::new(5_000.0);
        for n in 0..100 {
            let ph = -std::f32::consts::TAU * 2_500.0 * n as f32 / FS;
            last = d.push(Complex::new(ph.cos(), ph.sin()));
        }
        assert!((last + 0.5).abs() < 1e-3, "{last}");
    }

    #[test]
    fn subaudio_chain_passes_tones_and_drops_audio() {
        let run = |hz: f32| {
            let mut s = SubaudioChain::new();
            let x = sine(hz, 50_000);
            let y: Vec<f32> = x.iter().filter_map(|&v| s.push(v)).collect();
            assert_eq!(y.len(), 5_000);
            amp(&y[2_500..], hz, SUBAUDIO_RATE_HZ as f32)
        };
        assert!(db(run(67.0)).abs() < 0.5);
        assert!(db(run(254.1)) > -2.0);
        assert!(db(run(1_000.0)) < -40.0);
    }

    #[test]
    fn soft_limiter() {
        assert_eq!(soft_limit_i16(0.0), 0);
        assert_eq!(soft_limit_i16(0.5), (0.5 * 32767.0) as i16);
        assert!(soft_limit_i16(10.0) > 32_000);
        assert!(soft_limit_i16(-10.0) < -32_000);
        assert!(soft_limit_i16(0.9) < (0.9 * 32767.0) as i16);
    }
}
