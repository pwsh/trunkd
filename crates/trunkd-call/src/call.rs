// SPDX-License-Identifier: GPL-3.0-or-later
//! The in-progress call ([`CallBuilder`]) and the immutable result
//! ([`FinishedCall`]).

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::audio::AudioFormat;

/// Sample rate of assembled call audio, in hertz.
///
/// Both IMBE/AMBE voice synthesis and analog NBFM demodulation land on 8 kHz
/// mono, and that is what is uploaded; nothing in this crate resamples.
pub const PCM_RATE: u32 = 8000;

/// The RMS floor, in dBFS relative to i16 full scale, a 20 ms frame must clear
/// to be counted as speech by [`speech_seconds`].
///
/// The same floor Keyup's own speech/page/none classifier uses, so the two
/// sides agree about what an empty call looks like.
pub const SPEECH_FLOOR_DBFS: f64 = -40.0;

/// Samples in one 20 ms frame at [`PCM_RATE`].
const SPEECH_FRAME: usize = PCM_RATE as usize / 50;

/// Seconds of call audio loud enough to be speech.
///
/// The audio is cut into 20 ms frames (160 samples at 8 kHz); a frame counts
/// when its RMS is above [`SPEECH_FLOOR_DBFS`] of i16 full scale — 32768 ×
/// 10^(−40/20) ≈ 327.68 — and the answer is the number of counted frames times
/// 0.02. A trailing partial frame is ignored: at 20 ms it can only ever change
/// the answer by less than one frame, and dropping it keeps the measure a whole
/// number of frames.
///
/// This is deliberately the cheap half of what Keyup does. Keyup excludes tone
/// spans (a two-tone page is not speech) before measuring; the recorder does
/// not detect tones, so a page counts as loud audio here. That errs toward
/// uploading — the gate this feeds only ever refuses calls with *no* loud audio
/// at all, which is what a mic click is.
pub fn speech_seconds(pcm: &[i16]) -> f32 {
    // Squared, so the comparison below needs no square root per frame.
    let floor = f64::from(i16::MIN).abs() * 10f64.powf(SPEECH_FLOOR_DBFS / 20.0);
    let floor_sq = floor * floor;
    let (frames, _tail) = pcm.as_chunks::<SPEECH_FRAME>();
    let loud = frames
        .iter()
        .filter(|frame| {
            let sum: f64 = frame
                .iter()
                .map(|&s| {
                    let v = f64::from(s);
                    v * v
                })
                .sum();
            sum / frame.len() as f64 > floor_sq
        })
        .count();
    (loud as f64 * 0.02) as f32
}

/// One transmission within a call: the radio that keyed up and where it starts
/// in the call audio.
///
/// This is the trunk-recorder `srcList` entry. The *length* of a transmission
/// is not stored: it is the distance to the next segment (and, for the last
/// one, to the end of the call), which is exactly how Keyup's parser derives
/// it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SourceSegment {
    /// The radio (unit) id that keyed up.
    pub src: u32,
    /// Offset of this transmission from the start of the call, in seconds.
    pub pos_s: f64,
    /// Whether this transmission was flagged emergency.
    pub emergency: bool,
    /// Optional label for the radio (talker alias or a configured tag).
    pub tag: Option<String>,
}

/// One frequency the call rode on, with the decode-quality counters gathered
/// while it was there.
///
/// This is the trunk-recorder `freqList` entry. Most calls have exactly one;
/// a call that follows a grant update to another traffic channel has several.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FrequencySegment {
    /// Center frequency in hertz.
    pub freq_hz: u64,
    /// Offset from the start of the call at which this frequency began, in
    /// seconds.
    pub pos_s: f64,
    /// How long the call stayed on this frequency, in seconds.
    pub len_s: f64,
    /// Voice/EDAC errors counted while on this frequency.
    pub error_count: u32,
    /// Impulse-noise ("spike") events counted while on this frequency.
    pub spike_count: u32,
}

/// Comparable decode-quality facts about **this recorder's copy** of a call.
///
/// Several trunkd instances can hear the same P25 site, so the archive can be
/// offered the same call twice. Deduplication alone then has to pick a winner
/// blind; these are the numbers that let it pick the better copy instead —
/// which is why every one of them is a *fact this recorder measured*, not an
/// estimate. Fields that trunkd cannot honestly source are absent rather than
/// approximated:
///
/// * **`gap_ms`** is not here. The assembler appends vocoded frames back to
///   back and never pads or skips. A frame can be lost before it gets here —
///   the DSP → call-thread queue is drop-oldest, and what it evicts is counted
///   recorder-wide (`calls.queue_dropped`), not per call — but nothing in the
///   pipeline records where in a call a discontinuity fell. A number derived
///   from the wall-clock/audio difference would be silence between
///   transmissions as often as it was loss, so none is sent.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SourceQuality {
    /// Which recorder produced this copy — `recorder.instance_id`, the same
    /// identifier the tuner-status POST carries.
    ///
    /// Two copies of one call are worth comparing only when they came from
    /// different instances; this is what says so.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
    /// Voice frames this recorder decoded into audio for this call.
    ///
    /// Counts only frames that reached the vocoder and were appended: an
    /// encrypted call reports 0, because its ciphertext is discarded rather
    /// than synthesized.
    pub voice_frames: u32,
    /// Voice frames the vocoder gave up on, of the [`Self::voice_frames`] it
    /// decoded.
    ///
    /// A *muted* IMBE frame (`trunkd_mbe::ImbeDecoder::last_frame_muted`):
    /// replaced with low-level noise because its running error rate was too
    /// high to repair, or because the frame had been repeated to the limit.
    /// It is the vocoder's own "I could not recover this", so the ratio to
    /// `voice_frames` is directly comparable between recorders.
    pub bad_frames: u32,
    /// The recording device's **wideband** level in dBFS, read at call end.
    ///
    /// Context, not a per-channel measurement: it is the level the front end
    /// saw across the whole capture span (`device.{id}.rssi_dbfs`), not the
    /// signal-to-noise of this call's traffic channel. It says whether a tuner
    /// is deaf or being driven into compression; it does not say this call was
    /// clean. Compare it between recorders only alongside the frame counts.
    /// Absent when the device has never measured a level.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rssi_dbfs: Option<f64>,
}

