// SPDX-License-Identifier: GPL-3.0-or-later
//! `trunkd-analog` — analog conventional-channel receiver (NBFM and AM).
//!
//! One channel engine takes the 25 ksps complex baseband that the device's
//! shared channel bank produces for one conventional frequency and turns it
//! into calls: squelched 8 kHz mono PCM with call boundaries, gated by
//! carrier/SNR and, for NBFM, optionally by a CTCSS tone or DCS code
//! ([`tone::Tone`]). AM ([`Modulation::Am`], e.g. the VHF airband) shares the
//! whole path except the demodulator: an envelope detector with a
//! carrier-tracking AGC instead of the FM discriminator, no de-emphasis and
//! no tone gate.
//!
//! # Engine contract
//!
//! The engine (`NbfmChannel`, built from [`ChannelParams`]) is driven on the
//! device's DSP thread:
//!
//! ```text
//! NbfmChannel::new(params: ChannelParams) -> NbfmChannel
//! NbfmChannel::process(&mut self, iq: &[Complex<f32>], at_ms: u64, sink: &mut dyn FnMut(ChannelEvent))
//! NbfmChannel::finish(&mut self, at_ms: u64, sink: &mut dyn FnMut(ChannelEvent))   // EndReason::Shutdown
//! NbfmChannel::stats(&self) -> ChannelStats
//! ```
//!
//! - `iq` is at [`INPUT_RATE_HZ`], unit full scale (`|x| = 1.0` is 0 dBFS),
//!   any block length. `at_ms` is the stream time of `iq[0]` in ms; event
//!   times are derived from it plus the samples consumed.
//! - Events for one channel form the sequence
//!   `(Started Pcm* Ended)*`: every [`ChannelEvent::Started`] is followed by
//!   zero or more [`ChannelEvent::Pcm`] and exactly one
//!   [`ChannelEvent::Ended`] before the next `Started`.
//! - Only calls that are kept are announced. Audio is held back while the
//!   squelch is opening (tone not yet confirmed, call shorter than
//!   [`SquelchParams::min_call_ms`]); a carrier that fails the tone check or a
//!   kerchunk shorter than `min_call_ms` produces no events at all and is only
//!   counted in [`ChannelStats`]. When a call is confirmed, the held audio is
//!   flushed, so the PCM starts at carrier-up, not at tone lock.
//! - At [`SquelchParams::max_call_s`] the call ends with
//!   [`EndReason::MaxDuration`] and the next call starts at the same instant
//!   with no audio lost.
//!
//! Module map: [`tone`] (tone types and tables), [`golay`] (Golay(23,12) for
//! DCS), [`fm`] (filters, discriminator, resampler), [`squelch`] (power
//! squelch), [`ctcss`] / [`dcs`] (tone detectors), [`segment`] (call
//! segmentation), and `testsig` (feature `testsig`: synthetic signals for
//! tests).

pub mod ctcss;
pub mod dcs;
pub mod fm;
pub mod golay;
pub mod segment;
pub mod squelch;
#[cfg(feature = "testsig")]
pub mod testsig;
pub mod tone;

pub use segment::ToneDetector;
pub use tone::{Tone, ToneError};

use num_complex::Complex;

/// Sample rate of the engine's input IQ, Hz (one channel-bank output).
pub const INPUT_RATE_HZ: u32 = 25_000;

/// Sample rate of the sub-audio path (CTCSS/DCS detectors), Hz: the
/// discriminator output low-passed at 300 Hz and decimated by 10.
pub const SUBAUDIO_RATE_HZ: u32 = 2_500;

/// Sample rate of the PCM in [`ChannelEvent::Pcm`], Hz.
pub const AUDIO_RATE_HZ: u32 = 8_000;

/// PCM samples in one 20 ms frame (the unit the monitor hub and the IMBE
/// path use).
pub const FRAME_SAMPLES: usize = 160;

/// Maximum PCM samples in one [`ChannelEvent::Pcm`] (100 ms, 5 frames).
pub const PCM_BATCH_SAMPLES: usize = 800;

/// Channel bandwidth class. Sets the channel filter and the discriminator's
/// full-scale deviation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum Bandwidth {
    /// 12.5 kHz channel, ±2.5 kHz peak deviation.
    #[default]
    Narrow,
    /// 25 kHz channel, ±5 kHz peak deviation. Reserved: the engine runs it,
    /// but the recorder's channel bank delivers a 12.5 kHz slice per channel,
    /// so the recorder's config refuses 25 kHz channels for now.
    Wide,
}

