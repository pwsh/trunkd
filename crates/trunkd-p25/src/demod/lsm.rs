// SPDX-License-Identifier: GPL-3.0-or-later
//! LSM (linear simulcast / CQPSK) demodulator — the primary path (spec §6).
//!
//! Self-contained π/4-DQPSK demodulator with a first-order decision-directed
//! PLL and a Gardner timing error detector using linear interpolation.

use num_complex::Complex;

use crate::bits::Dibit;

const MAX_PLL: f32 = std::f32::consts::PI / 3.0;
const PLL_GAIN: f32 = 0.1;
const PHASE_ERROR_CLAMP: f32 = 0.3;
const AGC_GAIN: f32 = 0.05;
const MAX_AGC: f32 = 500.0;

/// Spacing of the decision-directed loop's stable points (§6 step 7).
///
/// The loop nulls `soft − idealPhase(slice(soft))`, and the π/4-DQPSK decision
/// points sit π/2 apart, so *every* `pll = −θ + m·π/2` is an equally stable
/// equilibrium. `MAX_PLL = π/3` spans 2π/3 > π/2, so **two** of them fit inside
/// the capture range as soon as `|θ| > π/2 − MAX_PLL = π/6` — a residual
/// carrier offset of only 400 Hz at 4800 baud. Which one the acquisition
/// transient lands on depends on the sample the stream starts at, and landing
/// on the neighbouring one rotates every dibit by one position: sync never
/// correlates and the channel decodes nothing, forever.
const PLL_AMBIGUITY: f32 = std::f32::consts::FRAC_PI_2;

/// The principal interval `(−π/4, π/4]` holds exactly one equilibrium.
///
/// DEVIATION (documented): the spec keeps `MAX_PLL = π/3` and this code still
/// does — the clamp is untouched — but a settled lock outside `±π/4` is now
/// folded back (below). That makes the *unambiguous* capture range ±600 Hz
/// rather than the nominal ±800 Hz, which is the honest figure: beyond 400 Hz
/// the pre-existing loop was already a coin flip between two equilibria, and
/// the coin was tossed by the acquisition transient. 0–600 Hz becomes
/// deterministic and correct (400–600 Hz was 50/50 before); 600–800 Hz becomes
/// deterministically wrong (it was 50/50 before). A tuner that is 600 Hz off at
/// 851 MHz is 0.7 ppm out, well past what the §14 feedback is meant to absorb.
const PLL_PRINCIPAL: f32 = std::f32::consts::FRAC_PI_4;

/// Consecutive symbols outside the principal interval that mark a *settled*
/// false lock rather than a fade excursion (0.1 s at 4800 baud).
///
/// Measured on a control-channel test capture: a correctly locked decode leaves the
/// principal interval only for short fade transients (worst run 87 symbols),
/// while a false lock sits outside it for 3200–8100 symbols at a stretch. The
/// threshold is 5.5× the worst correct excursion and 6.7× below the shortest
/// false-lock dwell, so a healthy decode never trips it.
const PLL_FALSE_LOCK_SYMBOLS: u32 = 480;

const RING: usize = 64;
const RING_MASK: usize = RING - 1;

/// One demodulated symbol: soft phase plus sliced dibit.
#[derive(Debug, Clone, Copy)]
pub struct Symbol {
    /// Differential phase in radians.
    pub soft: f32,
    /// Sliced dibit.
    pub hard: Dibit,
}

/// The LSM demodulator.
pub struct LsmDemodulator {
    sps: f32,
    half_sps: f32,
    ted_gain: f32,
    max_timing_adj: f32,

    ring: [Complex<f32>; RING],
    /// Total samples pushed; sample k lives at `ring[k & RING_MASK]`.
    sample_count: u64,
    /// Absolute (fractional) sample index of the next symbol instant.
    symbol_pos: f64,

    sample_gain: f32,
    pll: f32,
    prev_mid: Complex<f32>,
    prev_cur: Complex<f32>,
    prev_sym: Complex<f32>,
    /// Consecutive symbols with `|pll| > PLL_PRINCIPAL` (false-lock guard).
    pll_outside: u32,
}

