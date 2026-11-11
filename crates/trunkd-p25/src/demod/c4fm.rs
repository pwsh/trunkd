// SPDX-License-Identifier: GPL-3.0-or-later
//! C4FM demodulator (spec §5).
//!
//! Differential demodulation against an interpolated one-symbol-back sample,
//! symbol clock, and the sync-driven equalizer (PLL + gain) with the spec's
//! constants: initial gain **1.219**, loop gain 0.15, PLL clamp ±π/3, gain
//! clamp [1.0, 1.25], recalibrate threshold π/8.
//!
//! Simplifications permitted by spec §15 ("reference-specific
//! optimizations"): a single sync correlator with **linear** interpolation
//! replaces the dual-correlator + 128-phase MMSE interpolator + coordinate
//! hill-climb; sync-driven timing recentering uses a coarse/fine local
//! search with the specified step sizes.

use num_complex::Complex;

use crate::bits::Dibit;
use crate::demod::lsm::Symbol;
use crate::framer::SYNC_DIBITS;

const EQUALIZER_LOOP_GAIN: f32 = 0.15;
const MAX_PLL: f32 = std::f32::consts::PI / 3.0;
const MIN_GAIN: f32 = 1.0;
const MAX_GAIN: f32 = 1.25;
const INITIAL_GAIN: f32 = 1.219;
const RECALIBRATE_THRESHOLD: f32 = std::f32::consts::PI / 8.0;
const SYNC_DETECT_THRESHOLD: f32 = 80.0;

const RING: usize = 512;
const RING_MASK: usize = RING - 1;

fn sync_ideal(x: usize) -> f32 {
    const P: f32 = 3.0 * std::f32::consts::FRAC_PI_4;
    if SYNC_DIBITS[x] == 1 {
        P
    } else {
        -P
    }
}

/// The C4FM demodulator.
pub struct C4fmDemodulator {
    sps: f32,

    /// Raw complex samples (for differential demod).
    iq_ring: [Complex<f32>; RING],
    /// Per-sample differential phases (soft samples).
    soft_ring: [f32; RING],
    sample_count: u64,

    /// Absolute fractional index of the next symbol instant.
    symbol_pos: f64,

    pll: f32,
    gain: f32,
    initialized: bool,

    /// Last 24 equalized symbols for the sync correlator.
    sym_ring: [f32; 48],
    sym_ptr: usize,
}

impl C4fmDemodulator {
    /// New demodulator for `sps` samples per symbol at the decimated rate.
    pub fn new(sps: f64) -> Self {
        C4fmDemodulator {
            sps: sps as f32,
            iq_ring: [Complex::new(0.0, 0.0); RING],
            soft_ring: [0.0; RING],
            sample_count: 0,
            symbol_pos: sps * 2.0,
            pll: 0.0,
            gain: INITIAL_GAIN,
            initialized: false,
            sym_ring: [0.0; 48],
            sym_ptr: 0,
        }
    }

    /// Current PLL value (for frequency-error feedback).
    pub fn pll(&self) -> f32 {
        self.pll
    }

    /// Reset the PLL (frequency-change events).
    pub fn reset_pll(&mut self) {
        self.pll = 0.0;
        self.initialized = false;
    }

    #[inline]
    fn soft_at(&self, pos: f64) -> f32 {
        let i0 = pos.floor() as u64;
        // Both interpolation taps must already have been written this call, or
        // the read wraps the ring and returns data from RING samples ago. See
        // `readable_through`.
        debug_assert!(
            i0 + 1 < self.sample_count,
            "soft_at({pos}) reads index {} but only {} samples are written",
            i0 + 1,
            self.sample_count
        );
        let mu = (pos - pos.floor()) as f32;
        let a = self.soft_ring[(i0 as usize) & RING_MASK];
        let b = self.soft_ring[((i0 + 1) as usize) & RING_MASK];
        a + (b - a) * mu
    }

