// SPDX-License-Identifier: GPL-3.0-or-later
//! CTCSS detector: a bank of 50 single-bin DFTs (one per standard tone, see
//! [`CTCSS_TONES`]) over a sliding window of the 2.5 ksps sub-audio stream.
//!
//! # Input
//!
//! [`CtcssDetector::push`] takes the sub-audio path's output at
//! [`SUBAUDIO_RATE_HZ`]: discriminator output low-passed at 300 Hz and
//! decimated, in the engine's normalised units (±1.0 = the channel's full
//! scale deviation). The decisions are ratios between bins, so the scale only
//! matters for the absolute floor [`MIN_LEVEL`]. A DC component (carrier
//! offset) is removed per block, so the input need not be DC-free.
//!
//! # Window and resolution
//!
//! The closest standard tones are 2.3 Hz apart (67.0 / 69.3) and three more
//! pairs are within 2.5 Hz (159.8 / 162.2, 165.5 / 167.9: 2.4 Hz; 171.3 /
//! 173.8: 2.5 Hz). The rule of thumb "a T-second window resolves 1/T Hz" is
//! about separating two tones present *at the same time*; CTCSS carries one
//! tone, and what matters is how much of it leaks into the neighbour's bin.
//! With a rectangular window of T = [`WINDOW_MS`] = 400 ms the leakage at an
//! offset δ is `|sinc(δ·T)|`, whose first null is at 1/T = 2.5 Hz — exactly
//! where the close pairs sit:
//!
//! | pair           | δ      | neighbour bin |
//! |----------------|--------|---------------|
//! | 67.0 / 69.3    | 2.3 Hz | −21.3 dB      |
//! | 159.8 / 162.2  | 2.4 Hz | −27.6 dB      |
//! | 165.5 / 167.9  | 2.4 Hz | −27.6 dB      |
//! | 171.3 / 173.8  | 2.5 Hz | null          |
//!
//! A tapered (Hann) window would push the first null out to 2/T = 5 Hz and
//! leave the neighbour at about −5 dB: the rectangular window is the right one here,
//! and a longer rectangular window is *worse* (600 ms puts 2.3 Hz on the first
//! sidelobe, −13 dB). Pairs further apart fall on sidelobes, ≤ −13 dB. Every
//! decision needs the winner [`LOCK_RUNNER_UP_DB`] = 6 dB above the runner-up,
//! so even the 67.0 / 69.3 pair keeps 15 dB of margin for noise and for a
//! transmitter tone that is off frequency. Tested tolerance: ±0.3 % on every
//! tone (crystal/DSP encoders are within ±0.1 %); at ±0.5 % the close pairs
//! start to fail the 6 dB margin and are then *not detected* (no wrong tone).
//!
//! # Evaluation and decision rule
//!
//! The window is built from [`BLOCKS`] blocks of [`BLOCK_SAMPLES`] (50 ms);
//! each block's DFT values are referenced to the absolute sample clock, so the
//! window DFT is the plain sum of the last eight blocks (exact, no drift, no
//! re-scan of old samples). After every block, once the window is full:
//!
//! - **qualify**: the strongest bin is ≥ [`LOCK_RUNNER_UP_DB`] above the
//!   runner-up, ≥ [`LOCK_BANK_DB`] above the mean of the other 49 bins, its
//!   amplitude is ≥ [`MIN_LEVEL`], and its phase coherence across the eight
//!   blocks is ≥ [`LOCK_COHERENCE`] ([`Evaluation::coherence`]; this is what
//!   keeps a DCS code's strongest spectral line from passing as a tone).
//! - **lock**: the same tone qualifies in [`LOCK_RUNS`] consecutive
//!   evaluations → `Detected(tone)`. A different tone qualifying while locked
//!   switches the lock the same way.
//! - **hold**: while locked, an evaluation is a *hit* if no other bin beats
//!   the locked tone by [`LOCK_RUNNER_UP_DB`] or more, the locked tone is
//!   ≥ [`HOLD_BANK_DB`] above the bank mean, and its amplitude is
//!   ≥ [`HOLD_LEVEL_FRACTION`] of its running level. [`DROP_MISSES`]
//!   consecutive misses → `Lost`. So a different winner must beat the locked
//!   tone by 6 dB for two hops to end it: a voice harmonic or hum line that
//!   edges ahead for a moment does not split the call.
//!
//! # Latency (measured by `tests/ctcss.rs`)
//!
//! - **Lock** from a fresh detector ([`CtcssDetector::new`] /
//!   [`CtcssDetector::reset`]) with the tone present from the first sample:
//!   window fill (400 ms) + one more block for the second qualifying run =
//!   **450 ms**, fixed, at any SNR where the tone is detected at all.
//! - **Lock** with no reset at carrier-up (the window still holds pre-carrier
//!   discriminator noise): ≤ 450 ms for all 50 tones at 20, 10 and 6 dB SNR.
//!   Both are inside the 500 ms lock budget and well inside the 750 ms
//!   default `tone_timeout_ms`; a bare 400 ms (window fill only) is traded
//!   for the second qualifying run (one 50 ms block), which is what keeps noise from
//!   locking.
//! - **Switch** to a different tone with the carrier up: ≤ 450 ms.
//! - **Drop** after the tone stops while the carrier stays up: the level falls
//!   below half once half the window has turned over, then [`DROP_MISSES`]
//!   blocks: 300–350 ms measured. (A squelch-tail reverse burst is not
//!   detected as such; the carrier drop that follows ends the call.)
//!
//! No allocation after [`CtcssDetector::new`]: all state is fixed-size arrays
//! (about 5 KB per detector). Cost per input sample is 50 complex
//! multiply-accumulates (≈ 125 k/s per channel).

