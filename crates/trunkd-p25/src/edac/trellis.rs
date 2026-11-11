// SPDX-License-Identifier: GPL-3.0-or-later
//! 1/2-rate and 3/4-rate trellis (Viterbi) decoders (spec §8.5).
//!
//! Both are constraint-length-2 trellises: the expected 4-bit output of a
//! transition depends only on (previous input, current input) via the
//! transition matrix. A 196-bit deinterleaved block is split into 49 four-bit
//! symbols (MSB-first); the decoder starts in state 0, feeds 48 symbols with
//! add-compare-select, and flushes with the 49th symbol into a forced
//! input-0 node.
//!
//! **Tie-break is strict `<`:** an equal-metric challenger does NOT replace
//! the incumbent survivor (candidates evaluated in ascending previous-state
//! order). Preserved for bit-exactness.

use crate::bits::BitBuf;

/// Hamming distance between two 4-bit symbols, via the spec's table.
const HAMMING4: [u8; 16] = [0, 1, 1, 2, 1, 2, 2, 3, 1, 2, 2, 3, 2, 3, 3, 4];

/// 1/2-rate transition matrix: `[prev][cur]` → expected 4-bit symbol.
pub const TRANSITION_1_2: [[u8; 4]; 4] =
    [[2, 12, 1, 15], [14, 0, 13, 3], [9, 7, 10, 4], [5, 11, 6, 8]];

/// 3/4-rate transition matrix: `[prev][cur]` → expected 4-bit symbol.
pub const TRANSITION_3_4: [[u8; 8]; 8] = [
    [2, 13, 14, 1, 7, 8, 11, 4],
    [14, 1, 7, 8, 11, 4, 2, 13],
    [10, 5, 6, 9, 15, 0, 3, 12],
    [6, 9, 15, 0, 3, 12, 10, 5],
    [15, 0, 3, 12, 10, 5, 6, 9],
    [3, 12, 10, 5, 6, 9, 15, 0],
    [7, 8, 11, 4, 2, 13, 14, 1],
    [11, 4, 2, 13, 14, 1, 7, 8],
];

const SYMBOLS: usize = 49;
const PAYLOAD: usize = 48;

/// Result of a trellis decode.
#[derive(Debug, Clone, Copy)]
pub struct TrellisResult {
    /// Total Hamming error of the surviving path — the corrected-bit
    /// estimate for the block.
    pub path_error: u32,
}

/// Generic constraint-length-2 Viterbi over `S` states (4 or 2^3 = 8).
///
/// `input` holds the 196 deinterleaved bits at `[start, start+196)`;
/// `output` receives 48 node input values, each `BITS_PER_NODE` bits,
/// MSB-first, appended via `push`.
fn decode<const S: usize>(
    matrix: &[[u8; S]; S],
    input: &BitBuf,
    start: usize,
    output: &mut BitBuf,
    bits_per_node: usize,
) -> TrellisResult {
    // Split into 49 4-bit symbols, MSB-first.
    let mut symbols = [0u8; SYMBOLS];
    for (k, sym) in symbols.iter_mut().enumerate() {
        *sym = input.get_u32(start + 4 * k, 4) as u8;
    }

    const INF: u32 = u32::MAX / 2;
    // metric[s] = best path error ending in state s; paths hold node inputs.
    let mut metric = [INF; S];
    let mut paths = [[0u8; PAYLOAD]; S];
    metric[0] = 0; // start in state 0

    let mut new_paths = [[0u8; PAYLOAD]; S];

    for (k, &sym) in symbols.iter().enumerate().take(PAYLOAD) {
        let mut new_metric = [INF; S];
        for cur in 0..S {
            let mut best_prev = usize::MAX;
            let mut best = INF;
            for prev in 0..S {
                if metric[prev] >= INF {
                    continue;
                }
                let bm = u32::from(HAMMING4[usize::from(matrix[prev][cur] ^ sym)]);
                let cand = metric[prev] + bm;
                // Strict `<`: an equal-metric challenger does not replace the
                // incumbent (earlier prev wins ties).
                if cand < best {
                    best = cand;
                    best_prev = prev;
                }
            }
            new_metric[cur] = best;
            if best_prev != usize::MAX {
                new_paths[cur] = paths[best_prev];
                new_paths[cur][k] = cur as u8;
            }
        }
        metric = new_metric;
        paths = new_paths;
    }

    // Flush with the 49th symbol into a forced input-0 node.
    let sym = symbols[SYMBOLS - 1];
    let mut best_prev = 0usize;
    let mut best = INF;
    for prev in 0..S {
        if metric[prev] >= INF {
            continue;
        }
        let bm = u32::from(HAMMING4[usize::from(matrix[prev][0] ^ sym)]);
        let cand = metric[prev] + bm;
        if cand < best {
            best = cand;
            best_prev = prev;
        }
    }

    for &node in paths[best_prev].iter() {
        for b in (0..bits_per_node).rev() {
            output.push((node >> b) & 1 != 0);
        }
    }
    TrellisResult { path_error: best }
}

