# P25 Phase 1 Decoder — Implementation Specification

**Purpose.** This document is a standalone, recreation-grade specification of a P25 Phase 1
(C4FM and LSM/simulcast) receive-side decoder: complex baseband IQ in, decoded trunking
messages / voice frames / call events out. It was extracted from a working reference
implementation (sdrtrunk, Java) by direct source audit, with every constant, lookup table,
polynomial, and state machine reproduced exactly. An engine built purely from this document —
without consulting any other source — must achieve decode parity with the reference on real
signals.

**Scope.** Demodulation (C4FM and LSM), sync/framing, NID/BCH, all Phase 1 EDAC codes,
interleaving, TSBK/PDU/HDU/LDU/TDU message assembly, trunking-control message layouts needed
for voice-channel following (grants + IDEN/frequency-band), channel→frequency math, and the
traffic-channel tracking state machine. IMBE voice decode internals (the 144-bit frame's
inner deinterleave/Golay/Hamming/derandomize) are **out of scope**: the reference hands raw
144-bit frames to an external vocoder library at exactly the boundary specified in §12, and
this engine must do the same.

**Priority note.** The target system is an LSM (linear simulcast, CQPSK) network. The LSM
demodulator (§6) is the primary path; C4FM (§5) is required for completeness.

---

## 1. Conventions

- **Bit order:** MSB-first everywhere. Bit index 0 of any message is the most significant /
  first-received bit. A multi-bit field read from bits `a..b` (ascending index) has bit `a`
  as its MSB.
- **Dibit:** one 4-level symbol = 2 bits, `value = bit1*2 + bit2` (bit1 = MSB, first into the
  message; bit2 = LSB, second). Dibit N of a stream occupies bit positions `2N` (MSB) and
  `2N+1` (LSB).
- **Dibit ↔ phase mapping (π/4-DQPSK differential phase per symbol):**

  | Dibit | bits | value | ideal phase |
  |---|---|---|---|
  | D01_PLUS_3 | 01 | 1 | +3π/4 |
  | D00_PLUS_1 | 00 | 0 | +π/4 |
  | D10_MINUS_1 | 10 | 2 | −π/4 |
  | D11_MINUS_3 | 11 | 3 | −3π/4 |

  Bit-error distance between two dibits: 0 if equal, 2 if `xor == 3`, else 1.
- **Symbol rate:** 4800 symbols/s (9600 bits/s).
- **"Hex codeword" / "hex symbol":** a 6-bit field, MSB-first (lowest bit index = MSB).
- All frequencies in Hz, all times in ms unless stated.

---

## 2. Signal path overview

```
IQ (channel rate, e.g. 25 kHz complex) 
  → power-of-2 decimation (half-band cascade)        §3
  → baseband low-pass (Remez)                        §3
  → root-raised-cosine matched filter (I and Q)      §3  ← note the /2 quirk
  → differential demod + symbol timing + PLL         §5 (C4FM) or §6 (LSM)
  → soft symbol (phase) → dibit slicing              §4
  → sync correlator (0x5575F5FF77FF)                 §4
  → NID capture (33 dibits, skip status) → BCH       §7.3, §8.1
  → framer state machine (DUID → frame length)       §7
  → per-DUID pipeline:
      TSBK/PDU: deinterleave §9 → trellis §8.5 → CRC §8.4 → message parse §10/§11
      HDU: Golay18 §8.2 → RS(63,47,17) §8.6
      LDU1/LDU2: 9×144-bit IMBE frames → vocoder;  Hamming10 §8.3 → RS §8.6 → LC/ES
      TDULC: Golay24 §8.2 → link control
  → trunking state machine (grants, bands, call tracking)  §11, §13
  → frequency-error feedback (auto-PPM)              §14
```

---

## 3. Front end: decimation and filters

### 3.1 Decimation
Input sample rate `sr` must be > 9600 (reject otherwise).

```
decimation = 1
while sr / decimation >= 38400: decimation *= 2
decimatedRate = sr / decimation          # e.g. 25000 → stays 25000
samplesPerSymbol (sps) = decimatedRate / 4800   # 25000 → 5.208333…
```

Decimation stages are half-band FIR decimate-by-2 cascades:
- ÷2: single 63-tap half-band, HAMMING window.
- ÷4: 23-tap BLACKMAN half-band, then the ÷2 stage.
- ÷8: 15-tap BLACKMAN half-band, then the ÷4 chain.

