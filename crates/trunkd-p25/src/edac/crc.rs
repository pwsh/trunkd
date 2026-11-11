// SPDX-License-Identifier: GPL-3.0-or-later
//! CRC-CCITT-16, CRC-9 and CRC-32 (PDU) per spec §8.4.
//!
//! Decode pattern: accumulate table rows over **set** message bits, giving a
//! residual. For CRC-16 the residual a correct codeword lands on is fixed by
//! the parity of its data bits — see [`crc16_expected_residual`] — so exactly
//! one of 0 / all-ones is acceptable per block. CRC-9 and CRC-32 still accept
//! either (see their own notes). Otherwise attempt single-bit correction: find
//! the unique table row that moves the residual onto the value the corrected
//! block's parity requires, and flip that bit.
//!
//! Row semantics (verified against the generator polynomials in unit tests):
//! data rows are the bitwise complement of `x^e mod g(x)` for the bit's
//! position, and the trailing identity rows fold the transmitted CRC field
//! into the same accumulation.

use crate::bits::BitBuf;

/// Outcome of a CRC check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrcResult {
    /// Residual was the value this block's data parity requires.
    Passed,
    /// A single bit error was located and flipped (message bit index).
    Corrected(usize),
    /// Unrecoverable.
    Failed,
    /// CRC-16 only: the residual was the *complement* of the value the data
    /// parity requires — the block cannot have been transmitted as received.
    /// Split out from [`CrcResult::Failed`] because this is precisely the
    /// class the old dual-residual acceptance let through, and counting it
    /// measures what the narrowed rule now rejects.
    FailedWrongParity,
}

impl CrcResult {
    /// True for `Passed` or `Corrected`.
    pub fn is_valid(self) -> bool {
        matches!(self, CrcResult::Passed | CrcResult::Corrected(_))
    }

    /// Corrected bit count contributed by this check (0 or 1).
    pub fn corrected_bits(self) -> u32 {
        matches!(self, CrcResult::Corrected(_)) as u32
    }
}

/// CRC-CCITT-16 table: 96 entries — 80 data rows then 16 identity rows.
/// Polynomial 0x11021, init 0xFFFF.
pub const CRC16_TABLE: [u16; 96] = [
    0x1BCB, 0x8DE5, 0xC6F2, 0x6B69, 0xB5B4, 0x52CA, 0x2175, 0x90BA, 0x404D, 0xA026, 0x5803, 0xAC01,
    0xD600, 0x6310, 0x3998, 0x14DC, 0x027E, 0x092F, 0x8497, 0xC24B, 0xE125, 0xF092, 0x7059, 0xB82C,
    0x5406, 0x2213, 0x9109, 0xC884, 0x6C52, 0x3E39, 0x9F1C, 0x479E, 0x2BDF, 0x95EF, 0xCAF7, 0xE57B,
    0xF2BD, 0xF95E, 0x74BF, 0xBA5F, 0xDD2F, 0xEE97, 0xF74B, 0xFBA5, 0xFDD2, 0x76F9, 0xBB7C, 0x55AE,
    0x22C7, 0x9163, 0xC8B1, 0xE458, 0x7A3C, 0x350E, 0x1297, 0x894B, 0xC4A5, 0xE252, 0x7939, 0xBC9C,
    0x565E, 0x233F, 0x919F, 0xC8CF, 0xE467, 0xF233, 0xF919, 0xFC8C, 0x7656, 0x333B, 0x999D, 0xCCCE,
    0x6E77, 0xB73B, 0xDB9D, 0xEDCE, 0x7EF7, 0xBF7B, 0xDFBD, 0xEFDE, 0x0001, 0x0002, 0x0004, 0x0008,
    0x0010, 0x0020, 0x0040, 0x0080, 0x0100, 0x0200, 0x0400, 0x0800, 0x1000, 0x2000, 0x4000, 0x8000,
];