impl Bandwidth {
    /// From a configured `bandwidth_hz`; only 12500 and 25000 are valid.
    pub fn from_hz(hz: u32) -> Option<Bandwidth> {
        match hz {
            12_500 => Some(Bandwidth::Narrow),
            25_000 => Some(Bandwidth::Wide),
            _ => None,
        }
    }

    /// Channel spacing, Hz (12500 or 25000).
    pub fn hz(self) -> u32 {
        match self {
            Bandwidth::Narrow => 12_500,
            Bandwidth::Wide => 25_000,
        }
    }

    /// Peak deviation that maps to discriminator output ±1.0, Hz.
    pub fn max_deviation_hz(self) -> f32 {
        match self {
            Bandwidth::Narrow => 2_500.0,
            Bandwidth::Wide => 5_000.0,
        }
    }

    /// One-sided passband edge of the channel filter, Hz. The stopband
    /// starts 2.5 kHz further out. Narrow: 4 kHz / 6.5 kHz, so a strong
    /// neighbour 7.5 kHz away (VHF offset channels) is ≥ 75 dB down at its
    /// carrier and its splatter cannot open the squelch, while the ±2.5 kHz
    /// deviation of the wanted signal (Carson bandwidth ±5.5 kHz) passes
    /// with the peak-deviation measurement within a few percent.
    pub fn passband_hz(self) -> f32 {
        match self {
            Bandwidth::Narrow => 4_000.0,
            Bandwidth::Wide => 9_000.0,
        }
    }
}

/// How a channel's carrier is modulated (`[[channel]] mode`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum Modulation {
    /// Narrowband FM: discriminator, de-emphasis, optional CTCSS/DCS gate.
    #[default]
    Fm,
    /// Double-sideband AM with carrier (the VHF airband): envelope detector
    /// normalised by the tracked carrier level, so the audio level follows
    /// the modulation depth rather than the signal strength. No de-emphasis,
    /// no tone gate, no tone detectors.
    Am,
}

impl Modulation {
    /// Wire spelling of the upload's `mode` field: `nbfm` / `am`.
    pub fn as_str(self) -> &'static str {
        match self {
            Modulation::Fm => "nbfm",
            Modulation::Am => "am",
        }
    }
}

/// Squelch and call-segmentation settings (`[channel.squelch]`). All times in
/// ms unless the name says otherwise. [`Default`] gives the documented
/// defaults.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SquelchParams {
    /// Open when channel power is at least this many dB above the tracked
    /// noise floor (for `attack_ms`). Default 8.0. Must exceed `close_db`.
    pub open_db: f32,
    /// Close when channel power falls to or below this many dB above the noise
    /// floor (continuously for `hang_ms`). Default 5.0.
    pub close_db: f32,
    /// How long power must stay above `open_db` before the squelch opens.
    /// Default 40.
    pub attack_ms: u32,
    /// How long power must stay below `close_db` before an open call ends; a
    /// re-key within this time continues the same call. Default 1000.
    pub hang_ms: u32,
    /// Tone-gated channels only: how long after carrier-up the configured tone
    /// must be confirmed; the held audio is discarded on timeout and
    /// [`ChannelStats::tone_mismatch`] counts it. Default 750.
    pub tone_timeout_ms: u32,
    /// Longest call, seconds; a longer transmission is split with
    /// [`EndReason::MaxDuration`]. Default 180. Must be > 0.
    pub max_call_s: u32,
    /// Shortest call kept; shorter ones are dropped without events and counted
    /// in [`ChannelStats::calls_too_short`]. Default 500.
    pub min_call_ms: u32,
}

impl Default for SquelchParams {
    fn default() -> Self {
        SquelchParams {
            open_db: 8.0,
            close_db: 5.0,
            attack_ms: 40,
            hang_ms: 1000,
            tone_timeout_ms: 750,
            max_call_s: 180,
            min_call_ms: 500,
        }
    }
}

/// Everything one channel engine needs. Labels, frequency and routing live in
/// the recorder; the engine sees only signal-processing settings.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ChannelParams {
    /// FM or AM.
    pub modulation: Modulation,
    /// Channel bandwidth class (`bandwidth_hz` 12500 / 25000).
    pub bandwidth: Bandwidth,
    /// Required tone; `None` = carrier squelch (detectors still run and report
    /// what they hear in [`CallEnd::tone_detected`]).
    pub tone: Option<Tone>,
    /// De-emphasis time constant, µs; 0 disables. Default 750.
    pub deemphasis_us: u32,
    /// Squelch and segmentation settings.
    pub squelch: SquelchParams,
    /// DCS only: require an error-free Golay word (`true`) instead of allowing
    /// single-bit correction (`false`, default).
    pub dcs_strict: bool,
}

