// SPDX-License-Identifier: GPL-3.0-or-later
//! Observed site inventory: what a P25 site's channel plan looks like **from
//! the air**.
//!
//! [`SiteInventory`] is a pure fold over [`P25Event`]s — no I/O, no clock, no
//! allocation policy. The caller feeds every event a control-channel decoder
//! emits together with the frequency that decoder is tuned to and a
//! millisecond timestamp, and asks for an [`InventorySnapshot`] whenever it
//! wants to publish. The archive (Keyup) later reconciles that snapshot with
//! RadioReference and the recorder's own configuration; this type is the
//! on-air half of that comparison and deliberately reports only what was
//! *heard*.
//!
//! What it learns, and from which message (spec §18.7):
//!
//! | Snapshot field | Source |
//! |---|---|
//! | `nac` | the NID of every decoded frame |
//! | `wacn`, `system_id`, `lra` | network status broadcast (opcode 59) |
//! | `system_id`, `rfss`, `site`, `lra`, `control` | RFSS status broadcast (opcode 58) |
//! | `rfss`, `site`, `secondary_control` | secondary control channel broadcast (opcode 57) |
//! | `adjacent_sites` | adjacent status broadcast (opcode 60) |
//! | `bands` | identifier updates (opcodes 51 / 52 / 61) |
//! | `traffic` | every resolved voice-channel grant |
//!
//! # Confirmation, plausibility and expiry
//!
//! A TSBK that passes CRC is not necessarily a TSBK that was sent: on a weak
//! site a few bits flip inside a block whose CRC still closes, and the band,
//! neighbour and secondary-control tables used to admit that block's contents
//! permanently (the 2026-09-17 three-site comparison, §5/§6: one corrupt
//! `IDEN_UPDATE` put an 18 GHz "band 2" into 126 of 144 windows). Three rules
//! keep a one-off decode out of the snapshot:
//!
//! * **Plausibility** — a band whose base frequency is outside 100 MHz–1 GHz,
//!   whose channel spacing is not a standard multiple (2.5 / 5 / 6.25 / 12.5 /
//!   25 kHz, at most 100 kHz) or whose bandwidth is not a sane multiple of
//!   1.25 kHz is dropped on sight, as is an adjacent or secondary-control
//!   entry whose control frequency resolves outside 100 MHz–1 GHz. Each
//!   rejection is logged at debug with the block's hex.
//! * **Confirmation** — everything else is a *candidate* first: it is
//!   published only once the byte-identical decode has been seen twice
//!   ([`CONFIRMATIONS`]), and for an adjacent site twice within
//!   [`ADJACENT_CONFIRM_WINDOW_MS`]. Candidates live in a bounded map
//!   ([`PENDING_CAP`] per table, oldest dropped) so noise cannot grow memory.
//! * **Expiry** — a real site rebroadcasts all three tables every few seconds,
//!   so an admitted entry that has not been re-announced for
//!   [`ENTRY_TTL_MS`] of lock is dropped from the snapshot. This only removes
//!   ghosts; it never forgets a site that is still talking.
//!
//! The site's own control channel, the identity fields and the traffic table
//! are not held for confirmation: the first two are corroborated on every
//! sweep by two different broadcasts, and a traffic frequency is evidence of a
//! grant the recorder acted on, not an announcement. The traffic table does
//! expire, though: a frequency not granted for [`ENTRY_TTL_MS`] of lock leaves
//! the snapshot, so a grant decoded from a corrupt block (one that resolved to
//! a channel the site never uses) is not reported for the rest of the run.
//!
//! **One admission path.** The trunking layer re-raises every identifier
//! update as [`P25Event::IdenUpdate`] right after the `Tsbk` it came from. A
//! caller that forwards raw TSBKs (the recorder does) has that band folded in
//! from the TSBK, so the `IdenUpdate` is ignored; a caller that forwards only
//! trunking events has its `IdenUpdate`s put through the same plausibility and
//! confirmation rules.
//!
//! **Arrival order does not matter.** A broadcast naming a band whose
//! identifier-update has not been received yet is stored as its raw
//! `(band, channel)` reference and resolved to Hz at [`snapshot`] time, so a
//! late IDEN retroactively completes every announcement that referenced it —
//! the same "buffer, don't drop" rule the tracker applies to pending grants
//! (spec §13), expressed here as "resolve last" instead of "replay".

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::event::P25Event;
use crate::iden::IdenBand;
use crate::message::{
    regroup, AdjacentSiteFlags, ChannelRef, ServiceClass, SystemServices, TsbkMessage,
};

/// How long a supergroup (and each of its members) stays live after its last
/// regroup refresh before [`PatchBook`] expires it.
///
/// L3Harris re-asserts each active regroup continuously — the 2026-09-01 reference-system
/// capture repeats the same activate several times a second — so a supergroup
/// that has gone silent for a minute is no longer being asserted by the FNE and
/// a call on that talkgroup is an ordinary group call again. A cancel tears the
/// supergroup down immediately; this TTL is the backstop for a cancel that was
/// never heard (the recorder joined mid-patch, or the cancel was lost to noise).
pub const PATCH_TTL_MS: u64 = 60_000;

/// How many byte-identical decodes admit a band, secondary control channel or
/// neighbour into the snapshot.
///
/// Two, because that is the smallest number a single CRC-passing corrupt block
/// cannot reach: the corruption would have to repeat bit for bit. A real
/// announcement is repeated hundreds of times an hour, so the cost of the rule
/// is one announcement interval — seconds — of delay.
pub const CONFIRMATIONS: u32 = 2;

/// Both sightings of an adjacent site must fall inside this window.
///
/// Neighbour tables change (a site is added, a control channel moves), so an
/// old candidate must not be confirmable by a decode half a day later; bands
/// and secondary control channels are site configuration and carry no such
/// window.
pub const ADJACENT_CONFIRM_WINDOW_MS: u64 = 30 * 60_000;

/// How long an admitted band / secondary control channel / neighbour survives
/// without being re-announced before it leaves the snapshot.
pub const ENTRY_TTL_MS: u64 = 60 * 60_000;

/// Candidates held per table before the oldest is dropped.
pub const PENDING_CAP: usize = 64;

/// Lowest plausible P25 frequency (Hz): below this is a decode error, not a
/// band plan.
const MIN_PLAUSIBLE_HZ: u64 = 100_000_000;

/// Highest plausible P25 frequency (Hz).
const MAX_PLAUSIBLE_HZ: u64 = 1_000_000_000;

/// Largest plausible channel spacing / bandwidth (Hz).
const MAX_SPACING_HZ: u32 = 100_000;

/// A live supergroup→members map decoded from L3Harris group-regroup TSBKs
/// (mfid 164, opcode 48 — see [`crate::message::regroup`]).
///
/// While a patch is active a voice call carried on the *supergroup* talkgroup is
/// really being heard by every *member* talkgroup, so the call archive wants the
/// member set stamped on that call. The book is a pure fold: it holds no clock,
/// takes `now_ms` from the caller, and is driven from the same control-channel
/// TSBK stream everything else observes. Malformed or non-talkgroup regroups are
/// dropped, never surfaced.
#[derive(Clone, Debug, Default)]
pub struct PatchBook {
    /// supergroup id → its members (each with a last-seen ms) and the last time
    /// the supergroup itself was asserted.
    groups: BTreeMap<u16, PatchEntry>,
}

#[derive(Clone, Debug, Default)]
struct PatchEntry {
    /// member talkgroup id → last time it was asserted for this supergroup.
    members: BTreeMap<u16, u64>,
    /// last time any activate named this supergroup.
    last_ms: u64,
}

impl PatchBook {
    /// An empty book.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold one decoded message. A regroup **activate** naming a talkgroup adds
    /// (or refreshes) that member under its supergroup; a **cancel** tears the
    /// whole supergroup down. Everything else — every non-regroup message, a
    /// regroup targeting an individual radio, a zero supergroup or member — is
    /// ignored. Returns `true` when the live map actually changed, so a caller
    /// can emit an update only on a real edit rather than on every repeat.
    pub fn observe(&mut self, msg: &TsbkMessage, now_ms: u64) -> bool {
        let TsbkMessage::HarrisGroupRegroup {
            regroup_options,
            supergroup,
            target,
            target_is_talkgroup,
            ..
        } = msg
        else {
            return false;
        };
        let supergroup = *supergroup;
        if supergroup == 0 {
            return false;
        }
        if !regroup::is_activate(*regroup_options) {
            // Cancel: the vendor clears the talkgroup-target flag and zeroes the
            // target on a teardown, so key it on the supergroup alone.
            return self.groups.remove(&supergroup).is_some();
        }
        // Activate: only a talkgroup target patches a talkgroup; a regroup of an
        // individual radio is not a supergroup the call archive can use.
        if !target_is_talkgroup {
            return false;
        }
        let Ok(member) = u16::try_from(*target) else {
            return false;
        };
        if member == 0 {
            return false;
        }
        let entry = self.groups.entry(supergroup).or_default();
        entry.last_ms = now_ms;
        entry.members.insert(member, now_ms).is_none()
    }