impl LsmDemodulator {
    /// New demodulator for `sps` samples per symbol at the decimated rate.
    pub fn new(sps: f64) -> Self {
        let sps = sps as f32;
        LsmDemodulator {
            sps,
            half_sps: sps / 2.0,
            ted_gain: sps / 4.0,
            max_timing_adj: sps / 25.0,
            ring: [Complex::new(0.0, 0.0); RING],
            sample_count: 0,
            // Start far enough in that mid/current interpolation has data.
            symbol_pos: f64::from(sps),
            sample_gain: 1.0,
            pll: 0.0,
            prev_mid: Complex::new(0.0, 0.0),
            prev_cur: Complex::new(0.0, 0.0),
            prev_sym: Complex::new(0.7, 0.7),
            pll_outside: 0,
        }
    }

    /// Current PLL value in radians (for frequency-error feedback, §14).
    pub fn pll(&self) -> f32 {
        self.pll
    }

    /// Reset the PLL (on tuner frequency-change events).
    pub fn reset_pll(&mut self) {
        self.pll = 0.0;
        self.pll_outside = 0;
    }

    /// Linear interpolation at absolute fractional position `pos`.
    #[inline]
    fn interp(&self, pos: f64) -> Complex<f32> {
        let i0 = pos.floor() as u64;
        let mu = (pos - pos.floor()) as f32;
        let a = self.ring[(i0 as usize) & RING_MASK];
        let b = self.ring[((i0 + 1) as usize) & RING_MASK];
        a + (b - a) * mu
    }

