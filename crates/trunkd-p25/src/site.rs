// SPDX-License-Identifier: GPL-3.0-or-later
//! [`SiteEngine`] — one wideband IQ stream in, per-call voice-frame streams out.
//!
//! This is the recorder-facing assembly of everything below it: the
//! [`PolyphaseChannelizerM2`](crate::dsp::channelizer::PolyphaseChannelizerM2) splits the capture once (O(1) in the number of
//! followed channels, spec §3.6), a control-role [`P25P1Channel`] decodes the
//! trunking stream, and each grant spawns a traffic-role channel plus a
//! [`CallAssembler`] for the duration of the call (spec §13).
//!
//! # Who owns the channelizer
//!
//! The filterbank lives in a [`ChannelBank`], and an engine is one consumer
//! of it. Two ways to build one:
//!
//! - [`SiteEngine::new`] builds a bank of its own. [`SiteEngine::process_block`]
//!   then runs that bank over each wideband block and decodes — the
//!   one-site-per-capture shape (tools, tests, a single-site tuner).
//! - [`SiteEngine::with_bank`] attaches to a bank the caller owns and shares
//!   with other engines on the same capture — the recorder's shape, one bank
//!   per device and one engine per `[[site]]` on it. The caller runs
//!   [`ChannelBank::process_block`] once per block and then
//!   [`SiteEngine::process_channels`] on every engine; the filterbank's cost
//!   is paid once for the device instead of once per site.
//!
//! Either way the engine subscribes only what it decodes and releases it as
//! before, and a channel two engines both want is produced once (see
//! [`crate::dsp::bank`]).
//!
//! ```no_run
//! use trunkd_p25::site::{SiteConfig, SiteEngine, SiteEvent};
//!
//! let mut engine = SiteEngine::new(SiteConfig {
//!     sample_rate: 20_000_000,
//!     center_hz: 855_500_000.0,
//!     control_frequencies: vec![851_050_000, 852_137_500],
//!     ..SiteConfig::default()
//! })?;
//! # let wideband: Vec<num_complex::Complex<f32>> = Vec::new();
//! engine.process_block(&wideband, &mut |ev: SiteEvent| println!("{ev:?}"));
//! engine.shutdown(&mut |ev: SiteEvent| println!("{ev:?}"));
//! # Ok::<(), trunkd_p25::P25Error>(())
//! ```
//!
//! # Time
//!
//! Every `at_ms` is **stream time**: consumed samples × 1000 / `sample_rate`.
//! The caller maps it to wall clock. Each decoder's own millisecond clock is
//! seeded from the same stream clock when it is created
//! ([`P25P1Channel::set_clock_ms`]), which is what lets the site's channels
//! share one [`TrunkingTracker`] — its staleness and hand-off rules compare
//! timestamps produced by *different* channels.
//!
//! # Control acquisition
//!
//! Extra channelizer subscriptions are nearly free, so every configured
//! control candidate is subscribed and decoded at once; the first one to yield
//! [`ACQUIRE_TSBKS`] valid TSBKs under a consistent NAC wins and the rest are
//! dropped. A candidate that happens to be carrying a voice call decodes
//! plenty of NIDs under the site's NAC but no TSBKs at all, so it can never
//! win the hunt.
//!
//! # Allocation
//!
//! Steady state allocates nothing beyond per-call setup: the channelizer drain
//! buffer and the per-block event buffer are reused, and the only per-call
//! allocations are the traffic decoder's filters and its assembler.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use num_complex::Complex;

use crate::alias::{AliasMemory, TalkerAliasAssembler, ALIAS_BLOCK_LEN};
use crate::call::{is_alias_lc, CallAssembler, CallRecord};
use crate::dsp::bank::{BankHandle, ChannelBank, SharedChannelBank};
use crate::dsp::carrier::CarrierMeter;
use crate::dsp::channelizer::{ChannelizerError, BIN_BANDWIDTH_HZ, DEFAULT_TAPS_PER_CHANNEL};
use crate::dsp::DemodMode;
use crate::event::{CallKind, P25Event};
use crate::inventory::PatchBook;
use crate::payload::LinkControl;
use crate::trunking::{SharedTracker, TrunkingTracker, DEFAULT_POOL_SIZE};
use crate::{P25Error, P25P1Channel};

/// Valid TSBKs a control candidate must decode before it is accepted.
pub const ACQUIRE_TSBKS: u64 = 5;

/// Stream-time interval between [`SiteEvent::Stats`] emissions.
pub const STATS_INTERVAL_MS: u64 = 5_000;

/// How far the strongest control candidate must stand above the weakest before
/// [`SiteStats::hunt_carrier_offset_hz`] is reported at all.
///
/// With nothing on the air every candidate bin reads the same noise floor and
/// its "carrier offset" is whatever the noise happened to average to. Six dB of
/// spread is the cheapest available proof that at least one bin has a real
/// signal in it. The cost of this rule is that a site configured with a *single*
/// control candidate has no floor to compare against and so never reports a
/// hunt offset; it is corrected from the locked channel instead.
pub const HUNT_CARRIER_MIN_SNR_DB: f64 = 6.0;

/// Which channelizer bin counts as "the noise floor" for
/// [`SiteStats::noise_floor_dbfs`].
///
/// The 20th percentile of a few hundred bins: low enough that every occupied
/// channel in the capture (the control channel, every busy traffic channel,
/// the DC bin's LO spike) is above it and cannot pull it up, high enough that
/// it is still an average of many bins rather than the single luckiest one.
pub const NOISE_FLOOR_PERCENTILE: f64 = 20.0;

/// Candidates within this many dB of the strongest one are folded into
/// [`SiteStats::hunt_carrier_offset_hz`]. A simulcast site puts several real
/// control candidates within a few dB of each other and they share one LO
/// error, so averaging them is a better estimate than picking the loudest;
/// 10 dB down is far enough to still be signal rather than floor.
pub const HUNT_CARRIER_WINDOW_DB: f64 = 10.0;

/// Configuration for one [`SiteEngine`].
#[derive(Debug, Clone)]
pub struct SiteConfig {
    /// Wideband capture rate in Hz. Must be an exact multiple of 12.5 kHz and
    /// yield an even bin count ≥ 4 (20 MHz, 12.5 MHz and 10 MHz all qualify).
    pub sample_rate: u32,
    /// Capture centre frequency in Hz.
    pub center_hz: f64,
    /// Control-channel candidates, in preference order. Candidates outside the
    /// channelizer's usable span (or on the DC bin) are dropped at
    /// construction; see [`SiteEngine::candidates`].
    pub control_frequencies: Vec<u64>,
    /// Demodulator: LSM (simulcast, the default) or C4FM.
    pub mode: DemodMode,
    /// Maximum number of concurrently followed traffic channels.
    pub traffic_pool_size: usize,
    /// Channelizer prototype length per polyphase arm (clamped to ≥ 2).
    pub taps_per_channel: usize,
    /// How long a traffic subscription is kept alive after a call ends, so a
    /// follow-on transmission on the same frequency is caught in full.
    pub hang_ms: u64,
    /// A call with no voice/signaling activity for this long ends with
    /// [`EndReason::Inactivity`].
    pub call_inactivity_ms: u64,
    /// No valid TSBK or NID on the control channel for this long is a loss:
    /// the site tears down and re-hunts.
    pub control_loss_ms: u64,
    /// Pass every control-channel [`P25Event`] through as
    /// [`SiteEvent::Control`] (logging/diagnostics; off by default).
    pub raw_control: bool,
    /// Pass every **traffic**-channel event through as
    /// [`SiteEvent::Traffic`] (off by default).
    ///
    /// Voice is never passed: [`P25Event::ImbeFrame`] is dropped at the source
    /// rather than filtered downstream, so no consumer of this stream can
    /// receive vocoder bits however it is written. Voice reaches the recorder
    /// only through [`SiteEvent::CallVoice`], which is what the call path
    /// consumes.
    pub raw_traffic: bool,
    /// What this tuner does for its site (multi-tuner core, W1).
    pub role: EngineRole,
    /// Downlink ranges this tuner is responsible for recording, `[lo, hi]` in
    /// Hz. Empty means the whole usable span. A grant on a frequency outside
    /// every range is *not* followed locally: an `Auto`/`Control` front with a
    /// grant publisher emits it as [`SiteEvent::GrantOut`] for another tuner.
    pub bands: Vec<(u64, u64)>,
    /// When set, an `Auto` front publishes grants it is not responsible for
    /// (out of band, or outside its capture) as [`SiteEvent::GrantOut`] instead
    /// of counting them out-of-band. `false` reproduces the single-tuner path
    /// exactly: a grant is followed locally or counted, never published.
    pub publish_grants: bool,
    /// Base offset added to this engine's call ids, so two tuners serving one
    /// site never mint the same [`CallId`]. `0` (the default) reproduces the
    /// single-tuner id sequence 1, 2, 3, …
    pub call_id_base: u64,
    /// Follow SNDCP data-channel grants on an `Auto` front (opt-in, default
    /// **off**). Off keeps the single downlink tuner on the voice fast path:
    /// data grants are surfaced as radio events but no channelizer slot is spent
    /// reassembling PDUs. On, an in-span data grant is followed and its PDUs are
    /// reassembled and mined for LRRP.
    ///
    /// NB on a **downlink** capture this yields LRRP *transport* detection and
    /// PDU logging but **not** coordinates: the observed reference-system data PDUs are all
    /// outbound (network→radio) polls; the radio's GPS response is inbound
    /// (uplink), which a downlink-only tuner never hears. Decoding real
    /// positions needs an uplink data-channel capture (W5). A dedicated
    /// [`EngineRole::Data`] tuner always follows regardless of this flag.
    pub data_follow: bool,
    /// Follow unit-to-unit (private) voice grants carried as multi-block AMBT
    /// messages (`[recorder] follow_unit_calls`, default **on**).
    ///
    /// The measured site sends its private-call setup this way — see
    /// [`UnitGrantAmbt`](crate::message::UnitGrantAmbt) — and a private call is
    /// ordinary clear voice on an ordinary granted channel, so it competes for
    /// a traffic slot on the same terms as an unpatched group call. Off leaves
    /// the AMBT decoded and logged but never followed, which is the setting for
    /// a recorder whose operator does not want two-party conversations on the
    /// archive.
    pub follow_unit_calls: bool,
    /// Talkgroups this recorder does not record at all: no tuner, no call, no
    /// upload, and nothing published for another tuner to record either
    /// (`[recorder] skip_talkgroups`, unioned with the list keyup pushes as
    /// `recorder.skip_talkgroups`). `None` — the default — skips nothing.
    ///
    /// Shared, not copied: keyup can change the list at any moment and the
    /// engine must honour the change on the very next grant, without a restart.
    /// The grant is refused in [`SiteEngine::on_traffic_request`], which is
    /// before a channelizer slot is subscribed and before the grant would be
    /// published; the control-channel events for it — the grant itself, the
    /// affiliations around it — are decoded and emitted exactly as before, so
    /// the archive still knows the radio keyed up.
    pub skip_talkgroups: Option<Arc<trunkd_core::SkipList>>,
}

/// What a tuner does for its site (ARCHITECTURE multi-tuner core, W1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EngineRole {
    /// Control **and** voice in one channelizer: today's combined behavior.
    #[default]
    Auto,
    /// Control-channel follow only. Publishes every grant to the coordinator
    /// (via [`SiteEvent::GrantOut`]); records no traffic itself.
    Control,
    /// Traffic grabber only: no control hunt. Follows grants handed to it
    /// through [`SiteEngine::follow`].
    Voice,
    /// Reserved for the SNDCP data follower. Routed to like `Voice`, but the
    /// decode path is a stub: a follow is counted and dropped.
    Data,
}

impl Default for SiteConfig {
    fn default() -> Self {
        SiteConfig {
            sample_rate: 20_000_000,
            center_hz: 855_500_000.0,
            control_frequencies: Vec::new(),
            mode: DemodMode::Lsm,
            traffic_pool_size: DEFAULT_POOL_SIZE as usize,
            taps_per_channel: DEFAULT_TAPS_PER_CHANNEL,
            hang_ms: 2_000,
            call_inactivity_ms: 3_000,
            control_loss_ms: 5_000,
            raw_control: false,
            raw_traffic: false,
            role: EngineRole::Auto,
            bands: Vec::new(),
            publish_grants: false,
            call_id_base: 0,
            data_follow: false,
            follow_unit_calls: true,
            skip_talkgroups: None,
        }
    }
}

/// Identifies one call for the life of the engine (monotonic, never reused).
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug, PartialOrd, Ord)]
pub struct CallId(pub u64);

/// Why a call ended.
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
pub enum EndReason {
    /// A TDU/TDULC terminator closed it (or a fresh HDU replaced it).
    Terminator,
    /// No voice or signaling for `call_inactivity_ms`.
    Inactivity,
    /// The control channel granted a different talkgroup on this frequency.
    PoolReassigned,
    /// The engine tore the call down: [`SiteEngine::shutdown`], or a control
    /// channel loss taking the whole site down.
    Shutdown,
}

/// Rolling engine counters, emitted as [`SiteEvent::Stats`] every
/// [`STATS_INTERVAL_MS`] of stream time and readable at any point via
/// [`SiteEngine::stats`].
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct SiteStats {
    /// Stream time in ms (consumed samples / sample rate).
    pub stream_ms: u64,
    /// Acquired control frequency, 0 while hunting.
    pub control_frequency_hz: u64,
    /// NAC of the acquired control channel, 0 while hunting.
    pub nac: u16,
    /// Valid TSBKs decoded since the current control channel was acquired.
    pub control_tsbks: u64,
    /// TSBKs per second over the last stats window.
    pub control_tsbk_rate: f64,
    /// Calls currently open.
    pub active_calls: usize,
    /// Traffic channels currently subscribed (calls plus hang time).
    pub pool_in_use: usize,
    /// Configured pool size.
    pub pool_size: usize,
    /// Calls started since engine construction.
    pub calls_started: u64,
    /// Calls ended since engine construction.
    pub calls_ended: u64,
    /// IMBE voice frames emitted since engine construction.
    pub voice_frames: u64,
    /// Grants that found the pool full.
    pub pool_exhausted: u64,
    /// Grants for a frequency outside the capture's usable span.
    pub out_of_band_grants: u64,
    /// Grants routed to another tuner as [`SiteEvent::GrantOut`].
    pub grants_published: u64,
    /// Calls refused because their talkgroup is on the skip list
    /// ([`SiteConfig::skip_talkgroups`]). Counted once per call, not once per
    /// control-channel grant refresh.
    pub calls_skipped_by_policy: u64,
    /// Calls refused because they were granted on a TDMA (Phase 2) channel,
    /// which this Phase 1 decoder cannot follow ([`SiteEvent::TdmaRefused`]).
    /// Counted once per call.
    pub calls_tdma_refused: u64,
    /// SNDCP data-channel follows started (opcode-20 grants that resolved
    /// in-span and were subscribed).
    pub data_grants: u64,
    /// PDUs reassembled on followed data channels since engine construction.
    pub data_pdus: u64,
    /// Framer sync losses across all channels.
    pub sync_losses: u64,
    /// Mean control-channel [`P25Event::FrequencyError`] over the last window.
    pub control_frequency_error_hz: f64,
    /// Mean traffic-channel [`P25Event::FrequencyError`] over the last window,
    /// across every followed channel.
    pub traffic_frequency_error_hz: f64,
    /// Control channels acquired since engine construction.
    pub control_acquisitions: u64,
    /// Control channels lost since engine construction.
    pub control_losses: u64,
    /// Carrier offset of the control candidates over the last window, in Hz —
    /// the measurement a closed-loop ppm correction runs on while the site is
    /// still hunting. `None` when no candidate stood out of the noise floor
    /// (see [`HUNT_CARRIER_MIN_SNR_DB`]), which includes every window before
    /// the meters have a full second of samples.
    pub hunt_carrier_offset_hz: Option<f64>,
    /// Spread between the strongest and weakest control candidate over the
    /// last window, in dB — how much to trust `hunt_carrier_offset_hz`.
    pub hunt_carrier_snr_db: f64,
    /// Carrier offset of the locked control channel over the last window, in
    /// Hz. `None` while hunting or before the meter has a full second.
    pub control_carrier_offset_hz: Option<f64>,
    /// Capture noise floor over the last window, in dBFS: the
    /// [`NOISE_FLOOR_PERCENTILE`]-th quietest channelizer bin, on the same
    /// scale as [`CarrierEstimate::power_dbfs`]. `None` before the filterbank
    /// has produced a block.
    ///
    /// [`CarrierEstimate::power_dbfs`]: crate::dsp::CarrierEstimate::power_dbfs
    pub noise_floor_dbfs: Option<f64>,
    /// Control-channel signal-to-noise ratio over the last window, in dB:
    /// the locked control channel's mean power minus `noise_floor_dbfs`.
    /// `None` while hunting, before the carrier meter has a full second, or
    /// before there is a floor to compare against.
    pub control_snr_db: Option<f64>,
    /// Share of control-channel TSBKs that passed CRC over the last window,
    /// 0-100. `None` while hunting and in any window that carried no TSBK at
    /// all — "100% of nothing" is not a quality reading.
    pub tsbk_pass_pct: Option<f64>,
    /// Control-channel framer sync losses over the last window, scaled to a
    /// minute. Zero while hunting: the counter only moves on the locked
    /// control channel (traffic and data sync losses are counted into
    /// [`SiteStats::sync_losses`] but not here — they measure a call's channel,
    /// not the tuner's grip on the site).
    pub sync_losses_per_min: f64,
}

/// Everything a [`SiteEngine`] reports.
#[derive(Debug, Clone)]
pub enum SiteEvent {
    /// The engine is decoding every listed candidate, looking for the control
    /// channel. An empty list means no configured candidate is inside the
    /// capture's usable span — the engine can never acquire.
    ControlHunting {
        /// Candidates being decoded.
        candidates: Vec<u64>,
    },
    /// A candidate produced [`ACQUIRE_TSBKS`] TSBKs under a consistent NAC.
    ControlAcquired {
        /// Winning frequency.
        frequency_hz: u64,
        /// Modal NAC seen on it.
        nac: u16,
    },
    /// No valid TSBK or NID for `control_loss_ms`; the site tears down and
    /// re-hunts.
    ControlLost {
        /// Frequency that went silent.
        frequency_hz: u64,
    },
    /// A traffic channel started following a granted call.
    CallStarted {
        /// Call id.
        id: CallId,
        /// Downlink frequency.
        frequency_hz: u64,
        /// Talkgroup (0 for unit-to-unit / telephone).
        talkgroup: u16,
        /// Full 24-bit TO address.
        target: u32,
        /// Source radio (0 when unknown at grant time).
        source: u32,
        /// Call kind.
        kind: CallKind,
        /// Member talkgroups when this call is on an active patch supergroup;
        /// empty for an ordinary call. Ascending, from the live patch map.
        patches: Vec<u32>,
        /// Stream time.
        at_ms: u64,
    },
    /// The talker changed, or the talker alias became known.
    CallTalker {
        /// Call id.
        id: CallId,
        /// Current source radio.
        source: u32,
        /// Emergency flag seen on this call so far.
        emergency: bool,
        /// L3Harris talker alias assembled so far.
        alias: Option<String>,
        /// WACN of this talker, when a Source ID Extension (LC 9) qualified
        /// it. `None` for an ordinary local radio.
        source_wacn: Option<u32>,
        /// Home system id of this talker, from the same word.
        source_system: Option<u16>,
        /// Stream time.
        at_ms: u64,
    },
    /// One raw 144-bit IMBE frame, in decode order (spec §12.1).
    CallVoice {
        /// Call id.
        id: CallId,
        /// The 144 bits, MSB-first in 18 bytes.
        frame: [u8; 18],
        /// Stream time.
        at_ms: u64,
    },
    /// The call's encryption parameters were announced or changed.
    CallEncryption {
        /// Call id.
        id: CallId,
        /// Algorithm id (`0x80` = unencrypted).
        algid: u8,
        /// Key id.
        key_id: u16,
        /// Stream time.
        at_ms: u64,
    },
    /// A call ended; `record` is the assembled metadata.
    CallEnded {
        /// Call id.
        id: CallId,
        /// Assembled call metadata.
        record: CallRecord,
        /// Stream time.
        at_ms: u64,
        /// Why it ended.
        reason: EndReason,
    },
    /// A grant was refused before anything was spent on it: its talkgroup is
    /// on the skip list ([`SiteConfig::skip_talkgroups`]). No channel was
    /// subscribed, no call opened, and nothing published for another tuner.
    /// Emitted once per call rather than once per grant refresh.
    CallSkipped {
        /// Frequency that was granted and not followed.
        frequency_hz: u64,
        /// Talkgroup the operator turned off.
        talkgroup: u16,
    },
    /// A call was granted on a TDMA (Phase 2) channel and not followed: this
    /// decoder is Phase 1 only, so no channel was subscribed, no call opened,
    /// no pool slot spent and nothing published for another tuner. Emitted
    /// once per call (per carrier and timeslot), from
    /// [`P25Event::TdmaGrantRefused`].
    TdmaRefused {
        /// Group, unit-to-unit or telephone.
        kind: CallKind,
        /// Downlink frequency of the TDMA carrier.
        frequency_hz: u64,
        /// TDMA timeslot (1 or 2).
        timeslot: u8,
        /// Talkgroup; 0 for unit-to-unit / telephone.
        talkgroup: u16,
        /// Source radio; 0 when the grant named none.
        source: u32,
        /// True the first time this engine refuses this talkgroup, so the
        /// recorder can log each talkgroup once rather than every call.
        first_for_talkgroup: bool,
    },
    /// A grant arrived with no free traffic channel.
    PoolExhausted {
        /// Frequency that could not be followed.
        frequency_hz: u64,
        /// Talkgroup of the call that was dropped.
        talkgroup: u16,
    },
    /// Raw control-channel event pass-through (only when
    /// [`SiteConfig::raw_control`] is set).
    Control(P25Event),
    /// Raw traffic-channel event pass-through (only when
    /// [`SiteConfig::raw_traffic`] is set), with the frequency it was heard
    /// on. Never carries [`P25Event::ImbeFrame`].
    Traffic {
        /// Downlink frequency of the traffic channel.
        frequency_hz: u64,
        /// The event.
        event: P25Event,
    },
    /// Periodic counters, every [`STATS_INTERVAL_MS`] of stream time.
    Stats(SiteStats),
    /// A grant this tuner is not recording itself: it is out of the tuner's
    /// responsible band (or capture) and must be routed to another tuner.
    /// Emitted only by an `Auto` front with [`SiteConfig::publish_grants`] set,
    /// or by any [`EngineRole::Control`] front. Carries the `Send` metadata the
    /// receiving tuner needs to describe the call.
    GrantOut {
        /// Downlink frequency granted.
        frequency_hz: u64,
        /// Call metadata as the control channel last described it.
        info: GrantInfo,
        /// Stream time of the grant on the control tuner.
        at_ms: u64,
    },
}