impl Default for ChannelParams {
    /// Narrowband, carrier squelch, 750 µs de-emphasis, default squelch,
    /// single-bit DCS correction allowed.
    fn default() -> Self {
        ChannelParams {
            modulation: Modulation::Fm,
            bandwidth: Bandwidth::Narrow,
            tone: None,
            deemphasis_us: 750,
            squelch: SquelchParams::default(),
            dcs_strict: false,
        }
    }
}

impl ChannelParams {
    /// Default params with the given tone.
    pub fn with_tone(tone: Option<Tone>) -> Self {
        ChannelParams {
            tone,
            ..ChannelParams::default()
        }
    }

    /// Check the settings an engine cannot run with.
    pub fn validate(&self) -> Result<(), ParamsError> {
        if self.modulation == Modulation::Am && self.tone.is_some() {
            return Err(ParamsError::ToneOnAm);
        }
        let s = &self.squelch;
        if !s.open_db.is_finite() || !s.close_db.is_finite() {
            return Err(ParamsError::NonFinite);
        }
        if s.open_db <= s.close_db {
            return Err(ParamsError::Hysteresis {
                open_db: s.open_db,
                close_db: s.close_db,
            });
        }
        if s.max_call_s == 0 {
            return Err(ParamsError::ZeroMaxCall);
        }
        if u64::from(s.min_call_ms) >= u64::from(s.max_call_s) * 1000 {
            return Err(ParamsError::MinExceedsMax {
                min_call_ms: s.min_call_ms,
                max_call_s: s.max_call_s,
            });
        }
        Ok(())
    }
}

/// Why [`ChannelParams::validate`] refused a configuration.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum ParamsError {
    /// `open_db` / `close_db` is NaN or infinite.
    #[error("squelch open_db/close_db must be finite numbers")]
    NonFinite,
    /// `open_db` must be strictly greater than `close_db`.
    #[error("squelch open_db ({open_db}) must be greater than close_db ({close_db})")]
    Hysteresis {
        /// Configured open threshold.
        open_db: f32,
        /// Configured close threshold.
        close_db: f32,
    },
    /// `max_call_s` is zero.
    #[error("squelch max_call_s must be greater than 0")]
    ZeroMaxCall,
    /// A `tone` was set on an AM channel, which has no tone gate.
    #[error("tone squelch is NBFM only; an AM channel cannot have a tone")]
    ToneOnAm,
    /// `min_call_ms` is not shorter than `max_call_s`.
    #[error(
        "squelch min_call_ms ({min_call_ms}) must be shorter than max_call_s ({max_call_s} s)"
    )]
    MinExceedsMax {
        /// Configured minimum call length, ms.
        min_call_ms: u32,
        /// Configured maximum call length, s.
        max_call_s: u32,
    },
}

/// Why a call ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EndReason {
    /// Carrier/SNR dropped below `close_db` for `hang_ms`.
    SquelchClosed,
    /// Tone-gated channel: the configured tone went away (or another tone
    /// took over) for `hang_ms` while the carrier stayed up.
    ToneLost,
    /// The call reached `max_call_s`; a new call starts immediately.
    MaxDuration,
    /// The engine was finished (recorder stopping) with a call open.
    Shutdown,
}

impl EndReason {
    /// Wire/metrics spelling: `squelch_closed`, `tone_lost`, `max_duration`,
    /// `shutdown` (the `squelchReason` upload field).
    pub fn as_str(self) -> &'static str {
        match self {
            EndReason::SquelchClosed => "squelch_closed",
            EndReason::ToneLost => "tone_lost",
            EndReason::MaxDuration => "max_duration",
            EndReason::Shutdown => "shutdown",
        }
    }
}

impl std::fmt::Display for EndReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Start of a kept call.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CallStart {
    /// Stream time of the call's first PCM sample, ms (carrier-up, or the
    /// split instant after [`EndReason::MaxDuration`]).
    pub at_ms: u64,
    /// Channel power when the squelch opened, dBFS.
    pub power_dbfs: f32,
    /// Power above the noise floor when the squelch opened, dB.
    pub snr_db: f32,
    /// Tone detected when the call was confirmed: the configured tone on a
    /// tone-gated channel, whatever was heard (if anything yet) on carrier
    /// squelch.
    pub tone_detected: Option<Tone>,
}