/// Effective table row for message bit `i` of a CRC16-protected 96-bit
/// region. Data bits (0..80) use rows directly; the CRC field's identity
/// rows map **MSB-first** — bit 80 (field MSB) ↔ 0x8000 — i.e. the identity
/// region of the table is indexed in reverse (verified against on-air
/// codewords; see docs/SPEC-GAPS.md).
#[inline]
fn crc16_row(i: usize) -> u16 {
    if i < 80 {
        CRC16_TABLE[i]
    } else {
        CRC16_TABLE[80 + (95 - i)]
    }
}

/// The residual a correct CRC-16 codeword lands on, given the number of set
/// bits among its 80 data bits.
///
/// The table's data rows are the *complement* of `x^e mod g(x)`, which folds
/// one copy of the 0xFFFF init into the accumulation per set data bit. So a
/// codeword with an odd number of set data bits accumulates one surviving
/// 0xFFFF and cancels to `0x0000`, and one with an even number cancels to
/// `0xFFFF`. Accepting both — as this decoder used to — throws away one bit of
/// the CRC and doubles the false-pass rate to ~1 in 2^15.
///
/// Verified on air: of 6,120,199 logged TSBKs, 95 carried the residual their
/// parity forbids (private/docs/analysis/unknown-inventory-2026-09-03.md), and every
/// constant broadcast on the measured system obeys the rule exactly.
#[inline]
pub fn crc16_expected_residual(data_ones: u32) -> u16 {
    if data_ones % 2 == 1 {
        0x0000
    } else {
        0xFFFF
    }
}

/// Check (and possibly correct) a 96-bit region `[start, start+96)` of `msg`
/// protected by CRC-CCITT-16 (80 data bits + 16 CRC bits).
///
/// Acceptance is the single residual [`crc16_expected_residual`] gives for the
/// block's own data parity. A residual equal to its complement is reported as
/// [`CrcResult::FailedWrongParity`]; no single-bit flip can reach a valid
/// codeword from there (that would need a row of 0x0000 or 0xFFFF, and the
/// table has neither), so the early return loses no correction.
pub fn crc16_check_and_correct(msg: &mut BitBuf, start: usize) -> CrcResult {
    let mut residual: u16 = 0;
    let mut data_ones: u32 = 0;
    for i in 0..96 {
        if msg.get(start + i) {
            residual ^= crc16_row(i);
            data_ones += u32::from(i < 80);
        }
    }
    let expected = crc16_expected_residual(data_ones);
    if residual == expected {
        return CrcResult::Passed;
    }
    if residual == !expected {
        return CrcResult::FailedWrongParity;
    }
    // Flipping a *data* bit toggles the data parity, so the residual the
    // corrected block must land on is `!expected`; flipping a *CRC field* bit
    // leaves the parity — and so the target — alone.
    let want_data = residual ^ !expected;
    if let Some(i) = (0..80).find(|&i| crc16_row(i) == want_data) {
        msg.flip(start + i);
        return CrcResult::Corrected(start + i);
    }
    let want_crc = residual ^ expected;
    if let Some(i) = (80..96).find(|&i| crc16_row(i) == want_crc) {
        msg.flip(start + i);
        return CrcResult::Corrected(start + i);
    }
    CrcResult::Failed
}