use num_complex::Complex;

use crate::tone::{Tone, CTCSS_TONES};
use crate::SUBAUDIO_RATE_HZ;

/// Number of tones in the bank.
const TONES: usize = CTCSS_TONES.len();

/// Samples per evaluation block (50 ms at 2.5 ksps).
pub const BLOCK_SAMPLES: usize = 125;

/// Blocks in the sliding window.
pub const BLOCKS: usize = 8;

/// Window length in samples (1000 = 400 ms).
pub const WINDOW_SAMPLES: usize = BLOCK_SAMPLES * BLOCKS;

/// Window length, ms.
pub const WINDOW_MS: u32 = (WINDOW_SAMPLES as u32 * 1000) / SUBAUDIO_RATE_HZ;

/// Evaluation interval (one block), ms.
pub const HOP_MS: u32 = (BLOCK_SAMPLES as u32 * 1000) / SUBAUDIO_RATE_HZ;

/// Lock: winner over runner-up, dB.
pub const LOCK_RUNNER_UP_DB: f32 = 6.0;

/// Lock: winner over the mean of the other 49 bins, dB.
pub const LOCK_BANK_DB: f32 = 10.0;

/// Hold: locked tone over the mean of the other bins, dB (and no other bin
/// may beat it by [`LOCK_RUNNER_UP_DB`]).
pub const HOLD_BANK_DB: f32 = 6.0;

/// Consecutive qualifying evaluations needed to lock (or switch).
pub const LOCK_RUNS: u32 = 2;

/// Lock: phase coherence of the winner across the window's blocks (see
/// [`Evaluation::coherence`]). A steady tone scores ≥ 0.94 even at 6 dB SNR;
/// DCS (whose strongest line can otherwise pass the ratio tests) and noise
/// stay ≤ 0.83.
pub const LOCK_COHERENCE: f32 = 0.88;

/// Hold: the locked tone's amplitude must stay above this fraction of its
/// running reference level (−6 dB), so a tone that stops is noticed while
/// the window is still half full of it.
pub const HOLD_LEVEL_FRACTION: f32 = 0.5;

/// Consecutive failed hold evaluations that drop the lock (100 ms).
pub const DROP_MISSES: u32 = 2;

/// Smallest tone amplitude that can qualify, in input units (peak, ±1.0 =
/// full-scale deviation): 0.03 is 75 Hz on a 12.5 kHz channel (±2.5 kHz full
/// scale) and 150 Hz on a 25 kHz channel. Real encoders send 350–750 Hz. This
/// keeps a near-silent sub-audio band (an unmodulated carrier with a little
/// hum) from "detecting" its loudest bin.
pub const MIN_LEVEL: f32 = 0.03;