/// What the control channel last said about a frequency, used to describe the
/// call when a traffic channel starts it.
///
/// This is the whole of the `Send` grant metadata a cross-tuner voice front
/// needs: the coordinator carries it in a follow command so the receiving tuner
/// can describe the call it opens without any reference to the control tuner's
/// (non-`Send`) [`TrunkingTracker`].
#[derive(Debug, Clone, Copy)]
pub struct GrantInfo {
    /// Group / unit-to-unit / telephone.
    pub kind: CallKind,
    /// Talkgroup, 0 for unit-to-unit / telephone.
    pub talkgroup: u16,
    /// Full 24-bit TO address.
    pub target: u32,
    /// Source radio at grant time (0 when unknown).
    pub source: u32,
}

impl Default for GrantInfo {
    fn default() -> Self {
        GrantInfo {
            kind: CallKind::Group,
            talkgroup: 0,
            target: 0,
            source: 0,
        }
    }
}

/// A call the engine is currently reporting.
#[derive(Debug)]
struct ActiveCall {
    id: CallId,
    talkgroup: u16,
    target: u32,
    source: u32,
    kind: CallKind,
    /// Engine time the call was opened, so its end record can report a length.
    started_at_ms: u64,
    last_activity_ms: u64,
    emergency: bool,
    algid: Option<u8>,
    key_id: Option<u16>,
    /// L3Harris talker-alias assembly for the current talker (see
    /// [`crate::alias`]).
    alias: TalkerAliasAssembler,
    /// Whether a traffic-side link control word (LCO 0 / 3, or a terminator
    /// naming the same radio) has named the current talker. Until one has,
    /// an alias block cannot be told apart from the rest of the previous
    /// transmission's alias cycle — the talker may only be the grant's
    /// (control-side) word for it — so blocks are held, not filed.
    talker_confirmed: bool,
    /// Alias blocks received while the talker was unconfirmed, in arrival
    /// order (at most [`HELD_ALIAS_BLOCKS`], repeats dropped).
    held_alias: Vec<(usize, [u8; ALIAS_BLOCK_LEN])>,
    /// The current talker's alias recalled from the site's [`AliasMemory`],
    /// while all that arrived of it is the head of a longer field.
    recalled_alias: Option<String>,
    source_wacn: Option<u32>,
    source_system: Option<u16>,
}

impl ActiveCall {
    /// The alias this call names its current talker by: the one assembled
    /// on the air, else the one recalled from memory.
    fn talker_alias(&self) -> Option<String> {
        self.alias
            .to_alias()
            .or_else(|| self.recalled_alias.clone())
    }
}

/// How recently another radio must have been heard complete with a block for
/// that block to count as theirs on this call (see
/// [`AliasMemory::claimed_by_other`]).
const FOREIGN_ALIAS_MS: u64 = 30_000;

/// Alias blocks an unconfirmed talker's call holds: one full cycle.
const HELD_ALIAS_BLOCKS: usize = 4;

/// How long after a talker change (or a call's end) the previous talker's
/// alias blocks are recognised on the channel. The infrastructure keeps
/// cycling the previous talker's four blocks into the next transmission for
/// up to one cycle (4 superframes, ≈1.44 s) — observed as a console filed
/// under the field unit it answered.
const STALE_ALIAS_MS: u64 = 2_000;

/// The alias halves of the talker who last had a traffic channel, kept for
/// [`STALE_ALIAS_MS`] so a block of theirs arriving after the next talker is
/// confirmed is not read as the new talker's.
#[derive(Debug, Clone, Copy)]
struct StaleAlias {
    radio: u32,
    halves: [Option<[u8; ALIAS_BLOCK_LEN]>; 2],
    until_ms: u64,
}

/// One followed traffic channel: a channelizer subscription, its decoder, and
/// the assembler folding its events into a [`CallRecord`].
struct TrafficSlot {
    freq_hz: u64,
    handle: BankHandle,
    decoder: P25P1Channel,
    assembler: CallAssembler,
    call: Option<ActiveCall>,
    /// Stream time at which the subscription is dropped once no call is open.
    hang_until_ms: u64,
    /// The previous talker's alias halves (see [`StaleAlias`]).
    stale_alias: Option<StaleAlias>,
}

/// One followed SNDCP data channel: a channelizer subscription and a data-role
/// decoder that reassembles PDUs. Unlike a [`TrafficSlot`] it carries no call
/// state — a data channel has no HDU/voice/terminator lifecycle. It lives until
/// it goes quiet ([`DataSlot::expire_ms`]).
struct DataSlot {
    freq_hz: u64,
    handle: BankHandle,
    decoder: P25P1Channel,
    /// Stream time at which the subscription is dropped if no PDU arrives.
    expire_ms: u64,
}

/// A control candidate under evaluation.
struct HuntChannel {
    freq_hz: u64,
    handle: BankHandle,
    decoder: P25P1Channel,
    /// Carrier offset on this candidate's bin. A candidate that is hunting
    /// forever cannot report a decoder-derived frequency error — there is no
    /// lock — so the meter measures the raw bin instead, which is what lets
    /// the recorder correct a tuner *before* it has ever acquired.
    carrier: CarrierMeter,
}

/// The acquired control channel.
struct ControlChannel {
    freq_hz: u64,
    nac: u16,
    handle: BankHandle,
    decoder: P25P1Channel,
    last_activity_ms: u64,
    /// Carrier offset on the locked control channel, so slow oscillator drift
    /// is corrected without waiting for a loss of lock.
    carrier: CarrierMeter,
}

/// One P25 Phase 1 trunked site decoded from a single wideband capture.
pub struct SiteEngine {
    cfg: SiteConfig,
    /// The filterbank this engine reads its channels from: its own (built by
    /// [`SiteEngine::new`]) or its device's (handed to
    /// [`SiteEngine::with_bank`]). See "Who owns the channelizer".
    bank: SharedChannelBank,
    /// Whether [`SiteEngine::process_block`] may run the bank: only when this
    /// engine built it and so is its only consumer.
    owns_bank: bool,
    /// This engine's own noise-floor window in the bank.
    power_window: usize,
    /// The bank's sample count when this engine last looked, so stream time
    /// advances by exactly what the bank consumed since.
    bank_seen: u64,
    out_rate: u32,
    /// Control candidates that are actually reachable in this capture.
    candidates: Vec<u64>,
    hunt: Vec<HuntChannel>,
    control: Option<ControlChannel>,
    tracker: Option<SharedTracker>,
    traffic: Vec<TrafficSlot>,
    /// Followed SNDCP data channels (opcode-20 grants that resolved in-span).
    data: Vec<DataSlot>,
    /// Latest grant description per frequency (pruned with the slot).
    meta: HashMap<u64, GrantInfo>,
    /// Grants refused by the skip list: frequency → (talkgroup, stream time of
    /// the last refusal). The control channel repeats a grant for as long as
    /// the call is up and each repeat re-requests the traffic channel (the
    /// pool slot was handed straight back), so this is what makes
    /// [`SiteStats::calls_skipped_by_policy`] a count of calls rather than of
    /// grant messages. Entries expire with the same idle window a call does.
    skipped: HashMap<u64, (u16, u64)>,
    /// Talkgroups already refused as TDMA at least once, so
    /// [`SiteEvent::TdmaRefused::first_for_talkgroup`] is set once per
    /// talkgroup. Bounded by the 16-bit talkgroup space.
    tdma_seen: HashSet<u16>,
    /// The last complete talker alias heard from each radio on this site, so
    /// a call carrying only the head of a longer field can still be named
    /// (see [`AliasMemory`]). Bounded; entries expire after a day.
    aliases: AliasMemory,
    /// Live supergroup→members map from L3Harris regroup TSBKs, used to stamp a
    /// call on a patched talkgroup with the members hearing it.
    patches: PatchBook,
    /// Reused channelizer drain buffer.
    samples: Vec<Complex<f32>>,
    /// Reused per-channel event buffer.
    events: Vec<P25Event>,
    next_call_id: u64,
    samples_consumed: u64,
    now_ms: u64,
    announce_hunt: bool,
    stats: SiteStats,
    last_stats_ms: u64,
    window_tsbks: u64,
    /// Control-channel TSBKs that failed CRC this window, the denominator's
    /// other half for [`SiteStats::tsbk_pass_pct`].
    window_tsbk_failed: u64,
    /// Control-channel framer sync losses this window.
    window_sync_losses: u64,
    control_err_sum: f64,
    control_err_n: u64,
    traffic_err_sum: f64,
    traffic_err_n: u64,
}

impl SiteEngine {
    /// Build an engine for one site.
    ///
    /// # Errors
    ///
    /// Returns a [`P25Error`] when the capture rate cannot drive the
    /// channelizer or the resulting bin rate cannot drive a decoder:
    /// [`P25Error::RateNotDivisible`] when the rate is not a multiple of
    /// 12.5 kHz (or, for an odd bin count, of 25 kHz) and
    /// [`P25Error::SampleRateTooLow`] when it yields fewer than four bins.
    /// `taps_per_channel` is clamped to the filterbank's minimum of 2.
    ///
    /// Control candidates outside the capture's usable span (or on the DC bin)
    /// are **not** an error: they are dropped, leaving
    /// [`candidates`](Self::candidates) — and if that list ends up empty the
    /// engine reports `ControlHunting { candidates: [] }` and idles.
    pub fn new(cfg: SiteConfig) -> Result<Self, P25Error> {
        let bank = Self::bank_for(&cfg)?;
        Self::attach(cfg, bank, true)
    }

    /// A channel bank for `cfg`'s capture, to share between every engine
    /// decoding that capture (one per device). Fails exactly as
    /// [`new`](Self::new) would for the same configuration.
    pub fn bank_for(cfg: &SiteConfig) -> Result<SharedChannelBank, P25Error> {
        ChannelBank::shared(cfg.sample_rate, cfg.center_hz, cfg.taps_per_channel)
            .map_err(map_channelizer_error)
    }

    /// Build an engine for one site that reads its channels from `bank`, a
    /// filterbank the caller owns and runs (see "Who owns the channelizer").
    ///
    /// # Errors
    ///
    /// As [`new`](Self::new), plus [`P25Error::ChannelBankMismatch`] when the
    /// bank was built for another capture rate, centre or prototype length
    /// than `cfg` describes.
    pub fn with_bank(cfg: SiteConfig, bank: SharedChannelBank) -> Result<Self, P25Error> {
        {
            let b = bank.borrow();
            let what = if b.sample_rate() != cfg.sample_rate {
                Some("sample rate")
            } else if b.center_hz().to_bits() != cfg.center_hz.to_bits() {
                Some("centre frequency")
            } else if b.taps_per_channel() != cfg.taps_per_channel.max(2) {
                Some("taps per channel")
            } else {
                None
            };
            if let Some(what) = what {
                return Err(P25Error::ChannelBankMismatch { what });
            }
        }
        Self::attach(cfg, bank, false)
    }

    fn attach(cfg: SiteConfig, bank: SharedChannelBank, owns_bank: bool) -> Result<Self, P25Error> {
        let out_rate = bank.borrow().output_rate();
        // Proves the bin rate can drive a decoder, so every later channel
        // construction is infallible.
        drop(P25P1Channel::new(out_rate, cfg.mode, 0)?);
        let (power_window, bank_seen) = {
            let mut b = bank.borrow_mut();
            (b.open_power_window(), b.samples_consumed())
        };

        let mut engine = SiteEngine {
            cfg,
            bank,
            owns_bank,
            power_window,
            bank_seen,
            out_rate,
            candidates: Vec::new(),
            hunt: Vec::new(),
            control: None,
            tracker: None,
            traffic: Vec::new(),
            data: Vec::new(),
            meta: HashMap::new(),
            skipped: HashMap::new(),
            tdma_seen: HashSet::new(),
            aliases: AliasMemory::default(),
            patches: PatchBook::new(),
            samples: Vec::with_capacity(4096),
            events: Vec::with_capacity(64),
            next_call_id: 0,
            samples_consumed: 0,
            now_ms: 0,
            announce_hunt: true,
            stats: SiteStats::default(),
            last_stats_ms: 0,
            window_tsbks: 0,
            window_tsbk_failed: 0,
            window_sync_losses: 0,
            control_err_sum: 0.0,
            control_err_n: 0,
            traffic_err_sum: 0.0,
            traffic_err_n: 0,
        };
        // Call ids are namespaced per tuner: `call_id_base + 1, +2, …`. With the
        // default base of 0 this is the historical sequence 1, 2, 3, ….
        engine.next_call_id = engine.cfg.call_id_base + 1;
        engine.stats.pool_size = engine.cfg.traffic_pool_size;
        match engine.cfg.role {
            EngineRole::Auto | EngineRole::Control => {
                engine.candidates = engine
                    .cfg
                    .control_frequencies
                    .iter()
                    .copied()
                    .filter(|f| engine.bank.borrow().bin_for(*f as f64).is_ok())
                    .collect();
                engine.subscribe_candidates();
            }
            // A voice/data front never hunts control; it follows grants handed
            // to it. It still needs a tracker of its own so the traffic decoders
            // it builds have somewhere to key their call state — a fresh, local
            // (non-shared-across-threads) one, seeded by the channel plan the
            // coordinator forwards.
            EngineRole::Voice | EngineRole::Data => {
                let tracker = TrunkingTracker::new_shared(0);
                {
                    let mut t = tracker.borrow_mut();
                    t.set_pool_size(engine.cfg.traffic_pool_size as u32);
                    t.set_follow_unit_calls(engine.cfg.follow_unit_calls);
                }
                engine.tracker = Some(tracker);
            }
        }
        Ok(engine)
    }

    /// This tuner's role for its site.
    pub fn role(&self) -> EngineRole {
        self.cfg.role
    }

    /// Per-channel output rate (25 kHz for any legal capture rate).
    pub fn output_rate(&self) -> u32 {
        self.out_rate
    }

    /// Control candidates the engine can actually reach in this capture.
    pub fn candidates(&self) -> &[u64] {
        &self.candidates
    }

    /// Calls currently open.
    pub fn active_calls(&self) -> usize {
        self.traffic.iter().filter(|s| s.call.is_some()).count()
    }

    /// Current counters (also emitted periodically as [`SiteEvent::Stats`]).
    pub fn stats(&self) -> SiteStats {
        let mut s = self.stats;
        s.stream_ms = self.now_ms;
        s.active_calls = self.active_calls();
        s.pool_in_use = self.traffic.len();
        s
    }

    /// The acquired control frequency, if any.
    pub fn control_frequency_hz(&self) -> Option<u64> {
        self.control.as_ref().map(|c| c.freq_hz)
    }

    /// The NAC of the acquired control channel, if any.
    pub fn control_nac(&self) -> Option<u16> {
        self.control.as_ref().map(|c| c.nac)
    }

    /// The filterbank this engine reads from (shared with the device's other
    /// engines when it was built by [`with_bank`](Self::with_bank)).
    pub fn channel_bank(&self) -> &SharedChannelBank {
        &self.bank
    }

    /// Feed one block of wideband IQ at the configured capture rate: run this
    /// engine's own filterbank over it, then decode
    /// ([`process_channels`](Self::process_channels)).
    ///
    /// Only for an engine built by [`new`](Self::new). An engine attached to
    /// a shared bank never runs it — that would feed the block to every other
    /// engine on the bank a second time — its owner calls
    /// [`ChannelBank::process_block`] once and then `process_channels` on
    /// each engine.
    ///
    /// `sink` receives every event produced by the block, in order. It must
    /// not call back into this engine.
    pub fn process_block(&mut self, wideband: &[Complex<f32>], sink: &mut dyn FnMut(SiteEvent)) {
        debug_assert!(
            self.owns_bank,
            "process_block on an engine sharing its channel bank; run the bank once and call \
             process_channels on each engine"
        );
        self.bank.borrow_mut().process_block(wideband);
        self.process_channels(sink);
    }

    /// Decode the block the bank has just produced: read every channel this
    /// engine subscribes, advance stream time by the samples the bank
    /// consumed, and run the control, traffic and data paths.
    ///
    /// `sink` receives every event produced by the block, in order. It must
    /// not call back into this engine.
    pub fn process_channels(&mut self, sink: &mut dyn FnMut(SiteEvent)) {
        let follows_control = matches!(self.cfg.role, EngineRole::Auto | EngineRole::Control);
        if self.announce_hunt && follows_control {
            self.announce_hunt = false;
            sink(SiteEvent::ControlHunting {
                candidates: self.candidates.clone(),
            });
        }
        let consumed = self.bank.borrow().samples_consumed();
        self.samples_consumed += consumed.saturating_sub(self.bank_seen);
        self.bank_seen = consumed;
        self.now_ms = self.samples_consumed * 1000 / u64::from(self.cfg.sample_rate);

        if follows_control {
            if self.control.is_some() {
                self.run_control(sink);
            } else {
                self.run_hunt(sink);
            }
        }
        self.run_traffic(sink);
        self.run_data(sink);
        self.housekeeping(sink);
    }

    /// Follow a grant handed over by the site coordinator (voice/data fronts,
    /// and the cross-tuner path in general).
    ///
    /// Records the grant metadata and subscribes the bin, exactly as a locally
    /// decoded [`P25Event::TrafficChannelRequest`] would. A [`EngineRole::Data`]
    /// front counts the follow and drops it — the SNDCP decode path is a later
    /// task.
    pub fn follow(
        &mut self,
        frequency_hz: u64,
        info: GrantInfo,
        _at_ms: u64,
        sink: &mut dyn FnMut(SiteEvent),
    ) {
        self.meta.insert(frequency_hz, info);
        if self.cfg.role == EngineRole::Data {
            // A dedicated data front subscribes the granted data channel and
            // reassembles the subscriber's SNDCP PDUs.
            self.meta.remove(&frequency_hz);
            self.start_data(frequency_hz, sink);
            return;
        }
        self.start_traffic(frequency_hz, sink);
    }

    /// Stop following `frequency_hz` (coordinator release, or a keyup pull in
    /// the later control loop). Closes any open call so its audio is not lost.
    pub fn release(&mut self, frequency_hz: u64, sink: &mut dyn FnMut(SiteEvent)) {
        if let Some(i) = self.slot_of(frequency_hz) {
            if self.traffic[i].call.is_some() {
                self.close_call(i, EndReason::Shutdown, None, sink);
            }
            self.release_slot(i);
        }
    }

    /// Merge a channel-plan (IDEN) snapshot into this tuner's local tracker, so
    /// a voice/data front can resolve grants keyed by band+channel the same way
    /// the control tuner does. Absolute-frequency follows do not need it; this
    /// is the seam for grant forms that carry a channel reference.
    pub fn apply_channel_plan(&mut self, bands: &[crate::iden::IdenBand]) {
        if let Some(tracker) = &self.tracker {
            let mut t = tracker.borrow_mut();
            for band in bands {
                t.bands_mut().insert(*band);
            }
        }
    }

    /// A snapshot of this tuner's channel plan, for the coordinator to forward
    /// to voice/data fronts.
    pub fn channel_plan(&self) -> Vec<crate::iden::IdenBand> {
        self.tracker
            .as_ref()
            .map(|t| t.borrow().bands().iter().copied().collect())
            .unwrap_or_default()
    }

    /// End every open call and drop every traffic subscription.
    ///
    /// The control channel is left in place, so the engine can keep running.
    pub fn shutdown(&mut self, sink: &mut dyn FnMut(SiteEvent)) {
        while !self.traffic.is_empty() {
            let last = self.traffic.len() - 1;
            self.close_call(last, EndReason::Shutdown, None, sink);
            self.release_slot(last);
        }
        for slot in self.data.drain(..) {
            self.bank.borrow_mut().unsubscribe(slot.handle);
        }
    }

    // -- control acquisition -------------------------------------------------

    /// Subscribe every reachable candidate and give each its own decoder with
    /// a throwaway private tracker.
    fn subscribe_candidates(&mut self) {
        for i in 0..self.candidates.len() {
            let freq = self.candidates[i];
            let Ok(handle) = self.bank.borrow_mut().subscribe(freq as f64) else {
                continue;
            };
            let mut decoder = self.new_decoder(freq, None);
            decoder.set_clock_ms(self.now_ms);
            self.hunt.push(HuntChannel {
                freq_hz: freq,
                handle,
                decoder,
                carrier: CarrierMeter::new(self.out_rate),
            });
        }
    }

    /// Build a decoder for `freq`: control-role on the site tracker when one
    /// is given, otherwise standalone (hunting).
    fn new_decoder(&self, freq: u64, tracker: Option<SharedTracker>) -> P25P1Channel {
        let rate = self.out_rate;
        let mode = self.cfg.mode;
        match tracker {
            Some(t) => P25P1Channel::new_control(rate, mode, freq, t),
            None => P25P1Channel::new(rate, mode, freq),
        }
        .expect("channel rate validated in SiteEngine::new")
    }

    fn run_hunt(&mut self, sink: &mut dyn FnMut(SiteEvent)) {
        for i in 0..self.hunt.len() {
            self.samples.clear();
            let handle = self.hunt[i].handle;
            self.bank.borrow_mut().drain(handle, &mut self.samples);
            // Measured on the raw bin, ahead of the decoder: the whole point is
            // to have a reading on a candidate that never locks.
            self.hunt[i].carrier.feed(&self.samples);
            // Hunting only needs the framer's counters; events are discarded.
            self.hunt[i].decoder.process(&self.samples, &mut |_| {});
        }
        let Some((idx, nac)) = self.winner() else {
            return;
        };
        let won = self.hunt.swap_remove(idx);
        for h in self.hunt.drain(..) {
            self.bank.borrow_mut().unsubscribe(h.handle);
        }
        let tracker = TrunkingTracker::new_shared(won.freq_hz);
        {
            let mut t = tracker.borrow_mut();
            t.set_pool_size(self.cfg.traffic_pool_size as u32);
            t.set_follow_unit_calls(self.cfg.follow_unit_calls);
        }
        let mut decoder = self.new_decoder(won.freq_hz, Some(tracker.clone()));
        decoder.set_clock_ms(self.now_ms);
        self.tracker = Some(tracker);
        self.control = Some(ControlChannel {
            freq_hz: won.freq_hz,
            nac,
            handle: won.handle,
            decoder,
            last_activity_ms: self.now_ms,
            // A fresh meter: the winning candidate's accumulation belongs to
            // the hunt window that just ended, not to the locked channel.
            carrier: CarrierMeter::new(self.out_rate),
        });
        self.stats.control_frequency_hz = won.freq_hz;
        self.stats.nac = nac;
        self.stats.control_tsbks = 0;
        self.stats.control_acquisitions += 1;
        sink(SiteEvent::ControlAcquired {
            frequency_hz: won.freq_hz,
            nac,
        });
    }

