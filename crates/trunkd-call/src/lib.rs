// SPDX-License-Identifier: GPL-3.0-or-later
//! `trunkd-call` — call assembly (ARCHITECTURE §3, §4).
//!
//! A *call* is everything trunkd knows about one talkgroup transmission group:
//! the decoded 8 kHz PCM, the radios that keyed up and when, the frequencies it
//! rode on, and the flags (emergency, encrypted) the control channel reported.
//!
//! The crate has three jobs:
//!
//! 1. [`CallBuilder`] accumulates an in-progress call on the DSP thread — PCM
//!    push by push, one [`SourceSegment`] per talker change, one
//!    [`FrequencySegment`] per frequency the call rode on — and hands back an
//!    immutable [`FinishedCall`] at teardown.
//! 2. [`encode_audio`] turns the PCM into the bytes that get uploaded. MP3 is
//!    the default because it is the one format every downstream consumer takes
//!    (Keyup, Broadcastify Calls, OpenMHz, the Whisper appliance) and it is a
//!    fraction of WAV's bandwidth; [`AudioFormat::Wav`] stays available for
//!    local archival and debugging.
//! 3. [`sidecar_json`] renders the call as a **trunk-recorder-style JSON
//!    sidecar**, which is both what the offline spool writes next to the audio
//!    and what Keyup's file ingest already understands (its
//!    `app.ingest.tr_parser`), so a spooled call needs no bespoke reader.
//!
//! ## Encrypted calls
//!
//! An encrypted call is recorded as **metadata only**: [`CallBuilder::finish`]
//! discards any PCM when the encrypted flag is set, so [`FinishedCall::pcm`] is
//! empty and no audio is encoded or uploaded. The call still carries its
//! timestamps, talkgroup, sources and (where the control channel said so) the
//! algorithm and key id — enough to prove an agency was on that talkgroup at
//! that second.
//!
//! ## No allocation surprises
//!
//! `CallBuilder` grows one `Vec<i16>` as audio arrives and caps it at
//! [`CallBuilder::with_max_duration`] (default
//! [`CallBuilder::DEFAULT_MAX_DURATION`]); a stuck call can never grow without
//! bound, in keeping with ARCHITECTURE §2.

#![deny(missing_docs)]

mod audio;
mod call;
mod sidecar;

pub use audio::{
    encode_audio, encode_mp3, encode_wav, mp3_encoder_version, AudioError, AudioFormat, Mp3Stream,
};
pub use call::{
    speech_seconds, BroadcastifyOutcome, CallBuilder, CallKind, FinishedCall, FrequencySegment,
    SourceQuality, SourceSegment, PCM_RATE, SPEECH_FLOOR_DBFS,
};
pub use sidecar::sidecar_json;