    /// The member talkgroups of `supergroup` if it is a live patch at `now_ms`,
    /// sorted ascending; empty when it is not patched or has expired. Members
    /// that have themselves gone stale are dropped from the result.
    #[must_use]
    pub fn members_of(&self, supergroup: u16, now_ms: u64) -> Vec<u32> {
        let Some(entry) = self.groups.get(&supergroup) else {
            return Vec::new();
        };
        if now_ms.saturating_sub(entry.last_ms) > PATCH_TTL_MS {
            return Vec::new();
        }
        entry
            .members
            .iter()
            .filter(|(_, last)| now_ms.saturating_sub(**last) <= PATCH_TTL_MS)
            .map(|(tg, _)| u32::from(*tg))
            .collect()
    }

    /// Drop supergroups and members not asserted within [`PATCH_TTL_MS`]. Cheap
    /// enough to call on a timer; `members_of` already ignores stale entries, so
    /// this only bounds memory.
    pub fn expire(&mut self, now_ms: u64) {
        self.groups.retain(|_, entry| {
            entry
                .members
                .retain(|_, last| now_ms.saturating_sub(*last) <= PATCH_TTL_MS);
            !entry.members.is_empty() && now_ms.saturating_sub(entry.last_ms) <= PATCH_TTL_MS
        });
    }

    /// Number of live supergroups currently tracked (for observability/tests).
    #[must_use]
    pub fn len(&self) -> usize {
        self.groups.len()
    }

    /// True when no supergroup is being tracked.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }
}

/// A frequency this site announced or granted, as observed.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct ObservedFrequency {
    /// Downlink frequency in Hz, resolved through the observed band plan.
    /// `None` when the announcing band's identifier-update has not been heard
    /// yet (the announcement itself is still reported).
    pub hz: Option<u64>,
    /// Frequency band identifier from the announcement; `None` for an entry
    /// that came from the tuned frequency rather than from the air.
    pub band: Option<u8>,
    /// Channel number from the announcement; `None` as for `band`.
    pub channel: Option<u16>,
    /// `"band-chan"` label (the form the reference decoder logs, e.g.
    /// `"1-638"`); `None` when not announced on air.
    pub channel_label: Option<String>,
    /// Service-class flags carried by the announcement; `None` when not
    /// announced on air.
    pub service_class: Option<ServiceClass>,
    /// First time (caller's ms clock) this channel was announced.
    pub first_seen_ms: u64,
    /// Most recent announcement.
    pub last_seen_ms: u64,
    /// Announcements counted in the **current window** (see
    /// [`SiteInventory::reset_window`]).
    pub announcements: u64,
}

/// One entry of the observed IDEN band plan (spec §10).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct BandEntry {
    /// 4-bit band identifier.
    pub id: u8,
    /// Base frequency in Hz.
    pub base_hz: u64,
    /// Channel spacing in Hz.
    pub spacing_hz: u32,
    /// Signed transmit (uplink) offset in Hz; 0 = receive-only band.
    pub offset_hz: i64,
    /// Channel bandwidth in Hz.
    pub bandwidth_hz: u32,
    /// True for a TDMA channel type (opcode 51).
    pub tdma: bool,
    /// Slots per carrier (1 for FDMA).
    pub slots_per_carrier: u8,
    /// First time this band identifier was heard.
    pub first_seen_ms: u64,
    /// Most recent identifier-update for this band.
    pub last_seen_ms: u64,
}

/// A traffic (voice) frequency this site actually granted.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct TrafficFrequency {
    /// Downlink frequency in Hz, as resolved by the engine when it granted.
    pub hz: u64,
    /// `"band-chan"` label, recovered from the observed band plan by reverse
    /// lookup; `None` when no known band produces this frequency.
    pub channel: Option<String>,
    /// Grants seen on this frequency in the **current window**.
    pub grants: u64,
    /// Calls started on this frequency in the **current window**.
    pub calls: u64,
    /// Calls granted on this frequency in the **current window** that the
    /// recorder refused because the channel is TDMA (Phase 2) — they are not
    /// in `calls`, since nothing was started for them.
    #[serde(default)]
    pub tdma_refused: u64,
    /// First grant ever seen on this frequency.
    pub first_seen_ms: u64,
    /// Most recent grant.
    pub last_seen_ms: u64,
}

/// A neighbour site announced by an adjacent status broadcast (opcode 60).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct AdjacentSite {
    /// The neighbour's 12-bit system id.
    pub system_id: u16,
    /// The neighbour's RF sub-system id.
    pub rfss: u8,
    /// The neighbour's site id.
    pub site: u8,
    /// The neighbour's location registration area.
    pub lra: u8,
    /// The neighbour's control channel in Hz, resolved through *this* site's
    /// band plan; `None` while that band is unknown.
    pub control_hz: Option<u64>,
    /// `"band-chan"` label of the neighbour's control channel.
    pub channel: String,
    /// Site condition flags.
    pub flags: AdjacentSiteFlags,
    /// Services the neighbour advertises.
    pub service_class: ServiceClass,
    /// First time this neighbour was announced.
    pub first_seen_ms: u64,
    /// Most recent announcement.
    pub last_seen_ms: u64,
    /// Announcements counted in the **current window**.
    pub announcements: u64,
}

/// A point-in-time, serialisable view of everything the inventory has heard.
///
/// Identity fields are `Option` because a snapshot may be taken before the
/// broadcast that carries them has been decoded; a `None` means "not heard
/// yet", never "absent from the system".
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct InventorySnapshot {
    /// Network access code from the NID of decoded frames.
    pub nac: Option<u16>,
    /// 20-bit wide-area communications network id (opcode 59).
    pub wacn: Option<u32>,
    /// 12-bit system id (opcodes 58 / 59).
    pub system_id: Option<u16>,
    /// RF sub-system id of *this* site (opcodes 57 / 58).
    pub rfss: Option<u8>,
    /// Site id of *this* site (opcodes 57 / 58).
    pub site: Option<u8>,
    /// Location registration area of this site (opcodes 58 / 59).
    pub lra: Option<u8>,
    /// The services the site advertises (opcode 56): `(available, supported,
    /// request priority)`. `None` until a system service broadcast is heard.
    pub system_services: Option<(SystemServices, SystemServices, u8)>,
    /// The live control channel: the channel the site announces in its RFSS /
    /// network status broadcasts, falling back to the frequency the decoder is
    /// tuned to when no such broadcast has been heard.
    pub control: Option<ObservedFrequency>,
    /// Secondary / backup control channels announced by opcode 57, sorted by
    /// (band, channel).
    pub secondary_control: Vec<ObservedFrequency>,
    /// Control frequencies the recorder was *configured* with — passed to
    /// [`SiteInventory::new`] and echoed here so the archive can diff
    /// configuration against observation without a second lookup.
    pub configured_control: Vec<u64>,
    /// The observed IDEN band plan, sorted by band identifier.
    pub bands: Vec<BandEntry>,
    /// Every frequency granted within the last [`ENTRY_TTL_MS`], sorted by
    /// frequency.
    pub traffic: Vec<TrafficFrequency>,
    /// Neighbour sites, sorted by (system, rfss, site).
    pub adjacent_sites: Vec<AdjacentSite>,
    /// CRC-valid TSBKs observed in the current window.
    pub tsbk_window: u64,
    /// TSBK blocks that failed CRC in the current window.
    pub tsbk_failed_window: u64,
    /// Length of the current window in ms: `now_ms` minus the time of the
    /// first event observed since construction or the last
    /// [`reset_window`](SiteInventory::reset_window). 0 when nothing has been
    /// observed yet.
    pub window_ms: u64,
}

/// First/last/count bookkeeping shared by the announcement tables.
#[derive(Clone, Debug)]
struct Seen {
    first_ms: u64,
    last_ms: u64,
    /// Counted within the current window only.
    count: u64,
}

impl Seen {
    fn new(now_ms: u64) -> Self {
        Seen {
            first_ms: now_ms,
            last_ms: now_ms,
            count: 1,
        }
    }

    fn touch(&mut self, now_ms: u64) {
        self.last_ms = now_ms;
        self.count += 1;
    }
}

/// A decode waiting for a second, identical sighting before it is published.
///
/// One of these per table key; the value is compared for equality, so a
/// candidate that arrives differently every time never confirms — which is
/// exactly what a stream of corrupt blocks looks like.
#[derive(Clone, Debug)]
struct Candidate<V> {
    value: V,
    count: u32,
    first_ms: u64,
    last_ms: u64,
}

impl<V> Candidate<V> {
    fn new(value: V, now_ms: u64) -> Self {
        Candidate {
            value,
            count: 1,
            first_ms: now_ms,
            last_ms: now_ms,
        }
    }
}