/// CRC-9 table: 135 entries (7 rows for bits 0–6, then 128 rows for bits
/// 16–143; the CRC field occupies bits 7–15). Polynomial 0x259, init 0x1FF.
pub const CRC9_TABLE: [u16; 135] = [
    0x1E7, 0x1F3, 0x1F9, 0x1FC, 0x0D2, 0x045, 0x122, 0x0BD, 0x15E, 0x083, 0x141, 0x1A0, 0x0FC,
    0x052, 0x005, 0x102, 0x0AD, 0x156, 0x087, 0x143, 0x1A1, 0x1D0, 0x0C4, 0x04E, 0x00B, 0x105,
    0x182, 0x0ED, 0x176, 0x097, 0x14B, 0x1A5, 0x1D2, 0x0C5, 0x162, 0x09D, 0x14E, 0x08B, 0x145,
    0x1A2, 0x0FD, 0x17E, 0x093, 0x149, 0x1A4, 0x0FE, 0x053, 0x129, 0x194, 0x0E6, 0x05F, 0x12F,
    0x197, 0x1CB, 0x1E5, 0x1F2, 0x0D5, 0x16A, 0x099, 0x14C, 0x08A, 0x069, 0x134, 0x0B6, 0x077,
    0x13B, 0x19D, 0x1CE, 0x0CB, 0x165, 0x1B2, 0x0F5, 0x17A, 0x091, 0x148, 0x088, 0x068, 0x018,
    0x020, 0x03C, 0x032, 0x035, 0x11A, 0x0A1, 0x150, 0x084, 0x06E, 0x01B, 0x10D, 0x186, 0x0EF,
    0x177, 0x1BB, 0x1DD, 0x1EE, 0x0DB, 0x16D, 0x1B6, 0x0F7, 0x17B, 0x1BD, 0x1DE, 0x0C3, 0x161,
    0x1B0, 0x0F4, 0x056, 0x007, 0x103, 0x181, 0x1C0, 0x0CC, 0x04A, 0x009, 0x104, 0x0AE, 0x07B,
    0x13D, 0x19E, 0x0E3, 0x171, 0x1B8, 0x0F0, 0x054, 0x006, 0x02F, 0x117, 0x18B, 0x1C5, 0x1E2,
    0x0DD, 0x16E, 0x09B, 0x14D, 0x1A6,
];

/// Check the CRC-9 of a 144-bit confirmed PDU data block at `[start,
/// start+144)`. The 9-bit CRC sits at bits 7–15; covered bits are 0–6 and
/// 16–143. Detection only (spec §8.4b).
///
/// **Known blind spot (deliberate, do not "fix" casually).** Acceptance is
/// `residual == 0 || residual == 0x1FF` — the dual-residual rule inherited
/// from the spec/reference, which covers both the plain and the inverted
/// (all-ones init) convention. Because the all-ones residual is also
/// accepted, any error pattern whose row XOR equals 0x1FF slips through.
/// Exhaustive enumeration over the 144-bit block finds **17** undetected
/// weight-2 patterns:
///
/// * 16 pairs of covered data bits whose rows XOR to 0x1FF — (0,86), (2,133),
///   (6,139), (17,92), (20,59), (21,117), (24,49), (30,134), (31,74), (43,44),
///   (52,72), (61,85), (71,77), (82,140), (84,100), (94,108). These are the
///   dual-residual rule's own blind spot: a single-residual CRC-9 catches them.
/// * 1 pair — (10,87) — where a CRC-field bit and a data bit have identical
///   residual effect (row 78 = 0x020 = the identity effect of field bit 10),
///   giving residual 0. That one is a property of this table/polynomial and
///   would be missed by any residual rule.
///
/// Which convention real traffic uses has not been confirmed on air, so the
/// acceptance semantics are left exactly as the reference has them; the test
/// `crc9_dual_residual_blind_spot` pins the current behavior so that any
/// future narrowing is a deliberate, verified change.
pub fn crc9_check(msg: &BitBuf, start: usize) -> CrcResult {
    let mut acc: u16 = 0;
    for i in 0..144usize {
        if (7..16).contains(&i) {
            continue;
        }
        if msg.get(start + i) {
            let row = if i < 7 { i } else { i - 9 };
            acc ^= CRC9_TABLE[row];
        }
    }
    let transmitted = msg.get_u32(start + 7, 9) as u16;
    let residual = acc ^ transmitted;
    if residual == 0 || residual == 0x1FF {
        CrcResult::Passed
    } else {
        CrcResult::Failed
    }
}