Half-band tap generation (odd length N = 4m+3): center tap 0.5; for **odd** offsets from
center, `sin(offset·π/2) / (offset·π) · window(idx)`; **even** offsets (other than the
center) are exactly 0. (Correction from the parity build: `sin(offset·π/2)` vanishes at even
offsets, so the nonzero taps are the odd ones — a half-band filter's defining property.)

### 3.2 Baseband low-pass filter
Remez / Parks-McClellan equiripple design (no hardcoded taps). Order estimated with the
Herrmann (1973) estimator; grid density 16; pass/stop ripple 0.01 dB both bands. Ripple
amplitude `= (10^(dB/20) − 1)/(10^(dB/20) + 1)`; band weight `= 1/(rippleAmp/maxRippleAmp)`.

| Path | passband edge | stopband edge |
|---|---|---|
| C4FM | 5200 Hz | 6500 Hz |
| LSM  | 7250 Hz | 8000 Hz |

The wider LSM filter is deliberate and load-bearing (simulcast spectra are wider).

### 3.3 Root-raised-cosine matched filter — **critical quirk**
Applied separately to I and Q. Parameters: nominal `sps = decimatedRate/4800`,
`span = 16 symbols`, `alpha = 0.2`, GNU-Radio `firdes` RRC tap formula, taps normalized to
unity DC gain (sum-normalized).

**QUIRK (must replicate):** the reference's RRC generator *halves the samples-per-symbol
internally* before computing taps. With decimatedRate 25000 it designs for
`sps = 2.604166`, giving `taps = round(2.604166 × 16) = 42` taps (not 83). This is a
documented historical mistake retained for compatibility; the P25 path inherits it. A
"correct" RRC here will change the effective passband and break bit-exact parity.

### 3.4 Interpolators
- **Polyphase (C4FM differential demod):** 8-tap, 128-phase MMSE FIR interpolator
  (GNU Radio `gr_mmse_fir` table). Phase index `= (int)(128·mu)`.
- **Linear (LSM, and C4FM sub-symbol steps):** `x1 + (x2 − x1)·mu`, mu clamped to [0,1].

### 3.5 Construction-time rate validation (normative)

Every front-end constructor is fallible. The channel constructors return `P25Error`:
`SampleRateTooLow { rate, min }`, `RateNotDivisible { sample_rate, channel_rate }`,
`InvalidChannelRate`, and `Dsp(String)` for a message raised by a DSP stage.

- **`FrontEnd::new(sample_rate, mode)`** rejects `sample_rate <= 9600` (§3.1) **and**, beyond
  that, any rate whose *decimated* rate is below **2 × the mode's stopband edge** — 16 kHz for
  LSM's 8000 Hz edge, 13 kHz for C4FM's 6500 Hz edge. The §3.2 low-pass has cutoff
  `(pass + stop)/2`; below that bound the cutoff sits past 0.5 cycles/sample, so the
  "low-pass" aliases instead of filtering. The validated 25 kHz and 50 kHz rates are
  unaffected.
- **`Ddc::try_new(sample_rate, channel_rate, offset_hz)`** requires `channel_rate > 0`,
  `sample_rate > 0`, and `sample_rate` an exact integer multiple of `channel_rate` — the
  cascade has no fractional resampler, and a zero channel rate divides by zero. `Ddc::new`
  stays a panicking wrapper for callers that pass known-good rates.
- **DDC tail design.** The Kaiser low-pass after the half-band cascade has stopband edge
  `channel_rate / 2` and passband edge `min(12 kHz, 0.96 × stop)`. The design is well-formed
  only while `pass < stop`: a flat 12 kHz edge gives `stop <= pass` for every channel rate
  ≤ 24 kHz, i.e. a transition width ≤ 0 and an unbounded (`ceil(inf)`) tap count — an
  overflow panic in debug, a 1-tap passthrough with no anti-aliasing in release. The 50 kHz
  design keeps the classic 12 kHz pass edge and is unchanged tap-for-tap; the 25 kHz design
  now uses the alias-limited tail of §3.7.



### 3.7 DDC hot path: block API and tail design (implemented)

**Where the time went.** At 20 Msps the per-channel DDC cost 23.5 ns/input sample: NCO mix 3.1 ns
(13 %), half-band cascade + tail ~20 ns (85 %). The cascade was latency-bound scalar code:
`ComplexFir` advanced its ring with an integer `%` per insert and walked the taps with two
data-dependent branches (ring wrap, zero-tap skip), so nothing vectorized.

**Bit-exact rework (`filters.rs`, `ddc.rs`).** `ComplexFir` keeps a *mirrored* contiguous delay
line (each sample written at `pos` and `pos + n`, wrap by compare-and-reset) and a precomputed
non-zero `(offset, tap)` list in the **original tap order**, so every output is the same
sequence of IEEE operations as before. `HalfBandDecimator::process_block`, `FirDecimator::process_block`
and `Ddc::process_block` compute outputs in lanes of 4 with one independent accumulator per
output (vectorization *across* outputs, never across taps — no reassociation), and the NCO keeps
its f64 recursive-phasor semantics and 2048-sample renormalization cadence. Normative
consequence: **block and per-sample paths are bit-identical for any block partitioning**
(tests stream the same signal per-sample and in blocks of 1/7/64/1000/ragged and compare raw f32
bits; the `Ddc`/`FrontEnd` golden hashes are unchanged; every fixture decode is byte-identical).
`P25P1WidebandChannel` uses the block path. Measured (20 Msps → 50 kHz, per input sample):
per-sample `push` 23.45 → 20.79 ns; `process_block` **13.23 ns** (1.77×); 60 s of wideband air
≈ 30 s → ≈ 19 s wall (single channel; the per-channel path remains O(N) — see §3.6).

**Tail filter (25 kHz only, NOT bit-exact).** The Kaiser tail was designed with a hardcoded
12 kHz pass edge against `stop = channel_rate/2`; at 25 kHz that is a 500 Hz transition →
**1089 taps** (2.4 M capture) / **4529** (20 M). Only the LSM front end's ±8 kHz must be
alias-free, so `Ddc::tail_band_edges` keeps the classic design wherever it leaves ≥ 8.5 kHz of
transition (every rate ≥ 41 kHz, so 50 kHz is untouched) and otherwise uses `pass = 8.5 kHz`,
`stop = channel_rate − 8 kHz` at the same 60 dB: **65** / **269** taps (≥ 55 dB on every image
band folding into ±8 kHz, asserted). 25 kHz decodes: voice counts unchanged (imbe 486/486,
lc 13/13, es 12/12, wall 39.7 → 19.0 s); control 224 → 225 TSBK, grant/IDEN sets identical.

**Acquisition phase-dependence (found while validating; FIXED — see §6.2).** The LSM decision-directed
PLL could settle in a π/2-rotated false basin depending on the channel-rate stream's start phase;
it affected 50 kHz as well as 25 kHz and was in the demod, not the filter.

### 3.6 Polyphase channelizer (implemented) — `dsp::channelizer`

**Why.** The per-channel DDC (§3.1–§3.5) costs O(N) in the number of followed channels because every
channel mixes and decimates the *full* capture rate: measured 23.5 ns/sample per channel at 20 Msps
(NCO 3.1 ns, half-band cascade ~20 ns) ≈ 2× realtime → at most two concurrent channels on one core,
against a traffic-channel pool of 20. The channelizer filters the whole span **once** and is O(1) in
channel count.

**Structure.** Fred-Harris M2 (2×-oversampled) polyphase filterbank, a port of sdrtrunk
`ComplexPolyphaseChannelizerM2` (`PolyphaseChannelizerM2`, trait `Channelizer`). `M = fs / 12 500`
(must be even and ≥ 4: 1600 at 20 Msps; exact at 12.5 and 10 Msps too). One input block = `M/2`
samples; one `M`-point FFT per block (`rustfft`, planned once; no allocation in steady state).
**Per-bin output rate = 2·fs/M = 25 000 Hz exactly**, which is the `FrontEnd` working rate (§3.1):
feed a bin straight into `P25P1Channel::new(25_000, …)` with no resampling.

**Bin convention (normative).** Bin `k` is centred at `+k·fs/M` from the capture centre. The
top/middle block-map alternation cancels the block-rate phase exactly, so for every block
`Z_k[m] = (−1)^k · e^(−j2πk/M) · b_k(t_m)`. Hence a channel at an integer multiple of 12.5 kHz
from the centre lands on a bin **centre**, not a seam (tests `bin_centering_and_isolation`:
residual 0 Hz, unity amplitude ±0.05, DC output, neighbours ≥ 60 dB down;
`half_bin_offset_appears_at_quarter_rate`: centre + 6.25 kHz → +π/2 rad/sample). The test site's
channel plan and the capture centre are both on the 12.5 kHz raster, so every channel of that site
is a one-bin channel (at 20 Msps, `M = 1600`, a channel 4.45 MHz below centre → `One(1244)`); the design-time worry that on-grid channels straddle seams came
from sdrtrunk's `ChannelCalculator` *labelling* convention and does not apply here.

**Bin selection.** Residual within ±1/8 bin (±1562.5 Hz) of a centre → one bin plus a residual NCO;
otherwise the two straddling bins are synthesized as
`composite[m] = Z_k[m]·(−j)^(m+1) − Z_(k+1)[m]·(j)^(m+1)`, which expands to sdrtrunk's 2-point
IFFT (`Z_k ± Z_(k+1)`, alternating per block) followed by its `Fs/4` down-converter. No synthesis
low-pass is needed: both inputs are already band-limited by the analysis prototype and the two
−6.02 dB skirts sum flat across the seam.

**Prototype filter.** sinc·window, `taps_per_channel` (default 12; `M·taps` total), cutoff found by
bisection so the response is **exactly −6.02 dB at the bin edge** — the perfect-reconstruction
constraint two-bin synthesis relies on — with unity DC gain (no `×M` scaling: rustfft's inverse is
unnormalized). Measured at `M = 1600, T = 12`: 0.00 dB to 3 kHz, −0.78 dB @ 5 kHz, −4.37 @ 6 kHz,
−6.02 @ 6.25 kHz, −80 @ 9 kHz, −93 @ 12.5 kHz. "Flat to ±6 kHz" and "−6.02 dB at 6.25 kHz" are
mutually unrealizable; the PR constraint wins, and the parity numbers below show the edge droop
costs nothing (24 taps/channel changes decode counts by ~0.2 % for 35 % more CPU).

**Errors.** `Err` for: `fs` not a multiple of 12.5 kHz, odd `M`, `M < 4`, `taps < 2`, a frequency
beyond `fs/2 − bin spacing`, or one on bin 0 (the zero-IF DC/LO spike).

**Validation** (a 60 s, 20 Msps wideband capture of a live site, vs the DDC path on the same
tree): control channel — 1579 TSBK vs 1578 (1585 with the §7.8 framer fix), the 18-grant set and
both IDEN bands identical; voice channels within ~2 % (a few IMBE frames at call edges — a
group-delay/transient difference, not droop: unchanged at 24 taps/channel). Two-bin synthesis on
the same channels (capture pre-shifted by 6.25 kHz to force seams) matches or slightly beats one-bin
(1581 TSBK), as perfect reconstruction predicts.

**Throughput** (release, one thread, 60 s of 20 Msps air): DDC, 1 channel: 29.9 s. Channelizer:
1 channel 19.5 s, 3 channels 18.8 s, **11 channels 21.7 s** → ≈ 0.36 % of realtime per additional
channel, ~14× cheaper than per-channel DDCs at N = 11, and the gap grows linearly with N. Per-channel
decode output is bit-identical between the 3- and 11-channel runs (no cross-talk).

**Status.** Standalone module + `examples/channelizer_decode.rs`
(`--center --freq … [--taps-per-channel 12] [--shift <hz>]`). Not yet wired into
`P25P1WidebandChannel` / the recorder's traffic pool — that integration is the remaining step.

---

## 4. Symbols, slicing, and sync

### 4.1 Slicing (both demods)
Soft symbol = differential phase `φ = atan2(q, i)` after PLL rotation. Decision boundaries
0 and ±π/2:

```
φ >  0:  φ >  π/2 → D01 (+3)   else → D00 (+1)
φ <= 0:  φ < −π/2 → D11 (−3)   else → D10 (−1)
```

### 4.2 Frame sync pattern
`SYNC = 0x5575F5FF77FF` (48 bits = 24 dibits, mask 0xFFFFFFFFFFFF). Only ±3 symbols occur.
Dibit sequence MSB-first (values): 1,1,1,1,1,3,1,1,3,3,1,1,3,3,3,3,1,3,1,3,3,3,3,3.
Symbol sequence: `+3 +3 +3 +3 +3 −3 +3 +3 −3 −3 +3 +3 −3 −3 −3 −3 +3 −3 +3 −3 −3 −3 −3 −3`
— exactly 11×(+3) and 13×(−3).

### 4.3 Soft sync correlator
48-entry ring of soft symbols (written twice for wrap-free reads). Score
`= Σ_{x=0..23} SYNC_SYMBOL[x] · softSymbol[ptr+x]` where SYNC_SYMBOL is the ±3π/4 ideal
sequence. Theoretical max ≈ 133.2.

Thresholds: **detection 80**; **post-optimization acceptance 110** (C4FM equalized rescoring,
§5). The framer additionally uses a soft threshold of 60 for its own sync gate (§7.1).

---

## 5. C4FM demodulator

### 5.1 Differential demodulator
At channel rate (not decimated further): `sps = sr/4800` (5.2083 @25k),
`mu = frac(sps) = 0.2083`, `interpOffset = floor(sps) − 4 = 1`,
`bufferOverlap = floor(sps) + 4 = 9`. Per output sample:

```
iCur = interp(iBuf, offset + x, mu);  qCur = interp(qBuf, offset + x, mu)   # polyphase, 1 symbol back
dI = iPrev·iCur − (−qPrev)·qCur
dQ = iPrev·qCur + iCur·(−qPrev)
phase = atan2(dQ, dI)          # π/4-DQPSK phase advance over one symbol
```

### 5.2 Symbol processor + equalizer
Constants: `EQUALIZER_LOOP_GAIN = 0.15`, `MAX_PLL = π/3`, `MAX_GAIN = 1.25`,
`RECALIBRATE_THRESHOLD = π/8`, sync thresholds 80 (detect) / 80 (optimized) /
110 (equalized), NID length 33 dibits, sync length 24 dibits.

Symbol clock: `samplePoint -= 1` per sample; when `< 1`, emit a symbol and
`samplePoint += sps`.

Equalizer state: `pll = 0`, **`gain = 1.219`** at init (compensates constellation
compression from π/4-DQPSK demod of pulse-shaped 4FSK; LSM does not need this).
`equalize(sym) = (sym + pll) · gain`.

### 5.3 Sync handling and equalizer adaptation
Two correlators: primary at `samplePoint`, lagging at `samplePoint − sps/2`. On score > 80,
run a coordinate hill-climb (`optimize()`): step `sps/8` coarse then `sps/16` fine, minimum
step `sps/200`, max adjustment `sps/2` coarse / `sps` fine; objective = correlation of 24
interpolated+equalized symbols (spaced `sps`) against the ideal sync symbols.

Correction from a scored sync (`getCorrection`): compute balance over the 11 (+3) and
13 (−3) positions; `balancePlus3 /= −11`, `balanceMinus3 /= −13`,
`corr = (sum)/2` clamped ±π/2; `gainAdj = Σ(|ideal| − |resampled|) / (24 · 3π/4)`.
Apply: if `|pllAdj| > π/8` mark uninitialized (recalibrate); if initialized,
`pll += adj·0.15`, `gain += gainAdj·0.15`; else seize directly (no smoothing).
Clamp `pll` to ±π/3 and `gain` to [1.0, 1.25].

Noise gate (`isNoisy`): stddev of consecutive soft-sample differences across the sync span;
reject sync if `> (3π/4) · 2/sps · 1.2`.

NID validation: capture 33 dibits, skip index 11 (status dibit), BCH-decode (§8.1); accept
only if a DUID decodes **and** the optimized sync score > 110.

### 5.4 Sync timing-refinement bounds

The coarse (±sps/8, 9 candidates) and fine (±sps/16) timing searches around a detected sync
evaluate positions *after* the sample that triggered detection, but at emission only about
2–3 samples of lookahead exist. Interpolation at `pos` reads `floor(pos)` and
`floor(pos) + 1`; a candidate whose upper tap is at or beyond the newest written sample index
must be **skipped** (`readable_through(pos)`: `floor(pos) + 1 < sample_count`). The sample
ring wraps, so such a read silently scores data from a full ring (512 samples) earlier as if
it belonged to the current sync.

---

## 6. LSM demodulator (primary path for simulcast)

Self-contained π/4-DQPSK demodulator with a **first-order decision-directed PLL** (not a
Costas loop) and a Gardner timing error detector using **linear** interpolation.

Constants: `MAX_PLL = π/3`, objective magnitude 1.0, `pllGain = 0.1`, `tedGain = sps/4`,
`maxTimingAdj = sps/25`, `samplesPerHalfSymbol = sps/2`.
Init: `sampleGain = 1`, `pll = 0`, `prevSymbolI = prevSymbolQ = 0.7`.

Per symbol (when `samplePoint < 1`):

1. Linear-interpolate two I/Q points: *middle* at `samplePoint`, *current* at
   `samplePoint + samplesPerHalfSymbol` (half a symbol later).
2. AGC: `mag = sqrt(iCur² + qCur²)`; `requiredGain = min(1/mag, 500)`;
   `sampleGain += (requiredGain − sampleGain) · 0.05`; then
   `sampleGain = min(sampleGain, requiredGain)`; apply to all four interpolated values.
3. `pllI = cos(pll)`, `pllQ = sin(pll)`.
4. Differentially demodulate the *middle* point (multiply by conjugate of previous middle),
   rotate by PLL.
5. Differentially demodulate the *current* point (× conj of previous current), rotate by
   PLL; `softSymbol = atan2(qSym, iSym)`.
6. Gardner TED:
   `timingAdj = (prevSymI − iSym)·iMidDemod + (prevSymQ − qSym)·qMidDemod`,
   clamp to ±`sps/25`, multiply by `sps/4`, `samplePoint += timingAdj`.
7. PLL update: if `softSymbol != 0`: `hard = slice(softSymbol)`;
   `phaseError = clamp(softSymbol − hard.idealPhase, ±0.3)`;
   `pll −= phaseError · 0.1`; clamp `pll` to ±π/3.
8. Feed `(soft, hard)` to the framer's soft-sync detector; if a sync was just confirmed,
   feed the current `pll` to the frequency-error feedback (§14). Feed `hard` to the dibit
   assembler.
9. `prev = cur`; `samplePoint += sps`.

Reset `pll = 0` on any tuner frequency-change event.

### 6.1 Numerical robustness (normative)

The reference's `constrain()` returns 0 for a non-finite argument; the same discipline is
required here, because NaN survives `clamp` and one poisoned value is permanent.

- A **non-finite input sample** (either component) is replaced by zero at the demod entry,
  before it reaches the ring — the sample clock still advances. A NaN reaching `symbol_pos`
  floors to a saturating `as u64` of 0, after which *every* subsequent sample looks like a
  symbol instant, forever (measured: 1041 "symbols" from 1041 samples, all soft = NaN).
- The **Gardner timing error** (step 6) and the **PLL phase error** (step 7) are zeroed when
  non-finite *before* they are clamped.
- The **AGC gain update** (step 2) is skipped entirely when `mag` is zero or non-finite; only
  the gain application runs. Ramping toward `MAX_AGC` (500) on a dead or poisoned sample
  blows the loop up on the first real sample that follows.

---

### 6.2 Decision-directed PLL false lock and its escape (normative)

**Failure.** Step 7's PLL nulls `soft − ideal_phase(slice(soft))`. The π/4-DQPSK decision points are
π/2 apart, so every `pll = −θ + m·π/2` is an equally stable equilibrium. Because `MAX_PLL = π/3`
spans 2π/3 > π/2, **two equilibria fit inside the capture range as soon as the residual carrier
offset exceeds π/6 rad/symbol ≈ 400 Hz**. Which basin the loop lands in is decided during the
front end's FIR warm-up (the AGC winds toward `MAX_AGC` on ~1e-6-magnitude samples whose phase
still carries full weight, since `atan2` is magnitude-blind) and therefore by the channel-rate
stream's start phase. The wrong basin rotates every dibit one position: the sync correlator never
fires and the channel is silent forever. Measured on the control fixture: `--offset 400000` →
every start phase locks; `400500` (≈460 Hz residual) → bistable (25 kHz: phases 2/3/11/21 dead;
**50 kHz: 1/2/6/11 dead** — the parity baseline had merely landed well); `401000` → no phase locks
(1.31 rad > `MAX_PLL`, a genuine capture-range limit). Dead vs. live runs have identical
`timing`/`symbol_pos` trajectories and PLL values exactly π/2 apart. This is inherited from the
reference (`P25P1DemodulatorLSM` has the same loop and the same ±800 Hz `MAX_PLL`); sdrtrunk hides
it by continuously retuning the tuner from `processPLLError`, which this engine only reports (§14).

**Escape (implemented, `demod/lsm.rs`).** A correct decode leaves the principal interval
(−π/4, π/4] only for short fade transients (worst consecutive run measured: 87 symbols at 50 kHz,
70 at 25 kHz); a false lock dwells there for thousands. When the PLL has been outside the principal
interval for **480 consecutive symbols** (0.1 s; ≥ 5.5× the worst correct excursion, ≤ 1/6 of the
shortest false-lock dwell), fold it one π/2 step toward zero — i.e. prefer the smallest carrier
offset consistent with the data, as a correctly tuned receiver would. `MAX_PLL` is unchanged. On a
healthy decode the counter never reaches the threshold, so the arithmetic is **bit-exact** with the
previous loop (the 50 kHz control decode is byte-identical). After the fix every start phase locks
at both rates; recovered phases lose ~3 NIDs to the 0.1 s escape delay (tests
`locks_from_every_start_phase{,_negative_offset}`: 12 phases each, ±500 Hz, real 25 kHz
`FrontEnd` + LSM + `Framer`).

**Consequence for §14 / the recorder.** The unambiguous capture range is now ±600 Hz
(0–600 Hz: deterministically correct; 600–800 Hz: deterministically wrong; > 800 Hz: no lock). The
escape is a safety net, not the design: the recorder **must close the loop** on `FrequencyError`
(§14) — retune or apply a PPM correction so the residual offset stays well inside ±π/6 — exactly as
the reference does. Tuning a fixture with a deliberate 500 Hz mistune (`--offset 400500`) is the
worst case, not the norm.

