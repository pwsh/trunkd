// SPDX-License-Identifier: GPL-3.0-or-later
//! Synthetic NBFM signals for tests (feature `testsig`).
//!
//! - [`NbfmSignal`] / [`nbfm`]: an FM modulator at [`RATE_HZ`] (25 ksps, the
//!   engine's input rate) carrying test audio plus a CTCSS tone or DCS code,
//!   with carrier offset, keying (carrier on/off windows) and AWGN at a target
//!   SNR. Deterministic for a given seed.
//! - [`place_wideband`]: upsample several 25 ksps channels to a wideband rate
//!   and mix each to its frequency around a capture centre, the way a tuner
//!   would see them, for end-to-end tests through the channel bank.
//!
//! # Conventions
//!
//! - The carrier has amplitude [`NbfmSignal::amplitude`] (default 1.0, i.e.
//!   0 dBFS); the instantaneous frequency is
//!   `offset_hz + audio_hz · audio[n] + subaudio_hz · subaudio[n]`, with both
//!   waveforms normalised to a peak of 1.0.
//! - SNR is carrier power over the noise power in a
//!   [`NOISE_REF_BANDWIDTH_HZ`] (12.5 kHz) bandwidth. The noise is white over
//!   the whole 25 kHz band, so its total power is 3 dB above that
//!   ([`noise_power_for_snr`]). `f32::INFINITY` means no noise.
//! - DCS: the code's 23-bit word ([`Tone::dcs_word`]) repeats from sample 0,
//!   bit 0 first, NRZ at 134.4 bps (`1` = positive deviation; inverted codes
//!   are complemented), smoothed by a 2nd-order 300 Hz low-pass as a
//!   transmitter's sub-audio filter would (≈4 % overshoot on bit edges). No
//!   turn-off code is sent.
//! - CTCSS: a sine starting at phase 0 on sample 0.

use std::f64::consts::TAU;
use std::ops::Range;

use num_complex::Complex;

use crate::tone::{Tone, DCS_BIT_RATE};
use crate::INPUT_RATE_HZ;

/// Sample rate of everything [`NbfmSignal`] produces, Hz.
pub const RATE_HZ: u32 = INPUT_RATE_HZ;

/// Reference bandwidth of the SNR figure, Hz.
pub const NOISE_REF_BANDWIDTH_HZ: f32 = 12_500.0;

/// Test audio (modulating signal), normalised to a peak of 1.0.
#[derive(Clone, Debug, PartialEq)]
pub enum Audio {
    /// No audio: carrier plus sub-audio only.
    Silence,
    /// A single sine at `hz`.
    Sine {
        /// Tone frequency, Hz.
        hz: f32,
    },
    /// Speech-like: four tones (350, 900, 1700, 2600 Hz) under a 4 Hz
    /// syllabic envelope.
    Voice,
    /// White noise band-limited to 300–3000 Hz (RMS ≈ 0.3, clipped at ±1).
    Noise,
}

/// Peak deviations, Hz.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Deviation {
    /// Peak deviation of the audio at full scale, Hz.
    pub audio_hz: f32,
    /// Peak deviation of the CTCSS tone / DCS stream, Hz.
    pub subaudio_hz: f32,
}

impl Deviation {
    /// Typical 12.5 kHz channel: audio ±2.0 kHz, sub-audio ±600 Hz.
    pub const NARROW: Deviation = Deviation {
        audio_hz: 2_000.0,
        subaudio_hz: 600.0,
    };
    /// Typical 25 kHz channel: audio ±4.0 kHz, sub-audio ±750 Hz.
    pub const WIDE: Deviation = Deviation {
        audio_hz: 4_000.0,
        subaudio_hz: 750.0,
    };
}

impl Default for Deviation {
    fn default() -> Self {
        Deviation::NARROW
    }
}

