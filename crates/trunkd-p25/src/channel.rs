// SPDX-License-Identifier: GPL-3.0-or-later
//! Public push-style API: [`P25P1Channel`].
//!
//! Feed complex baseband samples at channel rate; receive [`P25Event`]s via
//! a callback (or bridge them onto a `trunkd_core` bounded channel with
//! [`P25P1Channel::process_into`]).
//!
//! # One tracker per site
//!
//! Spec §13 (and the reference `P25TrafficChannelManager`) models a site as
//! **one** trunking state machine driven by the control decoder *and* every
//! traffic decoder it spawns. Build the tracker with
//! [`TrunkingTracker::new_shared`], give it to one
//! [`new_control`](P25P1Channel::new_control) channel and to a
//! [`new_traffic`](P25P1Channel::new_traffic) channel per followed grant:
//!
//! ```no_run
//! use trunkd_p25::{ChannelRole, DemodMode, P25P1Channel, TrunkingTracker};
//!
//! let tracker = TrunkingTracker::new_shared(851_050_000);
//! let control = P25P1Channel::new_control(50_000, DemodMode::Lsm, 851_050_000, tracker.clone())?;
//! // …on a `TrafficChannelRequest { frequency_hz }` event:
//! let traffic = P25P1Channel::new_traffic(50_000, DemodMode::Lsm, 856_712_500, tracker.clone())?;
//! assert_eq!(traffic.role(), ChannelRole::Traffic);
//! # Ok::<(), trunkd_p25::P25Error>(())
//! ```
//!
//! [`P25P1Channel::new`] remains the standalone form: a channel with a private
//! tracker, useful for decoding a single capture in isolation.
//!
//! # Time
//!
//! The tracker's millisecond clock comes from each channel's own symbol
//! counter, which keeps a decode deterministic for a given input file. Every
//! channel of one device is fed from the same sample stream and so advances in
//! lockstep, which is what lets a shared tracker see a single consistent clock.
//! A consumer that prefers wall-clock time can drive the tracker explicitly
//! instead — [`TrunkingTracker`]'s hooks all take `now_ms` from the caller.

use num_complex::Complex;
use trunkd_core::chan::BoundedSender;

use crate::demod::{C4fmDemodulator, LsmDemodulator, Symbol};
use crate::dsp::{Ddc, DemodMode, FrontEnd};
use crate::event::P25Event;
use crate::framer::{Framer, FramerStats};
use crate::message::{parse_ambt_unit_grant, TsbkMessage};
use crate::payload::LinkControl;
use crate::trunking::{SharedTracker, TrunkingTracker};
use crate::P25Error;

/// Lowest channel-rate sample rate the front end accepts (exclusive bound).
const MIN_SAMPLE_RATE: u32 = 9600;

/// What a channel is doing for its site (spec §13).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelRole {
    /// Follows the control channel: TSBK grants and IDEN updates drive the
    /// tracker; a retune is a system change and resets it.
    Control,
    /// Follows one granted voice call: HDU/LDU/TDU drive the traffic-side
    /// hooks (call started / refresh / ended) for this channel's frequency.
    Traffic,
    /// Reserved for a granted SNDCP data channel. Routed to like [`Traffic`],
    /// but the decode path is not built yet (multi-tuner core W1 leaves this as
    /// the seam for the data follower). A channel is never constructed with this
    /// role today.
    Data,
}

enum Demod {
    Lsm(Box<LsmDemodulator>),
    C4fm(Box<C4fmDemodulator>),
}

impl Demod {
    #[inline]
    fn push(&mut self, s: Complex<f32>) -> Option<Symbol> {
        match self {
            Demod::Lsm(d) => d.push(s),
            Demod::C4fm(d) => d.push(s),
        }
    }

    fn pll(&self) -> f32 {
        match self {
            Demod::Lsm(d) => d.pll(),
            Demod::C4fm(d) => d.pll(),
        }
    }

    fn reset_pll(&mut self) {
        match self {
            Demod::Lsm(d) => d.reset_pll(),
            Demod::C4fm(d) => d.reset_pll(),
        }
    }
}

/// A complete P25 Phase 1 receive channel: front end → demodulator → framer
/// → trunking tracker.
pub struct P25P1Channel {
    frontend: FrontEnd,
    demod: Demod,
    framer: Framer,
    trunking: SharedTracker,
    role: ChannelRole,
    /// Decimated rate, needed for §14 frequency-error conversion.
    decimated_rate: f64,
    /// Frequency this channel is tuned to — the key the traffic-side call
    /// hooks use (spec §13 tracks calls by downlink frequency).
    channel_frequency_hz: u64,
    /// Traffic role: a call is in progress on this frequency (voice seen, no
    /// terminator yet). Gates the one-shot "call started" hook.
    call_active: bool,
    symbol_count: u64,
    /// Scratch buffer reused across frames (no steady-state allocation).
    scratch: Vec<P25Event>,
}

