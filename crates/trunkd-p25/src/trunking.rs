// SPDX-License-Identifier: GPL-3.0-or-later
//! Trunking / traffic-channel tracking state machine (spec §13).
//!
//! Call tracking is keyed by **downlink frequency** (Phase 1 is always
//! timeslot 1), not by talkgroup. Time is supplied by the caller in
//! milliseconds (the engine derives it from the sample clock, keeping event
//! streams deterministic for a given input file).

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use crate::event::{CallKind, P25Event, SiteBroadcastKind};
use crate::iden::{BandMap, IdenBand};
use crate::message::{ChannelRef, TsbkMessage, UnitGrantAmbt};

/// Default traffic channel pool size.
pub const DEFAULT_POOL_SIZE: u32 = 20;

/// Tracker staleness horizon: an event is stale when `now − eventEnd > 3000`.
const STALE_MS: u64 = 3000;

/// A grant whose band identifier had no IDEN yet is held here and retried when
/// the matching IDEN arrives (spec §13: the call "cannot be followed until that
/// band's identifier-update has been received" — buffered, not dropped). This
/// matches the reference decoder, which resolves such grants once the band plan
/// is learned. The buffer is bounded (drop-oldest) to preserve the engine's
/// no-unbounded-growth posture.
const MAX_PENDING_GRANTS: usize = 64;

/// Maximum age of a buffered grant before it is discarded unresolved.
///
/// Tied to [`STALE_MS`] on purpose: a replayed grant is fed back through the
/// normal grant path *as if it had just arrived*, so a grant older than the
/// tracker staleness horizon would be resurrected as a brand-new call long
/// after the call it described has ended. With a longer TTL a single IDEN can
/// replay a whole backlog of dead grants at once (observed: 70 buffered grants
/// → 64 CallStarts and 44 PoolExhausted events in one burst). Never let this
/// exceed `STALE_MS`.
const PENDING_GRANT_TTL_MS: u64 = STALE_MS;

/// One grant request: everything [`TrunkingTracker::grant`] needs. Also the
/// buffered form of a grant whose band identifier is not known yet.
#[derive(Debug, Clone, Copy)]
struct PendingGrant {
    kind: CallKind,
    channel: ChannelRef,
    /// TO — talkgroup for a group call, target radio for unit-to-unit, the
    /// telephone address for an interconnect update, 0 when absent.
    to: u32,
    /// FROM — source radio (0 when the message carries none).
    from: u32,
    update: bool,
    now_ms: u64,
}

impl PendingGrant {
    /// Talkgroup as reported on events: the 16-bit group id for group calls,
    /// 0 for unit-to-unit / telephone (whose TO is a 24-bit address that does
    /// not fit and is carried in `target` instead).
    fn talkgroup(&self) -> u16 {
        match self.kind {
            CallKind::Group => self.to as u16,
            CallKind::UnitToUnit | CallKind::Telephone => 0,
        }
    }
}

#[derive(Debug, Clone)]
struct Tracker {
    /// TO — talkgroup for group calls, target for unit-to-unit, 0 telephone.
    to: u32,
    /// FROM — source radio (may be 0 until known).
    from: u32,
    /// Talkgroup / target as reported on this call's events (see
    /// [`PendingGrant::talkgroup`]).
    talkgroup: u16,
    event_end_ms: u64,
    /// Set once a traffic decoder reported the call started (HDU seen) —
    /// the hand-off freeze: control-side refreshes stop extending after this.
    started_by_traffic: bool,
    complete: bool,
    /// `PoolExhausted` already reported for *this* call (report once, not on
    /// every control-channel refresh).
    pool_exhausted_reported: bool,
}

impl Tracker {
    fn new(g: &PendingGrant, started_by_traffic: bool) -> Self {
        Tracker {
            to: g.to,
            from: g.from,
            talkgroup: g.talkgroup(),
            event_end_ms: g.now_ms,
            started_by_traffic,
            complete: false,
            pool_exhausted_reported: false,
        }
    }

    fn is_stale(&self, now_ms: u64) -> bool {
        now_ms.saturating_sub(self.event_end_ms) > STALE_MS
    }
}

/// What a grant / traffic update means for the tracker already on a frequency.
enum SameCall {
    /// No usable tracker (absent or stale) → a new call.
    NewCall,
    /// Same TO, a genuinely different talker → a continuation.
    Continue,
    /// Same call: extend it.
    Refresh,
}

/// A [`TrunkingTracker`] shared by every decoder of one site.
///
/// Spec §13 / the reference `P25TrafficChannelManager`: the control decoder
/// and *all* traffic decoders of a site drive **one** tracker. That is what
/// makes the traffic side work at all — call trackers are created by grants,
/// which only ever arrive on the control channel, so a traffic decoder with a
/// private tracker can never find the entry for its own frequency (and its
/// band plan would be empty, breaking the IDEN preload of §13).
///
/// All channels of a device run on one DSP thread (`docs/ARCHITECTURE.md`), so
/// a single-threaded `Rc<RefCell<..>>` handle is the right cost here; there is
/// no cross-thread sharing to pay for.
///
/// Borrows are taken for the duration of one tracker call and are never held
/// across a caller's `process`; an event sink must therefore not re-enter
/// another channel of the same site (that would double-borrow and panic).
pub type SharedTracker = Rc<RefCell<TrunkingTracker>>;

/// The trunking state machine for one site (one control channel plus the
/// traffic channels it grants).
pub struct TrunkingTracker {
    /// Frequencies currently allocated to running traffic channels (the
    /// control frequency is pre-inserted so it is never allocated).
    allocated: HashSet<u64>,
    trackers: HashMap<u64, Tracker>,
    /// Known frequency bands (IDEN storage). Private: the band plan is site
    /// state owned by the tracker — a traffic decoder reads it through the
    /// shared handle rather than keeping (and clearing) a copy of its own.
    bands: BandMap,
    /// A decoded band plan that differs from the one [`Self::bands`] holds for
    /// the same identifier, waiting for an identical second decode before it
    /// replaces it (see [`TrunkingTracker::learn_band`]). One per identifier.
    band_candidates: [Option<IdenBand>; 16],
    /// Configured pool size (see [`TrunkingTracker::set_pool_size`]).
    pool_size: u32,
    pool_available: u32,
    control_frequency_hz: u64,
    /// Grants awaiting the IDEN for their band (bounded, drop-oldest).
    pending: Vec<PendingGrant>,
    /// Whether unit-to-unit (private) voice grants are followed
    /// (`[recorder] follow_unit_calls`). See
    /// [`TrunkingTracker::set_follow_unit_calls`].
    follow_unit_calls: bool,
    /// TDMA calls already reported as refused, keyed by (carrier, timeslot):
    /// the call's TO and its latest grant. Keeps
    /// [`P25Event::TdmaGrantRefused`] to one per call (see
    /// [`TrunkingTracker::refuse_tdma`]); pruned of stale entries once it
    /// grows past [`MAX_TDMA_REFUSED`].
    tdma_refused: HashMap<(u64, u8), (u32, u64)>,
}

/// Size at which [`TrunkingTracker::tdma_refused`] drops its stale entries. A
/// site has a few dozen TDMA slots at most; only corrupt grants could push it
/// further, and those age out.
const MAX_TDMA_REFUSED: usize = 128;

impl TrunkingTracker {
    /// Create a tracker seeded with the control channel frequency.
    pub fn new(control_frequency_hz: u64) -> Self {
        let mut allocated = HashSet::new();
        allocated.insert(control_frequency_hz);
        TrunkingTracker {
            allocated,
            trackers: HashMap::new(),
            bands: BandMap::new(),
            band_candidates: [None; 16],
            pool_size: DEFAULT_POOL_SIZE,
            pool_available: DEFAULT_POOL_SIZE,
            control_frequency_hz,
            pending: Vec::new(),
            follow_unit_calls: true,
            tdma_refused: HashMap::new(),
        }
    }

    /// Create a [`SharedTracker`] — one tracker for a whole site, to be handed
    /// to [`P25P1Channel::new_control`](crate::P25P1Channel::new_control) and
    /// every [`new_traffic`](crate::P25P1Channel::new_traffic) decoder.
    pub fn new_shared(control_frequency_hz: u64) -> SharedTracker {
        Rc::new(RefCell::new(Self::new(control_frequency_hz)))
    }

    /// The site's band plan (IDEN storage).
    pub fn bands(&self) -> &BandMap {
        &self.bands
    }