/// End of a call, with its measurements (the analog extras of the keyup
/// upload contract).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CallEnd {
    /// Stream time just past the call's last PCM sample, ms. Equals
    /// `CallStart::at_ms + duration_ms`.
    pub at_ms: u64,
    /// Why the call ended.
    pub reason: EndReason,
    /// PCM delivered for the call, in ms (sample count / 8). Gaps shorter
    /// than `hang_ms` inside the call are included, so this is wall time.
    pub duration_ms: u64,
    /// Tone heard for most of the call, if any (reported on carrier squelch
    /// too). Uploaded as `toneDetected`.
    pub tone_detected: Option<Tone>,
    /// Channel power at open, dBFS (the call's RSSI).
    pub power_dbfs: f32,
    /// Power above the noise floor at open, dB. Uploaded as `snrDb`.
    pub snr_db: f32,
    /// Largest instantaneous deviation seen during the call (audio plus
    /// sub-audio, offset removed), Hz. Uploaded as `peakDeviationHz`.
    pub peak_deviation_hz: f32,
    /// Mean discriminator output while open, Hz: the carrier's offset from
    /// the channel centre (a tuning/PPM health signal). Uploaded as
    /// `offsetHz`.
    pub offset_hz: f32,
}

/// What a channel engine emits.
#[derive(Clone, Debug, PartialEq)]
pub enum ChannelEvent {
    /// A kept call started.
    Started(CallStart),
    /// Audio for the open call: mono i16 at [`AUDIO_RATE_HZ`], a whole number
    /// of [`FRAME_SAMPLES`] frames and at most [`PCM_BATCH_SAMPLES`] samples
    /// (the call's last partial frame is zero-padded).
    Pcm(Vec<i16>),
    /// The open call ended.
    Ended(CallEnd),
}

/// The engine's channel state, for status/panels.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum ChannelState {
    /// No carrier.
    #[default]
    Closed,
    /// Carrier up; waiting for the tone and/or `min_call_ms`, audio held.
    Opening,
    /// A call is open and audio is flowing.
    Open,
    /// Carrier dropped; waiting `hang_ms` for a re-key before ending the
    /// call.
    Hang,
}

impl ChannelState {
    /// Status spelling: `closed`, `opening`, `open`, `hang`.
    pub fn as_str(self) -> &'static str {
        match self {
            ChannelState::Closed => "closed",
            ChannelState::Opening => "opening",
            ChannelState::Open => "open",
            ChannelState::Hang => "hang",
        }
    }
}

/// Snapshot of one channel engine (metrics `channel.{i}.*`, panel table).
/// Counters are cumulative since the engine was built.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct ChannelStats {
    /// Current state.
    pub state: ChannelState,
    /// Current channel power after the channel filter, dBFS.
    pub power_dbfs: f32,
    /// Tracked noise floor, dBFS.
    pub noise_floor_dbfs: f32,
    /// `power_dbfs - noise_floor_dbfs`, dB.
    pub snr_db: f32,
    /// Tone the detectors currently hear, if any.
    pub tone_detected: Option<Tone>,
    /// Carrier offset measured during the last open call, Hz (0 before the
    /// first call).
    pub offset_hz: f32,
    /// Kept calls started (each `Started` event, including `max_duration`
    /// continuations).
    pub calls: u64,
    /// Openings dropped for being shorter than `min_call_ms`.
    pub calls_too_short: u64,
    /// Tone-gated channels: carrier openings dropped because the configured
    /// tone was not confirmed within `tone_timeout_ms`.
    pub tone_mismatch: u64,
    /// Carrier openings vetoed by the discriminator noise meter (energy with
    /// no FM structure, e.g. adjacent-channel splatter or intermod).
    pub false_opens: u64,
    /// Stream time of the last `Ended` event, ms.
    pub last_call_end_ms: Option<u64>,
    /// Input IQ samples processed.
    pub samples_in: u64,
}

impl ChannelStats {
    /// A call is open (`Open` or `Hang`), i.e. events are flowing.
    pub fn is_open(&self) -> bool {
        matches!(self.state, ChannelState::Open | ChannelState::Hang)
    }
}