impl P25P1Channel {
    /// Create a **standalone** channel with a private trunking tracker.
    ///
    /// `sample_rate`: rate of the samples fed to [`process`](Self::process)
    /// (must be > 9600; e.g. 25000 or 50000). `mode` selects the LSM
    /// (primary) or C4FM demodulator. `channel_frequency_hz` seeds the
    /// tracker's control frequency.
    ///
    /// The channel takes the [`Control`](ChannelRole::Control) role: it is a
    /// site of one, with nothing to hand calls off to. To follow calls across
    /// channels use [`new_control`](Self::new_control) /
    /// [`new_traffic`](Self::new_traffic) with a shared tracker.
    pub fn new(
        sample_rate: u32,
        mode: DemodMode,
        channel_frequency_hz: u64,
    ) -> Result<Self, P25Error> {
        let tracker = TrunkingTracker::new_shared(channel_frequency_hz);
        Self::build(
            sample_rate,
            mode,
            channel_frequency_hz,
            tracker,
            ChannelRole::Control,
        )
    }

    /// Create the site's **control** channel, driving `tracker`.
    pub fn new_control(
        sample_rate: u32,
        mode: DemodMode,
        control_frequency_hz: u64,
        tracker: SharedTracker,
    ) -> Result<Self, P25Error> {
        Self::build(
            sample_rate,
            mode,
            control_frequency_hz,
            tracker,
            ChannelRole::Control,
        )
    }

    /// Create a **traffic** channel following the call granted on
    /// `traffic_frequency_hz`, sharing the site's `tracker`.
    ///
    /// The shared tracker is what makes the traffic side function: the call
    /// entry for this frequency was created by the grant on the control
    /// channel, and the band plan the tracker holds is already the site's
    /// (spec §13's "IDEN preload" needs no copying).
    pub fn new_traffic(
        sample_rate: u32,
        mode: DemodMode,
        traffic_frequency_hz: u64,
        tracker: SharedTracker,
    ) -> Result<Self, P25Error> {
        Self::build(
            sample_rate,
            mode,
            traffic_frequency_hz,
            tracker,
            ChannelRole::Traffic,
        )
    }

    /// Create a **data** channel following a granted SNDCP data channel on
    /// `data_frequency_hz`, sharing the site's `tracker`.
    ///
    /// Like [`new_traffic`](Self::new_traffic), but it takes the
    /// [`Data`](ChannelRole::Data) role: it decodes and dispatches PDUs (via
    /// [`P25Event::Pdu`]) without any voice-call bookkeeping. The tracker is
    /// shared only so a data burst that happens to carry a stray TSBK does not
    /// need a private one; the data path itself does not drive call state.
    pub fn new_data(
        sample_rate: u32,
        mode: DemodMode,
        data_frequency_hz: u64,
        tracker: SharedTracker,
    ) -> Result<Self, P25Error> {
        Self::build(
            sample_rate,
            mode,
            data_frequency_hz,
            tracker,
            ChannelRole::Data,
        )
    }

    fn build(
        sample_rate: u32,
        mode: DemodMode,
        channel_frequency_hz: u64,
        trunking: SharedTracker,
        role: ChannelRole,
    ) -> Result<Self, P25Error> {
        if sample_rate <= MIN_SAMPLE_RATE {
            return Err(P25Error::SampleRateTooLow {
                rate: sample_rate,
                min: MIN_SAMPLE_RATE,
            });
        }
        let frontend = FrontEnd::new(sample_rate, mode)?;
        let sps = frontend.samples_per_symbol;
        let decimated_rate = frontend.decimated_rate;
        let demod = match mode {
            DemodMode::Lsm => Demod::Lsm(Box::new(LsmDemodulator::new(sps))),
            DemodMode::C4fm => Demod::C4fm(Box::new(C4fmDemodulator::new(sps))),
        };
        Ok(P25P1Channel {
            frontend,
            demod,
            framer: Framer::new(),
            trunking,
            role,
            decimated_rate,
            channel_frequency_hz,
            call_active: false,
            symbol_count: 0,
            scratch: Vec::with_capacity(64),
        })
    }

    /// Millisecond clock derived from the symbol counter — deterministic for
    /// a given input stream.
    fn now_ms(&self) -> u64 {
        self.symbol_count * 1000 / 4800
    }