/// Fold one sighting into a pending table.
///
/// Returns `Some(first_ms)` — the time the candidate was *first* heard — when
/// this sighting is the [`CONFIRMATIONS`]th identical one and the entry should
/// be admitted; `None` while it is still a candidate. `window_ms` bounds how
/// far apart the sightings may be (adjacent sites only); a lapsed window, or a
/// value that differs from the one held, restarts the count at this sighting.
///
/// The table is bounded at [`PENDING_CAP`]: inserting a new key past the cap
/// drops the least recently touched candidate, so a weak site's noise costs a
/// fixed amount of memory.
fn confirm<K: Ord + Clone, V: PartialEq>(
    pending: &mut BTreeMap<K, Candidate<V>>,
    key: K,
    value: V,
    now_ms: u64,
    window_ms: Option<u64>,
) -> Option<u64> {
    if let Some(c) = pending.get_mut(&key) {
        let in_window = window_ms.is_none_or(|w| now_ms.saturating_sub(c.first_ms) <= w);
        if c.value == value && in_window {
            c.count += 1;
            c.last_ms = now_ms;
            if c.count >= CONFIRMATIONS {
                let first_ms = c.first_ms;
                pending.remove(&key);
                return Some(first_ms);
            }
        } else {
            *c = Candidate::new(value, now_ms);
        }
        return None;
    }
    if pending.len() >= PENDING_CAP {
        if let Some(oldest) = pending
            .iter()
            .min_by_key(|(_, c)| c.last_ms)
            .map(|(k, _)| k.clone())
        {
            pending.remove(&oldest);
        }
    }
    pending.insert(key, Candidate::new(value, now_ms));
    None
}

/// Whether a decoded band plan can be a real one (see the module docs).
fn plausible_band(b: &IdenBand) -> bool {
    // Bandwidth is announced in 125 Hz units; every real value is a multiple of
    // 1.25 kHz (6.25 / 12.5 / 25 kHz in practice).
    let bandwidth_ok = b.bandwidth_hz != 0
        && b.bandwidth_hz <= MAX_SPACING_HZ
        && b.bandwidth_hz.is_multiple_of(1_250);
    plausible_channel_grid(b) && bandwidth_ok
}

/// Whether a decoded band's *channel grid* — the two fields a grant's
/// frequency is computed from — can be a real one.
///
/// The half of [`plausible_band`] the trunking tracker applies before it lets
/// a band resolve grants: the bandwidth does not enter the frequency math, and
/// an unknown TDMA channel type legitimately decodes to a bandwidth of 0.
pub(crate) fn plausible_channel_grid(b: &IdenBand) -> bool {
    let base_ok = (MIN_PLAUSIBLE_HZ..=MAX_PLAUSIBLE_HZ).contains(&b.base_hz);
    // Standard channel spacings are multiples of 2.5 kHz (2.5 / 5 / 12.5 / 25)
    // or of 6.25 kHz; anything else, zero, or wider than 100 kHz is a flipped
    // bit rather than a channel grid.
    let spacing_ok = b.spacing_hz != 0
        && b.spacing_hz <= MAX_SPACING_HZ
        && (b.spacing_hz.is_multiple_of(2_500) || b.spacing_hz.is_multiple_of(6_250));
    base_ok && spacing_ok
}

/// Whether a resolved control frequency can be real. An *unresolved* channel
/// (its band has not been heard yet) is not rejected: it is resolved at
/// snapshot time, and dropping it here would lose every announcement that
/// arrives before the band plan.
fn plausible_control(hz: Option<u64>) -> bool {
    hz.is_none_or(|hz| (MIN_PLAUSIBLE_HZ..=MAX_PLAUSIBLE_HZ).contains(&hz))
}

/// Hex of the block a rejection came from, for the debug line.
fn hex12(raw: Option<[u8; 12]>) -> String {
    match raw {
        Some(raw) => raw.iter().fold(String::with_capacity(24), |mut s, b| {
            use std::fmt::Write as _;
            let _ = write!(s, "{b:02x}");
            s
        }),
        None => "?".into(),
    }
}

/// An announced control channel (this site's own, or one of its secondaries).
#[derive(Clone, Debug)]
struct ChannelEntry {
    channel: ChannelRef,
    service_class: ServiceClass,
    seen: Seen,
}

/// Everything one adjacent-status broadcast says about a neighbour, compared
/// as a whole: two sightings confirm a neighbour only when they agree on all
/// of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct AdjacentAnnouncement {
    lra: u8,
    channel: ChannelRef,
    flags: AdjacentSiteFlags,
    service_class: ServiceClass,
}

/// An announced neighbour site.
#[derive(Clone, Debug)]
struct AdjacentEntry {
    announcement: AdjacentAnnouncement,
    seen: Seen,
}

/// A granted traffic frequency.
#[derive(Clone, Debug)]
struct TrafficEntry {
    grants: u64,
    calls: u64,
    tdma_refused: u64,
    first_ms: u64,
    last_ms: u64,
}

/// An observed IDEN band plus when it was heard.
#[derive(Clone, Debug)]
struct BandRecord {
    band: IdenBand,
    first_ms: u64,
    last_ms: u64,
}

/// Site channel-plan observer: fold [`P25Event`]s in, take
/// [`InventorySnapshot`]s out.
///
/// See the [module documentation](self) for what is learned from which
/// message. The type holds no clock and no I/O: `now_ms` always comes from the
/// caller, so a given event stream always produces the same snapshot.
#[derive(Clone, Debug)]
pub struct SiteInventory {
    configured_control: Vec<u64>,
    tuned_control_hz: Option<u64>,
    nac: Option<u16>,
    wacn: Option<u32>,
    system_id: Option<u16>,
    rfss: Option<u8>,
    site: Option<u8>,
    lra: Option<u8>,
    /// Last-heard system service broadcast (opcode 56), see
    /// [`InventorySnapshot::system_services`].
    system_services: Option<(SystemServices, SystemServices, u8)>,
    /// The control channel as announced by opcode 58 / 59.
    control: Option<ChannelEntry>,
    /// Secondary control channels, keyed by (band, channel).
    secondary: BTreeMap<(u8, u16), ChannelEntry>,
    /// Observed band plan, keyed by band identifier.
    bands: BTreeMap<u8, BandRecord>,
    /// Granted traffic frequencies, keyed by Hz.
    traffic: BTreeMap<u64, TrafficEntry>,
    /// Neighbour sites, keyed by (system, rfss, site).
    adjacent: BTreeMap<(u16, u8, u8), AdjacentEntry>,
    /// Bands heard once and not yet confirmed (see the module docs).
    pending_bands: BTreeMap<u8, Candidate<IdenBand>>,
    /// Secondary control channels heard once and not yet confirmed.
    pending_secondary: BTreeMap<(u8, u16), Candidate<(ChannelRef, ServiceClass)>>,
    /// Neighbours heard once and not yet confirmed.
    pending_adjacent: BTreeMap<(u16, u8, u8), Candidate<AdjacentAnnouncement>>,
    /// The most recent TSBK's wire bytes, so a rejection can name the block it
    /// came from. The trunking layer raises a `SiteBroadcast` immediately after
    /// the `Tsbk` event it was decoded from, so this is that block's hex;
    /// `None` for a caller that forwards trunking events only.
    last_raw: Option<[u8; 12]>,
    /// Newest event time observed, used to expire stale entries on a window
    /// boundary.
    last_event_ms: u64,
    tsbk_window: u64,
    tsbk_failed_window: u64,
    /// Start of the current counting window; `None` until the first event
    /// after construction or [`SiteInventory::reset_window`].
    window_start_ms: Option<u64>,
}

impl SiteInventory {
    /// A fresh inventory that has heard nothing.
    ///
    /// `configured_control_hz` is the recorder's configured control-channel
    /// list; it is only carried through to the snapshot (this type never uses
    /// it to interpret what it hears).
    pub fn new(configured_control_hz: Vec<u64>) -> Self {
        SiteInventory {
            configured_control: configured_control_hz,
            tuned_control_hz: None,
            nac: None,
            wacn: None,
            system_id: None,
            rfss: None,
            site: None,
            lra: None,
            system_services: None,
            control: None,
            secondary: BTreeMap::new(),
            bands: BTreeMap::new(),
            traffic: BTreeMap::new(),
            adjacent: BTreeMap::new(),
            pending_bands: BTreeMap::new(),
            pending_secondary: BTreeMap::new(),
            pending_adjacent: BTreeMap::new(),
            last_raw: None,
            last_event_ms: 0,
            tsbk_window: 0,
            tsbk_failed_window: 0,
            window_start_ms: None,
        }
    }