    /// Mutable band plan — for seeding a standalone tracker with a band plan
    /// learned elsewhere (spec §13 "IDEN preload"). Channels sharing a
    /// [`SharedTracker`] need no preload: they read the live plan.
    pub fn bands_mut(&mut self) -> &mut BandMap {
        &mut self.bands
    }

    /// The frequency this tracker treats as the control channel (never
    /// allocated as a traffic channel).
    pub fn control_frequency_hz(&self) -> u64 {
        self.control_frequency_hz
    }

    /// Traffic channels still available in the pool.
    pub fn pool_available(&self) -> u32 {
        self.pool_available
    }

    /// Configured traffic-channel pool size (default [`DEFAULT_POOL_SIZE`]).
    pub fn pool_size(&self) -> u32 {
        self.pool_size
    }

    /// Whether unit-to-unit (private) voice grants are followed.
    pub fn follow_unit_calls(&self) -> bool {
        self.follow_unit_calls
    }

    /// Turn following of unit-to-unit (private) grants on or off
    /// (`[recorder] follow_unit_calls`, default on).
    ///
    /// It gates only the multi-block ([`process_unit_grant`]) path, which is
    /// the one this site actually uses: a single-block TSBK unit-to-unit grant
    /// has always been followed and stays followed, because turning *that* off
    /// would be a behaviour change nobody asked for.
    ///
    /// [`process_unit_grant`]: TrunkingTracker::process_unit_grant
    pub fn set_follow_unit_calls(&mut self, on: bool) {
        self.follow_unit_calls = on;
    }

    /// Resize the traffic-channel pool (a site's configured recorder count).
    ///
    /// Call it before any grant is processed: it resets the available count,
    /// so resizing mid-flight would lose track of channels already allocated.
    pub fn set_pool_size(&mut self, size: u32) {
        self.pool_size = size;
        self.pool_available = size;
    }

    /// End timestamp (ms, on the caller's clock) of the call tracked on
    /// `freq` — `None` when nothing is tracked there.
    pub fn event_end_ms(&self, freq: u64) -> Option<u64> {
        self.trackers.get(&freq).map(|t| t.event_end_ms)
    }

    /// Whether a traffic decoder has taken over the call on `freq` — the §13
    /// hand-off freeze, after which control-side refreshes stop extending the
    /// event.
    pub fn started_by_traffic(&self, freq: u64) -> bool {
        self.trackers
            .get(&freq)
            .is_some_and(|t| t.started_by_traffic)
    }

    /// Process a unit-to-unit voice grant that arrived as a multi-block
    /// (AMBT) message rather than a TSBK.
    ///
    /// Opcode 4 is the call start and opcode 6 the dwell keeper, which is
    /// exactly what [`grant`](Self::grant) already does with `update`: the
    /// grant goes down the same path a group grant does, so a private call
    /// competes for a traffic channel on the same terms and ages out on the
    /// same staleness horizon. A no-op when following is off.
    ///
    /// Returns whether the grant was acted on, so a caller can count what it
    /// declined to follow.
    pub fn process_unit_grant(
        &mut self,
        grant: &UnitGrantAmbt,
        now_ms: u64,
        sink: &mut dyn FnMut(P25Event),
    ) -> bool {
        if !self.follow_unit_calls {
            return false;
        }
        self.process(&grant.as_tsbk_message(), now_ms, sink);
        true
    }

    /// Process a parsed TSBK message, emitting trunking events.
    pub fn process(&mut self, msg: &TsbkMessage, now_ms: u64, sink: &mut dyn FnMut(P25Event)) {
        match msg {
            TsbkMessage::GroupVoiceGrant {
                channel,
                talkgroup,
                source,
                ..
            } => {
                self.grant(
                    PendingGrant {
                        kind: CallKind::Group,
                        channel: *channel,
                        to: u32::from(*talkgroup),
                        from: *source,
                        update: false,
                        now_ms,
                    },
                    sink,
                );
            }
            TsbkMessage::GroupVoiceGrantUpdate { a, b } => {
                for (channel, talkgroup) in std::iter::once(a).chain(b.iter()) {
                    self.grant(
                        PendingGrant {
                            kind: CallKind::Group,
                            channel: *channel,
                            to: u32::from(*talkgroup),
                            from: 0,
                            update: true,
                            now_ms,
                        },
                        sink,
                    );
                }
            }
            TsbkMessage::GroupVoiceGrantUpdateExplicit {
                downlink,
                talkgroup,
                ..
            } => {
                self.grant(
                    PendingGrant {
                        kind: CallKind::Group,
                        channel: *downlink,
                        to: u32::from(*talkgroup),
                        from: 0,
                        update: true,
                        now_ms,
                    },
                    sink,
                );
            }
            TsbkMessage::UnitToUnitGrant {
                update,
                channel,
                target,
                source,
            } => {
                self.grant(
                    PendingGrant {
                        kind: CallKind::UnitToUnit,
                        channel: *channel,
                        to: *target,
                        from: *source,
                        update: *update,
                        now_ms,
                    },
                    sink,
                );
            }
            TsbkMessage::TelephoneInterconnectGrant {
                update,
                channel,
                address,
                address_is_talkgroup,
                ..
            } => {
                // Opcode 9: address is a talkgroup (TO); opcode 8: a source (FROM).
                let (to, from) = if *address_is_talkgroup {
                    (*address, 0)
                } else {
                    (0, *address)
                };
                self.grant(
                    PendingGrant {
                        kind: CallKind::Telephone,
                        channel: *channel,
                        to,
                        from,
                        update: *update,
                        now_ms,
                    },
                    sink,
                );
            }
            TsbkMessage::IdenUpdate(band) => {
                let changed = self.learn_band(*band);
                // Raised for every decode, applied or not: an observer (the
                // site inventory) runs its own admission and needs to see
                // each sighting to count it.
                sink(P25Event::IdenUpdate {
                    id: band.id,
                    base_hz: band.base_hz,
                    spacing_hz: band.spacing_hz,
                    offset_hz: band.offset_hz,
                    bandwidth_hz: band.bandwidth_hz,
                });
                // A newly-learned band may unblock grants received earlier.
                if changed {
                    self.resolve_pending(band.id, now_ms, sink);
                }
            }
            // Site / network broadcasts (spec §18.7): no traffic action, but
            // they carry the site's channel plan — resolve their channel
            // references through the band plan and republish for observers.
            TsbkMessage::SecondaryControlChannel {
                channel_a,
                channel_b,
                ..
            } => self.site_broadcast(
                SiteBroadcastKind::SecondaryControl,
                msg,
                *channel_a,
                *channel_b,
                sink,
            ),
            TsbkMessage::RfssStatus { channel, .. } => {
                self.site_broadcast(SiteBroadcastKind::RfssStatus, msg, *channel, None, sink)
            }
            TsbkMessage::NetworkStatus { channel, .. } => {
                self.site_broadcast(SiteBroadcastKind::NetworkStatus, msg, *channel, None, sink)
            }
            TsbkMessage::AdjacentStatus { channel, .. } => {
                self.site_broadcast(SiteBroadcastKind::AdjacentStatus, msg, *channel, None, sink)
            }
            // SNDCP data-channel grant (opcode 20): not a voice call, but the
            // data follower needs its downlink frequency to subscribe the data
            // channel and reassemble the subscriber's PDUs. Resolve it through
            // the band plan and surface it as a `DataGrant`; drop it silently
            // when the band is not yet known (no follow is possible anyway).
            TsbkMessage::SndcpDataChannelGrant { downlink, .. } => {
                if let Some(frequency_hz) = self.resolve(*downlink) {
                    sink(P25Event::DataGrant { frequency_hz });
                }
            }
            // Registration / affiliation / data / vendor messages: parsed for
            // the observation and radio-event paths, but none of them starts,
            // updates or ends a voice call, so the follower ignores them.
            TsbkMessage::SndcpDataPageRequest { .. }
            | TsbkMessage::SndcpDataChannelAnnounceExplicit { .. }
            | TsbkMessage::AcknowledgeResponse { .. }
            | TsbkMessage::QueuedResponse { .. }
            | TsbkMessage::DenyResponse { .. }
            | TsbkMessage::GroupAffiliationResponse { .. }
            | TsbkMessage::GroupAffiliationQuery { .. }
            | TsbkMessage::ExtendedFunctionCommand { .. }
            | TsbkMessage::SystemServiceBroadcast { .. }
            | TsbkMessage::LocationRegistrationResponse { .. }
            | TsbkMessage::UnitRegistrationResponse { .. }
            | TsbkMessage::UnitRegistrationCommand { .. }
            | TsbkMessage::UnitDeRegistrationAck { .. }
            | TsbkMessage::HarrisGroupRegroup { .. }
            | TsbkMessage::HarrisOsp { .. }
            | TsbkMessage::Other { .. } => {}
        }
    }