    /// Feed baseband samples; deliver events to `sink`.
    pub fn process(&mut self, samples: &[Complex<f32>], sink: &mut dyn FnMut(P25Event)) {
        for &s in samples {
            let Some(filtered) = self.frontend.push(s) else {
                continue;
            };
            let Some(sym) = self.demod.push(filtered) else {
                continue;
            };
            self.symbol_count += 1;

            // Collect framer events into the reusable scratch buffer, then fan
            // out. The buffer is moved out for the duration so the dispatch
            // loop can borrow `self` mutably (the tracker hooks need it)
            // without cloning every event.
            let mut scratch = std::mem::take(&mut self.scratch);
            scratch.clear();
            let nid_validated = self
                .framer
                .process(sym.soft, sym.hard, &mut |e| scratch.push(e));

            if nid_validated {
                // §14: correctionHz = decimatedRate / 2π × pllError.
                let hz = self.decimated_rate / std::f64::consts::TAU * f64::from(self.demod.pll());
                sink(P25Event::FrequencyError { hz });
            }

            let now = self.now_ms();
            for e in scratch.drain(..) {
                self.dispatch(e, now, sink);
            }
            self.scratch = scratch;
        }
    }

    /// Emit one framer event and run the tracker hooks it implies.
    fn dispatch(&mut self, e: P25Event, now: u64, sink: &mut dyn FnMut(P25Event)) {
        // A TSBK's message is needed after the event is handed to the sink;
        // it is the only event whose payload outlives the emit.
        let tsbk: Option<TsbkMessage> = match &e {
            P25Event::Tsbk { tsbk, .. } => Some(tsbk.message.clone()),
            _ => None,
        };
        // A unit-to-unit voice grant reaches this site as a multi-block (AMBT)
        // message, not a TSBK, so it is decoded here and handed to the tracker
        // by the same path — see `UnitGrantAmbt`. Only a completed, block-valid
        // PDU is read: acting on a payload whose CRC-32 failed would put a
        // tuner on a frequency the site never granted.
        let unit_grant = match &e {
            P25Event::Pdu {
                header,
                valid: true,
                payload,
                ..
            } => parse_ambt_unit_grant(header, payload),
            _ => None,
        };
        // The voice-path hooks below are traffic-role only: on a control
        // channel they are meaningless (no tracker is keyed to the control
        // frequency, and control carries no voice), so a control decode stays
        // exactly what it was before the shared tracker landed.
        let traffic = self.role == ChannelRole::Traffic;
        // `lc`: an LDU1 Group / Unit-to-Unit Voice Channel User names the
        // active call's TO/FROM (spec §18.1). `voice`: an HDU opens a call —
        // if the HDU was missed (or corrupt) the first LDU does.
        // `terminator`: TDU/TDULC end it.
        let (lc, voice, terminator) = if traffic {
            let lc: Option<(u32, u32)> = match &e {
                P25Event::LinkControl { parsed, .. } => match parsed {
                    LinkControl::GroupVoiceChannelUser {
                        talkgroup, source, ..
                    } => Some((u32::from(*talkgroup), *source)),
                    LinkControl::UnitToUnitVoiceChannelUser { target, source, .. } => {
                        Some((*target, *source))
                    }
                    _ => None,
                },
                _ => None,
            };
            let voice = matches!(
                e,
                P25Event::Hdu { valid: true, .. }
                    | P25Event::LinkControl { .. }
                    | P25Event::ImbeFrame { .. }
            );
            let terminator = matches!(e, P25Event::Tdu | P25Event::TduLc { .. });
            (lc, voice, terminator)
        } else {
            (None, false, false)
        };
        let freq = self.channel_frequency_hz;

        // Spec §13 "call start (HDU seen)": freeze control-side refreshes as
        // soon as this decoder is carrying the call.
        if voice && !self.call_active {
            self.call_active = true;
            self.trunking.borrow_mut().traffic_call_started(freq, now);
        }

        sink(e);

        if let Some(m) = tsbk {
            self.trunking.borrow_mut().process(&m, now, sink);
        }
        if let Some(g) = unit_grant {
            self.trunking.borrow_mut().process_unit_grant(&g, now, sink);
        }
        if traffic {
            if let Some((to, from)) = lc {
                // Current-user refresh: extends the call, or reports the
                // talker change / new call the LC implies.
                self.trunking
                    .borrow_mut()
                    .traffic_call_update_with(freq, to, from, now, sink);
            }
            if terminator {
                // Spec §13 "call end (TDU/TDULC)" — the only producer of
                // `CallEnd`.
                self.call_active = false;
                self.trunking.borrow_mut().traffic_call_ended(freq, sink);
            }
        }
    }