/// The power-ratio and amplitude thresholds in linear form.
const LOCK_RUNNER_UP: f32 = 3.981_072; // 10^(6/10)
const LOCK_BANK: f32 = 10.0; // 10^(10/10)
const HOLD_BANK: f32 = 3.981_072; // 10^(6/10)

/// Phase clock period: tone frequencies are whole tenths of Hz, so
/// `ω·n = 2π · tenths · n / 25 000` and the phase of every tone repeats
/// every 25 000 samples (10 s).
const PHASE_PERIOD: u64 = SUBAUDIO_RATE_HZ as u64 * 10;

/// A change of the detector's decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ToneDecision {
    /// The tone now detected; `None` = the previously detected tone was lost.
    pub tone: Option<Tone>,
    /// Input sample count (since [`CtcssDetector::new`] or the last
    /// [`CtcssDetector::reset`]) at the end of the block whose evaluation
    /// made the change.
    pub at_sample: u64,
}

impl ToneDecision {
    /// [`ToneDecision::at_sample`] in ms of sub-audio time.
    pub fn at_ms(&self) -> u64 {
        self.at_sample * 1000 / u64::from(SUBAUDIO_RATE_HZ)
    }
}

/// The figures of one window evaluation, for diagnostics and tests.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Evaluation {
    /// Strongest bin.
    pub best: Tone,
    /// Its amplitude estimate, input units (peak).
    pub level: f32,
    /// Strongest bin over the runner-up, dB.
    pub runner_up_db: f32,
    /// Strongest bin over the mean of the other bins, dB.
    pub bank_db: f32,
    /// Block coherence of the strongest bin, 0..=1: `|Σ bᵢ·ū^i|² / (B·Σ|bᵢ|²)`
    /// over its [`BLOCKS`] per-block DFT values `bᵢ`, with `u` the mean
    /// block-to-block phase step (so a tone slightly off the bin frequency
    /// still scores high). A steady sinusoid on the
    /// bin scores ≈ 1; energy that only looks like a line over the whole
    /// window (DCS, noise) scores low.
    pub coherence: f32,
    /// The evaluation qualified `best` for a lock.
    pub qualifies: bool,
}

/// A held tone.
#[derive(Clone, Copy, Debug)]
struct Lock {
    /// Index into [`CTCSS_TONES`].
    tone: usize,
    /// Consecutive failed hold evaluations.
    misses: u32,
    /// Running reference amplitude (input units).
    level: f32,
}

/// Sliding-window CTCSS detector for one channel. See the module docs.
#[derive(Clone, Debug)]
pub struct CtcssDetector {
    /// Per-sample rotation `e^{-jω}` of each tone.
    rot: [Complex<f32>; TONES],
    /// Running oscillator `e^{-jωn}` of each tone, within the current block.
    osc: [Complex<f32>; TONES],
    /// Current block's `Σ x[n]·e^{-jωn}` per tone.
    acc: [Complex<f32>; TONES],
    /// Current block's `Σ e^{-jωn}` per tone (for the DC correction).
    osc_sum: [Complex<f32>; TONES],
    /// Current block's `Σ x[n]`.
    x_sum: f32,
    /// Samples in the current block.
    fill: usize,
    /// Completed blocks (DC-corrected), ring of `BLOCKS`.
    blocks: [[Complex<f32>; TONES]; BLOCKS],
    /// Next ring slot to write.
    head: usize,
    /// Completed blocks since reset, saturating at `BLOCKS`.
    filled: usize,
    /// Samples since reset.
    samples: u64,
    /// Tone index that qualified in the last evaluation, and its run length.
    candidate: Option<(usize, u32)>,
    /// The current lock.
    locked: Option<Lock>,
    /// The last evaluation.
    last: Option<Evaluation>,
}

impl Default for CtcssDetector {
    fn default() -> Self {
        CtcssDetector::new()
    }
}

impl CtcssDetector {
    /// A detector with an empty window.
    pub fn new() -> Self {
        let fs = f64::from(SUBAUDIO_RATE_HZ);
        let mut rot = [Complex::new(1.0f32, 0.0); TONES];
        for (r, &tenths) in rot.iter_mut().zip(CTCSS_TONES.iter()) {
            let w = std::f64::consts::TAU * f64::from(tenths) / 10.0 / fs;
            *r = Complex::new(w.cos() as f32, -(w.sin() as f32));
        }
        let mut d = CtcssDetector {
            rot,
            osc: [Complex::new(1.0, 0.0); TONES],
            acc: [Complex::new(0.0, 0.0); TONES],
            osc_sum: [Complex::new(0.0, 0.0); TONES],
            x_sum: 0.0,
            fill: 0,
            blocks: [[Complex::new(0.0, 0.0); TONES]; BLOCKS],
            head: 0,
            filled: 0,
            samples: 0,
            candidate: None,
            locked: None,
            last: None,
        };
        d.start_block();
        d
    }