## 7. Framing

### 7.1 Framer state and constants
- `DIBIT_LENGTH_NID = 33` (32 NID dibits + 1 interstitial status dibit).
- Soft-sync gate threshold in framer: **60**.
- NID fields: NAC = 12 bits at offset 0; DUID = 4 bits at offset 12.
- Startup: `dibitCounter = 58`, `statusSymbolDibitCounter = 36`.

### 7.2 Status symbols
The transmitter inserts one **status dibit after every 35 data dibits** (i.e. every 36th
dibit is status). The framer counts every dibit; when the counter reaches 36 that dibit is
diverted to a channel-status consumer, the counter resets to 0, and the dibit is **not**
appended to the assembling message. After a sync/NID (which consumed the NID-internal status
dibit at NID index 11), the counter restarts at 21 so the cadence stays aligned.

### 7.3 Sync → NID → frame
On sync detect (only honored when no assembler is active): reset a 33-dibit NID buffer.
Each subsequent dibit fills it; at 33 dibits run `checkNID`:

- Build a 64-bit message from the 33 dibits **skipping index 11** (status), each dibit
  contributing bit1 then bit2 (MSB-first).
- BCH(63,16,23) decode with NAC bias (§8.1). Fail (corrected count < 0) → stay in
  placeholder mode. Pass → track NAC, proceed.
- `nidDetected`: if DUID is unknown, coerce to PLACE_HOLDER and keep the previous NAC.
  If an assembler is still active: dispatch it if complete (discard completed placeholders);
  if incomplete, force-complete and account dropped bits. Sync-loss accounting:
  `dibitCounter −= 57`; positive remainder → report sync loss of `2×counter` bits, negative
  → report `−2×counter` dropped bits. Then `dibitCounter = 57`,
  `statusSymbolDibitCounter = 21`, and require assembly.
- The assembler starts at `dibitCounter == 57` — i.e. after 24 sync dibits + 32 NID dibits
  + 1 status dibit. If no explicit assembly is required but a NAC is tracked, start a
  PLACE_HOLDER assembler (discarded if it completes without a new sync).
- Completion: assembler full (buffer size = DUID message length, §7.4) **and**
  `statusSymbolDibitCounter == 35`; dispatch is delayed one dibit so a trailing status dibit
  is consumed first.
- Free-running sync loss: whenever `dibitCounter >= 4800` (1 s of dibits), subtract 4800 and
  emit a 9600-bit sync-loss report.

False syncs and the NAC gate are specified in §7.8; placeholder / force-complete accounting
in §7.9; assembly validity, late dispatch and `Framer::reset()` in §7.10.

### 7.4 DUID table
Buffer size (bits) = raw message length + null pad. Elapsed dibits
`= 56 + msgLen/2 + statusDibits + nullBits/2` (56 = 24 sync + 32 NID).

| DUID value | Unit | raw bits | status dibits | null bits | buffer bits | elapsed dibits |
|---|---|---|---|---|---|---|
| 0 | HDU | 648 | 11 | 10 | 658 | 396 |
| 3 | TDU | 0 | 2 | 28 | 28 | 72 |
| 5 | LDU1 | 1568 | 24 | 0 | 1568 | 864 |
| 7 | TSBK1 | 196 | 5 | 42 | 238 | 180 |
| 10 | LDU2 | 1568 | 24 | 0 | 1568 | 864 |
| 12 | PDU (header) | 196 | 5 | 42 | 238 | 180 |
| 15 | TDULC | 288 | 6 | 20 | 308 | 216 |
| — | PLACE_HOLDER | 1572 | 24 | 0 | 1572 | 866 |
| — | TSBK2 (internal) | 392 | 8 | 56 | 448 | 288 |
| — | TSBK3 (internal) | 588 | 10 | 0 | 588 | 360 |
| — | PDU_BLOCK_1..5 (internal) | 392/588/784/980/1176 | 8/10/13/16/19 | 56/0/14/28/42 | 448/588/798/1008/1218 | 288/360/468/576/684 |

Only DUID values {0,3,5,7,10,12,15} are producible from a NID; all others decode as UNKNOWN
→ placeholder. TSBK2/3 and PDU_BLOCK_n are internal continuation states reached by
reconfiguring a live assembler.

**Derivation — one table, two columns.** Both computed columns come from a single per-unit
triple `(raw bits, status dibits, null bits)` (`UnitLengths`):
`buffer_bits = raw + null` and `elapsed_dibits = 56 + raw/2 + status + null/2`, with
56 = `SYNC_NID_DIBITS` (24 sync + 32 NID). Every derived value equals the hand-tabulated
column above (pinned by test); stating the triple once keeps the two from drifting apart.

### 7.5 TSBK assembly (up to 3 blocks)
Each TSBK block is 196 interleaved bits. Pipeline per block: deinterleave (§9) → 1/2-rate
trellis (§8.5) → CRC-CCITT-16 (§8.4a) → parse (§10).

- Block 1 from buffer bits [0,196). If valid and `lastBlock` (decoded bit 0) → done; else
  reconfigure to TSBK2 (forced-completion guard: if buffer already ≥ 391 bits, treat as
  TSBK2 immediately).
- Block 2 from [196,392); guard ≥ 588 → TSBK3; else continue as block 1 logic.
- Block 3 from [392,588); always terminal.

### 7.6 PDU assembly (header + up to 5 blocks)
Header: bits [0,196) → same deinterleave+1/2-trellis+CRC as TSBK → header parse:

| Field | bits |
|---|---|
| confirmation required | 1 |
| direction | 2 |
| format | 3–7 (5 bits) |
| vendor | 16–23 |
| logical link ID | 24–47 (24 bits) |
| blocks to follow | 49–55 (7 bits) |
| header CRC (CCITT-16) | 80–95 |

If valid and blocksToFollow > 0, reconfigure through PDU_BLOCK_1..5; block *n* occupies
buffer bits `[196n, 196(n+1))`. Each block: deinterleave, then:
- **Confirmed** (header bit 1 set): 3/4-rate trellis → 144 bits; sequence number bits 0–6;
  payload = bits [16,144); CRC-9 (§8.4b) over the block. Valid on PASS/CORRECTED.
- **Unconfirmed:** 1/2-rate trellis → 96 bits; whole output is payload; no block CRC.

Sequence complete when block count equals blocksToFollow; dispatch then (block 5 always
dispatches).

### 7.7 TDU / TDULC
TDU: no payload — dispatch immediately. TDULC: 288 bits = twelve Golay(24,12,8) codewords
starting at bit 64-relative offsets (see §8.4e — in the reference the Golay walk starts at
absolute bit 64 of the raw frame including NID; within the 288-bit assembled message the
12×24-bit words are consecutive). Golay-correct each word (abort the frame if any word is
**uncorrectable** — status 2, §8.2), then RS(24,12,13) over the hex codewords exactly as LDU1 link control (§12.3).

### 7.8 False syncs: NID failure and the NAC gate

The soft correlator fires inside frames — voice payload correlates with the sync pattern
often enough to matter — so NID validation is what separates a real frame boundary from a
spurious hit.

**(a) A NID that fails BCH is not a NID.** When the 33-dibit capture completes and the BCH
check fails, the completing dibit must **not** be consumed. It falls through and is fed to
the assembly in flight as ordinary payload (status dibits are still diverted). Swallowing it
shifts the rest of that frame by two bits. This is the most damaging framing bug available
here: on a real traffic channel it corrupted roughly **45 % of IMBE frames** and halved the
LC/ES yield. Measured after the fix — link control 14 → **28** and encryption sync 12 → **28**
on one voice channel, control-channel TSBKs 224 → **227** (CRC failures 29 → 26, NID count
held at 253), and no more negative sync-loss reports (worst case had been −16634 bits).

**(b) False-NID gate.** BCH(63,16,t=11) accepts roughly 1 % of random 63-bit words, and those
accepts cluster at 9–11 corrections with a nonsense NAC — exactly the profile of a spurious
mid-frame sync. Once a **system NAC is established**, reject a candidate NID whose NAC
differs **and** whose correction count is at least
`NID_NAC_MISMATCH_CORRECTION_LIMIT` = **9**, treating it exactly like a BCH decode failure
(stay in placeholder mode, leave the assembly in flight alone). A NID that *agrees* with the
established NAC is accepted at any correction count.

The established NAC is the `NacTracker`'s tracked value when it has one, else `current_nac` —
the last NAC that actually produced a frame. The fallback is load-bearing: the tracker is
cleared by every one-second sync-loss report (§17.9), so on a traffic channel it is
unestablished throughout the gaps between transmissions. A decoded NAC of **0** is "no NAC
yet", never a system identity: it is never written to `current_nac` and so never displaces an
established one. Measured: a second voice channel's NIDs carrying a NAC other than the
system's dropped from 6 to 2, and its one spurious TDU disappeared.

### 7.9 Placeholder and dropped-bit accounting

- A **placeholder starts only at** `dibit_counter == ASSEMBLY_START_DIBIT` (**57**) — never
  immediately when the previous unit completes — and only while `current_nac > 0` (a NAC has
  actually been detected; the gate is the detected NAC, not the tracker). Starting one early
  charges dibits it never occupied.
- A **completed placeholder is not charged** its elapsed dibits (`finish` skips
  `Duid::PlaceHolder`). It stands in for dibits no frame claimed, so leaving the free counter
  running is precisely what lets it reach the one-second free-running sync-loss report — and
  hence the NAC-tracker reset — during a real signal gap. Charging it, or restarting the next
  placeholder at completion instead of at dibit 57, drives the counter and the reported sync
  loss negative.
- **Force-completion** (a NID arrives with an incomplete assembly) reports its dropped bits
  as a **positive** count — `target − buffer_len` — and skips the report entirely for a
  placeholder (nothing was lost). It then charges
  `min(elapsed_dibits(duid), dibit_counter − 57)`: the interrupted frame is charged exactly
  the air time observed since its own NID. **Deliberate deviation from the reference:** the
  reference instead *downgrades* the unit to a DUID whose length fits the dibits collected;
  capping reaches the same place — the frame is charged for exactly the air time it occupied
  — without inventing a DUID that was never transmitted.

### 7.10 Assembly validity, late dispatch, and reset

- **Truncated PDU.** A header promising `blocks_to_follow` blocks that the next sync cuts
  short is reported **`valid = false`** (`valid &= blocks_done >= header.blocks_to_follow`),
  however clean the blocks that did arrive were — not valid-with-fewer-blocks.
- **Complete-but-undispatched buffers.** If a NID arrives while an assembly is *full* but its
  one-dibit delayed dispatch has not yet fired, run the normal completion path
  (`on_buffer_full`) and then drop whatever continuation state it leaves behind — the frame
  this NID introduces supersedes it. Discarding the buffer silently loses TSBK block 2/3 and
  PDU blocks; a bare dispatch loses their decode and their elapsed-dibit charge.
- **`Framer::reset()`** clears every piece of framing state so decoding can restart cleanly:
  the soft correlator ring and its previous score, any assembly in flight (including a
  pending delayed dispatch), the NID capture, `dibit_counter` and `status_counter` back to
  the §7.1 startup values (58 / 36), the `NacTracker`, and `current_nac`. It deliberately
  **preserves** the aggregate `FramerStats` (lifetime totals) and the monotonic symbol
  counter behind the millisecond clock — winding that back would corrupt the NAC tracker's
  timestamp ordering. `P25P1Channel::frequency_changed` calls it on **every** retune, in both
  roles: the dibit stream is discontinuous there, and carried-over state produces a bogus
  first `SyncLoss` and a garbage force-complete on the new frequency.

---

## 8. Error control codes

### 8.1 BCH(63,16,23) — NID protection
GF(2^6), primitive polynomial `x^6 + x + 1` (0x43). n=63, k=16, t=11 (22 syndromes).
Codeword: 12-bit NAC (bits 0–11) + 4-bit DUID (12–15) + parity (16–62); bit 63 is an
overall even-parity bit **ignored by the decoder**.

- Antilog/log tables (size 64) built at runtime: `x <<= 1; if (x & 64) x ^= 0x43`;
  `a_pow[63] = 1`, `a_log[0] = 0` (sentinel). Sequence begins 1,2,4,8,16,32,3,6,12,24,48,35,…
- Syndromes: iterate **set** bits of the 63-bit word; for odd j,
  `syndrome[j] ^= a_pow[((j+1)·(62−i)) mod 63]` — note the **reversed bit index (62−i)**;
  even syndromes = GF-square of the corresponding odd ones. Loop excludes bit 63.
- Error-locator polynomial: simplified binary Berlekamp–Massey (inline). Root finding:
  Berlekamp trace-algorithm factorization with ad-hoc degree-1/2/3/4 solvers and recursive
  trace-map GCD splitting for degree ≥ 5. (Any correct BCH root-finder giving identical
  corrections is acceptable — e.g. Chien search — the algebra, not the algorithm, is
  normative.)
- If root count ≠ ELP degree, or duplicate roots → uncorrectable (corrected count −1).
  Else flip bit `62 − root` for each root; record correction count.
- **NAC-biased two-pass:** if pass 1 is uncorrectable and a tracked NAC (> 0) exists and
  differs from the received NAC field, overwrite the NAC field with the tracked NAC and
  decode once more.