/// A synthetic NBFM transmission as received at the channel centre.
#[derive(Clone, Debug, PartialEq)]
pub struct NbfmSignal {
    /// Modulating audio.
    pub audio: Audio,
    /// CTCSS tone or DCS code to send; `None` = carrier squelch only.
    pub tone: Option<Tone>,
    /// Peak deviations.
    pub deviation: Deviation,
    /// Carrier offset from the channel centre, Hz.
    pub offset_hz: f32,
    /// SNR in [`NOISE_REF_BANDWIDTH_HZ`], dB; `f32::INFINITY` = noiseless.
    pub snr_db: f32,
    /// Carrier amplitude (1.0 = 0 dBFS).
    pub amplitude: f32,
    /// Sample ranges in which the carrier is on; empty = on throughout. The
    /// noise is present everywhere (so a gap is noise only, at the same level
    /// as during the transmission).
    pub keyed: Vec<Range<usize>>,
    /// Seed for the noise generators (audio noise and AWGN).
    pub seed: u64,
}

impl NbfmSignal {
    /// Narrowband deviation, no offset, no noise, keyed throughout, seed 1.
    pub fn new(audio: Audio, tone: Option<Tone>) -> Self {
        NbfmSignal {
            audio,
            tone,
            deviation: Deviation::NARROW,
            offset_hz: 0.0,
            snr_db: f32::INFINITY,
            amplitude: 1.0,
            keyed: Vec::new(),
            seed: 1,
        }
    }

    /// `n` complex samples at [`RATE_HZ`].
    pub fn generate(&self, n: usize) -> Vec<Complex<f32>> {
        let audio = audio_waveform(&self.audio, n, self.seed);
        let sub = subaudio_waveform(self.tone, n);
        let fs = f64::from(RATE_HZ);
        let mut phase = 0.0f64;
        let mut out = Vec::with_capacity(n);
        let a = self.amplitude;
        for i in 0..n {
            let on = self.keyed.is_empty() || self.keyed.iter().any(|r| r.contains(&i));
            if on {
                let (s, c) = phase.sin_cos();
                out.push(Complex::new(a * c as f32, a * s as f32));
            } else {
                out.push(Complex::new(0.0, 0.0));
            }
            let f = f64::from(self.offset_hz)
                + f64::from(self.deviation.audio_hz) * f64::from(audio[i])
                + f64::from(self.deviation.subaudio_hz) * f64::from(sub[i]);
            phase = (phase + TAU * f / fs).rem_euclid(TAU);
        }
        if self.snr_db.is_finite() {
            let power = noise_power_for_snr(self.snr_db, self.amplitude);
            add_awgn(&mut out, power, self.seed ^ 0x9E37_79B9_7F4A_7C15);
        }
        out
    }
}

/// `n` samples at [`RATE_HZ`] of `audio` + `tone` at `deviation`, carrier
/// `offset_hz` from centre, AWGN at `snr_db` (seed 1, amplitude 1, keyed
/// throughout).
pub fn nbfm(
    audio: &Audio,
    tone: Option<Tone>,
    deviation: Deviation,
    offset_hz: f32,
    snr_db: f32,
    n: usize,
) -> Vec<Complex<f32>> {
    NbfmSignal {
        deviation,
        offset_hz,
        snr_db,
        ..NbfmSignal::new(audio.clone(), tone)
    }
    .generate(n)
}

/// Total complex AWGN power (over the full 25 kHz band) that gives `snr_db`
/// in [`NOISE_REF_BANDWIDTH_HZ`] for a carrier of `amplitude`.
pub fn noise_power_for_snr(snr_db: f32, amplitude: f32) -> f32 {
    amplitude * amplitude * 10f32.powf(-snr_db / 10.0) * (RATE_HZ as f32 / NOISE_REF_BANDWIDTH_HZ)
}

/// `n` samples of complex white Gaussian noise of total power `power`.
pub fn noise(n: usize, power: f32, seed: u64) -> Vec<Complex<f32>> {
    let mut v = vec![Complex::new(0.0, 0.0); n];
    add_awgn(&mut v, power, seed);
    v
}