impl SourceQuality {
    /// True when there is nothing worth telling the archive.
    ///
    /// The upload omits the field entirely in that case, so a recorder with
    /// nothing to report costs no bytes and no ambiguity.
    pub fn is_empty(&self) -> bool {
        self.instance.is_none()
            && self.voice_frames == 0
            && self.bad_frames == 0
            && self.rssi_dbfs.is_none()
    }
}

/// What kind of call this is on Keyup's side of the wire.
///
/// Deliberately narrower than the radio's own taxonomy: Keyup files a call
/// under a talkgroup or under a pair of radios, and nothing else. A telephone
/// interconnect is recorded as `Group` because that is how it has always been
/// filed, and changing it is not this feature's business.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallKind {
    /// A talkgroup call: `talkgroup` names it.
    #[default]
    Group,
    /// A unit-to-unit (private) call between two radios: `source` and
    /// [`FinishedCall::target_radio`] name it, and it has no talkgroup.
    Unit,
}

impl CallKind {
    /// The wire spelling Keyup's `call_kind` field takes.
    pub fn as_str(self) -> &'static str {
        match self {
            CallKind::Group => "group",
            CallKind::Unit => "unit",
        }
    }
}

/// What direct-to-Broadcastify delivery did with one call.
///
/// Attached to the [`FinishedCall`] by the uplink worker *before* the call is
/// pushed to Keyup, so the outcome rides along on the same multipart body, and
/// written into the spool sidecar so a call that waits out a Keyup outage is
/// retried with the outcome it already has rather than being registered with
/// Broadcastify a second time.
///
/// `None` on a [`FinishedCall`] means the feature decided nothing about this
/// call — it is off, or this recorder never looked at it — and the Keyup push
/// then carries no `broadcastify*` field at all.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BroadcastifyOutcome {
    /// `uploaded` | `duplicate` | `failed` | `late` | `claim_denied` |
    /// `skipped`.
    pub outcome: String,
    /// The numeric code of the stage-1 response line, when one was parsed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<i64>,
    /// The response, or the reason: the first line of what the endpoint said
    /// (or a short local explanation), capped and with the API key redacted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response: Option<String>,
    /// Broadcastify's id for the stored call: the last path segment of the
    /// upload URL. `uploaded` only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_id: Option<String>,
    /// The instance that holds the claim. `claim_denied` only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub holder: Option<String>,
}

impl BroadcastifyOutcome {
    /// An outcome with nothing but its verdict.
    pub fn new(outcome: &str) -> Self {
        BroadcastifyOutcome {
            outcome: outcome.to_owned(),
            code: None,
            response: None,
            remote_id: None,
            holder: None,
        }
    }

    /// An outcome with a verdict and a short explanation.
    pub fn with_response(outcome: &str, response: impl Into<String>) -> Self {
        BroadcastifyOutcome {
            response: Some(response.into()),
            ..BroadcastifyOutcome::new(outcome)
        }
    }
}

/// What a conventional (analog) channel adds to a call: the analog extras of
/// the keyup upload contract.
///
/// Present only on calls recorded from a `[[channel]]`; a P25 call has `None`
/// and its upload and sidecar carry none of these keys. Each optional member
/// is omitted from the wire when unknown (e.g. a call closed by the orphan
/// sweep never saw its end measurements).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ConventionalInfo {
    /// Demodulation mode, wire spelling (`nbfm`). Uploaded as `mode`.
    pub mode: String,
    /// Decoder name as sdrtrunk's ID3 tag carried it (`NBFM`). Uploaded as
    /// `decoder`.
    pub decoder: String,
    /// The configured tone, canonical spelling (`167.9`, `D445N`), when the
    /// channel is tone-gated. Uploaded as `tone`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tone: Option<String>,
    /// The tone heard for most of the call, if any. Uploaded as
    /// `toneDetected`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tone_detected: Option<String>,
    /// Why the call ended (`squelch_closed`, `tone_lost`, `max_duration`,
    /// `shutdown`, `orphaned`). Uploaded as `squelchReason`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub squelch_reason: Option<String>,
    /// Power above the noise floor when the squelch opened, dB. Uploaded as
    /// `snrDb`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snr_db: Option<f32>,
    /// Largest instantaneous deviation seen during the call, Hz. Uploaded as
    /// `peakDeviationHz`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peak_deviation_hz: Option<f32>,
    /// Mean carrier offset from the channel centre while open, Hz. Uploaded as
    /// `offsetHz`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offset_hz: Option<f32>,
}

impl ConventionalInfo {
    /// An NBFM call's extras with nothing measured yet: `mode=nbfm`,
    /// `decoder=NBFM`.
    pub fn nbfm() -> Self {
        ConventionalInfo {
            mode: "nbfm".to_owned(),
            decoder: "NBFM".to_owned(),
            ..ConventionalInfo::default()
        }
    }

    /// The extras as wire `(name, value)` pairs, in upload order. The one list
    /// both the multipart body and the sidecar are written from, so the two
    /// can never spell a key differently. Numbers carry one decimal.
    pub fn wire_fields(&self) -> Vec<(&'static str, WireValue)> {
        let mut out = vec![
            ("mode", WireValue::Text(self.mode.clone())),
            ("decoder", WireValue::Text(self.decoder.clone())),
        ];
        let text = |name, v: &Option<String>, out: &mut Vec<_>| {
            if let Some(v) = v {
                out.push((name, WireValue::Text(v.clone())));
            }
        };
        let num = |name, v: Option<f32>, out: &mut Vec<_>| {
            if let Some(v) = v.filter(|v| v.is_finite()) {
                out.push((name, WireValue::Number(round1(v))));
            }
        };
        text("tone", &self.tone, &mut out);
        text("toneDetected", &self.tone_detected, &mut out);
        text("squelchReason", &self.squelch_reason, &mut out);
        num("snrDb", self.snr_db, &mut out);
        num("peakDeviationHz", self.peak_deviation_hz, &mut out);
        num("offsetHz", self.offset_hz, &mut out);
        out
    }
}

/// One value of [`ConventionalInfo::wire_fields`].
#[derive(Clone, Debug, PartialEq)]
pub enum WireValue {
    /// A string field.
    Text(String),
    /// A measurement, already rounded to one decimal.
    Number(f64),
}