    /// Fold one control-channel event in.
    ///
    /// `control_frequency_hz` is the frequency the decoder that produced `e`
    /// is tuned to (0 = unknown); it is the fallback for the snapshot's
    /// `control` when the site has not yet announced its own control channel.
    /// Feed **every** event the control decoder emits: the identity, band plan
    /// and TSBK-rate fields all come from events other than the site
    /// broadcasts.
    pub fn observe(&mut self, control_frequency_hz: u64, e: &P25Event, now_ms: u64) {
        if control_frequency_hz != 0 {
            self.tuned_control_hz = Some(control_frequency_hz);
        }
        if self.window_start_ms.is_none() {
            self.window_start_ms = Some(now_ms);
        }
        self.last_event_ms = self.last_event_ms.max(now_ms);
        if let P25Event::Tsbk { tsbk, .. } = e {
            self.last_raw = Some(tsbk.raw);
        }
        match e {
            P25Event::NidDecoded { nac, .. } => self.nac = Some(*nac),
            P25Event::Tsbk { tsbk, .. } => {
                self.tsbk_window += 1;
                // The TSBK carries the *full* band definition (TDMA channel
                // type and slots included), which `P25Event::IdenUpdate` does
                // not — prefer it.
                match &tsbk.message {
                    TsbkMessage::IdenUpdate(band) => self.insert_band(*band, now_ms),
                    // The system service broadcast names no channel, so the
                    // trunking layer raises no `SiteBroadcast` for it; it is
                    // learned straight off the TSBK stream instead.
                    TsbkMessage::SystemServiceBroadcast {
                        available,
                        supported,
                        priority,
                    } => self.system_services = Some((*available, *supported, *priority)),
                    _ => {}
                }
            }
            P25Event::TsbkFailed => self.tsbk_failed_window += 1,
            P25Event::IdenUpdate {
                id,
                base_hz,
                spacing_hz,
                offset_hz,
                bandwidth_hz,
            } => {
                // The trunking layer raises this right after the `Tsbk` it was
                // decoded from, so a caller that forwards raw TSBKs has already
                // folded this very block in above — with its TDMA fields.
                // Folding it again would count one decode as two and confirm
                // a one-off corrupt block on its own (the second path the
                // 2026-09-22 review found, D1), and would publish a TDMA band
                // as FDMA.
                if self.last_raw.is_some() {
                    return;
                }
                // A caller that forwards only trunking events: the same
                // admission as the TSBK path — plausibility, then two
                // identical decodes. This event carries no TDMA fields, so the
                // band is taken as FDMA.
                self.insert_band(
                    IdenBand {
                        id: *id,
                        base_hz: *base_hz,
                        spacing_hz: *spacing_hz,
                        offset_hz: *offset_hz,
                        bandwidth_hz: *bandwidth_hz,
                        slots_per_carrier: 1,
                        tdma: false,
                    },
                    now_ms,
                );
            }
            P25Event::Grant { frequency_hz, .. } => {
                let t = self.traffic_entry(*frequency_hz, now_ms);
                t.grants += 1;
                t.last_ms = now_ms;
            }
            P25Event::CallStart { frequency_hz, .. } => {
                let t = self.traffic_entry(*frequency_hz, now_ms);
                t.calls += 1;
                t.last_ms = now_ms;
            }
            P25Event::TdmaGrantRefused { frequency_hz, .. } => {
                let t = self.traffic_entry(*frequency_hz, now_ms);
                t.tdma_refused += 1;
                t.last_ms = now_ms;
            }
            P25Event::SiteBroadcast { message, .. } => self.site_broadcast(message, now_ms),
            _ => {}
        }
    }

    /// A serialisable view of everything heard so far.
    pub fn snapshot(&self, now_ms: u64) -> InventorySnapshot {
        InventorySnapshot {
            nac: self.nac,
            wacn: self.wacn,
            system_id: self.system_id,
            rfss: self.rfss,
            site: self.site,
            lra: self.lra,
            system_services: self.system_services,
            control: self.control_snapshot(),
            secondary_control: self
                .secondary
                .values()
                .filter(|c| Self::live(now_ms, c.seen.last_ms))
                .map(|c| self.observed(c))
                .collect(),
            configured_control: self.configured_control.clone(),
            bands: self
                .bands
                .values()
                .filter(|r| Self::live(now_ms, r.last_ms))
                .map(|r| BandEntry {
                    id: r.band.id,
                    base_hz: r.band.base_hz,
                    spacing_hz: r.band.spacing_hz,
                    offset_hz: r.band.offset_hz,
                    bandwidth_hz: r.band.bandwidth_hz,
                    tdma: r.band.tdma,
                    slots_per_carrier: r.band.slots_per_carrier,
                    first_seen_ms: r.first_ms,
                    last_seen_ms: r.last_ms,
                })
                .collect(),
            traffic: self
                .traffic
                .iter()
                .filter(|(_, t)| Self::live(now_ms, t.last_ms))
                .map(|(&hz, t)| TrafficFrequency {
                    hz,
                    channel: self.channel_label(hz),
                    grants: t.grants,
                    calls: t.calls,
                    tdma_refused: t.tdma_refused,
                    first_seen_ms: t.first_ms,
                    last_seen_ms: t.last_ms,
                })
                .collect(),
            adjacent_sites: self
                .adjacent
                .iter()
                .filter(|(_, a)| Self::live(now_ms, a.seen.last_ms))
                .map(|(&(system_id, rfss, site), a)| AdjacentSite {
                    system_id,
                    rfss,
                    site,
                    lra: a.announcement.lra,
                    control_hz: self.resolve(a.announcement.channel),
                    channel: label(a.announcement.channel),
                    flags: a.announcement.flags,
                    service_class: a.announcement.service_class,
                    first_seen_ms: a.seen.first_ms,
                    last_seen_ms: a.seen.last_ms,
                    announcements: a.seen.count,
                })
                .collect(),
            tsbk_window: self.tsbk_window,
            tsbk_failed_window: self.tsbk_failed_window,
            window_ms: now_ms.saturating_sub(self.window_start_ms.unwrap_or(now_ms)),
        }
    }

    /// Clear the per-window counters (TSBK counts, grant/call counts,
    /// announcement counts) and restart the window at the next observed event.
    ///
    /// Identities, the band plan and every `first_seen_ms` / `last_seen_ms`
    /// survive: a window boundary is a reporting boundary, not a forget.
    pub fn reset_window(&mut self) {
        // A window boundary is the natural moment to reclaim what the snapshot
        // has already stopped reporting: an entry nothing has re-announced for
        // `ENTRY_TTL_MS` of lock, and any candidate that went silent before it
        // could be confirmed.
        let now = self.last_event_ms;
        self.bands.retain(|_, r| Self::live(now, r.last_ms));
        self.secondary
            .retain(|_, c| Self::live(now, c.seen.last_ms));
        self.adjacent.retain(|_, a| Self::live(now, a.seen.last_ms));
        self.traffic.retain(|_, t| Self::live(now, t.last_ms));
        self.pending_bands.retain(|_, c| Self::live(now, c.last_ms));
        self.pending_secondary
            .retain(|_, c| Self::live(now, c.last_ms));
        self.pending_adjacent
            .retain(|_, c| Self::live(now, c.last_ms));
        self.tsbk_window = 0;
        self.tsbk_failed_window = 0;
        self.window_start_ms = None;
        if let Some(c) = self.control.as_mut() {
            c.seen.count = 0;
        }
        for c in self.secondary.values_mut() {
            c.seen.count = 0;
        }
        for a in self.adjacent.values_mut() {
            a.seen.count = 0;
        }
        for t in self.traffic.values_mut() {
            t.grants = 0;
            t.calls = 0;
            t.tdma_refused = 0;
        }
    }

    // -- internals ---------------------------------------------------------

    /// Fold a site/network broadcast payload in.
    fn site_broadcast(&mut self, m: &TsbkMessage, now_ms: u64) {
        match m {
            TsbkMessage::SecondaryControlChannel {
                rfss,
                site,
                channel_a,
                service_class_a,
                channel_b,
                service_class_b,
            } => {
                self.rfss = Some(*rfss);
                self.site = Some(*site);
                self.touch_secondary(*channel_a, *service_class_a, now_ms);
                if let Some(cb) = channel_b {
                    self.touch_secondary(*cb, *service_class_b, now_ms);
                }
            }
            TsbkMessage::RfssStatus {
                lra,
                system_id,
                rfss,
                site,
                channel,
                service_class,
                ..
            } => {
                self.lra = Some(*lra);
                self.system_id = Some(*system_id);
                self.rfss = Some(*rfss);
                self.site = Some(*site);
                self.touch_control(*channel, *service_class, now_ms);
            }
            TsbkMessage::NetworkStatus {
                lra,
                wacn,
                system_id,
                channel,
                service_class,
            } => {
                self.lra = Some(*lra);
                self.wacn = Some(*wacn);
                self.system_id = Some(*system_id);
                self.touch_control(*channel, *service_class, now_ms);
            }
            TsbkMessage::AdjacentStatus {
                lra,
                flags,
                system_id,
                rfss,
                site,
                channel,
                service_class,
            } => {
                let announcement = AdjacentAnnouncement {
                    lra: *lra,
                    channel: *channel,
                    flags: *flags,
                    service_class: *service_class,
                };
                // A neighbour whose control channel resolves nowhere real is a
                // corrupt block, not a site.
                if !plausible_control(self.resolve(*channel)) {
                    tracing::debug!(
                        system_id = *system_id,
                        rfss = *rfss,
                        site = *site,
                        channel = %label(*channel),
                        hex = %hex12(self.last_raw),
                        "adjacent site rejected: control frequency is not plausible"
                    );
                    return;
                }
                // Keyed by the neighbour's identity: a site that moves its
                // control channel updates in place rather than duplicating —
                // but only once the move has been announced twice, so one
                // corrupt block can neither add a neighbour nor move one.
                let key = (*system_id, *rfss, *site);
                if let Some(a) = self.adjacent.get_mut(&key) {
                    if a.announcement == announcement {
                        a.seen.touch(now_ms);
                        return;
                    }
                }
                if let Some(first_ms) = confirm(
                    &mut self.pending_adjacent,
                    key,
                    announcement,
                    now_ms,
                    Some(ADJACENT_CONFIRM_WINDOW_MS),
                ) {
                    match self.adjacent.get_mut(&key) {
                        // A confirmed change to a neighbour already published
                        // keeps its history.
                        Some(a) => {
                            a.announcement = announcement;
                            a.seen.touch(now_ms);
                        }
                        None => {
                            self.adjacent.insert(
                                key,
                                AdjacentEntry {
                                    announcement,
                                    seen: Seen {
                                        first_ms,
                                        last_ms: now_ms,
                                        count: CONFIRMATIONS.into(),
                                    },
                                },
                            );
                        }
                    }
                }
            }
            _ => {}
        }
    }

