// SPDX-License-Identifier: GPL-3.0-or-later
//! DCS (Digital-Coded Squelch) detector: bit sync at 134.4 bps on the
//! 2.5 ksps sub-audio stream, Golay(23,12) codeword search in both
//! polarities.
//!
//! # Input
//!
//! [`DcsDetector::push`] takes the sub-audio path's output at
//! [`SUBAUDIO_RATE_HZ`]: the FM discriminator output
//! low-passed at 300 Hz and decimated by 10. Any scale (Hz or normalised) and
//! any DC offset is fine, with or without the engine's 1 Hz DC tracker in
//! front: the detector sets its own slicing level from the signal (see
//! below), so a carrier offset or an unbalanced code word does not bias the
//! bits. Positive values must mean positive frequency deviation (the
//! convention of [`Tone::dcs_word`]).
//!
//! # Method
//!
//! 1. A 2nd-order 200 Hz low-pass ([`PREFILTER_HZ`]) takes out what is left
//!    of voice audio above the sub-audio band; the DCS stream's main lobe is
//!    below 134 Hz.
//! 2. Bit timing: a phase accumulator at the nominal 134.4 bps (18.6 samples
//!    per bit) is pulled toward every crossing of the slicing level, which
//!    falls on a bit boundary for NRZ. The first crossings after
//!    [`DcsDetector::new`] / [`reset`](DcsDetector::reset) set the phase
//!    almost outright; later ones nudge it.
//! 3. At each bit centre the last 46 bit centres (two code words) are
//!    re-interpolated from the filtered input history at the *current* bit
//!    timing, and sliced at the two-cluster (k-means) midpoint of those 46
//!    samples. Bits that were first seen before the timing and level had
//!    settled are therefore taken again correctly once they have.
//! 4. Each 23-bit window is Golay-decoded (≤ 1 corrected bit, or 0 when
//!    `strict`) and the codeword is rotated to every alignment; a hit is an
//!    alignment whose data bits carry the fixed `100` and one of the 104
//!    standard codes. A code is **detected** when both 23-bit halves of the
//!    46-bit ring hit the same code at the same alignment (two consecutive
//!    agreeing words) and the ring's eye is open (within-cluster spread below
//!    [`MAX_EYE_SPREAD`] of the half eye height; a sampled sine such as a
//!    CTCSS tone fails this).
//! 5. A detected code is **lost** after [`LOSS_BITS`] bits (two words,
//!    ≈ 342 ms) in which the newest 23-bit window never held it. The window
//!    still holds the code for up to one word after the signal stops, so the
//!    loss is reported 2–3 words (342–513 ms) after it. A different code
//!    replaces the detected one as soon as it is confirmed by two words.
//!
//! # Polarity and reported tone
//!
//! The inverted stream of a code is the normal stream of its partner code
//! ([`Tone::dcs_equivalent`]), so the two are the same signal on air and no
//! receiver can tell them apart. The detector searches the received bits as
//! normal polarity only, which finds every code in either polarity: it always
//! reports the **normal-polarity member** of the pair (`D445N` for a `D445N`
//! transmitter, `D043N` for `D445I`). Compare a decision with a configured
//! tone using [`Tone::same_signal`], never `==`.
//!
//! # Latency
//!
//! Two words are 46 bits = 342 ms. From a fresh detector (or one
//! [`reset`](DcsDetector::reset) at carrier-up) a 10 dB SNR signal is
//! confirmed within 400 ms; a detector left running on noise before the
//! carrier needs a little longer (the timing loop is then in its slow mode).
//! The engine should reset the detector when the squelch opens.
//!
//! # Cost
//!
//! Per input sample: one biquad and a few comparisons. Per bit (134 per
//! second): 46 interpolations, a 46-sample k-means pass and two Golay decodes
//! with a 23-step rotation search. About 4.5 KB of state, no allocation.