    /// Forget everything (empty window, no lock, sample count 0), e.g. at
    /// carrier-up. Allocation-free.
    pub fn reset(&mut self) {
        self.acc = [Complex::new(0.0, 0.0); TONES];
        self.osc_sum = [Complex::new(0.0, 0.0); TONES];
        self.x_sum = 0.0;
        self.fill = 0;
        self.blocks = [[Complex::new(0.0, 0.0); TONES]; BLOCKS];
        self.head = 0;
        self.filled = 0;
        self.samples = 0;
        self.candidate = None;
        self.locked = None;
        self.last = None;
        self.start_block();
    }

    /// Feed sub-audio samples at [`SUBAUDIO_RATE_HZ`] (any length). Returns
    /// the last decision change made while consuming them, if any; call
    /// [`CtcssDetector::current`] for the standing decision. A slice of up to
    /// one block (125 samples) makes at most one change.
    pub fn push(&mut self, samples: &[f32]) -> Option<ToneDecision> {
        let mut change = None;
        let mut rest = samples;
        while !rest.is_empty() {
            let take = (BLOCK_SAMPLES - self.fill).min(rest.len());
            let (now, later) = rest.split_at(take);
            self.accumulate(now);
            rest = later;
            if self.fill == BLOCK_SAMPLES {
                if let Some(c) = self.end_block() {
                    change = Some(c);
                }
            }
        }
        change
    }

    /// The tone currently detected, if any.
    pub fn current(&self) -> Option<Tone> {
        self.locked.map(|l| Tone::Ctcss(CTCSS_TONES[l.tone]))
    }

    /// The most recent window evaluation (`None` until the window is full).
    pub fn last_evaluation(&self) -> Option<Evaluation> {
        self.last
    }

    /// Samples consumed since [`CtcssDetector::new`] / the last reset.
    pub fn samples(&self) -> u64 {
        self.samples
    }

    fn accumulate(&mut self, xs: &[f32]) {
        for &x in xs {
            for k in 0..TONES {
                let o = self.osc[k];
                self.acc[k] += o * x;
                self.osc_sum[k] += o;
                self.osc[k] = o * self.rot[k];
            }
            self.x_sum += x;
        }
        self.fill += xs.len();
        self.samples += xs.len() as u64;
    }

    /// Set each oscillator to the exact phase of the block's first sample on
    /// the absolute clock (so blocks add coherently) and clear the block sums.
    fn start_block(&mut self) {
        let n = self.samples % PHASE_PERIOD;
        for (o, &tenths) in self.osc.iter_mut().zip(CTCSS_TONES.iter()) {
            let cycles = (u64::from(tenths) * n % PHASE_PERIOD) as f64 / PHASE_PERIOD as f64;
            let (s, c) = (std::f64::consts::TAU * cycles).sin_cos();
            *o = Complex::new(c as f32, -(s as f32));
        }
        self.acc = [Complex::new(0.0, 0.0); TONES];
        self.osc_sum = [Complex::new(0.0, 0.0); TONES];
        self.x_sum = 0.0;
        self.fill = 0;
    }

    fn end_block(&mut self) -> Option<ToneDecision> {
        // Remove the block's mean (carrier offset, slow drift) from every bin.
        let mean = self.x_sum / BLOCK_SAMPLES as f32;
        let slot = &mut self.blocks[self.head];
        for ((b, &acc), &osc) in slot.iter_mut().zip(&self.acc).zip(&self.osc_sum) {
            *b = acc - osc * mean;
        }
        self.head = (self.head + 1) % BLOCKS;
        self.filled = (self.filled + 1).min(BLOCKS);
        self.start_block();
        if self.filled < BLOCKS {
            return None;
        }
        self.evaluate()
    }