    /// Record this site's announced control channel.
    fn touch_control(&mut self, channel: ChannelRef, service_class: ServiceClass, now_ms: u64) {
        match self.control.as_mut() {
            Some(c) if c.channel == channel => {
                c.service_class = service_class;
                c.seen.touch(now_ms);
            }
            // A different announced channel replaces the old one: the site has
            // exactly one live control channel (backups live in `secondary`).
            _ => {
                self.control = Some(ChannelEntry {
                    channel,
                    service_class,
                    seen: Seen::new(now_ms),
                })
            }
        }
    }

    /// Fold one identifier-update in: rejected outright when the band cannot be
    /// real, then held as a candidate until a byte-identical repeat confirms
    /// it (module docs, "Confirmation, plausibility and expiry").
    fn insert_band(&mut self, band: IdenBand, now_ms: u64) {
        if !plausible_band(&band) {
            tracing::debug!(
                band = band.id,
                base_hz = band.base_hz,
                spacing_hz = band.spacing_hz,
                bandwidth_hz = band.bandwidth_hz,
                hex = %hex12(self.last_raw),
                "band plan rejected: implausible base frequency, spacing or bandwidth"
            );
            return;
        }
        if let Some(r) = self.bands.get_mut(&band.id) {
            if r.band == band {
                r.last_ms = now_ms;
                return;
            }
        }
        if let Some(first_ms) = confirm(&mut self.pending_bands, band.id, band, now_ms, None) {
            match self.bands.get_mut(&band.id) {
                Some(r) => {
                    r.band = band;
                    r.last_ms = now_ms;
                }
                None => {
                    self.bands.insert(
                        band.id,
                        BandRecord {
                            band,
                            first_ms,
                            last_ms: now_ms,
                        },
                    );
                }
            }
        }
    }

    /// Fold one secondary-control-channel announcement in, under the same
    /// plausibility and confirmation rules as the band plan.
    fn touch_secondary(&mut self, channel: ChannelRef, service_class: ServiceClass, now_ms: u64) {
        if !plausible_control(self.resolve(channel)) {
            tracing::debug!(
                channel = %label(channel),
                hex = %hex12(self.last_raw),
                "secondary control channel rejected: frequency is not plausible"
            );
            return;
        }
        let key = (channel.band, channel.channel);
        if let Some(c) = self.secondary.get_mut(&key) {
            if c.service_class == service_class {
                c.seen.touch(now_ms);
                return;
            }
        }
        if let Some(first_ms) = confirm(
            &mut self.pending_secondary,
            key,
            (channel, service_class),
            now_ms,
            None,
        ) {
            match self.secondary.get_mut(&key) {
                Some(c) => {
                    c.service_class = service_class;
                    c.seen.touch(now_ms);
                }
                None => {
                    self.secondary.insert(
                        key,
                        ChannelEntry {
                            channel,
                            service_class,
                            seen: Seen {
                                first_ms,
                                last_ms: now_ms,
                                count: CONFIRMATIONS.into(),
                            },
                        },
                    );
                }
            }
        }
    }

    /// Whether an admitted entry last heard at `last_ms` is still live at
    /// `now_ms`.
    fn live(now_ms: u64, last_ms: u64) -> bool {
        now_ms.saturating_sub(last_ms) <= ENTRY_TTL_MS
    }

    fn traffic_entry(&mut self, hz: u64, now_ms: u64) -> &mut TrafficEntry {
        self.traffic.entry(hz).or_insert_with(|| TrafficEntry {
            grants: 0,
            calls: 0,
            tdma_refused: 0,
            first_ms: now_ms,
            last_ms: now_ms,
        })
    }

    /// Resolve a channel reference through the observed band plan.
    fn resolve(&self, ch: ChannelRef) -> Option<u64> {
        self.bands
            .get(&(ch.band & 0xF))
            .map(|r| r.band.downlink_hz(ch.channel))
    }

    fn observed(&self, c: &ChannelEntry) -> ObservedFrequency {
        ObservedFrequency {
            hz: self.resolve(c.channel),
            band: Some(c.channel.band),
            channel: Some(c.channel.channel),
            channel_label: Some(label(c.channel)),
            service_class: Some(c.service_class),
            first_seen_ms: c.seen.first_ms,
            last_seen_ms: c.seen.last_ms,
            announcements: c.seen.count,
        }
    }

    /// The live control channel: announced if heard, else the tuned frequency.
    fn control_snapshot(&self) -> Option<ObservedFrequency> {
        if let Some(c) = self.control.as_ref() {
            return Some(self.observed(c));
        }
        self.tuned_control_hz.map(|hz| ObservedFrequency {
            hz: Some(hz),
            band: None,
            channel: None,
            channel_label: None,
            service_class: None,
            first_seen_ms: 0,
            last_seen_ms: 0,
            announcements: 0,
        })
    }

    /// Best-effort `"band-chan"` label for an already-resolved frequency: the
    /// lowest-numbered band whose grid lands exactly on `hz`. Grants report a
    /// frequency, not the channel reference they came from, so the label is
    /// recovered rather than remembered; bands that alias produce the same
    /// frequency anyway, so only the label text is ambiguous.
    fn channel_label(&self, hz: u64) -> Option<String> {
        for r in self.bands.values() {
            let b = &r.band;
            if b.spacing_hz == 0 || hz < b.base_hz {
                continue;
            }
            let delta = hz - b.base_hz;
            let spacing = u64::from(b.spacing_hz);
            if !delta.is_multiple_of(spacing) {
                continue;
            }
            let mut ch = delta / spacing;
            if b.tdma && b.slots_per_carrier > 1 {
                ch *= u64::from(b.slots_per_carrier);
            }
            if ch < 4096 {
                return Some(format!("{}-{}", b.id, ch));
            }
        }
        None
    }
}