use crate::golay;
use crate::tone::{Tone, DCS_BIT_RATE, DCS_CODES, DCS_FIXED_BITS};
use crate::SUBAUDIO_RATE_HZ;

/// Bits in one DCS word.
const WORD_BITS: usize = 23;

/// Bits held for a decision: two words.
const RING_BITS: usize = 2 * WORD_BITS;

/// Input samples kept for re-sampling the bit centres (≥ 46 bits × 18.6).
const HIST: usize = 1024;

/// Cut-off of the detector's own low-pass in front of the bit slicer, Hz.
pub const PREFILTER_HZ: f32 = 200.0;

/// Largest within-cluster RMS spread of the 46 bit samples, as a fraction of
/// half the distance between the two cluster means, for which a decision is
/// made. Clean NRZ at 10 dB SNR measures at most ≈ 0.21; a sine sampled at
/// arbitrary phases (a CTCSS tone) ≈ 0.48.
pub const MAX_EYE_SPREAD: f32 = 0.35;

/// Bits without a hit of the detected code after which it is lost (two
/// words, ≈ 342 ms).
pub const LOSS_BITS: u32 = 2 * WORD_BITS as u32;

/// Timing-loop gain once acquired and while a code is detected.
const GAIN_TRACK: f32 = 0.1;

/// Timing-loop gain while searching (no code detected).
const GAIN_SEARCH: f32 = 0.3;

/// A change of the detector's state, returned by [`DcsDetector::push`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DcsDecision {
    /// This code is now being received (normal-polarity member of its
    /// on-air pair; see the module docs).
    Detected(Tone),
    /// The previously detected code is no longer received.
    Lost,
}

/// Streaming DCS detector for one channel.
#[derive(Clone, Debug)]
pub struct DcsDetector {
    strict: bool,
    lpf: Biquad,
    /// Previous pre-filtered sample.
    prev: f32,
    /// Bit phase in bit periods, [0, 1); 0 = bit boundary, 0.5 = centre.
    phase: f32,
    /// Phase advance per input sample.
    step: f32,
    /// The current bit's centre has been sampled.
    sampled: bool,
    /// Level crossings seen since reset (drives the timing-loop gain).
    crossings: u32,
    /// Slow mean of the input, the crossing level until the ring has data.
    mean: f32,
    samples_seen: u32,
    /// Slicing level from the last k-means pass.
    level: f32,
    /// Pre-filtered input history (circular, indexed by sample count).
    hist: [f32; HIST],
    /// Samples written to `hist`.
    written: u64,
    /// Bit-centre samples of the last `ring_len` bits, oldest first,
    /// re-interpolated from `hist` at the current timing every bit.
    ring: [f32; RING_BITS],
    ring_len: usize,
    /// Detected code (index into [`DCS_CODES`]).
    current: Option<usize>,
    /// Bits since the detected code was last seen in a window.
    miss_bits: u32,
}

impl DcsDetector {
    /// A detector that allows one corrected bit per word (`strict = false`)
    /// or requires error-free words (`strict = true`,
    /// [`ChannelParams::dcs_strict`](crate::ChannelParams::dcs_strict)).
    pub fn new(strict: bool) -> DcsDetector {
        let fs = SUBAUDIO_RATE_HZ as f32;
        DcsDetector {
            strict,
            lpf: Biquad::lowpass(PREFILTER_HZ, fs),
            prev: 0.0,
            phase: 0.0,
            step: DCS_BIT_RATE / fs,
            sampled: false,
            crossings: 0,
            mean: 0.0,
            samples_seen: 0,
            level: 0.0,
            hist: [0.0; HIST],
            written: 0,
            ring: [0.0; RING_BITS],
            ring_len: 0,
            current: None,
            miss_bits: 0,
        }
    }

    /// Forget all state (as after [`new`](DcsDetector::new)); call at
    /// carrier-up for the fastest lock. Does not report [`DcsDecision::Lost`].
    pub fn reset(&mut self) {
        *self = DcsDetector::new(self.strict);
    }

