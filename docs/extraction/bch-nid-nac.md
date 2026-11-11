# P25 P1 NID / BCH(63,16,23) — extraction

> **Porting notes, derived from sdrtrunk.** These notes summarize how
> [sdrtrunk](https://github.com/DSheirer/sdrtrunk) (© Dennis Sheirer, GPL-3.0)
> implements this part of the P25 Phase 1 decoder, written while porting it to
> `trunkd-p25`. Class, field and constant names refer to sdrtrunk's Java
> sources. They are paraphrased facts, not copied source; trunkd is
> GPL-3.0-or-later as well. See `NOTICE.md` for full attribution.

GF(2^6), primitive poly x^6+x+1 = 0x43; n=63 k=16 m=6 t=11, 2t=22 syndromes. Data 16 bits = 12-bit NAC (offset 0) + 4-bit DUID (offset 12). Parity bits 16-62. Bit 63 = overall even parity, IGNORED by decoder.

- `edac/bch/BCH_63_16_23_P25.java` extends BCH_63 extends BCH. No stored generator poly — syndrome-based.
- Antilog/log tables size 64 built at runtime: x<<=1; if(x&64) x^=0x43. a_pow[63]=1, a_log[0]=0. Sequence 1,2,4,8,16,32,3,6,12,24,48,35,5,10,20,40,19,38,15,30,60,59,...
- Syndromes (`BCH.java:903-928`): iterate set bits, syndromes[j]^=a_pow((j+1)*(62-i)) for odd j; even syndromes = gf_sqr(odd). Uses REVERSED bit index (62-i). Loop i<63 excludes bit 63.
- ELP: inline simplified binary Berlekamp-Massey (NOT edac/BerlekempMassey.java, which is unrelated RS code). Roots: Berlekamp Trace Algorithm factorizer; deg1/2/3/4 ad-hoc solvers (deg2 uses xi_tab from buildDegree2Base; deg3/4 → find_affine4_roots → Gaussian elim GF(2)), deg>=5 recursive trace-map GCD split.
- Correction: if roots.len != elp.degree OR duplicate roots → NOT_CORRECTED (-1). Else invert root = 62-root, flip bits, setCorrectedBitCount(count).
- Two-pass (`BCH_63_16_23_P25.decode(msg, observedNAC)`): if first pass NOT_CORRECTED and observedNAC>0 and decoded NAC != tracked, overwrite NAC field with observedNAC and retry once.

## NID codeword mapping — `P25P1MessageFramer.checkNID()` (:815-850)
- DIBIT_LENGTH_NID=33 (32 NID dibits + 1 status dibit). mNIDBuffer[33].
- Build 64-bit msg: for i in 0..33, skip i==11 (status symbol), add dibit.getBit1() then getBit2() (MSB-first).
- decode(nid, trackedNAC); nac=getInt(len12@0); duid=fromValue(getInt(len4@12)); if correctedBitCount<0 return false; NACTracker.track(nac); nidDetected(...).

## NACTracker (`NACTracker.java`)
MAX_TRACKER_COUNT=3, MIN_OBSERVATION_THRESHOLD=3. Map nac->Tracker{count,timestamp}. track(): new→insert count=1, evict oldest-timestamp if >3 entries; seen→count++ & refresh ts. getTrackedNAC(): highest count; return its nac only if count>=3 else 0. reset() clears (after sync loss).

## APCO25Nac
IdentifierClass.NETWORK, Form.NETWORK_ACCESS_CODE, Role.BROADCAST, Protocol.APCO25. isValid = value>0. toString = "value/x%03X".

## Dibit enum (`dsp/symbol/Dibit.java`)
value = bit1*2+bit2, bit1=MSB bit2=LSB. D00=0,D01=1,D10=2,D11=3. TCM: transition1 flips LSB, transition2 flips MSB. getBitErrorFrom returns 0/1/2 (xor==3 → 2).