/// One conventional NBFM channel: 25 ksps IQ in, [`ChannelEvent`]s out. See
/// the crate docs for the event contract.
///
/// Signal path per input sample: channel filter ([`fm::ChannelFilter`]) ->
/// power meter -> discriminator -> { 6–10 kHz noise meter, sub-audio path
/// (to the tone detectors), audio path (to 8 kHz PCM) }. Every
/// [`squelch::BLOCK_SAMPLES`] samples (5 ms) the block's power and noise go
/// to the [`squelch::PowerSquelch`], the sub-audio to the tone detectors,
/// and the block's 40 PCM samples with both decisions to the
/// [`segment::CallSegmenter`], which emits the events.
///
/// Allocation-free per sample; the only allocations in steady state are the
/// `Vec`s carried by [`ChannelEvent::Pcm`].
pub struct NbfmChannel {
    params: ChannelParams,
    max_dev_hz: f32,
    filter: fm::ChannelFilter,
    disc: fm::Discriminator,
    noise: fm::NoiseMeter,
    sub: fm::SubaudioChain,
    audio: fm::AudioChain,
    peak_lpf: fm::Biquad,
    squelch: squelch::PowerSquelch,
    seg: segment::CallSegmenter,
    detectors: Vec<Box<dyn ToneDetector>>,
    /// Samples in the current block.
    fill: usize,
    pow_acc: f32,
    dev_acc: f32,
    peak_acc: f32,
    /// Slow DC (carrier offset) estimate of the discriminator, normalised.
    dc: Option<f32>,
    sub_buf: [f32; SUB_PER_BLOCK_MAX],
    sub_len: usize,
    pcm_buf: Vec<i16>,
    block_index: u64,
    samples_in: u64,
    /// Stream time of input sample 0, ms (see `set_anchor`).
    anchor_ms: f64,
    anchored: bool,
    power_dbfs: f32,
    snr_db: f32,
    tone_now: Option<Tone>,
    /// AM only: tracked carrier amplitude (the AGC reference); 0 until the
    /// first sample.
    am_carrier: f32,
}

/// Sub-audio samples per squelch block, rounded up (125 / 10).
const SUB_PER_BLOCK_MAX: usize = squelch::BLOCK_SAMPLES.div_ceil(fm::SUBAUDIO_DECIMATION);

/// The tone detectors a channel runs: the one for its configured tone
/// (CTCSS or DCS), or both on carrier squelch (they report what is heard;
/// CTCSS first, it rejects DCS signals itself).
pub fn default_detectors(params: &ChannelParams) -> Vec<Box<dyn ToneDetector>> {
    let ctcss = || Box::new(ctcss::CtcssDetector::new()) as Box<dyn ToneDetector>;
    let dcs = || Box::new(dcs::DcsDetector::new(params.dcs_strict)) as Box<dyn ToneDetector>;
    if params.modulation == Modulation::Am {
        return Vec::new();
    }
    match params.tone {
        Some(t) if t.is_ctcss() => vec![ctcss()],
        Some(_) => vec![dcs()],
        None => vec![ctcss(), dcs()],
    }
}

impl NbfmChannel {
    /// Engine with the crate's tone detectors ([`default_detectors`]).
    /// `params` should pass [`ChannelParams::validate`]; the engine runs on
    /// anything, but `open_db <= close_db` makes the squelch chatter.
    pub fn new(params: ChannelParams) -> Self {
        let detectors = default_detectors(&params);
        NbfmChannel::with_detectors(params, detectors)
    }

    /// Engine with the given tone detectors (all run; the first one that
    /// hears a tone decides what is heard).
    pub fn with_detectors(params: ChannelParams, detectors: Vec<Box<dyn ToneDetector>>) -> Self {
        debug_assert!(params.validate().is_ok(), "{:?}", params.validate());
        let fs = INPUT_RATE_HZ as f32;
        let squelch = squelch::PowerSquelch::new(&params.squelch);
        let seg = segment::CallSegmenter::new(&params, squelch.attack_blocks());
        NbfmChannel {
            params,
            max_dev_hz: params.bandwidth.max_deviation_hz(),
            filter: fm::ChannelFilter::new(params.bandwidth),
            disc: fm::Discriminator::new(params.bandwidth.max_deviation_hz()),
            noise: fm::NoiseMeter::new(),
            sub: fm::SubaudioChain::new(),
            // AM audio is never pre-emphasised.
            audio: fm::AudioChain::new(match params.modulation {
                Modulation::Fm => params.deemphasis_us,
                Modulation::Am => 0,
            }),
            peak_lpf: fm::butter2_lowpass(fs, 3_000.0),
            squelch,
            seg,
            detectors,
            fill: 0,
            pow_acc: 0.0,
            dev_acc: 0.0,
            peak_acc: 0.0,
            dc: None,
            sub_buf: [0.0; SUB_PER_BLOCK_MAX],
            sub_len: 0,
            pcm_buf: Vec::with_capacity(segment::PCM_PER_BLOCK + 1),
            block_index: 0,
            samples_in: 0,
            anchor_ms: 0.0,
            anchored: false,
            power_dbfs: squelch::MIN_POWER_DBFS,
            snr_db: 0.0,
            tone_now: None,
            am_carrier: 0.0,
        }
    }

    /// The engine's settings.
    pub fn params(&self) -> &ChannelParams {
        &self.params
    }