/// CRC-32 (PDU multi-block) — polynomial 0x104C11DB7, init 0xFFFFFFFF.
/// PDU1 table: 96 rows (64 data + 32 identity). PDU2 prepends 96 more rows;
/// PDU3 prepends another 96 (spec §8.4c).
pub const CRC32_PDU1_TABLE: [u32; 96] = [
    0x86FFAACC, 0x411F5BBD, 0xA08FADDE, 0x52275834, 0x2B7322C1, 0x95B99160, 0x48BC466B, 0xA45E2335,
    0xD22F119A, 0x6B770616, 0x37DB0DD0, 0x198D0833, 0x8CC68419, 0xC663420C, 0x61512FDD, 0xB0A897EE,
    0x5A34C52C, 0x2F7AEC4D, 0x97BD7626, 0x49BE35C8, 0x26BF943F, 0x935FCA1F, 0xC9AFE50F, 0xE4D7F287,
    0xF26BF943, 0xF935FCA1, 0xFC9AFE50, 0x7C2DF1F3, 0xBE16F8F9, 0xDF0B7C7C, 0x6DE530E5, 0xB6F29872,
    0x5919C2E2, 0x2EEC6FAA, 0x1516B90E, 0x08EBD25C, 0x061567F5, 0x830AB3FA, 0x43E5D726, 0x23926548,
    0x13A9BC7F, 0x89D4DE3F, 0xC4EA6F1F, 0xE275378F, 0xF13A9BC7, 0xF89D4DE3, 0xFC4EA6F1, 0xFE275378,
    0x7D732767, 0xBEB993B3, 0xDF5CC9D9, 0xEFAE64EC, 0x75B7BCAD, 0xBADBDE56, 0x5F0D61F0, 0x2DE63E23,
    0x96F31F11, 0xCB798F88, 0x67DC491F, 0xB3EE248F, 0xD9F71247, 0xECFB8923, 0xF67DC491, 0xFB3EE248,
    0x00000001, 0x00000002, 0x00000004, 0x00000008, 0x00000010, 0x00000020, 0x00000040, 0x00000080,
    0x00000100, 0x00000200, 0x00000400, 0x00000800, 0x00001000, 0x00002000, 0x00004000, 0x00008000,
    0x00010000, 0x00020000, 0x00040000, 0x00080000, 0x00100000, 0x00200000, 0x00400000, 0x00800000,
    0x01000000, 0x02000000, 0x04000000, 0x08000000, 0x10000000, 0x20000000, 0x40000000, 0x80000000,
];

/// The 96 leading rows prepended to [`CRC32_PDU1_TABLE`] to form the PDU2
/// table (192 rows).
pub const CRC32_PDU2_LEAD: [u32; 96] = [
    0x9D231959, 0xCE918CAC, 0x6528488D, 0xB2942446, 0x5B2A9CF8, 0x2FF5C0A7, 0x97FAE053, 0xCBFD7029,
    0xE5FEB814, 0x709FD2D1, 0xB84FE968, 0x5E477A6F, 0xAF23BD37, 0xD791DE9B, 0xEBC8EF4D, 0xF5E477A6,
    0x7892B508, 0x3E29D45F, 0x9F14EA2F, 0xCF8A7517, 0xE7C53A8B, 0xF3E29D45, 0xF9F14EA2, 0x7E98298A,
    0x3D2C9A1E, 0x1CF6C3D4, 0x0C1BEF31, 0x860DF798, 0x41667517, 0xA0B33A8B, 0xD0599D45, 0xE82CCEA2,
    0x7676E98A, 0x395BFA1E, 0x1ECD73D4, 0x0D063731, 0x86831B98, 0x41210317, 0xA090818B, 0xD04840C5,
    0xE8242062, 0x76729EEA, 0x3959C1AE, 0x1ECC6E0C, 0x0D06B9DD, 0x86835CEE, 0x412120AC, 0x22F01E8D,
    0x91780F46, 0x4ADC8978, 0x270ECA67, 0x93876533, 0xC9C3B299, 0xE4E1D94C, 0x7010627D, 0xB808313E,
    0x5E649644, 0x2D52C5F9, 0x96A962FC, 0x49343FA5, 0xA49A1FD2, 0x502D8132, 0x2A764E42, 0x175BA9FA,
    0x09CD5A26, 0x068623C8, 0x01239F3F, 0x8091CF9F, 0xC048E7CF, 0xE02473E7, 0xF01239F3, 0xF8091CF9,
    0xFC048E7C, 0x7C62C9E5, 0xBE3164F2, 0x5D783CA2, 0x2CDC908A, 0x140EC69E, 0x0867ED94, 0x06537811,
    0x8329BC08, 0x43F450DF, 0xA1FA286F, 0xD0FD1437, 0xE87E8A1B, 0xF43F450D, 0xFA1FA286, 0x7F6F5F98,
    0x3DD72117, 0x9EEB908B, 0xCF75C845, 0xE7BAE422, 0x71BDFCCA, 0x3ABE70BE, 0x1F3FB684, 0x0DFF5599,
];