    /// Fold one decoded identifier-update into the band plan grants are
    /// resolved through. Returns whether the plan changed.
    ///
    /// * A band whose channel grid cannot be real (base frequency off the P25
    ///   spectrum, a non-standard spacing) is ignored: the inventory's
    ///   plausibility rule, minus the bandwidth, which the frequency math does
    ///   not use.
    /// * The **first** sighting of a band identifier applies at once — a
    ///   grant for a band nobody has announced cannot be followed at all, and
    ///   waiting would cost the first call of every run.
    /// * A **different** plan for an identifier already known replaces it only
    ///   on the second byte-identical decode in a row. One CRC-passing corrupt
    ///   IDEN used to replace a real band until the next good one, and every
    ///   grant in between resolved to a frequency the site never used (review
    ///   2026-09-22, D4). A re-announcement of the plan in force clears the
    ///   candidate, so the two decodes must be uninterrupted.
    fn learn_band(&mut self, band: IdenBand) -> bool {
        if !crate::inventory::plausible_channel_grid(&band) {
            tracing::debug!(
                band = band.id,
                base_hz = band.base_hz,
                spacing_hz = band.spacing_hz,
                "trunking ignores an implausible band plan"
            );
            return false;
        }
        let slot = usize::from(band.id & 0xF);
        match self.bands.get(band.id) {
            None => {
                self.bands.insert(band);
                self.band_candidates[slot] = None;
                true
            }
            Some(known) if *known == band => {
                self.band_candidates[slot] = None;
                false
            }
            Some(_) if self.band_candidates[slot] == Some(band) => {
                self.bands.insert(band);
                self.band_candidates[slot] = None;
                true
            }
            Some(known) => {
                tracing::debug!(
                    band = band.id,
                    known_base_hz = known.base_hz,
                    base_hz = band.base_hz,
                    spacing_hz = band.spacing_hz,
                    "band plan change held until a second identical decode"
                );
                self.band_candidates[slot] = Some(band);
                false
            }
        }
    }

    /// Resolve a channel reference through the band plan; `None` when that
    /// band has no identifier-update yet.
    fn resolve(&self, ch: ChannelRef) -> Option<u64> {
        self.bands.get(ch.band).map(|b| b.downlink_hz(ch.channel))
    }

    /// Emit a [`P25Event::SiteBroadcast`] with its channels resolved.
    fn site_broadcast(
        &self,
        kind: SiteBroadcastKind,
        msg: &TsbkMessage,
        a: ChannelRef,
        b: Option<ChannelRef>,
        sink: &mut dyn FnMut(P25Event),
    ) {
        sink(P25Event::SiteBroadcast {
            kind,
            message: msg.clone(),
            frequency_hz: self.resolve(a),
            frequency_b_hz: b.and_then(|c| self.resolve(c)),
        });
    }

    /// Handle a grant or grant update (spec §13 "On a grant").
    fn grant(&mut self, g: PendingGrant, sink: &mut dyn FnMut(P25Event)) {
        // 1. Resolve the downlink frequency; unknown band → buffer and retry
        //    when the IDEN arrives.
        let freq = self.bands.downlink_hz(g.channel.band, g.channel.channel);
        if freq == 0 {
            self.buffer_pending(g);
            sink(P25Event::GrantUnresolvable {
                band: g.channel.band,
            });
            return;
        }
        let talkgroup = g.talkgroup();
        sink(P25Event::Grant {
            kind: g.kind,
            frequency_hz: freq,
            talkgroup,
            target: g.to,
            source: g.from,
            update: g.update,
        });

        // A Phase 2 (TDMA) channel: this decoder is Phase 1 only, so following
        // it would spend a pool slot on a call that can only end as "no voice"
        // (review 2026-09-22 §4 gap 2). Refuse it before it is tracked or
        // allocated, and say so once per call.
        if let Some(band) = self.bands.get(g.channel.band).filter(|b| b.tdma) {
            let timeslot = band.timeslot(g.channel.channel);
            self.refuse_tdma(&g, freq, timeslot, sink);
            return;
        }

        // 2. Compare against the tracker on this frequency, discarding it if
        //    stale.
        match self.same_call(freq, g.to, g.from, g.now_ms) {
            // 3. Same call: merge FROM; a control-side refresh only extends the
            //    event if the traffic decoder hasn't taken over (hand-off
            //    freeze).
            SameCall::Refresh => {
                if let Some(t) = self.trackers.get_mut(&freq) {
                    if g.from != 0 {
                        t.from = g.from;
                    }
                    if !t.started_by_traffic {
                        t.event_end_ms = g.now_ms;
                    }
                }
            }
            // 4. Same TO, different talker → continuation. The reference
            //    *replaces* the tracked event (P25TrafficChannelManager removes
            //    the tracker and re-runs the channel-grant path), so the new
            //    call is once again control-driven: keeping `started_by_traffic`
            //    from the previous talker would freeze control-side extension
            //    for a call the traffic decoder has not started yet, and the
            //    tracker would go stale mid-call.
            SameCall::Continue => {
                self.trackers.insert(freq, Tracker::new(&g, false));
                sink(P25Event::CallContinue {
                    frequency_hz: freq,
                    talkgroup,
                    target: g.to,
                    source: g.from,
                });
            }
            // 5. New call (no tracker, a stale one, or a different TO).
            SameCall::NewCall => {
                self.trackers.insert(freq, Tracker::new(&g, false));
                sink(P25Event::CallStart {
                    frequency_hz: freq,
                    talkgroup,
                    target: g.to,
                    source: g.from,
                });
            }
        }
        // Re-check allocation: a tracked call may not have a running channel yet.
        self.ensure_allocated(freq, sink);
    }

    /// Report a refused TDMA grant, once per call: a grant on a (carrier,
    /// timeslot) that last carried the same TO within [`STALE_MS`] is the
    /// control channel refreshing a call already reported.
    fn refuse_tdma(
        &mut self,
        g: &PendingGrant,
        freq: u64,
        timeslot: u8,
        sink: &mut dyn FnMut(P25Event),
    ) {
        let key = (freq, timeslot);
        let fresh = match self.tdma_refused.get(&key) {
            Some(&(to, last_ms)) => to != g.to || g.now_ms.saturating_sub(last_ms) > STALE_MS,
            None => true,
        };
        if self.tdma_refused.len() >= MAX_TDMA_REFUSED && !self.tdma_refused.contains_key(&key) {
            let now = g.now_ms;
            self.tdma_refused
                .retain(|_, (_, last)| now.saturating_sub(*last) <= STALE_MS);
        }
        self.tdma_refused.insert(key, (g.to, g.now_ms));
        if fresh {
            sink(P25Event::TdmaGrantRefused {
                kind: g.kind,
                frequency_hz: freq,
                timeslot,
                talkgroup: g.talkgroup(),
                target: g.to,
                source: g.from,
            });
        }
    }

    /// Same-call comparison against the tracker on `freq`, mirroring the
    /// reference `ChannelEventTracker.isSameCallCheckingToAndFrom`: a stale
    /// tracker is never the same call (and is dropped here), the TO must match,
    /// and a FROM that is unknown on either side does not break the match
    /// ("sometimes GROUP_VOICE_CHANNEL_USER has a zero FROM address").
    fn same_call(&mut self, freq: u64, to: u32, from: u32, now_ms: u64) -> SameCall {
        let stale = self.trackers.get(&freq).is_some_and(|t| t.is_stale(now_ms));
        if stale {
            self.trackers.remove(&freq);
        }
        match self.trackers.get(&freq) {
            Some(t) if t.to == to => {
                if from != 0 && t.from != 0 && t.from != from {
                    SameCall::Continue
                } else {
                    SameCall::Refresh
                }
            }
            _ => SameCall::NewCall,
        }
    }