    /// Keep the time base: re-anchor on the first call and whenever the
    /// caller's clock departs from the sample count by more than
    /// [`REANCHOR_MS`] (a gap or jump), not on the caller's ms rounding.
    fn set_anchor(&mut self, at_ms: u64) {
        let consumed_ms = self.samples_in as f64 * 1000.0 / f64::from(INPUT_RATE_HZ);
        let predicted = self.anchor_ms + consumed_ms;
        if !self.anchored || (at_ms as f64 - predicted).abs() > REANCHOR_MS {
            self.anchor_ms = at_ms as f64 - consumed_ms;
            self.anchored = true;
        }
    }

    /// Process a block of IQ at [`INPUT_RATE_HZ`]; `at_ms` is the stream
    /// time of `iq[0]`. Emits events through `sink` as calls are confirmed,
    /// stream and end.
    pub fn process(&mut self, iq: &[Complex<f32>], at_ms: u64, sink: &mut dyn FnMut(ChannelEvent)) {
        self.set_anchor(at_ms);
        for &x in iq {
            let y = self.filter.push(x);
            self.pow_acc += y.norm_sqr();
            let d = self.disc.push(y);
            self.noise.push(d);
            if let Some(s) = self.sub.push(d) {
                self.sub_buf[self.sub_len] = s;
                self.sub_len += 1;
            }
            let audio = match self.params.modulation {
                Modulation::Fm => d,
                Modulation::Am => self.am_demod(y),
            };
            self.audio.push(audio, &mut self.pcm_buf);
            let lp = self.peak_lpf.push(d);
            self.dev_acc += d;
            if let Some(dc) = self.dc {
                self.peak_acc = self.peak_acc.max((lp - dc).abs());
            }
            self.fill += 1;
            if self.fill == squelch::BLOCK_SAMPLES {
                self.end_block(sink);
            }
        }
        self.samples_in += iq.len() as u64;
    }

    fn end_block(&mut self, sink: &mut dyn FnMut(ChannelEvent)) {
        let n = squelch::BLOCK_SAMPLES as f32;
        let power = squelch::power_dbfs(self.pow_acc / n);
        let noise_rms = self.noise.take_rms();
        let sq = self.squelch.update(power, noise_rms);
        self.power_dbfs = sq.power_dbfs;
        self.snr_db = sq.snr_db;

        if sq.run_started {
            for d in self.detectors.iter_mut() {
                d.reset();
            }
        }
        let sub = &self.sub_buf[..self.sub_len];
        for d in self.detectors.iter_mut() {
            d.push(sub);
        }
        self.tone_now = self.detectors.iter().find_map(|d| d.current());

        let mean = self.dev_acc / n;
        let dc = *self.dc.get_or_insert(mean);
        self.dc = Some(dc + DC_ALPHA * (mean - dc));
        let measure = segment::BlockMeasure {
            // The discriminator still measures an AM carrier's offset, but
            // "deviation" means nothing for AM.
            mean_dev_hz: mean * self.max_dev_hz,
            peak_dev_hz: match self.params.modulation {
                Modulation::Fm => self.peak_acc * self.max_dev_hz,
                Modulation::Am => 0.0,
            },
        };
        debug_assert_eq!(self.pcm_buf.len(), segment::PCM_PER_BLOCK);
        let inp = segment::BlockInput {
            index: self.block_index,
            pcm: &self.pcm_buf,
            squelch: sq,
            tone: self.tone_now,
            measure,
        };
        self.seg.block(&inp, self.anchor_ms, sink);

        self.audio.flush_denormals();
        self.sub.flush_denormals();
        self.peak_lpf.flush_denormals();
        self.pcm_buf.clear();
        self.sub_len = 0;
        self.fill = 0;
        self.pow_acc = 0.0;
        self.dev_acc = 0.0;
        self.peak_acc = 0.0;
        self.block_index += 1;
    }

    /// AM envelope detector with AGC: the modulation index `|y| / carrier - 1`,
    /// where `carrier` follows the envelope with a ~20 ms one-pole. Voice sits
    /// above the audio chain's 300 Hz high-pass, far above the tracker's
    /// ~8 Hz corner, so the tracker removes the carrier and leaves the voice;
    /// 100 % modulation comes out at ±1.0, the same scale as a full-scale FM
    /// deviation.
    #[inline]
    fn am_demod(&mut self, y: Complex<f32>) -> f32 {
        let env = y.norm();
        if self.am_carrier <= 0.0 {
            self.am_carrier = env;
        }
        // Rising fast lets a new transmission settle in a few ms instead of
        // blasting its first 50 ms at the noise floor's gain.
        let alpha = if env > 2.0 * self.am_carrier {
            AM_CARRIER_ATTACK
        } else {
            AM_CARRIER_ALPHA
        };
        self.am_carrier += alpha * (env - self.am_carrier);
        let c = self.am_carrier.max(AM_MIN_CARRIER);
        (env / c - 1.0).clamp(-1.5, 1.5)
    }