    /// First candidate with enough TSBKs under a dominant NAC.
    fn winner(&self) -> Option<(usize, u16)> {
        for (i, h) in self.hunt.iter().enumerate() {
            let stats = h.decoder.stats();
            if stats.tsbk_count < ACQUIRE_TSBKS {
                continue;
            }
            let total: u64 = stats.nac_histogram.iter().map(|e| e.1).sum();
            let Some(modal) = stats.nac_histogram.iter().max_by_key(|e| e.1) else {
                continue;
            };
            // "Consistent NAC": the winner must account for most of the NIDs.
            if modal.1 * 2 >= total {
                return Some((i, modal.0));
            }
        }
        None
    }

    fn run_control(&mut self, sink: &mut dyn FnMut(SiteEvent)) {
        let Some(handle) = self.control.as_ref().map(|c| c.handle) else {
            return;
        };
        self.samples.clear();
        self.bank.borrow_mut().drain(handle, &mut self.samples);
        let mut events = std::mem::take(&mut self.events);
        events.clear();
        if let Some(c) = self.control.as_mut() {
            c.carrier.feed(&self.samples);
            c.decoder.process(&self.samples, &mut |e| events.push(e));
        }
        for e in events.drain(..) {
            self.handle_control_event(&e, sink);
            if self.cfg.raw_control {
                sink(SiteEvent::Control(e));
            }
        }
        self.events = events;
    }

    fn handle_control_event(&mut self, e: &P25Event, sink: &mut dyn FnMut(SiteEvent)) {
        match e {
            P25Event::Tsbk { tsbk, .. } => {
                self.stats.control_tsbks += 1;
                self.window_tsbks += 1;
                // Fold L3Harris regroup (opcode 48) into the live patch map; a
                // no-op for every other message. Malformed regroups are dropped
                // inside `observe`, so this can never break control decode.
                self.patches.observe(&tsbk.message, self.now_ms);
                self.touch_control();
            }
            // A TSBK whose CRC failed is the other half of the decode-quality
            // ratio: it is the *only* direct evidence that the bits are
            // arriving damaged rather than not arriving at all, which a falling
            // TSBK rate on its own cannot tell apart from a quiet site.
            P25Event::TsbkFailed => self.window_tsbk_failed += 1,
            P25Event::NidDecoded { .. } => self.touch_control(),
            P25Event::SyncLoss { .. } => {
                self.stats.sync_losses += 1;
                self.window_sync_losses += 1;
            }
            P25Event::FrequencyError { hz } => {
                self.control_err_sum += hz;
                self.control_err_n += 1;
            }
            P25Event::Grant {
                kind,
                frequency_hz,
                talkgroup,
                target,
                source,
                ..
            } => {
                let meta = self.meta.entry(*frequency_hz).or_default();
                // A grant update (opcodes 2/3) carries no source, so a zero
                // keeps the talker the grant named — but only while it is the
                // same call. A grant for another kind, talkgroup or target is
                // another call, and the radio that keyed the previous one on
                // this frequency is not its caller.
                let same_call =
                    meta.kind == *kind && meta.talkgroup == *talkgroup && meta.target == *target;
                meta.kind = *kind;
                meta.talkgroup = *talkgroup;
                meta.target = *target;
                if *source != 0 || !same_call {
                    meta.source = *source;
                }
            }
            P25Event::CallStart {
                frequency_hz,
                talkgroup,
                source,
                ..
            } => {
                // The tracker has judged this a *new* call on the frequency
                // (no live tracker, a stale one, or a different TO). Whoever
                // it names — possibly nobody, for a grant update — is its
                // caller; the previous call's radio is not carried over.
                if let Some(meta) = self.meta.get_mut(frequency_hz) {
                    meta.source = *source;
                }
                self.on_control_call_start(*frequency_hz, *talkgroup, sink)
            }
            P25Event::CallContinue {
                frequency_hz,
                source,
                ..
            } => {
                if let Some(i) = self.slot_of(*frequency_hz) {
                    self.set_talker(i, *source, false, sink);
                }
            }
            P25Event::TrafficChannelRequest { frequency_hz } => {
                self.on_traffic_request(*frequency_hz, sink)
            }
            // An SNDCP data-channel grant: an `Auto` front follows it locally
            // only when `data_follow` is enabled (opt-in, default off) and the
            // data channel is in its capture. Off by default because a downlink
            // capture cannot yield coordinates (the PDUs are outbound polls; the
            // GPS response is uplink), so spending a channelizer slot on it has
            // no location payoff — the flag exists for W5 / uplink work.
            // Routing a data grant to a dedicated data tuner is the multi-tuner
            // follow-up (W5); until then an unreachable data channel is not
            // followed.
            P25Event::DataGrant { frequency_hz } => {
                if self.cfg.data_follow
                    && self.cfg.role == EngineRole::Auto
                    && self.responsible_for(*frequency_hz)
                {
                    self.start_data(*frequency_hz, sink);
                }
            }
            P25Event::TdmaGrantRefused {
                kind,
                frequency_hz,
                timeslot,
                talkgroup,
                source,
                ..
            } => {
                // The tracker refused it before tracking or allocating anything
                // (and once per call), so all that is left is to count it.
                self.stats.calls_tdma_refused += 1;
                sink(SiteEvent::TdmaRefused {
                    kind: *kind,
                    frequency_hz: *frequency_hz,
                    timeslot: *timeslot,
                    talkgroup: *talkgroup,
                    source: *source,
                    first_for_talkgroup: self.tdma_seen.insert(*talkgroup),
                });
            }
            P25Event::PoolExhausted { frequency_hz } => {
                self.stats.pool_exhausted += 1;
                sink(SiteEvent::PoolExhausted {
                    frequency_hz: *frequency_hz,
                    talkgroup: self.meta.get(frequency_hz).map_or(0, |m| m.talkgroup),
                });
            }
            _ => {}
        }
    }

    fn touch_control(&mut self) {
        let now = self.now_ms;
        if let Some(c) = self.control.as_mut() {
            c.last_activity_ms = now;
        }
    }

    /// The control channel says a *new* call owns this frequency.
    fn on_control_call_start(
        &mut self,
        freq: u64,
        talkgroup: u16,
        sink: &mut dyn FnMut(SiteEvent),
    ) {
        let Some(i) = self.slot_of(freq) else {
            // No channel yet — `TrafficChannelRequest` follows and opens it.
            return;
        };
        // A unit-to-unit call has no talkgroup — both directions of one
        // conversation report 0 — so the pair is what tells two calls apart.
        // When consecutive grants on one channel swap the two units, the other
        // party has keyed up: end the call and start a new one, so each
        // recording has exactly one talker (Keyup keys the radio id per call).
        // Only unit-to-unit: a group call's talkgroup already identifies it,
        // and a telephone interconnect keeps the rule it has always had.
        let pair = self
            .meta
            .get(&freq)
            .filter(|m| m.kind == CallKind::UnitToUnit)
            .map(|m| (m.target, m.source));
        let same = |c: &ActiveCall| match pair {
            None => c.talkgroup == talkgroup,
            Some((target, source)) => {
                c.talkgroup == talkgroup && c.target == target && c.source == source
            }
        };
        match &self.traffic[i].call {
            // Same call: the tracker aged out and re-created its entry
            // mid-call; keep the call we are already reporting.
            Some(c) if same(c) => {}
            Some(_) => {
                self.close_call(i, EndReason::PoolReassigned, None, sink);
                self.open_call(i, sink);
            }
            None => self.open_call(i, sink),
        }
    }

    // -- traffic pool --------------------------------------------------------

    fn slot_of(&self, freq: u64) -> Option<usize> {
        self.traffic.iter().position(|s| s.freq_hz == freq)
    }

    /// Whether this tuner is responsible for recording `freq` itself: inside its
    /// capture's usable span **and** inside one of its configured bands (or any
    /// band, when none are configured).
    fn responsible_for(&self, freq: u64) -> bool {
        if self.bank.borrow().bin_for(freq as f64).is_err() {
            return false;
        }
        self.cfg.bands.is_empty()
            || self
                .cfg
                .bands
                .iter()
                .any(|(lo, hi)| freq >= *lo && freq <= *hi)
    }

    /// A grant arrived on the control channel. Follow it locally when this tuner
    /// is responsible for it; otherwise publish it for another tuner, or — with
    /// no coordinator — take the historical single-tuner path.
    fn on_traffic_request(&mut self, freq: u64, sink: &mut dyn FnMut(SiteEvent)) {
        // The one refusal that comes before every role: a talkgroup the
        // operator has turned off costs this recorder nothing at all — no
        // channelizer slot, no call, and no `GrantOut` for a sibling tuner or a
        // peer over the recorder link (every peer holds the same list).
        if self.skip_grant(freq, sink) {
            return;
        }
        match self.cfg.role {
            // A pure control front never records traffic: every grant is routed.
            EngineRole::Control => self.publish_grant(freq, sink),
            EngineRole::Auto => {
                if !self.cfg.publish_grants {
                    // No coordinator: exactly today's path — subscribe or count
                    // out-of-band inside `start_traffic`.
                    self.start_traffic(freq, sink);
                } else if self.responsible_for(freq) {
                    self.start_traffic(freq, sink);
                } else {
                    self.publish_grant(freq, sink);
                }
            }
            // Voice/data fronts do not run the control path, so this is
            // unreachable; be safe and route rather than record.
            EngineRole::Voice | EngineRole::Data => self.publish_grant(freq, sink),
        }
    }

    /// Whether this grant is refused by the skip list, counting and reporting
    /// it the first time it is refused for a given call.
    ///
    /// Hands the pool slot the tracker allocated straight back, exactly as a
    /// published grant does: the call is not being recorded here or anywhere,
    /// so holding a traffic channel for it would deny one to a talkgroup the
    /// operator does want.
    ///
    /// Group calls only. A unit-to-unit or telephone call has no talkgroup to
    /// match (both report 0), and a call on a **patch** supergroup is refused
    /// only when every member talkgroup is skipped too — a patch that pulls one
    /// wanted talkgroup into a skipped one is still wanted.
    fn skip_grant(&mut self, freq: u64, sink: &mut dyn FnMut(SiteEvent)) -> bool {
        let Some(skip) = self.cfg.skip_talkgroups.as_ref() else {
            return false;
        };
        if skip.is_empty() {
            return false;
        }
        let Some(meta) = self.meta.get(&freq) else {
            return false;
        };
        if meta.kind != CallKind::Group || meta.talkgroup == 0 {
            return false;
        }
        let talkgroup = meta.talkgroup;
        if !skip.contains(u32::from(talkgroup)) {
            return false;
        }
        if !self
            .patches
            .members_of(talkgroup, self.now_ms)
            .iter()
            .all(|m| skip.contains(*m))
        {
            return false;
        }
        if let Some(tracker) = &self.tracker {
            tracker.borrow_mut().traffic_start_rejected(freq);
        }
        // One report per call: the control channel re-grants an open call every
        // few hundred ms, and each re-grant lands here because the pool slot
        // was given back.
        let idle = self.cfg.hang_ms + self.cfg.call_inactivity_ms;
        let fresh = match self.skipped.get(&freq) {
            Some((tg, last)) => *tg != talkgroup || self.now_ms.saturating_sub(*last) > idle,
            None => true,
        };
        self.skipped.insert(freq, (talkgroup, self.now_ms));
        if fresh {
            self.stats.calls_skipped_by_policy += 1;
            sink(SiteEvent::CallSkipped {
                frequency_hz: freq,
                talkgroup,
            });
        }
        true
    }

    /// Emit a grant for the coordinator to route to another tuner.
    ///
    /// The tracker allocated a pool slot when it produced the request; since the
    /// recording happens on another tuner, hand that slot straight back so this
    /// front's pool accounting does not leak on a long run.
    fn publish_grant(&mut self, freq: u64, sink: &mut dyn FnMut(SiteEvent)) {
        // NB: this counts grant *events* published, including a control-side
        // refresh (CallContinue/repeat grant) on a frequency already routed, so
        // it is not a count of distinct routed calls.
        self.stats.grants_published += 1;
        if let Some(tracker) = &self.tracker {
            tracker.borrow_mut().traffic_start_rejected(freq);
        }
        let info = self.meta.get(&freq).copied().unwrap_or_default();
        sink(SiteEvent::GrantOut {
            frequency_hz: freq,
            info,
            at_ms: self.now_ms,
        });
    }

    /// Honour the tracker's traffic-channel request: subscribe the bin, build
    /// a traffic-role decoder, and open the call.
    fn start_traffic(&mut self, freq: u64, sink: &mut dyn FnMut(SiteEvent)) {
        if self.slot_of(freq).is_some() {
            return;
        }
        let Some(tracker) = self.tracker.clone() else {
            return;
        };
        let handle = match self.bank.borrow_mut().subscribe(freq as f64) {
            Ok(h) => h,
            Err(_) => {
                // Outside the capture: give the pool slot straight back so a
                // reachable call can use it.
                self.stats.out_of_band_grants += 1;
                tracker.borrow_mut().traffic_start_rejected(freq);
                return;
            }
        };
        let mut decoder = P25P1Channel::new_traffic(self.out_rate, self.cfg.mode, freq, tracker)
            .expect("channel rate validated in SiteEngine::new");
        decoder.set_clock_ms(self.now_ms);
        self.traffic.push(TrafficSlot {
            freq_hz: freq,
            handle,
            decoder,
            assembler: CallAssembler::new(),
            call: None,
            hang_until_ms: self.now_ms + self.cfg.hang_ms,
            stale_alias: None,
        });
        let idx = self.traffic.len() - 1;
        self.open_call(idx, sink);
    }

    // -- data channel follow -------------------------------------------------

    /// Subscribe a granted SNDCP data channel and build a data-role decoder for
    /// it, or refresh its dwell if already followed. A follow needs a tracker
    /// (the shared site tracker for `Auto`, the local one for a `Data` front);
    /// with none, or a frequency outside the capture, nothing is followed.
    fn start_data(&mut self, freq: u64, _sink: &mut dyn FnMut(SiteEvent)) {
        let dwell = self.now_ms + self.cfg.hang_ms + self.cfg.call_inactivity_ms;
        if let Some(slot) = self.data.iter_mut().find(|s| s.freq_hz == freq) {
            slot.expire_ms = dwell;
            return;
        }
        let Some(tracker) = self.tracker.clone() else {
            return;
        };
        let handle = match self.bank.borrow_mut().subscribe(freq as f64) {
            Ok(h) => h,
            Err(_) => {
                self.stats.out_of_band_grants += 1;
                return;
            }
        };
        let mut decoder = P25P1Channel::new_data(self.out_rate, self.cfg.mode, freq, tracker)
            .expect("channel rate validated in SiteEngine::new");
        decoder.set_clock_ms(self.now_ms);
        self.data.push(DataSlot {
            freq_hz: freq,
            handle,
            decoder,
            expire_ms: dwell,
        });
        self.stats.data_grants += 1;
    }

    /// Drain every followed data channel and forward its PDU events. The PDU
    /// events cross to the non-voice fold as [`SiteEvent::Traffic`] (never gated
    /// by `raw_traffic` — a data channel carries no voice, only signaling), so
    /// the downstream LRRP inspector sees the reassembled payload bytes.
    fn run_data(&mut self, sink: &mut dyn FnMut(SiteEvent)) {
        let mut events = std::mem::take(&mut self.events);
        for i in 0..self.data.len() {
            self.samples.clear();
            let handle = self.data[i].handle;
            if self.bank.borrow_mut().drain(handle, &mut self.samples) == 0 {
                continue;
            }
            events.clear();
            self.data[i]
                .decoder
                .process(&self.samples, &mut |e| events.push(e));
            let freq = self.data[i].freq_hz;
            for e in events.drain(..) {
                match &e {
                    P25Event::Pdu { .. } => {
                        self.stats.data_pdus += 1;
                        self.data[i].expire_ms =
                            self.now_ms + self.cfg.hang_ms + self.cfg.call_inactivity_ms;
                        sink(SiteEvent::Traffic {
                            frequency_hz: freq,
                            event: e,
                        });
                    }
                    P25Event::PduHeader { .. } => {
                        self.data[i].expire_ms =
                            self.now_ms + self.cfg.hang_ms + self.cfg.call_inactivity_ms;
                        sink(SiteEvent::Traffic {
                            frequency_hz: freq,
                            event: e,
                        });
                    }
                    P25Event::SyncLoss { .. } => self.stats.sync_losses += 1,
                    _ => {}
                }
            }
        }
        self.events = events;
    }

    fn run_traffic(&mut self, sink: &mut dyn FnMut(SiteEvent)) {
        let mut events = std::mem::take(&mut self.events);
        for i in 0..self.traffic.len() {
            self.samples.clear();
            let handle = self.traffic[i].handle;
            if self.bank.borrow_mut().drain(handle, &mut self.samples) == 0 {
                continue;
            }
            events.clear();
            self.traffic[i]
                .decoder
                .process(&self.samples, &mut |e| events.push(e));
            for e in events.drain(..) {
                self.handle_traffic_event(i, &e, sink);
            }
        }
        self.events = events;
    }

    fn handle_traffic_event(&mut self, idx: usize, e: &P25Event, sink: &mut dyn FnMut(SiteEvent)) {
        let now = self.now_ms;
        // Signaling only: voice never enters the pass-through (see
        // `SiteConfig::raw_traffic`).
        if self.cfg.raw_traffic && !matches!(e, P25Event::ImbeFrame { .. }) {
            sink(SiteEvent::Traffic {
                frequency_hz: self.traffic[idx].freq_hz,
                event: e.clone(),
            });
        }
        // The assembler is the sole authority on call boundaries: it closes on
        // a terminator and on a fresh HDU that replaces a running call. Going
        // through it (rather than reacting to `Tdu`/`TduLc` directly) is what
        // keeps the engine's call and the record it will report in lockstep —
        // and a terminator arriving before the assembler has seen anything is
        // the tail of the *previous* transmission on this frequency, caught
        // because the grant subscribed us mid-teardown: it must not close the
        // call we have just opened.
        // A terminator whose link control names the talker we hold blocks
        // for is the transmission's own last word: a traffic-side
        // confirmation arriving just before the call closes. Short calls often
        // carry their alias head before any LDU1 names the talker, and this is
        // the only word that ever does.
        if let P25Event::TduLc {
            valid: true,
            link_control,
            ..
        } = e
        {
            self.confirm_from_terminator(idx, link_control, sink);
        }
        if let Some(record) = self.traffic[idx].assembler.observe(e) {
            if self.traffic[idx].call.is_some() {
                self.close_call(idx, EndReason::Terminator, Some(record), sink);
            }
        }
        match e {
            P25Event::Hdu {
                valid: true,
                header,
                ..
            } => {
                self.ensure_call(idx, sink);
                if let Some(h) = header {
                    self.note_crypto(idx, h.algid, h.kid, sink);
                }
                self.touch_call(idx, now);
            }
            P25Event::ImbeFrame { frame, .. } => {
                self.ensure_call(idx, sink);
                if let Some(c) = &self.traffic[idx].call {
                    sink(SiteEvent::CallVoice {
                        id: c.id,
                        frame: *frame,
                        at_ms: now,
                    });
                }
                self.stats.voice_frames += 1;
                self.touch_call(idx, now);
            }
            P25Event::LinkControl { parsed, .. } => {
                // A talker-alias block with no call open is the tail of the
                // *previous* transmission on this frequency: its alias cycle
                // runs on past the terminator, and the grant that subscribed
                // us can catch it mid-teardown. Opening a call for it would
                // file one talker's alias under the next talker's call — the
                // cross-contamination the archive shows.
                if self.traffic[idx].call.is_none() && is_alias_lc(parsed) {
                    return;
                }
                self.ensure_call(idx, sink);
                self.apply_link_control(idx, parsed, sink);
                self.touch_call(idx, now);
            }
            P25Event::EncryptionSync { parsed, .. } => {
                if self.traffic[idx].call.is_some() {
                    self.note_crypto(idx, parsed.algid, parsed.kid, sink);
                    self.touch_call(idx, now);
                }
            }
            // The traffic decoder's tracker hook reports the talker change;
            // the link control word that caused it confirms it.
            P25Event::CallContinue { source, .. } => self.set_talker(idx, *source, false, sink),
            P25Event::SyncLoss { .. } => self.stats.sync_losses += 1,
            P25Event::FrequencyError { hz } => {
                self.traffic_err_sum += hz;
                self.traffic_err_n += 1;
            }
            _ => {}
        }
    }

    fn touch_call(&mut self, idx: usize, now: u64) {
        if let Some(c) = self.traffic[idx].call.as_mut() {
            c.last_activity_ms = now;
        }
    }

    /// Voice arrived with no call open (a follow-on transmission inside the
    /// hang window, or a channel joined mid-call): start a new one.
    fn ensure_call(&mut self, idx: usize, sink: &mut dyn FnMut(SiteEvent)) {
        if self.traffic[idx].call.is_none() {
            self.open_call(idx, sink);
        }
    }