    /// Allocate a traffic channel for `freq` if it isn't running already.
    fn ensure_allocated(&mut self, freq: u64, sink: &mut dyn FnMut(P25Event)) {
        if self.allocated.contains(&freq) {
            return;
        }
        if self.pool_available == 0 {
            // Report pool exhaustion once per call — a call that is refreshed
            // by the control channel every few hundred ms must not emit one
            // event per refresh.
            if let Some(t) = self.trackers.get_mut(&freq) {
                if t.pool_exhausted_reported {
                    return;
                }
                t.pool_exhausted_reported = true;
            }
            sink(P25Event::PoolExhausted { frequency_hz: freq });
            return;
        }
        self.pool_available -= 1;
        // Insert before issuing the start request (duplicate-grant guard).
        self.allocated.insert(freq);
        sink(P25Event::TrafficChannelRequest { frequency_hz: freq });
    }

    /// Buffer a grant whose band is not yet known (bounded, dedup, drop-oldest).
    fn buffer_pending(&mut self, g: PendingGrant) {
        // Dedup on (band, channel, TO): a repeated unresolved grant-update just
        // refreshes the existing entry rather than growing the buffer.
        if let Some(existing) = self.pending.iter_mut().find(|p| {
            p.channel.band == g.channel.band
                && p.channel.channel == g.channel.channel
                && p.to == g.to
        }) {
            *existing = g;
            return;
        }
        if self.pending.len() >= MAX_PENDING_GRANTS {
            self.pending.remove(0); // drop oldest
        }
        self.pending.push(g);
    }

    /// Replay buffered grants for a band whose IDEN just arrived; drop any that
    /// have aged past the TTL. Called after inserting a new IDEN band.
    fn resolve_pending(&mut self, band_id: u8, now_ms: u64, sink: &mut dyn FnMut(P25Event)) {
        if self.pending.is_empty() {
            return;
        }
        let mut ready: Vec<PendingGrant> = Vec::new();
        self.pending.retain(|p| {
            if now_ms.saturating_sub(p.now_ms) > PENDING_GRANT_TTL_MS {
                return false; // expired → drop
            }
            if p.channel.band == band_id {
                ready.push(*p);
                return false; // will be replayed
            }
            true // keep: waiting on a different band
        });
        for g in ready {
            // Now resolvable (band just inserted) → drives the normal grant
            // path, dated now (it is only replayed while still fresh).
            self.grant(PendingGrant { now_ms, ..g }, sink);
        }
    }

    /// Traffic-side: call start observed (HDU seen on the traffic channel).
    pub fn traffic_call_started(&mut self, freq: u64, now_ms: u64) {
        if let Some(t) = self.trackers.get_mut(&freq) {
            t.started_by_traffic = true;
            t.event_end_ms = now_ms;
        }
    }

    /// Traffic-side: LDU1 link control refresh. Same-call check uses TO
    /// **and** FROM.
    ///
    /// Event-free convenience form of
    /// [`traffic_call_update_with`](Self::traffic_call_update_with) for callers
    /// that do not (yet) own an event sink at this point.
    pub fn traffic_call_update(&mut self, freq: u64, to: u32, from: u32, now_ms: u64) {
        self.traffic_call_update_with(freq, to, from, now_ms, &mut |_| {});
    }

    /// Traffic-side: LDU1 link control refresh, emitting the call events the
    /// update implies.
    ///
    /// Mirrors the reference `processP1TrafficCurrentUser`: a stale tracked
    /// event is not the same call, so the update starts a new one; a zero FROM
    /// (which `GROUP_VOICE_CHANNEL_USER` sometimes carries) still extends the
    /// current call; and a genuine talker change replaces the tracker and
    /// reports a continuation instead of being dropped.
    ///
    /// A TO that disagrees with a non-stale tracker is ignored: on a traffic
    /// channel the call identity comes from the control channel, and a single
    /// misdecoded LC must not hijack the tracker. (This also keeps the hook a
    /// no-op on the control channel, which has a tracker only by accident.)
    pub fn traffic_call_update_with(
        &mut self,
        freq: u64,
        to: u32,
        from: u32,
        now_ms: u64,
        sink: &mut dyn FnMut(P25Event),
    ) {
        // Nothing tracked here (e.g. the control channel) → nothing to update.
        if !self.trackers.contains_key(&freq) {
            return;
        }
        match self.same_call(freq, to, from, now_ms) {
            SameCall::Refresh => {
                if let Some(t) = self.trackers.get_mut(&freq) {
                    if from != 0 {
                        t.from = from;
                    }
                    t.started_by_traffic = true;
                    t.event_end_ms = now_ms;
                }
            }
            // Genuine talker change on the same TO: replace the tracked event
            // and report a continuation (the reference builds a new event here).
            SameCall::Continue => {
                let talkgroup = self.trackers.get(&freq).map_or(0, |t| t.talkgroup);
                self.trackers.insert(
                    freq,
                    Tracker {
                        to,
                        from,
                        talkgroup,
                        event_end_ms: now_ms,
                        started_by_traffic: true,
                        complete: false,
                        pool_exhausted_reported: false,
                    },
                );
                sink(P25Event::CallContinue {
                    frequency_hz: freq,
                    talkgroup,
                    target: to,
                    source: from,
                });
            }
            SameCall::NewCall => {
                // A non-stale tracker with a disagreeing TO is still there:
                // ignore the update (see the doc note above). If `same_call`
                // dropped a stale tracker, this LDU1 is a new call.
                if self.trackers.contains_key(&freq) {
                    return;
                }
                // Traffic-side LC carries a 16-bit group address; anything
                // wider is a unit-to-unit/telephone TO, which reports 0.
                let talkgroup = u16::try_from(to).unwrap_or(0);
                self.trackers.insert(
                    freq,
                    Tracker {
                        to,
                        from,
                        talkgroup,
                        event_end_ms: now_ms,
                        started_by_traffic: true,
                        complete: false,
                        pool_exhausted_reported: false,
                    },
                );
                sink(P25Event::CallStart {
                    frequency_hz: freq,
                    talkgroup,
                    target: to,
                    source: from,
                });
            }
        }
    }

    /// Traffic-side: call end (TDU/TDULC). Marks the event complete only if
    /// it was started; the tracker is *not* removed (it ages out or is
    /// replaced).
    pub fn traffic_call_ended(&mut self, freq: u64, sink: &mut dyn FnMut(P25Event)) {
        if let Some(t) = self.trackers.get_mut(&freq) {
            if t.started_by_traffic && !t.complete {
                t.complete = true;
                sink(P25Event::CallEnd { frequency_hz: freq });
            }
        }
    }

    /// Teardown: a traffic channel's processing stopped — free the frequency
    /// and return the channel to the pool.
    pub fn traffic_stopped(&mut self, freq: u64) {
        if freq == self.control_frequency_hz {
            return;
        }
        if self.allocated.remove(&freq) {
            self.pool_available += 1;
        }
        self.trackers.remove(&freq);
    }

    /// Teardown: a traffic-channel start request was rejected — return the
    /// channel but keep the tracker.
    pub fn traffic_start_rejected(&mut self, freq: u64) {
        // The control frequency is pre-allocated and never came from the pool;
        // freeing it would both un-protect it (a later grant could hand it out
        // as a traffic channel) and inflate the pool past its size.
        if freq == self.control_frequency_hz {
            return;
        }
        if self.allocated.remove(&freq) {
            self.pool_available += 1;
        }
    }