/// The 96 leading rows prepended to the PDU2 table to form the PDU3 table
/// (288 rows).
pub const CRC32_PDU3_LEAD: [u32; 96] = [
    0xAA5FA470, 0x574F5CE3, 0xABA7AE71, 0xD5D3D738, 0x68896547, 0xB444B2A3, 0xDA225951, 0xED112CA8,
    0x74E8188F, 0xBA740C47, 0xDD3A0623, 0xEE9D0311, 0xF74E8188, 0x79C7CE1F, 0xBCE3E70F, 0xDE71F387,
    0xEF38F9C3, 0xF79C7CE1, 0xFBCE3E70, 0x7F8791E3, 0xBFC3C8F1, 0xDFE1E478, 0x6D907CE7, 0xB6C83E73,
    0xDB641F39, 0xEDB20F9C, 0x74B98915, 0xBA5CC48A, 0x5F4EEC9E, 0x2DC7F894, 0x14837291, 0x8A41B948,
    0x4740527F, 0xA3A0293F, 0xD1D0149F, 0xE8E80A4F, 0xF4740527, 0xFA3A0293, 0xFD1D0149, 0xFE8E80A4,
    0x7D27CE89, 0xBE93E744, 0x5D297D79, 0xAE94BEBC, 0x552AD185, 0xAA9568C2, 0x572A3ABA, 0x29F59386,
    0x169A4718, 0x092DAD57, 0x8496D6AB, 0xC24B6B55, 0xE125B5AA, 0x72F2540E, 0x3B19A4DC, 0x1FEC5CB5,
    0x8FF62E5A, 0x459B99F6, 0x20AD4220, 0x12362FCB, 0x891B17E5, 0xC48D8BF2, 0x60264B22, 0x3273AB4A,
    0x1B595B7E, 0x0FCC2364, 0x05869F69, 0x82C34FB4, 0x43012901, 0xA1809480, 0x52A0C49B, 0xA950624D,
    0xD4A83126, 0x68349648, 0x367AC5FF, 0x9B3D62FF, 0xCD9EB17F, 0xE6CF58BF, 0xF367AC5F, 0xF9B3D62F,
    0xFCD9EB17, 0xFE6CF58B, 0xFF367AC5, 0xFF9B3D62, 0x7DAD106A, 0x3CB606EE, 0x1C3B8DAC, 0x0C7D480D,
    0x863EA406, 0x417FDCD8, 0x22DF60B7, 0x916FB05B, 0xC8B7D82D, 0xE45BEC16, 0x704D78D0, 0x3A4632B3,
];

/// Number of unconfirmed PDU data blocks the CRC-32 tables cover.
pub const CRC32_MAX_BLOCKS: usize = 3;

