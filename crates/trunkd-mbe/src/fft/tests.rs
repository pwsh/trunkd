// SPDX-License-Identifier: GPL-3.0-or-later
//! Bit-exactness checks for the FFT port against JTransforms' own output.

use super::*;

/// Sections of `tests/data/jtransforms_fft_probe.txt`, captured by running
/// JTransforms 3.2's `FloatFFT_1D(256)` — the version JMBE depends on — over a
/// deterministic input, including a bin-scaling step that mirrors what
/// `MBESynthesizer.getUnvoiced()` does between the forward and inverse
/// transforms. Values are raw IEEE-754 bit patterns so the comparison is exact.
fn probe() -> Vec<(String, Vec<f32>)> {
    let text = include_str!("../../tests/data/jtransforms_fft_probe.txt");
    let mut sections: Vec<(String, Vec<f32>)> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match u32::from_str_radix(line, 16) {
            Ok(bits) => sections
                .last_mut()
                .expect("value before section header")
                .1
                .push(f32::from_bits(bits)),
            Err(_) => sections.push((line.to_string(), Vec::new())),
        }
    }
    sections
}

fn section(name: &str) -> [f32; N] {
    let sections = probe();
    let values = &sections
        .iter()
        .find(|(header, _)| header == name)
        .unwrap_or_else(|| panic!("no section {name}"))
        .1;
    let mut out = [0.0f32; N];
    out.copy_from_slice(values);
    out
}

#[test]
fn real_forward_matches_jtransforms_bit_for_bit() {
    let mut a = section("INPUT");
    real_forward(&mut a);
    let expected = section("FORWARD");
    for (index, (got, want)) in a.iter().zip(expected.iter()).enumerate() {
        assert_eq!(
            got.to_bits(),
            want.to_bits(),
            "bin {index}: {got} != {want}"
        );
    }
}

#[test]
fn real_inverse_matches_jtransforms_bit_for_bit() {
    let mut a = section("SCALED");
    real_inverse_scaled(&mut a);
    let expected = section("INVERSE");
    for (index, (got, want)) in a.iter().zip(expected.iter()).enumerate() {
        assert_eq!(
            got.to_bits(),
            want.to_bits(),
            "sample {index}: {got} != {want}"
        );
    }
}