/// Add complex white Gaussian noise of total power `power` (`E|n|² = power`).
pub fn add_awgn(buf: &mut [Complex<f32>], power: f32, seed: u64) {
    let mut rng = Rng::new(seed);
    let sigma = (f64::from(power) / 2.0).sqrt();
    for x in buf {
        let (a, b) = rng.gaussian_pair();
        x.re += (sigma * a) as f32;
        x.im += (sigma * b) as f32;
    }
}

/// The modulating audio waveform, `n` samples at [`RATE_HZ`], peak ≤ 1.0.
pub fn audio_waveform(audio: &Audio, n: usize, seed: u64) -> Vec<f32> {
    let fs = f64::from(RATE_HZ);
    match *audio {
        Audio::Silence => vec![0.0; n],
        Audio::Sine { hz } => (0..n)
            .map(|i| (TAU * f64::from(hz) * i as f64 / fs).sin() as f32)
            .collect(),
        Audio::Voice => {
            const PARTS: [(f64, f64); 4] =
                [(350.0, 0.4), (900.0, 0.3), (1700.0, 0.2), (2600.0, 0.1)];
            (0..n)
                .map(|i| {
                    let t = i as f64 / fs;
                    let env = 0.5 + 0.5 * (TAU * 4.0 * t).sin();
                    let s: f64 = PARTS.iter().map(|&(f, w)| w * (TAU * f * t).sin()).sum();
                    (env * s) as f32
                })
                .collect()
        }
        Audio::Noise => {
            let mut rng = Rng::new(seed);
            let mut hp1 = Biquad::highpass(300.0, fs);
            let mut hp2 = Biquad::highpass(300.0, fs);
            let mut lp1 = Biquad::lowpass(3000.0, fs);
            let mut lp2 = Biquad::lowpass(3000.0, fs);
            let raw: Vec<f64> = (0..n)
                .map(|_| lp2.run(lp1.run(hp2.run(hp1.run(rng.gaussian_pair().0)))))
                .collect();
            let rms = (raw.iter().map(|x| x * x).sum::<f64>() / n.max(1) as f64).sqrt();
            let scale = if rms > 0.0 { 0.3 / rms } else { 0.0 };
            raw.iter()
                .map(|x| (x * scale).clamp(-1.0, 1.0) as f32)
                .collect()
        }
    }
}

/// The sub-audio waveform for `tone`, `n` samples at [`RATE_HZ`], peak ≈ 1.0
/// (DCS overshoots by ≈4 % on bit edges); zeros for `None`.
pub fn subaudio_waveform(tone: Option<Tone>, n: usize) -> Vec<f32> {
    let fs = f64::from(RATE_HZ);
    match tone {
        None => vec![0.0; n],
        Some(t @ Tone::Ctcss(_)) => {
            let hz = f64::from(t.ctcss_hz().unwrap_or(0.0));
            (0..n)
                .map(|i| (TAU * hz * i as f64 / fs).sin() as f32)
                .collect()
        }
        Some(t @ Tone::Dcs { .. }) => {
            let word = t
                .dcs_word()
                .expect("DCS tone built from the table has a word");
            let bit_rate = f64::from(DCS_BIT_RATE);
            let mut lpf = Biquad::lowpass(300.0, fs);
            (0..n)
                .map(|i| {
                    let bit = (i as f64 * bit_rate / fs) as u64 % 23;
                    let level = if word >> bit & 1 == 1 { 1.0 } else { -1.0 };
                    lpf.run(level) as f32
                })
                .collect()
        }
    }
}

/// Instantaneous frequency of `iq` sampled at `rate_hz`, Hz, by the polar
/// discriminator `arg(x[n+1]·conj(x[n]))`; `iq.len() - 1` values (empty for
/// fewer than two samples). A reference demodulator for tests.
pub fn instantaneous_frequency_hz(iq: &[Complex<f32>], rate_hz: f64) -> Vec<f32> {
    let k = rate_hz / TAU;
    iq.windows(2)
        .map(|w| {
            let d = w[1] * w[0].conj();
            (f64::from(d.im).atan2(f64::from(d.re)) * k) as f32
        })
        .collect()
}

