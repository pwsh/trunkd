// SPDX-License-Identifier: GPL-3.0-or-later
//! Audio encoding for upload: MP3 (default) and WAV.
//!
//! MP3 is the primary format because it is the only one every downstream
//! consumer accepts — Keyup (`mp3`/`wav`/`m4a`), Broadcastify Calls
//! (`mp3`/`m4a`), OpenMHz (`m4a`/`mp3`), the Whisper transcription appliance
//! (`mp3`/`wav`) — and at sdrtrunk's default 16 kbps CBR it is roughly an
//! eighth of the bandwidth of 8 kHz s16 WAV.
//!
//! Encoding happens on the uplink worker thread, never on a DSP thread.

use std::io::Cursor;

use mp3lame_encoder::{Bitrate, Builder, FlushNoGap, MonoPcm};

use crate::call::PCM_RATE;

/// How call audio is encoded for upload and for the offline spool.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioFormat {
    /// LAME MP3, constant bitrate, mono.
    ///
    /// `bitrate_kbps` is snapped to the nearest LAME-supported rate; 16 is
    /// sdrtrunk's default for voice and 32 is the common step up.
    Mp3 {
        /// Constant bitrate in kbit/s.
        bitrate_kbps: u32,
    },
    /// Uncompressed 8 kHz mono s16le RIFF/WAVE.
    Wav,
}

impl Default for AudioFormat {
    /// MP3 at 16 kbps — sdrtrunk's default for 8 kHz voice.
    fn default() -> Self {
        AudioFormat::Mp3 { bitrate_kbps: 16 }
    }
}

impl AudioFormat {
    /// The MIME type to send as `audioType` (and to store the call under).
    pub fn mime(self) -> &'static str {
        match self {
            AudioFormat::Mp3 { .. } => "audio/mpeg",
            AudioFormat::Wav => "audio/wav",
        }
    }

    /// The filename extension, without the dot.
    pub fn extension(self) -> &'static str {
        match self {
            AudioFormat::Mp3 { .. } => "mp3",
            AudioFormat::Wav => "wav",
        }
    }
}

/// Anything that can go wrong turning PCM into bytes.
#[derive(Debug, thiserror::Error)]
pub enum AudioError {
    /// The LAME encoder could not be built or configured.
    #[error("mp3 encoder setup failed: {0}")]
    Mp3Setup(String),

    /// LAME rejected a block of samples or the final flush.
    #[error("mp3 encode failed: {0}")]
    Mp3Encode(String),

    /// The WAV writer failed (only possible on an in-memory buffer overflow).
    #[error("wav encode failed: {0}")]
    Wav(String),
}

/// Encode 8 kHz mono PCM as a complete RIFF/WAVE file (s16le).
pub fn encode_wav(pcm: &[i16]) -> Vec<u8> {
    encode_wav_checked(pcm).expect("writing WAV to an in-memory buffer cannot fail")
}

fn encode_wav_checked(pcm: &[i16]) -> Result<Vec<u8>, AudioError> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: PCM_RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    // 44-byte header plus the samples; exact, so the Vec never reallocates.
    let mut cursor = Cursor::new(Vec::with_capacity(44 + pcm.len() * 2));
    {
        let mut writer =
            hound::WavWriter::new(&mut cursor, spec).map_err(|e| AudioError::Wav(e.to_string()))?;
        for &s in pcm {
            writer
                .write_sample(s)
                .map_err(|e| AudioError::Wav(e.to_string()))?;
        }
        writer
            .finalize()
            .map_err(|e| AudioError::Wav(e.to_string()))?;
    }
    Ok(cursor.into_inner())
}

/// The LAME-supported CBR rates, ascending.
const LAME_BITRATES: [(u32, Bitrate); 16] = [
    (8, Bitrate::Kbps8),
    (16, Bitrate::Kbps16),
    (24, Bitrate::Kbps24),
    (32, Bitrate::Kbps32),
    (40, Bitrate::Kbps40),
    (48, Bitrate::Kbps48),
    (64, Bitrate::Kbps64),
    (80, Bitrate::Kbps80),
    (96, Bitrate::Kbps96),
    (112, Bitrate::Kbps112),
    (128, Bitrate::Kbps128),
    (160, Bitrate::Kbps160),
    (192, Bitrate::Kbps192),
    (224, Bitrate::Kbps224),
    (256, Bitrate::Kbps256),
    (320, Bitrate::Kbps320),
];

