// SPDX-License-Identifier: GPL-3.0-or-later
//! Decodes hex-encoded IMBE frames (one per line on stdin) to raw
//! little-endian 16-bit PCM on stdout, 8 kHz mono.
//!
//! Handy for auditioning a decode: pipe the `frame` fields of
//! `trunkd-p25`'s `p25_decode` example through it and wrap the result in a WAV
//! header.

use std::io::{Read, Write};

use trunkd_mbe::ImbeDecoder;

fn main() {
    let mut input = String::new();
    std::io::stdin()
        .read_to_string(&mut input)
        .expect("read stdin");

    let mut decoder = ImbeDecoder::new();
    let mut out = Vec::new();

    for line in input.lines() {
        let line = line.trim();
        if line.len() != 36 {
            continue;
        }
        let mut frame = [0u8; 18];
        for (byte, chunk) in frame.iter_mut().zip(line.as_bytes().chunks(2)) {
            *byte =
                u8::from_str_radix(std::str::from_utf8(chunk).unwrap(), 16).expect("hex digits");
        }
        for sample in decoder.decode(&frame) {
            out.extend_from_slice(&sample.to_le_bytes());
        }
    }

    std::io::stdout().write_all(&out).expect("write stdout");
}
