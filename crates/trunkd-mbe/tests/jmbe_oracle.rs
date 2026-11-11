// SPDX-License-Identifier: GPL-3.0-or-later
//! Decodes captured runs of real off-air IMBE frames and compares the PCM
//! against JMBE's own output for the same frames.
//!
//! Both frame sets were sliced out of a P25 Phase 1 wideband IQ capture by
//! `trunkd-p25`'s `p25_decode` example, at the raw 144-bit vocoder boundary:
//!
//! * `imbe_frames_clear.hex` — 270 frames (5.4 s) of a clear, ALGID 0x80
//!   call. This is ordinary dispatch speech; a Whisper transcription of the
//!   decode is recognisable English.
//! * `imbe_frames_encrypted.hex` — 206 frames from the middle of an AES-256
//!   call. These are perfectly well-formed IMBE frames carrying
//!   ciphertext, so they drive the decoder through the paths a clean call never
//!   reaches: uncorrectable coset words, invalid fundamental frequencies, frame
//!   repeats, adaptive smoothing and muting.
//!
//! The `jmbe_pcm_*.i16` files are what JMBE 1.0.9 produces for exactly those
//! frames, fed sequentially through one `IMBEAudioCodec` instance and converted
//! with `(short)(sample * Short.MAX_VALUE)`.
//!
//! Muted frames are the one thing that cannot match: JMBE fills them from an
//! unseeded `java.util.Random`, so their content is not reproducible even
//! between two Java runs. Those frames are still decoded — they advance decoder
//! state — but their samples are excluded from the comparison.
//!
//! The encrypted pair is always present. The clear pair is real speech and is
//! optional (`data/README.md`): `build.rs` looks in `tests/data/`, then in
//! `$TRUNKD_MBE_VECTORS`, and where it finds neither it sets
//! `trunkd_mbe_clear_missing` so the tests that need the clear call are
//! reported **ignored**, naming what they need — never a silent pass.

use trunkd_mbe::{ImbeDecoder, SAMPLES_PER_FRAME};

#[cfg(not(trunkd_mbe_clear_missing))]
const CLEAR_FRAMES: &str = include_str!(concat!(
    env!("TRUNKD_MBE_CLEAR_DIR"),
    "/imbe_frames_clear.hex"
));
#[cfg(not(trunkd_mbe_clear_missing))]
const CLEAR_PCM: &[u8] =
    include_bytes!(concat!(env!("TRUNKD_MBE_CLEAR_DIR"), "/jmbe_pcm_clear.i16"));
// Never read: every test that touches these is ignored in this configuration.
#[cfg(trunkd_mbe_clear_missing)]
const CLEAR_FRAMES: &str = "";
#[cfg(trunkd_mbe_clear_missing)]
const CLEAR_PCM: &[u8] = &[];
const ENCRYPTED_FRAMES: &str = include_str!("data/imbe_frames_encrypted.hex");
const ENCRYPTED_PCM: &[u8] = include_bytes!("data/jmbe_pcm_encrypted.i16");

fn frames(hex: &str) -> Vec<[u8; 18]> {
    hex.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|line| {
            assert_eq!(line.len(), 36, "expected 36 hex characters: {line}");
            let mut frame = [0u8; 18];
            for (byte, chunk) in frame.iter_mut().zip(line.as_bytes().chunks(2)) {
                *byte = u8::from_str_radix(std::str::from_utf8(chunk).unwrap(), 16)
                    .expect("hex digits");
            }
            frame
        })
        .collect()
}

fn reference(pcm: &[u8], index: usize) -> i16 {
    i16::from_le_bytes([pcm[index * 2], pcm[index * 2 + 1]])
}