/// Mean power `E|x|²` of `iq` (0 for an empty slice).
pub fn mean_power(iq: &[Complex<f32>]) -> f32 {
    if iq.is_empty() {
        return 0.0;
    }
    (iq.iter().map(|x| f64::from(x.norm_sqr())).sum::<f64>() / iq.len() as f64) as f32
}

/// Taps per polyphase branch of [`place_wideband`]'s interpolator.
pub const PLACE_TAPS_PER_PHASE: usize = 32;

/// Build a wideband capture: each `(frequency_hz, iq)` channel (25 ksps, from
/// [`NbfmSignal`]) is interpolated to `rate_hz` and mixed to
/// `frequency_hz - center_hz`, then all are summed. The result is as long as
/// the longest channel (`len · rate_hz / 25 000`, rounded up); shorter
/// channels contribute nothing after their end. Sample `m` of the output is
/// time-aligned with input sample `m · 25 000 / rate_hz` of every channel (the
/// interpolator's delay is compensated). Channel power is preserved. Nothing
/// is added outside the channels: add wideband noise with [`add_awgn`] if a
/// test needs a non-zero floor.
///
/// The interpolator is a Blackman-windowed sinc, cut off at 12.5 kHz with
/// [`PLACE_TAPS_PER_PHASE`] taps per output sample (flat to ≈10 kHz, images
/// below ≈ −55 dB beyond ≈15 kHz). `rate_hz` may be any rate; the ratio is
/// reduced to `p/q` and the filter has `32·p + 1` taps.
///
/// # Panics
///
/// If `rate_hz < 25 000` or a channel's band (`frequency ± 12.5 kHz`) does not
/// fit inside `center_hz ± rate_hz / 2`.
pub fn place_wideband(
    rate_hz: u32,
    center_hz: f64,
    channels: &[(f64, &[Complex<f32>])],
) -> Vec<Complex<f32>> {
    assert!(
        rate_hz >= RATE_HZ,
        "wideband rate {rate_hz} Hz is below the channel rate"
    );
    let rate = f64::from(rate_hz);
    for &(f, _) in channels {
        assert!(
            (f - center_hz).abs() + f64::from(RATE_HZ) / 2.0 <= rate / 2.0,
            "channel at {f} Hz does not fit in {center_hz} Hz ± {} Hz",
            rate / 2.0
        );
    }
    let g = gcd(u64::from(rate_hz), u64::from(RATE_HZ));
    let p = (u64::from(rate_hz) / g) as usize; // interpolation
    let q = (u64::from(RATE_HZ) / g) as usize; // decimation
    let taps = interpolator_taps(p);
    let delay = taps.len() / 2; // taps.len() is odd: exact integer delay

    let out_len = channels
        .iter()
        .map(|(_, iq)| (iq.len() * p).div_ceil(q))
        .max()
        .unwrap_or(0);
    let mut out = vec![Complex::new(0.0f32, 0.0); out_len];

    for &(f, iq) in channels {
        let n = (iq.len() * p).div_ceil(q);
        let step = Complex::from_polar(1.0f64, TAU * (f - center_hz) / rate);
        let mut rot = Complex::new(1.0f64, 0.0);
        for (m, y) in out.iter_mut().take(n).enumerate() {
            let u = m * q + delay;
            let (base, phase) = (u / p, u % p);
            let mut acc = Complex::new(0.0f32, 0.0);
            let mut k = phase;
            let mut j = base;
            while k < taps.len() {
                if j < iq.len() {
                    acc += iq[j] * taps[k];
                }
                if j == 0 {
                    break;
                }
                j -= 1;
                k += p;
            }
            let r = Complex::new(rot.re as f32, rot.im as f32);
            *y += acc * r;
            rot *= step;
            if m % 4096 == 4095 {
                rot /= rot.norm();
            }
        }
    }
    out
}