    /// Feed baseband samples; deliver events into a bounded drop-oldest
    /// channel (never blocks the DSP thread).
    pub fn process_into(&mut self, samples: &[Complex<f32>], tx: &BoundedSender<P25Event>) {
        self.process(samples, &mut |e| {
            let _ = tx.send(e);
        });
    }

    /// Notify the engine of a tuner frequency change: resets the demod PLL
    /// (spec §6/§14) and re-keys this channel's call tracking.
    ///
    /// Only a [`Control`](ChannelRole::Control) channel retunes the tracker
    /// (spec §13: a control retune is a system change — stop everything, clear
    /// the maps *and the band plan*). A [`Traffic`](ChannelRole::Traffic)
    /// channel retuning to follow the next grant touches nothing but its own
    /// per-channel state: wiping the shared band plan there would leave the
    /// whole site unable to resolve any later grant.
    pub fn frequency_changed(&mut self, new_frequency_hz: u64) {
        self.demod.reset_pll();
        // A retune invalidates every in-flight assembly, the NID capture and
        // the NAC tracker: start the framer clean so the first sync on the
        // new frequency is not charged a bogus SyncLoss or fed stale state.
        self.framer.reset();
        self.channel_frequency_hz = new_frequency_hz;
        self.call_active = false;
        if self.role == ChannelRole::Control {
            self.trunking.borrow_mut().retune(new_frequency_hz);
        }
    }

    /// This channel's millisecond clock, derived from its symbol counter.
    pub fn clock_ms(&self) -> u64 {
        self.now_ms()
    }

    /// Seed this channel's millisecond clock to the site's stream time.
    ///
    /// A decoder built mid-stream starts its symbol counter at zero, so its
    /// clock would run behind every other channel of the site — and a
    /// [`SharedTracker`] compares timestamps produced by *different* channels
    /// (staleness, the §13 hand-off freeze). A site engine that spawns a
    /// traffic decoder on a grant must therefore seed the new decoder with the
    /// site's current time so all channels agree.
    pub fn set_clock_ms(&mut self, ms: u64) {
        self.symbol_count = ms * 4800 / 1000;
    }

    /// Decode statistics (for summaries).
    pub fn stats(&self) -> &FramerStats {
        &self.framer.stats
    }

    /// This channel's role within its site.
    pub fn role(&self) -> ChannelRole {
        self.role
    }

    /// The frequency this channel is tuned to.
    pub fn frequency_hz(&self) -> u64 {
        self.channel_frequency_hz
    }

    /// The shared trunking tracker handle — clone it to build the site's other
    /// channels.
    pub fn tracker(&self) -> &SharedTracker {
        &self.trunking
    }

    /// Borrow the trunking tracker (bands, pool state).
    pub fn trunking(&self) -> std::cell::Ref<'_, TrunkingTracker> {
        self.trunking.borrow()
    }

    /// Mutably borrow the trunking tracker (teardown, IDEN preload for a
    /// standalone tracker per spec §13).
    pub fn trunking_mut(&mut self) -> std::cell::RefMut<'_, TrunkingTracker> {
        self.trunking.borrow_mut()
    }

    /// Decimated (internal) sample rate in Hz.
    pub fn decimated_rate(&self) -> f64 {
        self.decimated_rate
    }
}

/// A P25 Phase 1 channel fed directly from a WIDEBAND capture: a [`Ddc`]
/// (complex mix + decimation) isolates one carrier from a wide span, then a
/// [`P25P1Channel`] decodes it. This is the entry point a recorder tuned to a
/// wide span uses — one instance per carrier of interest.
pub struct P25P1WidebandChannel {
    ddc: Ddc,
    channel: P25P1Channel,
    /// Reused per `process` call — channel-rate samples out of the DDC. Its
    /// capacity settles after the first call, so steady state does not allocate.
    scratch: Vec<Complex<f32>>,
}

impl P25P1WidebandChannel {
    /// Create a **standalone** wideband channel with a private tracker.
    ///
    /// `sample_rate`: the capture rate of the wideband IQ fed to
    /// [`process`](Self::process). `channel_rate`: the target P25 channel rate
    /// (e.g. 25000); it must divide `sample_rate` exactly. `offset_hz`: the
    /// carrier's offset from the capture center (positive = above). `mode`
    /// selects LSM (primary) or C4FM. `channel_frequency_hz` seeds the
    /// trunking tracker's control frequency.
    pub fn new(
        sample_rate: u32,
        channel_rate: u32,
        offset_hz: f64,
        mode: DemodMode,
        channel_frequency_hz: u64,
    ) -> Result<Self, P25Error> {
        let ddc = Self::ddc(sample_rate, channel_rate, offset_hz)?;
        let channel = P25P1Channel::new(ddc.output_rate(), mode, channel_frequency_hz)?;
        Ok(Self::assemble(ddc, channel))
    }