- **NAC tracker:** map NAC → {count, timestamp}; max 3 entries (evict oldest timestamp);
  a NAC is "tracked" only when its count ≥ 3; reset on sync loss.

### 8.2 Golay codes
All use the shared 23-entry checksum table (generator polynomial 0xC75, Golay 24/12):

```
0x63A, 0x31D, 0x7B4, 0x3DA, 0x1ED, 0x6CC, 0x366, 0x1B3,
0x6E3, 0x54B, 0x49F, 0x475, 0x400, 0x200, 0x100, 0x080,
0x040, 0x020, 0x010, 0x008, 0x004, 0x002, 0x001
```

Checksum of a word = XOR of `CHECKSUMS[i]` over set data bits i (first 12 positions).
Syndrome = transmitted 11-bit checksum (bits 12–22) XOR calculated.

- **Golay24 (24,12,8):** 12 data + 11 checksum + 1 overall-parity bit (bit 23). Corrects ≤3
  errors; the parity bit detects a 4th. Decode: if syndrome 0 → fix parity bit if overall
  cardinality is odd. Else systematic **rotate-and-search** (Hank Wallace method): try the
  received word and each single trial-flip of the 23-bit core; for each, rotate left up to
  23 times looking for a syndrome of weight ≤ (3, or 2 after a trial flip); apply syndrome
  XOR to the checksum region, rotate back, and accept only if total flipped bits vs the
  original ≤ 3. Returns 0 = clean, 1 = corrected, 2 = uncorrectable.
- **Golay23 (23,12,7):** same table and rotate-and-search on the 23-bit word (no parity
  bit); returns the actual error count 0–3, or >3 = failure (word left intact).
- **Golay18 (18,6,8):** zero-extend to 24 bits (place the 18 bits at positions 6–23, upper
  6 = 0), run Golay24, extract bits 6–23.

**Detection in Golay24 — the perfect-code trap (normative).** Golay(23,12,7) is a *perfect*
code: every 23-bit word lies within distance 3 of exactly one codeword, so the inner
rotate-and-search always succeeds and can never itself signal failure. Detection must
therefore come from the **extended** code (d = 8, t = 3): count the **total** number of
24-bit positions the decode would flip — core correction **plus** the overall-parity repair —
and return status **2 (uncorrectable, word left unmodified)** when that total exceeds 3.
Weight ≤ 3 is still corrected exactly, and every one of the C(24,4) = 10626 weight-4 patterns
is detected. A decoder that reports "corrected" whenever the inner search returns a word
silently miscorrects every weight-4 pattern.

**Golay18 detection.** The six leading data bits are known zero on the wire, so a decode that
sets any of them is provably a ≥4-error miscorrection: test them after the Golay24 decode and
report uncorrectable — do **not** mask them off, which hides the miscorrection.

**Caller semantics (deliberately different).**
- **TDULC** aborts the whole frame on an uncorrectable Golay24 word (§7.7, spec 8.4d) — a
  live path now that detection works.
- **HDU** does **not** abort on an uncorrectable Golay18 hex: the outer RS(63,47,17) corrects
  up to 8 of the 36 hexes, so the raw hex is passed through and **only an RS failure**
  invalidates the header. Aborting there would throw away recoverable headers.

### 8.3 Hamming codes
- **Hamming10 (10,6,3):** 6 data bits + 4 parity. Per-data-bit checksums:
  `0xE, 0xD, 0xB, 0x7, 0x3, 0xC`. Syndrome = 4-bit checksum (bits 6–9) XOR accumulated.
  Correction by direct syndrome switch:

  | syndrome | flip (relative bit) | | syndrome | flip |
  |---|---|---|---|---|
  | 0 | none (return 0) | | 8 | 6 (parity 8) |
  | 1 | 9 (parity 1) | | 9 | uncorrectable |
  | 2 | 8 (parity 2) | | 10 | uncorrectable |
  | 3 | 4 (data 2) | | 11 | 2 (data 4) |
  | 4 | 7 (parity 4) | | 12 | 5 (data 1) |
  | 5 | uncorrectable | | 13 | 1 (data 5) |
  | 6 | uncorrectable | | 14 | 0 (data 6) |
  | 7 | 3 (data 3) | | 15 | uncorrectable |

- **Hamming15 (15,11,3):** checksums per data bit:
  `0x9, 0xD, 0xF, 0xE, 0x7, 0xA, 0x5, 0xB, 0xC, 0x6, 0x3, 0x8, 0x4, 0x2, 0x1`;
  syndrome→error-bit table (index = syndrome, value = bit to flip, −1 = clean):
  `−1, 14, 13, 10, 12, 6, 9, 4, 11, 0, 5, 7, 8, 1, 3, 2`.

### 8.4 CRC codes
Common decode pattern: accumulate `CHECKSUMS[i − messageStart]` over **set** message bits,
XOR with the transmitted CRC field → residual. Residual 0 **or all-ones** = pass (the
all-ones case covers the inverted/CCITT init). Otherwise attempt single-bit correction: find
the (unique) table row equal to the residual and flip that bit; no match = failure.

**Field orientation (correction from the parity build):** the transmitted CRC field is read
**MSB-first**, exactly like every other multi-bit field in this spec (§1). On real blocks the
MSB-first field yields residual 0 or all-ones; reading it LSB-first (a plausible mistake from
looking at the identity rows) makes every block fail. Use an orientation-independent
single-bit corrector to be safe.

**(a) CRC-CCITT-16** — protects decoded TSBK blocks and PDU headers (80 data bits + 16 CRC).
Polynomial 0x11021, init 0xFFFF. Table (96 entries: 80 data rows then 16 identity rows):

```
0x1BCB, 0x8DE5, 0xC6F2, 0x6B69, 0xB5B4, 0x52CA, 0x2175, 0x90BA, 0x404D,
0xA026, 0x5803, 0xAC01, 0xD600, 0x6310, 0x3998, 0x14DC, 0x027E, 0x092F,
0x8497, 0xC24B, 0xE125, 0xF092, 0x7059, 0xB82C, 0x5406, 0x2213, 0x9109,
0xC884, 0x6C52, 0x3E39, 0x9F1C, 0x479E, 0x2BDF, 0x95EF, 0xCAF7, 0xE57B,
0xF2BD, 0xF95E, 0x74BF, 0xBA5F, 0xDD2F, 0xEE97, 0xF74B, 0xFBA5, 0xFDD2,
0x76F9, 0xBB7C, 0x55AE, 0x22C7, 0x9163, 0xC8B1, 0xE458, 0x7A3C, 0x350E,
0x1297, 0x894B, 0xC4A5, 0xE252, 0x7939, 0xBC9C, 0x565E, 0x233F, 0x919F,
0xC8CF, 0xE467, 0xF233, 0xF919, 0xFC8C, 0x7656, 0x333B, 0x999D, 0xCCCE,
0x6E77, 0xB73B, 0xDB9D, 0xEDCE, 0x7EF7, 0xBF7B, 0xDFBD, 0xEFDE, 0x0001,
0x0002, 0x0004, 0x0008, 0x0010, 0x0020, 0x0040, 0x0080, 0x0100, 0x0200,
0x0400, 0x0800, 0x1000, 0x2000, 0x4000, 0x8000
```

**(b) CRC-9** — confirmed PDU data blocks. Polynomial 0x259, init 0x1FF. Detection only.
The 9-bit CRC field sits at bits 7–15 of the 144-bit decoded block; when accumulating, bits
before the CRC use `TABLE[i]` and bits after it use `TABLE[i − 9]` (skipping the CRC field's
positions). Pass if residual is 0 or 0x1FF. Table (135 entries):

```
0x1E7, 0x1F3, 0x1F9, 0x1FC, 0x0D2, 0x045, 0x122, 0x0BD, 0x15E, 0x083,
0x141, 0x1A0, 0x0FC, 0x052, 0x005, 0x102, 0x0AD, 0x156, 0x087, 0x143,
0x1A1, 0x1D0, 0x0C4, 0x04E, 0x00B, 0x105, 0x182, 0x0ED, 0x176, 0x097,
0x14B, 0x1A5, 0x1D2, 0x0C5, 0x162, 0x09D, 0x14E, 0x08B, 0x145, 0x1A2,
0x0FD, 0x17E, 0x093, 0x149, 0x1A4, 0x0FE, 0x053, 0x129, 0x194, 0x0E6,
0x05F, 0x12F, 0x197, 0x1CB, 0x1E5, 0x1F2, 0x0D5, 0x16A, 0x099, 0x14C,
0x08A, 0x069, 0x134, 0x0B6, 0x077, 0x13B, 0x19D, 0x1CE, 0x0CB, 0x165,
0x1B2, 0x0F5, 0x17A, 0x091, 0x148, 0x088, 0x068, 0x018, 0x020, 0x03C,
0x032, 0x035, 0x11A, 0x0A1, 0x150, 0x084, 0x06E, 0x01B, 0x10D, 0x186,
0x0EF, 0x177, 0x1BB, 0x1DD, 0x1EE, 0x0DB, 0x16D, 0x1B6, 0x0F7, 0x17B,
0x1BD, 0x1DE, 0x0C3, 0x161, 0x1B0, 0x0F4, 0x056, 0x007, 0x103, 0x181,
0x1C0, 0x0CC, 0x04A, 0x009, 0x104, 0x0AE, 0x07B, 0x13D, 0x19E, 0x0E3,
0x171, 0x1B8, 0x0F0, 0x054, 0x006, 0x02F, 0x117, 0x18B, 0x1C5, 0x1E2,
0x0DD, 0x16E, 0x09B, 0x14D, 0x1A6
```

**CRC-9 blind spot (documented; deliberately not "fixed").** Acceptance is
`residual == 0 || residual == 0x1FF` — the dual-residual rule inherited from the reference,
covering both the plain and the inverted (all-ones init) convention. Because the all-ones
residual is also accepted, any error pattern whose row XOR equals 0x1FF slips through.
Exhaustive enumeration over the 144-bit block finds **17 undetected weight-2 patterns**:

- **16** pairs of covered data bits whose rows XOR to 0x1FF — (0,86), (2,133), (6,139),
  (17,92), (20,59), (21,117), (24,49), (30,134), (31,74), (43,44), (52,72), (61,85), (71,77),
  (82,140), (84,100), (94,108). These are the dual-residual rule's own blind spot; a
  single-residual CRC-9 would catch them.
- **1** pair — (10,87) — where a data row equals the identity effect of a CRC-field bit
  (row 78 = 0x020), giving residual 0. That one is a property of this table/polynomial and no
  residual rule catches it.

Which convention real traffic uses has **not** been confirmed on air (the validation capture
contains no PDUs), so the acceptance semantics are left exactly as the reference has them and
the blind spot is pinned by test rather than narrowed on a guess.

**(c) CRC-32 (PDU multi-block)** — polynomial 0x104C11DB7, init 0xFFFFFFFF. The CRC covers
message bits from position 160 up to the CRC field; CRC field position: 224 (PDU1, 64
covered bits), 320 (PDU2, 160 bits), 416 (PDU3, 256 bits). Single-bit correction supported;
corrections flip `errorRow + 160`. The three tables nest: **PDU2 = 96 new leading rows ‖
PDU1; PDU3 = 96 new leading rows ‖ PDU2** (each table's final 32 rows are the identity rows
0x00000001…0x80000000). PDU1 (96 rows):

```
0x86FFAACC, 0x411F5BBD, 0xA08FADDE, 0x52275834, 0x2B7322C1,
0x95B99160, 0x48BC466B, 0xA45E2335, 0xD22F119A, 0x6B770616,
0x37DB0DD0, 0x198D0833, 0x8CC68419, 0xC663420C, 0x61512FDD,
0xB0A897EE, 0x5A34C52C, 0x2F7AEC4D, 0x97BD7626, 0x49BE35C8,
0x26BF943F, 0x935FCA1F, 0xC9AFE50F, 0xE4D7F287, 0xF26BF943,
0xF935FCA1, 0xFC9AFE50, 0x7C2DF1F3, 0xBE16F8F9, 0xDF0B7C7C,
0x6DE530E5, 0xB6F29872, 0x5919C2E2, 0x2EEC6FAA, 0x1516B90E,
0x08EBD25C, 0x061567F5, 0x830AB3FA, 0x43E5D726, 0x23926548,
0x13A9BC7F, 0x89D4DE3F, 0xC4EA6F1F, 0xE275378F, 0xF13A9BC7,
0xF89D4DE3, 0xFC4EA6F1, 0xFE275378, 0x7D732767, 0xBEB993B3,
0xDF5CC9D9, 0xEFAE64EC, 0x75B7BCAD, 0xBADBDE56, 0x5F0D61F0,
0x2DE63E23, 0x96F31F11, 0xCB798F88, 0x67DC491F, 0xB3EE248F,
0xD9F71247, 0xECFB8923, 0xF67DC491, 0xFB3EE248, 0x00000001,
0x00000002, 0x00000004, 0x00000008, 0x00000010, 0x00000020,
0x00000040, 0x00000080, 0x00000100, 0x00000200, 0x00000400,
0x00000800, 0x00001000, 0x00002000, 0x00004000, 0x00008000,
0x00010000, 0x00020000, 0x00040000, 0x00080000, 0x00100000,
0x00200000, 0x00400000, 0x00800000, 0x01000000, 0x02000000,
0x04000000, 0x08000000, 0x10000000, 0x20000000, 0x40000000,
0x80000000
```