    fn evaluate(&mut self) -> Option<ToneDecision> {
        let mut power = [0.0f32; TONES];
        for (k, p) in power.iter_mut().enumerate() {
            let s: Complex<f32> = self.blocks.iter().map(|b| b[k]).sum();
            *p = s.norm_sqr();
        }
        let total: f32 = power.iter().sum();
        let (mut best, mut second) = (0usize, usize::MAX);
        for k in 1..TONES {
            if power[k] > power[best] {
                second = best;
                best = k;
            } else if second == usize::MAX || power[k] > power[second] {
                second = k;
            }
        }
        let pb = power[best];
        let ps = power[second].max(f32::MIN_POSITIVE);
        let rest_mean = ((total - pb) / (TONES - 1) as f32).max(f32::MIN_POSITIVE);
        let level = 2.0 * pb.sqrt() / WINDOW_SAMPLES as f32;
        let coherence = self.coherence(best);
        let qualifies = pb >= LOCK_RUNNER_UP * ps
            && pb >= LOCK_BANK * rest_mean
            && level >= MIN_LEVEL
            && coherence >= LOCK_COHERENCE;
        self.last = Some(Evaluation {
            best: Tone::Ctcss(CTCSS_TONES[best]),
            level,
            runner_up_db: db(pb / ps),
            bank_db: db(pb / rest_mean),
            coherence,
            qualifies,
        });

        // Lock / switch candidate.
        self.candidate = if qualifies {
            match self.candidate {
                Some((k, n)) if k == best => Some((k, n + 1)),
                _ => Some((best, 1)),
            }
        } else {
            None
        };
        let at_sample = self.samples;
        if let Some((k, n)) = self.candidate {
            if n >= LOCK_RUNS && self.locked.map(|l| l.tone) != Some(k) {
                self.locked = Some(Lock {
                    tone: k,
                    misses: 0,
                    level,
                });
                return Some(ToneDecision {
                    tone: Some(Tone::Ctcss(CTCSS_TONES[k])),
                    at_sample,
                });
            }
        }

        // Hold / drop.
        let lock = self.locked.as_mut()?;
        let l = lock.tone;
        let others = ((total - power[l]) / (TONES - 1) as f32).max(f32::MIN_POSITIVE);
        let l_level = 2.0 * power[l].sqrt() / WINDOW_SAMPLES as f32;
        // A different winner only counts against the lock when it beats the
        // locked tone by the lock margin; a voice harmonic or a hum line that
        // briefly edges ahead does not split the call.
        let hit = power[best] < LOCK_RUNNER_UP * power[l]
            && power[l] >= HOLD_BANK * others
            && l_level >= HOLD_LEVEL_FRACTION * lock.level;
        if hit {
            lock.misses = 0;
            lock.level += 0.25 * (l_level - lock.level);
            return None;
        }
        lock.misses += 1;
        if lock.misses < DROP_MISSES {
            return None;
        }
        self.locked = None;
        Some(ToneDecision {
            tone: None,
            at_sample,
        })
    }

    /// Phase coherence of bin `k` across the window's blocks, after removing
    /// a constant per-block phase step (a tone slightly off the bin
    /// frequency rotates steadily from block to block):
    /// `|Σ bᵢ·ū^i|² / (B·Σ|bᵢ|²)` with `u` the mean phase step.
    fn coherence(&self, k: usize) -> f32 {
        // Blocks oldest first.
        let block = |i: usize| self.blocks[(self.head + i) % BLOCKS][k];
        let mut step = Complex::new(0.0f32, 0.0);
        let mut energy = 0.0f32;
        for i in 0..BLOCKS {
            energy += block(i).norm_sqr();
            if i > 0 {
                step += block(i) * block(i - 1).conj();
            }
        }
        let n = step.norm();
        let unrot = if n > 0.0 {
            (step / n).conj()
        } else {
            Complex::new(1.0, 0.0)
        };
        let mut r = Complex::new(1.0f32, 0.0);
        let mut sum = Complex::new(0.0f32, 0.0);
        for i in 0..BLOCKS {
            sum += block(i) * r;
            r *= unrot;
        }
        sum.norm_sqr() / (BLOCKS as f32 * energy).max(f32::MIN_POSITIVE)
    }
}

fn db(ratio: f32) -> f32 {
    10.0 * ratio.log10()
}