    /// Create the site's **control** channel from a wideband capture, driving
    /// the shared `tracker`.
    pub fn new_control(
        sample_rate: u32,
        channel_rate: u32,
        offset_hz: f64,
        mode: DemodMode,
        control_frequency_hz: u64,
        tracker: SharedTracker,
    ) -> Result<Self, P25Error> {
        let ddc = Self::ddc(sample_rate, channel_rate, offset_hz)?;
        let channel =
            P25P1Channel::new_control(ddc.output_rate(), mode, control_frequency_hz, tracker)?;
        Ok(Self::assemble(ddc, channel))
    }

    /// Create a **traffic** channel from a wideband capture, sharing the
    /// site's `tracker` — one per followed grant, all fed the same wideband
    /// stream at different offsets.
    pub fn new_traffic(
        sample_rate: u32,
        channel_rate: u32,
        offset_hz: f64,
        mode: DemodMode,
        traffic_frequency_hz: u64,
        tracker: SharedTracker,
    ) -> Result<Self, P25Error> {
        let ddc = Self::ddc(sample_rate, channel_rate, offset_hz)?;
        let channel =
            P25P1Channel::new_traffic(ddc.output_rate(), mode, traffic_frequency_hz, tracker)?;
        Ok(Self::assemble(ddc, channel))
    }

    fn ddc(sample_rate: u32, channel_rate: u32, offset_hz: f64) -> Result<Ddc, P25Error> {
        if channel_rate == 0 {
            return Err(P25Error::InvalidChannelRate);
        }
        if !sample_rate.is_multiple_of(channel_rate) {
            return Err(P25Error::RateNotDivisible {
                sample_rate,
                channel_rate,
            });
        }
        Ddc::try_new(sample_rate, channel_rate, offset_hz)
    }

    fn assemble(ddc: Ddc, channel: P25P1Channel) -> Self {
        P25P1WidebandChannel {
            ddc,
            channel,
            scratch: Vec::new(),
        }
    }

    /// Feed wideband IQ samples; deliver events to `sink`.
    pub fn process(&mut self, wideband: &[Complex<f32>], sink: &mut dyn FnMut(P25Event)) {
        let Self {
            ddc,
            channel,
            scratch,
        } = self;
        scratch.clear();
        // Block path: bit-identical to per-sample `push` (proven by the DDC
        // golden hashes and block-vs-stream tests) but computes the half-band
        // and tail dot products in independent output lanes, ~1.8x faster.
        ddc.process_block(wideband, scratch);
        channel.process(scratch, sink);
    }

    /// Feed wideband IQ into a bounded drop-oldest channel (never blocks).
    pub fn process_into(&mut self, wideband: &[Complex<f32>], tx: &BoundedSender<P25Event>) {
        self.process(wideband, &mut |e| {
            let _ = tx.send(e);
        });
    }

    /// Access the inner channel-rate decoder (stats, trunking, retune).
    pub fn channel_mut(&mut self) -> &mut P25P1Channel {
        &mut self.channel
    }

