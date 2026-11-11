// SPDX-License-Identifier: GPL-3.0-or-later
//! Typed events emitted by the decoder engine.

use crate::message::{PduHeader, Tsbk, TsbkMessage};
use crate::payload::{EncryptionSync, HduHeader, LinkControl};

/// Which LDU a voice frame came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LduKind {
    /// LDU1 (carries link control).
    Ldu1,
    /// LDU2 (carries encryption sync).
    Ldu2,
}

/// The kind of call a trunking event refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallKind {
    /// Group voice call.
    Group,
    /// Unit-to-unit call.
    UnitToUnit,
    /// Telephone interconnect.
    Telephone,
}

/// Which site/network broadcast a [`P25Event::SiteBroadcast`] carries
/// (spec §18.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SiteBroadcastKind {
    /// Opcode 57 — secondary / backup control channels of this site.
    SecondaryControl,
    /// Opcode 58 — this site's identity and live control channel.
    RfssStatus,
    /// Opcode 59 — WACN / system identity and live control channel.
    NetworkStatus,
    /// Opcode 60 — a neighbour site and its control channel.
    AdjacentStatus,
}

/// Events produced by the P25 Phase 1 decode engine.
#[derive(Debug, Clone)]
pub enum P25Event {
    /// Frame sync detected by the framer's soft correlator (rising edge).
    SyncDetected {
        /// Correlator score at detection.
        score: f32,
    },
    /// Bits lost between frames (positive) or overlapped/dropped (negative).
    SyncLoss {
        /// Signed bit count per spec §7.3 accounting.
        bits: i64,
    },
    /// A NID passed BCH; a frame is being assembled.
    NidDecoded {
        /// 12-bit NAC.
        nac: u16,
        /// Raw 4-bit DUID value.
        duid: u8,
        /// BCH-corrected bit count.
        corrected: u32,
    },
    /// A CRC-valid TSBK.
    Tsbk {
        /// Parsed message.
        tsbk: Tsbk,
        /// Corrected bits (trellis path error + CRC correction).
        corrected: u32,
        /// CRC-16 corrections alone (0 = clean CRC pass; 1 = single-bit
        /// corrected). Isolated from the trellis path error so a false-accept
        /// audit can distinguish clean-CRC frames from corrected ones.
        crc_corrected: u32,
    },
    /// A TSBK block that failed CRC.
    TsbkFailed,
    /// A CRC-valid PDU header (blocks may follow).
    PduHeader {
        /// Parsed header.
        header: PduHeader,
        /// Corrected bits.
        corrected: u32,
    },
    /// A completed (multi-block) PDU.
    Pdu {
        /// Header.
        header: PduHeader,
        /// Number of data blocks decoded.
        blocks: u8,
        /// True if all block-level checks passed.
        valid: bool,
        /// Corrected bits across the PDU.
        corrected: u32,
        /// The reassembled user-data octets: everything after the 96-bit PDU
        /// header (the block payloads, still carrying any SNDCP/LLC framing and,
        /// for unconfirmed multi-block PDUs, the trailing CRC-32). Empty for a
        /// header-only PDU. This is the raw material for SNDCP/LRRP inspection
        /// downstream ([`crate::lrrp`]).
        payload: Vec<u8>,
    },
    /// HDU decoded.
    Hdu {
        /// True when Golay18 + RS(63,47) succeeded.
        valid: bool,
        /// 120-bit header data, MSB-first in 15 bytes (valid only if `valid`).
        data: [u8; 15],
        /// Parsed header fields (spec §18.3); `None` when `!valid`.
        header: Option<HduHeader>,
        /// Corrected bits.
        corrected: u32,
    },
    /// TDU (terminates a call).
    Tdu,
    /// TDULC decoded (terminates a call, with link control).
    TduLc {
        /// True when Golay24 walk + RS(24,12) succeeded.
        valid: bool,
        /// 72-bit link control word, MSB-first in 9 bytes (valid only if
        /// `valid`).
        link_control: [u8; 9],
        /// Corrected bits.
        corrected: u32,
    },
    /// A raw 144-bit IMBE voice frame (vocoder boundary, spec §12.1).
    ImbeFrame {
        /// Source LDU.
        ldu: LduKind,
        /// Frame index within the LDU (0..=8).
        index: u8,
        /// The 144 bits, raw and untouched, MSB-first.
        frame: [u8; 18],
    },
    /// LDU1 embedded link control (RS-valid).
    LinkControl {
        /// 72-bit link control word, MSB-first in 9 bytes.
        word: [u8; 9],
        /// Parsed link control (spec §18.1) — carries talkgroup/source for the
        /// group-voice case.
        parsed: LinkControl,
        /// Corrected bits (Hamming10 + RS).
        corrected: u32,
    },
    /// LDU2 encryption sync (RS-valid).
    EncryptionSync {
        /// 96-bit encryption sync word, MSB-first in 12 bytes.
        word: [u8; 12],
        /// Parsed encryption sync (spec §18.2).
        parsed: EncryptionSync,
        /// Corrected bits.
        corrected: u32,
    },
    /// LDU low-speed data: two octets, each FEC-decoded with the (16,8,5) LSD
    /// code (spec §17).
    LowSpeedData {
        /// The two FEC-corrected data bytes (as received when `!valid`).
        data: [u8; 2],
        /// Bit errors *corrected* per octet by the (16,8,5) LSD code (0 when
        /// the octet is `!valid` — an uncorrectable octet corrects nothing).
        errors: [u32; 2],
        /// Whether each octet was within the code's 2-bit correction capability.
        valid: [bool; 2],
    },
    /// Trunking: a voice-channel grant resolved to a frequency.
    Grant {
        /// Call kind.
        kind: CallKind,
        /// Downlink frequency in Hz (never 0 — unresolvable grants are
        /// reported via `GrantUnresolvable`).
        frequency_hz: u64,
        /// Talkgroup for a group call; **0** for unit-to-unit / telephone
        /// (whose target does not fit 16 bits — see `target`).
        talkgroup: u16,
        /// Full 24-bit TO address: the target radio for unit-to-unit, the
        /// telephone address for an interconnect update, and (for convenience)
        /// the talkgroup for a group call. 0 when the message carries no TO.
        target: u32,
        /// Source radio (0 if the message carries none).
        source: u32,
        /// True if this came from a grant-update message.
        update: bool,
    },
    /// Trunking: a grant referenced a band with no stored IDEN (spec: the
    /// grant is silently unusable; surfaced here for observability).
    GrantUnresolvable {
        /// Band identifier with no stored IDEN.
        band: u8,
    },
    /// Trunking: new call event created (traffic channel requested unless
    /// the pool was exhausted).
    CallStart {
        /// Downlink frequency.
        frequency_hz: u64,
        /// Talkgroup for a group call; 0 for unit-to-unit / telephone.
        talkgroup: u16,
        /// Full 24-bit TO address (target radio, telephone address, or the
        /// talkgroup for a group call). 0 when the call carries no TO.
        target: u32,
        /// Source radio.
        source: u32,
    },
    /// Trunking: same call continues with a different talker.
    CallContinue {
        /// Downlink frequency.
        frequency_hz: u64,
        /// Talkgroup for a group call; 0 for unit-to-unit / telephone.
        talkgroup: u16,
        /// Full 24-bit TO address (target radio, telephone address, or the
        /// talkgroup for a group call). 0 when the call carries no TO.
        target: u32,
        /// New source radio.
        source: u32,
    },
    /// A unit-to-unit (private) call the engine is following opened or closed.
    ///
    /// Not part of the trunking state machine: it is the *record* of a private
    /// call, emitted by [`SiteEngine`](crate::SiteEngine) on the control
    /// pass-through so the recorder can log it and put it on the radio
    /// timeline. Group calls have no equivalent — they are already on Keyup's
    /// timeline as calls; a private call may end with no audio worth uploading
    /// and would otherwise leave no trace at all.
    PrivateCall {
        /// True for the call's start record, false for its end record.
        start: bool,
        /// The granted channel's downlink frequency.
        frequency_hz: u64,
        /// The talking unit (A).
        source: u32,
        /// The unit being called (B).
        target: u32,
        /// Call length in milliseconds; 0 on the start record.
        ///
        /// A length rather than a start instant on purpose: the engine's clock
        /// is stream time, and every consumer already holds the event's own
        /// wall clock — so the start of the call is `at − duration_ms`, in the
        /// consumer's units, with no second clock to keep straight.
        duration_ms: u64,
    },
    /// The recorder re-anchored this device's stream clock.
    ///
    /// Not a decode event: like [`P25Event::PrivateCall`] it is a *record* the
    /// recorder puts on the control pass-through so it lands in the raw event
    /// log, where an operator reading a day of timestamps can see the seam. A
    /// sample-count clock cannot notice samples that never arrived, so a host
    /// stall leaves every timestamp early until something re-anchors it; this
    /// says when that happened and by how much.
    ClockResync {
        /// Milliseconds the epoch moved. Positive when the stream clock had
        /// fallen behind wall time — i.e. every timestamp before this record
        /// was that many milliseconds early.
        correction_ms: i64,
    },
    /// Trunking: call ended (traffic-side TDU/TDULC or teardown).
    CallEnd {
        /// Downlink frequency.
        frequency_hz: u64,
    },
    /// Trunking: traffic-channel start requested for a granted call.
    TrafficChannelRequest {
        /// Downlink frequency to tune.
        frequency_hz: u64,
    },
    /// Trunking: an SNDCP data-channel grant (opcode 20) resolved to a
    /// frequency. The data follower ([`SiteEngine`](crate::SiteEngine) with a
    /// [`Data`](crate::channel::ChannelRole::Data) channel) subscribes this
    /// frequency to reassemble the subscriber's SNDCP PDUs; voice following
    /// ignores it. Emitted only when the grant's downlink band is known.
    DataGrant {
        /// Downlink data-channel frequency in Hz.
        frequency_hz: u64,
    },
    /// Trunking: the traffic channel pool (default 20) is exhausted.
    ///
    /// Emitted **once per call**: repeated control-channel refreshes of the
    /// same call do not re-report it (a new call on the frequency does).
    PoolExhausted {
        /// Frequency of the call that could not be started.
        frequency_hz: u64,
    },
    /// Trunking: a voice grant on a **TDMA** (Phase 2) channel, which this
    /// decoder cannot follow — its band's identifier-update (opcode 51) names
    /// a TDMA channel type. No call is tracked and no traffic channel is
    /// requested, so no pool slot is spent on a call that could only ever be
    /// dropped as "no voice".
    ///
    /// Emitted **once per call**, keyed by carrier and timeslot: the control
    /// channel's refreshes of the same call do not re-report it; a different
    /// TO on that slot, or the same one after the staleness horizon, does.
    /// The [`P25Event::Grant`] for it is still raised, so observers keep
    /// seeing the site's traffic.
    TdmaGrantRefused {
        /// Group, unit-to-unit or telephone.
        kind: CallKind,
        /// Downlink frequency of the TDMA carrier.
        frequency_hz: u64,
        /// TDMA timeslot (1 or 2) the channel number selects.
        timeslot: u8,
        /// Talkgroup for a group call; 0 for unit-to-unit / telephone.
        talkgroup: u16,
        /// Full 24-bit TO address; 0 when the grant carries none.
        target: u32,
        /// Source radio; 0 when the grant carries none.
        source: u32,
    },
    /// Trunking: a site / network broadcast (spec §18.7) — the site's own
    /// identity and control channel, the secondary control channels it
    /// announces, or a neighbour site.
    ///
    /// The parsed message carries every raw field (including the
    /// [`ChannelRef`](crate::message::ChannelRef)s); `frequency_hz` /
    /// `frequency_b_hz` are those channels resolved through the site's band
    /// plan, and are `None` when the band's identifier-update has not been
    /// received yet. A consumer that must not lose such a broadcast (the
    /// [`SiteInventory`](crate::SiteInventory) does exactly this) keeps the
    /// raw channel and re-resolves it when the IDEN arrives.
    SiteBroadcast {
        /// Which broadcast this is.
        kind: SiteBroadcastKind,
        /// The parsed TSBK payload, with all raw fields.
        message: TsbkMessage,
        /// Resolved downlink Hz of the announced channel (channel **A** for a
        /// secondary-control-channel broadcast); `None` when the band is
        /// unknown.
        frequency_hz: Option<u64>,
        /// Resolved downlink Hz of channel **B** of a secondary-control-channel
        /// broadcast; always `None` for the other kinds.
        frequency_b_hz: Option<u64>,
    },
    /// An identifier update decoded (also carried inside `Tsbk`, with the TDMA
    /// fields this omits). Raised for every decode, whether or not the tracker
    /// applied it to the band plan grants resolve through: a band that is not
    /// plausible is ignored, and a change to a known band waits for a second
    /// identical decode.
    IdenUpdate {
        /// Band identifier.
        id: u8,
        /// Base frequency Hz.
        base_hz: u64,
        /// Channel spacing Hz.
        spacing_hz: u32,
        /// Signed transmit offset Hz.
        offset_hz: i64,
        /// Bandwidth Hz.
        bandwidth_hz: u32,
    },
    /// Frequency-error feedback (auto-PPM), spec §14.
    FrequencyError {
        /// Correction in Hz derived from the demod PLL at sync validation.
        hz: f64,
    },
}