    /// **Control-channel** retune: stop everything, clear maps and bands,
    /// re-seed the (new) control frequency (spec §13 "Teardown").
    ///
    /// This is a *system change* — a new NAC / a different control channel —
    /// and it is the only place the band plan may be wiped. A traffic decoder
    /// retuning to follow a grant must never call it: the bands it would clear
    /// are the site's, shared with the control decoder and every other traffic
    /// decoder, and clearing them makes every later grant unresolvable. See
    /// [`P25P1Channel::frequency_changed`](crate::P25P1Channel::frequency_changed),
    /// which calls this only in the [`Control`](crate::ChannelRole::Control)
    /// role.
    pub fn retune(&mut self, new_control_frequency_hz: u64) {
        self.allocated.clear();
        self.trackers.clear();
        self.bands.clear();
        self.band_candidates = [None; 16];
        self.pending.clear();
        self.tdma_refused.clear();
        self.pool_available = self.pool_size;
        self.control_frequency_hz = new_control_frequency_hz;
        self.allocated.insert(new_control_frequency_hz);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::iden::IdenBand;

    fn band() -> IdenBand {
        IdenBand {
            id: 1,
            base_hz: 851_000_000,
            spacing_hz: 12_500,
            offset_hz: -45_000_000,
            bandwidth_hz: 12_500,
            slots_per_carrier: 1,
            tdma: false,
        }
    }

    fn grant(tg: u16, ch: u16, src: u32) -> TsbkMessage {
        TsbkMessage::GroupVoiceGrant {
            options: Default::default(),
            channel: ChannelRef {
                band: 1,
                channel: ch,
            },
            talkgroup: tg,
            source: src,
        }
    }

    fn freq_of(ch: u16) -> u64 {
        851_000_000 + 12_500 * u64::from(ch)
    }

    /// Review 2026-09-22, D4: one corrupt IDEN for a band in use used to move
    /// every grant on that band until the next good one.
    #[test]
    fn a_known_band_is_replaced_only_by_two_identical_decodes_in_a_row() {
        let mut t = TrunkingTracker::new(851_050_000);
        let shifted = IdenBand {
            base_hz: 851_100_000,
            ..band()
        };
        let mut echoed = 0;
        let mut sink = |e: P25Event| {
            if matches!(e, P25Event::IdenUpdate { .. }) {
                echoed += 1;
            }
        };
        t.process(&TsbkMessage::IdenUpdate(band()), 0, &mut sink);
        assert_eq!(t.bands().get(1), Some(&band()), "a first sighting applies");

        t.process(&TsbkMessage::IdenUpdate(shifted), 1, &mut sink);
        assert_eq!(t.bands().get(1), Some(&band()), "one decode moves nothing");

        // The plan in force is re-announced in between: the candidate is
        // forgotten, so the next differing decode is a first one again.
        t.process(&TsbkMessage::IdenUpdate(band()), 2, &mut sink);
        t.process(&TsbkMessage::IdenUpdate(shifted), 3, &mut sink);
        assert_eq!(t.bands().get(1), Some(&band()));

        // Two identical decodes in a row: the site really changed its plan.
        t.process(&TsbkMessage::IdenUpdate(shifted), 4, &mut sink);
        assert_eq!(t.bands().get(1), Some(&shifted));
        assert_eq!(echoed, 5, "every decode is still reported to observers");

        // And grants follow the confirmed plan.
        let mut events = Vec::new();
        t.process(&grant(100, 4, 0xA1), 5, &mut |e| events.push(e));
        assert!(events.iter().any(|e| matches!(
            e,
            P25Event::Grant { frequency_hz, .. } if *frequency_hz == 851_100_000 + 12_500 * 4
        )));
    }

    /// A band whose grid cannot be real never resolves a grant, even as the
    /// first sighting of its identifier.
    #[test]
    fn an_implausible_band_never_resolves_grants() {
        let mut t = TrunkingTracker::new(851_050_000);
        // A neighbour site's 18 GHz "band 2" (2026-09-17 comparison, §5), twice over.
        let ghost = IdenBand {
            id: 1,
            base_hz: 18_082_306_730,
            spacing_hz: 38_625,
            offset_hz: -9_250_000,
            bandwidth_hz: 17_375,
            slots_per_carrier: 1,
            tdma: false,
        };
        let mut events = Vec::new();
        t.process(&TsbkMessage::IdenUpdate(ghost), 0, &mut |e| events.push(e));
        t.process(&TsbkMessage::IdenUpdate(ghost), 1, &mut |e| events.push(e));
        assert!(t.bands().get(1).is_none());
        t.process(&grant(100, 4, 0xA1), 2, &mut |e| events.push(e));
        assert!(!events.iter().any(|e| matches!(e, P25Event::Grant { .. })));
        // The real band still applies on its first sighting and releases the
        // buffered grant.
        t.process(&TsbkMessage::IdenUpdate(band()), 3, &mut |e| events.push(e));
        assert!(events.iter().any(|e| matches!(
            e,
            P25Event::Grant { frequency_hz, .. } if *frequency_hz == freq_of(4)
        )));
    }

    #[test]
    fn grant_before_iden_is_unusable() {
        let mut t = TrunkingTracker::new(851_050_000);
        let mut events = Vec::new();
        t.process(&grant(100, 4, 0xA1), 0, &mut |e| events.push(e));
        assert!(events
            .iter()
            .any(|e| matches!(e, P25Event::GrantUnresolvable { band: 1 })));
        assert!(!events.iter().any(|e| matches!(e, P25Event::Grant { .. })));
    }

    #[test]
    fn grant_before_iden_resolves_when_iden_arrives() {
        // Reference behavior (spec §13): a grant whose band is unknown is
        // buffered, then resolved and surfaced once its IDEN arrives. This is
        // the real capture's ordering (grants precede the band-1 IDEN).
        let mut t = TrunkingTracker::new(851_050_000);
        let mut events = Vec::new();
        // Grant on band 1 channel 456 arrives first — only unresolvable so far.
        t.process(&grant(52101, 456, 9_000_038), 0, &mut |e| events.push(e));
        assert!(!events.iter().any(|e| matches!(e, P25Event::Grant { .. })));
        events.clear();
        // Band-1 IDEN arrives → the buffered grant resolves and is surfaced.
        t.process(&TsbkMessage::IdenUpdate(band()), 100, &mut |e| {
            events.push(e)
        });
        let freq = 851_000_000 + 12_500 * 456; // 856.7125 MHz
        assert!(
            events.iter().any(|e| matches!(
                e,
                P25Event::Grant { frequency_hz, talkgroup: 52101, .. } if *frequency_hz == freq
            )),
            "buffered grant should resolve to 856.7125 MHz once the IDEN is known; got {events:?}"
        );
    }

    #[test]
    fn pending_grant_expires_after_ttl() {
        // A buffered grant older than the TTL is dropped, not surfaced late.
        let mut t = TrunkingTracker::new(851_050_000);
        let mut events = Vec::new();
        t.process(&grant(52101, 456, 9_000_038), 0, &mut |e| events.push(e));
        events.clear();
        // IDEN arrives well past the TTL.
        t.process(
            &TsbkMessage::IdenUpdate(band()),
            PENDING_GRANT_TTL_MS + 1,
            &mut |e| events.push(e),
        );
        assert!(!events.iter().any(|e| matches!(e, P25Event::Grant { .. })));
    }

    #[test]
    fn pending_ttl_does_not_outlive_the_staleness_horizon() {
        // Regression (review T2): with a TTL longer than STALE_MS, one IDEN
        // replayed a backlog of long-dead grants as fresh CallStarts and drained
        // the channel pool. A grant older than STALE_MS must never be replayed.
        const { assert!(PENDING_GRANT_TTL_MS <= STALE_MS) };

        let mut t = TrunkingTracker::new(851_050_000);
        let mut events = Vec::new();
        // A burst of 30 distinct grants on the unknown band 1.
        for ch in 0..30u16 {
            t.process(&grant(1000 + ch, ch + 10, 1), 0, &mut |e| events.push(e));
        }
        events.clear();
        // The IDEN lands just past the staleness horizon.
        t.process(&TsbkMessage::IdenUpdate(band()), STALE_MS + 1, &mut |e| {
            events.push(e)
        });
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, P25Event::CallStart { .. }))
                .count(),
            0,
            "stale buffered grants must not be replayed: {events:?}"
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, P25Event::PoolExhausted { .. }))
                .count(),
            0
        );

        // Within the horizon they do still resolve.
        let mut t2 = TrunkingTracker::new(851_050_000);
        let mut ev2 = Vec::new();
        t2.process(&grant(2000, 40, 1), 0, &mut |e| ev2.push(e));
        ev2.clear();
        t2.process(&TsbkMessage::IdenUpdate(band()), STALE_MS, &mut |e| {
            ev2.push(e)
        });
        assert!(ev2.iter().any(|e| matches!(
            e,
            P25Event::CallStart {
                talkgroup: 2000,
                ..
            }
        )));
    }

    #[test]
    fn pending_buffer_is_capped_and_deduped() {
        let mut t = TrunkingTracker::new(851_050_000);
        let mut sink = |_: P25Event| {};
        // The same (band, channel, TO) repeated only refreshes one entry.
        for _ in 0..10 {
            t.process(&grant(500, 7, 1), 0, &mut sink);
        }
        assert_eq!(t.pending.len(), 1);
        // A different TO on the same channel is a different entry.
        t.process(&grant(501, 7, 1), 0, &mut sink);
        assert_eq!(t.pending.len(), 2);

        // Overflow is bounded, dropping the oldest.
        let mut t2 = TrunkingTracker::new(851_050_000);
        for ch in 0..(MAX_PENDING_GRANTS as u16 + 10) {
            t2.process(&grant(1000 + ch, ch, 1), 0, &mut sink);
        }
        assert_eq!(t2.pending.len(), MAX_PENDING_GRANTS);
        // The first 10 were dropped; the newest is still present.
        assert_eq!(t2.pending[0].to, 1010);
        assert_eq!(
            t2.pending[MAX_PENDING_GRANTS - 1].to,
            u32::from(1000 + MAX_PENDING_GRANTS as u16 + 9)
        );
    }

    #[test]
    fn grant_flow_and_pool() {
        let mut t = TrunkingTracker::new(853_000_000);
        let mut events = Vec::new();
        t.process(&TsbkMessage::IdenUpdate(band()), 0, &mut |e| events.push(e));
        t.process(&grant(100, 4, 0xA1), 10, &mut |e| events.push(e));
        let freq = freq_of(4);
        assert!(events.iter().any(
            |e| matches!(e, P25Event::CallStart { frequency_hz, talkgroup: 100, .. } if *frequency_hz == freq)
        ));
        assert!(events.iter().any(
            |e| matches!(e, P25Event::TrafficChannelRequest { frequency_hz } if *frequency_hz == freq)
        ));
        // Repeat grant, same talker → no new start or request.
        events.clear();
        t.process(&grant(100, 4, 0xA1), 20, &mut |e| events.push(e));
        assert!(!events
            .iter()
            .any(|e| matches!(e, P25Event::CallStart { .. })));
        assert!(!events
            .iter()
            .any(|e| matches!(e, P25Event::TrafficChannelRequest { .. })));
        // Different talker → continuation.
        events.clear();
        t.process(&grant(100, 4, 0xB2), 30, &mut |e| events.push(e));
        assert!(events
            .iter()
            .any(|e| matches!(e, P25Event::CallContinue { source: 0xB2, .. })));
    }

    #[test]
    fn group_grant_reports_talkgroup_and_target() {
        let mut t = TrunkingTracker::new(853_000_000);
        let mut events = Vec::new();
        t.process(&TsbkMessage::IdenUpdate(band()), 0, &mut |e| events.push(e));
        t.process(&grant(52101, 4, 0xA1), 10, &mut |e| events.push(e));
        assert!(events.iter().any(|e| matches!(
            e,
            P25Event::Grant {
                kind: CallKind::Group,
                talkgroup: 52101,
                target: 52101,
                source: 0xA1,
                ..
            }
        )));
        assert!(events.iter().any(|e| matches!(
            e,
            P25Event::CallStart {
                talkgroup: 52101,
                target: 52101,
                ..
            }
        )));
    }

    #[test]
    fn unit_to_unit_and_telephone_keep_the_full_24_bit_target() {
        // Review T1: `to as u16` truncated 24-bit addresses. A non-group call
        // reports talkgroup 0 (as documented) and the full TO in `target`.
        let mut t = TrunkingTracker::new(853_000_000);
        let mut events = Vec::new();
        t.process(&TsbkMessage::IdenUpdate(band()), 0, &mut |e| events.push(e));

        t.process(
            &TsbkMessage::UnitToUnitGrant {
                update: false,
                channel: ChannelRef {
                    band: 1,
                    channel: 4,
                },
                target: 0xAB_CDEF,
                source: 0x12_3456,
            },
            10,
            &mut |e| events.push(e),
        );
        assert!(
            events.iter().any(|e| matches!(
                e,
                P25Event::Grant {
                    kind: CallKind::UnitToUnit,
                    talkgroup: 0,
                    target: 0xAB_CDEF,
                    source: 0x12_3456,
                    ..
                }
            )),
            "{events:?}"
        );
        assert!(events.iter().any(|e| matches!(
            e,
            P25Event::CallStart {
                talkgroup: 0,
                target: 0xAB_CDEF,
                source: 0x12_3456,
                ..
            }
        )));

        // Telephone update (opcode 9): the address is a 24-bit TO.
        events.clear();
        t.process(
            &TsbkMessage::TelephoneInterconnectGrant {
                update: true,
                options: Default::default(),
                channel: ChannelRef {
                    band: 1,
                    channel: 5,
                },
                call_timer: 300,
                address: 0x00_C001,
                address_is_talkgroup: true,
            },
            20,
            &mut |e| events.push(e),
        );
        assert!(events.iter().any(|e| matches!(
            e,
            P25Event::Grant {
                kind: CallKind::Telephone,
                talkgroup: 0,
                target: 0x00_C001,
                source: 0,
                update: true,
                ..
            }
        )));

        // Telephone grant (opcode 8): the address is the FROM, so TO is 0.
        events.clear();
        t.process(
            &TsbkMessage::TelephoneInterconnectGrant {
                update: false,
                options: Default::default(),
                channel: ChannelRef {
                    band: 1,
                    channel: 6,
                },
                call_timer: 300,
                address: 0x0D_EAD1,
                address_is_talkgroup: false,
            },
            30,
            &mut |e| events.push(e),
        );
        assert!(events.iter().any(|e| matches!(
            e,
            P25Event::Grant {
                kind: CallKind::Telephone,
                talkgroup: 0,
                target: 0,
                source: 0x0D_EAD1,
                ..
            }
        )));
    }

    #[test]
    fn two_unit_to_unit_targets_that_share_low_16_bits_are_distinct_calls() {
        // The truncation bug made 0x01_0064 and 0x02_0064 the same "talkgroup".
        let mut t = TrunkingTracker::new(853_000_000);
        let mut events = Vec::new();
        t.process(&TsbkMessage::IdenUpdate(band()), 0, &mut |e| events.push(e));
        let mk = |target: u32| TsbkMessage::UnitToUnitGrant {
            update: false,
            channel: ChannelRef {
                band: 1,
                channel: 4,
            },
            target,
            source: 7,
        };
        t.process(&mk(0x01_0064), 10, &mut |e| events.push(e));
        events.clear();
        t.process(&mk(0x02_0064), 20, &mut |e| events.push(e));
        assert!(
            events.iter().any(|e| matches!(
                e,
                P25Event::CallStart {
                    target: 0x02_0064,
                    ..
                }
            )),
            "a different 24-bit target is a different call: {events:?}"
        );
    }

    #[test]
    fn pool_exhaustion() {
        let mut t = TrunkingTracker::new(851_050_000);
        let mut events = Vec::new();
        t.process(&TsbkMessage::IdenUpdate(band()), 0, &mut |e| events.push(e));
        for ch in 0..DEFAULT_POOL_SIZE as u16 + 1 {
            t.process(&grant(1000 + ch, ch + 10, 1), 5, &mut |e| events.push(e));
        }
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, P25Event::TrafficChannelRequest { .. }))
                .count() as u32,
            DEFAULT_POOL_SIZE
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, P25Event::PoolExhausted { .. }))
                .count(),
            1
        );
        // Teardown returns capacity.
        t.traffic_stopped(freq_of(10));
        events.clear();
        t.process(&grant(2000, 200, 1), 6, &mut |e| events.push(e));
        assert!(events
            .iter()
            .any(|e| matches!(e, P25Event::TrafficChannelRequest { .. })));
    }

    #[test]
    fn pool_exhausted_reported_once_per_call() {
        // Review T4: control refreshes of the same unstartable call must not
        // re-emit PoolExhausted; a *new* call on that frequency must.
        let mut t = TrunkingTracker::new(851_050_000);
        let mut events = Vec::new();
        t.process(&TsbkMessage::IdenUpdate(band()), 0, &mut |e| events.push(e));
        for ch in 0..DEFAULT_POOL_SIZE as u16 {
            t.process(&grant(1000 + ch, ch + 10, 1), 5, &mut |e| events.push(e));
        }
        events.clear();
        // Five control refreshes of one over-capacity call → one event.
        for k in 0..5u64 {
            t.process(&grant(7777, 200, 1), 10 + k * 100, &mut |e| events.push(e));
        }
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, P25Event::PoolExhausted { .. }))
                .count(),
            1,
            "{events:?}"
        );
        // A different call on the same frequency reports again.
        events.clear();
        t.process(&grant(8888, 200, 1), 600, &mut |e| events.push(e));
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, P25Event::PoolExhausted { .. }))
                .count(),
            1
        );
    }

    #[test]
    fn traffic_start_rejected_returns_the_channel_but_never_the_control_frequency() {
        // Review T3: without the control-frequency guard this both un-protected
        // the control channel and inflated the pool to 21.
        let control = 851_050_000;
        let mut t = TrunkingTracker::new(control);
        let mut events = Vec::new();
        t.process(&TsbkMessage::IdenUpdate(band()), 0, &mut |e| events.push(e));
        // Channel 40 → 851.5 MHz (channel 4 *is* the control frequency here).
        t.process(&grant(100, 40, 1), 10, &mut |e| events.push(e));
        let freq = freq_of(40);
        assert_ne!(freq, control);
        assert_eq!(t.pool_available, DEFAULT_POOL_SIZE - 1);

        // A rejected start returns the channel and keeps the tracker.
        t.traffic_start_rejected(freq);
        assert_eq!(t.pool_available, DEFAULT_POOL_SIZE);
        assert!(!t.allocated.contains(&freq));
        assert!(t.trackers.contains_key(&freq));

        // A rejection naming the control frequency changes nothing.
        t.traffic_start_rejected(control);
        assert_eq!(t.pool_available, DEFAULT_POOL_SIZE);
        assert!(
            t.allocated.contains(&control),
            "the control frequency must stay protected"
        );
    }

    #[test]
    fn retune_clears_all_state_and_protects_the_new_control_frequency() {
        let mut t = TrunkingTracker::new(851_050_000);
        let mut events = Vec::new();
        t.process(&TsbkMessage::IdenUpdate(band()), 0, &mut |e| events.push(e));
        t.process(&grant(100, 40, 1), 10, &mut |e| events.push(e));
        assert_eq!(t.pool_available, DEFAULT_POOL_SIZE - 1);
        // An unresolvable grant on an unknown band leaves a pending entry.
        t.process(
            &TsbkMessage::GroupVoiceGrant {
                options: Default::default(),
                channel: ChannelRef {
                    band: 9,
                    channel: 3,
                },
                talkgroup: 5,
                source: 1,
            },
            10,
            &mut |e| events.push(e),
        );
        assert!(!t.pending.is_empty());

        t.retune(852_000_000);
        assert!(t.trackers.is_empty());
        assert!(t.pending.is_empty());
        assert_eq!(t.pool_available, DEFAULT_POOL_SIZE);
        assert!(t.bands.get(1).is_none());
        assert_eq!(t.allocated.len(), 1);
        assert!(t.allocated.contains(&852_000_000));
        // The old control frequency is no longer protected...
        assert_ne!(t.control_frequency_hz, 851_050_000);
        // ...and the band plan really is gone: grants are unresolvable again.
        events.clear();
        t.process(&grant(100, 4, 1), 20, &mut |e| events.push(e));
        assert!(events
            .iter()
            .any(|e| matches!(e, P25Event::GrantUnresolvable { band: 1 })));
    }

    #[test]
    fn stale_tracker_replaced_and_update_as_fresh_grant() {
        let mut t = TrunkingTracker::new(851_050_000);
        let mut events = Vec::new();
        t.process(&TsbkMessage::IdenUpdate(band()), 0, &mut |e| events.push(e));
        // A grant *update* for an untracked frequency = fresh grant.
        t.process(
            &TsbkMessage::GroupVoiceGrantUpdate {
                a: (
                    ChannelRef {
                        band: 1,
                        channel: 7,
                    },
                    300,
                ),
                b: None,
            },
            100,
            &mut |e| events.push(e),
        );
        assert!(events
            .iter()
            .any(|e| matches!(e, P25Event::CallStart { .. })));
        // 4 s later the tracker is stale; a new grant for a different group
        // on the same frequency starts a new call.
        events.clear();
        t.process(&grant(400, 7, 9), 4200, &mut |e| events.push(e));
        assert!(events
            .iter()
            .any(|e| matches!(e, P25Event::CallStart { talkgroup: 400, .. })));
    }

    #[test]
    fn grant_update_pair_b_is_processed() {
        let mut t = TrunkingTracker::new(851_050_000);
        let mut events = Vec::new();
        t.process(&TsbkMessage::IdenUpdate(band()), 0, &mut |e| events.push(e));
        t.process(
            &TsbkMessage::GroupVoiceGrantUpdate {
                a: (
                    ChannelRef {
                        band: 1,
                        channel: 7,
                    },
                    300,
                ),
                b: Some((
                    ChannelRef {
                        band: 1,
                        channel: 8,
                    },
                    301,
                )),
            },
            100,
            &mut |e| events.push(e),
        );
        assert!(events.iter().any(
            |e| matches!(e, P25Event::CallStart { talkgroup: 300, frequency_hz, .. } if *frequency_hz == freq_of(7))
        ));
        assert!(events.iter().any(
            |e| matches!(e, P25Event::CallStart { talkgroup: 301, frequency_hz, .. } if *frequency_hz == freq_of(8))
        ));
    }

    #[test]
    fn control_side_continue_unfreezes_control_updates() {
        // Review T9: the reference *replaces* the tracked event on a talker
        // change, so the new call is control-driven again. Keeping
        // `started_by_traffic` froze `event_end_ms` and the tracker went stale
        // mid-call.
        let mut t = TrunkingTracker::new(851_050_000);
        let mut events = Vec::new();
        t.process(&TsbkMessage::IdenUpdate(band()), 0, &mut |e| events.push(e));
        t.process(&grant(100, 4, 0xA1), 10, &mut |e| events.push(e));
        let freq = freq_of(4);
        // The traffic decoder takes over.
        t.traffic_call_started(freq, 20);
        assert!(t.trackers[&freq].started_by_traffic);
        // A control-side talker change replaces the tracked call.
        events.clear();
        t.process(&grant(100, 4, 0xB2), 1000, &mut |e| events.push(e));
        assert!(events
            .iter()
            .any(|e| matches!(e, P25Event::CallContinue { source: 0xB2, .. })));
        let tk = &t.trackers[&freq];
        assert!(!tk.started_by_traffic, "the new call is control-driven");
        assert_eq!(tk.from, 0xB2);
        assert_eq!(tk.event_end_ms, 1000);
        // ...so later control refreshes still extend it.
        t.process(&grant(100, 4, 0xB2), 2500, &mut |e| events.push(e));
        assert_eq!(t.trackers[&freq].event_end_ms, 2500);
    }

    #[test]
    fn traffic_side_ldu1_group_voice_hook() {
        // Mirrors channel.rs wiring: an LDU1 Group Voice Channel User names the
        // active call's talkgroup(to)/source(from) on the tuned frequency.
        let mut t = TrunkingTracker::new(853_000_000);
        let mut events = Vec::new();
        t.process(&TsbkMessage::IdenUpdate(band()), 0, &mut |e| events.push(e));
        // Grant with an unknown talker (from == 0) creates the tracker.
        t.process(&grant(52101, 4, 0), 10, &mut |e| events.push(e));
        let freq = freq_of(4);

        // LDU1 LC refresh: same TO, learns the FROM, marks started-by-traffic.
        t.traffic_call_update(freq, 52101, 0x89545e, 20);
        let tk = t.trackers.get(&freq).expect("tracker");
        assert_eq!(tk.to, 52101);
        assert_eq!(tk.from, 0x89545e);
        assert!(tk.started_by_traffic);

        // Wrong TO on the same frequency must not overwrite the call.
        t.traffic_call_update(freq, 999, 0x111, 30);
        let tk = t.trackers.get(&freq).unwrap();
        assert_eq!(tk.to, 52101);
        assert_eq!(tk.from, 0x89545e);

        // A control-side grant on a control frequency has no tracker → the hook
        // is a harmless no-op there (control carries no voice anyway).
        t.traffic_call_update(853_000_000, 1, 2, 40);
        assert!(!t.trackers.contains_key(&853_000_000));
    }

    #[test]
    fn traffic_update_with_zero_from_extends_the_same_call() {
        // Review T9(b): "sometimes GROUP_VOICE_CHANNEL_USER has a zero FROM" —
        // that is still the same call, and it must extend the event.
        let mut t = TrunkingTracker::new(853_000_000);
        let mut events = Vec::new();
        t.process(&TsbkMessage::IdenUpdate(band()), 0, &mut |e| events.push(e));
        t.process(&grant(52101, 4, 0), 10, &mut |e| events.push(e));
        let freq = freq_of(4);
        t.traffic_call_update(freq, 52101, 0x89545e, 20);

        events.clear();
        t.traffic_call_update_with(freq, 52101, 0, 500, &mut |e| events.push(e));
        let tk = &t.trackers[&freq];
        assert_eq!(tk.from, 0x89545e, "a zero FROM must not erase the talker");
        assert_eq!(tk.event_end_ms, 500, "the call must still be extended");
        assert!(events.is_empty(), "no new call event: {events:?}");
    }

    #[test]
    fn traffic_update_talker_change_reports_a_continuation() {
        // Review T9(c): a genuine traffic-side talker change replaces the
        // tracked call instead of being silently dropped.
        let mut t = TrunkingTracker::new(853_000_000);
        let mut events = Vec::new();
        t.process(&TsbkMessage::IdenUpdate(band()), 0, &mut |e| events.push(e));
        t.process(&grant(52101, 4, 0), 10, &mut |e| events.push(e));
        let freq = freq_of(4);
        t.traffic_call_update(freq, 52101, 0x89545e, 20);

        events.clear();
        t.traffic_call_update_with(freq, 52101, 0x999999, 900, &mut |e| events.push(e));
        assert!(
            events.iter().any(|e| matches!(
                e,
                P25Event::CallContinue {
                    talkgroup: 52101,
                    target: 52101,
                    source: 0x999999,
                    ..
                }
            )),
            "{events:?}"
        );
        let tk = &t.trackers[&freq];
        assert_eq!(tk.from, 0x999999);
        assert_eq!(tk.event_end_ms, 900);
        assert!(tk.started_by_traffic);
        assert!(!tk.complete);
    }

    #[test]
    fn traffic_update_on_a_stale_tracker_starts_a_new_call() {
        // Review T9(a): a stale tracked event is never the same call.
        let mut t = TrunkingTracker::new(853_000_000);
        let mut events = Vec::new();
        t.process(&TsbkMessage::IdenUpdate(band()), 0, &mut |e| events.push(e));
        t.process(&grant(52101, 4, 0xA1), 10, &mut |e| events.push(e));
        let freq = freq_of(4);

        events.clear();
        let late = 10 + STALE_MS + 1;
        t.traffic_call_update_with(freq, 52101, 0xA1, late, &mut |e| events.push(e));
        assert!(
            events.iter().any(|e| matches!(
                e,
                P25Event::CallStart {
                    talkgroup: 52101,
                    source: 0xA1,
                    ..
                }
            )),
            "a stale tracker must be replaced by a new call: {events:?}"
        );
        let tk = &t.trackers[&freq];
        assert_eq!(tk.event_end_ms, late);
        assert!(tk.started_by_traffic);
        assert!(!tk.complete);
    }

    #[test]
    fn traffic_call_ended_marks_complete_once() {
        let mut t = TrunkingTracker::new(853_000_000);
        let mut events = Vec::new();
        t.process(&TsbkMessage::IdenUpdate(band()), 0, &mut |e| events.push(e));
        t.process(&grant(52101, 4, 0xA1), 10, &mut |e| events.push(e));
        let freq = freq_of(4);
        // Not started by traffic yet → no CallEnd.
        events.clear();
        t.traffic_call_ended(freq, &mut |e| events.push(e));
        assert!(events.is_empty());
        // Started, then ended → exactly one CallEnd.
        t.traffic_call_started(freq, 20);
        t.traffic_call_ended(freq, &mut |e| events.push(e));
        t.traffic_call_ended(freq, &mut |e| events.push(e));
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, P25Event::CallEnd { .. }))
                .count(),
            1
        );
    }

    /// A Phase 2 band as opcode 51 describes one: 12.5 kHz carriers, two
    /// slots each. Channel `2k` is slot 1 and `2k + 1` slot 2 of carrier `k`.
    fn tdma_band() -> IdenBand {
        IdenBand {
            id: 2,
            base_hz: 851_006_250,
            spacing_hz: 12_500,
            offset_hz: -45_000_000,
            bandwidth_hz: 12_500,
            slots_per_carrier: 2,
            tdma: true,
        }
    }

    fn tdma_grant(tg: u16, ch: u16, src: u32) -> TsbkMessage {
        TsbkMessage::GroupVoiceGrant {
            options: Default::default(),
            channel: ChannelRef {
                band: 2,
                channel: ch,
            },
            talkgroup: tg,
            source: src,
        }
    }

    fn refusals(events: &[P25Event]) -> Vec<(u64, u8, u16)> {
        events
            .iter()
            .filter_map(|e| match e {
                P25Event::TdmaGrantRefused {
                    frequency_hz,
                    timeslot,
                    talkgroup,
                    ..
                } => Some((*frequency_hz, *timeslot, *talkgroup)),
                _ => None,
            })
            .collect()
    }

    /// Review 2026-09-22 §4 gap 2 (T-D1): a grant on a TDMA channel is seen
    /// (the `Grant` still goes out) but never tracked, never allocated and
    /// never started, and it is reported once per call — per carrier *and*
    /// slot, so two calls sharing a carrier are two refusals, and a control
    /// channel refreshing one call every few hundred ms is still one.
    #[test]
    fn a_grant_on_a_tdma_channel_is_refused_once_per_call_and_costs_no_slot() {
        let mut t = TrunkingTracker::new(851_050_000);
        let mut events = Vec::new();
        t.process(&TsbkMessage::IdenUpdate(band()), 0, &mut |e| events.push(e));
        t.process(&TsbkMessage::IdenUpdate(tdma_band()), 0, &mut |e| {
            events.push(e)
        });
        let pool = t.pool_available();
        let carrier = 851_006_250 + 12_500 * 5;
        events.clear();

        // Slot 1 of carrier 5, then its refreshes.
        t.process(&tdma_grant(52103, 10, 0xA1), 100, &mut |e| events.push(e));
        t.process(&tdma_grant(52103, 10, 0), 400, &mut |e| events.push(e));
        t.process(&tdma_grant(52103, 10, 0), 700, &mut |e| events.push(e));
        // Slot 2 of the same carrier: another call.
        t.process(&tdma_grant(57011, 11, 0xB2), 800, &mut |e| events.push(e));
        assert_eq!(
            refusals(&events),
            vec![(carrier, 1, 52103), (carrier, 2, 57011)]
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, P25Event::Grant { frequency_hz, .. } if *frequency_hz == carrier))
                .count(),
            4,
            "every grant is still reported to observers"
        );
        assert!(
            !events.iter().any(|e| matches!(
                e,
                P25Event::CallStart { .. }
                    | P25Event::CallContinue { .. }
                    | P25Event::TrafficChannelRequest { .. }
                    | P25Event::PoolExhausted { .. }
            )),
            "nothing is followed: {events:?}"
        );
        assert_eq!(t.pool_available(), pool, "no pool slot was spent");
        assert_eq!(t.event_end_ms(carrier), None, "nothing is tracked");

        // A new talkgroup on slot 1 is a new call; so is the old one coming
        // back after the staleness horizon.
        events.clear();
        t.process(&tdma_grant(57151, 10, 0), 900, &mut |e| events.push(e));
        t.process(&tdma_grant(57151, 10, 0), 900 + STALE_MS + 1, &mut |e| {
            events.push(e)
        });
        assert_eq!(
            refusals(&events),
            vec![(carrier, 1, 57151), (carrier, 1, 57151)]
        );

        // The FDMA band beside it is followed exactly as before.
        events.clear();
        t.process(&grant(100, 40, 0xA1), 5_000, &mut |e| events.push(e));
        assert!(events.iter().any(|e| matches!(
            e,
            P25Event::TrafficChannelRequest { frequency_hz } if *frequency_hz == freq_of(40)
        )));
        assert!(refusals(&events).is_empty());
        assert_eq!(t.pool_available(), pool - 1);
    }

    /// A TDMA grant heard before its band is known is buffered like any other
    /// and refused, not followed, when the band arrives.
    #[test]
    fn a_buffered_grant_resolved_onto_a_tdma_band_is_refused() {
        let mut t = TrunkingTracker::new(851_050_000);
        let mut events = Vec::new();
        t.process(&tdma_grant(52103, 10, 0xA1), 100, &mut |e| events.push(e));
        assert!(refusals(&events).is_empty());
        t.process(&TsbkMessage::IdenUpdate(tdma_band()), 200, &mut |e| {
            events.push(e)
        });
        assert_eq!(
            refusals(&events),
            vec![(851_006_250 + 12_500 * 5, 1, 52103)]
        );
        assert!(!events
            .iter()
            .any(|e| matches!(e, P25Event::TrafficChannelRequest { .. })));
    }
}