/// Decodes `hex` and compares against `pcm`, returning
/// `(compared, exact, muted_frames, max_difference)`.
fn compare(label: &str, hex: &str, pcm: &[u8]) -> (usize, usize, usize, i32) {
    let frames = frames(hex);
    assert_eq!(
        pcm.len(),
        frames.len() * SAMPLES_PER_FRAME * 2,
        "{label}: fixture length mismatch"
    );

    let mut decoder = ImbeDecoder::new();
    let (mut compared, mut exact, mut muted, mut max_difference) = (0usize, 0usize, 0usize, 0i32);

    for (index, frame) in frames.iter().enumerate() {
        let audio = decoder.decode(frame);

        if decoder.last_frame_muted() {
            muted += 1;
            continue;
        }

        for (sample, &value) in audio.iter().enumerate() {
            let want = reference(pcm, index * SAMPLES_PER_FRAME + sample);
            compared += 1;
            if value == want {
                exact += 1;
            } else {
                let difference = (i32::from(value) - i32::from(want)).abs();
                max_difference = max_difference.max(difference);
                assert!(
                    difference <= 2,
                    "{label}: frame {index} sample {sample}: got {value}, JMBE says {want}"
                );
            }
        }
    }

    println!(
        "{label}: {} frames ({muted} muted and skipped), {compared} samples compared, \
         {exact} bit-exact ({:.4}%), max difference {max_difference} LSB",
        frames.len(),
        100.0 * exact as f64 / compared as f64
    );

    (compared, exact, muted, max_difference)
}

#[test]
#[cfg_attr(
    trunkd_mbe_clear_missing,
    ignore = "needs the clear-call vectors imbe_frames_clear.hex + jmbe_pcm_clear.i16: \
              put them in crates/trunkd-mbe/tests/data/ or set TRUNKD_MBE_VECTORS, \
              then rebuild — see tests/data/README.md"
)]
fn clear_call_matches_jmbe_sample_for_sample() {
    let (compared, exact, muted, max_difference) = compare("clear", CLEAR_FRAMES, CLEAR_PCM);
    assert_eq!(muted, 0, "a clean call should never mute");
    assert_eq!(max_difference, 0);
    assert_eq!(exact, compared);
}

#[test]
fn encrypted_call_matches_jmbe_sample_for_sample() {
    let (compared, exact, muted, max_difference) =
        compare("encrypted", ENCRYPTED_FRAMES, ENCRYPTED_PCM);
    assert_eq!(
        muted, 1,
        "this run should exercise the muting path exactly once"
    );
    assert_eq!(max_difference, 0);
    assert_eq!(exact, compared);
}

/// The clear call should decode to something with real speech energy, and the
/// encrypted one to noise; a decoder that silently produced zeros would still
/// pass a bit-exact comparison against a broken reference, so check the audio
/// is actually there.
fn assert_carries_signal(label: &str, hex: &str) {
    let mut decoder = ImbeDecoder::new();
    let mut peak = 0i32;
    let mut energy = 0f64;
    let mut samples = 0usize;
    for frame in frames(hex) {
        for value in decoder.decode(&frame) {
            peak = peak.max(i32::from(value).abs());
            energy += f64::from(value) * f64::from(value);
            samples += 1;
        }
    }
    let rms = (energy / samples as f64).sqrt();
    println!("{label}: peak {peak}, rms {rms:.1}");
    assert!(peak > 3000, "{label}: peak {peak} is implausibly quiet");
    assert!(rms > 100.0, "{label}: rms {rms} is implausibly quiet");
}

#[test]
#[cfg_attr(
    trunkd_mbe_clear_missing,
    ignore = "needs the clear-call vectors imbe_frames_clear.hex + jmbe_pcm_clear.i16: \
              put them in crates/trunkd-mbe/tests/data/ or set TRUNKD_MBE_VECTORS, \
              then rebuild — see tests/data/README.md"
)]
fn clear_audio_carries_signal() {
    assert_carries_signal("clear", CLEAR_FRAMES);
}

#[test]
fn encrypted_audio_carries_signal() {
    assert_carries_signal("encrypted", ENCRYPTED_FRAMES);
}