impl std::fmt::Display for WireValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WireValue::Text(s) => f.write_str(s),
            WireValue::Number(n) => write!(f, "{n:.1}"),
        }
    }
}

fn round1(v: f32) -> f64 {
    (f64::from(v) * 10.0).round() / 10.0
}

/// A completed call: immutable, self-describing, ready to encode and upload.
///
/// `pcm` is 8 kHz mono signed 16-bit audio and is **empty for encrypted
/// calls**, which are recorded as metadata only.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FinishedCall {
    /// System label (Keyup's `systemLabel` / trunk-recorder's `short_name`).
    pub system: String,
    /// Site label within the system.
    pub site: String,
    /// True call start, unix milliseconds.
    pub started_at_ms: i64,
    /// True call end, unix milliseconds.
    pub ended_at_ms: i64,
    /// Frequency the call started on, in hertz.
    pub frequency_hz: u64,
    /// Talkgroup id (the supergroup id for a patch).
    ///
    /// Meaningless for a [`CallKind::Unit`] call, which is filed under the two
    /// radios instead and whose upload omits the field entirely.
    pub talkgroup: u32,
    /// Group or unit-to-unit. `#[serde(default)]`, so a sidecar written before
    /// private calls were followed still loads as the group call it was.
    #[serde(default)]
    pub call_kind: CallKind,
    /// The radio being called, for a [`CallKind::Unit`] call: unit B, where
    /// [`Self::source`] is the unit doing the talking.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_radio: Option<u32>,
    /// Talkgroup label, when the recorder was configured with one.
    pub talkgroup_label: Option<String>,
    /// First radio heard on the call, if any.
    pub source: Option<u32>,
    /// Every transmission in call order.
    pub sources: Vec<SourceSegment>,
    /// Every frequency the call rode on, in call order.
    pub frequencies: Vec<FrequencySegment>,
    /// Patch member talkgroups (empty for an ordinary group call).
    pub patches: Vec<u32>,
    /// Whether any part of the call was flagged emergency.
    pub emergency: bool,
    /// Whether the call was encrypted (and therefore has no audio).
    pub encrypted: bool,
    /// Encryption algorithm name (e.g. `"AES-256"`, `"ADP"`), when known.
    pub algorithm: Option<String>,
    /// Encryption key id, when known.
    pub key_id: Option<u16>,
    /// Talker alias transmitted over the air, when received.
    pub talker_alias: Option<String>,
    /// WACN of the first talker, when a P25 Source ID Extension (LC 9)
    /// qualified it over the air.
    ///
    /// Present only for a roaming subscriber: an ordinary local radio sends
    /// only its 24-bit id, and this stays `None`. It is what lets the archive
    /// tell "radio 9100025 of system 57" from a local radio that happens to
    /// share the number.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_wacn: Option<u32>,
    /// Home system id of the first talker, from the same word.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_system: Option<u16>,
    /// Network Access Code of the site the call was recorded from.
    pub nac: Option<u16>,
    /// What this recorder can say about the quality of *its* copy, for an
    /// archive choosing between copies from several recorders.
    ///
    /// `#[serde(default)]`, so a sidecar written before the field existed still
    /// loads; `None` means the recorder had nothing to report.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_quality: Option<SourceQuality>,
    /// What direct-to-Broadcastify delivery did with this call, when the
    /// feature looked at it at all.
    ///
    /// Set on the uplink thread just before the Keyup push, never on a DSP
    /// thread: [`CallBuilder::finish`] always leaves it `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub broadcastify: Option<BroadcastifyOutcome>,
    /// The analog extras, for a call recorded from a conventional channel;
    /// `None` for every P25 call. Also what makes the sidecar say
    /// `"audio_type": "analog"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conventional: Option<ConventionalInfo>,
    /// Seconds of audio loud enough to be speech, from [`speech_seconds`]:
    /// 20 ms frames whose RMS clears −40 dBFS, times 0.02.
    ///
    /// Measured once at [`CallBuilder::finish`], while the PCM is still in
    /// hand, so the uplink thread can refuse to spend a Broadcastify register
    /// and PUT on a mic click without touching the audio again. `0.0` for a
    /// metadata-only (encrypted) call, and `#[serde(default)]`, so a sidecar
    /// written before the field existed still loads.
    #[serde(default)]
    pub speech_s: f32,
    /// 8 kHz mono PCM. Empty for encrypted (metadata-only) calls.
    pub pcm: Vec<i16>,
}

impl FinishedCall {
    /// Call length in seconds.
    ///
    /// Derived from the true start/stop timestamps; if those are degenerate
    /// (equal, or out of order) the length of the accumulated audio is used
    /// instead, so a call is never reported as zero-length while carrying
    /// audio.
    pub fn duration_s(&self) -> f64 {
        let wall = (self.ended_at_ms - self.started_at_ms) as f64 / 1000.0;
        if wall > 0.0 {
            wall
        } else {
            self.audio_duration_s()
        }
    }

    /// Length of the accumulated audio in seconds (0.0 for a metadata-only
    /// call).
    pub fn audio_duration_s(&self) -> f64 {
        self.pcm.len() as f64 / f64::from(PCM_RATE)
    }

    /// The filename stem shared by the audio file and its JSON sidecar:
    /// `<system>_<talkgroup>_<start-unix-ms>`, e.g.
    /// `Example-Network_52101_1788012000123`.
    ///
    /// A unit-to-unit call has no talkgroup, so `UNIT_<A>_TO_<B>` stands where
    /// the talkgroup would be — `Example-Network_UNIT_9000001_TO_9000007_1788…` —
    /// which keeps every name unique, sortable and readable without inventing
    /// a fake group id that would collide with a real one.
    ///
    /// The system label is sanitized to `[A-Za-z0-9._-]` so the name is safe as
    /// a path component, as a `filename=` in a multipart part, and as something
    /// Keyup will store verbatim.
    pub fn audio_stem(&self) -> String {
        format!(
            "{}_{}_{}",
            sanitize(&self.system),
            self.call_label(),
            self.started_at_ms
        )
    }