    /// Access the inner channel-rate decoder.
    pub fn channel(&self) -> &P25P1Channel {
        &self.channel
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::CallKind;
    use crate::iden::IdenBand;
    use crate::message::{ChannelRef, ServiceOptions, Tsbk, TsbkMessage};
    use crate::payload::LinkControl;
    use crate::trunking::DEFAULT_POOL_SIZE;

    const CONTROL_HZ: u64 = 851_050_000;
    const BAND: u8 = 1;
    const CHANNEL: u16 = 456;
    /// 851.0 MHz + 12.5 kHz × 456.
    const TRAFFIC_HZ: u64 = 851_000_000 + 12_500 * CHANNEL as u64;
    const TALKGROUP: u16 = 52_101;

    fn iden() -> IdenBand {
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

    fn tsbk(message: TsbkMessage) -> P25Event {
        P25Event::Tsbk {
            tsbk: Tsbk {
                last_block: true,
                encrypted: false,
                opcode: 0,
                vendor: 0,
                message,
                raw: [0u8; 12],
            },
            corrected: 0,
            crc_corrected: 0,
        }
    }

    fn grant(source: u32) -> P25Event {
        tsbk(TsbkMessage::GroupVoiceGrant {
            options: ServiceOptions::default(),
            channel: ChannelRef {
                band: BAND,
                channel: CHANNEL,
            },
            talkgroup: TALKGROUP,
            source,
        })
    }

    fn link_control(talkgroup: u16, source: u32) -> P25Event {
        P25Event::LinkControl {
            word: [0; 9],
            parsed: LinkControl::GroupVoiceChannelUser {
                options: ServiceOptions::default(),
                talkgroup,
                source_ext_flag: false,
                source,
            },
            corrected: 0,
        }
    }

    fn channel(role: ChannelRole, freq: u64, tracker: SharedTracker) -> P25P1Channel {
        match role {
            ChannelRole::Control => {
                P25P1Channel::new_control(50_000, DemodMode::Lsm, freq, tracker)
            }
            ChannelRole::Traffic | ChannelRole::Data => {
                P25P1Channel::new_traffic(50_000, DemodMode::Lsm, freq, tracker)
            }
        }
        .expect("50 kHz channel")
    }

    /// Feed one already-decoded framer event through the dispatch path at
    /// `now_ms`, collecting everything the channel emits. This is the same
    /// code path `process` uses once the DSP has produced a frame; driving it
    /// directly keeps the test about the trunking wiring rather than about
    /// synthesizing IQ.
    fn feed(ch: &mut P25P1Channel, e: P25Event, now_ms: u64) -> Vec<P25Event> {
        let mut out = Vec::new();
        ch.dispatch(e, now_ms, &mut |ev| out.push(ev));
        out
    }

    fn call_ends(events: &[P25Event]) -> Vec<u64> {
        events
            .iter()
            .filter_map(|e| match e {
                P25Event::CallEnd { frequency_hz } => Some(*frequency_hz),
                _ => None,
            })
            .collect()
    }

    /// The HIGH finding, end to end: a control channel and a traffic channel
    /// sharing one tracker produce a full CallStart → refresh → CallContinue →
    /// **CallEnd** lifecycle. With a per-channel tracker (the old design) the
    /// traffic channel's frequency has no tracker entry at all and every step
    /// after (a) is dead.
    #[test]
    fn shared_tracker_drives_full_call_lifecycle() {
        let tracker = TrunkingTracker::new_shared(CONTROL_HZ);
        let mut control = channel(ChannelRole::Control, CONTROL_HZ, tracker.clone());
        let mut traffic = channel(ChannelRole::Traffic, TRAFFIC_HZ, tracker.clone());

        // (a) Grant on the control channel → CallStart + TrafficChannelRequest.
        feed(&mut control, tsbk(TsbkMessage::IdenUpdate(iden())), 0);
        let out = feed(&mut control, grant(0xA1), 10);
        assert!(
            out.iter().any(|e| matches!(
                e,
                P25Event::CallStart { frequency_hz, talkgroup, .. }
                    if *frequency_hz == TRAFFIC_HZ && *talkgroup == TALKGROUP
            )),
            "grant should start the call: {out:?}"
        );
        assert!(
            out.iter().any(|e| matches!(
                e,
                P25Event::TrafficChannelRequest { frequency_hz } if *frequency_hz == TRAFFIC_HZ
            )),
            "grant should request a traffic channel: {out:?}"
        );
        assert_eq!(
            tracker.borrow().pool_available(),
            DEFAULT_POOL_SIZE - 1,
            "one channel taken from the pool"
        );

        // (b) LC on the traffic channel, matching TO → refresh, no new call.
        let before = tracker.borrow().event_end_ms(TRAFFIC_HZ).expect("tracked");
        let out = feed(&mut traffic, link_control(TALKGROUP, 0xA1), 800);
        assert!(
            !out.iter().any(|e| matches!(e, P25Event::CallStart { .. })),
            "same-call LC must not restart the call: {out:?}"
        );
        let after = tracker.borrow().event_end_ms(TRAFFIC_HZ).expect("tracked");
        assert!(after > before, "traffic LC should extend the call event");
        assert_eq!(after, 800);

        // (c) LC with a different non-zero FROM → CallContinue.
        let out = feed(&mut traffic, link_control(TALKGROUP, 0xB2), 1_600);
        assert!(
            out.iter().any(|e| matches!(
                e,
                P25Event::CallContinue { frequency_hz, source, .. }
                    if *frequency_hz == TRAFFIC_HZ && *source == 0xB2
            )),
            "talker change should report a continuation: {out:?}"
        );

        // (d) TDU on the traffic channel → CallEnd for this frequency.
        let out = feed(&mut traffic, P25Event::Tdu, 2_000);
        assert_eq!(
            call_ends(&out),
            vec![TRAFFIC_HZ],
            "TDU must emit CallEnd for the traffic frequency: {out:?}"
        );
        assert!(
            out.iter().any(|e| matches!(e, P25Event::Tdu)),
            "the raw Tdu event is still delivered: {out:?}"
        );
        // A second terminator does not double-report.
        let out = feed(&mut traffic, P25Event::Tdu, 2_100);
        assert!(call_ends(&out).is_empty(), "CallEnd is reported once");

        // Teardown returns the slot to the pool (spec §13 "Teardown").
        tracker.borrow_mut().traffic_stopped(TRAFFIC_HZ);
        assert_eq!(tracker.borrow().pool_available(), DEFAULT_POOL_SIZE);
    }

    /// A traffic channel that has no HDU still opens the call on its first
    /// LDU1 link control (spec §13 "call start"), which is what freezes
    /// control-side refresh extension.
    /// A completed AMBT PDU carrying a unit-to-unit grant drives the tracker
    /// from the control decoder's own dispatch, exactly as a TSBK grant does —
    /// which is the whole mechanism by which private calls get followed.
    #[test]
    fn a_multi_block_unit_grant_reaches_the_tracker_from_dispatch() {
        // The layout of a captured grant (unit IDs synthetic): 9000001 -> 9000007,
        // band 1 channel 456.
        let mut payload = vec![
            0x45, 0x56, 0x40, 0x34, 0x89, 0x54, 0x41, 0x89, 0x54, 0x47, 0x11, 0xc8, 0xff, 0xff,
            0x56, 0x40, 0x34, 0x89, 0x54, 0x47,
        ];
        payload.extend_from_slice(&[0x92, 0x9f, 0x38, 0x80]); // CRC-32
        let header = crate::message::PduHeader {
            confirmed: false,
            outbound: true,
            format: crate::message::PDU_FORMAT_AMBT,
            vendor: 0,
            llid: 9_000_001,
            blocks_to_follow: 2,
            ambt_opcode: Some(crate::message::AMBT_OP_UU_VCH_GRANT),
        };
        let pdu = |valid: bool| P25Event::Pdu {
            header,
            blocks: 2,
            valid,
            corrected: 0,
            payload: payload.clone(),
        };

        let tracker = TrunkingTracker::new_shared(CONTROL_HZ);
        let mut control = channel(ChannelRole::Control, CONTROL_HZ, tracker.clone());
        feed(&mut control, tsbk(TsbkMessage::IdenUpdate(iden())), 0);

        // A PDU whose blocks did not check out names a frequency nobody
        // granted: it must not move a tuner.
        let out = feed(&mut control, pdu(false), 10);
        assert!(!out.iter().any(|e| matches!(e, P25Event::Grant { .. })));

        let out = feed(&mut control, pdu(true), 20);
        assert!(
            out.iter().any(|e| matches!(
                e,
                P25Event::Grant {
                    kind: CallKind::UnitToUnit,
                    frequency_hz,
                    talkgroup: 0,
                    target: 9_000_007,
                    source: 9_000_001,
                    update: false,
                } if *frequency_hz == TRAFFIC_HZ
            )),
            "the AMBT grant resolved and was followed: {out:?}"
        );
        assert!(out
            .iter()
            .any(|e| matches!(e, P25Event::TrafficChannelRequest { frequency_hz } if *frequency_hz == TRAFFIC_HZ)));

        // Turned off, the same PDU is decoded and declined.
        let tracker = TrunkingTracker::new_shared(CONTROL_HZ);
        tracker.borrow_mut().set_follow_unit_calls(false);
        let mut control = channel(ChannelRole::Control, CONTROL_HZ, tracker);
        feed(&mut control, tsbk(TsbkMessage::IdenUpdate(iden())), 0);
        let out = feed(&mut control, pdu(true), 20);
        assert!(
            !out.iter().any(|e| matches!(e, P25Event::Grant { .. })),
            "follow_unit_calls = false: no grant, no tuner"
        );
    }

    #[test]
    fn traffic_channel_starts_call_on_first_voice_frame() {
        let tracker = TrunkingTracker::new_shared(CONTROL_HZ);
        let mut control = channel(ChannelRole::Control, CONTROL_HZ, tracker.clone());
        let mut traffic = channel(ChannelRole::Traffic, TRAFFIC_HZ, tracker.clone());
        feed(&mut control, tsbk(TsbkMessage::IdenUpdate(iden())), 0);
        feed(&mut control, grant(0), 10);
        assert!(!tracker.borrow().started_by_traffic(TRAFFIC_HZ));

        feed(&mut traffic, link_control(TALKGROUP, 0x522_686), 500);
        assert!(
            tracker.borrow().started_by_traffic(TRAFFIC_HZ),
            "first LDU1 LC hands the call over to the traffic decoder"
        );
        // And the TDULC path ends it just like a bare TDU.
        let out = feed(
            &mut traffic,
            P25Event::TduLc {
                valid: true,
                link_control: [0; 9],
                corrected: 0,
            },
            1_000,
        );
        assert_eq!(call_ends(&out), vec![TRAFFIC_HZ]);
    }

    /// (e) A traffic channel retune must not wipe the site's band plan: a
    /// later grant on the same band still resolves.
    #[test]
    fn traffic_retune_keeps_shared_band_plan() {
        let tracker = TrunkingTracker::new_shared(CONTROL_HZ);
        let mut control = channel(ChannelRole::Control, CONTROL_HZ, tracker.clone());
        let mut traffic = channel(ChannelRole::Traffic, TRAFFIC_HZ, tracker.clone());
        feed(&mut control, tsbk(TsbkMessage::IdenUpdate(iden())), 0);
        assert!(tracker.borrow().bands().get(BAND).is_some());

        // Follow a different grant.
        traffic.frequency_changed(852_000_000);
        assert_eq!(traffic.frequency_hz(), 852_000_000);
        assert!(
            tracker.borrow().bands().get(BAND).is_some(),
            "a traffic retune must not clear the shared IDEN bands"
        );

        // The site can still resolve grants on that band.
        let out = feed(&mut control, grant(7), 100);
        assert!(
            out.iter().any(|e| matches!(
                e,
                P25Event::Grant { kind: CallKind::Group, frequency_hz, .. }
                    if *frequency_hz == TRAFFIC_HZ
            )),
            "band plan survived: {out:?}"
        );
        assert!(!out
            .iter()
            .any(|e| matches!(e, P25Event::GrantUnresolvable { .. })));

        // A *control* retune is a system change and does clear it (spec §13).
        control.frequency_changed(853_000_000);
        assert!(tracker.borrow().bands().get(BAND).is_none());
        assert_eq!(tracker.borrow().control_frequency_hz(), 853_000_000);
    }

    /// (f) The standalone constructor still owns a private tracker and behaves
    /// exactly as before: Control role, its own band plan, and voice events
    /// drive no traffic-side hooks.
    #[test]
    fn standalone_channel_keeps_private_tracker() {
        let shared = TrunkingTracker::new_shared(CONTROL_HZ);
        let mut standalone =
            P25P1Channel::new(50_000, DemodMode::Lsm, CONTROL_HZ).expect("standalone");
        assert_eq!(standalone.role(), ChannelRole::Control);

        feed(&mut standalone, tsbk(TsbkMessage::IdenUpdate(iden())), 0);
        assert!(standalone.trunking().bands().get(BAND).is_some());
        assert!(
            shared.borrow().bands().get(BAND).is_none(),
            "a standalone channel must not touch any other tracker"
        );

        // Voice events on a Control-role channel emit nothing extra — the
        // stream is byte-identical to the pre-change engine.
        feed(&mut standalone, grant(1), 10);
        let out = feed(&mut standalone, link_control(TALKGROUP, 0xA1), 20);
        assert_eq!(out.len(), 1, "only the raw LC event: {out:?}");
        let out = feed(&mut standalone, P25Event::Tdu, 30);
        assert_eq!(out.len(), 1, "only the raw TDU event: {out:?}");
        assert!(call_ends(&out).is_empty());
    }

    #[test]
    fn constructors_report_typed_errors() {
        assert!(matches!(
            P25P1Channel::new(4_800, DemodMode::Lsm, CONTROL_HZ),
            Err(P25Error::SampleRateTooLow {
                rate: 4_800,
                min: MIN_SAMPLE_RATE
            })
        ));
        assert!(matches!(
            P25P1WidebandChannel::new(2_400_000, 0, 0.0, DemodMode::Lsm, CONTROL_HZ),
            Err(P25Error::InvalidChannelRate)
        ));
        assert!(matches!(
            P25P1WidebandChannel::new(2_400_000, 33_333, 0.0, DemodMode::Lsm, CONTROL_HZ),
            Err(P25Error::RateNotDivisible {
                sample_rate: 2_400_000,
                channel_rate: 33_333
            })
        ));
        // The Display impl is what the example prints.
        assert_eq!(
            P25Error::InvalidChannelRate.to_string(),
            "invalid channel rate (must be > 0)"
        );
    }
}