    /// Whether `soft_at(pos)` (and hence a sync correlation ending at `pos`)
    /// only touches samples that have actually been written: the interpolator
    /// reads `floor(pos)` and `floor(pos) + 1`, and the newest written index is
    /// `sample_count - 1`.
    #[inline]
    fn readable_through(&self, pos: f64) -> bool {
        (pos.floor() as u64) + 1 < self.sample_count
    }

    #[inline]
    fn equalize(&self, sym: f32) -> f32 {
        (sym + self.pll) * self.gain
    }

    /// Correlate 24 equalized, interpolated symbols ending at `end_pos`
    /// (spaced sps) against the ideal sync.
    fn sync_correlation(&self, end_pos: f64) -> f32 {
        let mut score = 0.0f32;
        for x in 0..24 {
            let pos = end_pos - f64::from(self.sps) * (23 - x) as f64;
            score += sync_ideal(x) * self.equalize(self.soft_at(pos));
        }
        score
    }

    /// On sync: refine symbol timing with a coarse (sps/8 within ±sps/2) then
    /// fine (sps/16 within ±sps/8) local search.
    ///
    /// Forward candidates are skipped when they would run past the newest
    /// written sample. At the emission point only `sample_count - symbol_pos`
    /// (≈2–3) samples of lookahead exist, so the far forward candidates would
    /// otherwise interpolate against ring entries from RING samples earlier —
    /// stale data scored as if it were the current sync.
    fn optimize_timing(&mut self, end_pos: f64) -> f64 {
        let sps = f64::from(self.sps);
        let mut best_pos = end_pos;
        let mut best = self.sync_correlation(end_pos);
        let coarse = sps / 8.0;
        let mut k = -4i32;
        while k <= 4 {
            let cand = end_pos + coarse * f64::from(k);
            if self.readable_through(cand) {
                let s = self.sync_correlation(cand);
                if s > best {
                    best = s;
                    best_pos = cand;
                }
            }
            k += 1;
        }
        let fine = sps / 16.0;
        let base = best_pos;
        let mut k = -2i32;
        while k <= 2 {
            let cand = base + fine * f64::from(k);
            if self.readable_through(cand) {
                let s = self.sync_correlation(cand);
                if s > best {
                    best = s;
                    best_pos = cand;
                }
            }
            k += 1;
        }
        best_pos
    }

    /// Equalizer adaptation from a scored sync (`getCorrection`, spec §5.3).
    fn apply_correction(&mut self, end_pos: f64) {
        let mut balance_plus = 0.0f32;
        let mut balance_minus = 0.0f32;
        let mut gain_sum = 0.0f32;
        const IDEAL_MAG: f32 = 3.0 * std::f32::consts::FRAC_PI_4;
        for x in 0..24 {
            let pos = end_pos - f64::from(self.sps) * (23 - x) as f64;
            let resampled = self.equalize(self.soft_at(pos));
            let ideal = sync_ideal(x);
            if ideal > 0.0 {
                balance_plus += resampled - ideal;
            } else {
                balance_minus += resampled - ideal;
            }
            gain_sum += IDEAL_MAG - resampled.abs();
        }
        balance_plus /= -11.0;
        balance_minus /= -13.0;
        let corr = ((balance_plus + balance_minus) / 2.0)
            .clamp(-std::f32::consts::FRAC_PI_2, std::f32::consts::FRAC_PI_2);
        let gain_adj = gain_sum / (24.0 * IDEAL_MAG);

        if corr.abs() > RECALIBRATE_THRESHOLD {
            self.initialized = false;
        }
        if self.initialized {
            self.pll += corr * EQUALIZER_LOOP_GAIN;
            self.gain += gain_adj * EQUALIZER_LOOP_GAIN;
        } else {
            // Seize directly (no smoothing).
            self.pll += corr;
            self.gain += gain_adj;
            self.initialized = true;
        }
        self.pll = self.pll.clamp(-MAX_PLL, MAX_PLL);
        self.gain = self.gain.clamp(MIN_GAIN, MAX_GAIN);
    }