/// Low-pass prototype at `p · 25 kHz`, cut off at 12.5 kHz, gain `p` (so the
/// zero-stuffed input keeps its amplitude), `32·p + 1` taps.
fn interpolator_taps(p: usize) -> Vec<f32> {
    let len = PLACE_TAPS_PER_PHASE * p + 1;
    let mid = (len - 1) as f64 / 2.0;
    let fc = 0.5 / p as f64; // 12.5 kHz over p·25 kHz, in cycles/sample
    let mut h: Vec<f64> = (0..len)
        .map(|i| {
            let x = i as f64 - mid;
            let sinc = if x == 0.0 {
                2.0 * fc
            } else {
                (TAU * fc * x).sin() / (std::f64::consts::PI * x)
            };
            let w = 0.42 - 0.5 * (TAU * i as f64 / (len - 1) as f64).cos()
                + 0.08 * (2.0 * TAU * i as f64 / (len - 1) as f64).cos();
            sinc * w
        })
        .collect();
    let sum: f64 = h.iter().sum();
    for t in &mut h {
        *t *= p as f64 / sum;
    }
    h.into_iter().map(|t| t as f32).collect()
}

fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

/// RBJ-cookbook biquad (Butterworth Q), f64, for the generator's own filters.
struct Biquad {
    b: [f64; 3],
    a: [f64; 2],
    z: [f64; 2],
}

impl Biquad {
    fn lowpass(fc: f64, fs: f64) -> Biquad {
        let (cos, alpha) = Self::omega(fc, fs);
        let b1 = 1.0 - cos;
        Self::norm(
            [b1 / 2.0, b1, b1 / 2.0],
            [1.0 + alpha, -2.0 * cos, 1.0 - alpha],
        )
    }

    fn highpass(fc: f64, fs: f64) -> Biquad {
        let (cos, alpha) = Self::omega(fc, fs);
        let b1 = 1.0 + cos;
        Self::norm(
            [b1 / 2.0, -b1, b1 / 2.0],
            [1.0 + alpha, -2.0 * cos, 1.0 - alpha],
        )
    }

    fn omega(fc: f64, fs: f64) -> (f64, f64) {
        let w = TAU * fc / fs;
        (w.cos(), w.sin() / (2.0 * std::f64::consts::FRAC_1_SQRT_2))
    }

    fn norm(b: [f64; 3], a: [f64; 3]) -> Biquad {
        Biquad {
            b: [b[0] / a[0], b[1] / a[0], b[2] / a[0]],
            a: [a[1] / a[0], a[2] / a[0]],
            z: [0.0; 2],
        }
    }

    /// Transposed direct form II.
    fn run(&mut self, x: f64) -> f64 {
        let y = self.b[0] * x + self.z[0];
        self.z[0] = self.b[1] * x - self.a[0] * y + self.z[1];
        self.z[1] = self.b[2] * x - self.a[1] * y;
        y
    }
}