    /// Push one decimated-rate sample; produces at most one symbol.
    #[inline]
    pub fn push(&mut self, s: Complex<f32>) -> Option<Symbol> {
        // Robustness: a single NaN/Inf input sample would otherwise poison the
        // ring, the TED and the PLL permanently (NaN survives `clamp`, and a
        // NaN `symbol_pos` floors to a saturating `as u64` of 0, which makes
        // every subsequent sample look like a symbol instant). Substitute zero
        // and keep the sample clock intact; the reference guards the same way
        // (`constrain()` returns 0 for non-finite inputs).
        let s = if s.re.is_finite() && s.im.is_finite() {
            s
        } else {
            Complex::new(0.0, 0.0)
        };
        self.ring[(self.sample_count as usize) & RING_MASK] = s;
        self.sample_count += 1;

        // Need samples through symbol_pos + 1 (interpolation upper index).
        let needed = self.symbol_pos.floor() as u64 + 2;
        if self.sample_count < needed {
            return None;
        }

        // 1. Linear-interpolate the *middle* (half a symbol back) and
        //    *current* points.
        let mut cur = self.interp(self.symbol_pos);
        let mut mid = self.interp(self.symbol_pos - f64::from(self.half_sps));

        // 2. AGC on the current point's magnitude. The gain update is skipped
        //    entirely when the magnitude is zero or non-finite (matching the
        //    reference, which only adapts on a usable magnitude): ramping the
        //    gain toward MAX_AGC on a dead/poisoned sample would blow the loop
        //    up on the first real sample that follows.
        let mag = (cur.re * cur.re + cur.im * cur.im).sqrt();
        if mag > 0.0 && mag.is_finite() {
            let required = (1.0 / mag).min(MAX_AGC);
            self.sample_gain += (required - self.sample_gain) * AGC_GAIN;
            self.sample_gain = self.sample_gain.min(required);
        }
        cur *= self.sample_gain;
        mid *= self.sample_gain;

        // 3–5. Differential demod of middle and current, rotated by the PLL.
        let pll_rot = Complex::new(self.pll.cos(), self.pll.sin());
        let mid_demod = mid * self.prev_mid.conj() * pll_rot;
        let cur_demod = cur * self.prev_cur.conj() * pll_rot;
        let soft = cur_demod.im.atan2(cur_demod.re);

        // 6. Gardner TED (sign convention per spec — bit-exactness critical).
        let timing = (self.prev_sym.re - cur_demod.re) * mid_demod.re
            + (self.prev_sym.im - cur_demod.im) * mid_demod.im;
        // `clamp` propagates NaN, so a non-finite error must be zeroed *before*
        // it can reach `symbol_pos` (which never recovers once it is NaN).
        let timing = if timing.is_finite() {
            timing.clamp(-self.max_timing_adj, self.max_timing_adj) * self.ted_gain
        } else {
            0.0
        };
        self.symbol_pos += f64::from(timing);

        // 7. Decision-directed PLL update.
        //
        // DEVIATION (documented, no message impact): `Dibit::slice(0.0)` yields
        // D10 here, whereas the reference emits D00 for a soft value of exactly
        // 0.0. Spec §4.1 specifies the D10 mapping and §6 is silent, so the
        // spec's slicer is kept. An exactly-zero soft value only occurs on a
        // dead/zeroed sample, which the `soft != 0.0` guard below excludes from
        // the PLL anyway.
        let hard = Dibit::slice(soft);
        if soft != 0.0 {
            let err = soft - hard.ideal_phase();
            // Same NaN caveat as the TED above: clamp alone would not rescue it.
            let err = if err.is_finite() {
                err.clamp(-PHASE_ERROR_CLAMP, PHASE_ERROR_CLAMP)
            } else {
                0.0
            };
            self.pll -= err * PLL_GAIN;
            self.pll = self.pll.clamp(-MAX_PLL, MAX_PLL);
        }

        // 7b. False-lock escape (see `PLL_AMBIGUITY`). The loop cannot tell the
        // equilibria apart from the constellation alone, so resolve the
        // ambiguity the way a correctly tuned receiver does: prefer the
        // smallest carrier offset consistent with the data. Once the PLL has
        // *settled* outside the principal interval — long enough that it cannot
        // be a fade transient — fold it one step back toward zero.
        //
        // On a healthy decode the counter never reaches the threshold, so this
        // is arithmetically inert there (bit-exact with the pre-fix loop).
        if self.pll.abs() > PLL_PRINCIPAL {
            self.pll_outside += 1;
            if self.pll_outside >= PLL_FALSE_LOCK_SYMBOLS {
                self.pll -= PLL_AMBIGUITY.copysign(self.pll);
                self.pll_outside = 0;
            }
        } else {
            self.pll_outside = 0;
        }

        // 9. Advance.
        self.prev_mid = mid;
        self.prev_cur = cur;
        self.prev_sym = cur_demod;
        self.symbol_pos += f64::from(self.sps);

        Some(Symbol { soft, hard })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Synthesize an ideal π/4-DQPSK baseband at `sps` samples/symbol and
    /// check the demodulator recovers the dibit stream.
    #[test]
    fn recovers_ideal_dqpsk() {
        let sps = 25_000.0 / 4800.0;
        let mut demod = LsmDemodulator::new(sps);
        let dibits = [0u8, 1, 2, 3, 1, 1, 3, 0, 2, 2, 1, 3, 0, 0, 3, 2]
            .iter()
            .cycle()
            .take(200)
            .copied()
            .collect::<Vec<_>>();
        // Build the phase trajectory: phase advances by the dibit's ideal
        // differential phase per symbol; rectangular pulse (no shaping) is
        // fine for a functional check.
        let total = (dibits.len() as f64 * sps) as usize;
        let mut samples = Vec::new();
        let mut phase = 0.0f64;
        for n in 0..total {
            let sym = ((n as f64) / sps) as usize;
            let step = f64::from(Dibit::from_value(dibits[sym]).ideal_phase()) / sps;
            phase += step;
            samples.push(Complex::new(phase.cos() as f32, phase.sin() as f32));
        }
        let mut out = Vec::new();
        for s in samples {
            if let Some(sym) = demod.push(s) {
                out.push(sym.hard.value());
            }
        }
        // Skip the settling head, then require a long exact run against the
        // transmitted cycle (alignment found by search).
        assert!(out.len() > 150, "symbols out: {}", out.len());
        let tail = &out[out.len() - 100..];
        let pattern = [0u8, 1, 2, 3, 1, 1, 3, 0, 2, 2, 1, 3, 0, 0, 3, 2];
        let matched = (0..16).any(|shift| {
            tail.iter()
                .enumerate()
                .all(|(i, &v)| v == pattern[(i + shift) % 16])
        });
        assert!(matched, "tail: {tail:?}");
    }

    /// Build an ideal π/4-DQPSK sample stream of `symbols` symbols at `sps`.
    fn dqpsk_samples(sps: f64, symbols: usize) -> Vec<Complex<f32>> {
        let pattern = [0u8, 1, 2, 3, 1, 1, 3, 0, 2, 2, 1, 3, 0, 0, 3, 2];
        let total = (symbols as f64 * sps) as usize;
        let mut samples = Vec::with_capacity(total);
        let mut phase = 0.0f64;
        for n in 0..total {
            let sym = ((n as f64) / sps) as usize;
            let d = Dibit::from_value(pattern[sym % pattern.len()]);
            phase += f64::from(d.ideal_phase()) / sps;
            samples.push(Complex::new(phase.cos() as f32, phase.sin() as f32));
        }
        samples
    }

    /// A single NaN sample must not permanently kill the demodulator: before
    /// the guards, the NaN reached `symbol_pos` (NaN floor → saturating
    /// `as u64` → 0), so *every* subsequent sample emitted a NaN "symbol".
    #[test]
    fn survives_a_single_nan_sample() {
        let sps = 25_000.0 / 4800.0;
        let mut samples = dqpsk_samples(sps, 400);
        let nan_at = samples.len() / 2;
        samples[nan_at] = Complex::new(f32::NAN, f32::NAN);

        let mut demod = LsmDemodulator::new(sps);
        // Symbols emitted strictly after the NaN, and how far past it.
        let mut after: Vec<(usize, Symbol)> = Vec::new();
        for (n, &s) in samples.iter().enumerate() {
            if let Some(sym) = demod.push(s) {
                if n > nan_at {
                    after.push((n, sym));
                }
            }
        }

        // The rate must stay ~one symbol per `sps` samples, not one per sample.
        let window = samples.len() - nan_at - 1;
        let expected = window as f64 / sps;
        assert!(
            (after.len() as f64) < expected * 1.2,
            "emitted {} symbols in {window} samples (expected ~{expected:.0}); \
             the demodulator is free-running",
            after.len()
        );
        assert!(
            (after.len() as f64) > expected * 0.8,
            "emitted only {} symbols in {window} samples (expected ~{expected:.0})",
            after.len()
        );

        // Once the poisoned sample has flushed out of the ring (RING samples)
        // every soft value must be finite again.
        let recovered: Vec<f32> = after
            .iter()
            .filter(|(n, _)| *n > nan_at + 2 * RING)
            .map(|(_, s)| s.soft)
            .collect();
        assert!(recovered.len() > 100, "too few post-NaN symbols to check");
        assert!(
            recovered.iter().all(|v| v.is_finite()),
            "non-finite soft values persist after the NaN"
        );
        assert!(demod.pll().is_finite(), "PLL poisoned: {}", demod.pll());
    }

    const BAUD: f64 = 4800.0;
    const CHANNEL_RATE: f64 = 25_000.0;

    /// One P25 frame's worth of dibits: sync + NID(nac, duid) + `body` filler
    /// dibits, with the transmitter's status-dibit cadence applied (§7.2).
    fn frame_dibits(nac: u16, duid: u8, body: usize) -> Vec<Dibit> {
        let nid = crate::edac::bch::encode_nid(nac, duid);
        let mut payload: Vec<Dibit> = crate::framer::SYNC_DIBITS
            .iter()
            .map(|&v| Dibit::from_value(v))
            .collect();
        payload.extend((0..32).map(|i| Dibit::from_value(((nid >> (62 - 2 * i)) & 3) as u8)));
        payload.extend((0..body).map(|i| Dibit::from_value((i % 4) as u8)));
        // A status dibit every 36th transmitted dibit.
        let mut out = Vec::with_capacity(payload.len() + payload.len() / 35 + 1);
        let mut count = 0u32;
        for d in payload {
            out.push(d);
            count += 1;
            if count % 36 == 35 {
                out.push(Dibit::D00Plus1);
                count += 1;
            }
        }
        out
    }

    /// Modulate `dibits` as ideal π/4-DQPSK at `CHANNEL_RATE` with a residual
    /// carrier offset of `offset_hz`, appending to `out`. The phase
    /// accumulator is carried in `phase` so segments splice seamlessly.
    fn modulate(dibits: &[Dibit], offset_hz: f64, phase: &mut f64, out: &mut Vec<Complex<f32>>) {
        let sps = CHANNEL_RATE / BAUD;
        let carrier = std::f64::consts::TAU * offset_hz / CHANNEL_RATE;
        let total = (dibits.len() as f64 * sps) as usize;
        for n in 0..total {
            let d = dibits[((n as f64) / sps) as usize];
            *phase += f64::from(d.ideal_phase()) / sps + carrier;
            out.push(Complex::new(phase.cos() as f32, phase.sin() as f32));
        }
    }

    /// Run the real 25 kHz front end + LSM demod + framer over `samples`,
    /// starting at sample `skip`, and report (sync_count, nid_count).
    fn decode(samples: &[Complex<f32>], skip: usize) -> (u64, u64) {
        let mut fe = crate::dsp::FrontEnd::new(25_000, crate::dsp::DemodMode::Lsm).unwrap();
        let mut demod = LsmDemodulator::new(fe.samples_per_symbol);
        let mut framer = crate::framer::Framer::new();
        for &s in &samples[skip..] {
            if let Some(v) = fe.push(s) {
                if let Some(sym) = demod.push(v) {
                    framer.process(sym.soft, sym.hard, &mut |_| {});
                }
            }
        }
        (framer.stats.sync_count, framer.stats.nid_count)
    }

    /// Re-run the decode and report whether the PLL settled on the principal
    /// equilibrium (the direct signature of the false lock this guards).
    fn demod_pll_is_principal(samples: &[Complex<f32>], skip: usize) -> bool {
        let mut fe = crate::dsp::FrontEnd::new(25_000, crate::dsp::DemodMode::Lsm).unwrap();
        let mut demod = LsmDemodulator::new(fe.samples_per_symbol);
        for &s in &samples[skip..] {
            if let Some(v) = fe.push(s) {
                demod.push(v);
            }
        }
        demod.pll().abs() <= PLL_PRINCIPAL
    }

    /// Regression (phase-dependent false lock): with a residual carrier offset
    /// in the ambiguous band, the decision-directed PLL has two equilibria
    /// inside `MAX_PLL`, and which one the acquisition transient lands on used
    /// to depend on the sample the stream happened to start at — decoding
    /// either everything or *nothing at all*. Every start phase must lock.
    #[test]
    fn locks_from_every_start_phase() {
        let mut dibits = Vec::new();
        for _ in 0..40 {
            dibits.extend(frame_dibits(0x34, 7, 217));
        }
        let mut phase = 0.0;
        let mut samples = Vec::new();
        // 500 Hz: inside MAX_PLL (±800 Hz) but past the ±400 Hz point where a
        // second equilibrium enters the capture range.
        modulate(&dibits, 500.0, &mut phase, &mut samples);

        for skip in 0..12 {
            let (syncs, nids) = decode(&samples, skip);
            assert!(
                syncs > 8 && nids > 8,
                "start phase {skip}: sync {syncs}, nid {nids} — the demodulator \
                 failed to acquire from this sample phase"
            );
            assert!(
                demod_pll_is_principal(&samples, skip),
                "start phase {skip}: the PLL settled outside the principal interval"
            );
        }
    }

    /// The same guarantee for the opposite sign of residual offset.
    #[test]
    fn locks_from_every_start_phase_negative_offset() {
        let mut dibits = Vec::new();
        for _ in 0..40 {
            dibits.extend(frame_dibits(0x34, 7, 217));
        }
        let mut phase = 0.0;
        let mut samples = Vec::new();
        modulate(&dibits, -500.0, &mut phase, &mut samples);

        for skip in 0..8 {
            let (syncs, nids) = decode(&samples, skip);
            assert!(
                syncs > 8 && nids > 8,
                "start phase {skip}: sync {syncs}, nid {nids}"
            );
        }
    }

    /// An all-zero (dead) input must not ramp the AGC toward MAX_AGC — the
    /// gain update is skipped when the magnitude is zero.
    #[test]
    fn agc_holds_on_dead_input() {
        let sps = 25_000.0 / 4800.0;
        let mut demod = LsmDemodulator::new(sps);
        for _ in 0..2000 {
            demod.push(Complex::new(0.0, 0.0));
        }
        assert!(
            (demod.sample_gain - 1.0).abs() < 1e-6,
            "AGC ramped on silence: {}",
            demod.sample_gain
        );
    }
}
