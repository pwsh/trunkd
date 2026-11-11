# P25 P1 Trellis/Viterbi (rate 1/2 & 3/4) — extraction

> **Porting notes, derived from sdrtrunk.** These notes summarize how
> [sdrtrunk](https://github.com/DSheirer/sdrtrunk) (© Dennis Sheirer, GPL-3.0)
> implements this part of the P25 Phase 1 decoder, written while porting it to
> `trunkd-p25`. Class, field and constant names refer to sdrtrunk's Java
> sources. They are paraphrased facts, not copied source; trunkd is
> GPL-3.0-or-later as well. See `NOTICE.md` for full attribution.

- **1/2-rate** (TSBK): 2-bit input / 4-bit output, 4-state transition matrix. 196 bits = 49 four-bit symbols → 96 bits.
- **3/4-rate** (PDU confirmed data): 3-bit input / 4-bit output, 8-state matrix. 196 bits → 144 bits.
- Branch metric = Hamming distance via `HAMMING_ERROR_COUNT[expected ^ transmitted]` lookup.
- Full add-compare-select, keep best path per terminating state, then flush.
- Files: `edac/trellis/ViterbiDecoder_1_2_P25.java`, `ViterbiDecoder_3_4_P25.java`, `P25_1_2_Node.java`, `P25_3_4_Node.java`.

The exact transition tables, deinterleave, block-code polynomials and voice-frame layout are specified in `docs/P25P1-SPEC.md`.