/// splitmix64 + Box–Muller: small, deterministic, dependency-free.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Rng {
        Rng(seed)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in (0, 1].
    fn uniform(&mut self) -> f64 {
        ((self.next_u64() >> 11) + 1) as f64 / (1u64 << 53) as f64
    }

    /// Two independent standard normal samples.
    fn gaussian_pair(&mut self) -> (f64, f64) {
        let r = (-2.0 * self.uniform().ln()).sqrt();
        let (s, c) = (TAU * self.uniform()).sin_cos();
        (r * c, r * s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FS: f64 = RATE_HZ as f64;

    /// Amplitude of the `hz` component of a real signal (single-bin DFT).
    fn tone_amplitude(x: &[f32], hz: f64, fs: f64) -> f64 {
        let (mut re, mut im) = (0.0f64, 0.0f64);
        for (i, &v) in x.iter().enumerate() {
            let (s, c) = (TAU * hz * i as f64 / fs).sin_cos();
            re += f64::from(v) * c;
            im -= f64::from(v) * s;
        }
        2.0 * (re * re + im * im).sqrt() / x.len() as f64
    }

    /// Complex correlation of `x` with `e^{j2π f n / fs}`, normalised by length.
    fn correlate(x: &[Complex<f32>], hz: f64, fs: f64) -> Complex<f64> {
        let mut acc = Complex::new(0.0f64, 0.0);
        for (i, v) in x.iter().enumerate() {
            let r = Complex::from_polar(1.0, -TAU * hz * i as f64 / fs);
            acc += Complex::new(f64::from(v.re), f64::from(v.im)) * r;
        }
        acc / x.len() as f64
    }

    #[test]
    fn noiseless_carrier_has_unit_power() {
        let iq = nbfm(
            &Audio::Voice,
            Tone::ctcss(1000),
            Deviation::NARROW,
            300.0,
            f32::INFINITY,
            25_000,
        );
        assert_eq!(iq.len(), 25_000);
        for x in &iq {
            assert!((x.norm() - 1.0).abs() < 1e-5);
        }
        let mut s = NbfmSignal::new(Audio::Silence, None);
        s.amplitude = 0.1;
        assert!((mean_power(&s.generate(1000)) - 0.01).abs() < 1e-6);
    }

    #[test]
    fn noise_power_matches_snr() {
        // Unmodulated carrier at 10 dB SNR (12.5 kHz ref) → total noise 0.2.
        let iq = nbfm(&Audio::Silence, None, Deviation::NARROW, 0.0, 10.0, 200_000);
        let carrier = iq.iter().sum::<Complex<f32>>() / iq.len() as f32;
        let noise_p: f32 =
            iq.iter().map(|x| (x - carrier).norm_sqr()).sum::<f32>() / iq.len() as f32;
        assert!((carrier.norm() - 1.0).abs() < 0.01, "{carrier}");
        assert!((noise_p / 0.2 - 1.0).abs() < 0.03, "noise power {noise_p}");
        assert!((noise_power_for_snr(20.0, 1.0) - 0.02).abs() < 1e-7);
        // Same seed, same noise; different seed, different noise.
        let a = noise(1000, 1.0, 7);
        assert_eq!(a, noise(1000, 1.0, 7));
        assert_ne!(a, noise(1000, 1.0, 8));
        assert!((mean_power(&noise(100_000, 0.5, 3)) - 0.5).abs() < 0.01);
    }

    #[test]
    fn sine_deviation_and_offset() {
        let dev = Deviation {
            audio_hz: 2_500.0,
            subaudio_hz: 0.0,
        };
        let iq = nbfm(
            &Audio::Sine { hz: 1_000.0 },
            None,
            dev,
            -400.0,
            f32::INFINITY,
            25_000,
        );
        let f = instantaneous_frequency_hz(&iq, FS);
        let max = f.iter().copied().fold(f32::MIN, f32::max);
        let min = f.iter().copied().fold(f32::MAX, f32::min);
        let mean = f.iter().sum::<f32>() / f.len() as f32;
        assert!((max - (2_500.0 - 400.0)).abs() < 5.0, "max {max}");
        assert!((min - (-2_500.0 - 400.0)).abs() < 5.0, "min {min}");
        assert!((mean + 400.0).abs() < 2.0, "mean {mean}");
        let amp = tone_amplitude(&f, 1_000.0, FS);
        assert!((amp - 2_500.0).abs() < 10.0, "1 kHz amplitude {amp}");
    }

    #[test]
    fn ctcss_deviation() {
        for tenths in [670u16, 1679, 2541] {
            let t = Tone::ctcss(tenths).unwrap();
            let iq = nbfm(
                &Audio::Silence,
                Some(t),
                Deviation::NARROW,
                0.0,
                f32::INFINITY,
                50_000,
            );
            let f = instantaneous_frequency_hz(&iq, FS);
            let hz = f64::from(t.ctcss_hz().unwrap());
            let amp = tone_amplitude(&f, hz, FS);
            assert!((amp - 600.0).abs() < 6.0, "{t}: amplitude {amp}");
            let peak = f.iter().map(|v| v.abs()).fold(0.0f32, f32::max);
            assert!((peak - 600.0).abs() < 2.0, "{t}: peak {peak}");
        }
    }

    #[test]
    fn audio_waveforms_are_bounded() {
        for a in [
            Audio::Silence,
            Audio::Sine { hz: 1_000.0 },
            Audio::Voice,
            Audio::Noise,
        ] {
            let w = audio_waveform(&a, 25_000, 5);
            assert!(w.iter().all(|v| v.abs() <= 1.0), "{a:?}");
        }
        let n = audio_waveform(&Audio::Noise, 100_000, 5);
        let rms = (n.iter().map(|v| v * v).sum::<f32>() / n.len() as f32).sqrt();
        assert!((rms - 0.3).abs() < 0.01, "noise rms {rms}");
        // Band-limited: little energy near 60 Hz or 6 kHz compared with 1 kHz
        // (mean power over nine independent DFT bins each).
        let band_power = |from: f64| -> f64 {
            (0..9)
                .map(|k| tone_amplitude(&n, from + 5.0 * k as f64, FS).powi(2))
                .sum::<f64>()
                / 9.0
        };
        let in_band = band_power(980.0);
        assert!(band_power(5_980.0) < in_band / 50.0);
        assert!(band_power(40.0) < in_band / 50.0);
    }

    #[test]
    fn dcs_bits_follow_the_word() {
        for t in ["D023N", "D023I", "D445N", "D754I"] {
            let tone: Tone = t.parse().unwrap();
            let word = tone.dcs_word().unwrap();
            let n = 3 * 23 * 187; // three words
            let iq = nbfm(
                &Audio::Silence,
                Some(tone),
                Deviation::NARROW,
                0.0,
                f32::INFINITY,
                n,
            );
            let f = instantaneous_frequency_hz(&iq, FS);
            let bit_len = FS / f64::from(DCS_BIT_RATE);
            for bit in 0..(3 * 23 - 1) {
                // Sample late in the bit, after the 300 Hz filter settles.
                let at = ((bit as f64 + 0.8) * bit_len) as usize;
                let expect = word >> (bit % 23) & 1 == 1;
                assert_eq!(f[at] > 0.0, expect, "{t} bit {bit}");
                assert!(
                    f[at].abs() > 400.0 && f[at].abs() < 650.0,
                    "{t} bit {bit}: {}",
                    f[at]
                );
            }
        }
        // Inverted is the exact negation of normal.
        let n = subaudio_waveform(Some("D445N".parse().unwrap()), 5000);
        let i = subaudio_waveform(Some("D445I".parse().unwrap()), 5000);
        assert!(n.iter().zip(&i).all(|(a, b)| (a + b).abs() < 1e-6));
    }

    #[test]
    fn keying_gates_the_carrier_not_the_noise() {
        let mut s = NbfmSignal::new(Audio::Voice, None);
        s.keyed = vec![1_000..2_000, 3_000..3_500];
        s.snr_db = 20.0;
        let iq = s.generate(5_000);
        let off = mean_power(&iq[..1_000]);
        let on = mean_power(&iq[1_000..2_000]);
        let expect_noise = noise_power_for_snr(20.0, 1.0);
        assert!((off / expect_noise - 1.0).abs() < 0.2, "off {off}");
        assert!((on - (1.0 + expect_noise)).abs() < 0.05, "on {on}");
        assert!(mean_power(&iq[2_000..3_000]) < 0.05);
        assert!(mean_power(&iq[3_000..3_500]) > 0.9);
        s.snr_db = f32::INFINITY;
        let quiet = s.generate(5_000);
        assert!(quiet[..1_000].iter().all(|x| x.norm() == 0.0));
    }

    #[test]
    fn wideband_places_channels_at_their_frequencies() {
        let rate = 2_000_000u32;
        let center = 100_000_000.0;
        // A 25 ksps "carrier + 2 kHz offset" (pure tone) per channel.
        let a = nbfm(
            &Audio::Silence,
            None,
            Deviation::NARROW,
            2_000.0,
            f32::INFINITY,
            2_500,
        );
        let b = nbfm(
            &Audio::Silence,
            None,
            Deviation::NARROW,
            -1_000.0,
            f32::INFINITY,
            2_000,
        );
        let fa = center + 312_500.0;
        let fb = center - 507_500.0; // off the 12.5 kHz grid
        let wide = place_wideband(rate, center, &[(fa, &a), (fb, &b)]);
        assert_eq!(wide.len(), 2_500 * 80);
        let rate = f64::from(rate);
        // Skip the interpolator's edges.
        let span = &wide[4_000..150_000];
        let ca = correlate(span, fa - center + 2_000.0, rate).norm();
        let cb = correlate(span, fb - center - 1_000.0, rate).norm();
        assert!((ca - 1.0).abs() < 0.01, "channel a amplitude {ca}");
        assert!((cb - 1.0).abs() < 0.01, "channel b amplitude {cb}");
        // Nothing at the mirror image or at the centre.
        assert!(correlate(span, -(fa - center + 2_000.0), rate).norm() < 0.01);
        assert!(correlate(span, 0.0, rate).norm() < 0.01);
        // Power is the sum of the two unit carriers while both are present...
        let p = mean_power(span);
        assert!((p - 2.0).abs() < 0.05, "power {p}");
        // ...and only channel a's after b ends (2000 samples × 80).
        let p = mean_power(&wide[165_000..196_000]);
        assert!((p - 1.0).abs() < 0.02, "power after b ends {p}");
    }

    #[test]
    fn wideband_preserves_timing_and_deviation() {
        // Key a carrier on at sample 1000 of 25 ksps; it must appear at
        // 1000 × p/q in the wideband output, with its deviation intact after
        // mixing back down.
        let mut s = NbfmSignal::new(Audio::Sine { hz: 1_000.0 }, None);
        s.keyed.push(1_000..4_000);
        let iq = s.generate(5_000);
        let rate = 2_500_000u32; // p/q = 100
        let center = 450_000_000.0;
        let f = center + 160_000.0;
        let wide = place_wideband(rate, center, &[(f, &iq)]);
        let edge = wide.iter().position(|x| x.norm() > 0.5).unwrap();
        assert!((edge as i64 - 100_000).abs() <= 100, "edge at {edge}");
        // Mix down and take every 100th sample: back at 25 ksps.
        let step = Complex::from_polar(1.0f64, -TAU * 160_000.0 / f64::from(rate));
        let mut rot = Complex::new(1.0f64, 0.0);
        let mut base = Vec::new();
        for (m, x) in wide.iter().enumerate() {
            if m % 100 == 0 {
                base.push(*x * Complex::new(rot.re as f32, rot.im as f32));
            }
            rot *= step;
        }
        let fi = instantaneous_frequency_hz(&base[1_200..3_800], FS);
        let peak = fi.iter().map(|v| v.abs()).fold(0.0f32, f32::max);
        assert!((peak - 2_000.0).abs() < 30.0, "peak deviation {peak}");
        assert!((tone_amplitude(&fi, 1_000.0, FS) - 2_000.0).abs() < 30.0);
    }

    #[test]
    fn wideband_handles_a_non_integer_ratio() {
        // 2.0125 Msps / 25 ksps = 161/2.
        let iq = nbfm(
            &Audio::Silence,
            None,
            Deviation::NARROW,
            500.0,
            f32::INFINITY,
            1_000,
        );
        let rate = 2_012_500u32;
        let wide = place_wideband(rate, 0.0, &[(250_000.0, &iq)]);
        assert_eq!(wide.len(), (1_000 * 161usize).div_ceil(2));
        let c = correlate(&wide[2_000..78_000], 250_500.0, f64::from(rate)).norm();
        assert!((c - 1.0).abs() < 0.01, "amplitude {c}");
    }

    #[test]
    #[should_panic(expected = "does not fit")]
    fn wideband_rejects_out_of_span_channels() {
        let iq = vec![Complex::new(1.0f32, 0.0); 10];
        place_wideband(2_000_000, 0.0, &[(990_000.0, &iq)]);
    }
}
