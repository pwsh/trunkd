# P25P1-SPEC.md — gaps, ambiguities, and resolutions

Findings from the clean-room implementation of `crates/trunkd-p25` built
solely from `docs/P25P1-SPEC.md`. Each entry records the ambiguity, the
engineering choice made, and how it was validated (where possible, against
a real 10 s, 2.4 Msps control-channel capture of an 800 MHz LSM site, kept
locally under `fixtures/` and not shipped).

## Resolved empirically (spec wrong or under-specified, answer recovered)

### 1. CRC identity-row orientation (§8.4a, §8.4c) — **spec listing is misleading**
The CRC-CCITT-16 table lists its 16 identity rows as `0x0001, 0x0002, …,
0x8000`, and the decode pattern says "accumulate `CHECKSUMS[i]` over set
message bits". Taken literally (row 80 = 0x0001 ↔ message bit 80), residuals
on real on-air TSBKs are never 0/all-ones. Empirically (8 distinct repeated
error-free blocks from the capture) the CRC field maps **MSB-first**: message
bit 80 (field MSB) ↔ 0x8000 — the identity region must be indexed in
*reverse*. With that reading, every clean on-air block gives residual 0 or
0xFFFF exactly. The same reversed reading was applied to the CRC-32 PDU
tables' trailing 32 identity rows (not yet verifiable on-air — no PDUs in the
capture). Also verified: the real transmitted CRC equals CRC-CCITT
(poly 0x1021, init 0xFFFF) XOR a constant 0x1EC6, which is why the "0 or
all-ones" acceptance is needed. The data rows themselves are exactly
`~(x^(95−i) mod g)` — verified in unit tests for all three CRC tables.