/// Row of the effective PDU-n CRC-32 table (n = 1..=3, `rows` = 96·n).
/// As with CRC16, the trailing 32 identity rows map the CRC field
/// **MSB-first** (field MSB ↔ 0x80000000): the identity region is indexed in
/// reverse.
fn crc32_row(blocks: usize, i: usize) -> u32 {
    // The tables nest: PDU3 = PDU3_LEAD ‖ PDU2_LEAD ‖ PDU1.
    assert!(
        (1..=CRC32_MAX_BLOCKS).contains(&blocks),
        "CRC-32 PDU tables cover 1..={CRC32_MAX_BLOCKS} blocks, got {blocks}"
    );
    let total = 96 * blocks;
    assert!(i < total, "row {i} out of range for {blocks} blocks");
    let covered = total - 32;
    let idx = if i < covered {
        i
    } else {
        covered + (total - 1 - i)
    };
    let from_end = total - 1 - idx;
    if from_end < 96 {
        CRC32_PDU1_TABLE[95 - from_end]
    } else if from_end < 192 {
        CRC32_PDU2_LEAD[95 - (from_end - 96)]
    } else {
        CRC32_PDU3_LEAD[95 - (from_end - 192)]
    }
}

/// Check (and possibly single-bit correct) the CRC-32 of a multi-block PDU
/// with `blocks` (1..=3) unconfirmed data blocks. The covered region starts
/// at message bit 160; the CRC field ends at `160 + 96·blocks` (CRC positions
/// 224/320/416 per spec). Corrections flip `errorRow + 160`.
///
/// # Panics
/// `blocks` must be in `1..=`[`CRC32_MAX_BLOCKS`]; there is no table outside
/// that range (0 would underflow the covered-region arithmetic and silently
/// "pass" in release, ≥4 would index past the tables). Callers guard on the
/// PDU header block count.
pub fn crc32_pdu_check_and_correct(msg: &mut BitBuf, blocks: usize) -> CrcResult {
    assert!(
        (1..=CRC32_MAX_BLOCKS).contains(&blocks),
        "CRC-32 PDU check supports 1..={CRC32_MAX_BLOCKS} blocks, got {blocks}"
    );
    let rows = 96 * blocks;
    let mut residual: u32 = 0;
    for i in 0..rows {
        if msg.get(160 + i) {
            residual ^= crc32_row(blocks, i);
        }
    }
    if residual == 0 || residual == 0xFFFF_FFFF {
        return CrcResult::Passed;
    }
    for i in 0..rows {
        if crc32_row(blocks, i) == residual {
            msg.flip(160 + i);
            return CrcResult::Corrected(160 + i);
        }
    }
    let comp = residual ^ 0xFFFF_FFFF;
    for i in 0..rows {
        if crc32_row(blocks, i) == comp {
            msg.flip(160 + i);
            return CrcResult::Corrected(160 + i);
        }
    }
    CrcResult::Failed
}

#[cfg(test)]
mod tests {
    use super::*;

    /// x^e mod g, for a generator polynomial `g` with `gbits` significant
    /// low-order bits (leading term implicit).
    fn polymod(e: usize, g: u64, gbits: u32) -> u64 {
        let mut r: u64 = 1;
        for _ in 0..e {
            r <<= 1;
            if (r >> gbits) & 1 != 0 {
                r ^= g;
            }
        }
        r & ((1u64 << gbits) - 1)
    }

    #[test]
    fn crc16_table_matches_polynomial() {
        for (i, &row) in CRC16_TABLE.iter().enumerate().take(80) {
            let expect = (polymod(95 - i, 0x11021, 16) ^ 0xFFFF) as u16;
            assert_eq!(row, expect, "row {i}");
        }
        for (k, &row) in CRC16_TABLE.iter().enumerate().skip(80) {
            assert_eq!(row, 1 << (k - 80), "identity row {k}");
        }
    }

    #[test]
    fn crc9_table_matches_polynomial() {
        for (i, &row) in CRC9_TABLE.iter().enumerate() {
            let e = if i < 7 { 143 - i } else { 136 - (i - 7) };
            let expect = (polymod(e, 0x259, 9) ^ 0x1FF) as u16;
            assert_eq!(row, expect, "row {i}");
        }
    }

