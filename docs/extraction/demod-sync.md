# P25 P1 demod/sync/timing — extraction. RECREATION-GRADE.

> **Porting notes, derived from sdrtrunk.** These notes summarize how
> [sdrtrunk](https://github.com/DSheirer/sdrtrunk) (© Dennis Sheirer, GPL-3.0)
> implements this part of the P25 Phase 1 decoder, written while porting it to
> `trunkd-p25`. Class, field and constant names refer to sdrtrunk's Java
> sources. They are paraphrased facts, not copied source; trunkd is
> GPL-3.0-or-later as well. See `NOTICE.md` for full attribution.

LSM = the simulcast (CQPSK) modulation of the reference test site.

## KEY CORRECTION
Phase-1 LSM uses `P25P1DemodulatorLSM` (own inline diff-demod + Gardner TED + 1st-order phase-error PLL). The DQPSKGardnerDemodulator/CostasLoop/InterpolatingSampleBuffer classes are **Phase 2 only** — NOT used by P1.

## Sample rate / decimation (both C4FM & LSM identical)
SYMBOL_RATE=4800. Reject sr<=9600. decimation=1; while(sr/decimation>=38400) decimation*=2. decimatedRate=sr/decimation ∈ [19200,38400) i.e. 4–8 sps. Common 25k/50k channel → 25000 Hz → 5.2083 sps. C4FM sps=sr/symrate; LSM sps=decimatedRate/4800.
Decimation filters `DecimationFilterFactory`: cascade of half-band /2 stages. X2=63tap HAMMING, X4=23tap BLACKMAN+X2, X8=15tap BLACKMAN+X4. Half-band taps runtime `FilterFactory.getHalfBand`: center=0.5, even idx = sin(offset*PI/2)/(offset*PI)*window, odd=0. N=4m+3.

## Filters
Baseband LPF: Remez/Parks-McClellan via FIRFilterSpecification.lowPassBuilder, order ESTIMATED (Herrmann 1973 estimator, grid density 16, ripple 0.01 dB both bands). **C4FM pass 5200 / stop 6500 Hz**. **LSM pass 7250 / stop 8000 Hz** (wider = key LSM diff). No hardcoded taps — must port RemezFIRFilterDesigner. Ripple amp = (10^(dB/20)-1)/(10^(dB/20)+1); weight=1/(rippleAmp/maxRippleAmp).
RRC matched filter: getRootRaisedCosine(decimatedRate/4800, span=16, alpha=0.2), applied to I and Q separately. **CRITICAL QUIRK: method halves sps internally (line 650)** → with 5.2083 uses 2.604 → taps=round(2.604*16)=42. Documented "likely mistake" kept for P25/DMR compat. P25 calls it directly and inherits /2 — MUST replicate. Unity DC gain (sum-normalized). Formula = GNU Radio firdes RRC.
Interpolator (C4FM diff demod): 8-tap 128-phase polyphase, table hardcoded `Interpolator.java:27-159` (GNU Radio gr_mmse_fir). idx=(int)(128*mu). LSM & C4FM sub-symbol use LinearInterpolator: x1+(x2-x1)*mu (clamp).

## C4FM demod
Diff demod (`DifferentialDemodulatorFloatScalar`): sps=sr/symrate=5.2083, mu=sps%1=0.2083, interpOffset=floor(sps)-4=1, bufferOverlap=floor(sps)+4=9. Per sample: iCur/qCur = interp(buffer,offset+x,mu); differentialI=iPrev*iCur - (-qPrev)*qCur; differentialQ=iPrev*qCur + iCur*(-qPrev); phase=atan2(dQ,dI). = π/4-DQPSK phase advance over 1 symbol.
Symbol processor `P25P1DemodulatorC4FM`: EQUALIZER_LOOP_GAIN=0.15, MAX_PLL=π/3, MAX_GAIN=1.25, RECALIBRATE_THRESHOLD=π/8, SYNC_THRESHOLD_DETECTION=80, _OPTIMIZED=80, _EQUALIZED=110, DIBIT_LENGTH_NID=33, DIBIT_LENGTH_SYNC=24. samplePoint counter -= 1/sample, emit when <1 then +=sps.
Equalizer: mPll=0, **mGain=1.219** (init — corrects C4FM/DMR constellation compression from π/4-DQPSK demod of pulse-shaped 4FSK; LSM doesn't need it). equalize(sym)=(sym+pll)*gain. Slicing toSymbol: >0: (>π/2)?+3(D01):+1(D00); <=0: (<-π/2)?-3(D11):-1(D10). Boundaries 0,±π/2.
Sync: dual correlators (primary + lagging half-symbol at samplePoint-sps/2). score>80 → Equalizer.optimize() coordinate hill-climb (step sps/8 coarse, sps/16 fine, min sps/200, adjMax sps/2 coarse / sps fine). score()=Σ 24 interp+equalized symbols spaced sps vs SYNC_PATTERN_SYMBOLS. getCorrection: balance over 11×(+3)/13×(-3); balancePlus3/=-11, balanceMinus3/=-13, corr=(sum)/2 clamp ±π/2; gain=Σ(|ideal|-|resampled|)/(24*3π/4). apply: if |pllAdj|>π/8 uninit; if init pll+=adj*0.15, gain+=adj*0.15 else seize (no filter); clamp pll±π/3, gain[1.0,1.25].
isNoisy: stddev of consecutive sample diffs over sync span; reject if > (3π/4)*2/sps*1.2.
validateNID: 33 dibits skip idx11 (status) → 64-bit → BCH decode (NAC-biased); valid only if DUID decoded AND optScore>110.

## LSM demod `P25P1DemodulatorLSM` (AUTHORITATIVE for the LSM path)
MAX_PLL=π/3, OBJECTIVE_MAGNITUDE=1.0. init sampleGain=1, mPLL=0, prevSymbolI=prevSymbolQ=0.7. samplesPerHalfSymbol=sps/2. In-process gains: pllGain=0.1, tedGain=sps/4, maxTimingAdj=sps/25.
Per symbol (emit when samplePoint<1): 
1. two interp I/Q half-symbol apart (Linear): middle @samplePoint, current @+samplesPerHalfSymbol.
2. AGC: mag=sqrt(iC²+qC²); requiredGain=constrain(1/mag,500); sampleGain += (req-gain)*0.05; sampleGain=min(gain,req); apply to all 4.
3. pllI=cos(pll), pllQ=sin(pll).
4. diff demod middle (× conj prevMiddle) then rotate by PLL.
5. diff demod symbol (× conj prevCurrent) rotate by PLL; softSymbol=atan2(qSym,iSym).
6. Gardner TED: timingAdj=(prevSymI-iSym)*iMidDemod + (prevSymQ-qSym)*qMidDemod; constrain ±sps/25; *=sps/4; samplePoint+=adj.
7. PLL: if softSym!=0: hard=toDibit(soft); phaseError=constrain(soft-hard.idealPhase, 0.3); pll -= phaseError*0.1; clamp ±π/3.
8. framer.processWithSoftSyncDetect(soft,hard); if true feedbackDecoder.processPLLError(pll). dibitAssembler.receive(hard).
9. shuffle prev=cur; samplePoint+=sps.
1st-order decision-directed PLL (NOT Costas). Linear interp (not polyphase). toDibit same regions as C4FM.

## Sync pattern & Dibit
SYNC_PATTERN=0x5575F5FF77FF (48 bits/24 dibits), SYNC_MASK=0xFFFFFFFFFFFF. Dibit map: value1→D01_PLUS_3, else→D11_MINUS_3 (sync uses only ±3). Dibit sequence (MSB-first): 1,1,1,1,1,3,1,1,3,3,1,1,3,3,3,3,1,3,1,3,3,3,3,3. Symbols: +3+3+3+3+3-3+3+3-3-3+3+3-3-3-3-3+3-3+3-3-3-3-3-3. Exactly 11×(+3) 13×(-3).
Dibit enum: D01_PLUS_3 (bit1=0,bit2=1) val1 phase +3π/4; D00_PLUS_1 (0,0) val0 +π/4; D10_MINUS_1 (1,0) val2 -π/4; D11_MINUS_3 (1,1) val3 -3π/4. getBitErrorFrom: 0 if equal, 2 if xor==3, else 1.
Soft-sync correlator `P25P1SoftSyncDetector`: 48-len ring written twice; score=Σ x=0..23 SYNC_PATTERN_SYMBOLS[x]*symbols[ptr+x]. Max≈133.2, detect 80, equalized 110.

## Freq-error feedback (auto-PPM)
FeedbackDecoder.processPLLError(pllError): correctionHz=(long)(decimatedRate/(2π)*pllError); broadcast frequencyCorrectionRequest. C4FM feeds equalizer mPll on each sync; LSM feeds pll when frame sync valid. Reset pll=0 on freq-change source events.

## Deviations from TIA-102 / essential vs optim
Essential: 4800 symrate; decimate→LPF→RRC chain + /2 RRC quirk; diff demod; decision boundaries 0,±π/2; sync 0x5575F5FF77FF + 11/13 ±3π/4 seq; correlator + thresholds 80/110; Dibit table; NID 33 dibits skip 11 + BCH(63,16,23); LSM Gardner sign + pllGain 0.1 clamp ±π/3; C4FM 1.219 gain + equalizer updates.
Optim only (can simplify): all SIMD variants; C4FM dual sync detectors + hill-climb (single detector + fixed interp works, less robust low-SNR); NACTracker; isNoisy; delayed dibit line sync overwrite; recalibrate-on-large-error.