    /// Push one decimated-rate sample; produces at most one symbol.
    pub fn push(&mut self, s: Complex<f32>) -> Option<Symbol> {
        let idx = (self.sample_count as usize) & RING_MASK;
        self.iq_ring[idx] = s;
        // Differential phase vs the interpolated sample one symbol back.
        let pos = self.sample_count as f64 - f64::from(self.sps);
        let soft = if pos >= 0.0 {
            let i0 = pos.floor() as u64;
            let mu = (pos - pos.floor()) as f32;
            let a = self.iq_ring[(i0 as usize) & RING_MASK];
            let b = self.iq_ring[((i0 + 1) as usize) & RING_MASK];
            let prev = a + (b - a) * mu;
            let d = s * prev.conj();
            d.im.atan2(d.re)
        } else {
            0.0
        };
        self.soft_ring[idx] = soft;
        self.sample_count += 1;

        if (self.sample_count as f64) < self.symbol_pos + 2.0 {
            return None;
        }

        let raw = self.soft_at(self.symbol_pos);
        let sym = self.equalize(raw);
        self.sym_ring[self.sym_ptr] = sym;
        self.sym_ring[self.sym_ptr + 24] = sym;
        self.sym_ptr = (self.sym_ptr + 1) % 24;

        // Sync correlator over the last 24 equalized symbols. `sym_ring` is
        // duplicated (48 entries), so the 24 oldest-to-newest symbols are the
        // contiguous slice starting at `sym_ptr`.
        let start = self.sym_ptr;
        let mut score = 0.0f32;
        for x in 0..24 {
            score += sync_ideal(x) * self.sym_ring[start + x];
        }
        if score > SYNC_DETECT_THRESHOLD {
            let refined = self.optimize_timing(self.symbol_pos);
            self.apply_correction(refined);
            // Recenter the symbol clock on the refined position.
            self.symbol_pos = refined;
        }

        self.symbol_pos += f64::from(self.sps);
        let hard = Dibit::slice(sym);
        Some(Symbol { soft: sym, hard })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ideal 4FSK: frequency deviation proportional to the symbol value,
    /// integrated phase; the differential phase over one symbol equals the
    /// dibit's ideal phase.
    #[test]
    fn recovers_ideal_c4fm() {
        let sps = 25_000.0 / 4800.0;
        let mut demod = C4fmDemodulator::new(sps);
        // Preamble of sync patterns then a payload cycle, as C4FM adaptation
        // needs syncs to calibrate.
        let mut dibits: Vec<u8> = Vec::new();
        for _ in 0..6 {
            dibits.extend_from_slice(&SYNC_DIBITS);
            dibits.extend_from_slice(&[0, 1, 2, 3, 0, 2, 1, 3, 3, 1, 0, 0]);
        }
        let total = (dibits.len() as f64 * sps) as usize;
        let mut phase = 0.0f64;
        let mut out = Vec::new();
        for n in 0..total {
            let sym = ((n as f64) / sps) as usize;
            let step = f64::from(Dibit::from_value(dibits[sym]).ideal_phase()) / sps;
            phase += step;
            // The equalizer expects the constellation compression the 1.219
            // gain corrects; ideal input decodes fine within the gain clamp.
            if let Some(s) = demod.push(Complex::new(phase.cos() as f32, phase.sin() as f32)) {
                out.push(s.hard.value());
            }
        }
        // After the first sync the stream should contain at least one exact
        // sync pattern detection worth of dibits.
        let pat: Vec<u8> = SYNC_DIBITS.to_vec();
        let found = out.windows(pat.len()).any(|w| w == pat.as_slice());
        assert!(found, "sync pattern not recovered: {out:?}");
    }
}