    #[test]
    fn crc32_tables_match_polynomial() {
        // PDU-n data rows are ~(x^(96n − 1 − i) mod g); identity rows follow.
        for blocks in 1..=3usize {
            let rows = 96 * blocks;
            for i in 0..rows - 32 {
                let e = rows - 1 - i;
                let expect = (polymod(e, 0x104C11DB7, 32) ^ 0xFFFF_FFFF) as u32;
                assert_eq!(crc32_row(blocks, i), expect, "blocks {blocks} row {i}");
            }
            // Identity rows map the CRC field MSB-first (reversed indexing).
            for k in 0..32 {
                assert_eq!(crc32_row(blocks, rows - 32 + k), 1 << (31 - k));
            }
        }
    }

    /// Build a valid 96-bit CRC16 message: 80 random-ish data bits plus a CRC
    /// field chosen so the table residual is the one the data parity requires
    /// (0x0000 for odd weight, 0xFFFF for even).
    fn make_crc16_msg(seed: u64) -> BitBuf {
        let mut b = BitBuf::new();
        let mut s = seed;
        for _ in 0..96 {
            b.push(false);
        }
        for i in 0..80 {
            s = s
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            b.set(i, (s >> 62) & 1 != 0);
        }
        let mut acc: u16 = 0;
        let mut ones: u32 = 0;
        for (i, &row) in CRC16_TABLE.iter().enumerate().take(80) {
            if b.get(i) {
                acc ^= row;
                ones += 1;
            }
        }
        // Set CRC-region bits (MSB-first mapping) so the residual lands on the
        // value this block's data parity requires.
        let field = acc ^ crc16_expected_residual(ones);
        for k in 0..16 {
            b.set(80 + k, (field >> (15 - k)) & 1 != 0);
        }
        b
    }

    /// The residual of a 96-bit region, and the parity of its data bits.
    fn crc16_residual(b: &BitBuf) -> (u16, u32) {
        let mut residual: u16 = 0;
        let mut ones: u32 = 0;
        for i in 0..96 {
            if b.get(i) {
                residual ^= crc16_row(i);
                ones += u32::from(i < 80);
            }
        }
        (residual, ones)
    }

    /// Parse 24 hex octets into a 96-bit buffer, MSB-first.
    fn from_hex(h: &str) -> BitBuf {
        let bytes: Vec<u8> = (0..h.len() / 2)
            .map(|i| u8::from_str_radix(&h[i * 2..i * 2 + 2], 16).unwrap())
            .collect();
        assert_eq!(bytes.len(), 12);
        let mut b = BitBuf::new();
        for by in bytes {
            for k in (0..8).rev() {
                b.push((by >> k) & 1 != 0);
            }
        }
        b
    }

    /// Every constant broadcast the 2026-09-03 inventory measured on air obeys
    /// the parity rule: odd data weight → residual 0x0000, even → 0xFFFF.
    /// These three are the whole-window constants (Harris OSP 1, Harris OSP 15
    /// and SYS_SVC_BCAST), 1.7 M occurrences between them.
    #[test]
    fn on_air_constants_land_on_the_parity_required_residual() {
        for (hex, want_residual, want_odd) in [
            ("81a4110dfffffffffffff79d", 0xFFFFu16, false),
            ("8fa40a57000a00000000edda", 0x0000, true),
            ("b800040c2c680c2c6801aa30", 0xFFFF, false),
        ] {
            let mut b = from_hex(hex);
            let (residual, ones) = crc16_residual(&b);
            assert_eq!(residual, want_residual, "{hex}");
            assert_eq!(ones % 2 == 1, want_odd, "{hex}");
            assert_eq!(crc16_expected_residual(ones), want_residual, "{hex}");
            assert_eq!(
                crc16_check_and_correct(&mut b, 0),
                CrcResult::Passed,
                "{hex} must still pass"
            );
        }
    }