    /// The code currently detected, if any (normal-polarity member).
    pub fn current(&self) -> Option<Tone> {
        self.current.map(code_tone)
    }

    /// Feed sub-audio samples at 2.5 ksps. Returns the last state change in
    /// this block, if any (a block long enough to contain both a detection
    /// and its loss reports only the latter).
    pub fn push(&mut self, x: &[f32]) -> Option<DcsDecision> {
        let mut decision = None;
        for &raw in x {
            if let Some(d) = self.push_sample(raw) {
                decision = Some(d);
            }
        }
        decision
    }

    fn push_sample(&mut self, raw: f32) -> Option<DcsDecision> {
        let y = self.lpf.run(raw);
        // Slow mean (≈ 50 ms) for the crossing level before the ring fills.
        self.samples_seen = self.samples_seen.saturating_add(1);
        let a = (1.0 / self.samples_seen as f32).max(1.0 / 128.0);
        self.mean += a * (y - self.mean);
        let level = if self.ring_len >= 8 {
            self.level
        } else {
            self.mean
        };

        let prev = self.prev;
        self.prev = y;
        self.hist[(self.written % HIST as u64) as usize] = y;
        self.written += 1;
        let prev_phase = self.phase;
        self.phase += self.step;

        let mut decision = None;
        // Bit centre between the previous sample and this one.
        if !self.sampled && prev_phase < 0.5 && self.phase >= 0.5 {
            let t = (0.5 - prev_phase) / self.step;
            self.sampled = true;
            decision = self.bit(1.0 - t);
        }
        if self.phase >= 1.0 {
            self.phase -= 1.0;
            self.sampled = false;
        }

        // Level crossing between the previous sample and this one: that
        // instant should be a bit boundary (phase 0).
        let (d0, d1) = (prev - level, y - level);
        if self.samples_seen > 1 && (d0 < 0.0) != (d1 < 0.0) && d0 != d1 {
            let frac = d0 / (d0 - d1); // 0 at prev, 1 at this sample
            let at = prev_phase + frac * self.step;
            let mut err = at - at.round(); // [-0.5, 0.5]
            if err >= 0.5 {
                err -= 1.0;
            }
            self.crossings = self.crossings.saturating_add(1);
            let gain = if self.crossings <= 3 {
                1.0 / self.crossings as f32
            } else if self.current.is_some() {
                GAIN_TRACK
            } else {
                GAIN_SEARCH
            };
            self.phase -= gain * err;
            if self.phase < 0.0 {
                // Pulled back across a boundary: this is still the bit that
                // was being sampled.
                self.phase += 1.0;
                self.sampled = true;
            } else if self.phase >= 1.0 {
                self.phase -= 1.0;
                self.sampled = false;
            }
        }
        decision
    }

    /// A bit centre `back` samples before the newest sample: re-sample the
    /// last (up to) 46 bit centres at this timing, then slice and search.
    fn bit(&mut self, back: f32) -> Option<DcsDecision> {
        let sps = 1.0 / self.step;
        let newest = self.written - 1;
        // Bit centres that lie inside the history (only limits start-up).
        let span = newest.min(HIST as u64 - 1) as f32;
        let avail = ((span - back) / sps).max(0.0) as usize + 1;
        self.ring_len = (self.ring_len + 1).min(RING_BITS).min(avail);
        let len = self.ring_len;
        for i in 0..len {
            let b = back + (len - 1 - i) as f32 * sps;
            let whole = b.floor();
            let f = b - whole;
            let idx = newest - whole as u64; // sample at or after the centre
            let after = self.hist[(idx % HIST as u64) as usize];
            let before = self.hist[(idx.saturating_sub(1) % HIST as u64) as usize];
            self.ring[i] = after + f * (before - after);
        }
        let eye = self.slice();
        if self.current.is_some() {
            self.miss_bits = self.miss_bits.saturating_add(1);
        }
        let confirmed = match eye {
            Some((bits, spread)) if self.ring_len == RING_BITS => self.search(bits, spread),
            _ => None,
        };
        match (confirmed, self.current) {
            (Some(code), cur) if cur != Some(code) => {
                self.current = Some(code);
                self.miss_bits = 0;
                Some(DcsDecision::Detected(code_tone(code)))
            }
            (_, Some(_)) if self.miss_bits >= LOSS_BITS => {
                self.current = None;
                self.miss_bits = 0;
                Some(DcsDecision::Lost)
            }
            _ => None,
        }
    }