    fn open_call(&mut self, idx: usize, sink: &mut dyn FnMut(SiteEvent)) {
        let now = self.now_ms;
        let freq = self.traffic[idx].freq_hz;
        let meta = self.meta.get(&freq).copied().unwrap_or_default();
        let id = CallId(self.next_call_id);
        self.next_call_id += 1;
        self.traffic[idx].call = Some(ActiveCall {
            id,
            talkgroup: meta.talkgroup,
            target: meta.target,
            source: meta.source,
            kind: meta.kind,
            started_at_ms: now,
            last_activity_ms: now,
            emergency: false,
            algid: None,
            key_id: None,
            alias: TalkerAliasAssembler::new(),
            talker_confirmed: false,
            held_alias: Vec::new(),
            recalled_alias: None,
            source_wacn: None,
            source_system: None,
        });
        self.stats.calls_started += 1;
        // A group call on a talkgroup that is a live supergroup is a patched
        // call: stamp it with the members hearing it. Any other call → empty.
        let patches = match meta.kind {
            CallKind::Group => self.patches.members_of(meta.talkgroup, now),
            _ => Vec::new(),
        };
        sink(SiteEvent::CallStarted {
            id,
            frequency_hz: freq,
            talkgroup: meta.talkgroup,
            target: meta.target,
            source: meta.source,
            kind: meta.kind,
            patches,
            at_ms: now,
        });
        if meta.kind == CallKind::UnitToUnit {
            sink(SiteEvent::Control(P25Event::PrivateCall {
                start: true,
                frequency_hz: freq,
                source: meta.source,
                target: meta.target,
                duration_ms: 0,
            }));
        }
    }

    /// Close the call on `idx` (if any), reporting `record` — or whatever the
    /// assembler still holds — merged with what the control channel said.
    fn close_call(
        &mut self,
        idx: usize,
        reason: EndReason,
        record: Option<CallRecord>,
        sink: &mut dyn FnMut(SiteEvent),
    ) {
        let now = self.now_ms;
        let hang_ms = self.cfg.hang_ms;
        let slot = &mut self.traffic[idx];
        let Some(call) = slot.call.take() else {
            return;
        };
        let mut record = record.or_else(|| slot.assembler.take()).unwrap_or_default();
        if record.talkgroup.is_none() && call.talkgroup != 0 {
            record.talkgroup = Some(call.talkgroup);
        }
        if record.source.is_none() && call.source != 0 {
            record.source = Some(call.source);
        }
        if record.target.is_none() && call.kind != CallKind::Group && call.target != 0 {
            record.target = Some(call.target);
        }
        record.emergency |= call.emergency;
        // The engine's alias, never the call assembler's: only the engine
        // knows whether a traffic-side word confirmed the talker before a
        // block arrived, which blocks were the previous talker's, and what
        // this radio was heard with before — the assembler sees one
        // transmission and files every block in it.
        record.talker_alias = call.talker_alias();
        if record.source_wacn.is_none() {
            record.source_wacn = call.source_wacn;
            record.source_system = call.source_system;
        }
        slot.hang_until_ms = now + hang_ms;
        // The next call on this channel may be another talker's, and the
        // infrastructure can still be cycling this one's blocks into it.
        if call.source != 0 && call.alias.halves() != [None, None] {
            slot.stale_alias = Some(StaleAlias {
                radio: call.source,
                halves: call.alias.halves(),
                until_ms: now + STALE_ALIAS_MS,
            });
        }
        self.stats.calls_ended += 1;
        let freq = slot.freq_hz;
        sink(SiteEvent::CallEnded {
            id: call.id,
            record,
            at_ms: now,
            reason,
        });
        if call.kind == CallKind::UnitToUnit {
            sink(SiteEvent::Control(P25Event::PrivateCall {
                start: false,
                frequency_hz: freq,
                source: call.source,
                target: call.target,
                duration_ms: now.saturating_sub(call.started_at_ms),
            }));
        }
    }

    /// Drop a traffic subscription and return its pool slot.
    fn release_slot(&mut self, idx: usize) {
        let slot = self.traffic.swap_remove(idx);
        self.bank.borrow_mut().unsubscribe(slot.handle);
        if let Some(t) = &self.tracker {
            t.borrow_mut().traffic_stopped(slot.freq_hz);
        }
        self.meta.remove(&slot.freq_hz);
    }

    // -- per-call metadata ---------------------------------------------------

    /// Name `source` as the call's talker. `confirmed` is true when a
    /// traffic-side link control word named it — the only evidence that the
    /// transmission on the channel is that radio's, and so the only thing
    /// that lets its alias blocks through (see [`ActiveCall::talker_confirmed`]).
    fn set_talker(
        &mut self,
        idx: usize,
        source: u32,
        confirmed: bool,
        sink: &mut dyn FnMut(SiteEvent),
    ) {
        let now = self.now_ms;
        let slot = &mut self.traffic[idx];
        let Some(call) = slot.call.as_mut() else {
            return;
        };
        if source == 0 {
            return;
        }
        if call.source == source {
            if confirmed && !call.talker_confirmed {
                call.talker_confirmed = true;
                self.release_held_alias(idx, sink);
            }
            return;
        }
        // Blocks held before a traffic-side word named anyone go through the
        // same checks once one does, whoever the grant had named: the stale
        // and foreign rules are what tell the previous talker's blocks apart.
        // A talker change the control channel reports discards them.
        let release = confirmed && !call.talker_confirmed;
        if !release {
            call.held_alias.clear();
        }
        // The previous talker's blocks may still be on the channel.
        if call.source != 0 && call.alias.halves() != [None, None] {
            slot.stale_alias = Some(StaleAlias {
                radio: call.source,
                halves: call.alias.halves(),
                until_ms: now + STALE_ALIAS_MS,
            });
        }
        // A new talker re-sends its own alias blocks (see `CallAssembler`),
        // and its own Source ID Extension if it has one. Carrying either over
        // would attribute the previous talker's identity to this one.
        call.alias.reset();
        call.recalled_alias = None;
        call.talker_confirmed = confirmed;
        call.source_wacn = None;
        call.source_system = None;
        call.source = source;
        let (id, emergency) = (call.id, call.emergency);
        sink(SiteEvent::CallTalker {
            id,
            source,
            emergency,
            alias: None,
            source_wacn: None,
            source_system: None,
            at_ms: now,
        });
        if release {
            self.release_held_alias(idx, sink);
        }
    }

    fn apply_link_control(
        &mut self,
        idx: usize,
        lc: &LinkControl,
        sink: &mut dyn FnMut(SiteEvent),
    ) {
        let now = self.now_ms;
        match lc {
            LinkControl::GroupVoiceChannelUser {
                options,
                talkgroup,
                source,
                ..
            } => {
                if let Some(call) = self.traffic[idx].call.as_mut() {
                    if call.talkgroup == 0 && *talkgroup != 0 {
                        call.talkgroup = *talkgroup;
                    }
                    call.emergency |= options.emergency;
                }
                self.set_talker(idx, *source, true, sink);
            }
            LinkControl::UnitToUnitVoiceChannelUser {
                options,
                target,
                source,
            } => {
                if let Some(call) = self.traffic[idx].call.as_mut() {
                    call.emergency |= options.emergency;
                    if call.target == 0 {
                        call.target = *target;
                    }
                }
                self.set_talker(idx, *source, true, sink);
            }
            // LCO 9 qualifies the 24-bit radio id LCO 0 carries. It names
            // its own radio, so a word for a different radio than the current
            // talker is stale or foreign and must not overwrite the latch.
            LinkControl::SourceIdExtension {
                wacn,
                system,
                source,
            } => {
                let Some(call) = self.traffic[idx].call.as_mut() else {
                    return;
                };
                if call.source != 0 && call.source != *source {
                    return;
                }
                if call.source_wacn == Some(*wacn) && call.source_system == Some(*system) {
                    return;
                }
                call.source_wacn = Some(*wacn);
                call.source_system = Some(*system);
                let (id, source, emergency) = (call.id, call.source, call.emergency);
                let (alias, source_wacn, source_system) =
                    (call.talker_alias(), call.source_wacn, call.source_system);
                sink(SiteEvent::CallTalker {
                    id,
                    source,
                    emergency,
                    alias,
                    source_wacn,
                    source_system,
                    at_ms: now,
                });
            }
            LinkControl::Harris { opcode, word } => {
                let Some((half, block)) = TalkerAliasAssembler::block_of(*opcode, word) else {
                    return;
                };
                let Some(call) = self.traffic[idx].call.as_mut() else {
                    return;
                };
                // Held until a traffic-side word names the talker: before
                // that, the block may be the rest of the previous
                // transmission's cycle (the residual cross-call leak).
                // Released through the same checks once the talker is
                // confirmed, dropped if another talker is named first.
                if !call.talker_confirmed {
                    if call.held_alias.len() < HELD_ALIAS_BLOCKS
                        && !call.held_alias.contains(&(half, block))
                    {
                        call.held_alias.push((half, block));
                    }
                    return;
                }
                self.fold_alias_block(idx, half, block, sink);
            }
            _ => {}
        }
    }

    /// Fold one talker-alias block into the confirmed talker's alias, and
    /// report the alias when it becomes known.
    fn fold_alias_block(
        &mut self,
        idx: usize,
        half: usize,
        block: [u8; ALIAS_BLOCK_LEN],
        sink: &mut dyn FnMut(SiteEvent),
    ) {
        let now = self.now_ms;
        let slot = &mut self.traffic[idx];
        let Some(call) = slot.call.as_mut() else {
            return;
        };
        // The previous talker's own block, still being cycled into this
        // transmission. An all-NUL half is anybody's tail and proves nothing
        // either way; any other block this talker sends means its own cycle
        // has begun, and the window closes.
        if let Some(stale) = slot.stale_alias {
            if stale.radio == call.source || now > stale.until_ms {
                slot.stale_alias = None;
            } else if stale.halves[half] == Some(block) {
                if block != [0; ALIAS_BLOCK_LEN] {
                    return;
                }
            } else {
                slot.stale_alias = None;
            }
        }
        // Somebody else's alias, as this site heard it complete moments ago:
        // the infrastructure cycling another radio's blocks — typically a
        // console's, into the transmission of a unit it patched or answered —
        // through this one. Dropped only on evidence both ways: this radio is
        // known to send something else in that half, *and* another radio was
        // just heard with exactly this block. A radio nothing is known about
        // is never judged.
        if block != [0; ALIAS_BLOCK_LEN]
            && self
                .aliases
                .own_half(call.source, half, now)
                .is_some_and(|own| own != block)
            && self
                .aliases
                .claimed_by_other(call.source, half, &block, now, FOREIGN_ALIAS_MS)
        {
            return;
        }
        // An all-NUL second half is the tail of every alias that fits in its
        // first half, so on its own it says nothing about whose it is: it is
        // taken only to finish a head this talker has already sent. Without
        // this, a leftover pad block spliced onto the new talker's head read
        // `FIRE_ENGINE_8` as `FIRE_EN`.
        if half == 1 && block == [0; ALIAS_BLOCK_LEN] && call.alias.halves()[0].is_none() {
            return;
        }
        let before = call.talker_alias();
        call.alias.put(half, block);
        match (call.alias.to_alias(), call.alias.halves()[0]) {
            // Complete on the air: the radio's alias, remembered.
            (Some(alias), Some(_)) => {
                call.recalled_alias = None;
                self.aliases
                    .remember(call.source, &alias, call.alias.halves(), now);
            }
            // Only the head of a longer field: named only if this radio was
            // heard complete with exactly that head.
            _ => {
                call.recalled_alias = call.alias.head_only().and_then(|h| {
                    self.aliases
                        .complete(call.source, &h, now)
                        .map(str::to_owned)
                });
            }
        }
        // The assembler retracts a complete alias when a contradicting block
        // arrives (a new talker, or a block left over from the previous
        // transmission). Nothing downstream can act on the retraction, so only
        // an alias that is now *known* is reported.
        let alias = call.talker_alias();
        if alias == before {
            return;
        }
        let Some(alias) = alias else {
            return;
        };
        sink(SiteEvent::CallTalker {
            id: call.id,
            source: call.source,
            emergency: call.emergency,
            alias: Some(alias),
            source_wacn: call.source_wacn,
            source_system: call.source_system,
            at_ms: now,
        });
    }

    /// Confirm the current talker from a terminator's link control (TDULC),
    /// but only when it names that same radio: a terminator naming another
    /// radio is the infrastructure announcing the *next* talker.
    fn confirm_from_terminator(
        &mut self,
        idx: usize,
        link_control: &[u8; 9],
        sink: &mut dyn FnMut(SiteEvent),
    ) {
        let named = match LinkControl::parse(link_control) {
            LinkControl::GroupVoiceChannelUser { source, .. }
            | LinkControl::UnitToUnitVoiceChannelUser { source, .. }
            | LinkControl::CallTermination { source } => source,
            _ => return,
        };
        let Some(call) = self.traffic[idx].call.as_mut() else {
            return;
        };
        if named == 0 || named != call.source || call.talker_confirmed {
            return;
        }
        call.talker_confirmed = true;
        self.release_held_alias(idx, sink);
    }

    /// The current talker was just confirmed by a traffic-side word: fold in
    /// the alias blocks held while it was not.
    fn release_held_alias(&mut self, idx: usize, sink: &mut dyn FnMut(SiteEvent)) {
        let held = match self.traffic[idx].call.as_mut() {
            Some(call) => std::mem::take(&mut call.held_alias),
            None => return,
        };
        for (half, block) in held {
            self.fold_alias_block(idx, half, block, sink);
        }
    }

    fn note_crypto(&mut self, idx: usize, algid: u8, key_id: u16, sink: &mut dyn FnMut(SiteEvent)) {
        let now = self.now_ms;
        let Some(call) = self.traffic[idx].call.as_mut() else {
            return;
        };
        if call.algid == Some(algid) && call.key_id == Some(key_id) {
            return;
        }
        call.algid = Some(algid);
        call.key_id = Some(key_id);
        let id = call.id;
        sink(SiteEvent::CallEncryption {
            id,
            algid,
            key_id,
            at_ms: now,
        });
    }

    // -- periodic work -------------------------------------------------------

    fn housekeeping(&mut self, sink: &mut dyn FnMut(SiteEvent)) {
        let now = self.now_ms;
        let inactivity = self.cfg.call_inactivity_ms;
        for i in 0..self.traffic.len() {
            let idle = self.traffic[i]
                .call
                .as_ref()
                .is_some_and(|c| now.saturating_sub(c.last_activity_ms) > inactivity);
            if idle {
                self.close_call(i, EndReason::Inactivity, None, sink);
            }
        }
        let mut i = 0;
        while i < self.traffic.len() {
            if self.traffic[i].call.is_none() && now >= self.traffic[i].hang_until_ms {
                self.release_slot(i);
            } else {
                i += 1;
            }
        }
        // Drop data channels that have gone quiet (no PDU within their dwell).
        let mut i = 0;
        while i < self.data.len() {
            if now >= self.data[i].expire_ms {
                let slot = self.data.swap_remove(i);
                self.bank.borrow_mut().unsubscribe(slot.handle);
            } else {
                i += 1;
            }
        }
        let lost = self
            .control
            .as_ref()
            .is_some_and(|c| now.saturating_sub(c.last_activity_ms) > self.cfg.control_loss_ms);
        if lost {
            self.lose_control(sink);
        }
        if now.saturating_sub(self.last_stats_ms) >= STATS_INTERVAL_MS {
            self.patches.expire(now);
            // Bounded: a frequency nobody has been refused on for longer than a
            // call's idle window is forgotten, so a long run cannot grow this
            // map past the site's channel count.
            let idle = self.cfg.hang_ms + self.cfg.call_inactivity_ms;
            self.skipped
                .retain(|_, (_, last)| now.saturating_sub(*last) <= idle);
            self.emit_stats(sink);
        }
    }

    /// Control channel went silent: tear the site down and re-hunt.
    fn lose_control(&mut self, sink: &mut dyn FnMut(SiteEvent)) {
        let Some(control) = self.control.take() else {
            return;
        };
        self.shutdown(sink);
        self.bank.borrow_mut().unsubscribe(control.handle);
        self.tracker = None;
        self.meta.clear();
        self.patches = PatchBook::new();
        self.stats.control_frequency_hz = 0;
        self.stats.nac = 0;
        self.stats.control_losses += 1;
        sink(SiteEvent::ControlLost {
            frequency_hz: control.freq_hz,
        });
        self.subscribe_candidates();
        sink(SiteEvent::ControlHunting {
            candidates: self.candidates.clone(),
        });
    }

    fn emit_stats(&mut self, sink: &mut dyn FnMut(SiteEvent)) {
        let now = self.now_ms;
        let window_ms = now.saturating_sub(self.last_stats_ms).max(1);
        let mut stats = self.stats();
        stats.control_tsbk_rate = self.window_tsbks as f64 * 1000.0 / window_ms as f64;
        stats.control_frequency_error_hz = if self.control_err_n > 0 {
            self.control_err_sum / self.control_err_n as f64
        } else {
            0.0
        };
        stats.traffic_frequency_error_hz = if self.traffic_err_n > 0 {
            self.traffic_err_sum / self.traffic_err_n as f64
        } else {
            0.0
        };
        let (hunt_offset, hunt_snr) = self.hunt_carrier();
        stats.hunt_carrier_offset_hz = hunt_offset;
        stats.hunt_carrier_snr_db = hunt_snr;
        let control = self.control.as_ref().and_then(|c| c.carrier.estimate());
        stats.control_carrier_offset_hz = control.map(|e| e.offset_hz);
        // Signal quality, all three of it from measurements this engine already
        // owns, so it is per *tuner* however many tuners serve this site.
        //
        // The floor is read before the carrier meters are rolled below, and
        // both terms went through the same dB conversion, so the subtraction is
        // a real SNR rather than two scales differenced.
        stats.noise_floor_dbfs = self
            .bank
            .borrow_mut()
            .power_percentile_dbfs(self.power_window, NOISE_FLOOR_PERCENTILE);
        stats.control_snr_db = match (control, stats.noise_floor_dbfs) {
            (Some(e), Some(floor)) => Some(e.power_dbfs - floor),
            _ => None,
        };
        let decoded = self.window_tsbks + self.window_tsbk_failed;
        stats.tsbk_pass_pct = (self.control.is_some() && decoded > 0)
            .then(|| 100.0 * self.window_tsbks as f64 / decoded as f64);
        stats.sync_losses_per_min = self.window_sync_losses as f64 * 60_000.0 / window_ms as f64;
        // Each window is an independent measurement: a correction applied on
        // this one must not be argued for again by samples it already covered.
        self.reset_carrier_meters();
        self.last_stats_ms = now;
        self.window_tsbks = 0;
        self.window_tsbk_failed = 0;
        self.window_sync_losses = 0;
        self.control_err_sum = 0.0;
        self.control_err_n = 0;
        self.traffic_err_sum = 0.0;
        self.traffic_err_n = 0;
        sink(SiteEvent::Stats(stats));
    }

    /// Fold the control candidates' carrier meters into one offset for the
    /// window, plus the spread that says whether to believe it.
    ///
    /// Two passes, no allocation: find the loudest and quietest candidate that
    /// has a reading, then power-weight the offsets of everything within
    /// [`HUNT_CARRIER_WINDOW_DB`] of the loudest. Power weighting rather than a
    /// plain mean because a candidate 9 dB down is 9 dB less certain about
    /// where its carrier is.
    fn hunt_carrier(&self) -> (Option<f64>, f64) {
        let mut strongest = f64::NEG_INFINITY;
        let mut floor = f64::INFINITY;
        for h in &self.hunt {
            if let Some(e) = h.carrier.estimate() {
                strongest = strongest.max(e.power_dbfs);
                floor = floor.min(e.power_dbfs);
            }
        }
        if !strongest.is_finite() || !floor.is_finite() {
            return (None, 0.0);
        }
        let snr = strongest - floor;
        if snr < HUNT_CARRIER_MIN_SNR_DB {
            return (None, snr);
        }
        let (mut sum, mut weight) = (0.0f64, 0.0f64);
        for h in &self.hunt {
            let Some(e) = h.carrier.estimate() else {
                continue;
            };
            if strongest - e.power_dbfs > HUNT_CARRIER_WINDOW_DB {
                continue;
            }
            let w = 10f64.powf(e.power_dbfs / 10.0);
            sum += e.offset_hz * w;
            weight += w;
        }
        if weight > 0.0 {
            (Some(sum / weight), snr)
        } else {
            (None, snr)
        }
    }

    /// Zero every carrier meter on this engine.
    ///
    /// The recorder calls this straight after a retune: samples measured on the
    /// *old* frequency still argue for the correction that was just applied, so
    /// leaving them in would double it.
    pub fn reset_carrier_meters(&mut self) {
        for h in &mut self.hunt {
            h.carrier.reset();
        }
        if let Some(c) = self.control.as_mut() {
            c.carrier.reset();
        }
        // The noise floor is the other half of the SNR these meters feed, and
        // it is measured on the same samples: rolling one window without the
        // other would difference a fresh signal against a stale floor.
        self.bank.borrow_mut().reset_power_window(self.power_window);
    }
}

impl Drop for SiteEngine {
    /// Hand every subscription and the noise-floor window back to the bank.
    /// Irrelevant for a private bank, which goes with the engine; on a shared
    /// one it stops the filterbank producing channels nobody reads.
    fn drop(&mut self) {
        let Ok(mut bank) = self.bank.try_borrow_mut() else {
            return;
        };
        let handles = self
            .hunt
            .iter()
            .map(|h| h.handle)
            .chain(self.control.as_ref().map(|c| c.handle))
            .chain(self.traffic.iter().map(|t| t.handle))
            .chain(self.data.iter().map(|d| d.handle));
        for handle in handles {
            bank.unsubscribe(handle);
        }
        bank.close_power_window(self.power_window);
    }
}