    /// A block whose residual is the complement of what its parity allows is
    /// rejected, and reported as such. Built by flipping the whole CRC field
    /// of a valid codeword, which moves the residual by exactly 0xFFFF and
    /// leaves the data parity alone.
    #[test]
    fn a_wrong_parity_residual_is_rejected() {
        for seed in 0..8u64 {
            let mut m = make_crc16_msg(seed);
            let (expected, _) = crc16_residual(&m);
            for k in 80..96 {
                m.flip(k);
            }
            let (residual, ones) = crc16_residual(&m);
            assert_eq!(residual, !expected);
            assert_eq!(residual, !crc16_expected_residual(ones));
            let before = m.clone();
            assert_eq!(
                crc16_check_and_correct(&mut m, 0),
                CrcResult::FailedWrongParity
            );
            assert!(!crc16_check_and_correct(&mut m, 0).is_valid());
            // A rejected block is left untouched.
            assert_eq!(m.get_u64(0, 64), before.get_u64(0, 64));
        }
        // The same, from a real on-air constant: complementing its CRC field
        // gives the residual its parity forbids.
        let mut b = from_hex("81a4110dfffffffffffff79d");
        for k in 80..96 {
            b.flip(k);
        }
        assert_eq!(
            crc16_check_and_correct(&mut b, 0),
            CrcResult::FailedWrongParity
        );
    }

    /// The old rule accepted residual 0 *or* all-ones unconditionally; the new
    /// one accepts exactly the value the block's data parity requires. Fix the
    /// 80 data bits and sweep all 65,536 CRC fields: the old rule called two of
    /// them valid, the new rule calls exactly one — the false-pass surface is
    /// halved, and the field the old rule accepted in error is now reported as
    /// `FailedWrongParity`.
    #[test]
    fn narrowed_acceptance_halves_the_false_pass_surface() {
        for seed in 0..4u64 {
            let base = make_crc16_msg(seed);
            let (_, ones) = crc16_residual(&base);
            let expected = crc16_expected_residual(ones);
            let mut old_pass = Vec::new();
            let mut new_pass = Vec::new();
            let mut wrong_parity = Vec::new();
            for field in 0..=0xFFFFu32 {
                let mut e = base.clone();
                for k in 0..16 {
                    e.set(80 + k, (field >> (15 - k)) & 1 != 0);
                }
                let (residual, _) = crc16_residual(&e);
                if residual == 0 || residual == 0xFFFF {
                    old_pass.push(field);
                }
                if residual == expected {
                    new_pass.push(field);
                }
                if crc16_check_and_correct(&mut e, 0) == CrcResult::FailedWrongParity {
                    wrong_parity.push(field);
                }
            }
            assert_eq!(old_pass.len(), 2, "seed {seed}");
            assert_eq!(new_pass.len(), 1, "seed {seed}");
            assert_eq!(wrong_parity.len(), 1, "seed {seed}");
            assert!(old_pass.contains(&new_pass[0]));
            assert!(old_pass.contains(&wrong_parity[0]));
            assert_ne!(new_pass[0], wrong_parity[0]);
            // The two candidates differ in every bit: one 16-bit complement.
            assert_eq!(new_pass[0] ^ wrong_parity[0], 0xFFFF);
        }
    }

    #[test]
    fn crc32_roundtrip_and_correction() {
        for blocks in 1..=3usize {
            let rows = 96 * blocks;
            let mut b = BitBuf::new();
            for _ in 0..160 + rows {
                b.push(false);
            }
            let mut s = 7u64;
            for i in 0..rows - 32 {
                s = s.wrapping_mul(6364136223846793005).wrapping_add(9);
                b.set(160 + i, (s >> 60) & 1 != 0);
            }
            let mut acc: u32 = 0;
            for i in 0..rows - 32 {
                if b.get(160 + i) {
                    acc ^= crc32_row(blocks, i);
                }
            }
            for k in 0..32 {
                b.set(160 + rows - 32 + k, (acc >> (31 - k)) & 1 != 0);
            }
            assert_eq!(
                crc32_pdu_check_and_correct(&mut b, blocks),
                CrcResult::Passed
            );
            let mut e = b.clone();
            e.flip(165);
            match crc32_pdu_check_and_correct(&mut e, blocks) {
                CrcResult::Corrected(p) => assert_eq!(p, 165),
                other => panic!("blocks {blocks}: {other:?}"),
            }
        }
    }
}