/// Both captures are real off-air audio, so both mix clean frames with frames
/// that needed EDAC correction; the reported counts must reflect that.
fn assert_error_counts(label: &str, hex: &str) {
    let mut decoder = ImbeDecoder::new();
    let mut errors = 0u64;
    let mut clean = 0usize;
    let frames = frames(hex);
    for frame in &frames {
        decoder.decode(frame);
        errors += u64::from(decoder.last_frame_errors());
        if decoder.last_frame_errors() == 0 {
            clean += 1;
        }
    }
    println!("{label}: {clean}/{} clean, {errors} errors", frames.len());

    // Most frames decode without any correction at all, but the run is not
    // clean throughout.
    assert!(clean * 2 > frames.len(), "{label}: too few clean frames");
    assert!(
        clean < frames.len() && errors > 0,
        "{label}: no EDAC corrections reported"
    );
}

#[test]
#[cfg_attr(
    trunkd_mbe_clear_missing,
    ignore = "needs the clear-call vectors imbe_frames_clear.hex + jmbe_pcm_clear.i16: \
              put them in crates/trunkd-mbe/tests/data/ or set TRUNKD_MBE_VECTORS, \
              then rebuild — see tests/data/README.md"
)]
fn clear_call_reports_error_counts() {
    assert_error_counts("clear", CLEAR_FRAMES);
}

#[test]
fn encrypted_call_reports_error_counts() {
    assert_error_counts("encrypted", ENCRYPTED_FRAMES);
}

/// Decoding the same frames twice from fresh decoders must give identical
/// audio: nothing in the steady-state path may depend on ambient state.
#[test]
fn decoding_is_reproducible() {
    let mut first = ImbeDecoder::new();
    let mut second = ImbeDecoder::default();

    for frame in frames(ENCRYPTED_FRAMES) {
        // The muted frame's filler is deterministic in this port (JMBE seeds it
        // from the clock), so even that one matches across decoders.
        assert_eq!(first.decode(&frame), second.decode(&frame));
    }
}

/// `reset()` clears the previous frame's model parameters but deliberately
/// leaves the noise sequence and harmonic phase running, matching
/// `IMBESynthesizer.reset()`. Pin that: a reset decoder is close to, but not
/// identical to, a fresh one.
#[test]
#[cfg_attr(
    trunkd_mbe_clear_missing,
    ignore = "needs the clear-call vectors imbe_frames_clear.hex + jmbe_pcm_clear.i16: \
              put them in crates/trunkd-mbe/tests/data/ or set TRUNKD_MBE_VECTORS, \
              then rebuild — see tests/data/README.md"
)]
fn reset_clears_model_parameters_but_not_the_noise_sequence() {
    let clear = frames(CLEAR_FRAMES);

    let mut fresh = ImbeDecoder::new();
    let baseline: Vec<_> = clear.iter().map(|f| fresh.decode(f)).collect();

    let mut reused = ImbeDecoder::new();
    for frame in frames(ENCRYPTED_FRAMES) {
        reused.decode(&frame);
    }
    reused.reset();
    assert_eq!(reused.last_frame_errors(), 0);
    assert!(!reused.last_frame_muted());

    let after_reset: Vec<_> = clear.iter().map(|f| reused.decode(f)).collect();

    assert_ne!(baseline, after_reset);

    // Only the unvoiced excitation differs, so the two decodes must still be
    // the same call at roughly the same level.
    let peak = |pcm: &[[i16; SAMPLES_PER_FRAME]]| {
        pcm.iter()
            .flatten()
            .map(|s| i32::from(*s).abs())
            .max()
            .unwrap()
    };
    let (a, b) = (peak(&baseline), peak(&after_reset));
    assert!(
        (a - b).abs() * 4 < a.max(b),
        "reset changed the decode far more than the noise sequence should: {a} vs {b}"
    );
}