### 2. BCH syndrome indexing (§8.1)
"for odd j, `syndrome[j] ^= a_pow[((j+1)·(62−i)) mod 63]`" mixes 0-based
array index with 1-based syndrome number. Read as: syndromes s₁..s₂₂ with
s_j = Σ α^(j·(62−i)); even syndromes squared from s_(j/2). Implemented with
Berlekamp–Massey + Chien search (explicitly permitted: "the algebra, not the
algorithm, is normative"). Validated by round-trip encode (generator built
from the field's minimal polynomials) with up to 11 injected errors, and by
253/253 NAC-consistent NID decodes on the capture.

## Interpretation choices (spec ambiguous, choice documented)

### 3. "Sync detect only honored when no assembler is active" (§7.3)
Contradicted one paragraph later by `nidDetected` explicitly handling a
still-active assembler (dispatch/force-complete). Since a PLACE_HOLDER
assembler (866 dibits) is active most of the time on a control channel, a
literal reading would miss most frames. Choice: the soft sync detector always
runs and always (re)starts NID capture; assembler handling happens at NID
validation exactly as §7.3 describes. Validated: back-to-back frame decode on
the capture with zero-sync-loss accounting between adjacent frames.

### 4. Dibit-counter bookkeeping at dispatch (§7.3/§7.4)
The sync-loss rule ("dibitCounter −= 57 at NID; positive remainder → loss")
only balances if the counter is also reduced when a frame completes. The
"elapsed dibits" column of the DUID table is interpreted as exactly that
dispatch-time adjustment: on terminal completion, `dibitCounter -=
elapsed(duid)`. With the delayed-one-dibit dispatch this makes contiguous
frames account to exactly zero sync loss (verified on the capture and in the
framer unit test).

**Status (closed, spec §7.9).** Two exceptions are now normative and were the
cause of negative sync-loss reports: a *completed placeholder* is not charged
its elapsed dibits (it stands in for dibits no frame claimed, and letting the
counter run is what reaches the one-second report), and a placeholder starts
only at `ASSEMBLY_START_DIBIT` = 57 while `current_nac > 0`. Force-completion
reports positive dropped bits (`target − buffer_len`) and charges
`min(elapsed(duid), dibit_counter − 57)` instead of the reference's DUID
downgrade.

### 5. Status-dibit counter semantics (§7.2)
"when the counter reaches 36 that dibit is diverted" + startup value 36 +
post-NID restart at 21 only line up as *increment-then-compare* with a `>=`
divert (startup 36 → first dibit is 37 → diverted, harmless pre-sync).
Post-NID value 21 then puts the next divert exactly at frame dibit 71 as the
transmitter cadence requires. Validated by the framer cadence unit test and
by real TSBK decodes (a one-dibit cadence error scrambles every block).

### 6. PDU multi-block CRC-32 message geometry (§8.4c)
CRC positions 224/320/416 with coverage starting at bit 160 imply a message
of a **160-bit header region** followed by 96-bit unconfirmed blocks (the
region [96,160) is uncovered and unspecified; implemented as zero padding
after the 96 decoded header bits). Tables exist only for 1–3 blocks;
CRC-32 is skipped for confirmed PDUs (CRC-9 per block instead) and for
unconfirmed PDUs with more than 3 blocks. Untested against real traffic (the
capture contains no PDUs).

### 7. Golay24 return-code details (§8.2)
Whether a syndrome-0 word with a wrong overall-parity bit counts as "clean"
(0) or "corrected" (1) is unspecified; implemented as 0 with the parity bit
silently repaired (the parity bit is not data). The "parity detects a 4th
error" behavior is implemented as: rotate-and-search failure → 2
(uncorrectable); overall parity is re-established after any core correction.
Exhaustive tests confirm all ≤3-bit patterns correct for Golay23/24/18.

**Status (superseded, spec §8.2).** "Rotate-and-search failure → 2" is not a
reachable detection: Golay(23,12,7) is *perfect*, so the inner search always
succeeds. Uncorrectable is now decided by the **total 24-bit flip count**
(core + overall-parity repair) exceeding t = 3, with the word left unmodified;
all 10626 weight-4 patterns are detected. Golay18 additionally reports
uncorrectable when a decode sets any of the six known-zero data bits. The
syndrome-0/parity-repair case still returns 0 (clean), as recorded above.

### 8. Half-band tap parity (§3.1)
"for even offsets from center, `sin(offset·π/2)/(offset·π)`; odd offsets are
exactly 0" — mathematically inverted: `sin(kπ/2)` is zero at *even* k; a
half-band filter has its zeros at even offsets and energy at odd ones.
Implemented the mathematically consistent version (center 0.5, odd offsets
windowed-sinc, even offsets zero). The spec's own formula produces the same
taps if applied at odd offsets, so this is a wording slip.

### 9. Unknown-DUID NID accounting (§7.3)
"if DUID is unknown, coerce to PLACE_HOLDER and keep the previous NAC" —
whether such NIDs count toward nid_count/NAC histogram is unspecified. They
are counted (BCH passed), the decoded NAC is recorded in the histogram and
NAC tracker, but `current_nac` (used for parsing context) is unchanged.

### 10. NAC-tracker reset scope (§8.1)
"reset on sync loss" — implemented as reset on the free-running 1-second
sync-loss report only (true signal loss), not on the small per-frame
accounting reports, which fire routinely between frames.

**Status (extended, spec §7.8b).** Because that reset leaves a traffic channel
unestablished between transmissions, the false-NID gate falls back to
`current_nac` — the last NAC that produced a real frame, cleared only by
`Framer::reset()`. A NID whose NAC differs from the established one **and**
needed ≥ `NID_NAC_MISMATCH_CORRECTION_LIMIT` (9) corrections is rejected as a
BCH false accept; a decoded NAC of 0 never becomes the system NAC.

## Explicit substitutions permitted by §15 (documented)

### 11. Baseband LPF: Kaiser windowed-sinc instead of Remez (§3.2)
Band edges per spec (C4FM 5200/6500, LSM 7250/8000) and the ripple-derived
≈64.8 dB stopband are honored; tap values differ from a true Parks-McClellan
design. Frequency-response assertions in unit tests confirm the edges.
(The task brief allowed "an equivalent equiripple/windowed design meeting
the stated band edges — document your choice"; this is that documentation.)

### 12. C4FM: single correlator + linear interpolation (§5, §15)
The dual sync correlator, 128-phase MMSE interpolator (whose verbatim table
the spec does not include and outside sources are off-limits) and the exact
coordinate hill-climb are replaced by a single correlator on equalized soft
samples with linear interpolation and a coarse(sps/8)/fine(sps/16) local
timing search — the §15-sanctioned simplification. All specified equalizer
math (1.219 initial gain, 0.15 loop gain, ±π/3 PLL clamp, [1.0,1.25] gain
clamp, π/8 recalibrate threshold, balance/gain-adjust formulas, 80/110
thresholds) is implemented as written. LSM (§6), the primary path, is
implemented exactly and validated on the real capture.

## Not specified at all (stubs / raw passthrough)

### 13. Link-control (72-bit) and encryption-sync (96-bit) field layouts
§12.3 says the LC word "carries talkgroup/source" and the ES word carries
"algorithm ID, key ID, message indicator", but gives no bit offsets. Both are
emitted as raw words; the traffic-side same-call check (§13) that needs
TO/FROM from LC therefore has hooks (`traffic_call_update`) but no LC parser.
Same for the HDU's 120-bit header data (MFID/ALGID/KID/TGID offsets absent):
emitted raw with validity + corrected counts, which §11 says is all parity
requires.

**Status: resolved — spec §18.1 (LC), §18.2 (ES), §18.3 (HDU header).** All
three are parsed; `traffic_call_update_with` now receives real TO/FROM from the
LDU1 LC (§13.2).

### 14. LDU low-speed data parity (§12.2)
The 16 parity bits' code is unspecified ("the other 16 bits … are their
parity"); the two data bytes are extracted unchecked.

**Status: resolved — spec §18.6.** The code is a (16,8,5) shortened cyclic code
with g(x) = x⁸+x⁵+x⁴+x³+1, decoded per octet. Beyond the 2-error bound the
decoder reports the **raw** received data byte with `errors = 0` and
`valid = false` rather than an arbitrary nearest codeword.

### 15. Telephone interconnect grant *update* (opcode 9) layout
§10 lists opcode 9 but gives a payload layout only for opcode 8; opcode 9 is
parsed with the same layout.

**Status: resolved — spec §18.4.** The wire layout is identical; only the
address *type* at bits 56–79 differs (radio unit for opcode 8, talkgroup for
opcode 9).

## Capture-validation results (for the §16 parity diff)

`p25_decode fixtures/<capture>.iq --rate 2400000
--format int8 --offset <OFFSET_HZ> --freq <CONTROL_HZ>`:

- sync_count 273, nid_count 253, the site's NAC ×252 (+1 miscorrect)
- tsbk_count 224; opcodes {1:31, 2:12, 15:34, 22:16, 40:1, 44:1, 56:16,
  57:16, 58:35, 59:31, 60:15, 61:16}
  (**updated:** tsbk_count **227** after the §7.8a dropped-dibit fix — the three
  extra blocks are one GRP_VCH_GRNT_UPD and two GRP_VCH_GRANT, all clean-CRC;
  tsbk_failed 29 → 26, nid_count unchanged at 253.)
- IDEN 0: base 769.00625 MHz, spacing 12.5 kHz, offset +30 MHz, bw 12.5 kHz;
  IDEN 1: base 851.0125 MHz, spacing 12.5 kHz, offset −45 MHz, bw 12.5 kHz
- grants list empty: the only grant activity (12 × opcode 2 for band 1)
  occurs in the first ~2.5 s, before the first band-1 IDEN arrives — the
  spec-mandated "silently unusable" path (§10/§13). A reference decoder
  started cold on the same file hits the same ordering.
- corrected_bits_total 437; ≈6.3× real time in release on this machine
  including the 2.4 Msps DDC.