    /// Search the sliced ring: refresh the detected code's hit counter and
    /// return a code confirmed by both words.
    fn search(&mut self, bits: u64, spread: f32) -> Option<usize> {
        let max_errors = if self.strict { 0 } else { 1 };
        let mask = u64::from(golay::CODEWORD_MASK);
        let newer = lookup((bits >> WORD_BITS & mask) as u32, max_errors);
        if newer.is_some() && newer.map(|h| h.0) == self.current {
            self.miss_bits = 0;
        }
        let older = lookup((bits & mask) as u32, max_errors);
        match (older, newer) {
            (Some(a), Some(b)) if a == b && spread <= MAX_EYE_SPREAD => Some(a.0),
            _ => None,
        }
    }

    /// Recompute the slicing level by two-cluster k-means over the ring and
    /// slice it. Returns the bits (oldest = bit 0, 46 bits) and the eye
    /// spread, or `None` if the ring does not separate into two clusters.
    fn slice(&mut self) -> Option<(u64, f32)> {
        let n = self.ring_len;
        let at = |i: usize| self.ring[i];
        let mut level = (0..n).map(at).sum::<f32>() / n as f32;
        let (mut hi, mut lo) = (level, level);
        for _ in 0..4 {
            let (mut sh, mut nh, mut sl, mut nl) = (0.0f32, 0u32, 0.0f32, 0u32);
            for i in 0..n {
                let v = at(i);
                if v > level {
                    sh += v;
                    nh += 1;
                } else {
                    sl += v;
                    nl += 1;
                }
            }
            if nh == 0 || nl == 0 {
                return None;
            }
            hi = sh / nh as f32;
            lo = sl / nl as f32;
            let next = 0.5 * (hi + lo);
            if next == level {
                break;
            }
            level = next;
        }
        self.level = level;
        let half = 0.5 * (hi - lo);
        if half <= 0.0 || !half.is_finite() {
            return None;
        }
        let mut bits = 0u64;
        let mut spread = 0.0f32;
        for i in 0..n {
            let v = at(i);
            let one = v > level;
            if one {
                bits |= 1 << i;
            }
            let d = v - if one { hi } else { lo };
            spread += d * d;
        }
        Some((bits, (spread / n as f32).sqrt() / half))
    }
}

/// The standard code (index into [`DCS_CODES`]) and alignment whose
/// normal-polarity word, rotated, is `window` within `max_errors` bits.
fn lookup(window: u32, max_errors: u32) -> Option<(usize, u32)> {
    let d = golay::decode(window, max_errors)?;
    (0..WORD_BITS as u32).find_map(|r| {
        let aligned = golay::rotate_right(d.codeword, r);
        let data = (aligned & golay::DATA_MASK) as u16;
        if data & 0xE00 != DCS_FIXED_BITS {
            return None;
        }
        let v = data & 0x1FF;
        let code = (v >> 6 & 7) * 100 + (v >> 3 & 7) * 10 + (v & 7);
        DCS_CODES.binary_search(&code).ok().map(|i| (i, r))
    })
}

fn code_tone(index: usize) -> Tone {
    Tone::Dcs {
        code: DCS_CODES[index],
        inverted: false,
    }
}

/// RBJ-cookbook Butterworth low-pass biquad, f32, transposed direct form II.
#[derive(Clone, Debug)]
struct Biquad {
    b: [f32; 3],
    a: [f32; 2],
    z: [f32; 2],
}