    /// What stands in for the talkgroup in a filename: the talkgroup id, or
    /// `UNIT_<A>_TO_<B>` for a private call.
    fn call_label(&self) -> String {
        match self.call_kind {
            CallKind::Group => self.talkgroup.to_string(),
            CallKind::Unit => format!(
                "UNIT_{}_TO_{}",
                self.source.unwrap_or(0),
                self.target_radio.unwrap_or(0)
            ),
        }
    }

    /// [`Self::audio_stem`] plus an explicit extension (given without the dot,
    /// e.g. `"mp3"`).
    pub fn audio_name_with_ext(&self, ext: &str) -> String {
        format!("{}.{}", self.audio_stem(), ext)
    }

    /// The upload filename for a given audio format, e.g.
    /// `Example-Network_52101_1788012000123.mp3`.
    pub fn audio_name(&self, format: AudioFormat) -> String {
        self.audio_name_with_ext(format.extension())
    }
}

/// Replace anything outside `[A-Za-z0-9._-]` with `-`, collapsing runs.
fn sanitize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_dash = false;
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
            out.push(c);
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    // Leading dots and dashes are trimmed too: a name like `../..` sanitizes to
    // `..-..`, which is harmless as a single path component but reads as a
    // traversal attempt, and Keyup's own `safe_filename` strips the same set.
    let trimmed = out.trim_matches(|c| c == '-' || c == '.');
    if trimmed.is_empty() {
        "call".to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// Accumulates one in-progress call.
///
/// Lives on the DSP thread: every method is allocation-light and none of them
/// block. Audio is capped at [`Self::DEFAULT_MAX_DURATION`] (override with
/// [`Self::with_max_duration`]) so a call that never tears down cannot grow the
/// process without bound.
///
/// ```
/// use trunkd_call::CallBuilder;
///
/// let mut b = CallBuilder::new("Example Network", "Site 1", 1_788_012_000_123, 852_137_500, 52101);
/// b.talker(9_000_011, 1_788_012_000_123, false, Some("UNIT_313".into()));
/// b.push_pcm(&[0i16; 8000]);
/// let call = b.finish(1_788_012_001_123);
/// assert_eq!(call.audio_stem(), "Example-Network_52101_1788012000123");
/// assert_eq!(call.sources.len(), 1);
/// ```
pub struct CallBuilder {
    system: String,
    site: String,
    started_at_ms: i64,
    frequency_hz: u64,
    talkgroup: u32,
    talkgroup_label: Option<String>,
    call_kind: CallKind,
    target_radio: Option<u32>,
    sources: Vec<SourceSegment>,
    frequencies: Vec<FrequencySegment>,
    patches: Vec<u32>,
    emergency: bool,
    encrypted: bool,
    algorithm: Option<String>,
    key_id: Option<u16>,
    talker_alias: Option<String>,
    source_wacn: Option<u32>,
    source_system: Option<u16>,
    nac: Option<u16>,
    quality: SourceQuality,
    conventional: Option<ConventionalInfo>,
    pcm: Vec<i16>,
    max_samples: usize,
    truncated: bool,
}

impl CallBuilder {
    /// Longest call audio kept by default (30 minutes ≈ 28 MB of PCM).
    ///
    /// Far longer than any real transmission group; it exists purely so a call
    /// whose teardown is missed cannot grow without bound.
    pub const DEFAULT_MAX_DURATION: Duration = Duration::from_secs(30 * 60);

    /// Audio reserved when a call's first samples arrive: 10 s (160 KB), longer
    /// than most transmissions, so a typical call's PCM is allocated once.
    pub const INITIAL_PCM: Duration = Duration::from_secs(10);

    /// Start a call on `frequency_hz` for `talkgroup` at `started_at_ms` (unix
    /// milliseconds).
    pub fn new(
        system: impl Into<String>,
        site: impl Into<String>,
        started_at_ms: i64,
        frequency_hz: u64,
        talkgroup: u32,
    ) -> Self {
        CallBuilder {
            system: system.into(),
            site: site.into(),
            started_at_ms,
            frequency_hz,
            talkgroup,
            talkgroup_label: None,
            call_kind: CallKind::Group,
            target_radio: None,
            sources: Vec::new(),
            // A call always has at least the frequency it was granted on.
            frequencies: vec![FrequencySegment {
                freq_hz: frequency_hz,
                pos_s: 0.0,
                len_s: 0.0,
                error_count: 0,
                spike_count: 0,
            }],
            patches: Vec::new(),
            emergency: false,
            encrypted: false,
            algorithm: None,
            key_id: None,
            talker_alias: None,
            source_wacn: None,
            source_system: None,
            nac: None,
            quality: SourceQuality::default(),
            conventional: None,
            pcm: Vec::new(),
            max_samples: duration_to_samples(Self::DEFAULT_MAX_DURATION),
            truncated: false,
        }
    }

    /// Override the audio cap (see [`Self::DEFAULT_MAX_DURATION`]).
    pub fn with_max_duration(mut self, max: Duration) -> Self {
        self.max_samples = duration_to_samples(max);
        self
    }

    /// Append synthesized 8 kHz mono audio.
    ///
    /// Silently stops appending once the cap is reached; [`Self::truncated`]
    /// then reports true.
    pub fn push_pcm(&mut self, samples: &[i16]) {
        if self.pcm.len() >= self.max_samples {
            self.truncated = true;
            return;
        }
        let room = self.max_samples - self.pcm.len();
        let take = samples.len().min(room);
        self.grow_for(take);
        self.pcm.extend_from_slice(&samples[..take]);
        if take < samples.len() {
            self.truncated = true;
        }
    }

    /// Make room for `n` more samples.
    ///
    /// Left to `Vec`, a call's PCM starts at a few samples and doubles its way
    /// up — about ten reallocations and copies for a 10 s call, one per frame
    /// early on — and can end with nearly twice the cap reserved. So the first
    /// audio reserves [`Self::INITIAL_PCM`] (most calls never grow past it),
    /// later growth doubles, and neither ever reserves beyond the cap.
    fn grow_for(&mut self, n: usize) {
        let need = self.pcm.len() + n;
        if need <= self.pcm.capacity() {
            return;
        }
        let target = if self.pcm.capacity() == 0 {
            duration_to_samples(Self::INITIAL_PCM).max(need)
        } else {
            (self.pcm.capacity() * 2).max(need)
        };
        let target = target.min(self.max_samples).max(need);
        self.pcm.reserve_exact(target - self.pcm.len());
    }

    /// Append one vocoded voice frame and count it.
    ///
    /// The counting half is what [`push_pcm`](Self::push_pcm) deliberately does
    /// not do: a caller handing over a second of audio is not handing over one
    /// frame, and [`SourceQuality::voice_frames`] only means anything if it
    /// counts frames. `muted` is the vocoder's verdict on this frame
    /// (`ImbeDecoder::last_frame_muted`) and raises
    /// [`SourceQuality::bad_frames`].
    pub fn push_voice_frame(&mut self, samples: &[i16], muted: bool) {
        self.quality.voice_frames = self.quality.voice_frames.saturating_add(1);
        if muted {
            self.quality.bad_frames = self.quality.bad_frames.saturating_add(1);
        }
        self.push_pcm(samples);
    }

    /// Name the recorder this copy of the call came from
    /// ([`SourceQuality::instance`]).
    pub fn set_instance(&mut self, instance: Option<String>) {
        self.quality.instance = instance.filter(|s| !s.is_empty());
    }

    /// Record the capture device's wideband level at call end
    /// ([`SourceQuality::rssi_dbfs`]).
    pub fn set_rssi_dbfs(&mut self, dbfs: Option<f64>) {
        self.quality.rssi_dbfs = dbfs.filter(|v| v.is_finite());
    }

    /// The quality facts gathered so far.
    pub fn quality(&self) -> &SourceQuality {
        &self.quality
    }

    /// Whether audio was dropped because the call hit its duration cap.
    pub fn truncated(&self) -> bool {
        self.truncated
    }

    /// Samples of audio accumulated so far.
    pub fn sample_count(&self) -> usize {
        self.pcm.len()
    }

    /// Record that `src` started transmitting at `at_ms` (unix milliseconds).
    ///
    /// Repeating the same radio with the same emergency flag is a no-op, so the
    /// trunking manager can call this on every voice frame. `emergency` also
    /// latches onto the call as a whole.
    ///
    /// Radio 0 means "not named yet", never a radio: a call opened from a
    /// grant update, or a Source ID Extension / talker alias that arrives
    /// before the link control naming the talker, reports it. It latches the
    /// emergency flag and is otherwise ignored, so the first *named* radio is
    /// the call's [`FinishedCall::source`] and `sources` never lists a 0
    /// (Keyup takes `sources[0]` as the caller). A call on which no radio is
    /// ever named finishes with no source at all.
    pub fn talker(&mut self, src: u32, at_ms: i64, emergency: bool, tag: Option<String>) {
        if emergency {
            self.emergency = true;
        }
        if src == 0 {
            return;
        }
        if let Some(last) = self.sources.last() {
            if last.src == src && last.emergency == emergency {
                return;
            }
        }
        let pos_s = self.pos_at(at_ms);
        self.sources.push(SourceSegment {
            src,
            pos_s,
            emergency,
            tag,
        });
    }

    /// Record that the call moved to `freq_hz` at `at_ms`, closing the previous
    /// frequency segment.
    pub fn frequency(&mut self, freq_hz: u64, at_ms: i64) {
        let pos_s = self.pos_at(at_ms);
        if let Some(last) = self.frequencies.last_mut() {
            if last.freq_hz == freq_hz {
                return;
            }
            last.len_s = (pos_s - last.pos_s).max(0.0);
        }
        self.frequencies.push(FrequencySegment {
            freq_hz,
            pos_s,
            len_s: 0.0,
            error_count: 0,
            spike_count: 0,
        });
    }

    /// Add `n` decode errors to the frequency the call is currently on.
    pub fn add_errors(&mut self, n: u32) {
        if let Some(last) = self.frequencies.last_mut() {
            last.error_count = last.error_count.saturating_add(n);
        }
    }

    /// Add `n` impulse-noise spikes to the frequency the call is currently on.
    pub fn add_spikes(&mut self, n: u32) {
        if let Some(last) = self.frequencies.last_mut() {
            last.spike_count = last.spike_count.saturating_add(n);
        }
    }

    /// Set the emergency and encrypted flags.
    ///
    /// Both latch: once either has been set on a call it stays set, because a
    /// call that was emergency (or encrypted) for one transmission is reported
    /// as such for the whole call.
    pub fn set_flags(&mut self, emergency: bool, encrypted: bool) {
        self.emergency |= emergency;
        self.encrypted |= encrypted;
    }

    /// Mark the call encrypted and record what the control channel said about
    /// the encryption. Audio is discarded at [`Self::finish`].
    pub fn set_encryption(&mut self, algorithm: Option<String>, key_id: Option<u16>) {
        self.encrypted = true;
        self.set_algorithm(algorithm, key_id);
    }

    /// Record the encryption parameters **without** asserting that the call is
    /// encrypted.
    ///
    /// A P25 HDU or Encryption Sync announces an ALGID on every call, including
    /// `0x80` (`UNENCRYPTED`), and the upload contract carries that value for
    /// clear calls too. Routing it through [`Self::set_encryption`] would latch
    /// the encrypted flag and silently throw the audio away at
    /// [`Self::finish`], so recording the algorithm and asserting encryption
    /// are separate acts.
    pub fn set_algorithm(&mut self, algorithm: Option<String>, key_id: Option<u16>) {
        if algorithm.is_some() {
            self.algorithm = algorithm;
        }
        if key_id.is_some() {
            self.key_id = key_id;
        }
    }

    /// Set the talkgroup label (from configuration or a playlist import).
    pub fn set_talkgroup_label(&mut self, label: Option<String>) {
        self.talkgroup_label = label;
    }

    /// Mark this a unit-to-unit (private) call between the talker and
    /// `target_radio`.
    ///
    /// It changes what the call *is*, not just a label: the upload sends
    /// `call_kind=unit` and `target_radio_id`, omits `talkgroup`, and the file
    /// is named `UNIT_<A>_TO_<B>` (see [`FinishedCall::audio_stem`]).
    pub fn set_unit_call(&mut self, target_radio: u32) {
        self.call_kind = CallKind::Unit;
        self.target_radio = Some(target_radio);
    }

    /// Set the patch member talkgroups covered by this call.
    pub fn set_patches(&mut self, patches: Vec<u32>) {
        self.patches = patches;
    }

    /// Set the over-the-air talker alias.
    pub fn set_talker_alias(&mut self, alias: Option<String>) {
        self.talker_alias = alias;
    }

    /// Set the fully qualified SUID of the talker, from a Source ID
    /// Extension link-control word.
    pub fn set_source_suid(&mut self, wacn: Option<u32>, system: Option<u16>) {
        if wacn.is_some() {
            self.source_wacn = wacn;
            self.source_system = system;
        }
    }

    /// Mark this a conventional-channel call and attach its analog extras
    /// ([`FinishedCall::conventional`]).
    pub fn set_conventional(&mut self, info: Option<ConventionalInfo>) {
        self.conventional = info;
    }

    /// Set the site NAC.
    pub fn set_nac(&mut self, nac: Option<u16>) {
        self.nac = nac;
    }

    /// Close the call at `ended_at_ms` (unix milliseconds) and produce the
    /// immutable [`FinishedCall`].
    ///
    /// Closes the trailing frequency segment, derives [`FinishedCall::source`]
    /// from the first transmission, and discards the PCM if the call is
    /// encrypted.
    pub fn finish(mut self, ended_at_ms: i64) -> FinishedCall {
        let ended_at_ms = ended_at_ms.max(self.started_at_ms);
        let wall = (ended_at_ms - self.started_at_ms) as f64 / 1000.0;
        let audio = self.pcm.len() as f64 / f64::from(PCM_RATE);
        let duration = if wall > 0.0 { wall } else { audio };

        if let Some(last) = self.frequencies.last_mut() {
            last.len_s = (duration - last.pos_s).max(0.0);
        }

        let source = self.sources.first().map(|s| s.src);
        let pcm = if self.encrypted { Vec::new() } else { self.pcm };
        // Measured here and nowhere else: this is the last place the samples
        // are in hand before the uplink thread has only an MP3.
        let speech_s = speech_seconds(&pcm);

        FinishedCall {
            system: self.system,
            site: self.site,
            started_at_ms: self.started_at_ms,
            ended_at_ms,
            frequency_hz: self.frequency_hz,
            talkgroup: self.talkgroup,
            call_kind: self.call_kind,
            target_radio: self.target_radio,
            talkgroup_label: self.talkgroup_label,
            source,
            sources: self.sources,
            frequencies: self.frequencies,
            patches: self.patches,
            emergency: self.emergency,
            encrypted: self.encrypted,
            algorithm: self.algorithm,
            key_id: self.key_id,
            talker_alias: self.talker_alias,
            source_wacn: self.source_wacn,
            source_system: self.source_system,
            nac: self.nac,
            // Omitted rather than sent empty: "I have nothing to say about this
            // copy" and "this copy scored zero" are different claims, and only
            // the second one is a number.
            source_quality: (!self.quality.is_empty()).then_some(self.quality),
            // Filled in later, on the uplink thread, and only when keyup has
            // turned the feature on: call assembly knows nothing about it.
            broadcastify: None,
            conventional: self.conventional,
            speech_s,
            pcm,
        }
    }

    /// Offset into the call for a wall-clock instant.
    ///
    /// While audio is accumulating the *audio* position is authoritative — it
    /// is the offset a player will seek to, which is what `pos` means to Keyup
    /// — and wall clock only answers for calls with no audio at all (encrypted,
    /// or a source seen before the first voice frame).
    fn pos_at(&self, at_ms: i64) -> f64 {
        if self.pcm.is_empty() {
            ((at_ms - self.started_at_ms).max(0)) as f64 / 1000.0
        } else {
            self.pcm.len() as f64 / f64::from(PCM_RATE)
        }
    }
}

fn duration_to_samples(d: Duration) -> usize {
    let samples = d.as_secs_f64() * f64::from(PCM_RATE);
    if samples <= 0.0 {
        0
    } else {
        samples as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn builder() -> CallBuilder {
        CallBuilder::new(
            "Example Network",
            "Site 1",
            1_788_012_000_123,
            852_137_500,
            52101,
        )
    }

    #[test]
    fn audio_name_sanitizes_and_uses_the_format_extension() {
        let call = builder().finish(1_788_012_002_123);
        assert_eq!(call.audio_stem(), "Example-Network_52101_1788012000123");
        assert_eq!(
            call.audio_name(AudioFormat::default()),
            "Example-Network_52101_1788012000123.mp3"
        );
        assert_eq!(
            call.audio_name(AudioFormat::Wav),
            "Example-Network_52101_1788012000123.wav"
        );
    }

    #[test]
    fn duration_comes_from_the_true_timestamps() {
        let call = builder().finish(1_788_012_004_623);
        assert!((call.duration_s() - 4.5).abs() < 1e-9);
    }

    #[test]
    fn duration_falls_back_to_audio_when_timestamps_are_degenerate() {
        let mut b = builder();
        b.push_pcm(&[0i16; 16_000]);
        let call = b.finish(1_788_012_000_123);
        assert!((call.duration_s() - 2.0).abs() < 1e-9);
    }

    #[test]
    fn repeated_talker_does_not_duplicate_segments() {
        let mut b = builder();
        for _ in 0..10 {
            b.talker(9_000_011, 1_788_012_000_123, false, Some("UNIT_313".into()));
        }
        b.push_pcm(&[0i16; 8000]);
        b.talker(9_000_012, 1_788_012_001_123, false, None);
        let call = b.finish(1_788_012_002_123);
        assert_eq!(call.sources.len(), 2);
        assert_eq!(call.source, Some(9_000_011));
        assert!((call.sources[0].pos_s - 0.0).abs() < 1e-9);
        // The second talker starts one second of *audio* in.
        assert!((call.sources[1].pos_s - 1.0).abs() < 1e-9);
    }

    #[test]
    fn an_unnamed_talker_is_never_the_caller() {
        // A call opened from a grant update hears "radio 0" before the
        // link control names the talker. That is "unknown", not a radio.
        let mut b = builder();
        b.talker(0, 1_788_012_000_123, false, Some("early alias".into()));
        b.push_pcm(&[0i16; 8000]);
        b.talker(9_000_011, 1_788_012_001_123, false, None);
        b.talker(0, 1_788_012_001_500, false, None);
        let call = b.finish(1_788_012_002_123);
        assert_eq!(call.source, Some(9_000_011), "the first named radio");
        assert_eq!(
            call.sources.len(),
            1,
            "no radio-0 segment: {:?}",
            call.sources
        );
        assert_eq!(call.sources[0].src, 9_000_011);
        assert!((call.sources[0].pos_s - 1.0).abs() < 1e-9);
    }

    #[test]
    fn a_call_whose_radio_is_never_named_has_no_source() {
        let mut b = builder();
        b.talker(0, 1_788_012_000_123, true, None);
        let call = b.finish(1_788_012_001_123);
        assert_eq!(call.source, None);
        assert!(call.sources.is_empty());
        assert!(call.emergency, "radio 0 still latches the emergency flag");
    }

    #[test]
    fn emergency_latches_from_a_transmission() {
        let mut b = builder();
        b.talker(1, 1_788_012_000_123, false, None);
        b.talker(2, 1_788_012_000_500, true, None);
        let call = b.finish(1_788_012_001_123);
        assert!(call.emergency);
        assert!(call.sources[1].emergency);
    }

    #[test]
    fn frequency_segments_close_on_change_and_at_finish() {
        let mut b = builder();
        b.add_errors(3);
        b.push_pcm(&[0i16; 8000]);
        b.frequency(852_162_500, 1_788_012_001_123);
        b.add_spikes(2);
        b.push_pcm(&[0i16; 8000]);
        let call = b.finish(1_788_012_002_123);
        assert_eq!(call.frequencies.len(), 2);
        assert_eq!(call.frequencies[0].freq_hz, 852_137_500);
        assert_eq!(call.frequencies[0].error_count, 3);
        assert!((call.frequencies[0].len_s - 1.0).abs() < 1e-9);
        assert_eq!(call.frequencies[1].spike_count, 2);
        assert!((call.frequencies[1].pos_s - 1.0).abs() < 1e-9);
        assert!((call.frequencies[1].len_s - 1.0).abs() < 1e-9);
        // The call's headline frequency stays the one it was granted on.
        assert_eq!(call.frequency_hz, 852_137_500);
    }

    #[test]
    fn encrypted_calls_keep_metadata_and_drop_audio() {
        let mut b = builder();
        b.push_pcm(&[1234i16; 8000]);
        b.set_encryption(Some("AES-256".into()), Some(0x2a));
        let call = b.finish(1_788_012_003_123);
        assert!(call.encrypted);
        assert!(call.pcm.is_empty());
        assert_eq!(call.algorithm.as_deref(), Some("AES-256"));
        assert_eq!(call.key_id, Some(42));
        assert!((call.duration_s() - 3.0).abs() < 1e-9);
    }

    #[test]
    fn recording_a_clear_algorithm_keeps_the_audio() {
        // Every P25 call announces an ALGID, `0x80` included. Reporting the
        // clear one must not be a roundabout way of deleting the recording.
        let mut b = builder();
        b.push_pcm(&[1234i16; 8000]);
        b.set_algorithm(Some("UNENCRYPTED".into()), None);
        let call = b.finish(1_788_012_001_123);
        assert!(!call.encrypted);
        assert_eq!(call.pcm.len(), 8000);
        assert_eq!(call.algorithm.as_deref(), Some("UNENCRYPTED"));
        assert_eq!(call.key_id, None);
    }

    #[test]
    fn audio_is_capped_at_the_duration_limit() {
        let mut b = builder().with_max_duration(Duration::from_secs(1));
        b.push_pcm(&[0i16; 6000]);
        b.push_pcm(&[0i16; 6000]);
        b.push_pcm(&[0i16; 6000]);
        assert!(b.truncated());
        assert_eq!(b.sample_count(), 8000);
        assert_eq!(b.pcm.capacity(), 8000, "nothing reserved past the cap");
    }

    /// The first frame reserves a typical call's worth
    /// of audio, growth after that doubles, and the buffer never reserves past
    /// the cap (it used to overshoot a 30-minute cap to ~33 MB).
    #[test]
    fn the_pcm_buffer_is_sized_once_and_never_past_the_cap() {
        let mut b = builder();
        b.push_voice_frame(&[0i16; 160], false);
        let initial = b.pcm.capacity();
        assert_eq!(initial, 80_000, "10 s reserved at the first frame");
        for _ in 1..500 {
            b.push_voice_frame(&[0i16; 160], false);
        }
        assert_eq!(b.pcm.capacity(), initial, "a 10 s call never regrows");
        b.push_voice_frame(&[0i16; 160], false);
        assert_eq!(b.pcm.capacity(), 160_000, "then it doubles");

        let mut capped = builder().with_max_duration(Duration::from_secs(15));
        for _ in 0..1_000 {
            capped.push_voice_frame(&[0i16; 160], false);
        }
        assert_eq!(capped.sample_count(), 120_000);
        assert_eq!(capped.pcm.capacity(), 120_000, "doubling stops at the cap");
    }

    #[test]
    fn flags_latch() {
        let mut b = builder();
        b.set_flags(true, false);
        b.set_flags(false, false);
        let call = b.finish(1_788_012_000_223);
        assert!(call.emergency);
        assert!(!call.encrypted);
    }

    #[test]
    fn a_call_with_nothing_to_report_carries_no_quality() {
        // `push_pcm` is not a frame count, so audio pushed by a caller that
        // never vocoded anything must not invent one.
        let mut b = builder();
        b.push_pcm(&[0i16; 8000]);
        let call = b.finish(1_788_012_001_123);
        assert!(call.source_quality.is_none());
    }

    #[test]
    fn voice_frames_and_bad_frames_count_what_the_vocoder_did() {
        let mut b = builder();
        for i in 0..10 {
            // Every third frame came back muted: the vocoder could not repair
            // it.
            b.push_voice_frame(&[0i16; 160], i % 3 == 0);
        }
        b.set_instance(Some("rec-north-01".into()));
        b.set_rssi_dbfs(Some(-38.4));
        let call = b.finish(1_788_012_001_123);
        let q = call.source_quality.expect("quality was gathered");
        assert_eq!(q.voice_frames, 10);
        assert_eq!(q.bad_frames, 4);
        assert_eq!(q.instance.as_deref(), Some("rec-north-01"));
        assert_eq!(q.rssi_dbfs, Some(-38.4));
        // The frames really did become audio.
        assert_eq!(call.pcm.len(), 1600);
    }

    #[test]
    fn an_encrypted_call_still_names_its_recorder() {
        // The ciphertext never reaches the vocoder, so it reports no frames —
        // but which receiver heard it, and how hard, is still worth knowing.
        let mut b = builder();
        b.set_instance(Some("rec-north-01".into()));
        b.set_rssi_dbfs(Some(-51.0));
        b.set_encryption(Some("AES-256".into()), Some(42));
        let call = b.finish(1_788_012_001_123);
        let q = call.source_quality.expect("quality was gathered");
        assert_eq!(q.voice_frames, 0);
        assert_eq!(q.instance.as_deref(), Some("rec-north-01"));
    }

    #[test]
    fn an_empty_instance_or_a_nan_level_is_not_a_measurement() {
        let mut b = builder();
        b.set_instance(Some(String::new()));
        b.set_rssi_dbfs(Some(f64::NAN));
        assert!(b.quality().is_empty());
        assert!(b.finish(1_788_012_001_123).source_quality.is_none());
    }

    #[test]
    fn quality_survives_a_serde_round_trip() {
        let q = SourceQuality {
            instance: Some("rec-north-01".into()),
            voice_frames: 1234,
            bad_frames: 7,
            rssi_dbfs: Some(-38.4),
        };
        let text = serde_json::to_string(&q).expect("serializes");
        assert_eq!(
            text,
            r#"{"instance":"rec-north-01","voice_frames":1234,"bad_frames":7,"rssi_dbfs":-38.4}"#
        );
        let back: SourceQuality = serde_json::from_str(&text).expect("deserializes");
        assert_eq!(back, q);
    }

    #[test]
    fn a_quality_object_written_by_another_version_still_loads() {
        // Every member defaults, so a copy that omits what it could not source
        // — and one that carries a key this build has never heard of — both
        // load rather than failing the whole call.
        let q: SourceQuality =
            serde_json::from_str(r#"{"voice_frames":12,"gap_ms":40}"#).expect("deserializes");
        assert_eq!(q.voice_frames, 12);
        assert_eq!(q.bad_frames, 0);
        assert_eq!(q.instance, None);
        assert_eq!(q.rssi_dbfs, None);
        assert!(!q.is_empty());
        assert!(SourceQuality::default().is_empty());
    }

    #[test]
    fn a_call_serialized_before_the_field_existed_still_loads() {
        // The spool sidecar is written by whichever binary was running, and an
        // upgrade must not strand what the old one left on disk.
        let mut b = builder();
        b.push_voice_frame(&[1i16; 160], false);
        let call = b.finish(1_788_012_001_123);
        let mut v = serde_json::to_value(&call).expect("serializes");
        assert!(v.get("source_quality").is_some());
        v.as_object_mut()
            .expect("an object")
            .remove("source_quality");
        let old: FinishedCall = serde_json::from_value(v).expect("an older call still loads");
        assert!(old.source_quality.is_none());
        assert_eq!(old.talkgroup, 52101);
    }

    /// `n` samples alternating +/- `amp`: a square wave, whose RMS is exactly
    /// `amp`, so a test can name the level it wants in dBFS.
    fn square(n: usize, amp: i16) -> Vec<i16> {
        (0..n)
            .map(|i| if i % 2 == 0 { amp } else { -amp })
            .collect()
    }

    #[test]
    fn speech_seconds_counts_only_the_frames_somebody_is_talking_in() {
        // Silence is silence, and so is a level just under the floor: -40 dBFS
        // of full scale is 327.68, so 327 is out and 328 is in.
        assert_eq!(speech_seconds(&[0i16; 8000]), 0.0);
        assert_eq!(speech_seconds(&square(8000, 327)), 0.0);
        assert_eq!(speech_seconds(&square(8000, 328)), 1.0);

        // A 30 ms mic click in half a second of silence: it lights the frame it
        // fills and the one it spills into, and nothing else. 0.04 s — which is
        // what the calls that started this gate measured.
        let mut pcm = vec![0i16; 4000];
        pcm[..240].copy_from_slice(&square(240, 4000));
        assert_eq!(speech_seconds(&pcm), 0.04);

        // A second of -20 dBFS (3277 of full scale) is a second of speech.
        assert_eq!(speech_seconds(&square(8000, 3277)), 1.0);

        // Speech-like: three 400 ms bursts with 200 ms of silence between them.
        // 1.2 s of audio, and the gaps cost nothing.
        let mut pcm = Vec::new();
        for _ in 0..3 {
            pcm.extend_from_slice(&square(3200, 4000));
            pcm.extend_from_slice(&[0i16; 1600]);
        }
        assert!(
            (speech_seconds(&pcm) - 1.2).abs() < 1e-6,
            "got {}",
            speech_seconds(&pcm)
        );

        // Shorter than one frame is not measurable, and never a panic.
        assert_eq!(speech_seconds(&[]), 0.0);
        assert_eq!(speech_seconds(&square(159, 20_000)), 0.0);
    }

    #[test]
    fn a_finished_call_carries_its_speech_seconds_and_an_encrypted_one_carries_none() {
        let mut b = builder();
        b.push_pcm(&square(8000, 4000));
        assert_eq!(b.finish(1_788_012_001_123).speech_s, 1.0);

        // Encrypted calls drop their audio, so there is nothing to measure.
        let mut b = builder();
        b.set_encryption(Some("AES-256".into()), Some(42));
        b.push_pcm(&square(8000, 4000));
        assert_eq!(b.finish(1_788_012_001_123).speech_s, 0.0);
    }

    #[test]
    fn sanitize_never_yields_an_empty_or_escaping_name() {
        assert_eq!(sanitize("../../etc"), "etc");
        assert_eq!(sanitize("///"), "call");
        assert_eq!(sanitize("Example  County"), "Example-County");
    }
}