PDU2 leading 96 rows (prepend to PDU1 to form the 192-row PDU2 table):

```
0x9D231959, 0xCE918CAC, 0x6528488D, 0xB2942446, 0x5B2A9CF8,
0x2FF5C0A7, 0x97FAE053, 0xCBFD7029, 0xE5FEB814, 0x709FD2D1,
0xB84FE968, 0x5E477A6F, 0xAF23BD37, 0xD791DE9B, 0xEBC8EF4D,
0xF5E477A6, 0x7892B508, 0x3E29D45F, 0x9F14EA2F, 0xCF8A7517,
0xE7C53A8B, 0xF3E29D45, 0xF9F14EA2, 0x7E98298A, 0x3D2C9A1E,
0x1CF6C3D4, 0x0C1BEF31, 0x860DF798, 0x41667517, 0xA0B33A8B,
0xD0599D45, 0xE82CCEA2, 0x7676E98A, 0x395BFA1E, 0x1ECD73D4,
0x0D063731, 0x86831B98, 0x41210317, 0xA090818B, 0xD04840C5,
0xE8242062, 0x76729EEA, 0x3959C1AE, 0x1ECC6E0C, 0x0D06B9DD,
0x86835CEE, 0x412120AC, 0x22F01E8D, 0x91780F46, 0x4ADC8978,
0x270ECA67, 0x93876533, 0xC9C3B299, 0xE4E1D94C, 0x7010627D,
0xB808313E, 0x5E649644, 0x2D52C5F9, 0x96A962FC, 0x49343FA5,
0xA49A1FD2, 0x502D8132, 0x2A764E42, 0x175BA9FA, 0x09CD5A26,
0x068623C8, 0x01239F3F, 0x8091CF9F, 0xC048E7CF, 0xE02473E7,
0xF01239F3, 0xF8091CF9, 0xFC048E7C, 0x7C62C9E5, 0xBE3164F2,
0x5D783CA2, 0x2CDC908A, 0x140EC69E, 0x0867ED94, 0x06537811,
0x8329BC08, 0x43F450DF, 0xA1FA286F, 0xD0FD1437, 0xE87E8A1B,
0xF43F450D, 0xFA1FA286, 0x7F6F5F98, 0x3DD72117, 0x9EEB908B,
0xCF75C845, 0xE7BAE422, 0x71BDFCCA, 0x3ABE70BE, 0x1F3FB684,
0x0DFF5599
```

PDU3 leading 96 rows (prepend to PDU2 to form the 288-row PDU3 table):

```
0xAA5FA470, 0x574F5CE3, 0xABA7AE71, 0xD5D3D738, 0x68896547,
0xB444B2A3, 0xDA225951, 0xED112CA8, 0x74E8188F, 0xBA740C47,
0xDD3A0623, 0xEE9D0311, 0xF74E8188, 0x79C7CE1F, 0xBCE3E70F,
0xDE71F387, 0xEF38F9C3, 0xF79C7CE1, 0xFBCE3E70, 0x7F8791E3,
0xBFC3C8F1, 0xDFE1E478, 0x6D907CE7, 0xB6C83E73, 0xDB641F39,
0xEDB20F9C, 0x74B98915, 0xBA5CC48A, 0x5F4EEC9E, 0x2DC7F894,
0x14837291, 0x8A41B948, 0x4740527F, 0xA3A0293F, 0xD1D0149F,
0xE8E80A4F, 0xF4740527, 0xFA3A0293, 0xFD1D0149, 0xFE8E80A4,
0x7D27CE89, 0xBE93E744, 0x5D297D79, 0xAE94BEBC, 0x552AD185,
0xAA9568C2, 0x572A3ABA, 0x29F59386, 0x169A4718, 0x092DAD57,
0x8496D6AB, 0xC24B6B55, 0xE125B5AA, 0x72F2540E, 0x3B19A4DC,
0x1FEC5CB5, 0x8FF62E5A, 0x459B99F6, 0x20AD4220, 0x12362FCB,
0x891B17E5, 0xC48D8BF2, 0x60264B22, 0x3273AB4A, 0x1B595B7E,
0x0FCC2364, 0x05869F69, 0x82C34FB4, 0x43012901, 0xA1809480,
0x52A0C49B, 0xA950624D, 0xD4A83126, 0x68349648, 0x367AC5FF,
0x9B3D62FF, 0xCD9EB17F, 0xE6CF58BF, 0xF367AC5F, 0xF9B3D62F,
0xFCD9EB17, 0xFE6CF58B, 0xFF367AC5, 0xFF9B3D62, 0x7DAD106A,
0x3CB606EE, 0x1C3B8DAC, 0x0C7D480D, 0x863EA406, 0x417FDCD8,
0x22DF60B7, 0x916FB05B, 0xC8B7D82D, 0xE45BEC16, 0x704D78D0,
0x3A4632B3
```

**(d) TDULC Golay walk:** starting at bit 64 of the raw frame, apply Golay24
`checkAndCorrect` every 24 bits (12 codewords); abort the frame if any word returns ≥ 2
(uncorrectable).

### 8.5 Trellis codes (Viterbi)
Both are constraint-length-2 (memory = one input symbol) trellises: the expected 4-bit
output for a transition depends only on (previous input, current input) via
`TRANSITION_MATRIX[prev][cur]`. The received 196-bit deinterleaved block is split into
**49 four-bit symbols** (MSB-first). Decode:

- Start in state 0. Feed symbols 1..48 with standard add-compare-select: one survivor per
  current-input state; branch metric = popcount of `(expected XOR received)` (Hamming
  distance on the 4-bit symbol, table `{0,1,1,2,1,2,2,3,1,2,2,3,2,3,3,4}`).
- **Tie-break:** strict `<` — an equal-metric challenger does NOT replace the incumbent
  survivor. Preserve this for bit-exactness.
- Flush with the 49th symbol into a forced input-0 node; pick the global minimum-error path.
- Decoded output = the sequence of surviving-path node **input values** (excluding the
  starting node and the flushing node), written MSB-first.
- Output sizes: 48 payload nodes → **96 bits (1/2-rate)** or **144 bits (3/4-rate)**.
  (Matches TSBK = 96 bits incl. CRC, confirmed-PDU block = 144 bits.)
- Path error total = corrected-bit estimate for the block.

**1/2-rate transition matrix** (4×4; `[prev][cur]` → expected 4-bit symbol):

```
{ 2, 12,  1, 15}
{14,  0, 13,  3}
{ 9,  7, 10,  4}
{ 5, 11,  6,  8}
```

**3/4-rate transition matrix** (8×8):

```
{ 2, 13, 14,  1,  7,  8, 11,  4}
{14,  1,  7,  8, 11,  4,  2, 13}
{10,  5,  6,  9, 15,  0,  3, 12}
{ 6,  9, 15,  0,  3, 12, 10,  5}
{15,  0,  3, 12, 10,  5,  6,  9}
{ 3, 12, 10,  5,  6,  9, 15,  0}
{ 7,  8, 11,  4,  2, 13, 14,  1}
{11,  4,  2, 13, 14,  1,  7,  8}
```

(The constellation→bit conversion from TIA-102 is already folded into these values; do not
apply any separate constellation table.)

### 8.6 Reed-Solomon (shortened RS(63,·) over GF(2^6))
Field: GF(2^6), primitive polynomial `x^6 + x + 1` (same field as BCH). Primitive element
α = 2. Antilog table: seed `alpha_to[i] = 1<<i` for i = 0..5; `alpha_to[6] = 3` (α^6 = α+1);
extend multiplicatively; `index_of[0] = −1` sentinel. Code generator = product of
`(X + α^i)` for i = 1..(n−k) — narrow-sense, first root α^1 (encoding only; decode doesn't
need it).

| Use | Code | as shortened | parity symbols | max corrected |
|---|---|---|---|---|
| LDU1 link control, TDULC | RS(24,12,13) | RS(63,51) | 12 | 6 |
| LDU2 encryption sync | RS(24,16,9) | RS(63,55) | 8 | 4 |
| HDU header | RS(63,47,17) | RS(63,47) | 16 | 8 |

Decoder: classic Simon Rockliff (1991) — syndromes `s[i] = r(α^i)` for i = 1..n−k;
Berlekamp iterative ELP; Chien search (roots recorded as `loc = 63 − i`); Forney error
values; correct in place. Uncorrectable if ELP degree > t or root count ≠ ELP degree; the
consuming message is then marked invalid. Corrected-bit count = Σ popcount(out XOR in) per
symbol.

**Shortened-length bound (normative).** The decoder also takes the **shortened** length:
`decode(input, parity, used_len)` with `used_len` = **24** for the LDU link-control and
encryption-sync codes and **36** for the HDU's RS(63,47). Symbols `used_len..63` are known
zero on both sides of the link, so an error locator at or beyond `used_len` means the Chien
search found a valid RS(63,·) codeword that is **not** a codeword of the shortened code:
reject the word instead of "correcting" it. Running the search over all 63 positions and
accepting such corrections was the main source of wrong-but-valid decodes (bogus
EncryptionSync ALGID/KID/MI). Measured over 3000 trials at t+1 random symbol errors:
RS(24,16) accepted **82** wrong words and RS(24,12) **2** — every one with a locator ≥ 24;
with the bound, both accept **0**.

**Symbol packing into the 63-symbol decoder input — normative and easy to get wrong:**
symbols are entered in **reverse order, parity first, then data, then zero padding**:

- LDU1 RS(24,12,13): `input[0..11]` = parity hex 11…0, `input[12..23]` = data hex 11…0,
  `input[24..62]` = 0. Read back data from `input[23]` down to `input[12]` → 72-bit LC word.
- LDU2 RS(24,16,9): `input[0..7]` = parity 7…0, `input[8..23]` = data 15…0, rest 0.
  Read back 23→8 → 96-bit encryption sync word.
- HDU RS(63,47,17): `input[0..15]` = parity 15…0, `input[16..35]` = data 19…0, rest 0.
  Read back 35→16 → 120-bit header data.

Each 6-bit hex symbol is MSB-first in the message bits.

---

## 9. Interleaving (196-bit data blocks)

Applied to every TSBK block, PDU header, and PDU data block **before** trellis decoding.
Mapping semantics: for each set source bit `i`, set destination bit `PATTERN[i]`
(source index → table value = destination index). HDU and LDU do **not** use this
interleaver.

`DATA_DEINTERLEAVE` (receive direction — the one you apply to the received 196 bits):

```
  0,   1,   2,   3,  16,  17,  18,  19,  32,  33,  34,  35,  48,  49,  50,  51,
 64,  65,  66,  67,  80,  81,  82,  83,  96,  97,  98,  99, 112, 113, 114, 115,
128, 129, 130, 131, 144, 145, 146, 147, 160, 161, 162, 163, 176, 177, 178, 179,
192, 193, 194, 195,   4,   5,   6,   7,  20,  21,  22,  23,  36,  37,  38,  39,
 52,  53,  54,  55,  68,  69,  70,  71,  84,  85,  86,  87, 100, 101, 102, 103,
116, 117, 118, 119, 132, 133, 134, 135, 148, 149, 150, 151, 164, 165, 166, 167,
180, 181, 182, 183,   8,   9,  10,  11,  24,  25,  26,  27,  40,  41,  42,  43,
 56,  57,  58,  59,  72,  73,  74,  75,  88,  89,  90,  91, 104, 105, 106, 107,
120, 121, 122, 123, 136, 137, 138, 139, 152, 153, 154, 155, 168, 169, 170, 171,
184, 185, 186, 187,  12,  13,  14,  15,  28,  29,  30,  31,  44,  45,  46,  47,
 60,  61,  62,  63,  76,  77,  78,  79,  92,  93,  94,  95, 108, 109, 110, 111,
124, 125, 126, 127, 140, 141, 142, 143, 156, 157, 158, 159, 172, 173, 174, 175,
188, 189, 190, 191
```

(The transmit-direction table is this permutation's inverse; a decoder needs only the above.
144-bit voice interleave tables exist in the reference but are dead code — the LDU voice
path performs no interleaving; JMBE/the vocoder handles the IMBE-internal interleave.)

---

## 10. TSBK message layouts (trunking control)

All offsets are absolute bits within the decoded 96-bit TSBK. Header (every TSBK):
bit 0 = last-block flag; bit 1 = encryption flag; bits 2–7 = opcode (6 bits);
bits 8–15 = vendor; bits 16–79 = payload; bits 80–95 = CRC-16. Opcode meaning depends on
direction (outbound/OSP for a receive-only decoder) and vendor (standard = 0).

Standard outbound opcodes needed for voice following:

| Opcode | Message |
|---|---|
| 0 | Group voice channel grant |
| 2 | Group voice channel grant update |
| 3 | Group voice channel grant update — explicit |
| 4 | Unit-to-unit voice channel grant |
| 6 | Unit-to-unit voice channel grant update |
| 8 | Telephone interconnect voice channel grant |
| 9 | Telephone interconnect voice channel grant update |
| 51 | Identifier update — TDMA |
| 52 | Identifier update — VHF/UHF bands |
| 61 | Identifier update (standard FDMA, UHF/700/800) |

Payload layouts:

**Group voice channel grant (0):** service options 16–23; frequency band 24–27; channel
number 28–39; group address (talkgroup) 40–55; source address (radio) 56–79.