impl Biquad {
    fn lowpass(fc: f32, fs: f32) -> Biquad {
        let w = std::f32::consts::TAU * fc / fs;
        let (sin, cos) = w.sin_cos();
        let alpha = sin / (2.0 * std::f32::consts::FRAC_1_SQRT_2);
        let a0 = 1.0 + alpha;
        let b1 = (1.0 - cos) / a0;
        Biquad {
            b: [b1 / 2.0, b1, b1 / 2.0],
            a: [-2.0 * cos / a0, (1.0 - alpha) / a0],
            z: [0.0; 2],
        }
    }

    fn run(&mut self, x: f32) -> f32 {
        let y = self.b[0] * x + self.z[0];
        self.z[0] = self.b[1] * x - self.a[0] * y + self.z[1];
        self.z[1] = self.b[2] * x - self.a[1] * y;
        y
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ideal NRZ at 2.5 ksps for `word`, starting `skip` samples in.
    fn nrz(word: u32, skip: usize, n: usize, amp: f32, dc: f32) -> Vec<f32> {
        let sps = SUBAUDIO_RATE_HZ as f64 / f64::from(DCS_BIT_RATE);
        (skip..skip + n)
            .map(|i| {
                let bit = (i as f64 / sps) as u64 % 23;
                dc + if word >> bit & 1 == 1 { amp } else { -amp }
            })
            .collect()
    }

    #[test]
    fn lookup_finds_every_rotation_in_both_polarities() {
        for (i, &code) in DCS_CODES.iter().enumerate() {
            let n = Tone::dcs(code, false).unwrap();
            let w = n.dcs_word().unwrap();
            for k in 0..23 {
                assert_eq!(lookup(golay::rotate_right(w, k), 0).map(|h| h.0), Some(i));
                // One flipped bit is corrected only when allowed.
                let bad = golay::rotate_right(w, k) ^ (1 << (k % 23));
                assert_eq!(lookup(bad, 1).map(|h| h.0), Some(i));
                assert_eq!(lookup(bad, 0), None);
            }
            let inv = Tone::dcs(code, true).unwrap();
            let partner = inv.dcs_equivalent().unwrap();
            let hit = lookup(inv.dcs_word().unwrap(), 0).map(|h| code_tone(h.0));
            assert_eq!(hit, Some(partner), "{inv}");
        }
    }

    #[test]
    fn clean_nrz_with_offset_detects_every_code() {
        for &code in &DCS_CODES {
            for inverted in [false, true] {
                let t = Tone::dcs(code, inverted).unwrap();
                let x = nrz(t.dcs_word().unwrap(), 77 + code as usize, 1000, 0.25, 0.12);
                let mut d = DcsDetector::new(true);
                match d.push(&x) {
                    Some(DcsDecision::Detected(got)) => assert!(got.same_signal(&t), "{t}: {got}"),
                    other => panic!("{t}: {other:?}"),
                }
            }
        }
    }

    #[test]
    fn loses_the_code_when_the_signal_stops() {
        let t = Tone::dcs(445, false).unwrap();
        let mut d = DcsDetector::new(false);
        let x = nrz(t.dcs_word().unwrap(), 0, 1500, 1.0, 0.0);
        assert_eq!(d.push(&x), Some(DcsDecision::Detected(t)));
        // A different code takes over after two of its words.
        let u = Tone::dcs(23, false).unwrap();
        let y = nrz(u.dcs_word().unwrap(), 0, 1500, 1.0, 0.0);
        assert_eq!(d.push(&y), Some(DcsDecision::Detected(u)));
        // Silence (flat line): lost once no window has held the code for
        // LOSS_BITS bits, i.e. between two and three words after the end.
        assert_eq!(d.push(&[0.0; 800]), None);
        assert_eq!(d.push(&[0.0; 600]), Some(DcsDecision::Lost));
        assert_eq!(d.current(), None);
        d.reset();
        assert_eq!(d.current(), None);
    }
}