/// Decode a 1/2-rate trellis block: 196 input bits → 96 output bits.
pub fn decode_1_2(input: &BitBuf, start: usize, output: &mut BitBuf) -> TrellisResult {
    decode::<4>(&TRANSITION_1_2, input, start, output, 2)
}

/// Decode a 3/4-rate trellis block: 196 input bits → 144 output bits.
pub fn decode_3_4(input: &BitBuf, start: usize, output: &mut BitBuf) -> TrellisResult {
    decode::<8>(&TRANSITION_3_4, input, start, output, 3)
}

/// Encode a 96-bit payload with the 1/2-rate trellis into 196 bits.
pub fn encode_block_1_2(data: &BitBuf) -> BitBuf {
    let mut out = BitBuf::new();
    let mut prev = 0usize;
    for k in 0..PAYLOAD {
        let cur = data.get_u32(2 * k, 2) as usize;
        let sym = TRANSITION_1_2[prev][cur];
        for b in (0..4).rev() {
            out.push((sym >> b) & 1 != 0);
        }
        prev = cur;
    }
    // Flush symbol: transition prev → 0.
    let sym = TRANSITION_1_2[prev][0];
    for b in (0..4).rev() {
        out.push((sym >> b) & 1 != 0);
    }
    out
}

/// Encode a 144-bit payload with the 3/4-rate trellis into 196 bits.
pub fn encode_block_3_4(data: &BitBuf) -> BitBuf {
    let mut out = BitBuf::new();
    let mut prev = 0usize;
    for k in 0..PAYLOAD {
        let cur = data.get_u32(3 * k, 3) as usize;
        let sym = TRANSITION_3_4[prev][cur];
        for b in (0..4).rev() {
            out.push((sym >> b) & 1 != 0);
        }
        prev = cur;
    }
    let sym = TRANSITION_3_4[prev][0];
    for b in (0..4).rev() {
        out.push((sym >> b) & 1 != 0);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pattern_buf(len: usize, seed: u64) -> BitBuf {
        let mut b = BitBuf::new();
        let mut s = seed;
        for _ in 0..len {
            s = s
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            b.push((s >> 63) & 1 != 0);
        }
        b
    }

    #[test]
    fn half_rate_roundtrip_clean() {
        let data = pattern_buf(96, 3);
        let coded = encode_block_1_2(&data);
        assert_eq!(coded.len(), 196);
        let mut out = BitBuf::new();
        let r = decode_1_2(&coded, 0, &mut out);
        assert_eq!(r.path_error, 0);
        assert_eq!(out.len(), 96);
        for i in 0..96 {
            assert_eq!(out.get(i), data.get(i), "bit {i}");
        }
    }

    #[test]
    fn half_rate_corrects_errors() {
        let data = pattern_buf(96, 9);
        let coded = encode_block_1_2(&data);
        // Scatter single-bit errors in well-separated symbols.
        for positions in [[8usize, 60, 120], [3, 90, 190], [40, 100, 160]] {
            let mut e = coded.clone();
            for &p in &positions {
                e.flip(p);
            }
            let mut out = BitBuf::new();
            let r = decode_1_2(&e, 0, &mut out);
            assert!(r.path_error >= 3);
            for i in 0..96 {
                assert_eq!(out.get(i), data.get(i), "bit {i} pos {positions:?}");
            }
        }
    }

    #[test]
    fn three_quarter_rate_roundtrip() {
        let data = pattern_buf(144, 5);
        let coded = encode_block_3_4(&data);
        assert_eq!(coded.len(), 196);
        let mut out = BitBuf::new();
        let r = decode_3_4(&coded, 0, &mut out);
        assert_eq!(r.path_error, 0);
        assert_eq!(out.len(), 144);
        for i in 0..144 {
            assert_eq!(out.get(i), data.get(i), "bit {i}");
        }
        // With scattered errors.
        let mut e = coded.clone();
        for &p in &[10usize, 88, 150] {
            e.flip(p);
        }
        let mut out2 = BitBuf::new();
        decode_3_4(&e, 0, &mut out2);
        for i in 0..144 {
            assert_eq!(out2.get(i), data.get(i), "bit {i}");
        }
    }
}