**Group voice grant update (2):** two pairs, no service options — band A 16–19,
channel A 20–31, group A 32–47; band B 48–51, channel B 52–63, group B 64–79.
Pair B is meaningful only if group B ≠ group A and ≠ 0.

**Group voice grant update explicit (3):** service options 16–23; reserved 24–31;
downlink band 32–35, downlink channel 36–47; uplink band 48–51, uplink channel 52–63;
group 64–79.

**Unit-to-unit grant (4) and update (6)** (identical layout): band 16–19, channel 20–31,
target address 32–55, source address 56–79.

**Telephone interconnect grant (8):** service options 16–23; band 24–27; channel 28–39;
call timer 40–55; source address 56–79. (No talkgroup.)

Service options byte: standard P25 decomposition (emergency, encrypted, duplex, mode,
priority in low 3 bits).

### Identifier (IDEN / frequency band) updates

**Opcode 61 — standard FDMA:** identifier 16–19 (4 bits); bandwidth 20–28 (9 bits, ×125 Hz);
transmit-offset sign bit 29 (set = positive, clear = negative); transmit offset 30–37
(8 bits, ×250000 Hz); channel spacing 38–47 (10 bits, ×125 Hz); base frequency 48–79
(32 bits, ×5 Hz). "No offset" sentinel: raw offset field == 0x80.

**Opcode 52 — VHF/UHF:** identifier 16–19; bandwidth code 20–23 (0x4 → 6250 Hz,
0x5 → 12500 Hz, else 0); offset sign bit 24; transmit offset 25–37 (13 bits,
**× channel spacing** Hz); spacing 38–47 (×125); base 48–79 (×5).

**Opcode 51 — TDMA:** identifier 16–19; channel type 20–23; offset sign 24; offset 25–37
(13 bits, × channel spacing); spacing 38–47 (×125); base 48–79 (×5). Channel-type table:

| Value | Access | BW Hz | slots/carrier |
|---|---|---|---|
| 0 | FDMA | 12500 | 1 |
| 1 | FDMA | 12500 | 1 |
| 2 | FDMA | 6250 | 1 |
| 3 | TDMA | 12500 | 2 |
| 4 | TDMA | 25000 | 4 |
| 5 | TDMA | 12500 | 2 (H-D8PSK simulcast) |

**"No transmit offset" sentinel — all three IDEN opcodes.** A raw transmit-offset field of
**0x80** means "this band has no transmit offset" (receive-only, uplink == downlink) for
opcodes **51 and 52** exactly as for 61 (sdrtrunk `FrequencyBandUpdate*.isTransmitOffsetValid`).
Honoring it only on opcode 61 produces a nonsense uplink offset of ±128 × spacing on VHF/UHF
and TDMA bands. One helper — `signed_offset(offset_raw, sign_positive, unit_hz)` — implements
it for all three, with `unit_hz` = 250 kHz for opcode 61 and the channel spacing for 51/52.

**Standard vendor.** MFID **0** (STANDARD) *and* MFID **1** (STANDARD_V1) both parse with the
standard opcode tables. One predicate, `payload::is_standard_vendor(vendor)`, is used by the
TSBK parser and the Link Control parser (§18.1) alike, so no MFID can be "standard" in one
place and vendor-specific in the other.

### Channel → frequency

A channel reference = (4-bit band identifier, 12-bit channel number) — the grant layouts
above already split them; equivalently a 16-bit channel word has band in the top 4 bits.

```
FDMA:  downlink Hz = base(raw×5) + spacing(raw×125) × channelNumber
TDMA:  downlink Hz = base(raw×5) + spacing(raw×125) × floor(channelNumber / slotsPerCarrier)
uplink Hz = downlink + signedTransmitOffset        (0/absent if no offset)
```

Explicit (opcode 3) channels: uplink frequency = the **downlink** formula evaluated with the
uplink band + uplink channel number (offset not used). Uplink channel number 0xFFF (4095) =
"no uplink". TDMA timeslot: `channelNumber % slotsPerCarrier == 1` → timeslot 2, else 1
(Phase 1 is always timeslot 1 conceptually).

A grant naming a band identifier with no stored IDEN yields frequency 0 — the call cannot be
followed until that band's identifier-update has been received (see §13 preload note).

---

## 11. HDU / TDU / TDULC

- **HDU (658-bit buffer, 648 raw):** 36 Golay(18,6,8)-protected hex codewords at bit starts
  0, 18, 36, … (18-bit stride): 20 data hexes (starts 0..342) then 16 RS parity hexes
  (starts 360..630). Decode: Golay18 each word → RS(63,47,17) (§8.6 packing) → 120-bit
  header data (contains MFID, algorithm ID, key ID, TGID — not required for trunking
  parity, but decode validity/counts are).
- **TDU:** empty payload; terminates a call.
- **TDULC (308-bit buffer, 288 raw):** twelve Golay24 words (§8.4d) → 24 hex symbols → RS
  (24,12,13) → 72-bit link control word. Also terminates a call (with LC info).

---

## 12. LDU1 / LDU2 (voice)

Each LDU is 1568 bits carrying **9 IMBE voice frames of 144 bits** at these bit offsets
(non-contiguous — signaling sits in the gaps):

```
frame1 0     frame2 144   frame3 328   frame4 512   frame5 696
frame6 880   frame7 1064  frame8 1248  frame9 1424
```

### 12.1 Vocoder boundary (normative)
Slice each 144-bit frame **raw and untouched** (18 bytes) and hand it to the IMBE vocoder.
The frame's internal deinterleave, Golay(23,12)/Hamming(15,11) protection, and PN
derandomization are the vocoder's job (per TIA-102.BABA), not this engine's.

### 12.2 Low-speed data (LDU): 32 bits at positions 1392–1399, 1408–1415 (two 16-bit
halves; the other 16 bits of 1392–1423 are their parity).

### 12.3 Embedded signaling — 24 Hamming(10,6,3) words
Both LDUs use the same 24 word start offsets (10-bit words: 6 data + 4 parity):

```
288, 298, 308, 318, 472, 482, 492, 502, 656, 666, 676, 686,
840, 850, 860, 870, 1024, 1034, 1044, 1054, 1208, 1218, 1228, 1238
```

Hamming10-correct each word, take its 6-bit data hex, then:
- **LDU1:** first 12 hexes (offsets 288..686) = data, last 12 (840..1238) = RS parity →
  RS(24,12,13) → 72-bit **link control word** (carries talkgroup/source for the active
  call — used by the traffic-channel tracker).
- **LDU2:** first 16 hexes (288..870) = data, last 8 (1024..1238) = parity → RS(24,16,9) →
  96-bit **encryption sync** (algorithm ID, key ID, message indicator).

---

## 13. Trunking / traffic-channel tracking

Call tracking is keyed by **downlink frequency** (Phase 1: always timeslot 1) — not by
talkgroup.

State: `allocatedTrafficChannels: freq → channel` (the control frequency is pre-inserted so
it is never allocated as traffic); `eventTrackers: freq → tracker`;
`frequencyBands: idenId → band`. Traffic channel pool: default size **20**.

**On a grant (or grant update):**
1. Resolve `freq = downlinkFrequency(band, channel)`. If 0 (unknown band) the grant is not
   yet usable — guards on `freq > 0`. **Clarification (from the parity build):** do not
   permanently drop it. The reference surfaces such a grant once the band plan is learned, and
   in the real capture every grant precedes its band's IDEN. Buffer the grant (bounded,
   drop-oldest, with a short TTL) and replay it when the matching IDEN arrives, so the call is
   followed as soon as the frequency becomes resolvable. A strict "drop on first sight"
   reading misses the capture's only grant; buffer-and-retry matches the reference.