/// Concatenate the L3Harris alias blocks present so far (printable ASCII).
/// Map a channelizer configuration failure onto the engine's error type.
///
/// Only the capture-rate failures can be hit from [`SiteEngine::new`]: taps
/// are clamped, and per-frequency failures are handled where they occur.
fn map_channelizer_error(e: ChannelizerError) -> P25Error {
    match e {
        ChannelizerError::RateNotOnGrid(rate) => P25Error::RateNotDivisible {
            sample_rate: rate,
            channel_rate: BIN_BANDWIDTH_HZ as u32,
        },
        // An even bin count is exactly "a multiple of two 12.5 kHz bins".
        ChannelizerError::OddBinCount { rate, .. } => P25Error::RateNotDivisible {
            sample_rate: rate,
            channel_rate: 2 * BIN_BANDWIDTH_HZ as u32,
        },
        ChannelizerError::TooFewBins { rate, .. } => P25Error::SampleRateTooLow {
            rate,
            min: 4 * BIN_BANDWIDTH_HZ as u32,
        },
        // Unreachable from `SiteEngine::new`: taps are clamped to the minimum,
        // and frequency/handle errors belong to subscribe time.
        _ => P25Error::InvalidSampleRate,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::LduKind;
    use crate::iden::IdenBand;
    use crate::message::{ChannelRef, ServiceOptions, Tsbk, TsbkMessage, UnitGrantAmbt};
    use crate::payload::{EncryptionSync, HduHeader};

    /// A small, cheap filterbank: 1 Msps → 80 bins, ±487.5 kHz usable.
    const RATE: u32 = 1_000_000;
    const CENTER: f64 = 852_000_000.0;
    const CONTROL: u64 = 851_800_000;
    const BAND: u8 = 1;
    /// 851 MHz + 12.5 kHz × channel.
    fn freq_of(channel: u16) -> u64 {
        851_000_000 + 12_500 * u64::from(channel)
    }

    fn config() -> SiteConfig {
        SiteConfig {
            sample_rate: RATE,
            center_hz: CENTER,
            control_frequencies: vec![CONTROL],
            traffic_pool_size: 4,
            // Timers off by default; each test enables the one it exercises.
            call_inactivity_ms: 60_000,
            control_loss_ms: 600_000,
            ..SiteConfig::default()
        }
    }

    fn engine(cfg: SiteConfig) -> SiteEngine {
        let mut e = SiteEngine::new(cfg).expect("engine builds");
        e.test_acquire(CONTROL);
        e
    }

    fn band() -> IdenBand {
        IdenBand {
            id: BAND,
            base_hz: 851_000_000,
            spacing_hz: 12_500,
            offset_hz: -45_000_000,
            bandwidth_hz: 12_500,
            slots_per_carrier: 1,
            tdma: false,
        }
    }

    fn grant(channel: u16, talkgroup: u16, source: u32) -> TsbkMessage {
        TsbkMessage::GroupVoiceGrant {
            options: ServiceOptions::default(),
            channel: ChannelRef {
                band: BAND,
                channel,
            },
            talkgroup,
            source,
        }
    }

    fn hdu(talkgroup: u16, algid: u8) -> P25Event {
        P25Event::Hdu {
            valid: true,
            data: [0u8; 15],
            header: Some(HduHeader {
                mi: [0u8; 9],
                mfid: 0,
                algid,
                kid: 0,
                talkgroup,
            }),
            corrected: 0,
        }
    }

    fn imbe() -> P25Event {
        P25Event::ImbeFrame {
            ldu: LduKind::Ldu1,
            index: 0,
            frame: [0u8; 18],
        }
    }

    fn group_lc(talkgroup: u16, source: u32) -> P25Event {
        P25Event::LinkControl {
            word: [0u8; 9],
            parsed: LinkControl::GroupVoiceChannelUser {
                options: ServiceOptions::default(),
                talkgroup,
                source_ext_flag: false,
                source,
            },
            corrected: 0,
        }
    }

    fn encryption(algid: u8, kid: u16) -> P25Event {
        P25Event::EncryptionSync {
            word: [0u8; 12],
            parsed: EncryptionSync {
                mi: [1, 2, 3, 4, 5, 6, 7, 8, 9],
                algid,
                kid,
            },
            corrected: 0,
        }
    }

    /// Test-only drivers: they inject the events the DSP would have produced,
    /// so the hunt / pool / hang state machine can be exercised without IQ.
    impl SiteEngine {
        /// Acquire `freq` as the control channel with no signal.
        fn test_acquire(&mut self, freq: u64) {
            for h in std::mem::take(&mut self.hunt) {
                self.bank.borrow_mut().unsubscribe(h.handle);
            }
            let handle = self
                .bank
                .borrow_mut()
                .subscribe(freq as f64)
                .expect("control candidate is in band");
            let tracker = TrunkingTracker::new_shared(freq);
            {
                let mut t = tracker.borrow_mut();
                t.set_pool_size(self.cfg.traffic_pool_size as u32);
                t.set_follow_unit_calls(self.cfg.follow_unit_calls);
            }
            let decoder = self.new_decoder(freq, Some(tracker.clone()));
            self.tracker = Some(tracker);
            self.control = Some(ControlChannel {
                freq_hz: freq,
                nac: 0x293,
                handle,
                decoder,
                last_activity_ms: self.now_ms,
                carrier: CarrierMeter::new(self.out_rate),
            });
            self.stats.control_frequency_hz = freq;
        }

        /// Drive one TSBK through the site tracker, exactly as the control
        /// decoder's dispatch does.
        fn test_tsbk(&mut self, msg: &TsbkMessage, sink: &mut dyn FnMut(SiteEvent)) {
            let now = self.now_ms;
            let tracker = self.tracker.clone().expect("acquired");
            let mut events = Vec::new();
            tracker
                .borrow_mut()
                .process(msg, now, &mut |e| events.push(e));
            self.touch_control();
            for e in events {
                self.handle_control_event(&e, sink);
            }
        }

        /// Drive one multi-block unit-to-unit grant through the site tracker,
        /// exactly as the control decoder's dispatch does for a completed,
        /// block-valid AMBT PDU.
        fn test_unit_grant(
            &mut self,
            grant: &crate::message::UnitGrantAmbt,
            sink: &mut dyn FnMut(SiteEvent),
        ) -> bool {
            let now = self.now_ms;
            let tracker = self.tracker.clone().expect("acquired");
            let mut events = Vec::new();
            let acted = tracker
                .borrow_mut()
                .process_unit_grant(grant, now, &mut |e| events.push(e));
            self.touch_control();
            for e in events {
                self.handle_control_event(&e, sink);
            }
            acted
        }

        /// Feed one decoded event to the traffic channel on `freq`.
        fn test_traffic(&mut self, freq: u64, e: &P25Event, sink: &mut dyn FnMut(SiteEvent)) {
            let i = self.slot_of(freq).expect("traffic channel exists");
            self.handle_traffic_event(i, e, sink);
        }

        /// Advance stream time and run the periodic work.
        fn test_advance(&mut self, ms: u64, sink: &mut dyn FnMut(SiteEvent)) {
            self.now_ms += ms;
            self.samples_consumed = self.now_ms * u64::from(self.cfg.sample_rate) / 1000;
            self.housekeeping(sink);
        }

        fn test_pool_available(&self) -> u32 {
            self.tracker
                .as_ref()
                .map_or(0, |t| t.borrow().pool_available())
        }
    }

    /// Collect events into a Vec, with helpers for the assertions below.
    #[derive(Default)]
    struct Sink(Vec<SiteEvent>);

    impl Sink {
        fn started(&self) -> Vec<CallId> {
            self.0
                .iter()
                .filter_map(|e| match e {
                    SiteEvent::CallStarted { id, .. } => Some(*id),
                    _ => None,
                })
                .collect()
        }
        fn ended(&self) -> Vec<(CallId, EndReason, CallRecord)> {
            self.0
                .iter()
                .filter_map(|e| match e {
                    SiteEvent::CallEnded {
                        id, reason, record, ..
                    } => Some((*id, *reason, record.clone())),
                    _ => None,
                })
                .collect()
        }
        fn voice(&self) -> usize {
            self.0
                .iter()
                .filter(|e| matches!(e, SiteEvent::CallVoice { .. }))
                .count()
        }
    }

    #[test]
    fn unreachable_candidates_are_dropped_and_hunting_is_announced() {
        let mut cfg = config();
        // 900 MHz is far outside a 1 MHz capture centred on 852 MHz.
        cfg.control_frequencies = vec![CONTROL, 900_000_000, 852_000_000];
        let mut engine = SiteEngine::new(cfg).expect("engine builds");
        // 852.0 MHz is the capture centre: the DC bin, never usable.
        assert_eq!(engine.candidates(), [CONTROL]);
        let mut sink = Sink::default();
        engine.process_block(&[], &mut |e| sink.0.push(e));
        assert!(matches!(
            sink.0.first(),
            Some(SiteEvent::ControlHunting { candidates }) if candidates == &[CONTROL]
        ));
        assert_eq!(engine.control_frequency_hz(), None);
    }

    #[test]
    fn rejects_a_capture_rate_off_the_bin_grid() {
        let mut cfg = config();
        cfg.sample_rate = 1_000_001;
        assert!(matches!(
            SiteEngine::new(cfg),
            Err(P25Error::RateNotDivisible { .. })
        ));
    }

    /// A decoded multi-block unit-to-unit grant, as
    /// `parse_ambt_unit_grant` would return it for a captured site's payloads.
    fn unit_grant(channel: u16, source: u32, target: u32, update: bool) -> UnitGrantAmbt {
        UnitGrantAmbt {
            source_wacn: 0xBEE00,
            source_system: 0x1F5,
            source,
            target,
            channel: ChannelRef {
                band: BAND,
                channel,
            },
            update,
            outbound: true,
        }
    }

    /// The private-call records the engine emits on the control pass-through,
    /// as `(start, frequency, source, target, duration_ms)`.
    fn private_records(sink: &Sink) -> Vec<(bool, u64, u32, u32, u64)> {
        sink.0
            .iter()
            .filter_map(|e| match e {
                SiteEvent::Control(P25Event::PrivateCall {
                    start,
                    frequency_hz,
                    source,
                    target,
                    duration_ms,
                }) => Some((*start, *frequency_hz, *source, *target, *duration_ms)),
                _ => None,
            })
            .collect()
    }

    /// An 0:4 opens a private call on the granted channel and the 0:6 updates
    /// that follow keep its dwell open — the same two jobs a group grant and
    /// its updates do, which is the point: nothing about a private call needs
    /// its own state machine.
    #[test]
    fn a_unit_to_unit_grant_starts_a_call_and_its_updates_keep_it() {
        let mut cfg = config();
        cfg.raw_control = true;
        cfg.call_inactivity_ms = 5_000;
        let mut engine = engine(cfg);
        let freq = freq_of(90);
        let mut sink = Sink::default();
        engine.test_tsbk(&TsbkMessage::IdenUpdate(band()), &mut |e| sink.0.push(e));

        assert!(
            engine.test_unit_grant(&unit_grant(90, 9_000_001, 9_000_007, false), &mut |e| {
                sink.0.push(e)
            })
        );
        let started = sink.started();
        assert_eq!(started.len(), 1, "the 0:4 opened exactly one call");
        assert!(matches!(
            sink.0.iter().find(|e| matches!(e, SiteEvent::CallStarted { .. })),
            Some(SiteEvent::CallStarted { frequency_hz, kind: CallKind::UnitToUnit, talkgroup: 0, source: 9_000_001, target: 9_000_007, .. })
                if *frequency_hz == freq
        ));
        assert_eq!(
            private_records(&sink),
            [(true, freq, 9_000_001, 9_000_007, 0)],
            "and said so on the control pass-through"
        );

        // Three seconds of 0:6 updates, each past the tracker's staleness
        // horizon from the last: the call must be one call, not four.
        for _ in 0..3 {
            engine.test_advance(1_000, &mut |e| sink.0.push(e));
            assert!(engine
                .test_unit_grant(&unit_grant(90, 9_000_001, 9_000_007, true), &mut |e| sink
                    .0
                    .push(e)));
        }
        assert_eq!(
            sink.started().len(),
            1,
            "the updates kept the call, not restarted it"
        );
        assert!(sink.ended().is_empty());

        // Silence past the inactivity timer ends it.
        engine.test_advance(6_000, &mut |e| sink.0.push(e));
        let ended = sink.ended();
        assert_eq!(ended.len(), 1);
        assert_eq!(ended[0].0, started[0]);
        assert_eq!(
            ended[0].2.target,
            Some(9_000_007),
            "the called radio is on the record"
        );
        assert_eq!(ended[0].2.source, Some(9_000_001));
        let records = private_records(&sink);
        assert_eq!(records.len(), 2);
        assert_eq!(
            (records[1].0, records[1].2, records[1].3),
            (false, 9_000_001, 9_000_007)
        );
        assert!(
            records[1].4 >= 3_000,
            "the end record carries the call length"
        );
    }

    /// The other party keys up: consecutive grants on one channel swap A and
    /// B inside the hangtime. Each recording must have exactly one talker
    /// (Keyup keys the radio id per call), so the call is ended and a new one
    /// started rather than the talker being swapped underneath it.
    #[test]
    fn a_swapped_pair_on_one_channel_splits_into_two_calls() {
        let mut cfg = config();
        cfg.raw_control = true;
        let mut engine = engine(cfg);
        let freq = freq_of(90);
        let mut sink = Sink::default();
        engine.test_tsbk(&TsbkMessage::IdenUpdate(band()), &mut |e| sink.0.push(e));
        engine.test_unit_grant(&unit_grant(90, 9_000_001, 9_000_007, false), &mut |e| {
            sink.0.push(e)
        });
        engine.test_advance(500, &mut |e| sink.0.push(e));
        // Well inside the hangtime, and the same channel: only the direction
        // changed.
        engine.test_unit_grant(&unit_grant(90, 9_000_007, 9_000_001, false), &mut |e| {
            sink.0.push(e)
        });

        let started = sink.started();
        assert_eq!(started.len(), 2, "one call per talker");
        assert_ne!(started[0], started[1]);
        assert_eq!(sink.ended().len(), 1, "the first was closed, not left open");
        let talkers: Vec<(u32, u32)> = sink
            .0
            .iter()
            .filter_map(|e| match e {
                SiteEvent::CallStarted { source, target, .. } => Some((*source, *target)),
                _ => None,
            })
            .collect();
        assert_eq!(talkers, [(9_000_001, 9_000_007), (9_000_007, 9_000_001)]);
        assert_eq!(
            private_records(&sink)
                .iter()
                .map(|r| (r.0, r.2, r.3))
                .collect::<Vec<_>>(),
            [
                (true, 9_000_001, 9_000_007),
                (false, 9_000_001, 9_000_007),
                (true, 9_000_007, 9_000_001),
            ],
            "both directions are on the record, in order"
        );
        assert_eq!(freq, freq_of(90));
    }

    /// A repeated group grant on one frequency is still one call: the split
    /// rule looks at the pair only for a private call, so nothing about group
    /// following changed.
    #[test]
    fn a_group_grant_with_a_new_talker_is_still_one_call() {
        let mut engine = engine(config());
        let mut sink = Sink::default();
        engine.test_tsbk(&TsbkMessage::IdenUpdate(band()), &mut |e| sink.0.push(e));
        engine.test_tsbk(&grant(88, 100, 7), &mut |e| sink.0.push(e));
        engine.test_advance(200, &mut |e| sink.0.push(e));
        engine.test_tsbk(&grant(88, 100, 9), &mut |e| sink.0.push(e));
        assert_eq!(sink.started().len(), 1);
        assert!(sink.ended().is_empty());
    }

    /// `follow_unit_calls = false`: the grant is decoded and declined, and no
    /// tuner is spent on it.
    #[test]
    fn following_can_be_turned_off() {
        let mut cfg = config();
        cfg.follow_unit_calls = false;
        let mut engine = engine(cfg);
        let mut sink = Sink::default();
        engine.test_tsbk(&TsbkMessage::IdenUpdate(band()), &mut |e| sink.0.push(e));
        let before = engine.test_pool_available();
        assert!(
            !engine.test_unit_grant(&unit_grant(90, 9_000_001, 9_000_007, false), &mut |e| {
                sink.0.push(e)
            }),
            "the tracker reports that it did not act"
        );
        assert!(sink.started().is_empty(), "no call");
        assert!(private_records(&sink).is_empty(), "and no record of one");
        assert_eq!(
            engine.test_pool_available(),
            before,
            "no traffic slot spent"
        );
    }

    #[test]
    fn grant_starts_a_call_and_a_terminator_ends_it() {
        let mut engine = engine(config());
        let freq = freq_of(88);
        let mut sink = Sink::default();
        engine.test_tsbk(&TsbkMessage::IdenUpdate(band()), &mut |e| sink.0.push(e));
        engine.test_tsbk(&grant(88, 100, 7), &mut |e| sink.0.push(e));

        let started = sink.started();
        assert_eq!(started.len(), 1, "one call started: {:?}", sink.0);
        assert!(sink.0.iter().any(|e| matches!(
            e,
            SiteEvent::CallStarted { frequency_hz, talkgroup: 100, source: 7, .. }
                if *frequency_hz == freq
        )));
        assert_eq!(engine.active_calls(), 1);

        engine.test_traffic(freq, &hdu(100, 0x80), &mut |e| sink.0.push(e));
        for _ in 0..9 {
            engine.test_traffic(freq, &imbe(), &mut |e| sink.0.push(e));
        }
        engine.test_traffic(freq, &group_lc(100, 42), &mut |e| sink.0.push(e));
        assert_eq!(sink.voice(), 9);
        assert!(sink
            .0
            .iter()
            .any(|e| matches!(e, SiteEvent::CallEncryption { algid: 0x80, .. })));
        assert!(sink
            .0
            .iter()
            .any(|e| matches!(e, SiteEvent::CallTalker { source: 42, .. })));

        engine.test_traffic(freq, &P25Event::Tdu, &mut |e| sink.0.push(e));
        let ended = sink.ended();
        assert_eq!(ended.len(), 1);
        assert_eq!(ended[0].0, started[0]);
        assert_eq!(ended[0].1, EndReason::Terminator);
        assert_eq!(ended[0].2.talkgroup, Some(100));
        assert_eq!(ended[0].2.source, Some(42));
        assert_eq!(ended[0].2.voice_frames, 9);
        assert!(ended[0].2.terminated);
        assert_eq!(engine.active_calls(), 0);
    }

    #[test]
    fn encryption_is_reported_once_per_change() {
        let mut engine = engine(config());
        let freq = freq_of(88);
        let mut sink = Sink::default();
        engine.test_tsbk(&TsbkMessage::IdenUpdate(band()), &mut |e| sink.0.push(e));
        engine.test_tsbk(&grant(88, 100, 7), &mut |e| sink.0.push(e));
        for _ in 0..3 {
            engine.test_traffic(freq, &encryption(0x84, 0x044c), &mut |e| sink.0.push(e));
        }
        engine.test_traffic(freq, &encryption(0x80, 0), &mut |e| sink.0.push(e));
        let crypto: Vec<_> = sink
            .0
            .iter()
            .filter_map(|e| match e {
                SiteEvent::CallEncryption { algid, key_id, .. } => Some((*algid, *key_id)),
                _ => None,
            })
            .collect();
        assert_eq!(crypto, vec![(0x84, 0x044c), (0x80, 0)]);
    }

    #[test]
    fn hang_holds_the_channel_then_releases_it_to_the_pool() {
        let mut cfg = config();
        cfg.hang_ms = 2_000;
        let mut engine = engine(cfg);
        let freq = freq_of(88);
        let mut sink = Sink::default();
        engine.test_tsbk(&TsbkMessage::IdenUpdate(band()), &mut |e| sink.0.push(e));
        engine.test_tsbk(&grant(88, 100, 7), &mut |e| sink.0.push(e));
        engine.test_traffic(freq, &imbe(), &mut |e| sink.0.push(e));
        engine.test_traffic(freq, &P25Event::Tdu, &mut |e| sink.0.push(e));
        assert_eq!(
            engine.stats().pool_in_use,
            1,
            "subscription survives the call"
        );
        assert_eq!(engine.test_pool_available(), 3);

        engine.test_advance(1_000, &mut |e| sink.0.push(e));
        assert_eq!(
            engine.stats().pool_in_use,
            1,
            "still inside the hang window"
        );

        engine.test_advance(1_500, &mut |e| sink.0.push(e));
        assert_eq!(engine.stats().pool_in_use, 0, "hang expired");
        assert_eq!(engine.test_pool_available(), 4, "channel back in the pool");
    }

    #[test]
    fn a_new_transmission_inside_the_hang_window_is_a_new_call() {
        let mut engine = engine(config());
        let freq = freq_of(88);
        let mut sink = Sink::default();
        engine.test_tsbk(&TsbkMessage::IdenUpdate(band()), &mut |e| sink.0.push(e));
        engine.test_tsbk(&grant(88, 100, 7), &mut |e| sink.0.push(e));
        engine.test_traffic(freq, &imbe(), &mut |e| sink.0.push(e));
        engine.test_traffic(freq, &P25Event::Tdu, &mut |e| sink.0.push(e));
        // Same frequency, no new grant: the follow-on HDU opens a new call id.
        engine.test_advance(500, &mut |e| sink.0.push(e));
        engine.test_traffic(freq, &hdu(100, 0x80), &mut |e| sink.0.push(e));
        engine.test_traffic(freq, &imbe(), &mut |e| sink.0.push(e));
        engine.test_traffic(freq, &P25Event::Tdu, &mut |e| sink.0.push(e));

        let started = sink.started();
        let ended = sink.ended();
        assert_eq!(started.len(), 2, "two calls: {:?}", sink.0);
        assert_ne!(started[0], started[1], "a new call id");
        assert_eq!(ended.len(), 2);
        assert!(ended.iter().all(|(_, r, rec)| *r == EndReason::Terminator
            && rec.voice_frames == 1
            && rec.talkgroup == Some(100)));
        assert_eq!(engine.stats().pool_in_use, 1, "one subscription throughout");
    }

    #[test]
    fn a_silent_call_ends_on_inactivity() {
        let mut cfg = config();
        cfg.call_inactivity_ms = 3_000;
        let mut engine = engine(cfg);
        let mut sink = Sink::default();
        engine.test_tsbk(&TsbkMessage::IdenUpdate(band()), &mut |e| sink.0.push(e));
        engine.test_tsbk(&grant(88, 100, 7), &mut |e| sink.0.push(e));
        engine.test_advance(2_000, &mut |e| sink.0.push(e));
        assert!(sink.ended().is_empty());
        engine.test_advance(1_500, &mut |e| sink.0.push(e));
        let ended = sink.ended();
        assert_eq!(ended.len(), 1);
        assert_eq!(ended[0].1, EndReason::Inactivity);
        // The grant's talkgroup survives even with no traffic-side signaling.
        assert_eq!(ended[0].2.talkgroup, Some(100));
    }

    /// The `(talkgroup, source)` of every call the engine started, in order.
    fn started_calls(sink: &Sink) -> Vec<(u64, u16, u32)> {
        sink.0
            .iter()
            .filter_map(|e| match e {
                SiteEvent::CallStarted {
                    frequency_hz,
                    talkgroup,
                    source,
                    ..
                } => Some((*frequency_hz, *talkgroup, *source)),
                _ => None,
            })
            .collect()
    }

    fn grant_update(channel: u16, talkgroup: u16) -> TsbkMessage {
        TsbkMessage::GroupVoiceGrantUpdate {
            a: (
                ChannelRef {
                    band: BAND,
                    channel,
                },
                talkgroup,
            ),
            b: None,
        }
    }

    /// A frequency re-granted inside its hang window,
    /// where the first thing heard is a grant *update* (which names no radio),
    /// used to start the new call attributed to the previous call's talker.
    #[test]
    fn a_new_call_on_a_reused_frequency_does_not_inherit_the_previous_caller() {
        let mut cfg = config();
        cfg.call_inactivity_ms = 3_000;
        let mut engine = engine(cfg);
        let freq = freq_of(88);
        let mut sink = Sink::default();
        engine.test_tsbk(&TsbkMessage::IdenUpdate(band()), &mut |e| sink.0.push(e));
        engine.test_tsbk(&grant(88, 100, 1_620_719), &mut |e| sink.0.push(e));
        // The call ends on inactivity; the channel is still held (hang time).
        engine.test_advance(3_500, &mut |e| sink.0.push(e));
        assert_eq!(sink.ended().len(), 1);

        // Another talkgroup on the same channel, heard only as an update.
        engine.test_tsbk(&grant_update(88, 200), &mut |e| sink.0.push(e));
        // Then an explicit grant names a radio, and that call ends too.
        engine.test_advance(3_500, &mut |e| sink.0.push(e));
        engine.test_tsbk(&grant(88, 200, 9_000_038), &mut |e| sink.0.push(e));
        engine.test_advance(3_500, &mut |e| sink.0.push(e));
        // The same talkgroup again after the tracker went stale, heard only as
        // an update: a new call, and not necessarily the same radio.
        engine.test_tsbk(&grant_update(88, 200), &mut |e| sink.0.push(e));

        assert_eq!(
            started_calls(&sink),
            vec![
                (freq, 100, 1_620_719),
                (freq, 200, 0),
                (freq, 200, 9_000_038),
                (freq, 200, 0),
            ],
            "no call is attributed to the radio that keyed the previous one"
        );
        let ended = sink.ended();
        assert_eq!(ended[1].2.talkgroup, Some(200));
        assert_eq!(ended[1].2.source, None, "and its record names nobody");
    }

    /// End to end: one corrupt IDEN for the band in use
    /// does not move the next grant; two identical ones do.
    #[test]
    fn one_corrupt_iden_cannot_move_a_known_band_under_the_grants() {
        let mut engine = engine(config());
        let mut sink = Sink::default();
        let shifted = IdenBand {
            base_hz: 851_100_000,
            ..band()
        };
        engine.test_tsbk(&TsbkMessage::IdenUpdate(band()), &mut |e| sink.0.push(e));
        engine.test_tsbk(&TsbkMessage::IdenUpdate(shifted), &mut |e| sink.0.push(e));
        engine.test_tsbk(&grant(88, 100, 7), &mut |e| sink.0.push(e));
        // The second identical decode: the site really moved the band.
        engine.test_tsbk(&TsbkMessage::IdenUpdate(shifted), &mut |e| sink.0.push(e));
        engine.test_tsbk(&grant(90, 200, 8), &mut |e| sink.0.push(e));
        assert_eq!(
            started_calls(&sink),
            vec![(freq_of(88), 100, 7), (851_100_000 + 12_500 * 90, 200, 8)]
        );
    }

    #[test]
    fn a_full_pool_reports_exhaustion_once_per_call() {
        let mut cfg = config();
        cfg.traffic_pool_size = 1;
        let mut engine = engine(cfg);
        let mut sink = Sink::default();
        engine.test_tsbk(&TsbkMessage::IdenUpdate(band()), &mut |e| sink.0.push(e));
        engine.test_tsbk(&grant(88, 100, 7), &mut |e| sink.0.push(e));
        // Second call on another frequency: nothing left in the pool.
        engine.test_tsbk(&grant(96, 200, 8), &mut |e| sink.0.push(e));
        engine.test_tsbk(&grant(96, 200, 8), &mut |e| sink.0.push(e));
        let exhausted: Vec<_> = sink
            .0
            .iter()
            .filter_map(|e| match e {
                SiteEvent::PoolExhausted {
                    frequency_hz,
                    talkgroup,
                } => Some((*frequency_hz, *talkgroup)),
                _ => None,
            })
            .collect();
        assert_eq!(exhausted, vec![(freq_of(96), 200)]);
        assert_eq!(engine.stats().pool_in_use, 1);
        assert_eq!(engine.stats().pool_exhausted, 1);
    }

    #[test]
    fn a_different_talkgroup_on_a_live_frequency_reassigns_the_channel() {
        let mut engine = engine(config());
        let freq = freq_of(88);
        let mut sink = Sink::default();
        engine.test_tsbk(&TsbkMessage::IdenUpdate(band()), &mut |e| sink.0.push(e));
        engine.test_tsbk(&grant(88, 100, 7), &mut |e| sink.0.push(e));
        engine.test_traffic(freq, &imbe(), &mut |e| sink.0.push(e));
        engine.test_tsbk(&grant(88, 200, 9), &mut |e| sink.0.push(e));

        let ended = sink.ended();
        assert_eq!(ended.len(), 1, "the old call is closed: {:?}", sink.0);
        assert_eq!(ended[0].1, EndReason::PoolReassigned);
        assert_eq!(ended[0].2.talkgroup, Some(100));
        let started = sink.started();
        assert_eq!(started.len(), 2);
        assert!(sink.0.iter().any(|e| matches!(
            e,
            SiteEvent::CallStarted { id, talkgroup: 200, .. } if *id == started[1]
        )));
        assert_eq!(engine.stats().pool_in_use, 1, "the same channel is reused");
    }

    #[test]
    fn shutdown_ends_open_calls_and_frees_the_pool() {
        let mut engine = engine(config());
        let freq = freq_of(88);
        let mut sink = Sink::default();
        engine.test_tsbk(&TsbkMessage::IdenUpdate(band()), &mut |e| sink.0.push(e));
        engine.test_tsbk(&grant(88, 100, 7), &mut |e| sink.0.push(e));
        engine.test_traffic(freq, &imbe(), &mut |e| sink.0.push(e));
        engine.shutdown(&mut |e| sink.0.push(e));
        let ended = sink.ended();
        assert_eq!(ended.len(), 1);
        assert_eq!(ended[0].1, EndReason::Shutdown);
        assert_eq!(ended[0].2.voice_frames, 1);
        assert!(!ended[0].2.terminated, "cut off, not terminated");
        assert_eq!(engine.stats().pool_in_use, 0);
        assert_eq!(engine.active_calls(), 0);
    }

    #[test]
    fn control_loss_tears_the_site_down_and_re_hunts() {
        let mut cfg = config();
        cfg.control_loss_ms = 5_000;
        let mut engine = engine(cfg);
        let mut sink = Sink::default();
        engine.test_tsbk(&TsbkMessage::IdenUpdate(band()), &mut |e| sink.0.push(e));
        engine.test_tsbk(&grant(88, 100, 7), &mut |e| sink.0.push(e));
        engine.test_advance(6_000, &mut |e| sink.0.push(e));

        assert_eq!(engine.control_frequency_hz(), None);
        assert!(sink.0.iter().any(
            |e| matches!(e, SiteEvent::ControlLost { frequency_hz } if *frequency_hz == CONTROL)
        ));
        assert!(sink.0.iter().any(|e| matches!(
            e,
            SiteEvent::ControlHunting { candidates } if candidates == &[CONTROL]
        )));
        let ended = sink.ended();
        assert_eq!(ended.len(), 1, "the open call is torn down");
        assert_eq!(ended[0].1, EndReason::Shutdown);
        assert_eq!(engine.stats().pool_in_use, 0);
        assert_eq!(engine.stats().control_losses, 1);
    }

    #[test]
    fn stats_are_emitted_every_five_seconds_of_stream_time() {
        let mut engine = engine(config());
        let mut sink = Sink::default();
        engine.test_advance(STATS_INTERVAL_MS - 1, &mut |e| sink.0.push(e));
        assert!(!sink.0.iter().any(|e| matches!(e, SiteEvent::Stats(_))));
        engine.test_advance(2, &mut |e| sink.0.push(e));
        let stats: Vec<_> = sink
            .0
            .iter()
            .filter_map(|e| match e {
                SiteEvent::Stats(s) => Some(*s),
                _ => None,
            })
            .collect();
        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0].stream_ms, STATS_INTERVAL_MS + 1);
        assert_eq!(stats[0].pool_size, 4);
        assert_eq!(stats[0].control_frequency_hz, CONTROL);
    }

    // -- per-tuner signal quality --------------------------------------------

    /// Every [`SiteEvent::Stats`] the sink saw, newest last.
    fn windows(sink: &Sink) -> Vec<SiteStats> {
        sink.0
            .iter()
            .filter_map(|e| match e {
                SiteEvent::Stats(s) => Some(*s),
                _ => None,
            })
            .collect()
    }

    /// A CRC-valid TSBK, as the control decoder would hand one over.
    fn passing_tsbk() -> P25Event {
        P25Event::Tsbk {
            tsbk: Tsbk {
                last_block: true,
                encrypted: false,
                opcode: 0x3d,
                vendor: 0,
                message: TsbkMessage::IdenUpdate(band()),
                raw: [0u8; 12],
            },
            corrected: 0,
            crc_corrected: 0,
        }
    }

    /// A TSBK that failed CRC used to be invisible to the stats: the window
    /// counted passes only, so a control channel decoding half its bits looked
    /// exactly like a quiet one. Both halves of the ratio are counted now.
    #[test]
    fn failed_tsbks_are_counted_into_the_pass_rate() {
        let mut engine = engine(config());
        let mut sink = Sink::default();
        for _ in 0..3 {
            engine.handle_control_event(&passing_tsbk(), &mut |e| sink.0.push(e));
        }
        engine.handle_control_event(&P25Event::TsbkFailed, &mut |e| sink.0.push(e));
        engine.test_advance(STATS_INTERVAL_MS, &mut |e| sink.0.push(e));

        let s = *windows(&sink).last().expect("a window was emitted");
        assert_eq!(s.tsbk_pass_pct, Some(75.0));
        // A failure is not a decode: the TSBK counters stay counts of TSBKs.
        assert_eq!(s.control_tsbks, 3);
        assert!((s.control_tsbk_rate - 0.6).abs() < 1e-9, "{s:?}");
    }

    /// Both decode-quality figures are per window, not cumulative: a clean
    /// window after a bad one reads clean, which is the whole point of putting
    /// them on a live panel.
    #[test]
    fn decode_quality_is_reset_every_window() {
        let mut engine = engine(config());
        let mut sink = Sink::default();
        engine.handle_control_event(&passing_tsbk(), &mut |e| sink.0.push(e));
        engine.handle_control_event(&P25Event::TsbkFailed, &mut |e| sink.0.push(e));
        engine.handle_control_event(&P25Event::SyncLoss { bits: 96 }, &mut |e| sink.0.push(e));
        engine.handle_control_event(&P25Event::SyncLoss { bits: 96 }, &mut |e| sink.0.push(e));
        engine.test_advance(STATS_INTERVAL_MS, &mut |e| sink.0.push(e));
        for _ in 0..4 {
            engine.handle_control_event(&passing_tsbk(), &mut |e| sink.0.push(e));
        }
        engine.test_advance(STATS_INTERVAL_MS, &mut |e| sink.0.push(e));

        let w = windows(&sink);
        assert_eq!(w.len(), 2);
        assert_eq!(w[0].tsbk_pass_pct, Some(50.0));
        // Two losses in a five-second window is 24 a minute.
        assert!((w[0].sync_losses_per_min - 24.0).abs() < 1e-9, "{:?}", w[0]);
        assert_eq!(w[1].tsbk_pass_pct, Some(100.0));
        assert_eq!(w[1].sync_losses_per_min, 0.0);
        // The lifetime counter keeps both, which is what it is for.
        assert_eq!(w[1].sync_losses, 2);
    }

    /// "100% of nothing" is not a quality reading, and neither is a pass rate
    /// for a tuner that has no control channel to decode.
    #[test]
    fn a_pass_rate_needs_a_lock_and_a_tsbk() {
        let mut sink = Sink::default();
        // Locked, but the window carried no TSBK at all.
        let mut locked = engine(config());
        locked.test_advance(STATS_INTERVAL_MS, &mut |e| sink.0.push(e));
        assert_eq!(windows(&sink).last().expect("a window").tsbk_pass_pct, None);

        // Still hunting: nothing here is measured, so nothing is reported.
        let mut sink = Sink::default();
        let mut hunting = SiteEngine::new(config()).expect("engine builds");
        hunting.test_advance(STATS_INTERVAL_MS, &mut |e| sink.0.push(e));
        let s = *windows(&sink).last().expect("a window");
        assert_eq!(s.tsbk_pass_pct, None);
        assert_eq!(s.control_snr_db, None);
        assert_eq!(s.sync_losses_per_min, 0.0);
    }

    /// No samples have reached the filterbank, so there is no floor and no SNR
    /// — absent, not a fabricated `-120 dBFS`.
    #[test]
    fn an_unfed_engine_reports_no_noise_floor() {
        let mut engine = engine(config());
        let mut sink = Sink::default();
        engine.test_advance(STATS_INTERVAL_MS, &mut |e| sink.0.push(e));
        let s = *windows(&sink).last().expect("a window");
        assert_eq!(s.noise_floor_dbfs, None);
        assert_eq!(s.control_snr_db, None);
    }

    /// End to end: a tone on the control frequency riding a noisy capture,
    /// straight through `process_block`, comes out as an SNR in the stats.
    #[test]
    fn a_locked_control_channel_reports_its_snr() {
        // 1.2 s of capture: the control carrier meter wants a full second of
        // its own 25 kHz output before it will report at all. The stats window
        // is then reached by advancing stream time rather than by feeding four
        // more seconds of samples, which measures nothing new.
        const SAMPLES: usize = RATE as usize * 6 / 5;
        const AMPLITUDE: f32 = 0.9;

        let mut engine = engine(config());
        let mut sink = Sink::default();
        // The control frequency's offset from the capture centre.
        let w = std::f64::consts::TAU * (CONTROL as f64 - CENTER) / f64::from(RATE);
        let mut rng: u32 = 0x1357_9bdf;
        let mut noise = || {
            rng ^= rng << 13;
            rng ^= rng >> 17;
            rng ^= rng << 5;
            (rng >> 8) as f32 / 8_388_608.0 - 1.0
        };
        let mut block = Vec::with_capacity(4096);
        let mut done = 0usize;
        while done < SAMPLES {
            let n = 4096.min(SAMPLES - done);
            block.clear();
            for k in 0..n {
                let a = w * (done + k) as f64;
                block.push(Complex::new(
                    AMPLITUDE * a.cos() as f32 + noise(),
                    AMPLITUDE * a.sin() as f32 + noise(),
                ));
            }
            engine.process_block(&block, &mut |e| sink.0.push(e));
            done += n;
        }
        engine.test_advance(STATS_INTERVAL_MS, &mut |e| sink.0.push(e));

        let s = *windows(&sink).last().expect("a window was emitted");
        let floor = s.noise_floor_dbfs.expect("the filterbank ran");
        let snr = s.control_snr_db.expect("the control channel is locked");
        // A 0.9-amplitude tone against uniform ±1 noise spread over 80 bins:
        // roughly 0.81 / (0.667/80), i.e. about 20 dB. Wide bounds — the test
        // is that the two terms share a scale, not that the DSP is a
        // calibration standard.
        assert!(
            (10.0_f64..30.0).contains(&snr),
            "control SNR {snr} dB over a {floor} dBFS floor"
        );
        assert!((-30.0..-12.0).contains(&floor), "noise floor {floor} dBFS");
    }

    #[test]
    fn out_of_band_grants_do_not_consume_the_pool() {
        let mut engine = engine(config());
        let mut sink = Sink::default();
        engine.test_tsbk(&TsbkMessage::IdenUpdate(band()), &mut |e| sink.0.push(e));
        // Channel 4000 → 901 MHz, far outside the 1 MHz capture.
        engine.test_tsbk(&grant(4_000, 100, 7), &mut |e| sink.0.push(e));
        assert_eq!(engine.stats().pool_in_use, 0);
        assert_eq!(engine.stats().out_of_band_grants, 1);
        assert_eq!(engine.test_pool_available(), 4, "slot returned to the pool");
        assert!(sink.started().is_empty());
    }

    // -- Phase 2 (TDMA) grants -------------------------------------------------

    /// A grant on a TDMA channel — the band
    /// plan's opcode-51 channel type says so — is refused before anything is
    /// spent on it: no channel, no call, no pool slot, no `GrantOut` for a
    /// sibling tuner. It is counted once per call, and flagged as the first
    /// for its talkgroup only once, so the recorder logs each talkgroup once.
    #[test]
    fn a_grant_on_a_tdma_channel_is_refused_and_counted_once_per_call() {
        let tdma = IdenBand {
            id: BAND + 1,
            base_hz: 851_000_000,
            spacing_hz: 12_500,
            offset_hz: -45_000_000,
            bandwidth_hz: 12_500,
            slots_per_carrier: 2,
            tdma: true,
        };
        // Channel 2k is slot 1 and 2k + 1 slot 2 of carrier k.
        let tdma_grant = |channel: u16, talkgroup: u16| TsbkMessage::GroupVoiceGrant {
            options: ServiceOptions::default(),
            channel: ChannelRef {
                band: BAND + 1,
                channel,
            },
            talkgroup,
            source: 7,
        };
        let refused = |sink: &Sink| -> Vec<(u64, u8, u16, bool)> {
            sink.0
                .iter()
                .filter_map(|e| match e {
                    SiteEvent::TdmaRefused {
                        frequency_hz,
                        timeslot,
                        talkgroup,
                        first_for_talkgroup,
                        ..
                    } => Some((*frequency_hz, *timeslot, *talkgroup, *first_for_talkgroup)),
                    _ => None,
                })
                .collect()
        };
        let mut engine = engine(config());
        let mut sink = Sink::default();
        engine.test_tsbk(&TsbkMessage::IdenUpdate(band()), &mut |e| sink.0.push(e));
        engine.test_tsbk(&TsbkMessage::IdenUpdate(tdma), &mut |e| sink.0.push(e));

        // One call on slot 1 of carrier 88, refreshed; then another talkgroup
        // on slot 2 of the same carrier; then the first talkgroup again on a
        // different carrier.
        for _ in 0..3 {
            engine.test_tsbk(&tdma_grant(176, 100), &mut |e| sink.0.push(e));
        }
        engine.test_tsbk(&tdma_grant(177, 200), &mut |e| sink.0.push(e));
        engine.test_tsbk(&tdma_grant(180, 100), &mut |e| sink.0.push(e));

        assert_eq!(
            refused(&sink),
            vec![
                (freq_of(88), 1, 100, true),
                (freq_of(88), 2, 200, true),
                (freq_of(90), 1, 100, false),
            ]
        );
        assert_eq!(engine.stats().calls_tdma_refused, 3);
        assert!(sink.started().is_empty(), "no call: {:?}", sink.0);
        assert_eq!(engine.stats().pool_in_use, 0, "no channel subscribed");
        assert_eq!(engine.test_pool_available(), 4, "no pool slot spent");
        assert!(
            !sink
                .0
                .iter()
                .any(|e| matches!(e, SiteEvent::GrantOut { .. })),
            "nothing is handed to another tuner"
        );

        // The FDMA band beside it is recorded exactly as before.
        engine.test_tsbk(&grant(96, 300, 8), &mut |e| sink.0.push(e));
        assert_eq!(sink.started().len(), 1);
        assert_eq!(engine.stats().calls_tdma_refused, 3);
    }

    // -- the skip list --------------------------------------------------------

    fn skipping(tgids: &[u32]) -> (SiteConfig, std::sync::Arc<trunkd_core::SkipList>) {
        let skip = trunkd_core::SkipList::new(tgids.iter().copied());
        (
            SiteConfig {
                skip_talkgroups: Some(skip.clone()),
                ..config()
            },
            skip,
        )
    }

    fn skipped(sink: &Sink) -> Vec<(u64, u16)> {
        sink.0
            .iter()
            .filter_map(|e| match e {
                SiteEvent::CallSkipped {
                    frequency_hz,
                    talkgroup,
                } => Some((*frequency_hz, *talkgroup)),
                _ => None,
            })
            .collect()
    }

    /// The whole point: a grant for a talkgroup the operator turned off costs
    /// no channel, no call and no pool slot — and the grant is still decoded, so
    /// everything the archive learns from the control channel it still learns.
    #[test]
    fn a_grant_for_a_skipped_talkgroup_claims_nothing() {
        let (cfg, _skip) = skipping(&[100]);
        let mut engine = engine(cfg);
        let mut sink = Sink::default();
        engine.test_tsbk(&TsbkMessage::IdenUpdate(band()), &mut |e| sink.0.push(e));
        engine.test_tsbk(&grant(88, 100, 7), &mut |e| sink.0.push(e));

        assert!(sink.started().is_empty(), "no call: {:?}", sink.0);
        assert_eq!(engine.active_calls(), 0);
        assert_eq!(engine.stats().pool_in_use, 0, "no channel subscribed");
        assert_eq!(engine.test_pool_available(), 4, "the pool slot went back");
        assert_eq!(skipped(&sink), vec![(freq_of(88), 100)]);
        assert_eq!(engine.stats().calls_skipped_by_policy, 1);

        // A talkgroup that is not on the list is recorded exactly as before.
        engine.test_tsbk(&grant(96, 200, 8), &mut |e| sink.0.push(e));
        assert_eq!(sink.started().len(), 1);
        assert_eq!(engine.stats().calls_skipped_by_policy, 1);
    }

    /// The control channel re-grants a live call every few hundred ms and each
    /// repeat lands in the refusal (the pool slot was handed straight back), so
    /// the counter has to be a count of calls rather than of grant messages.
    #[test]
    fn a_repeated_grant_for_one_skipped_call_is_reported_once() {
        let (cfg, _skip) = skipping(&[100]);
        let mut engine = engine(cfg);
        let mut sink = Sink::default();
        engine.test_tsbk(&TsbkMessage::IdenUpdate(band()), &mut |e| sink.0.push(e));
        for _ in 0..5 {
            engine.test_tsbk(&grant(88, 100, 7), &mut |e| sink.0.push(e));
        }
        assert_eq!(skipped(&sink), vec![(freq_of(88), 100)]);
        assert_eq!(engine.stats().calls_skipped_by_policy, 1);

        // A different talkgroup landing on the same channel is a different
        // call, skipped or not.
        engine.test_tsbk(&grant(88, 101, 7), &mut |e| sink.0.push(e));
        assert_eq!(
            engine.stats().calls_skipped_by_policy,
            1,
            "101 is not on it"
        );
    }

    /// Keyup can add a talkgroup while the recorder runs, and the very next
    /// grant is refused — no restart, nothing rebuilt.
    #[test]
    fn keyup_can_add_a_talkgroup_mid_run_and_a_call_already_open_finishes() {
        let (cfg, skip) = skipping(&[]);
        let mut engine = engine(cfg);
        let freq = freq_of(88);
        let mut sink = Sink::default();
        engine.test_tsbk(&TsbkMessage::IdenUpdate(band()), &mut |e| sink.0.push(e));
        engine.test_tsbk(&grant(88, 100, 7), &mut |e| sink.0.push(e));
        assert_eq!(sink.started().len(), 1);

        skip.apply(Some(trunkd_core::SkipSpec {
            tgids: vec![100],
            revision: "r1".to_owned(),
        }));

        // The open call keeps running and ends on its own terminator: pulling
        // the audio out from under a call in progress would leave a truncated
        // recording, which helps nobody.
        engine.test_traffic(freq, &imbe(), &mut |e| sink.0.push(e));
        engine.test_traffic(freq, &P25Event::Tdu, &mut |e| sink.0.push(e));
        let ended = sink.ended();
        assert_eq!(ended.len(), 1);
        assert_eq!(ended[0].1, EndReason::Terminator);
        assert_eq!(ended[0].2.voice_frames, 1);

        // The next grant for it is refused.
        engine.test_advance(engine.cfg.hang_ms + 1, &mut |e| sink.0.push(e));
        engine.test_tsbk(&grant(88, 100, 7), &mut |e| sink.0.push(e));
        assert_eq!(sink.started().len(), 1, "no second call: {:?}", sink.0);
        assert_eq!(engine.stats().calls_skipped_by_policy, 1);
    }

    /// A skipped grant is not published either: a peer recorder holds the same
    /// list, so routing it there would only move the refusal one hop.
    #[test]
    fn a_skipped_grant_is_not_published_to_another_tuner() {
        let (mut cfg, _skip) = skipping(&[100]);
        cfg.role = EngineRole::Control;
        cfg.publish_grants = true;
        let mut engine = engine(cfg);
        let mut sink = Sink::default();
        engine.test_tsbk(&TsbkMessage::IdenUpdate(band()), &mut |e| sink.0.push(e));
        engine.test_tsbk(&grant(88, 100, 7), &mut |e| sink.0.push(e));
        assert!(published(&sink).is_empty(), "nothing routed: {:?}", sink.0);
        assert_eq!(engine.stats().grants_published, 0);
        assert_eq!(engine.stats().calls_skipped_by_policy, 1);
        assert_eq!(engine.test_pool_available(), 4);

        // An unskipped grant on the same front still routes.
        engine.test_tsbk(&grant(96, 200, 8), &mut |e| sink.0.push(e));
        assert_eq!(published(&sink), vec![freq_of(96)]);
    }

    /// Unit-to-unit and telephone calls have no talkgroup to match — both
    /// report 0 — so they can never be skipped by a talkgroup list.
    #[test]
    fn a_private_call_is_never_skipped_by_a_talkgroup_list() {
        let (cfg, skip) = skipping(&[100]);
        skip.apply(Some(trunkd_core::SkipSpec {
            tgids: vec![100],
            revision: "r1".to_owned(),
        }));
        let mut engine = engine(cfg);
        let mut sink = Sink::default();
        engine.test_tsbk(&TsbkMessage::IdenUpdate(band()), &mut |e| sink.0.push(e));
        engine.test_unit_grant(&unit_grant(88, 7, 9, false), &mut |e| sink.0.push(e));
        assert_eq!(
            sink.started().len(),
            1,
            "the private call ran: {:?}",
            sink.0
        );
        assert_eq!(engine.stats().calls_skipped_by_policy, 0);
    }

    /// A regroup TSBK, as the control decoder delivers it.
    fn regroup(supergroup: u16, member: u16) -> P25Event {
        P25Event::Tsbk {
            tsbk: Tsbk {
                last_block: true,
                encrypted: false,
                opcode: 0x30,
                vendor: 0xa4,
                message: TsbkMessage::HarrisGroupRegroup {
                    regroup_options: 0xa0,
                    supergroup,
                    key_id: 0,
                    target: u32::from(member),
                    target_is_talkgroup: true,
                },
                raw: [0u8; 12],
            },
            corrected: 0,
            crc_corrected: 0,
        }
    }

    /// A patch pulls other talkgroups into the one being granted: refusing it
    /// would silently drop traffic the operator still wants, so a patched call
    /// is skipped only when *every* member is skipped too.
    #[test]
    fn a_patched_call_is_skipped_only_when_every_member_is() {
        let (cfg, skip) = skipping(&[100, 300]);
        let mut engine = engine(cfg);
        let mut sink = Sink::default();
        engine.test_tsbk(&TsbkMessage::IdenUpdate(band()), &mut |e| sink.0.push(e));
        // 100 is patched to 200, which the operator has *not* turned off.
        engine.handle_control_event(&regroup(100, 200), &mut |e| sink.0.push(e));
        engine.test_tsbk(&grant(88, 100, 7), &mut |e| sink.0.push(e));
        assert_eq!(sink.started().len(), 1, "the patch is still wanted");
        assert_eq!(engine.stats().calls_skipped_by_policy, 0);

        // 300 is patched only to talkgroups that are themselves skipped.
        skip.apply(Some(trunkd_core::SkipSpec {
            tgids: vec![100, 300, 301],
            revision: "r1".to_owned(),
        }));
        engine.handle_control_event(&regroup(300, 301), &mut |e| sink.0.push(e));
        engine.test_tsbk(&grant(96, 300, 8), &mut |e| sink.0.push(e));
        assert_eq!(sink.started().len(), 1, "no new call: {:?}", sink.0);
        assert_eq!(skipped(&sink), vec![(freq_of(96), 300)]);
    }

    // -- multi-tuner core (W1) ------------------------------------------------

    fn published(sink: &Sink) -> Vec<u64> {
        sink.0
            .iter()
            .filter_map(|e| match e {
                SiteEvent::GrantOut { frequency_hz, .. } => Some(*frequency_hz),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn an_auto_front_with_no_publisher_keeps_the_single_tuner_path() {
        // publish_grants defaults to false: an out-of-band grant is counted, not
        // published — byte-for-byte the historical behavior.
        let mut engine = engine(config());
        let mut sink = Sink::default();
        engine.test_tsbk(&TsbkMessage::IdenUpdate(band()), &mut |e| sink.0.push(e));
        engine.test_tsbk(&grant(4_000, 100, 7), &mut |e| sink.0.push(e));
        assert!(published(&sink).is_empty(), "nothing published");
        assert_eq!(engine.stats().out_of_band_grants, 1);
        assert_eq!(engine.stats().grants_published, 0);
    }

    #[test]
    fn an_auto_front_with_a_publisher_routes_an_out_of_band_grant() {
        let mut engine = engine(SiteConfig {
            role: EngineRole::Auto,
            publish_grants: true,
            ..config()
        });
        let mut sink = Sink::default();
        engine.test_tsbk(&TsbkMessage::IdenUpdate(band()), &mut |e| sink.0.push(e));
        // 901 MHz, outside the 1 MHz capture: published, not counted out-of-band.
        engine.test_tsbk(&grant(4_000, 100, 7), &mut |e| sink.0.push(e));
        assert_eq!(published(&sink), vec![901_000_000]);
        assert_eq!(engine.stats().grants_published, 1);
        assert_eq!(engine.stats().out_of_band_grants, 0);
        assert!(sink.started().is_empty());
    }

    #[test]
    fn an_auto_front_publishes_out_of_band_but_records_in_band_grants() {
        // A band partition: this tuner records only [852.0, 852.2] MHz.
        let mut engine = engine(SiteConfig {
            role: EngineRole::Auto,
            publish_grants: true,
            bands: vec![(freq_of(80), freq_of(96))],
            ..config()
        });
        let mut sink = Sink::default();
        engine.test_tsbk(&TsbkMessage::IdenUpdate(band()), &mut |e| sink.0.push(e));
        // Channel 88 → 852.1 MHz: inside the band → recorded locally.
        engine.test_tsbk(&grant(88, 100, 7), &mut |e| sink.0.push(e));
        // Channel 60 → 851.75 MHz: inside the capture but outside the band →
        // published for another tuner.
        engine.test_tsbk(&grant(60, 200, 9), &mut |e| sink.0.push(e));

        assert_eq!(sink.started().len(), 1, "the in-band grant is recorded");
        assert!(sink.0.iter().any(|e| matches!(
            e,
            SiteEvent::CallStarted { frequency_hz, .. } if *frequency_hz == freq_of(88)
        )));
        assert_eq!(published(&sink), vec![freq_of(60)]);
    }

    #[test]
    fn a_control_front_publishes_every_grant() {
        let mut engine = engine(SiteConfig {
            role: EngineRole::Control,
            publish_grants: true,
            ..config()
        });
        let mut sink = Sink::default();
        engine.test_tsbk(&TsbkMessage::IdenUpdate(band()), &mut |e| sink.0.push(e));
        // Channel 88 → 852.1 MHz: well inside the capture, yet a control front
        // records nothing itself — it routes.
        engine.test_tsbk(&grant(88, 100, 7), &mut |e| sink.0.push(e));
        assert_eq!(published(&sink), vec![freq_of(88)]);
        assert!(
            sink.started().is_empty(),
            "control front records no traffic"
        );
        assert_eq!(engine.stats().grants_published, 1);
    }

    #[test]
    fn a_voice_front_follows_a_grant_handed_to_it() {
        let mut engine = SiteEngine::new(SiteConfig {
            role: EngineRole::Voice,
            control_frequencies: Vec::new(),
            ..config()
        })
        .expect("voice engine builds");
        let mut sink = Sink::default();
        let freq = freq_of(88);
        let info = GrantInfo {
            kind: CallKind::Group,
            talkgroup: 100,
            target: 100,
            source: 7,
        };
        engine.follow(freq, info, 0, &mut |e| sink.0.push(e));
        assert_eq!(sink.started().len(), 1, "the followed grant opens a call");
        assert!(sink.0.iter().any(|e| matches!(
            e,
            SiteEvent::CallStarted { frequency_hz, talkgroup: 100, source: 7, .. }
                if *frequency_hz == freq
        )));

        engine.test_traffic(freq, &hdu(100, 0x80), &mut |e| sink.0.push(e));
        for _ in 0..9 {
            engine.test_traffic(freq, &imbe(), &mut |e| sink.0.push(e));
        }
        engine.test_traffic(freq, &P25Event::Tdu, &mut |e| sink.0.push(e));
        assert_eq!(sink.voice(), 9);
        assert_eq!(sink.ended().len(), 1, "a terminator ends the followed call");
    }

    #[test]
    fn a_data_front_counts_a_follow_and_records_nothing() {
        let mut engine = SiteEngine::new(SiteConfig {
            role: EngineRole::Data,
            control_frequencies: Vec::new(),
            ..config()
        })
        .expect("data engine builds");
        let mut sink = Sink::default();
        engine.follow(freq_of(88), GrantInfo::default(), 0, &mut |e| {
            sink.0.push(e)
        });
        assert!(sink.started().is_empty(), "the data decode path is a stub");
        assert_eq!(engine.stats().data_grants, 1);
        assert_eq!(engine.active_calls(), 0);
    }

    #[test]
    fn call_ids_are_namespaced_by_the_configured_base() {
        // Two tuners serving one site get disjoint id ranges from `call_id_base`,
        // so their call ids never collide downstream.
        let mut a = engine(config());
        let mut b = engine(SiteConfig {
            call_id_base: 1 << 48,
            ..config()
        });
        let mut sink = Sink::default();
        a.test_tsbk(&TsbkMessage::IdenUpdate(band()), &mut |e| sink.0.push(e));
        a.test_tsbk(&grant(88, 100, 7), &mut |e| sink.0.push(e));
        b.test_tsbk(&TsbkMessage::IdenUpdate(band()), &mut |e| sink.0.push(e));
        b.test_tsbk(&grant(90, 101, 8), &mut |e| sink.0.push(e));

        let ids: Vec<u64> = sink.started().iter().map(|c| c.0).collect();
        assert_eq!(ids, vec![1, (1 << 48) + 1], "disjoint id ranges: {ids:?}");
    }

    /// Build the traffic-channel event carrying one L3Harris alias block.
    fn alias_block(opcode: crate::payload::HarrisLcOpcode, text: &[u8; 7]) -> P25Event {
        let mut word = [0u8; 9];
        word[1] = 0xA4;
        word[2..9].copy_from_slice(text);
        P25Event::LinkControl {
            word,
            parsed: LinkControl::Harris { opcode, word },
            corrected: 0,
        }
    }

    /// A Group Voice Channel User LC, which opens the call the blocks attach
    /// to and names its talker.
    fn talker_lc(talkgroup: u16, source: u32) -> P25Event {
        P25Event::LinkControl {
            word: [0u8; 9],
            parsed: LinkControl::GroupVoiceChannelUser {
                options: crate::message::ServiceOptions::from_byte(0),
                talkgroup,
                source_ext_flag: false,
                source,
            },
            corrected: 0,
        }
    }

    fn reported_aliases(sink: &Sink) -> Vec<String> {
        sink.0
            .iter()
            .filter_map(|e| match e {
                SiteEvent::CallTalker { alias, .. } => alias.clone(),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn assembles_a_live_talker_alias() {
        use crate::payload::HarrisLcOpcode::*;
        // A captured block sequence (unit name replaced):
        //   32 a4 554e49545f585f  "UNIT_X_"   field[0..7]
        //   33 a4 42415345310000  "BASE1\0\0" field[7..14]
        let mut engine = engine(config());
        let freq = freq_of(88);
        let mut sink = Sink::default();
        engine.test_tsbk(&TsbkMessage::IdenUpdate(band()), &mut |e| sink.0.push(e));
        engine.test_tsbk(&grant(88, 100, 7), &mut |e| sink.0.push(e));
        engine.test_traffic(freq, &talker_lc(100, 7), &mut |e| sink.0.push(e));

        let head = alias_block(TalkerAlias1, b"UNIT_X_");
        engine.test_traffic(freq, &head, &mut |e| sink.0.push(e));
        assert!(
            reported_aliases(&sink).is_empty(),
            "a full first block means the field runs on — nothing to report yet"
        );
        engine.test_traffic(freq, &alias_block(TalkerAlias2, b"BASE1\0\0"), &mut |e| {
            sink.0.push(e)
        });
        // Blocks 3/4 repeat 1/2 verbatim, and the cycle repeats per superframe:
        // neither may re-report or double-append.
        engine.test_traffic(freq, &alias_block(TalkerAlias3, b"UNIT_X_"), &mut |e| {
            sink.0.push(e)
        });
        engine.test_traffic(freq, &alias_block(TalkerAlias4, b"BASE1\0\0"), &mut |e| {
            sink.0.push(e)
        });
        engine.test_traffic(freq, &head, &mut |e| sink.0.push(e));
        assert_eq!(reported_aliases(&sink), vec!["UNIT_X_BASE1".to_string()]);
    }

    #[test]
    fn a_stray_alias_block_never_opens_a_call() {
        use crate::payload::HarrisLcOpcode::*;
        // The alias cycle runs on past the transmission's terminator. Those
        // trailing blocks belong to the talker that just stopped; letting them
        // open a call of their own files one talker's alias under the next
        // talker's call.
        let mut engine = engine(config());
        let freq = freq_of(88);
        let mut sink = Sink::default();
        engine.test_tsbk(&TsbkMessage::IdenUpdate(band()), &mut |e| sink.0.push(e));
        engine.test_tsbk(&grant(88, 100, 7), &mut |e| sink.0.push(e));
        engine.test_traffic(freq, &talker_lc(100, 7), &mut |e| sink.0.push(e));
        engine.test_traffic(freq, &P25Event::Tdu, &mut |e| sink.0.push(e));
        let opened = sink.started().len();

        engine.test_traffic(freq, &alias_block(TalkerAlias1, b"DISPATC"), &mut |e| {
            sink.0.push(e)
        });
        engine.test_traffic(
            freq,
            &alias_block(TalkerAlias2, b"H\0\0\0\0\0\0"),
            &mut |e| sink.0.push(e),
        );
        assert_eq!(
            sink.started().len(),
            opened,
            "trailing alias blocks must not open a call"
        );
        assert!(reported_aliases(&sink).is_empty(), "and must name nobody");
    }

    /// The alias each ended call was filed under, in order.
    fn filed_aliases(sink: &Sink) -> Vec<Option<String>> {
        sink.0
            .iter()
            .filter_map(|e| match e {
                SiteEvent::CallEnded { record, .. } => Some(record.talker_alias.clone()),
                _ => None,
            })
            .collect()
    }

    /// A terminator whose link control is LCO 15, releasing `source`.
    fn termination(source: u32) -> P25Event {
        let mut link_control = [0u8; 9];
        link_control[0] = 0x0F;
        link_control[6..9].copy_from_slice(&source.to_be_bytes()[1..]);
        P25Event::TduLc {
            valid: true,
            link_control,
            corrected: 0,
        }
    }

    /// A block that arrives before any traffic-side word has named the
    /// talker is held, and filed only once one does — here the talker's own
    /// head, sent in the first LDU1 ahead of its LCO 0.
    #[test]
    fn alias_blocks_are_held_until_the_talker_is_confirmed() {
        use crate::payload::HarrisLcOpcode::*;
        let mut engine = engine(config());
        let freq = freq_of(88);
        let mut sink = Sink::default();
        engine.test_tsbk(&TsbkMessage::IdenUpdate(band()), &mut |e| sink.0.push(e));
        engine.test_tsbk(&grant(88, 100, 7), &mut |e| sink.0.push(e));
        engine.test_traffic(freq, &hdu(100, 0x80), &mut |e| sink.0.push(e));
        engine.test_traffic(freq, &alias_block(TalkerAlias1, b"UNIT_9\0"), &mut |e| {
            sink.0.push(e)
        });
        assert!(
            reported_aliases(&sink).is_empty(),
            "only the grant has named radio 7 so far"
        );
        engine.test_traffic(freq, &talker_lc(100, 7), &mut |e| sink.0.push(e));
        assert_eq!(reported_aliases(&sink), vec!["UNIT_9".to_string()]);
        engine.test_traffic(freq, &P25Event::Tdu, &mut |e| sink.0.push(e));
        assert_eq!(filed_aliases(&sink), vec![Some("UNIT_9".to_string())]);
    }

    /// A short transmission whose only traffic-side word naming its talker is
    /// the terminator: the held head is filed as the call closes.
    #[test]
    fn a_terminator_naming_the_talker_confirms_it() {
        use crate::payload::HarrisLcOpcode::*;
        let mut engine = engine(config());
        let freq = freq_of(88);
        let mut sink = Sink::default();
        engine.test_tsbk(&TsbkMessage::IdenUpdate(band()), &mut |e| sink.0.push(e));
        engine.test_tsbk(&grant(88, 100, 7), &mut |e| sink.0.push(e));
        engine.test_traffic(freq, &hdu(100, 0x80), &mut |e| sink.0.push(e));
        engine.test_traffic(freq, &alias_block(TalkerAlias1, b"A20\0\0\0\0"), &mut |e| {
            sink.0.push(e)
        });
        engine.test_traffic(freq, &termination(7), &mut |e| sink.0.push(e));
        assert_eq!(filed_aliases(&sink), vec![Some("A20".to_string())]);

        // A terminator naming another radio confirms nothing.
        engine.test_tsbk(&grant(88, 101, 8), &mut |e| sink.0.push(e));
        engine.test_traffic(freq, &hdu(101, 0x80), &mut |e| sink.0.push(e));
        engine.test_traffic(freq, &alias_block(TalkerAlias1, b"A21\0\0\0\0"), &mut |e| {
            sink.0.push(e)
        });
        engine.test_traffic(freq, &termination(9), &mut |e| sink.0.push(e));
        assert_eq!(filed_aliases(&sink), vec![Some("A20".to_string()), None]);
    }

    /// The residual cross-call leak (2026-09-21 replay): the infrastructure
    /// keeps cycling the previous talker's blocks into the next transmission —
    /// before the new talker's first LCO 0 (`ENG_301` onto console `UNIT_A1`)
    /// and after it (`A33`). Neither is filed under the new talker, and a pad
    /// tail left over from anyone never completes the new talker's head.
    #[test]
    fn the_previous_talkers_blocks_are_not_filed_under_the_next() {
        use crate::payload::HarrisLcOpcode::*;
        let mut engine = engine(config());
        let freq = freq_of(88);
        let mut sink = Sink::default();
        let feed = |engine: &mut SiteEngine, sink: &mut Sink, e: P25Event| {
            engine.test_traffic(freq, &e, &mut |ev| sink.0.push(ev))
        };
        engine.test_tsbk(&TsbkMessage::IdenUpdate(band()), &mut |_| {});
        engine.test_tsbk(&grant(88, 100, 9_000_043), &mut |_| {});
        // The field unit's transmission, alias complete.
        feed(&mut engine, &mut sink, hdu(100, 0x80));
        feed(&mut engine, &mut sink, talker_lc(100, 9_000_043));
        feed(
            &mut engine,
            &mut sink,
            alias_block(TalkerAlias1, b"ENG_301"),
        );
        feed(
            &mut engine,
            &mut sink,
            alias_block(TalkerAlias2, b"\0\0\0\0\0\0\0"),
        );
        feed(&mut engine, &mut sink, P25Event::Tdu);
        // The console answers: the unit's head arrives before the console's
        // first LCO 0, its pad tail after, and its head again after that.
        feed(&mut engine, &mut sink, hdu(100, 0x80));
        feed(
            &mut engine,
            &mut sink,
            alias_block(TalkerAlias3, b"ENG_301"),
        );
        feed(&mut engine, &mut sink, talker_lc(100, 9_000_102));
        feed(
            &mut engine,
            &mut sink,
            alias_block(TalkerAlias4, b"\0\0\0\0\0\0\0"),
        );
        feed(
            &mut engine,
            &mut sink,
            alias_block(TalkerAlias1, b"ENG_301"),
        );
        feed(&mut engine, &mut sink, P25Event::Tdu);
        assert_eq!(
            filed_aliases(&sink),
            vec![Some("ENG_301".to_string()), None],
            "the console's call names nobody rather than the unit it answered"
        );
    }

    /// Per-radio alias memory: `UNIT_A1` exactly fills its first half, so a
    /// call that carries only that half cannot be named from the air alone —
    /// but radio 9000102 was heard with it complete, so it is. A radio with no
    /// complete sighting is still never named from a head.
    #[test]
    fn a_head_alone_is_completed_from_the_radios_known_alias() {
        use crate::payload::HarrisLcOpcode::*;
        let mut engine = engine(config());
        let freq = freq_of(88);
        let mut sink = Sink::default();
        engine.test_tsbk(&TsbkMessage::IdenUpdate(band()), &mut |_| {});
        let call = |engine: &mut SiteEngine, sink: &mut Sink, radio: u32, blocks: &[P25Event]| {
            engine.test_tsbk(&grant(88, 100, radio), &mut |e| sink.0.push(e));
            engine.test_traffic(freq, &hdu(100, 0x80), &mut |e| sink.0.push(e));
            engine.test_traffic(freq, &talker_lc(100, radio), &mut |e| sink.0.push(e));
            for b in blocks {
                engine.test_traffic(freq, b, &mut |e| sink.0.push(e));
            }
            engine.test_traffic(freq, &P25Event::Tdu, &mut |e| sink.0.push(e));
        };
        let head = alias_block(TalkerAlias1, b"UNIT_A1");
        let pad = alias_block(TalkerAlias2, b"\0\0\0\0\0\0\0");
        // No sighting yet: the head alone names nobody.
        call(
            &mut engine,
            &mut sink,
            9_000_102,
            std::slice::from_ref(&head),
        );
        // A complete sighting, then a short call with only the head.
        call(&mut engine, &mut sink, 9_000_102, &[head.clone(), pad]);
        call(
            &mut engine,
            &mut sink,
            9_000_102,
            std::slice::from_ref(&head),
        );
        // Another radio sending the same head is not named by 9000102's alias.
        call(
            &mut engine,
            &mut sink,
            9_000_106,
            std::slice::from_ref(&head),
        );
        assert_eq!(
            filed_aliases(&sink),
            vec![
                None,
                Some("UNIT_A1".to_string()),
                Some("UNIT_A1".to_string()),
                None
            ]
        );
    }

    /// A console's alias cycled into a unit's transmission right after the
    /// console was heard with it: dropped, because the unit is known to send
    /// its own. A unit nothing is known about would not be judged.
    #[test]
    fn another_radios_recent_alias_is_not_filed_under_a_known_radio() {
        use crate::payload::HarrisLcOpcode::*;
        let mut engine = engine(config());
        let mut sink = Sink::default();
        engine.test_tsbk(&TsbkMessage::IdenUpdate(band()), &mut |_| {});
        let call =
            |engine: &mut SiteEngine, sink: &mut Sink, ch: u16, radio: u32, blocks: &[&[u8; 7]]| {
                let freq = freq_of(ch);
                engine.test_tsbk(&grant(ch, 100, radio), &mut |e| sink.0.push(e));
                engine.test_traffic(freq, &hdu(100, 0x80), &mut |e| sink.0.push(e));
                engine.test_traffic(freq, &talker_lc(100, radio), &mut |e| sink.0.push(e));
                for (i, b) in blocks.iter().enumerate() {
                    let op = if i % 2 == 0 {
                        TalkerAlias1
                    } else {
                        TalkerAlias2
                    };
                    engine.test_traffic(freq, &alias_block(op, b), &mut |e| sink.0.push(e));
                }
                engine.test_traffic(freq, &P25Event::Tdu, &mut |e| sink.0.push(e));
            };
        // The car, heard complete; then the console, on another channel.
        call(
            &mut engine,
            &mut sink,
            88,
            9_000_048,
            &[b"JC_CAR5", b"3\0\0\0\0\0\0"],
        );
        call(
            &mut engine,
            &mut sink,
            90,
            9_000_107,
            &[b"UNIT_A7", b"\0\0\0\0\0\0\0"],
        );
        // The car keys up and the console's blocks come through its channel.
        call(
            &mut engine,
            &mut sink,
            88,
            9_000_048,
            &[b"UNIT_A7", b"\0\0\0\0\0\0\0"],
        );
        assert_eq!(
            filed_aliases(&sink),
            vec![
                Some("JC_CAR53".to_string()),
                Some("UNIT_A7".to_string()),
                None
            ]
        );
    }

    #[test]
    fn a_talker_change_retires_the_previous_alias() {
        use crate::payload::HarrisLcOpcode::*;
        let mut engine = engine(config());
        let freq = freq_of(88);
        let mut sink = Sink::default();
        engine.test_tsbk(&TsbkMessage::IdenUpdate(band()), &mut |e| sink.0.push(e));
        engine.test_tsbk(&grant(88, 100, 7), &mut |e| sink.0.push(e));
        engine.test_traffic(freq, &talker_lc(100, 7), &mut |e| sink.0.push(e));
        engine.test_traffic(freq, &alias_block(TalkerAlias1, b"UNIT_X_"), &mut |e| {
            sink.0.push(e)
        });
        engine.test_traffic(freq, &alias_block(TalkerAlias2, b"BASE1\0\0"), &mut |e| {
            sink.0.push(e)
        });
        assert_eq!(reported_aliases(&sink), vec!["UNIT_X_BASE1".to_string()]);
        // New talker: its own head arrives, but the old tail must not finish it.
        engine.test_traffic(freq, &talker_lc(100, 9), &mut |e| sink.0.push(e));
        engine.test_traffic(freq, &alias_block(TalkerAlias1, b"DISPATC"), &mut |e| {
            sink.0.push(e)
        });
        assert_eq!(
            reported_aliases(&sink),
            vec!["UNIT_X_BASE1".to_string()],
            "the new talker is unnamed until its own tail arrives"
        );
        engine.test_traffic(
            freq,
            &alias_block(TalkerAlias2, b"H\0\0\0\0\0\0"),
            &mut |e| sink.0.push(e),
        );
        assert_eq!(
            reported_aliases(&sink),
            vec!["UNIT_X_BASE1".to_string(), "DISPATCH".to_string()]
        );
    }

    fn sndcp_grant(channel: u16, target: u32) -> TsbkMessage {
        TsbkMessage::SndcpDataChannelGrant {
            options: crate::message::DataServiceOptions::from_byte(0x10),
            downlink: ChannelRef {
                band: BAND,
                channel,
            },
            uplink: ChannelRef {
                band: BAND,
                channel: 4095,
            },
            target,
        }
    }

    #[test]
    fn a_default_auto_front_does_not_follow_data_grants() {
        // data_follow defaults to false: the single downlink tuner stays on the
        // voice fast path and spends no channelizer slot on coordinate-less
        // outbound data PDUs.
        let mut engine = engine(config());
        assert!(!engine.cfg.data_follow, "data_follow is off by default");
        let mut sink = Sink::default();
        engine.test_tsbk(&TsbkMessage::IdenUpdate(band()), &mut |e| sink.0.push(e));
        engine.test_tsbk(&sndcp_grant(100, 9_100_355), &mut |e| sink.0.push(e));
        assert!(
            engine.data.is_empty(),
            "no data follow when the flag is off"
        );
        assert_eq!(engine.stats().data_grants, 0);
    }

    #[test]
    fn a_data_grant_follows_the_data_channel_and_expires_when_quiet() {
        let mut cfg = config();
        cfg.data_follow = true;
        cfg.hang_ms = 1_000;
        cfg.call_inactivity_ms = 1_000;
        let mut engine = engine(cfg);
        let mut sink = Sink::default();
        // The band must be known for the data-channel grant to resolve.
        engine.test_tsbk(&TsbkMessage::IdenUpdate(band()), &mut |e| sink.0.push(e));
        // Channel 100 → 852.25 MHz, inside the 852 MHz ±487.5 kHz capture.
        let data_freq = freq_of(100);
        engine.test_tsbk(&sndcp_grant(100, 9_100_355), &mut |e| sink.0.push(e));

        assert_eq!(engine.data.len(), 1, "one data channel followed");
        assert_eq!(engine.data[0].freq_hz, data_freq);
        assert_eq!(engine.stats().data_grants, 1);
        // No voice call was opened by the data follow.
        assert!(sink.started().is_empty());

        // A repeat grant refreshes the same follow, does not add a second.
        engine.test_tsbk(&sndcp_grant(100, 9_100_355), &mut |e| sink.0.push(e));
        assert_eq!(engine.data.len(), 1);
        assert_eq!(engine.stats().data_grants, 1);

        // After the dwell (hang + inactivity) with no PDU, the slot is dropped.
        engine.test_advance(2_500, &mut |e| sink.0.push(e));
        assert!(engine.data.is_empty(), "quiet data channel is released");
    }

    #[test]
    fn a_data_grant_off_the_capture_is_not_followed() {
        let mut cfg = config();
        cfg.data_follow = true;
        let mut engine = engine(cfg);
        let mut sink = Sink::default();
        engine.test_tsbk(&TsbkMessage::IdenUpdate(band()), &mut |e| sink.0.push(e));
        // Channel 4000 → far outside the 1 MHz capture.
        engine.test_tsbk(&sndcp_grant(4000, 1), &mut |e| sink.0.push(e));
        assert!(
            engine.data.is_empty(),
            "out-of-span data channel is skipped"
        );
    }

    // -- one filterbank per capture -------------------------------------------

    /// Two more candidates either side of `CONTROL`, inside the 1 MHz capture.
    const WEST: u64 = 851_900_000;
    const EAST: u64 = 852_100_000;

    /// Two sites that both list `CONTROL` as a candidate, each with one of
    /// its own — the "same control frequency announced by two sites" case.
    fn two_sites() -> (SiteConfig, SiteConfig) {
        let mut a = config();
        a.control_frequencies = vec![CONTROL, WEST];
        let mut b = config();
        b.control_frequencies = vec![CONTROL, EAST];
        (a, b)
    }

    /// A 0.9-amplitude tone on `CONTROL` over uniform noise, in 4 Ki blocks.
    fn noisy_control_blocks(samples: usize) -> Vec<Vec<Complex<f32>>> {
        let w = std::f64::consts::TAU * (CONTROL as f64 - CENTER) / f64::from(RATE);
        let mut rng: u32 = 0x2468_ace0;
        let mut noise = || {
            rng ^= rng << 13;
            rng ^= rng >> 17;
            rng ^= rng << 5;
            (rng >> 8) as f32 / 8_388_608.0 - 1.0
        };
        let mut blocks = Vec::new();
        let mut done = 0usize;
        while done < samples {
            let n = 4096.min(samples - done);
            blocks.push(
                (0..n)
                    .map(|k| {
                        let a = w * (done + k) as f64;
                        Complex::new(
                            0.9 * a.cos() as f32 + noise(),
                            0.9 * a.sin() as f32 + noise(),
                        )
                    })
                    .collect(),
            );
            done += n;
        }
        blocks
    }

    #[test]
    fn two_sites_on_one_bank_run_one_filterbank_and_share_a_candidate() {
        use crate::dsp::channelizer::channelizers_built_on_this_thread;

        let (a_cfg, b_cfg) = two_sites();
        let built = channelizers_built_on_this_thread();
        let bank = SiteEngine::bank_for(&a_cfg).expect("bank builds");
        let mut a = SiteEngine::with_bank(a_cfg, bank.clone()).expect("site a attaches");
        let mut b = SiteEngine::with_bank(b_cfg, bank.clone()).expect("site b attaches");
        assert_eq!(
            channelizers_built_on_this_thread() - built,
            1,
            "two sites, one filterbank"
        );
        assert!(std::rc::Rc::ptr_eq(a.channel_bank(), b.channel_bank()));

        // CONTROL is wanted by both hunts and produced once.
        {
            let bk = bank.borrow();
            assert_eq!(bk.subscription_count(), 4, "each site subscribes its own");
            assert_eq!(bk.channel_count(), 3, "CONTROL, WEST, EAST");
            assert_eq!(bk.channelizer().subscription_count(), 3);
        }

        // One filterbank pass per block, however many sites read it.
        let blocks = noisy_control_blocks(40_960);
        let (mut sa, mut sb) = (Sink::default(), Sink::default());
        for block in &blocks {
            bank.borrow_mut().process_block(block);
            a.process_channels(&mut |e| sa.0.push(e));
            b.process_channels(&mut |e| sb.0.push(e));
        }
        assert_eq!(bank.borrow().filterbank_blocks(), 40_960 / 40);
        assert_eq!(a.stats().stream_ms, 40, "stream time follows the bank");
        assert_eq!(b.stats().stream_ms, 40);

        // A site going away takes only its own subscriptions with it.
        drop(b);
        let bk = bank.borrow();
        assert_eq!(bk.subscription_count(), 2);
        assert_eq!(bk.channel_count(), 2, "CONTROL stays for site a, EAST goes");
        drop(bk);
        drop(a);
        assert_eq!(bank.borrow().channel_count(), 0);
    }

    /// Engines sharing a bank report exactly what engines with filterbanks of
    /// their own report: the same hunt carrier readings off the shared
    /// candidate, and each its own noise floor, window for window.
    #[test]
    fn shared_engines_measure_exactly_what_private_ones_do() {
        let (a_cfg, b_cfg) = two_sites();
        let blocks = noisy_control_blocks(RATE as usize * 6 / 5);

        let mut private = [
            SiteEngine::new(a_cfg.clone()).unwrap(),
            SiteEngine::new(b_cfg.clone()).unwrap(),
        ];
        let bank = SiteEngine::bank_for(&a_cfg).unwrap();
        let mut shared = [
            SiteEngine::with_bank(a_cfg, bank.clone()).unwrap(),
            SiteEngine::with_bank(b_cfg, bank.clone()).unwrap(),
        ];
        let mut sinks: [Sink; 4] = Default::default();
        for block in &blocks {
            for (e, s) in private.iter_mut().zip(&mut sinks[..2]) {
                e.process_block(block, &mut |ev| s.0.push(ev));
            }
            bank.borrow_mut().process_block(block);
            for (e, s) in shared.iter_mut().zip(&mut sinks[2..]) {
                e.process_channels(&mut |ev| s.0.push(ev));
            }
        }
        // Site a rolls its meters (as a retune would) before the window
        // closes; site b does not. The shared floor must not be rolled for b.
        private[0].reset_carrier_meters();
        shared[0].reset_carrier_meters();
        for (e, s) in private
            .iter_mut()
            .chain(shared.iter_mut())
            .zip(sinks.iter_mut())
        {
            e.test_advance(STATS_INTERVAL_MS, &mut |ev| s.0.push(ev));
        }

        let [pa, pb, sa, sb] = sinks.map(|s| windows(&s));
        assert_eq!(pa, sa, "site a: shared bank == private filterbank");
        assert_eq!(pb, sb, "site b: shared bank == private filterbank");
        let b_stats = sb.last().expect("a window");
        assert!(b_stats.noise_floor_dbfs.is_some(), "b kept its floor");
        assert!(
            b_stats.hunt_carrier_offset_hz.is_some(),
            "the shared candidate carried b's hunt reading: {b_stats:?}"
        );
        assert_eq!(
            sa.last().expect("a window").noise_floor_dbfs,
            None,
            "a rolled its own"
        );
    }

    #[test]
    fn a_bank_built_for_another_capture_is_refused() {
        let bank = SiteEngine::bank_for(&config()).unwrap();
        let mut moved = config();
        moved.center_hz += 12_500.0;
        assert_eq!(
            SiteEngine::with_bank(moved, bank.clone()).err(),
            Some(P25Error::ChannelBankMismatch {
                what: "centre frequency"
            })
        );
        let mut faster = config();
        faster.sample_rate *= 2;
        assert_eq!(
            SiteEngine::with_bank(faster, bank.clone()).err(),
            Some(P25Error::ChannelBankMismatch {
                what: "sample rate"
            })
        );
        assert_eq!(bank.borrow().subscription_count(), 0, "nothing subscribed");
    }
    // -- talker-alias replay over a day's event log ---------------------------

    /// Replays a recorder's raw event log (`[recorder] event_log_dir`, one
    /// JSON line per signaling event, gunzipped) through this engine's
    /// trunking, call and talker-alias code: control-channel TSBKs through the
    /// tracker, and each followed channel's HDU / LDU1 link control / TDULC
    /// through the traffic path, in log order and on the log's clock. Voice
    /// frames are not in the log, so what it measures is the signaling —
    /// which alias each call ends up filed under — and it prints, for the
    /// radios whose calls carried an alias, every radio seen with more than
    /// one alias and the calls that disagree with their radio's majority.
    ///
    /// ```text
    /// TRUNKD_ALIAS_REPLAY=events/2026-09-21.jsonl \
    ///   cargo t -p trunkd-p25 --lib alias_replay -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "needs a day's event log: TRUNKD_ALIAS_REPLAY=<events/YYYY-MM-DD.jsonl>"]
    fn alias_replay_over_an_event_log() {
        use std::collections::BTreeMap;
        use std::io::BufRead;

        let Ok(path) = std::env::var("TRUNKD_ALIAS_REPLAY") else {
            eprintln!("TRUNKD_ALIAS_REPLAY is not set; nothing to replay");
            return;
        };
        let hex = |s: &str| -> Vec<u8> {
            (0..s.len() / 2)
                .filter_map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok())
                .collect()
        };
        // "2026-09-21T00:00:03.03Z" → ms of the day.
        let ms_of_day = |at: &str| -> Option<u64> {
            let t = at.split('T').nth(1)?.trim_end_matches('Z');
            let mut parts = t.split(':');
            let h: u64 = parts.next()?.parse().ok()?;
            let m: u64 = parts.next()?.parse().ok()?;
            let sec: f64 = parts.next()?.parse().ok()?;
            Some((h * 3600 + m * 60) * 1000 + (sec * 1000.0).round() as u64)
        };

        const CONTROL: u64 = 851_787_500;
        let mut engine = SiteEngine::new(SiteConfig {
            sample_rate: 20_000_000,
            center_hz: 855_500_000.0,
            control_frequencies: vec![CONTROL],
            traffic_pool_size: 20,
            // The log has gaps (restarts); nothing here is a lost control
            // channel.
            control_loss_ms: u64::MAX / 4,
            ..SiteConfig::default()
        })
        .expect("engine builds");
        engine.test_acquire(CONTROL);

        // Call id → (last talker, last alias reported live for that talker).
        let mut live: HashMap<CallId, (u32, Option<String>)> = HashMap::new();
        // (radio, alias) of every ended call that carried one.
        let mut filed: Vec<(u32, String)> = Vec::new();
        let mut ended = 0u64;
        let mut on_event = |e: SiteEvent| match e {
            SiteEvent::CallTalker {
                id, source, alias, ..
            } => {
                let entry = live.entry(id).or_insert((source, None));
                if entry.0 != source {
                    *entry = (source, None);
                }
                if alias.is_some() {
                    entry.1 = alias;
                }
            }
            SiteEvent::CallEnded { id, record, .. } => {
                ended += 1;
                let last = live.remove(&id);
                // What the recorder uploads (`pipeline/calls.rs`): the
                // record's alias, else the last live alias of the talker the
                // record names.
                let alias = record.talker_alias.clone().or_else(|| {
                    last.filter(|(src, _)| Some(*src) == record.source)
                        .and_then(|(_, a)| a)
                });
                if let (Some(radio), Some(alias)) = (record.source, alias) {
                    filed.push((radio, alias));
                }
            }
            _ => {}
        };

        let file = std::fs::File::open(&path).expect("event log opens");
        let mut lines = 0u64;
        for line in std::io::BufReader::new(file).lines() {
            let Ok(line) = line else { continue };
            let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            let (Some(at), Some(ch), Some(kind), Some(h)) = (
                v["at"].as_str().and_then(ms_of_day),
                v["ch"].as_str(),
                v["type"].as_str(),
                v["hex"].as_str(),
            ) else {
                continue;
            };
            lines += 1;
            if at > engine.now_ms {
                engine.test_advance(at - engine.now_ms, &mut on_event);
            }
            let bytes = hex(h);
            if ch == "control" {
                if kind != "tsbk" || bytes.len() != 12 {
                    continue;
                }
                let mut bits = crate::bits::BitBuf::default();
                for byte in &bytes {
                    for k in (0..8).rev() {
                        bits.push(byte >> k & 1 == 1);
                    }
                }
                let tsbk = crate::message::parse_tsbk(&bits, 0);
                engine.test_tsbk(&tsbk.message, &mut on_event);
                continue;
            }
            let Ok(freq) = ch.parse::<u64>() else {
                continue;
            };
            let Some(i) = engine.slot_of(freq) else {
                continue;
            };
            let event = match (kind, bytes.len()) {
                ("lc", 9) => {
                    let word: [u8; 9] = bytes.try_into().expect("9 bytes");
                    P25Event::LinkControl {
                        word,
                        parsed: LinkControl::parse(&word),
                        corrected: 0,
                    }
                }
                ("tdulc", 9) => P25Event::TduLc {
                    valid: true,
                    link_control: bytes.try_into().expect("9 bytes"),
                    corrected: 0,
                },
                ("hdu", 15) => {
                    let data: [u8; 15] = bytes.try_into().expect("15 bytes");
                    P25Event::Hdu {
                        valid: true,
                        data,
                        header: Some(crate::payload::HduHeader::parse(&data)),
                        corrected: 0,
                    }
                }
                _ => continue,
            };
            engine.handle_traffic_event(i, &event, &mut on_event);
        }
        engine.shutdown(&mut on_event);

        // Majority alias per radio, and every call filed under another one.
        let mut by_radio: BTreeMap<u32, BTreeMap<String, u64>> = BTreeMap::new();
        for (radio, alias) in &filed {
            *by_radio
                .entry(*radio)
                .or_default()
                .entry(alias.clone())
                .or_default() += 1;
        }
        let mut minority_calls = 0u64;
        let mut multi = Vec::new();
        for (radio, aliases) in &by_radio {
            if aliases.len() > 1 {
                let top = aliases.values().max().copied().unwrap_or(0);
                minority_calls += aliases.values().sum::<u64>() - top;
                multi.push(format!("{radio}: {aliases:?}"));
            }
        }
        eprintln!(
            "alias replay of {path}: {lines} lines, {ended} calls ended, {} filed with an \
             alias, {} radios with an alias, {} radios with more than one, {minority_calls} \
             calls filed under a radio's minority alias",
            filed.len(),
            by_radio.len(),
            multi.len(),
        );
        for m in &multi {
            eprintln!("  {m}");
        }
    }
}