/// Snap a requested bitrate to the nearest rate LAME accepts.
///
/// Rejecting an unsupported number would turn a config typo into a call that
/// never uploads; snapping keeps the recorder recording.
fn nearest_bitrate(kbps: u32) -> Bitrate {
    let mut best = LAME_BITRATES[1]; // 16 kbps, the default
    let mut best_delta = u32::MAX;
    for &(rate, bitrate) in &LAME_BITRATES {
        let delta = rate.abs_diff(kbps);
        if delta < best_delta {
            best_delta = delta;
            best = (rate, bitrate);
        }
    }
    best.1
}

/// The MP3 encoder linked into this build, as `(name, version)`.
///
/// LAME is statically linked through `mp3lame-sys`, so unlike the SDR
/// libraries there is nothing to `dlopen` and nothing that can be missing at
/// run time — but "which LAME is in this binary" is still a question a bug
/// report needs answered, and the C library is the one component whose version
/// the Rust dependency table does not state.
pub fn mp3_encoder_version() -> (&'static str, String) {
    (
        "libmp3lame",
        mp3lame_encoder::mp3lame_version().into_owned(),
    )
}

/// Encode 8 kHz mono PCM as a complete CBR MP3 (MPEG-2.5 Layer III at this
/// sample rate).
///
/// The final [`flush`](mp3lame_encoder::Encoder::flush) is what makes the last
/// frames real: without it LAME keeps up to a frame and a half of audio in its
/// internal buffer and short calls lose their tail.
pub fn encode_mp3(pcm: &[i16], bitrate_kbps: u32) -> Result<Vec<u8>, AudioError> {
    if pcm.is_empty() {
        // LAME complains to stderr when asked to flush an encoder it was never
        // fed, and an "MP3" of zero frames is not a file worth uploading.
        return Ok(Vec::new());
    }
    let mut encoder = Builder::new()
        .ok_or_else(|| AudioError::Mp3Setup("could not allocate a LAME encoder".to_owned()))?
        .with_num_channels(1)
        .map_err(|e| AudioError::Mp3Setup(format!("channels: {e}")))?
        .with_sample_rate(PCM_RATE)
        .map_err(|e| AudioError::Mp3Setup(format!("sample rate: {e}")))?
        .with_brate(nearest_bitrate(bitrate_kbps))
        .map_err(|e| AudioError::Mp3Setup(format!("bitrate: {e}")))?
        .with_mode(mp3lame_encoder::Mode::Mono)
        .map_err(|e| AudioError::Mp3Setup(format!("mode: {e}")))?
        .with_quality(mp3lame_encoder::Quality::Good)
        .map_err(|e| AudioError::Mp3Setup(format!("quality: {e}")))?
        .build()
        .map_err(|e| AudioError::Mp3Setup(e.to_string()))?;

    let mut out = Vec::with_capacity(mp3lame_encoder::max_required_buffer_size(pcm.len()));
    encoder
        .encode_to_vec(MonoPcm(pcm), &mut out)
        .map_err(|e| AudioError::Mp3Encode(e.to_string()))?;
    encoder
        .flush_to_vec::<FlushNoGap>(&mut out)
        .map_err(|e| AudioError::Mp3Encode(format!("flush: {e}")))?;
    Ok(out)
}

/// An MP3 encoder held open across many small blocks of PCM.
///
/// [`encode_mp3`] is the one-shot form: a whole call arrives, one file goes
/// out. The live monitor has the opposite shape — 20 ms of audio at a time,
/// forever, with no end to flush at — and building a fresh encoder per block
/// would emit a fresh LAME header per block and produce a stream no decoder
/// keeps in sync with.
///
/// So the encoder is kept. LAME's own framing is what paces the output: at
/// 8 kHz this is MPEG-2.5 Layer III, whose frames are 576 samples, so feeding
/// it 160-sample blocks yields nothing at all for two blocks out of three and
/// one frame on the third. Callers must therefore tolerate
/// [`encode`](Self::encode) appending zero bytes; they must *not* treat that as
/// end of stream.
///
/// There is deliberately no flush: a flush ends the stream (and pads out the
/// final frame), which is exactly what a monitor that runs until the listener
/// leaves must never do mid-stream. Dropping the encoder discards at most a
/// frame and a half of audio nobody is waiting on.
pub struct Mp3Stream {
    encoder: mp3lame_encoder::Encoder,
}