2. Fetch tracker for freq, discarding it if stale (see below).
3. If a tracker exists and it is the same call (compare TO/talkgroup only): if the talker
   (FROM) differs → emit a call-continuation event and replace the tracker; else merge FROM
   into the event and refresh duration (control-side refresh only extends the event if the
   call hasn't been started by a traffic decoder — the **hand-off freeze**). Re-check
   allocation — a tracked call may not have a running channel yet.
4. Else (new call): create event + tracker. If freq is not allocated: take a channel from
   the pool; empty pool → mark the event "MAX TRAFFIC CHANNELS EXCEEDED" and do not start;
   else request traffic-channel start.
5. A grant **update** for an untracked frequency is treated as a fresh grant (catches
   in-progress calls missed at grant time).

**Traffic channel start:** insert freq into the allocated map **before** issuing the start
request (duplicate-grant guard). The new traffic decoder must be seeded with **all known
frequency bands** (and patch groups) at startup — it starts with an empty band map and
cannot resolve channel numbers otherwise (**IDEN preload — omitting this breaks all traffic
decoding**).

**Tracker staleness:** an event is stale when `now − eventEnd > 3000 ms`; staleness is
evaluated lazily on next access (no timer thread).

**Traffic-side updates:** call start (HDU seen), current-user refresh (LDU1 LC: merge
encryption/GPS/alias, set started + extend end; same-call check uses TO **and** FROM),
call end (TDU/TDULC: mark complete only if started; tracker is *not* removed — it ages out
or is replaced).

**Teardown:** on a traffic channel's processing-stop: remove freq from allocated map, remove
tracker, return channel to pool. On start-rejected: return the channel but keep the tracker
marked "CHANNEL START REJECTED". On control-channel retune: stop all traffic channels, clear
all maps and bands, re-seed the control frequency.

### 13.1 One tracker per site (shared), and channel roles

A site is **one** `TrunkingTracker`, driven by the control decoder *and* by every traffic
decoder it spawns — the reference's `P25TrafficChannelManager` model. A private tracker per
channel cannot work: tracker entries are created only by grants, which arrive on the control
channel, so a traffic decoder holding its own tracker (seeded with its *own* frequency as the
control frequency) can never have an entry for the frequency it is following. Every
traffic-side hook — the hand-off freeze, the same-call refresh, talker-change continuations
and `CallEnd` — is then dead code.

- **Handle:** `SharedTracker = Rc<RefCell<TrunkingTracker>>`, created by
  `TrunkingTracker::new_shared(control_frequency_hz)`. All channels of one device run on a
  single DSP thread, so a single-threaded handle is the right cost.
- **Roles:** `ChannelRole::{Control, Traffic}`. `P25P1Channel::new_control(..., tracker)` and
  `new_traffic(..., tracker)` (mirrored on `P25P1WidebandChannel`) join a shared tracker. The
  plain `new(...)` keeps its old signature and meaning: a **standalone** channel with a
  private tracker, in the `Control` role.
- **Band plan is site state:** `TrunkingTracker::bands` is private, reached through
  `bands()` / `bands_mut()`.

### 13.2 Traffic-role wiring (the only producer of `CallEnd`)

These hooks run **only** in the `Traffic` role. On a control channel they are meaningless (no
tracker is keyed to the control frequency, and control carries no voice), so a control decode
is exactly what it was before the shared tracker.

| Framer event | Hook |
|---|---|
| first valid `Hdu` — or the first `LinkControl` / `ImbeFrame` when the HDU was missed or invalid | `traffic_call_started(freq, now)`: sets `started_by_traffic`, freezing control-side refreshes (the hand-off) |
| LDU1 LC `GroupVoiceChannelUser` or `UnitToUnitVoiceChannelUser` (TO/FROM, §18.1) | `traffic_call_update_with(freq, to, from, now, sink)` |
| `Tdu` / `TduLc` | `traffic_call_ended(freq, sink)` → **`CallEnd`** |

`traffic_call_update_with` mirrors the reference `processP1TrafficCurrentUser` /
`isSameCallCheckingToAndFrom`:

- no tracker for this frequency → no-op (nothing on this channel to update);
- **stale** tracker (older than `STALE_MS`) → not the same call: install a new tracker and
  emit **`CallStart`**;
- same TO with **FROM = 0** (which `GROUP_VOICE_CHANNEL_USER` sometimes carries) → same-call
  refresh: extend `event_end_ms`, keep the known talker;
- same TO with a **different non-zero FROM** → genuine talker change: replace the tracker and
  emit **`CallContinue`**;
- a **TO that disagrees with a live (non-stale) tracker → ignored**. *Deliberate deviation
  from the reference:* on a traffic channel the call identity comes from the control channel,
  and a single misdecoded LC must not hijack a live call.

`traffic_call_ended` emits `CallEnd` only if the call was `started_by_traffic` and not already
complete; the tracker is **not** removed (it ages out or is replaced). Before this wiring
`P25Event::CallEnd` was never emitted at all.

**Retune.** `P25P1Channel::frequency_changed` calls `TrunkingTracker::retune` — stop
everything, clear allocations, trackers, pending grants **and the IDEN band plan**, re-seed
the control frequency — **only in the `Control` role**: that is a system change (a new NAC /
a different control channel). A **traffic** retune, which merely follows the next grant, must
never clear the shared bands; doing so leaves the whole site unable to resolve any later
grant. Both roles reset the framer (§7.10).

**Known limitation (matches the reference).** A traffic channel with no preceding grant has
no tracker entry for its frequency, so the update hook is a no-op: it emits raw frame events
but no call events. Call events on a traffic channel presuppose the control channel that
granted it.

### 13.3 Pending grants, pool accounting, and event fields

- **Pending-grant TTL** = `PENDING_GRANT_TTL_MS` = `STALE_MS` (**3 s**), and must never
  exceed it. A replayed grant (§17.11) is fed back through the normal grant path, so a longer
  TTL lets a single IDEN replay a backlog of long-dead grants as fresh `CallStart`s and drain
  the channel pool (it was 10 s = 3× `STALE_MS`).
- **`PoolExhausted` once per call.** The flag lives on the tracker
  (`pool_exhausted_reported`), so a control-channel refresh of the same exhausted call does
  not re-emit it.
- **`traffic_start_rejected(freq)` guards the control frequency**, exactly as
  `traffic_stopped` does. The control frequency is pre-allocated and never came from the
  pool: freeing it would both un-protect it (a later grant could hand it out as a traffic
  channel) and inflate the pool past `DEFAULT_POOL_SIZE` (20 → 21).
- **Control-side continuation replaces the tracker** (with `started_by_traffic = false`), as
  the reference does, so later control refreshes keep extending the *new* call.
- **24-bit target.** `Grant`, `CallStart` and `CallContinue` carry **`target: u32`** — the
  full TO address: the target radio for unit-to-unit, the telephone address for an
  interconnect update, and (for convenience) the talkgroup for a group call; 0 when the
  message carries no TO. `talkgroup: u16` then means what it documents: the group id for a
  group call, **0** for unit-to-unit and telephone calls. Truncating a 24-bit TO into
  `talkgroup` corrupted both fields.

---

## 14. Frequency-error feedback (auto-PPM)

When a frame sync is validated, convert the demod PLL value to Hz:
`correctionHz = decimatedRate / (2π) × pllError`, and feed it to the tuner's frequency
correction. C4FM sources this from the equalizer `pll` on each sync; LSM from its PLL when
frame sync is valid. Reset the PLL on frequency-change events.

---

## 15. Essential vs. optimization (implementation guidance)

**Bit-exactness-critical (must match):** 4800 sym/s; decimate→LPF→RRC chain including the
RRC /2 quirk; differential demod; slicing boundaries 0/±π/2; sync pattern + 11/13 symbol
split; correlator thresholds (80 detect / 110 equalized / 60 framer gate); NID = 33 dibits
skipping index 11; every table and polynomial in §8–§10; trellis strict-`<` tie-break;
RS reversed/parity-first packing; status-dibit cadence (every 36th, restart at 21 after
NID); DUID buffer lengths; LSM Gardner sign convention and gains (0.1 PLL / clamp ±π/3 /
±0.3 phase-error clamp / AGC 0.05); C4FM 1.219 initial gain.

**Reference-specific optimizations you may replace with simpler equivalents** (at some
low-SNR robustness cost, without breaking message-level parity on clean signals): SIMD
variants; the C4FM dual sync correlator + hill-climb optimizer (a single correlator with
fixed interpolation works); the NAC tracker's exact eviction policy; the `isNoisy` gate;
delayed-dispatch micro-ordering. The BCH root-finder may be any mathematically equivalent
algorithm (e.g. Chien search); the Viterbi may use metric+backpointer arrays instead of
path copies **provided the tie-break is preserved**.

**Known reference quirks that are behavior, not bugs to fix:** the RRC /2 taps; Golay24's
correction loop assuming startIndex 0; the DATA voice-interleave tables existing but unused.
A grant for an unknown band is unusable *at first sight* but must be buffered and resolved
when its IDEN arrives (§13) — not permanently dropped. Replicate behavior; do not "fix" the
first three during the parity phase.

---

## 16. Acceptance criteria (parity harness)

Reference fixture: a local capture under `fixtures/` (not shipped; see `fixtures/README.md`) —
10 s of a live P25 Phase 1 LSM control channel; int8 interleaved IQ at 2.4 Msps, the P25 channel
about +400 kHz off the LO (clear of the DC spike), NAC and message mix per the reference decode.

Metrics vs the reference implementation on identical input:
1. Frame syncs detected (count within ±2%).
2. NID decodes with valid BCH (count and NAC value — NAC must match exactly).
3. TSBK count and per-opcode histogram (exact-match target on clean signal).
4. Grant events: (frequency, talkgroup, source) tuples — set equality.
5. IDEN messages: decoded (base, spacing, offset, bandwidth) — exact.
6. Corrected-bit totals within ±10% (differences reflect EDAC-path variance).
7. Throughput: ≥ 1× real time per channel on a Raspberry Pi 4 core; no per-symbol heap
   allocation in steady state.

---

## 17. Implementation clarifications (from two independent parity builds)

The spec was reimplemented twice, clean-room, from this document alone. Both engines reached
parity with the reference; the ambiguities they had to resolve are recorded here so a future
implementer doesn't rediscover them. Each references the section it clarifies. The
`CONFIRMED` items were verified against the live capture; the rest are consistent readings.

**17.1 CRC field orientation (§8.4) — CONFIRMED, spec listing was misleading.** The
transmitted CRC field is read **MSB-first**, like every other field (§1): for CCITT-16, block
bit 80 (field MSB) ↔ table row `0x8000`, so the 16 identity rows are indexed in *reverse* of
how they're listed. With that reading every clean on-air block gives residual 0 or 0xFFFF.
Equivalently, the transmitted 16-bit value = CRC-CCITT(poly 0x1021, init 0xFFFF) XOR the
constant **0x1EC6** — which is why "residual 0 **or** all-ones" is the acceptance test. The
data rows are exactly `~(x^(95−i) mod g)`. Apply the same reversed reading to the CRC-32 PDU
tables' trailing 32 identity rows. (Verified on 8 distinct repeated error-free blocks.)

**17.2 Half-band tap parity (§3.1) — CONFIRMED.** Nonzero taps are at **odd** offsets from
center; `sin(offset·π/2)` is zero at even offsets. (Already corrected inline in §3.1.)

**17.3 BCH syndrome indexing (§8.1).** The formula mixes 0-based array index and 1-based
syndrome number. Read it as: syndromes s₁..s₂₂ with `s_j = Σ_i α^(j·(62−i))` over set bits i;
even-indexed syndromes are the GF-square of the half-index odd one. The root-finder is
explicitly non-normative — Berlekamp–Massey + Chien search gives identical corrections and is
simpler than the reference's trace-map factorizer. (Verified: 253/253 NAC-consistent NIDs.)

**17.4 Sync-detect gating vs. active assembler (§7.3).** "Sync detect only honored when no
assembler is active" is contradicted one paragraph later by `nidDetected` handling a still-
active assembler. On a control channel a PLACE_HOLDER assembler is active almost always, so a
literal reading misses most frames. **Resolution:** the soft sync detector always runs and
always (re)starts NID capture; the active-assembler dispatch/force-complete logic happens at
NID *validation*, exactly as the rest of §7.3 describes.

**17.5 Dibit-counter dispatch accounting (§7.3/§7.4).** The sync-loss rule (`dibitCounter −= 57`
at NID; positive remainder = loss) only balances if the counter is *also* reduced on frame
completion. Interpret the DUID table's **elapsed-dibits** column as exactly that: on terminal
completion, `dibitCounter −= elapsed(duid)`. With the one-dibit delayed dispatch, contiguous
frames then account to zero sync loss. (Verified on the capture.)

**17.6 Status-dibit counter (§7.2).** The startup value 36, the "reaches 36 → divert", and the
post-NID restart at 21 line up only as **increment-then-compare with a `>=` divert**: startup
36 → first dibit is 37 → diverted (harmless pre-sync); post-NID 21 puts the next divert at
frame dibit 71, matching the transmitter cadence. A one-dibit cadence error scrambles every
block, so this is load-bearing. (Verified.)

**17.7 Golay24 return code (§8.2).** A syndrome-0 word with a wrong overall-parity bit counts
as **clean (0)** with the parity bit silently repaired (it's not data). Rotate-and-search
failure → 2 (uncorrectable). Overall parity is re-established after any core correction.

**17.8 Unknown-DUID NID accounting (§7.3).** A NID whose DUID is unknown (coerced to
PLACE_HOLDER) still counts toward the NID/NAC totals if BCH passed — record its NAC in the
histogram and the NAC tracker — but do **not** overwrite the parsing-context `current_nac`.

**17.9 NAC-tracker reset scope (§8.1).** "Reset on sync loss" means reset only on the
free-running 1-second sync-loss report (true signal loss), not on the small per-frame
accounting reports that fire routinely between adjacent frames.

**17.10 PDU multi-block CRC-32 geometry (§8.4c).** CRC positions 224/320/416 with coverage
from bit 160 imply a **160-bit header region** followed by 96-bit unconfirmed blocks; the
region [96,160) is uncovered — zero-pad it after the 96 decoded header bits. CRC-32 is used
only for unconfirmed PDUs of 1–3 blocks; confirmed PDUs use per-block CRC-9 instead.
(Untested — the capture contains no PDUs.)

**17.11 Grant buffering (§13) — CONFIRMED.** A grant for a not-yet-known band is buffered
(bounded, drop-oldest, short TTL) and replayed when its IDEN arrives, not dropped. (Already
in §13; verified — the capture's only grant resolves this way to its band-1 frequency.)

**17.12 §15-sanctioned substitutions actually taken.** Both builds used a windowed-sinc
(Kaiser/Hamming) baseband LPF at the specified band edges instead of a true Parks–McClellan
design (§3.2), and a single C4FM sync correlator with linear interpolation instead of the
dual-correlator + 128-phase MMSE + hill-climb (§5). Both are permitted by §15 and do not
affect message-level parity on clean signals; the LSM path (§6, primary) is implemented
exactly. The 128-phase MMSE interpolator table is **not** reproduced in this spec — if exact
C4FM bit-parity is ever required, source that table from GNU Radio's `gr_mmse_fir`.

**17.13 Still unspecified — field layouts (see §11/§12).** The bit layouts inside the LDU1
Link Control word (talkgroup/source), the LDU2 Encryption Sync word (ALGID/KID/MI), and the
HDU header data (MFID/ALGID/KID/TGID), plus the LDU low-speed-data parity code and the TSBK
opcode-9 payload, are addressed in §18.

---

## 18. Message payload field layouts

All fields MSB-first (§1); bit indices are absolute within the error-corrected container and
inclusive. These layouts let the decoder surface talkgroup/source on voice channels, key/algo
on encrypted calls, and named vendor messages — beyond the raw-word passthrough of §11/§12.

### 18.1 LDU1 Link Control Word (72-bit)

The 72-bit word (from RS(24,12,13), §12.3) begins with a header that selects a subtype:

| Field | bits | width | meaning |
|---|---|---|---|
| ENCRYPTION_FLAG | 0 | 1 | LCW encrypted flag |
| STANDARD_VENDOR_FLAG (P) | 1 | 1 | 1 = standard format (vendor = STANDARD, ignore bits 8–15); 0 = read MFID from bits 8–15 |
| LCO (opcode) | 2–7 | 6 | Link Control Opcode |
| VENDOR (MFID) | 8–15 | 8 | manufacturer id (used when P = 0) |

> **Dispatch by VENDOR, not by the P flag.** The subtype is selected by the
> resolved **vendor** (`STANDARD` when P = 1, else bits 8–15), exactly as
> sdrtrunk's `LinkControlWord.getVendor` + `LinkControlWordFactory` do. In
> practice standard LCs are transmitted with **P = 0 and MFID = 0x00**
> (STANDARD), so gating the standard-LCO parse on the P flag alone silently
> drops them (loses the source radio). MFID 0x00 (STANDARD) and 0x01
> (STANDARD_V1) are both "standard"; MFID 164 = L3Harris.

**Standard LCO values:** `GROUP_VOICE_CHANNEL_USER = 0`, `UNIT_TO_UNIT_VOICE_CHANNEL_USER = 3`,
`TELEPHONE_INTERCONNECT_VOICE_CHANNEL_USER = 6`. All voice subtypes carry
**SERVICE_OPTIONS at bits 16–23** (8).

- **LCO 0 — Group Voice Channel User** (the common one — gives TG + radio on a group call):
  GROUP_ADDRESS (talkgroup) **32–47** (16); SOURCE_ADDRESS_EXTENSION_FLAG bit **31** (set when a
  fully-qualified source-ID extension LC follows); SOURCE_ADDRESS (radio) **48–71** (24).
  Bits 24–30 unused.
- **LCO 3 — Unit-to-Unit Voice Channel User:** TARGET_ADDRESS **24–47** (24); SOURCE_ADDRESS
  **48–71** (24).
- **LCO 6 — Telephone Interconnect Voice Channel User:** CALL_TIMER **32–47** (16, ×100 ms);
  ADDRESS **48–71** (24, landline-side talkgroup). Bits 24–31 unused.

**L3Harris LC opcodes** (P = 0, MFID = 164): 10 = return-to-control-channel, 42/43 =
talker-GPS block 1/2, 50–53 = talker-alias block 1–4. (Vendor-specific block payloads live in
sdrtrunk's `lc/l3harris/`; not required for standard decode — name them, emit raw otherwise.)

**Talker alias wire format (observed).** The four alias opcodes are **not** four successive
pieces of a 28-octet string. The alias is a **14-octet NUL-padded ASCII field carried by a
pair of blocks**, and the second pair repeats the first verbatim. Each block's 7 payload
octets follow the LCO byte and the MFID byte (`word[2..9]`):

| LCO | word | payload | field octets |
|---|---|---|---|
| 50 | `32 a4 554e49545f415f` | `"UNIT_A_"` | 0–6 |
| 51 | `33 a4 42415345310000` | `"BASE1\0\0"` | 7–13 |
| 52 | `34 a4 554e49545f415f` | `"UNIT_A_"` | 0–6 (repeat) |
| 53 | `35 a4 42415345310000` | `"BASE1\0\0"` | 7–13 (repeat) |

The cycle 1,2,3,4 runs once per LDU superframe (≈360 ms) and repeats for the length of the
transmission, so a short transmission carries only a prefix of the cycle. There is no block
index or length field beyond the opcode; the field ends at the first NUL. Aliases run 3–14
characters; a block-1 payload with no NUL means the field continues into block 2.

Two rules follow, and both are required — without them the decoder publishes spliced and
partial aliases (`BASE1UNIT_A_` from blocks 2,3; `UNIT_A_BASE1BASE1` from 1,2,4;
`UNIT_A_UNIT_A_BASE1` from 1,3,4; bare `UNIT_A_`, `BASE1`, `U`):

- **Fold 50/52 onto half 0 and 51/53 onto half 1** — never concatenate whichever blocks
  arrived. A half re-sent with the *same* payload is the protocol's own repetition and
  changes nothing.
- **Publish only a complete field.** Half 0 alone is the whole alias only when it carries the
  terminating NUL; a full 7 octets means half 1 must arrive first. Half 1 on its own is a
  *tail*, never an alias. A half re-sent with *different* content is a new alias (new talker,
  or a block left over from the previous transmission): drop what was held rather than merge.

**Call-assembly boundaries.** Two rules keep one call's metadata out of the next call's
record:

- **Alias blocks reset on a talker change.** The L3Harris talker-alias halves are cleared
  when the source address changes mid-call, not only at call end; otherwise a leftover half
  from the previous talker is read as the new talker's tail (e.g. `"CAR5"` + `"2-LONG "`).
  The new talker re-sends its own blocks on the following superframes.
- **A talker-alias block never opens a call.** The alias cycle runs on past the terminator,
  and a grant can subscribe the receiver mid-teardown, so an alias block arriving with no
  call open belongs to the talker that just stopped. Opening a call for it files one talker's
  alias under the next talker's call.
- **The site engine files a block only under a confirmed talker** (`SiteEngine`, T-D4). The
  infrastructure keeps cycling the previous talker's blocks into the next transmission —
  before the new talker's first LCO 0 and, for up to a cycle, after it (observed: a field
  unit's `FD_301` / `A33` filed under the console that answered it). So: a block arriving
  before a traffic-side word (LCO 0/3, or a terminator naming the same radio) has named the
  talker is *held* and released through the checks below once one does, or dropped if the
  control channel names another talker first; a block equal to a half of the previous
  talker's alias on this channel within 2 s of the change is dropped (an all-NUL half proves
  nothing and is exempt); an all-NUL second half is taken only to finish a head this talker
  already sent (a leftover pad read `FD_ENGINE_8` as `FD_ENGI`); and a block another radio
  was heard with complete in the last 30 s is dropped when this radio is known to send
  something else there. The engine's alias replaces the call assembler's in the call record.
- **Per-radio alias memory.** A radio's last complete alias (per site, ≤ 4096 radios, least
  recently heard evicted, 24 h lifetime) names a call that carried only the first half of a
  longer field when that half matches the remembered field's first 7 octets exactly
  (`PD_CON2`, `EX-POS2`, `FD_ENGINE_8` on short calls). A radio with no complete sighting
  is never named from a head. Replayed over three days of one site's event log
  (2026-09-19..21), radios filed under more than one alias went 9/10/6 → 1/5/2 — the rest
  are consoles or radios that send two variants themselves (`EX_MED_0` / `EX_MED_07`, inside
  their own confirmed transmissions) — and calls filed with an alias rose ~37 %.
- **An invalid HDU never splits a call.** An `Hdu` event with `valid = false` is a
  false-positive NID whose 120 bits survived neither Golay18 nor RS(63,47): it carries no
  header and is not evidence of a call boundary. Treating it as one fragments a live call in
  two mid-stream. Only a **valid** HDU starts (and, if one was open, closes) a call.

### 18.2 LDU2 Encryption Sync Parameters (96-bit)

Standalone word from RS(24,16,9) (§12.3) — **no LCO header**:

| Field | bits | width |
|---|---|---|
| MESSAGE_INDICATOR (MI) | 0–71 | 72 |
| ALGORITHM_ID (ALGID) | 72–79 | 8 |
| KEY_ID (KID) | 80–95 | 16 |

**Clear-call sentinel: `ALGID == 0x80` (UNENCRYPTED).** This is the
authoritative signal, matching sdrtrunk's `Encryption` enum. Note `ALGID == 0x00`
is the **ACCORDION_3 cipher**, not clear — so an `algid == 0` test is wrong.
An all-zero word (no ES transmitted) is also treated as clear. Common ALGIDs:
0x80 = UNENCRYPTED, 0x81 = DES-OFB, 0x83 = 3-key Triple-DES, 0x84 = AES-256,
0x85 = AES-128, 0xAA = ADP/RC4. Same rule applies to the HDU header ALGID (§18.3).

**One rule, three consumers.** The test is stated once, as
`payload::is_clear_params(algid, kid, mi)`: `algid == 0x80` ⇒ clear whatever the KID/MI say;
an all-zero encryption word (`algid == 0`, `kid == 0`, `mi` all zero) ⇒ clear, since that is
"no encryption parameters were sent", not a cipher; anything else ⇒ encrypted — in particular
`algid == 0x00` with a non-zero KID or MI, which is ACCORDION 3. `EncryptionSync::is_clear`,
`HduHeader::is_clear` and the call assembler (`CallRecord::encrypted`) all defer to it, so a
call can never be reported clear by one and encrypted by another.

### 18.3 HDU Header Data (120-bit)

Standalone word from RS(63,47,17) (§11):

| Field | bits | width |
|---|---|---|
| MESSAGE_INDICATOR_A | 0–35 | 36 |
| MESSAGE_INDICATOR_B | 36–71 | 36 |
| VENDOR (MFID) | 72–79 | 8 |
| ALGORITHM_ID | 80–87 | 8 |
| KEY_ID | 88–103 | 16 |
| TALKGROUP_ID | 104–119 | 16 |

MI = A‖B (72 bits, same as LDU2). All 120 bits are assigned — no reserved bits in the
reference (published TIA diagrams sometimes show a reserved nibble; the reference allocates
none).

### 18.4 TSBK opcode 9 (Telephone Interconnect grant UPDATE)

Bit layout is **identical** to opcode 8 (§10): SERVICE_OPTIONS 16–23, FREQUENCY_BAND 24–27,
CHANNEL_NUMBER 28–39, CALL_TIMER 40–55 (×100 ms), address 56–79. **Semantic difference:** the
24-bit field at 56–79 is a **radio unit ID (source)** for opcode 8, but a **talkgroup** for
opcode 9. (Corrects the §10 assumption that they are fully identical — the wire bits are, the
address *type* is not.)

### 18.5 L3Harris vendor TSBK opcodes (MFID = 164)

Of the vendor TSBK opcode space, the reference names exactly one by value:
**opcode 48 (0x30) = Group Regroup Command with Explicit Encryption** (patch/supergroup).
Its payload: REGROUP_OPTIONS 16–23; SUPERGROUP_ADDRESS 24–39 (16); ENCRYPTION_KEY_ID 40–55
(16); TARGET_RADIO_ADDRESS 56–79 (24) **or** TARGET_TALKGROUP_ADDRESS 64–79 (16) — selected by
the group flag inside REGROUP_OPTIONS. The REGROUP_OPTIONS byte (bits 16–23) decomposes as:
`0x80` = regroup type (set = simulselect one-way, clear = two-way patch); **`0x40` = target is
talkgroup (set) vs individual radio (clear) — absolute bit 17, this is `isTalkgroupAddress`**;
`0x20` = activate (set) vs deactivate; `0x1F` = supergroup sequence number. Every other Harris
OSP opcode is a
generic "Harris OSP unknown"; no Harris ISP opcode is decoded by value. A decoder should
recognize MFID 164, name opcode 48, and label the rest "Harris OSP (opcode N)" rather than
mis-binning them as standard opcodes.

### 18.6 LDU low-speed data error-control code (resolves §17.13)

Each LDU's 32-bit low-speed-data region (§12.2) is **two independent octets**, each a
codeword of a **(16, 8, 5) shortened cyclic code**: 8 data bits then 8 parity bits.
- Octet 0: data bits 1392–1399, parity 1400–1407.
- Octet 1: data bits 1408–1415, parity 1416–1423.

Generator polynomial **g(x) = x⁸ + x⁵ + x⁴ + x³ + 1** (i.e. `x⁸ mod g = 0x39`). Systematic
encoding: parity = (data · x⁸) mod g(x). Minimum distance 5 → corrects up to 2 bit errors per
octet (bounded-distance nearest-codeword decoding). This is **not in the reference** (sdrtrunk
extracts the data bits without checking parity); the polynomial was recovered from the code's
structure and cross-checked against the MMDVMHost `CCS_PARITY` table (bit-compatible with
on-air LSD — the table generated from g(x) reproduces MMDVMHost's exactly). Emitting the
corrected data plus a per-octet error/validity indication is a superset of the reference.

**Failure semantics (normative).** Beyond the correction radius the nearest codeword is
*arbitrary* — several codewords may be equidistant, and none is likelier than the word
actually received — so an octet outside the 2-error bound is **not** corrected: report the
**raw received data byte**, `errors = 0`, and `valid = false`. Reporting a nearest-codeword
guess with `errors = 3/4` fabricates data the link never carried. Consumers must check
`valid` before using `data`; `P25Event::LowSpeedData` carries `data`, `errors` and `valid`
per octet.

### 18.7 Site / network broadcasts (TSBK opcodes 57–60)

The four standard-vendor OSP broadcasts that describe a site's **channel plan** rather than a
call. None of them affects traffic-channel tracking; they exist so a recorder can observe, from
the air, which control channel is live, which backups the site announces, and which neighbours
it has. All bit offsets are absolute within the decoded 96-bit TSBK (§10 header rules apply:
opcode 2–7, MFID 8–15, standard vendor per `payload::is_standard_vendor`).

**Opcode 57 (0x39) — Secondary Control Channel Broadcast.** RFSS 16–23; SITE 24–31;
FREQUENCY_BAND_A 32–35, CHANNEL_NUMBER_A 36–47, SYSTEM_SERVICE_CLASS_A 48–55;
FREQUENCY_BAND_B 56–59, CHANNEL_NUMBER_B 60–71, SYSTEM_SERVICE_CLASS_B 72–79. A channel B
whose band **and** channel number are both zero means "no second channel" and is reported as
absent. These are the site's backup control channels; their service class normally has only
the backup-control bit set.

**Opcode 58 (0x3A) — RFSS Status Broadcast.** LRA 16–23; **bit 27** = active network connection
to the RFSS controller (bits 24–26 reserved); SYSTEM 28–39 (12); RFSS 40–47; SITE 48–55;
FREQUENCY_BAND 56–59, CHANNEL_NUMBER 60–71; SYSTEM_SERVICE_CLASS 72–79. The channel is the
site's **live** control channel — the authoritative on-air answer to "what am I tuned to".

**Opcode 59 (0x3B) — Network Status Broadcast.** LRA 16–23; WACN 24–43 (**20 bits**);
SYSTEM 44–55 (12); FREQUENCY_BAND 56–59, CHANNEL_NUMBER 60–71; SYSTEM_SERVICE_CLASS 72–79.
Carries the same live control channel as opcode 58, plus the WACN.

**Opcode 60 (0x3C) — Adjacent Status Broadcast.** LRA 16–23; flags **24** = conventional
channel, **25** = site in failure condition, **26** = site information valid, **27** = active
network connection to the neighbour's RFSS controller; SYSTEM 28–39 (12); RFSS 40–47;
SITE 48–55; FREQUENCY_BAND 56–59, CHANNEL_NUMBER 60–71; SYSTEM_SERVICE_CLASS 72–79. One
message per neighbour site; a site cycles through its neighbours over several seconds, so an
observer must accumulate rather than expect them all at once. The LRA/RFSS/SITE here belong to
the **neighbour**, not to the receiving site.

Opcode 61 is IDEN_UPDATE (§10). Opcode 62 (ADJ_STS_BCST_UNC) is not decoded by value.

**System service class byte** (the `SYSTEM_SERVICE_CLASS` field of all four, reference
`SystemServiceClass`):

| Mask | Meaning |
|---|---|
| `0x01` | composite control channel (carries traffic as well as control) |
| `0x02` | no service requests accepted (announcement-only / "update" channel) |
| `0x04` | backup control channel |
| `0x10` | data service |
| `0x20` | voice service |
| `0x40` | registration service |
| `0x80` | authentication service |

`0x08` is reserved; the decoded `ServiceClass` keeps the raw byte so reserved bits are not lost.

**Channel references resolve like grants** (§10 channel→frequency), through the same IDEN band
map — and, like grants, a broadcast may name a band whose identifier-update has not arrived
yet. The engine emits `P25Event::SiteBroadcast` with `frequency_hz` = `None` in that case,
carrying the raw `(band, channel)` in the parsed message; the observer
(`inventory::SiteInventory`) stores the raw reference and resolves at snapshot time, so a late
IDEN retroactively completes every earlier announcement. That is the "resolve last" dual of
§13's "buffer and replay" rule for pending grants — a site broadcast is idempotent state, not
an action, so it needs no replay. On the 10 s control-channel capture 26 of the broadcasts in one 10 s pass
arrive before their band's IDEN; all of them resolve in the snapshot.