/// `"band-chan"`, the form the reference decoder logs.
fn label(ch: ChannelRef) -> String {
    format!("{}-{}", ch.band, ch.channel)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bits::BitBuf;
    use crate::event::SiteBroadcastKind;
    use crate::message::{parse_tsbk, Tsbk};

    /// Decode a 12-byte hex TSBK (the raw event log's `hex` field) to a message.
    fn regroup_msg(hex: &str) -> TsbkMessage {
        let mut b = BitBuf::new();
        for _ in 0..96 {
            b.push(false);
        }
        let bytes: Vec<u8> = (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect();
        for (i, byte) in bytes.iter().enumerate() {
            b.set_u64(i * 8, 8, u64::from(*byte));
        }
        parse_tsbk(&b, 0).message
    }

    #[test]
    fn patch_book_tracks_activate_and_cancel_from_captured_hex() {
        let mut book = PatchBook::new();
        // Supergroup 0x36b1 gains members 0x06aa then 0x06ab (captured shapes, synthetic members).
        assert!(book.observe(&regroup_msg("b0a4ee36b100008006aa1f21"), 1_000));
        assert!(book.observe(&regroup_msg("b0a4ee36b100008006ab0f00"), 1_100));
        assert_eq!(book.members_of(0x36b1, 1_200), vec![0x06aa_u32, 0x06ab_u32]);
        assert_eq!(book.len(), 1);
        // A repeat of the same activate is not a change.
        assert!(!book.observe(&regroup_msg("b0a4ee36b100008006aa1f21"), 1_300));
        // Its cancel tears the supergroup down (target flag/id are zeroed).
        assert!(book.observe(&regroup_msg("b0a48036b1000000000089d4"), 1_400));
        assert!(book.members_of(0x36b1, 1_500).is_empty());
        assert!(book.is_empty());
    }

    #[test]
    fn patch_book_expires_a_stale_supergroup() {
        let mut book = PatchBook::new();
        book.observe(&regroup_msg("b0a464fcfa00008006ac3ff3"), 0);
        assert_eq!(book.members_of(0xfcfa, PATCH_TTL_MS), vec![0x06ac_u32]);
        // Past the TTL the patch reads as gone even before expire() reclaims it.
        assert!(book.members_of(0xfcfa, PATCH_TTL_MS + 1).is_empty());
        book.expire(PATCH_TTL_MS + 1);
        assert!(book.is_empty());
    }

    #[test]
    fn patch_book_ignores_malformed_and_non_regroup() {
        let mut book = PatchBook::new();
        // A non-regroup TSBK (opcode 59, network status) is ignored outright.
        assert!(!book.observe(&regroup_msg("bb000a455640341003701a41"), 0));
        // A cancel for a supergroup never activated changes nothing.
        assert!(!book.observe(&regroup_msg("b0a480ff660000000000b8f8"), 0));
        assert!(book.is_empty());
    }

    /// The two bands the reference fixture announces (spec §10 math).
    fn band0() -> IdenBand {
        IdenBand {
            id: 0,
            base_hz: 769_006_250,
            spacing_hz: 12_500,
            offset_hz: 30_000_000,
            bandwidth_hz: 12_500,
            slots_per_carrier: 1,
            tdma: false,
        }
    }

    fn band1() -> IdenBand {
        IdenBand {
            id: 1,
            base_hz: 851_012_500,
            spacing_hz: 12_500,
            offset_hz: -45_000_000,
            bandwidth_hz: 12_500,
            slots_per_carrier: 1,
            tdma: false,
        }
    }

    fn iden_event(band: IdenBand) -> P25Event {
        P25Event::Tsbk {
            tsbk: Tsbk {
                last_block: true,
                encrypted: false,
                opcode: 61,
                vendor: 0,
                message: TsbkMessage::IdenUpdate(band),
                raw: [0u8; 12],
            },
            corrected: 0,
            crc_corrected: 0,
        }
    }

    fn ch(band: u8, channel: u16) -> ChannelRef {
        ChannelRef { band, channel }
    }

    fn broadcast(kind: SiteBroadcastKind, message: TsbkMessage) -> P25Event {
        P25Event::SiteBroadcast {
            kind,
            message,
            frequency_hz: None,
            frequency_b_hz: None,
        }
    }

    /// The four broadcasts the fixture's site emits (values from the reference
    /// decoder's log of the same capture).
    fn fixture_broadcasts() -> Vec<P25Event> {
        vec![
            broadcast(
                SiteBroadcastKind::RfssStatus,
                TsbkMessage::RfssStatus {
                    lra: 10,
                    active_network_connection: true,
                    system_id: 52,
                    rfss: 10,
                    site: 1,
                    channel: ch(1, 3),
                    service_class: ServiceClass::from_byte(0x70),
                },
            ),
            broadcast(
                SiteBroadcastKind::NetworkStatus,
                TsbkMessage::NetworkStatus {
                    lra: 10,
                    wacn: 284_004,
                    system_id: 52,
                    channel: ch(1, 3),
                    service_class: ServiceClass::from_byte(0x70),
                },
            ),
            broadcast(
                SiteBroadcastKind::SecondaryControl,
                TsbkMessage::SecondaryControlChannel {
                    rfss: 10,
                    site: 1,
                    channel_a: ch(1, 638),
                    service_class_a: ServiceClass::from_byte(0x04),
                    channel_b: Some(ch(1, 718)),
                    service_class_b: ServiceClass::from_byte(0x04),
                },
            ),
            broadcast(
                SiteBroadcastKind::AdjacentStatus,
                TsbkMessage::AdjacentStatus {
                    lra: 20,
                    flags: AdjacentSiteFlags {
                        conventional: false,
                        site_failure: false,
                        valid_information: true,
                        active_rfss_connection: true,
                    },
                    system_id: 57,
                    rfss: 20,
                    site: 20,
                    channel: ch(1, 6),
                    service_class: ServiceClass::from_byte(0xF0),
                },
            ),
            broadcast(
                SiteBroadcastKind::AdjacentStatus,
                TsbkMessage::AdjacentStatus {
                    lra: 10,
                    flags: AdjacentSiteFlags {
                        conventional: false,
                        site_failure: false,
                        valid_information: true,
                        active_rfss_connection: true,
                    },
                    system_id: 6,
                    rfss: 10,
                    site: 10,
                    channel: ch(0, 61),
                    service_class: ServiceClass::from_byte(0xF0),
                },
            ),
            broadcast(
                SiteBroadcastKind::AdjacentStatus,
                TsbkMessage::AdjacentStatus {
                    lra: 1,
                    flags: AdjacentSiteFlags {
                        conventional: false,
                        site_failure: false,
                        valid_information: true,
                        active_rfss_connection: true,
                    },
                    system_id: 57,
                    rfss: 1,
                    site: 1,
                    channel: ch(1, 48),
                    service_class: ServiceClass::from_byte(0xF0),
                },
            ),
        ]
    }

    fn assert_fixture_snapshot(s: &InventorySnapshot) {
        assert_eq!(s.nac, Some(52));
        assert_eq!(s.wacn, Some(284_004));
        assert_eq!(s.system_id, Some(52));
        assert_eq!(s.rfss, Some(10));
        assert_eq!(s.site, Some(1));
        assert_eq!(s.lra, Some(10));
        // Live control channel: 851.050 MHz (band 1, channel 3).
        let c = s.control.as_ref().expect("control");
        assert_eq!(c.hz, Some(851_050_000));
        assert_eq!(c.channel_label.as_deref(), Some("1-3"));
        // Secondary (backup) control channels: 858.9875 / 859.9875 MHz.
        let sec: Vec<_> = s.secondary_control.iter().map(|f| f.hz).collect();
        assert_eq!(sec, vec![Some(858_987_500), Some(859_987_500)]);
        assert!(s
            .secondary_control
            .iter()
            .all(|f| f.service_class.expect("service class").backup_control));
        // Adjacent sites, sorted by (system, rfss, site).
        let adj: Vec<_> = s
            .adjacent_sites
            .iter()
            .map(|a| {
                (
                    a.system_id,
                    a.rfss,
                    a.site,
                    a.channel.as_str(),
                    a.control_hz,
                )
            })
            .collect();
        assert_eq!(
            adj,
            vec![
                (6, 10, 10, "0-61", Some(769_768_750)),
                (57, 1, 1, "1-48", Some(851_612_500)),
                (57, 20, 20, "1-6", Some(851_087_500)),
            ]
        );
        assert!(s.adjacent_sites.iter().all(|a| a.flags.valid_information));
        // Both bands are in the plan.
        assert_eq!(s.bands.iter().map(|b| b.id).collect::<Vec<_>>(), vec![0, 1]);
    }

    #[test]
    fn fixture_ground_truth_iden_first() {
        let mut inv = SiteInventory::new(vec![851_050_000]);
        inv.observe(
            851_050_000,
            &P25Event::NidDecoded {
                nac: 52,
                duid: 7,
                corrected: 0,
            },
            0,
        );
        // Every announcement is heard twice, as a real site repeats them every
        // few seconds: nothing is published on a single sighting.
        for round in 0..2u64 {
            inv.observe(851_050_000, &iden_event(band0()), 10 + round * 50);
            inv.observe(851_050_000, &iden_event(band1()), 20 + round * 50);
            for (i, e) in fixture_broadcasts().iter().enumerate() {
                inv.observe(851_050_000, e, 100 + round * 50 + i as u64);
            }
        }
        assert_fixture_snapshot(&inv.snapshot(1000));
    }

    #[test]
    fn fixture_ground_truth_broadcasts_first() {
        // Reverse order: every announcement arrives before the band plan and
        // must still resolve once the IDENs land.
        let mut inv = SiteInventory::new(vec![851_050_000]);
        for round in 0..2u64 {
            for (i, e) in fixture_broadcasts().iter().enumerate() {
                inv.observe(851_050_000, e, round * 20 + i as u64);
            }
        }
        // Before the IDENs nothing resolves, but nothing is lost either.
        let early = inv.snapshot(50);
        assert_eq!(early.control.as_ref().unwrap().hz, None);
        assert_eq!(early.secondary_control.len(), 2);
        assert!(early.secondary_control.iter().all(|f| f.hz.is_none()));
        assert_eq!(early.adjacent_sites.len(), 3);
        assert!(early.adjacent_sites.iter().all(|a| a.control_hz.is_none()));

        inv.observe(
            851_050_000,
            &P25Event::NidDecoded {
                nac: 52,
                duid: 7,
                corrected: 0,
            },
            100,
        );
        for round in 0..2u64 {
            inv.observe(851_050_000, &iden_event(band0()), 110 + round * 10);
            inv.observe(851_050_000, &iden_event(band1()), 120 + round * 10);
        }
        assert_fixture_snapshot(&inv.snapshot(1000));
    }

    #[test]
    fn control_falls_back_to_the_tuned_frequency() {
        let mut inv = SiteInventory::new(vec![851_050_000]);
        inv.observe(851_050_000, &P25Event::TsbkFailed, 0);
        let s = inv.snapshot(10);
        let c = s.control.expect("tuned fallback");
        assert_eq!(c.hz, Some(851_050_000));
        assert_eq!(c.band, None);
        assert_eq!(c.channel_label, None);
        assert_eq!(s.configured_control, vec![851_050_000]);
        assert_eq!(s.tsbk_failed_window, 1);
        assert_eq!(s.window_ms, 10);
    }

    #[test]
    fn traffic_counts_grants_and_calls_and_labels_the_channel() {
        let mut inv = SiteInventory::new(vec![]);
        inv.observe(851_050_000, &iden_event(band1()), 0);
        inv.observe(851_050_000, &iden_event(band1()), 1);
        let grant = P25Event::Grant {
            kind: crate::event::CallKind::Group,
            frequency_hz: 856_712_500,
            talkgroup: 52101,
            target: 52101,
            source: 1,
            update: false,
        };
        inv.observe(851_050_000, &grant, 100);
        inv.observe(
            851_050_000,
            &P25Event::CallStart {
                frequency_hz: 856_712_500,
                talkgroup: 52101,
                target: 52101,
                source: 1,
            },
            100,
        );
        inv.observe(851_050_000, &grant, 500);
        let s = inv.snapshot(1000);
        assert_eq!(s.traffic.len(), 1);
        let t = &s.traffic[0];
        assert_eq!(t.hz, 856_712_500);
        // 851_012_500 + 12_500 × 456 = 856_712_500.
        assert_eq!(t.channel.as_deref(), Some("1-456"));
        assert_eq!(t.grants, 2);
        assert_eq!(t.calls, 1);
        assert_eq!(t.first_seen_ms, 100);
        assert_eq!(t.last_seen_ms, 500);
    }

    /// A call refused as Phase 2 is counted on its carrier (it has a grant
    /// but no `CallStart`), and the count is a window counter like the others.
    #[test]
    fn traffic_counts_tdma_refusals_per_window() {
        let mut inv = SiteInventory::new(vec![]);
        let carrier = 851_068_750;
        let grant = P25Event::Grant {
            kind: crate::event::CallKind::Group,
            frequency_hz: carrier,
            talkgroup: 52103,
            target: 52103,
            source: 0,
            update: false,
        };
        let refused = |slot| P25Event::TdmaGrantRefused {
            kind: crate::event::CallKind::Group,
            frequency_hz: carrier,
            timeslot: slot,
            talkgroup: 52103,
            target: 52103,
            source: 0,
        };
        inv.observe(851_050_000, &grant, 100);
        inv.observe(851_050_000, &refused(1), 100);
        inv.observe(851_050_000, &refused(2), 200);
        let t = &inv.snapshot(300).traffic[0];
        assert_eq!((t.grants, t.calls, t.tdma_refused), (1, 0, 2));
        inv.reset_window();
        assert_eq!(inv.snapshot(400).traffic[0].tdma_refused, 0);
    }

    #[test]
    fn reset_window_clears_counters_but_keeps_identities() {
        let mut inv = SiteInventory::new(vec![]);
        for round in 0..2u64 {
            inv.observe(851_050_000, &iden_event(band1()), round);
            for (i, e) in fixture_broadcasts().iter().enumerate() {
                inv.observe(851_050_000, e, round * 20 + i as u64);
            }
        }
        inv.observe(
            851_050_000,
            &P25Event::Grant {
                kind: crate::event::CallKind::Group,
                frequency_hz: 856_712_500,
                talkgroup: 52101,
                target: 52101,
                source: 1,
                update: false,
            },
            10,
        );
        let before = inv.snapshot(100);
        assert!(before.tsbk_window > 0);
        assert_eq!(before.traffic[0].grants, 1);

        inv.reset_window();
        let after = inv.snapshot(100);
        assert_eq!(after.tsbk_window, 0);
        assert_eq!(after.tsbk_failed_window, 0);
        assert_eq!(after.window_ms, 0);
        // Identities and first-seen survive the window boundary.
        assert_eq!(after.system_id, before.system_id);
        assert_eq!(after.wacn, before.wacn);
        assert_eq!(after.adjacent_sites.len(), before.adjacent_sites.len());
        assert_eq!(
            after.traffic[0].first_seen_ms,
            before.traffic[0].first_seen_ms
        );
        // Per-window counters are zeroed.
        assert_eq!(after.traffic[0].grants, 0);
        assert_eq!(after.traffic[0].calls, 0);
        assert!(after.adjacent_sites.iter().all(|a| a.announcements == 0));
        assert!(after.secondary_control.iter().all(|f| f.announcements == 0));

        // The window restarts at the next observed event, not at reset time.
        inv.observe(851_050_000, &P25Event::TsbkFailed, 500);
        assert_eq!(inv.snapshot(800).window_ms, 300);
    }

    // -- confirmation, plausibility and expiry ------------------------------

    /// A neighbour site's bogus "band 2" from the 2026-09-17 comparison, §5: one corrupt
    /// `IDEN_UPDATE` block, base 18 GHz, 38.625 kHz spacing.
    fn corrupt_band() -> IdenBand {
        IdenBand {
            id: 2,
            base_hz: 18_082_306_730,
            spacing_hz: 38_625,
            offset_hz: -9_250_000,
            bandwidth_hz: 17_375,
            slots_per_carrier: 1,
            tdma: false,
        }
    }

    /// A band whose every field is standard but whose base frequency is off the
    /// P25 spectrum entirely.
    fn off_range_band() -> IdenBand {
        IdenBand {
            id: 3,
            base_hz: 2_400_000_000,
            spacing_hz: 12_500,
            offset_hz: 0,
            bandwidth_hz: 12_500,
            slots_per_carrier: 1,
            tdma: false,
        }
    }

    fn adjacent(system_id: u16, channel: ChannelRef) -> P25Event {
        broadcast(
            SiteBroadcastKind::AdjacentStatus,
            TsbkMessage::AdjacentStatus {
                lra: 10,
                flags: AdjacentSiteFlags {
                    conventional: false,
                    site_failure: false,
                    valid_information: true,
                    active_rfss_connection: true,
                },
                system_id,
                rfss: 10,
                site: 10,
                channel,
                service_class: ServiceClass::from_byte(0xF0),
            },
        )
    }

    fn band_ids(inv: &SiteInventory, now_ms: u64) -> Vec<u8> {
        inv.snapshot(now_ms).bands.iter().map(|b| b.id).collect()
    }

    #[test]
    fn a_band_seen_once_is_never_published_and_twice_is() {
        let mut inv = SiteInventory::new(vec![]);
        inv.observe(851_050_000, &iden_event(band1()), 0);
        assert!(
            band_ids(&inv, 10).is_empty(),
            "one sighting publishes nothing"
        );
        inv.observe(851_050_000, &iden_event(band1()), 100);
        assert_eq!(band_ids(&inv, 110), vec![1]);
        // And the first-seen time is the *first* sighting, not the confirming
        // one: the entry's history is not restarted by the rule.
        assert_eq!(inv.snapshot(110).bands[0].first_seen_ms, 0);
    }

    #[test]
    fn one_corrupt_iden_block_never_enters_the_band_plan() {
        let mut inv = SiteInventory::new(vec![]);
        for round in 0..2u64 {
            inv.observe(851_050_000, &iden_event(band0()), round * 10);
            inv.observe(851_050_000, &iden_event(band1()), round * 10 + 1);
        }
        inv.observe(851_050_000, &iden_event(corrupt_band()), 100);
        assert_eq!(
            band_ids(&inv, 200),
            vec![0, 1],
            "the 18 GHz band is not real"
        );
    }

    #[test]
    fn an_off_range_base_frequency_is_rejected_however_often_it_repeats() {
        let mut inv = SiteInventory::new(vec![]);
        for i in 0..10u64 {
            inv.observe(851_050_000, &iden_event(off_range_band()), i * 10);
        }
        assert!(
            band_ids(&inv, 200).is_empty(),
            "plausibility is not a vote: a repeat cannot admit 2.4 GHz"
        );
    }

    #[test]
    fn a_confirmed_band_expires_after_an_hour_without_re_announcement() {
        let mut inv = SiteInventory::new(vec![]);
        inv.observe(851_050_000, &iden_event(band1()), 0);
        inv.observe(851_050_000, &iden_event(band1()), 1_000);
        assert_eq!(band_ids(&inv, ENTRY_TTL_MS + 1_000), vec![1]);
        // Silent past the TTL: gone from the snapshot, and reclaimed at the
        // next window boundary.
        inv.observe(851_050_000, &P25Event::TsbkFailed, 1_000 + ENTRY_TTL_MS + 1);
        assert!(band_ids(&inv, 1_000 + ENTRY_TTL_MS + 1).is_empty());
        inv.reset_window();
        inv.observe(851_050_000, &iden_event(band1()), 1_000 + ENTRY_TTL_MS + 2);
        assert!(
            band_ids(&inv, 1_000 + ENTRY_TTL_MS + 2).is_empty(),
            "the expired band is re-admitted only by a fresh pair of sightings"
        );
    }

    #[test]
    fn an_adjacent_site_needs_two_sightings_within_half_an_hour() {
        let mut inv = SiteInventory::new(vec![]);
        for round in 0..2u64 {
            inv.observe(851_050_000, &iden_event(band1()), round);
        }
        // Heard once: not a neighbour yet.
        inv.observe(851_050_000, &adjacent(7, ch(1, 10)), 1_000);
        assert!(inv.snapshot(2_000).adjacent_sites.is_empty());
        // The repeat confirms it.
        inv.observe(851_050_000, &adjacent(7, ch(1, 10)), 2_000);
        let s = inv.snapshot(3_000);
        assert_eq!(s.adjacent_sites.len(), 1);
        assert_eq!(s.adjacent_sites[0].system_id, 7);
        assert_eq!(s.adjacent_sites[0].control_hz, Some(851_137_500));

        // A different neighbour whose two sightings straddle the 30-minute
        // window never confirms.
        inv.observe(851_050_000, &adjacent(90, ch(1, 20)), 10_000);
        inv.observe(
            851_050_000,
            &adjacent(90, ch(1, 20)),
            10_000 + ADJACENT_CONFIRM_WINDOW_MS + 1,
        );
        let ids: Vec<u16> = inv
            .snapshot(10_000 + ADJACENT_CONFIRM_WINDOW_MS + 2)
            .adjacent_sites
            .iter()
            .map(|a| a.system_id)
            .collect();
        assert_eq!(ids, vec![7]);
    }

    #[test]
    fn an_adjacent_site_off_the_p25_spectrum_is_rejected() {
        let mut inv = SiteInventory::new(vec![]);
        // A band that resolves channel 4095 to 2.4 GHz, confirmed twice so the
        // rejection below is about the neighbour, not about the band.
        let wide = IdenBand {
            id: 4,
            base_hz: 950_000_000,
            spacing_hz: 100_000,
            offset_hz: 0,
            bandwidth_hz: 12_500,
            slots_per_carrier: 1,
            tdma: false,
        };
        inv.observe(851_050_000, &iden_event(wide), 0);
        inv.observe(851_050_000, &iden_event(wide), 1);
        for i in 0..4u64 {
            inv.observe(851_050_000, &adjacent(82, ch(4, 4095)), 100 + i * 100);
        }
        assert!(
            inv.snapshot(1_000).adjacent_sites.is_empty(),
            "1.36 GHz is not a control channel"
        );
    }

    #[test]
    fn a_secondary_control_channel_needs_two_sightings_and_then_expires() {
        let mut inv = SiteInventory::new(vec![]);
        inv.observe(851_050_000, &iden_event(band1()), 0);
        inv.observe(851_050_000, &iden_event(band1()), 1);
        let sec = broadcast(
            SiteBroadcastKind::SecondaryControl,
            TsbkMessage::SecondaryControlChannel {
                rfss: 10,
                site: 1,
                channel_a: ch(1, 638),
                service_class_a: ServiceClass::from_byte(0x04),
                channel_b: None,
                service_class_b: ServiceClass::from_byte(0x04),
            },
        );
        inv.observe(851_050_000, &sec, 100);
        assert!(inv.snapshot(200).secondary_control.is_empty());
        inv.observe(851_050_000, &sec, 200);
        assert_eq!(
            inv.snapshot(300)
                .secondary_control
                .iter()
                .map(|c| c.hz)
                .collect::<Vec<_>>(),
            vec![Some(858_987_500)]
        );
        assert!(
            inv.snapshot(200 + ENTRY_TTL_MS + 1)
                .secondary_control
                .is_empty(),
            "an hour of silence drops a secondary control channel"
        );
    }

    #[test]
    fn the_pending_tables_are_bounded() {
        let mut inv = SiteInventory::new(vec![]);
        // Twice the cap of distinct, never-repeated neighbours — the shape of a
        // weak site's noise — and none of them is published.
        for i in 0..(PENDING_CAP as u64) * 2 {
            inv.observe(
                851_050_000,
                &adjacent(1_000 + i as u16, ch(1, i as u16)),
                i * 10,
            );
        }
        assert!(inv.snapshot(10_000).adjacent_sites.is_empty());
        assert!(inv.pending_adjacent.len() <= PENDING_CAP);
    }

    /// The trunking layer's re-raise of an identifier update, as it follows
    /// the `Tsbk` it was decoded from.
    fn iden_echo(band: IdenBand) -> P25Event {
        P25Event::IdenUpdate {
            id: band.id,
            base_hz: band.base_hz,
            spacing_hz: band.spacing_hz,
            offset_hz: band.offset_hz,
            bandwidth_hz: band.bandwidth_hz,
        }
    }

    /// The pair the recorder actually feeds for every IDEN block: the raw
    /// `Tsbk`, then the tracker's `IdenUpdate` for the same block.
    fn iden_pair(inv: &mut SiteInventory, band: IdenBand, now_ms: u64) {
        inv.observe(851_050_000, &iden_event(band), now_ms);
        inv.observe(851_050_000, &iden_echo(band), now_ms);
    }

    /// Review 2026-09-22, D1: one decode reaches the inventory twice, and the
    /// `IdenUpdate` half used to be admitted with no plausibility check and no
    /// confirmation. One block must count as one sighting however it arrives.
    #[test]
    fn a_blocks_trunking_echo_is_not_a_second_sighting() {
        let mut inv = SiteInventory::new(vec![]);
        // A plausible band decoded once — a one-off block whose CRC closed.
        iden_pair(&mut inv, band1(), 0);
        assert!(
            band_ids(&inv, 10).is_empty(),
            "the echo of one block does not confirm it"
        );
        // The documented ghost, fed as the recorder feeds it: never admitted.
        iden_pair(&mut inv, corrupt_band(), 20);
        assert!(band_ids(&inv, 30).is_empty());
        // A second real decode confirms band 1 as before.
        iden_pair(&mut inv, band1(), 100);
        assert_eq!(band_ids(&inv, 110), vec![1]);
    }

    /// The echo carries no TDMA fields; it must not beat the TSBK to the
    /// table and publish a TDMA band as FDMA.
    #[test]
    fn a_tdma_band_fed_as_the_recorder_feeds_it_stays_tdma() {
        let tdma = IdenBand {
            id: 4,
            base_hz: 851_006_250,
            spacing_hz: 12_500,
            offset_hz: -45_000_000,
            bandwidth_hz: 12_500,
            slots_per_carrier: 2,
            tdma: true,
        };
        let mut inv = SiteInventory::new(vec![]);
        iden_pair(&mut inv, tdma, 0);
        iden_pair(&mut inv, tdma, 100);
        let bands = inv.snapshot(200).bands;
        assert_eq!(bands.len(), 1);
        assert!(bands[0].tdma);
        assert_eq!(bands[0].slots_per_carrier, 2);
    }

    /// A caller that forwards only trunking events gets the same admission:
    /// plausibility, then two identical decodes.
    #[test]
    fn trunking_only_iden_updates_go_through_the_same_admission() {
        let mut inv = SiteInventory::new(vec![]);
        inv.observe(851_050_000, &iden_echo(band1()), 0);
        assert!(
            band_ids(&inv, 10).is_empty(),
            "one sighting publishes nothing"
        );
        for i in 0..5u64 {
            inv.observe(851_050_000, &iden_echo(corrupt_band()), 20 + i);
        }
        assert!(
            band_ids(&inv, 30).is_empty(),
            "the 18 GHz band is never real"
        );
        inv.observe(851_050_000, &iden_echo(band1()), 100);
        assert_eq!(band_ids(&inv, 110), vec![1]);
    }

    fn traffic_grant(frequency_hz: u64) -> P25Event {
        P25Event::Grant {
            kind: crate::event::CallKind::Group,
            frequency_hz,
            talkgroup: 52101,
            target: 52101,
            source: 1,
            update: false,
        }
    }

    fn traffic_hz(inv: &SiteInventory, now_ms: u64) -> Vec<u64> {
        inv.snapshot(now_ms).traffic.iter().map(|t| t.hz).collect()
    }

    /// Review 2026-09-22, D5: a grant decoded from a corrupt block used to put
    /// a frequency in every snapshot for the rest of the run.
    #[test]
    fn a_traffic_frequency_expires_after_an_hour_without_a_grant() {
        let mut inv = SiteInventory::new(vec![]);
        inv.observe(851_050_000, &traffic_grant(856_712_500), 0);
        inv.observe(851_050_000, &traffic_grant(852_825_000), 0);
        // One of the two keeps being granted; the other was a one-off.
        inv.observe(
            851_050_000,
            &traffic_grant(852_825_000),
            ENTRY_TTL_MS - 1_000,
        );
        assert_eq!(
            traffic_hz(&inv, ENTRY_TTL_MS),
            vec![852_825_000, 856_712_500],
            "both are live inside the hour"
        );
        assert_eq!(
            traffic_hz(&inv, ENTRY_TTL_MS + 1),
            vec![852_825_000],
            "the one-off grant is gone from the snapshot after an hour"
        );
        // And reclaimed at the next window boundary, not just hidden.
        inv.observe(851_050_000, &P25Event::TsbkFailed, ENTRY_TTL_MS + 1);
        inv.reset_window();
        assert_eq!(inv.traffic.len(), 1);
        // A frequency granted again later comes back as a fresh entry.
        inv.observe(851_050_000, &traffic_grant(856_712_500), ENTRY_TTL_MS + 2);
        let s = inv.snapshot(ENTRY_TTL_MS + 3);
        let back = s
            .traffic
            .iter()
            .find(|t| t.hz == 856_712_500)
            .expect("granted again");
        assert_eq!(back.first_seen_ms, ENTRY_TTL_MS + 2);
    }

    #[test]
    fn snapshot_round_trips_through_serde() {
        let mut inv = SiteInventory::new(vec![851_050_000]);
        for round in 0..2u64 {
            inv.observe(851_050_000, &iden_event(band1()), round);
            for (i, e) in fixture_broadcasts().iter().enumerate() {
                inv.observe(851_050_000, e, round * 20 + i as u64);
            }
        }
        let snap = inv.snapshot(100);
        let json = serde_json::to_string(&snap).expect("serialize");
        let back: InventorySnapshot = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(snap, back);
    }
}