impl Mp3Stream {
    /// Open an encoder for 8 kHz mono at (the nearest LAME rate to)
    /// `bitrate_kbps`.
    pub fn new(bitrate_kbps: u32) -> Result<Self, AudioError> {
        let encoder = Builder::new()
            .ok_or_else(|| AudioError::Mp3Setup("could not allocate a LAME encoder".to_owned()))?
            .with_num_channels(1)
            .map_err(|e| AudioError::Mp3Setup(format!("channels: {e}")))?
            .with_sample_rate(PCM_RATE)
            .map_err(|e| AudioError::Mp3Setup(format!("sample rate: {e}")))?
            .with_brate(nearest_bitrate(bitrate_kbps))
            .map_err(|e| AudioError::Mp3Setup(format!("bitrate: {e}")))?
            .with_mode(mp3lame_encoder::Mode::Mono)
            .map_err(|e| AudioError::Mp3Setup(format!("mode: {e}")))?
            // `Good` is what the call encoder uses; a monitor stream is the one
            // place where spending less CPU per frame matters more than the
            // last decibel, but keeping the two identical means the audio an
            // operator hears live is the audio that gets uploaded.
            .with_quality(mp3lame_encoder::Quality::Good)
            .map_err(|e| AudioError::Mp3Setup(format!("quality: {e}")))?
            .build()
            .map_err(|e| AudioError::Mp3Setup(e.to_string()))?;
        Ok(Mp3Stream { encoder })
    }

    /// Append however many whole MP3 frames `pcm` completed. Often zero.
    ///
    /// The reserve is not an optimisation: `encode_to_vec` writes into the
    /// vector's *spare capacity* and trusts it to be large enough, so handing
    /// it a full (or freshly cleared) `Vec` is a buffer overrun rather than a
    /// reallocation. `max_required_buffer_size` is LAME's own worst case,
    /// which includes the whole frame it may have been holding back.
    pub fn encode(&mut self, pcm: &[i16], out: &mut Vec<u8>) -> Result<(), AudioError> {
        out.reserve(mp3lame_encoder::max_required_buffer_size(pcm.len()));
        self.encoder
            .encode_to_vec(MonoPcm(pcm), out)
            .map(|_| ())
            .map_err(|e| AudioError::Mp3Encode(e.to_string()))
    }
}

