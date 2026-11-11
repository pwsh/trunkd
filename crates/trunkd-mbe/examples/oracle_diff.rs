// SPDX-License-Identifier: GPL-3.0-or-later
//! Compares this decoder against a JMBE reference decode, sample by sample.
//!
//! Usage: `oracle_diff <frames.hex> <reference.f32>`, where `frames.hex` is one
//! 36-character IMBE frame per line and `reference.f32` is little-endian `f32`
//! samples, 160 per frame.
//!
//! Producing the reference needs a JMBE checkout and a five-line Java harness:
//! construct a single `jmbe.codec.imbe.IMBEAudioCodec`, call `getAudio(byte[])`
//! for each frame in order, and write each returned `float[160]` out as
//! little-endian binary. State carries across frames, so the frames must go
//! through one codec instance in capture order.
//!
//! The committed fixtures under `tests/data/` were produced exactly that way;
//! this example is the tool for re-checking the port against a new JMBE
//! release, or for localising a regression to a frame and sample.

use std::fs;

use trunkd_mbe::{ImbeDecoder, SAMPLES_PER_FRAME};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let frames_path = &args[1];
    let reference_path = &args[2];

    let hex = fs::read_to_string(frames_path).expect("frames");
    let raw = fs::read(reference_path).expect("reference");

    let mut decoder = ImbeDecoder::new();
    let mut worst = 0.0f32;
    let mut worst_frame = 0usize;
    let mut exact = 0usize;
    let mut total = 0usize;
    let mut first_bad: Option<(usize, usize, f32, f32)> = None;
    let frame_count = hex.lines().filter(|l| !l.trim().is_empty()).count();
    let mut per_frame = vec![(0usize, 0.0f32); frame_count];
    let mut muted: Vec<usize> = Vec::new();

    for (index, line) in hex.lines().filter(|l| !l.trim().is_empty()).enumerate() {
        let mut frame = [0u8; 18];
        for (byte, chunk) in frame.iter_mut().zip(line.trim().as_bytes().chunks(2)) {
            *byte = u8::from_str_radix(std::str::from_utf8(chunk).unwrap(), 16).unwrap();
        }
        let audio = decoder.decode_f32(&frame);
        if decoder.last_frame_muted() {
            muted.push(index);
        }

        for (sample, value) in audio.iter().enumerate() {
            let offset = (index * SAMPLES_PER_FRAME + sample) * 4;
            if offset + 4 > raw.len() {
                break;
            }
            let want = f32::from_le_bytes(raw[offset..offset + 4].try_into().unwrap());
            total += 1;
            if want.to_bits() == value.to_bits() || (want.is_nan() && value.is_nan()) {
                exact += 1;
            } else {
                if first_bad.is_none() {
                    first_bad = Some((index, sample, *value, want));
                }
                let diff = (want - value).abs();
                per_frame[index].0 += 1;
                if diff > per_frame[index].1 {
                    per_frame[index].1 = diff;
                }
                if diff > worst {
                    worst = diff;
                    worst_frame = index;
                }
            }
        }
    }

    println!("samples: {total}");
    println!(
        "bit-exact: {exact} ({:.4}%)",
        100.0 * exact as f64 / total as f64
    );
    println!("max abs f32 diff: {worst} (frame {worst_frame})");
    println!("max diff in i16 LSB: {}", worst * 32767.0);
    if let Some((frame, sample, got, want)) = first_bad {
        println!("first mismatch: frame {frame} sample {sample} got {got} want {want}");
    }
    println!("muted frames: {muted:?}");
    println!("per-frame worst (frames with any mismatch):");
    for (frame, (count, diff)) in per_frame.iter().enumerate() {
        if *count > 0 {
            println!("  frame {frame}: {count} mismatched, max diff {diff}");
        }
    }
}