    /// The recorder is stopping at stream time `at_ms`: an open call ends
    /// with [`EndReason::Shutdown`] (its `at_ms` is, as always, start plus
    /// the audio delivered); an unconfirmed opening is dropped. The partial
    /// block in progress is not processed.
    pub fn finish(&mut self, at_ms: u64, sink: &mut dyn FnMut(ChannelEvent)) {
        self.set_anchor(at_ms);
        self.seg.finish(sink);
    }

    /// Snapshot for status and metrics.
    pub fn stats(&self) -> ChannelStats {
        let c = self.seg.counters();
        ChannelStats {
            state: self.seg.state(),
            power_dbfs: self.power_dbfs,
            noise_floor_dbfs: self.squelch.floor_dbfs(),
            snr_db: self.snr_db,
            tone_detected: self.tone_now,
            offset_hz: c.last_offset_hz,
            calls: c.calls,
            calls_too_short: c.calls_too_short,
            tone_mismatch: c.tone_mismatch,
            false_opens: self.squelch.false_opens(),
            last_call_end_ms: c.last_call_end_ms,
            samples_in: self.samples_in,
        }
    }
}

/// Largest disagreement between the caller's `at_ms` and the engine's own
/// sample clock that is treated as rounding, ms.
const REANCHOR_MS: f64 = 2.0;

/// AM carrier tracker: one-pole coefficient at 25 ksps for a ~20 ms time
/// constant (1 / (0.02 × 25 000)).
const AM_CARRIER_ALPHA: f32 = 0.002;

/// AM carrier tracker while the envelope is more than twice the estimate (a
/// carrier coming up): ~1 ms.
const AM_CARRIER_ATTACK: f32 = 0.04;

/// Floor for the AM carrier estimate, so silence cannot divide by zero
/// (−120 dBFS amplitude).
const AM_MIN_CARRIER: f32 = 1e-6;