/// Encode PCM in `format`, returning the bytes, the MIME type to declare and
/// the filename extension to store it under.
pub fn encode_audio(
    pcm: &[i16],
    format: AudioFormat,
) -> Result<(Vec<u8>, &'static str, &'static str), AudioError> {
    let bytes = match format {
        AudioFormat::Mp3 { bitrate_kbps } => encode_mp3(pcm, bitrate_kbps)?,
        AudioFormat::Wav => encode_wav_checked(pcm)?,
    };
    Ok((bytes, format.mime(), format.extension()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One second of a 1 kHz tone at 8 kHz.
    fn tone(seconds: f64) -> Vec<i16> {
        let n = (seconds * f64::from(PCM_RATE)) as usize;
        (0..n)
            .map(|i| {
                let t = i as f64 / f64::from(PCM_RATE);
                (8000.0 * (2.0 * std::f64::consts::PI * 1000.0 * t).sin()) as i16
            })
            .collect()
    }

    #[test]
    fn wav_has_a_riff_header_and_every_sample() {
        let pcm = tone(0.5);
        let wav = encode_wav(&pcm);
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(wav.len(), 44 + pcm.len() * 2);
        // Sample rate is little-endian at byte 24 of a canonical header.
        assert_eq!(u32::from_le_bytes(wav[24..28].try_into().unwrap()), 8000);
        assert_eq!(u16::from_le_bytes(wav[22..24].try_into().unwrap()), 1);
    }

    #[test]
    fn wav_round_trips_through_hound() {
        let pcm = tone(0.25);
        let wav = encode_wav(&pcm);
        let mut reader = hound::WavReader::new(Cursor::new(wav)).expect("readable wav");
        assert_eq!(reader.spec().sample_rate, 8000);
        assert_eq!(reader.spec().channels, 1);
        let back: Vec<i16> = reader.samples::<i16>().map(|s| s.unwrap()).collect();
        assert_eq!(back, pcm);
    }

    #[test]
    fn mp3_starts_with_an_mpeg_frame_sync() {
        let mp3 = encode_mp3(&tone(1.0), 16).expect("encodes");
        assert!(mp3.len() > 100, "suspiciously small mp3: {}", mp3.len());
        // No ID3 tag is requested, so the first bytes are a frame header:
        // 11 sync bits, then the version/layer bits. 8 kHz is MPEG-2.5
        // Layer III, i.e. 0xFF 0xE3 with no CRC.
        assert_eq!(mp3[0], 0xFF, "no frame sync: {:02X?}", &mp3[..8]);
        assert_eq!(
            mp3[1] & 0xE0,
            0xE0,
            "no frame sync: {:02X?}",
            &mp3[..8.min(mp3.len())]
        );
        // Layer III (bits 2-1 of byte 1 == 01).
        assert_eq!(mp3[1] & 0x06, 0x02, "not layer III");
    }

    #[test]
    fn mp3_is_far_smaller_than_wav() {
        let pcm = tone(5.0);
        let mp3 = encode_mp3(&pcm, 16).expect("encodes");
        let wav = encode_wav(&pcm);
        assert!(
            mp3.len() * 4 < wav.len(),
            "mp3 {} vs wav {}",
            mp3.len(),
            wav.len()
        );
        // 16 kbps for 5 s is ~10 kB; allow generous slack for the flush.
        assert!(mp3.len() < 20_000, "mp3 too large: {}", mp3.len());
    }

    #[test]
    fn empty_pcm_still_encodes() {
        assert_eq!(encode_mp3(&[], 16).expect("encodes"), Vec::<u8>::new());
        assert_eq!(encode_wav(&[]).len(), 44);
    }

    #[test]
    fn encode_audio_reports_mime_and_extension() {
        let (bytes, mime, ext) = encode_audio(&tone(0.2), AudioFormat::default()).expect("mp3");
        assert_eq!((mime, ext), ("audio/mpeg", "mp3"));
        assert!(!bytes.is_empty());
        let (bytes, mime, ext) = encode_audio(&tone(0.2), AudioFormat::Wav).expect("wav");
        assert_eq!((mime, ext), ("audio/wav", "wav"));
        assert_eq!(&bytes[0..4], b"RIFF");
    }

    #[test]
    fn streaming_encoder_keeps_one_header_and_paces_by_frame() {
        let mut stream = Mp3Stream::new(32).expect("an encoder");
        let mut out = Vec::new();
        let pcm = tone(1.0);
        let mut empties = 0;
        for block in pcm.chunks(160) {
            let before = out.len();
            stream.encode(block, &mut out).expect("encodes");
            if out.len() == before {
                empties += 1;
            }
        }
        // A 576-sample frame per 160-sample block means most blocks emit
        // nothing; the caller must not read that as end of stream.
        assert!(
            empties > 0,
            "every block emitted bytes; frame pacing changed"
        );
        assert!(out.len() > 100, "suspiciously small stream: {}", out.len());
        assert_eq!(out[0], 0xFF, "no frame sync: {:02X?}", &out[..8]);
        assert_eq!(out[1] & 0xE0, 0xE0, "no frame sync");
        // One second at 32 kbps is ~4 kB. Generous bounds: the point is that
        // this is a *stream*, not a header per block.
        assert!(out.len() < 9_000, "stream too large: {}", out.len());
    }

    #[test]
    fn odd_bitrates_snap_to_a_supported_rate() {
        // 20 kbps is not a LAME rate; encoding must still succeed.
        assert!(encode_mp3(&tone(0.2), 20).is_ok());
        assert!(encode_mp3(&tone(0.2), 0).is_ok());
        assert!(encode_mp3(&tone(0.2), 100_000).is_ok());
    }

    /// A real decoder must agree about the duration. Skipped where ffprobe is
    /// not installed.
    #[test]
    fn ffprobe_agrees_about_the_duration() {
        let Ok(probe) = which_ffprobe() else {
            eprintln!("ffprobe not found; skipping");
            return;
        };
        let dir = std::env::temp_dir().join(format!("trunkd-call-mp3-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("tone.mp3");
        std::fs::write(&path, encode_mp3(&tone(3.0), 16).expect("encodes")).expect("write");

        let out = std::process::Command::new(probe)
            .args([
                "-v",
                "error",
                "-show_entries",
                "format=duration",
                "-of",
                "csv=p=0",
            ])
            .arg(&path)
            .output()
            .expect("run ffprobe");
        let text = String::from_utf8_lossy(&out.stdout);
        let seconds: f64 = text.trim().parse().unwrap_or_else(|_| {
            panic!(
                "ffprobe said {text:?} / {}",
                String::from_utf8_lossy(&out.stderr)
            )
        });
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            (seconds - 3.0).abs() < 0.2,
            "ffprobe reported {seconds} s for a 3 s tone"
        );
    }

    fn which_ffprobe() -> Result<std::path::PathBuf, ()> {
        for dir in ["/usr/bin", "/usr/local/bin", "/opt/homebrew/bin"] {
            let p = std::path::Path::new(dir).join("ffprobe");
            if p.is_file() {
                return Ok(p);
            }
        }
        Err(())
    }
}