/// One-pole smoothing of the per-block discriminator mean (≈ 50 ms), the
/// DC reference for the peak-deviation measurement.
const DC_ALPHA: f32 = 0.1;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bandwidth_from_hz() {
        assert_eq!(Bandwidth::from_hz(12_500), Some(Bandwidth::Narrow));
        assert_eq!(Bandwidth::from_hz(25_000), Some(Bandwidth::Wide));
        assert_eq!(Bandwidth::from_hz(6_250), None);
        for b in [Bandwidth::Narrow, Bandwidth::Wide] {
            assert_eq!(Bandwidth::from_hz(b.hz()), Some(b));
            assert!(b.passband_hz() < b.hz() as f32 / 2.0 + 3_500.0);
        }
    }

    #[test]
    fn default_params_validate() {
        let p = ChannelParams::default();
        assert_eq!(p.deemphasis_us, 750);
        assert_eq!(p.squelch.open_db, 8.0);
        assert_eq!(p.tone, None);
        assert_eq!(p.validate(), Ok(()));
        let t: Tone = "D445N".parse().unwrap();
        assert_eq!(ChannelParams::with_tone(Some(t)).tone, Some(t));
    }

    #[test]
    fn validate_rejects_bad_squelch() {
        let mut p = ChannelParams::default();
        p.squelch.close_db = 8.0;
        assert!(matches!(p.validate(), Err(ParamsError::Hysteresis { .. })));
        p.squelch.open_db = f32::NAN;
        assert_eq!(p.validate(), Err(ParamsError::NonFinite));
        let mut p = ChannelParams::default();
        p.squelch.max_call_s = 0;
        assert_eq!(p.validate(), Err(ParamsError::ZeroMaxCall));
        let mut p = ChannelParams::default();
        p.squelch.max_call_s = 1;
        p.squelch.min_call_ms = 1000;
        assert!(matches!(
            p.validate(),
            Err(ParamsError::MinExceedsMax { .. })
        ));
    }

    /// Deterministic complex noise (xorshift), unit-variance-ish per rail.
    struct Noise(u64);
    impl Noise {
        fn next(&mut self) -> f32 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 >> 40) as f32 / (1u64 << 24) as f32 * 2.0 - 1.0
        }
        fn c(&mut self, amp: f32) -> Complex<f32> {
            Complex::new(self.next() * amp, self.next() * amp)
        }
    }

    /// `secs` of 25 ksps IQ: noise throughout, and from `on.0` to `on.1`
    /// seconds an AM carrier of amplitude `carrier`, 300 Hz off centre,
    /// modulated `depth` by a 1 kHz tone.
    fn am_iq(secs: f32, on: (f32, f32), carrier: f32, depth: f32) -> Vec<Complex<f32>> {
        let fs = INPUT_RATE_HZ as f32;
        let mut n = Noise(0x9e37_79b9_7f4a_7c15);
        (0..(secs * fs) as usize)
            .map(|i| {
                let t = i as f32 / fs;
                let mut x = n.c(1e-4);
                if t >= on.0 && t < on.1 {
                    let env = carrier * (1.0 + depth * (std::f32::consts::TAU * 1_000.0 * t).sin());
                    let ph = std::f32::consts::TAU * 300.0 * t;
                    x += Complex::new(ph.cos(), ph.sin()) * env;
                }
                x
            })
            .collect()
    }

    /// Run an engine over `iq` in 10 ms blocks; return (calls, all PCM).
    fn run_am(iq: &[Complex<f32>]) -> (usize, Vec<i16>) {
        let params = ChannelParams {
            modulation: Modulation::Am,
            ..ChannelParams::default()
        };
        assert_eq!(params.validate(), Ok(()));
        let mut e = NbfmChannel::new(params);
        let (mut calls, mut pcm) = (0, Vec::new());
        for (k, chunk) in iq.chunks(250).enumerate() {
            e.process(chunk, k as u64 * 10, &mut |ev| match ev {
                ChannelEvent::Started(_) => calls += 1,
                ChannelEvent::Pcm(p) => pcm.extend(p),
                ChannelEvent::Ended(_) => {}
            });
        }
        (calls, pcm)
    }

    /// Goertzel power of `f` Hz in 8 kHz PCM, normalised to full scale.
    fn tone_level(pcm: &[i16], f: f32) -> f32 {
        let w = std::f32::consts::TAU * f / AUDIO_RATE_HZ as f32;
        let (mut s1, mut s2) = (0.0f32, 0.0f32);
        for &x in pcm {
            let s0 = x as f32 / i16::MAX as f32 + 2.0 * w.cos() * s1 - s2;
            s2 = s1;
            s1 = s0;
        }
        (s1 * s1 + s2 * s2 - 2.0 * w.cos() * s1 * s2).sqrt() / pcm.len() as f32 * 2.0
    }

    #[test]
    fn am_channel_records_the_modulation_at_a_level_set_by_depth_not_strength() {
        let mut levels = Vec::new();
        for carrier in [0.01f32, 0.1] {
            let (calls, pcm) = run_am(&am_iq(4.0, (0.5, 3.0), carrier, 0.8));
            assert_eq!(
                calls, 1,
                "one call for one transmission (carrier {carrier})"
            );
            assert!(
                pcm.len() > 8_000 * 2,
                "about 2.5 s of audio, got {}",
                pcm.len()
            );
            // A settled second from the middle of the transmission.
            let mid = &pcm[4_000..12_000];
            let on = tone_level(mid, 1_000.0);
            let off = tone_level(mid, 1_700.0);
            assert!(on > 0.3, "1 kHz tone at 80 % depth comes through: {on}");
            assert!(on > 20.0 * off, "and dominates the audio: {on} vs {off}");
            levels.push(on);
        }
        // 20 dB stronger signal, same modulation depth: same audio level.
        assert!((levels[0] / levels[1] - 1.0).abs() < 0.1, "AGC: {levels:?}");
    }

    #[test]
    fn am_channel_stays_closed_on_noise_and_refuses_a_tone() {
        let (calls, pcm) = run_am(&am_iq(3.0, (9.0, 9.0), 0.0, 0.0));
        assert_eq!((calls, pcm.len()), (0, 0), "noise alone opens nothing");
        assert!(default_detectors(&ChannelParams {
            modulation: Modulation::Am,
            ..ChannelParams::default()
        })
        .is_empty());
        let p = ChannelParams {
            modulation: Modulation::Am,
            tone: Some("100.0".parse().unwrap()),
            ..ChannelParams::default()
        };
        assert_eq!(p.validate(), Err(ParamsError::ToneOnAm));
        assert_eq!(Modulation::Am.as_str(), "am");
        assert_eq!(Modulation::Fm.as_str(), "nbfm");
    }

    #[test]
    fn spellings() {
        assert_eq!(EndReason::MaxDuration.to_string(), "max_duration");
        assert_eq!(EndReason::SquelchClosed.as_str(), "squelch_closed");
        assert_eq!(EndReason::ToneLost.as_str(), "tone_lost");
        assert_eq!(EndReason::Shutdown.as_str(), "shutdown");
        assert_eq!(ChannelState::Opening.as_str(), "opening");
        let mut s = ChannelStats::default();
        assert!(!s.is_open());
        s.state = ChannelState::Hang;
        assert!(s.is_open());
    }
}
