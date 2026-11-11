// SPDX-License-Identifier: GPL-3.0-or-later
//! TSBK and PDU-header message layouts (spec §10, §7.6).

use serde::{Deserialize, Serialize};

use crate::bits::BitBuf;
use crate::iden::{tdma_channel_type, IdenBand};
use crate::payload::{is_standard_vendor, ExtendedFunction, MFID_HARRIS};

/// Decomposed service-options byte (standard P25).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ServiceOptions {
    /// Emergency flag (bit 0 of the byte, MSB).
    pub emergency: bool,
    /// Encrypted flag.
    pub encrypted: bool,
    /// Duplex flag.
    pub duplex: bool,
    /// Mode flag (packet vs circuit).
    pub packet_mode: bool,
    /// Priority (low 3 bits).
    pub priority: u8,
}

impl ServiceOptions {
    /// Decode from the 8-bit field.
    pub fn from_byte(b: u8) -> Self {
        ServiceOptions {
            emergency: b & 0x80 != 0,
            encrypted: b & 0x40 != 0,
            duplex: b & 0x20 != 0,
            packet_mode: b & 0x10 != 0,
            priority: b & 0x07,
        }
    }
}

/// Decomposed **data** service-options byte (sdrtrunk `DataServiceOptions`).
///
/// The same byte as [`ServiceOptions`] but its low nibble is the NSAPI, not a
/// priority — the SNDCP messages (opcodes 20/21/22) carry this form.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DataServiceOptions {
    /// The undecoded byte, so nothing is lost.
    pub raw: u8,
    /// Emergency flag (`0x80`).
    pub emergency: bool,
    /// Encrypted flag (`0x40`).
    pub encrypted: bool,
    /// Duplex: true = full, false = half (`0x20`).
    pub full_duplex: bool,
    /// Session mode: true = packet, false = circuit (`0x10`).
    pub packet_mode: bool,
    /// Network service access point identifier (`0x0F`).
    pub nsapi: u8,
}

impl DataServiceOptions {
    /// Decode from the 8-bit field.
    pub fn from_byte(b: u8) -> Self {
        DataServiceOptions {
            raw: b,
            emergency: b & 0x80 != 0,
            encrypted: b & 0x40 != 0,
            full_duplex: b & 0x20 != 0,
            packet_mode: b & 0x10 != 0,
            nsapi: b & 0x0F,
        }
    }
}

/// Name of a 2-bit registration/affiliation response value (sdrtrunk
/// `Response`): 0 accepted, 1 failed, 2 denied, 3 refused.
pub fn response_name(v: u8) -> &'static str {
    match v {
        0 => "accepted",
        1 => "failed",
        2 => "denied",
        3 => "refused",
        _ => "unknown",
    }
}

/// A (band, channel) pair as carried in grant messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChannelRef {
    /// 4-bit frequency band identifier.
    pub band: u8,
    /// 12-bit channel number.
    pub channel: u16,
}

/// Decoded **system service class** byte, as carried by the site/network
/// broadcasts (opcodes 57-60), spec §18.7.
///
/// Bit values follow the reference decoder's `SystemServiceClass`; the raw
/// byte is retained so a consumer can see reserved bits this type does not
/// name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ServiceClass {
    /// The undecoded byte, so reserved/unknown bits are never lost.
    pub raw: u8,
    /// `0x01` — composite control channel (carries traffic as well as control).
    pub composite: bool,
    /// `0x02` — no service requests accepted (the "update"/announcement-only
    /// control channel bit).
    pub no_service_requests: bool,
    /// `0x04` — backup control channel.
    pub backup_control: bool,
    /// `0x10` — data service.
    pub data: bool,
    /// `0x20` — voice service.
    pub voice: bool,
    /// `0x40` — registration service.
    pub registration: bool,
    /// `0x80` — authentication service.
    pub authentication: bool,
}

impl ServiceClass {
    /// Decode from the 8-bit field.
    pub fn from_byte(b: u8) -> Self {
        ServiceClass {
            raw: b,
            composite: b & 0x01 != 0,
            no_service_requests: b & 0x02 != 0,
            backup_control: b & 0x04 != 0,
            data: b & 0x10 != 0,
            voice: b & 0x20 != 0,
            registration: b & 0x40 != 0,
            authentication: b & 0x80 != 0,
        }
    }
}

/// The 24-bit **service bitmap** carried twice by the System Service Broadcast
/// (opcode 56), spec §18.7 — once as *available* services and once as
/// *supported* services.
///
/// This is a different field from [`ServiceClass`]: that is the 8-bit service
/// *class* byte of the site/network broadcasts (opcodes 57–60), while this is
/// the 24-bit service bitmap the reference decoder calls `Service` (sdrtrunk
/// `p25/reference/Service.java`, used by
/// `tsbk/standard/osp/SystemServiceBroadcast.java`). The raw value is kept so
/// nothing is lost, and the named bits are read through [`SystemServices::has`]
/// / [`SystemServices::names`] rather than 24 booleans.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SystemServices {
    /// The undecoded 24-bit bitmap.
    pub raw: u32,
}

impl SystemServices {
    /// Every named bit of the bitmap, most-significant first, exactly as the
    /// reference decoder's `Service` enum orders them. The two `reserved_*`
    /// entries are named so a set reserved bit is reported rather than lost.
    pub const FLAGS: [(u32, &'static str); 24] = [
        (0x80_0000, "extended_services"),
        (0x40_0000, "extended_services_extension"),
        (0x20_0000, "network_active"),
        (0x10_0000, "reserved_4"),
        (0x08_0000, "group_voice"),
        (0x04_0000, "individual_voice"),
        (0x02_0000, "pstn_to_unit_voice"),
        (0x01_0000, "unit_to_pstn_voice"),
        (0x00_8000, "reserved_9"),
        (0x00_4000, "group_data"),
        (0x00_2000, "individual_data"),
        (0x00_1000, "reserved_12"),
        (0x00_0800, "unit_registration"),
        (0x00_0400, "group_affiliation"),
        (0x00_0200, "group_affiliation_query"),
        (0x00_0100, "authentication"),
        (0x00_0080, "encryption_settings"),
        (0x00_0040, "user_status"),
        (0x00_0020, "user_message"),
        (0x00_0010, "unit_status"),
        (0x00_0008, "user_status_query"),
        (0x00_0004, "unit_status_query"),
        (0x00_0002, "call_alert"),
        (0x00_0001, "emergency_alarm"),
    ];

    /// Decode from the 24-bit field (higher bits are masked away).
    pub fn from_bits(v: u32) -> Self {
        SystemServices {
            raw: v & 0x00FF_FFFF,
        }
    }

    /// Whether one [`SystemServices::FLAGS`] mask is set.
    pub fn has(&self, mask: u32) -> bool {
        self.raw & mask == mask
    }

    /// The set bits by name, most-significant first.
    pub fn names(&self) -> Vec<&'static str> {
        Self::FLAGS
            .iter()
            .filter(|(mask, _)| self.has(*mask))
            .map(|(_, name)| *name)
            .collect()
    }
}

/// Site condition flags of an adjacent-status broadcast (opcode 60), spec
/// §18.7.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct AdjacentSiteFlags {
    /// Bit 24 — the announced channel is a conventional repeater channel.
    pub conventional: bool,
    /// Bit 25 — the neighbour site is in a failure condition.
    pub site_failure: bool,
    /// Bit 26 — the announced site information is valid.
    pub valid_information: bool,
    /// Bit 27 — the neighbour has an active network connection to its RFSS
    /// controller.
    pub active_rfss_connection: bool,
}

/// Parsed standard-vendor outbound TSBK payloads needed for voice following.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TsbkMessage {
    /// Opcode 0 — group voice channel grant.
    GroupVoiceGrant {
        /// Service options.
        options: ServiceOptions,
        /// Voice channel.
        channel: ChannelRef,
        /// Talkgroup (group address).
        talkgroup: u16,
        /// Source radio address.
        source: u32,
    },
    /// Opcode 2 — group voice channel grant update (one or two pairs).
    GroupVoiceGrantUpdate {
        /// Pair A.
        a: (ChannelRef, u16),
        /// Pair B — only meaningful if group B ≠ group A and ≠ 0.
        b: Option<(ChannelRef, u16)>,
    },
    /// Opcode 3 — group voice channel grant update, explicit.
    GroupVoiceGrantUpdateExplicit {
        /// Service options.
        options: ServiceOptions,
        /// Downlink channel.
        downlink: ChannelRef,
        /// Uplink channel (channel 0xFFF = no uplink).
        uplink: ChannelRef,
        /// Talkgroup.
        talkgroup: u16,
    },
    /// Opcodes 4 / 6 — unit-to-unit voice channel grant / update.
    UnitToUnitGrant {
        /// True for opcode 6 (update).
        update: bool,
        /// Voice channel.
        channel: ChannelRef,
        /// Target radio address.
        target: u32,
        /// Source radio address.
        source: u32,
    },
    /// Opcodes 8 / 9 — telephone interconnect grant / update.
    ///
    /// The 24-bit address at bits 56–79 is a **source radio** for opcode 8 but
    /// a **talkgroup** for opcode 9 (spec §18.4) — the wire bits are identical,
    /// the address *type* is not.
    TelephoneInterconnectGrant {
        /// True for opcode 9 (update).
        update: bool,
        /// Service options.
        options: ServiceOptions,
        /// Voice channel.
        channel: ChannelRef,
        /// Call timer (×100 ms).
        call_timer: u16,
        /// Address field (bits 56–79): source radio (opcode 8) or talkgroup
        /// (opcode 9), per `address_is_talkgroup`.
        address: u32,
        /// True when `address` is a talkgroup (opcode 9 update).
        address_is_talkgroup: bool,
    },
    /// Opcode 57 (0x39) — Secondary Control Channel Broadcast: the backup /
    /// secondary control channels this site announces (spec §18.7).
    SecondaryControlChannel {
        /// RF sub-system id.
        rfss: u8,
        /// Site id.
        site: u8,
        /// First announced channel.
        channel_a: ChannelRef,
        /// Service class of channel A.
        service_class_a: ServiceClass,
        /// Second announced channel; `None` when band **and** channel number
        /// are zero ("no second channel").
        channel_b: Option<ChannelRef>,
        /// Service class of channel B (meaningless when `channel_b` is `None`).
        service_class_b: ServiceClass,
    },
    /// Opcode 58 (0x3A) — RFSS Status Broadcast: this site's identity and its
    /// *live* control channel (spec §18.7).
    RfssStatus {
        /// Location registration area.
        lra: u8,
        /// Bit 27 — active network connection to the RFSS controller.
        active_network_connection: bool,
        /// 12-bit system id.
        system_id: u16,
        /// RF sub-system id.
        rfss: u8,
        /// Site id.
        site: u8,
        /// The site's current control channel.
        channel: ChannelRef,
        /// Service class of the control channel.
        service_class: ServiceClass,
    },
    /// Opcode 59 (0x3B) — Network Status Broadcast: WACN / system identity and
    /// the live control channel (spec §18.7).
    NetworkStatus {
        /// Location registration area.
        lra: u8,
        /// 20-bit wide-area communications network id.
        wacn: u32,
        /// 12-bit system id.
        system_id: u16,
        /// The site's current control channel.
        channel: ChannelRef,
        /// Service class of the control channel.
        service_class: ServiceClass,
    },
    /// Opcode 60 (0x3C) — Adjacent Status Broadcast: a neighbour site and its
    /// control channel (spec §18.7).
    AdjacentStatus {
        /// The neighbour's location registration area.
        lra: u8,
        /// Site condition flags.
        flags: AdjacentSiteFlags,
        /// The neighbour's 12-bit system id.
        system_id: u16,
        /// The neighbour's RF sub-system id.
        rfss: u8,
        /// The neighbour's site id.
        site: u8,
        /// The neighbour's control channel.
        channel: ChannelRef,
        /// Service class advertised by the neighbour.
        service_class: ServiceClass,
    },
    /// Opcode 20 — SNDCP Data Channel Grant (sdrtrunk
    /// `SNDCPDataChannelGrant`).
    SndcpDataChannelGrant {
        /// Data service options (bits 16–23).
        options: DataServiceOptions,
        /// Downlink data channel (bits 24–39).
        downlink: ChannelRef,
        /// Uplink data channel (bits 40–55); channel 4095 = not explicit.
        uplink: ChannelRef,
        /// Target radio address (bits 56–79).
        target: u32,
    },
    /// Opcode 21 — SNDCP Data Page Request (sdrtrunk
    /// `SNDCPDataPageRequest`).
    ///
    /// The message names one address, the subscriber being paged, and it sits
    /// in bits 56–79 — the same place opcode 20 keeps its target. Bits 32–55
    /// are reserved, and on the measured system they hold `0x00ffff` on every
    /// one of 2,279 page requests: the low half is the `0xFFFF` "no explicit
    /// uplink" sentinel opcode 20 puts at bits 40–55. This decoder used to read
    /// bits 32–55 as the target and 56–79 as a source, which made every page
    /// request a page of radio 65535 from a plausible radio id
    /// (private/docs/analysis/unknown-inventory-2026-09-03.md §5).
    SndcpDataPageRequest {
        /// Data service options (bits 16–23).
        options: DataServiceOptions,
        /// Target radio address (bits 56–79) — the subscriber being paged.
        target: u32,
        /// Reserved (bits 32–55); `0x00ffff` on every observed page request.
        reserved: u32,
    },
    /// Opcode 22 — SNDCP Data Channel Announcement Explicit (sdrtrunk
    /// `SNDCPDataChannelAnnouncementExplicit`).
    ///
    /// On the measured system this is a static advertisement repeated ~1.6/s;
    /// it is parsed so the *change* of one is visible, not for its content.
    SndcpDataChannelAnnounceExplicit {
        /// Data service options (bits 16–23).
        options: DataServiceOptions,
        /// Autonomous access flag (bit 24).
        autonomous_access: bool,
        /// Requested access flag (bit 25).
        requested_access: bool,
        /// Downlink data channel (bits 32–47).
        downlink: ChannelRef,
        /// Uplink data channel (bits 48–63).
        uplink: ChannelRef,
        /// Data access control (bits 64–79).
        data_access_control: u16,
    },
    /// Opcode 32 — Acknowledge Response FNE (sdrtrunk `AcknowledgeResponse`).
    ///
    /// Bits 24–55 are a union selected by the two flags: with both set they
    /// are a WACN + system id, with only `additional_information` set they are
    /// a 24-bit source address (bits 32–55), and with neither set they carry
    /// nothing.
    AcknowledgeResponse {
        /// Additional-information flag (bit 16).
        additional_information: bool,
        /// Extended-information flag (bit 17).
        extended_information: bool,
        /// The **inbound** service opcode being acknowledged (bits 18–23).
        service_type: u8,
        /// WACN (bits 24–43), present only when both flags are set.
        wacn: Option<u32>,
        /// System id (bits 44–55), present only when both flags are set.
        system_id: Option<u16>,
        /// Source radio (bits 32–55), present only when
        /// `additional_information && !extended_information`.
        source: Option<u32>,
        /// Target radio address (bits 56–79).
        target: u32,
    },
    /// Opcode 33 — Queued Response (sdrtrunk `QueuedResponse`).
    QueuedResponse {
        /// Additional-information flag (bit 16).
        additional_information: bool,
        /// The **outbound** service opcode being queued (bits 18–23).
        service_type: u8,
        /// Queued-response reason code (bits 24–31).
        reason: u8,
        /// Additional info (bits 32–55), meaningful per `service_type`.
        additional_info: u32,
        /// Target radio address (bits 56–79).
        target: u32,
    },
    /// Opcode 39 — Deny Response (sdrtrunk `DenyResponse`).
    DenyResponse {
        /// Additional-information flag (bit 16).
        additional_information: bool,
        /// The **inbound** service opcode being denied (bits 18–23).
        service_type: u8,
        /// Deny reason code (bits 24–31).
        reason: u8,
        /// Additional info (bits 32–55), meaningful per `service_type`.
        additional_info: u32,
        /// Target radio address (bits 56–79).
        target: u32,
    },
    /// Opcode 40 — Group Affiliation Response (sdrtrunk
    /// `GroupAffiliationResponse`).
    GroupAffiliationResponse {
        /// Bit 16: true = global affiliation, false = local.
        global_affiliation: bool,
        /// Affiliation response value (bits 22–23), see [`response_name`].
        response: u8,
        /// Announcement group address (bits 24–39).
        announcement_group: u16,
        /// Group address the radio affiliated to (bits 40–55).
        group: u16,
        /// The radio being answered (bits 56–79).
        target: u32,
    },
    /// Opcode 36 — Extended Function Command (sdrtrunk
    /// `ExtendedFunctionCommand`), the control-channel twin of Link Control
    /// opcode 23.
    ///
    /// On the measured system every one of the 752 in the 2026-09-03 window is
    /// function `0x007d` (radio detach) with arguments `0xfffffd` — the system
    /// controller as the requesting entity — to one of 243 radios.
    ExtendedFunctionCommand {
        /// What the radio is being told to do (bits 16–31).
        function: ExtendedFunction,
        /// Function arguments (bits 32–55); the requesting entity, in practice.
        arguments: u32,
        /// The radio being commanded (bits 56–79).
        target: u32,
    },
    /// Opcode 42 — Group Affiliation Query (sdrtrunk
    /// `GroupAffiliationQuery`).
    GroupAffiliationQuery {
        /// The radio being queried (bits 32–55).
        target: u32,
        /// The FNE / radio asking (bits 56–79).
        source: u32,
    },
    /// Opcode 43 — Location Registration Response (sdrtrunk
    /// `LocationRegistrationResponse`).
    LocationRegistrationResponse {
        /// Registration response value (bits 22–23), see [`response_name`].
        response: u8,
        /// Group address (bits 24–39).
        group: u16,
        /// RF sub-system id (bits 40–47).
        rfss: u8,
        /// Site id (bits 48–55).
        site: u8,
        /// The radio being answered (bits 56–79).
        target: u32,
    },
    /// Opcode 44 — Unit Registration Response (sdrtrunk
    /// `UnitRegistrationResponse`).
    ///
    /// `source_id` is the radio's own id; `source_address` is its address on
    /// this system, which differs from `source_id` only for a roaming radio.
    UnitRegistrationResponse {
        /// Registration response value (bits 18–19), see [`response_name`].
        response: u8,
        /// The radio's home system id (bits 20–31).
        system_id: u16,
        /// The radio's id (bits 32–55).
        source_id: u32,
        /// The radio's address on this system (bits 56–79).
        source_address: u32,
    },
    /// Opcode 45 — Unit Registration Command (sdrtrunk
    /// `UnitRegistrationCommand`): the FNE ordering a radio to register.
    UnitRegistrationCommand {
        /// The radio commanded to register (bits 32–55).
        target: u32,
        /// The FNE / radio issuing the command (bits 56–79).
        source: u32,
    },
    /// Opcode 47 — Unit De-Registration Acknowledge (sdrtrunk
    /// `UnitDeRegistrationAcknowledge`): the fully qualified SUID of a radio
    /// leaving the system.
    UnitDeRegistrationAck {
        /// 20-bit WACN (bits 24–43).
        wacn: u32,
        /// 12-bit system id (bits 44–55).
        system_id: u16,
        /// The de-registering radio (bits 56–79).
        source: u32,
    },
    /// Opcodes 51 / 52 / 61 — identifier updates.
    IdenUpdate(IdenBand),
    /// L3Harris (MFID 164) opcode 48 (0x30) — Group Regroup Command with
    /// Explicit Encryption (patch/supergroup), spec §18.5.
    HarrisGroupRegroup {
        /// Regroup options (bits 16–23).
        regroup_options: u8,
        /// Supergroup address (bits 24–39).
        supergroup: u16,
        /// Encryption key id (bits 40–55).
        key_id: u16,
        /// Target: 24-bit radio (bits 56–79) or 16-bit talkgroup (bits 64–79),
        /// selected by `target_is_talkgroup`.
        target: u32,
        /// True when `target` is a talkgroup.
        target_is_talkgroup: bool,
    },
    /// Opcode 56 — System Service Broadcast (sdrtrunk
    /// `SystemServiceBroadcast`), spec §18.7.
    ///
    /// The site's advertisement of what it can do. On the measured system it
    /// is the single most common TSBK (2.05 M in 7 days) and its payload is
    /// constant: available == supported == `0x0C2C68`, priority 1.
    SystemServiceBroadcast {
        /// Services the site will accept requests for right now (bits 24–47).
        available: SystemServices,
        /// Services the site is built to offer (bits 48–71).
        supported: SystemServices,
        /// Request priority level (bits 72–79).
        priority: u8,
    },
    /// Any other L3Harris (MFID 164) OSP opcode — named generically, not
    /// parsed by value (spec §18.5).
    HarrisOsp {
        /// 6-bit opcode.
        opcode: u8,
    },
    /// Any other (opcode, vendor) combination.
    Other {
        /// 6-bit opcode.
        opcode: u8,
        /// Vendor ID.
        vendor: u8,
    },
}

/// A decoded TSBK: common header plus parsed payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tsbk {
    /// Last-block flag (bit 0).
    pub last_block: bool,
    /// Encryption flag (bit 1).
    pub encrypted: bool,
    /// 6-bit opcode.
    pub opcode: u8,
    /// Vendor ID (0 = standard).
    pub vendor: u8,
    /// Parsed payload.
    pub message: TsbkMessage,
    /// The error-corrected 96-bit TSBK, MSB-first in 12 bytes.
    ///
    /// Kept so a consumer can audit an opcode this crate does not parse — the
    /// unknown-vendor exemplars and the raw event log both need the wire bytes,
    /// and re-deriving them downstream is impossible once the `BitBuf` is gone.
    pub raw: [u8; 12],
}

/// sdrtrunk's short name for each of the 64 standard (MFID 0/1) **outbound**
/// TSBK opcodes, indexed by opcode.
///
/// The table is sdrtrunk's `Opcode` enum verbatim (its short labels, the ones
/// its decode log prints), so a histogram produced here and one produced there
/// name the same message the same way.
const OSP_OPCODE_NAMES: [&str; 64] = [
    "GRP_VCH_GRANT",
    "RESERVED_01",
    "GRP_VCH_GRNT_UPD",
    "GRP_VCH_GRNT_UPX",
    "UU_VCH_GRANT",
    "UU_ANS_REQ",
    "UU_VCH_GRANT_UPD",
    "RESERVED_07",
    "TEL_INT_VCH_GRNT",
    "TEL_INT_VCH_GRNU",
    "TEL_INT_ANS_RQST",
    "RESERVED_0B",
    "RESERVED_0C",
    "RESERVED_0D",
    "RESERVED_0E",
    "RESERVED_0F",
    "IND_DCH_GRANT",
    "GRP_DCH_GRANT",
    "GRP_DCH_ANNOUNCE",
    "GRP_DCH_ANNC_EXP",
    "SNDCP_DCH_GRANT",
    "SNDCP_DCH_PAG_RQ",
    "SNDCP_DCH_ANN_EX",
    "RESERVED_17",
    "STATUS_UPDATE",
    "RESERVED_19",
    "OSP_STATUS_QUERY",
    "RESERVED_1B",
    "MESSAGE_UPDATE",
    "RADIO_MONITR_CMD",
    "RADIO UNIT MONITOR ENHANCED COMMAND",
    "CALL_ALERT",
    "ACK_RESPONSE_FNE",
    "QUEUED_RESPONSE",
    "RESERVED_22",
    "RESERVED_23",
    "EXTNDED_FUNC_CMD",
    "RESERVED_25",
    "RESERVED_26",
    "DENY_RESPONSE",
    "GRP_AFFIL_RESP",
    "SCCB_CCH_BCST_EX",
    "GRP_AFFIL_QUERY",
    "LOCN_REG_RESPONS",
    "UNIT_REG_RESPONS",
    "UNIT_REG_COMMAND",
    "AUTH_COMMAND",
    "DE_REGIST_ACK",
    "TDMA_SYNC_BCST",
    "AUTH_DEMAND",
    "AUTH_FNE_RESP",
    "IDEN_UPDATE_TDMA",
    "IDEN_UPDATE_VUHF",
    "TIME_DATE_ANNOUN",
    "ROAM_ADDR_CMD",
    "ROAM_ADDR_UPDATE",
    "SYS_SVC_BCAST",
    "SEC_CCH_BROADCST",
    "RFSS_STATUS_BCST",
    "NET_STATUS_BCAST",
    "ADJ SITE STATUS",
    "IDEN_UPDATE",
    "ADJ_STS_BCST_UNC",
    "OSP RESERVED 3F",
];

/// sdrtrunk's short name for a standard outbound TSBK opcode.
///
/// `opcode` is masked to 6 bits, so every input names something.
pub fn osp_opcode_name(opcode: u8) -> &'static str {
    OSP_OPCODE_NAMES[usize::from(opcode & 0x3F)]
}

/// Names for the L3Harris (MFID 164) OSP opcodes this decoder does not parse
/// by value — a *catalogued* unknown, so a consumer watching for a payload
/// change can tell `164:1` from `164:15` by name and not only by opcode.
const HARRIS_OSP_NAMES: [&str; 64] = [
    "HARRIS OSP 0",
    "HARRIS OSP 1",
    "HARRIS OSP 2",
    "HARRIS OSP 3",
    "HARRIS OSP 4",
    "HARRIS OSP 5",
    "HARRIS OSP 6",
    "HARRIS OSP 7",
    "HARRIS OSP 8",
    "HARRIS OSP 9",
    "HARRIS OSP 10",
    "HARRIS OSP 11",
    "HARRIS OSP 12",
    "HARRIS OSP 13",
    "HARRIS OSP 14",
    "HARRIS OSP 15",
    "HARRIS OSP 16",
    "HARRIS OSP 17",
    "HARRIS OSP 18",
    "HARRIS OSP 19",
    "HARRIS OSP 20",
    "HARRIS OSP 21",
    "HARRIS OSP 22",
    "HARRIS OSP 23",
    "HARRIS OSP 24",
    "HARRIS OSP 25",
    "HARRIS OSP 26",
    "HARRIS OSP 27",
    "HARRIS OSP 28",
    "HARRIS OSP 29",
    "HARRIS OSP 30",
    "HARRIS OSP 31",
    "HARRIS OSP 32",
    "HARRIS OSP 33",
    "HARRIS OSP 34",
    "HARRIS OSP 35",
    "HARRIS OSP 36",
    "HARRIS OSP 37",
    "HARRIS OSP 38",
    "HARRIS OSP 39",
    "HARRIS OSP 40",
    "HARRIS OSP 41",
    "HARRIS OSP 42",
    "HARRIS OSP 43",
    "HARRIS OSP 44",
    "HARRIS OSP 45",
    "HARRIS OSP 46",
    "HARRIS OSP 47",
    "HARRIS OSP 48",
    "HARRIS OSP 49",
    "HARRIS OSP 50",
    "HARRIS OSP 51",
    "HARRIS OSP 52",
    "HARRIS OSP 53",
    "HARRIS OSP 54",
    "HARRIS OSP 55",
    "HARRIS OSP 56",
    "HARRIS OSP 57",
    "HARRIS OSP 58",
    "HARRIS OSP 59",
    "HARRIS OSP 60",
    "HARRIS OSP 61",
    "HARRIS OSP 62",
    "HARRIS OSP 63",
];

/// `HARRIS OSP <opcode>` for any 6-bit L3Harris opcode (masked, so every input
/// names something).
pub fn harris_osp_name(opcode: u8) -> &'static str {
    HARRIS_OSP_NAMES[usize::from(opcode & 0x3F)]
}

impl TsbkMessage {
    /// sdrtrunk-style name of this payload.
    ///
    /// Every variant names itself; [`TsbkMessage::Other`] falls back to the
    /// standard opcode table for a standard vendor and to sdrtrunk's generic
    /// unknown-vendor label otherwise. When the *exact* opcode matters (the
    /// three identifier updates share one variant) use [`Tsbk::name`], which
    /// has the opcode to hand.
    pub fn name(&self) -> &'static str {
        match self {
            TsbkMessage::GroupVoiceGrant { .. } => osp_opcode_name(0),
            TsbkMessage::GroupVoiceGrantUpdate { .. } => osp_opcode_name(2),
            TsbkMessage::GroupVoiceGrantUpdateExplicit { .. } => osp_opcode_name(3),
            TsbkMessage::UnitToUnitGrant { update, .. } => {
                osp_opcode_name(if *update { 6 } else { 4 })
            }
            TsbkMessage::TelephoneInterconnectGrant { update, .. } => {
                osp_opcode_name(if *update { 9 } else { 8 })
            }
            TsbkMessage::SndcpDataChannelGrant { .. } => osp_opcode_name(20),
            TsbkMessage::SndcpDataPageRequest { .. } => osp_opcode_name(21),
            TsbkMessage::SndcpDataChannelAnnounceExplicit { .. } => osp_opcode_name(22),
            TsbkMessage::AcknowledgeResponse { .. } => osp_opcode_name(32),
            TsbkMessage::QueuedResponse { .. } => osp_opcode_name(33),
            TsbkMessage::DenyResponse { .. } => osp_opcode_name(39),
            TsbkMessage::GroupAffiliationResponse { .. } => osp_opcode_name(40),
            TsbkMessage::ExtendedFunctionCommand { .. } => osp_opcode_name(36),
            TsbkMessage::GroupAffiliationQuery { .. } => osp_opcode_name(42),
            TsbkMessage::LocationRegistrationResponse { .. } => osp_opcode_name(43),
            TsbkMessage::UnitRegistrationResponse { .. } => osp_opcode_name(44),
            TsbkMessage::UnitRegistrationCommand { .. } => osp_opcode_name(45),
            TsbkMessage::UnitDeRegistrationAck { .. } => osp_opcode_name(47),
            TsbkMessage::SystemServiceBroadcast { .. } => osp_opcode_name(56),
            TsbkMessage::SecondaryControlChannel { .. } => osp_opcode_name(57),
            TsbkMessage::RfssStatus { .. } => osp_opcode_name(58),
            TsbkMessage::NetworkStatus { .. } => osp_opcode_name(59),
            TsbkMessage::AdjacentStatus { .. } => osp_opcode_name(60),
            // One variant covers opcodes 51, 52 and 61; the TDMA flag is the
            // only one of the three this type can tell apart.
            TsbkMessage::IdenUpdate(band) => osp_opcode_name(if band.tdma { 51 } else { 61 }),
            TsbkMessage::HarrisGroupRegroup { .. } => "HARRIS REGROUP",
            TsbkMessage::HarrisOsp { opcode } => harris_osp_name(*opcode),
            TsbkMessage::Other { opcode, vendor } => {
                if is_standard_vendor(*vendor) {
                    osp_opcode_name(*opcode)
                } else {
                    "UNKNOWN VENDOR/OPCODE OSP"
                }
            }
        }
    }
}

impl Tsbk {
    /// sdrtrunk-style name of this TSBK, from its actual opcode and vendor.
    ///
    /// Exact for all 64 standard opcodes — unlike [`TsbkMessage::name`], which
    /// cannot separate the three identifier updates.
    pub fn name(&self) -> &'static str {
        if is_standard_vendor(self.vendor) {
            osp_opcode_name(self.opcode)
        } else {
            self.message.name()
        }
    }

    /// The raw 96-bit TSBK as lower-case hex (24 characters).
    pub fn hex(&self) -> String {
        use std::fmt::Write as _;
        let mut s = String::with_capacity(24);
        for byte in self.raw {
            let _ = write!(s, "{byte:02x}");
        }
        s
    }
}

/// Raw transmit-offset field value meaning "this band has no transmit offset"
/// (receive-only): uplink == downlink. Identifier updates 51, 52 and 61 all
/// test the raw field against this sentinel (sdrtrunk
/// `FrequencyBandUpdate*.isTransmitOffsetValid`).
const NO_TRANSMIT_OFFSET: u32 = 0x80;

/// Signed transmit offset in Hz from a raw offset field, its sign bit, and the
/// per-opcode unit (250 kHz for opcode 61, the channel spacing for 51/52).
/// Returns 0 for the "no transmit offset" sentinel.
fn signed_offset(offset_raw: u32, sign_positive: bool, unit_hz: i64) -> i64 {
    if offset_raw == NO_TRANSMIT_OFFSET {
        return 0;
    }
    let mag = i64::from(offset_raw) * unit_hz;
    if sign_positive {
        mag
    } else {
        -mag
    }
}

/// Interpretation of the L3Harris REGROUP_OPTIONS byte (opcode 48).
///
/// Only the three flags the recorder can corroborate against captured traffic
/// are named here (the byte-layout note in [`parse_tsbk`] cites the samples);
/// the low bits carry a supergroup sequence the recorder does not interpret.
/// Every reader goes through these so the bit meanings live in one place.
pub mod regroup {
    /// `0x20` — the regroup is being **activated** (clear = canceled/torn down).
    #[must_use]
    pub const fn is_activate(options: u8) -> bool {
        options & 0x20 != 0
    }

    /// `0x80` — **patch** (two-way) when set, simulselect (one-way) when clear.
    #[must_use]
    pub const fn is_patch(options: u8) -> bool {
        options & 0x80 != 0
    }

    /// `0x40` — the regrouped target is a **talkgroup** (clear = an individual
    /// radio, which the patch tracker ignores).
    #[must_use]
    pub const fn target_is_talkgroup(options: u8) -> bool {
        options & 0x40 != 0
    }
}

/// Parse a decoded, CRC-valid 96-bit TSBK at `[start, start+96)`.
pub fn parse_tsbk(b: &BitBuf, start: usize) -> Tsbk {
    let last_block = b.get(start);
    let encrypted = b.get(start + 1);
    let opcode = b.get_u32(start + 2, 6) as u8;
    let vendor = b.get_u32(start + 8, 8) as u8;
    let g = |off: usize, w: usize| b.get_u32(start + off, w);

    let message = if vendor == MFID_HARRIS {
        // L3Harris vendor OSP: name opcode 48; all others generic. Standard
        // opcode parsing must not run for a vendor≠STANDARD TSBK.
        match opcode {
            48 => {
                let regroup_options = g(16, 8) as u8;
                // GRG_EXENC_CMD (sdrtrunk name for mfid 0xA4 / opcode 0x30).
                // REGROUP_OPTIONS byte (bits 16–23): 0x80 = regroup type
                // (patch/simulselect), 0x40 = group-vs-individual target
                // (isTalkgroupAddress), 0x20 = activate, 0x1F = supergroup
                // sequence. The target-type flag is 0x40 (absolute bit 17).
                //
                // Confirmed against captured control traffic (see the
                // patch-tracker tests and inventory::PatchBook). One live
                // activate, `b0a4ee36b100008006aa1f21`: byte2 0xee (patch,
                // talkgroup target, activate), supergroup 0x36b1, member
                // talkgroup 0x06aa in bits 64–79. Its cancel counterpart
                // `b0a48036b1000000000089d4` clears the activate bit and zeroes
                // the target. `regroup::` reads these flags in one place.
                let target_is_talkgroup = regroup::target_is_talkgroup(regroup_options);
                let target = if target_is_talkgroup {
                    g(64, 16) // TARGET_TALKGROUP_ADDRESS 64–79
                } else {
                    g(56, 24) // TARGET_RADIO_ADDRESS 56–79
                };
                TsbkMessage::HarrisGroupRegroup {
                    regroup_options,
                    supergroup: g(24, 16) as u16,
                    key_id: g(40, 16) as u16,
                    target,
                    target_is_talkgroup,
                }
            }
            _ => TsbkMessage::HarrisOsp { opcode },
        }
    } else if !is_standard_vendor(vendor) {
        // MFID 0 (STANDARD) and MFID 1 (STANDARD_V1) both use the standard
        // opcode table; every other MFID is a vendor message we do not parse.
        // Same rule as `LinkControl::parse` (sdrtrunk `Vendor`).
        TsbkMessage::Other { opcode, vendor }
    } else {
        match opcode {
            0 => TsbkMessage::GroupVoiceGrant {
                options: ServiceOptions::from_byte(g(16, 8) as u8),
                channel: ChannelRef {
                    band: g(24, 4) as u8,
                    channel: g(28, 12) as u16,
                },
                talkgroup: g(40, 16) as u16,
                source: g(56, 24),
            },
            2 => {
                let a = (
                    ChannelRef {
                        band: g(16, 4) as u8,
                        channel: g(20, 12) as u16,
                    },
                    g(32, 16) as u16,
                );
                let bpair = (
                    ChannelRef {
                        band: g(48, 4) as u8,
                        channel: g(52, 12) as u16,
                    },
                    g(64, 16) as u16,
                );
                let b_valid = bpair.1 != 0 && bpair.1 != a.1;
                TsbkMessage::GroupVoiceGrantUpdate {
                    a,
                    b: b_valid.then_some(bpair),
                }
            }
            3 => TsbkMessage::GroupVoiceGrantUpdateExplicit {
                options: ServiceOptions::from_byte(g(16, 8) as u8),
                downlink: ChannelRef {
                    band: g(32, 4) as u8,
                    channel: g(36, 12) as u16,
                },
                uplink: ChannelRef {
                    band: g(48, 4) as u8,
                    channel: g(52, 12) as u16,
                },
                talkgroup: g(64, 16) as u16,
            },
            4 | 6 => TsbkMessage::UnitToUnitGrant {
                update: opcode == 6,
                channel: ChannelRef {
                    band: g(16, 4) as u8,
                    channel: g(20, 12) as u16,
                },
                target: g(32, 24),
                source: g(56, 24),
            },
            8 | 9 => TsbkMessage::TelephoneInterconnectGrant {
                update: opcode == 9,
                options: ServiceOptions::from_byte(g(16, 8) as u8),
                channel: ChannelRef {
                    band: g(24, 4) as u8,
                    channel: g(28, 12) as u16,
                },
                call_timer: g(40, 16) as u16,
                // Bits 56–79: talkgroup for opcode 9, source radio for 8.
                address: g(56, 24),
                address_is_talkgroup: opcode == 9,
            },
            20 => TsbkMessage::SndcpDataChannelGrant {
                options: DataServiceOptions::from_byte(g(16, 8) as u8),
                downlink: ChannelRef {
                    band: g(24, 4) as u8,
                    channel: g(28, 12) as u16,
                },
                uplink: ChannelRef {
                    band: g(40, 4) as u8,
                    channel: g(44, 12) as u16,
                },
                target: g(56, 24),
            },
            21 => TsbkMessage::SndcpDataPageRequest {
                options: DataServiceOptions::from_byte(g(16, 8) as u8),
                target: g(56, 24),
                reserved: g(32, 24),
            },
            22 => TsbkMessage::SndcpDataChannelAnnounceExplicit {
                options: DataServiceOptions::from_byte(g(16, 8) as u8),
                autonomous_access: b.get(start + 24),
                requested_access: b.get(start + 25),
                downlink: ChannelRef {
                    band: g(32, 4) as u8,
                    channel: g(36, 12) as u16,
                },
                uplink: ChannelRef {
                    band: g(48, 4) as u8,
                    channel: g(52, 12) as u16,
                },
                data_access_control: g(64, 16) as u16,
            },
            32 => {
                let additional_information = b.get(start + 16);
                let extended_information = b.get(start + 17);
                // Bits 24–55 are one field read three ways (sdrtrunk
                // `AcknowledgeResponse.hasWACN/hasSourceAddress`).
                let qualified = additional_information && extended_information;
                let plain = additional_information && !extended_information;
                TsbkMessage::AcknowledgeResponse {
                    additional_information,
                    extended_information,
                    service_type: g(18, 6) as u8,
                    wacn: qualified.then(|| g(24, 20)),
                    system_id: qualified.then(|| g(44, 12) as u16),
                    source: plain.then(|| g(32, 24)),
                    target: g(56, 24),
                }
            }
            33 => TsbkMessage::QueuedResponse {
                additional_information: b.get(start + 16),
                service_type: g(18, 6) as u8,
                reason: g(24, 8) as u8,
                additional_info: g(32, 24),
                target: g(56, 24),
            },
            39 => TsbkMessage::DenyResponse {
                additional_information: b.get(start + 16),
                service_type: g(18, 6) as u8,
                reason: g(24, 8) as u8,
                additional_info: g(32, 24),
                target: g(56, 24),
            },
            40 => TsbkMessage::GroupAffiliationResponse {
                // Bit 16 overlaps sdrtrunk's nominal RESERVED byte: set =
                // global affiliation, clear = local.
                global_affiliation: b.get(start + 16),
                response: g(22, 2) as u8,
                announcement_group: g(24, 16) as u16,
                group: g(40, 16) as u16,
                target: g(56, 24),
            },
            36 => TsbkMessage::ExtendedFunctionCommand {
                function: ExtendedFunction::from_code(g(16, 16) as u16),
                arguments: g(32, 24),
                target: g(56, 24),
            },
            42 => TsbkMessage::GroupAffiliationQuery {
                target: g(32, 24),
                source: g(56, 24),
            },
            43 => TsbkMessage::LocationRegistrationResponse {
                response: g(22, 2) as u8,
                group: g(24, 16) as u16,
                rfss: g(40, 8) as u8,
                site: g(48, 8) as u8,
                target: g(56, 24),
            },
            // The response field sits at 18–19 here, not 22–23 as on opcodes
            // 40 and 43 (sdrtrunk `UnitRegistrationResponse.RESPONSE`).
            44 => TsbkMessage::UnitRegistrationResponse {
                response: g(18, 2) as u8,
                system_id: g(20, 12) as u16,
                source_id: g(32, 24),
                source_address: g(56, 24),
            },
            45 => TsbkMessage::UnitRegistrationCommand {
                target: g(32, 24),
                source: g(56, 24),
            },
            47 => TsbkMessage::UnitDeRegistrationAck {
                wacn: g(24, 20),
                system_id: g(44, 12) as u16,
                source: g(56, 24),
            },
            // Layout from the reference decoder
            // (`tsbk/standard/osp/SystemServiceBroadcast.java`): RESERVED
            // 16–23, AVAILABLE_SERVICES 24–47, SUPPORTED_SERVICES 48–71,
            // REQUEST_PRIORITY_LEVEL 72–79.
            56 => TsbkMessage::SystemServiceBroadcast {
                available: SystemServices::from_bits(g(24, 24)),
                supported: SystemServices::from_bits(g(48, 24)),
                priority: g(72, 8) as u8,
            },
            57 => {
                let channel_b = ChannelRef {
                    band: g(56, 4) as u8,
                    channel: g(60, 12) as u16,
                };
                TsbkMessage::SecondaryControlChannel {
                    rfss: g(16, 8) as u8,
                    site: g(24, 8) as u8,
                    channel_a: ChannelRef {
                        band: g(32, 4) as u8,
                        channel: g(36, 12) as u16,
                    },
                    service_class_a: ServiceClass::from_byte(g(48, 8) as u8),
                    // A zero band *and* channel number means "no second channel".
                    channel_b: (channel_b.band != 0 || channel_b.channel != 0).then_some(channel_b),
                    service_class_b: ServiceClass::from_byte(g(72, 8) as u8),
                }
            }
            58 => TsbkMessage::RfssStatus {
                lra: g(16, 8) as u8,
                active_network_connection: b.get(start + 27),
                system_id: g(28, 12) as u16,
                rfss: g(40, 8) as u8,
                site: g(48, 8) as u8,
                channel: ChannelRef {
                    band: g(56, 4) as u8,
                    channel: g(60, 12) as u16,
                },
                service_class: ServiceClass::from_byte(g(72, 8) as u8),
            },
            59 => TsbkMessage::NetworkStatus {
                lra: g(16, 8) as u8,
                wacn: g(24, 20),
                system_id: g(44, 12) as u16,
                channel: ChannelRef {
                    band: g(56, 4) as u8,
                    channel: g(60, 12) as u16,
                },
                service_class: ServiceClass::from_byte(g(72, 8) as u8),
            },
            60 => TsbkMessage::AdjacentStatus {
                lra: g(16, 8) as u8,
                flags: AdjacentSiteFlags {
                    conventional: b.get(start + 24),
                    site_failure: b.get(start + 25),
                    valid_information: b.get(start + 26),
                    active_rfss_connection: b.get(start + 27),
                },
                system_id: g(28, 12) as u16,
                rfss: g(40, 8) as u8,
                site: g(48, 8) as u8,
                channel: ChannelRef {
                    band: g(56, 4) as u8,
                    channel: g(60, 12) as u16,
                },
                service_class: ServiceClass::from_byte(g(72, 8) as u8),
            },
            61 => {
                // Standard FDMA identifier update.
                let id = g(16, 4) as u8;
                let bandwidth = g(20, 9) * 125;
                let sign_positive = b.get(start + 29);
                let offset_raw = g(30, 8);
                let spacing = g(38, 10) * 125;
                let base = u64::from(b.get_u32(start + 48, 32)) * 5;
                // "No offset" sentinel: raw offset field == 0x80.
                let offset_hz = signed_offset(offset_raw, sign_positive, 250_000);
                TsbkMessage::IdenUpdate(IdenBand {
                    id,
                    base_hz: base,
                    spacing_hz: spacing,
                    offset_hz,
                    bandwidth_hz: bandwidth,
                    slots_per_carrier: 1,
                    tdma: false,
                })
            }
            52 => {
                // VHF/UHF identifier update.
                let id = g(16, 4) as u8;
                let bandwidth = match g(20, 4) {
                    0x4 => 6_250,
                    0x5 => 12_500,
                    _ => 0,
                };
                let sign_positive = b.get(start + 24);
                let offset_raw = g(25, 13);
                let spacing = g(38, 10) * 125;
                let base = u64::from(b.get_u32(start + 48, 32)) * 5;
                // "No transmit offset" sentinel, same as opcode 61: sdrtrunk's
                // FrequencyBandUpdateVUHF.isTransmitOffsetValid() tests the raw
                // offset field against 0x80 (band is receive-only → uplink ==
                // downlink).
                let offset_hz = signed_offset(offset_raw, sign_positive, i64::from(spacing));
                TsbkMessage::IdenUpdate(IdenBand {
                    id,
                    base_hz: base,
                    spacing_hz: spacing,
                    offset_hz,
                    bandwidth_hz: bandwidth,
                    slots_per_carrier: 1,
                    tdma: false,
                })
            }
            51 => {
                // TDMA identifier update.
                let id = g(16, 4) as u8;
                let channel_type = g(20, 4) as u8;
                let (tdma, bandwidth, slots) = tdma_channel_type(channel_type);
                let sign_positive = b.get(start + 24);
                let offset_raw = g(25, 13);
                let spacing = g(38, 10) * 125;
                let base = u64::from(b.get_u32(start + 48, 32)) * 5;
                // "No transmit offset" sentinel (sdrtrunk
                // FrequencyBandUpdateTDMA.isTransmitOffsetValid()).
                let offset_hz = signed_offset(offset_raw, sign_positive, i64::from(spacing));
                TsbkMessage::IdenUpdate(IdenBand {
                    id,
                    base_hz: base,
                    spacing_hz: spacing,
                    offset_hz,
                    bandwidth_hz: bandwidth,
                    slots_per_carrier: slots,
                    tdma,
                })
            }
            _ => TsbkMessage::Other { opcode, vendor },
        }
    };

    let mut raw = [0u8; 12];
    b.copy_bits_to_bytes(start, 96, &mut raw);

    Tsbk {
        last_block,
        encrypted,
        opcode,
        vendor,
        message,
        raw,
    }
}

/// PDU header format for Alternate Multi-Block Trunking (AMBT): a trunking
/// control message too large for one TSBK, carried as a PDU header plus data
/// blocks. 1,980 of the 2,009 PDUs in the 2026-09-03 window are this format,
/// all on the control channel.
pub const PDU_FORMAT_AMBT: u8 = 0x17;

/// Parsed PDU header (spec §7.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PduHeader {
    /// Confirmation required (bit 1) — selects the 3/4-rate block path.
    pub confirmed: bool,
    /// Direction flag (bit 2).
    pub outbound: bool,
    /// Format (bits 3–7).
    pub format: u8,
    /// Vendor (bits 16–23).
    pub vendor: u8,
    /// Logical link ID (bits 24–47).
    pub llid: u32,
    /// Blocks to follow (bits 49–55).
    pub blocks_to_follow: u8,
    /// Trunking opcode carried in the header of an [`PDU_FORMAT_AMBT`] packet
    /// — octet 7's low six bits, message bits 58–63 — and `None` for every
    /// other format. Its manufacturer id is the header's own [`Self::vendor`]
    /// (octet 2), which AMBT shares with the ordinary PDU header.
    ///
    /// TIA-102.AABC-D: the AMBT header block reuses the data PDU header
    /// through octet 6 (format, SAP, MFID, target LLID, blocks to follow) and
    /// puts the trunking opcode where a data packet keeps its pad-octet count,
    /// as `[reserved(2) | opcode(6)]`. Nothing in this repo's spec extract
    /// (docs/P25P1-SPEC.md) covers AMBT, and the recorder's own log carries no
    /// raw header hex to check the field against, so this is the standard
    /// layout applied, not a layout corroborated on air. Logging it is what
    /// makes that check possible next time.
    pub ambt_opcode: Option<u8>,
}

impl PduHeader {
    /// sdrtrunk-style name of an AMBT header's trunking opcode, or `None` when
    /// this is not an AMBT header. Vendor messages have no standard name.
    pub fn ambt_opcode_name(&self) -> Option<&'static str> {
        self.ambt_opcode.map(|op| {
            if is_standard_vendor(self.vendor) {
                osp_opcode_name(op)
            } else {
                "UNKNOWN VENDOR/OPCODE OSP"
            }
        })
    }
}

/// AMBT trunking opcode 4 — UNIT-TO-UNIT VOICE CHANNEL GRANT.
pub const AMBT_OP_UU_VCH_GRANT: u8 = 4;

/// AMBT trunking opcode 6 — UNIT-TO-UNIT VOICE CHANNEL GRANT UPDATE.
pub const AMBT_OP_UU_VCH_GRANT_UPDATE: u8 = 6;

/// The sentinel this site puts in the reverse-channel field of a unit-to-unit
/// grant: band 15, channel 4095 — "no explicit uplink", the same sentinel the
/// SNDCP data-channel grant uses.
const CHANNEL_NULL: u16 = 0xFFFF;

/// Octets of user data a unit-to-unit AMBT grant occupies, CRC-32 excluded.
const AMBT_UU_LEN: usize = 20;

/// A unit-to-unit (private) voice channel grant carried as an **Alternate
/// Multi-Block Trunking** message: PDU format [`PDU_FORMAT_AMBT`], opcode
/// [`AMBT_OP_UU_VCH_GRANT`] or [`AMBT_OP_UU_VCH_GRANT_UPDATE`].
///
/// The site of `private/docs/analysis/unknown-phase3-2026-09-05.md` sends its private
/// call setup this way rather than as a single-block TSBK — 20 + 216 messages
/// a day naming ~45-60 conversations that the recorder could hear and, before
/// this type existed, never followed.
///
/// # Layout
///
/// Measured over 4 874 payloads from two independent receivers (§5 of that
/// report); the share of payloads each assertion holds for is in brackets.
///
/// ```text
/// octet  0 ..  3   source SUID: WACN (20 bits) | system id (12 bits)   [98.6%]
/// octet  4 ..  6   source unit — the unit transmitting on the grant    [99.0%]
/// octet  7 ..  9   target unit — the other party
/// octet 10 .. 11   granted channel: band (4) | channel number (12)     [99.7%]
/// octet 12 .. 13   reverse channel, always 0xFFFF on this site         [98.8%]
/// octet 14 .. 16   abbreviated SUID repeat
/// octet 17 .. 19   target unit repeat                                  [97.5%]
/// ```
///
/// Anything after octet 19 is the unconfirmed multi-block PDU's CRC-32 and is
/// not part of the message.
///
/// Which of the two units is *talking* is §10 of the same report: clustered
/// into sessions, the source is constant across a session and changes only
/// when the granted channel changes — a conversation taking turns, not one
/// message repeated to each party. So the source is the talker of this grant,
/// and a later grant on the same channel that swaps the two is the other party
/// keying up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnitGrantAmbt {
    /// WACN of the source's fully qualified SUID.
    pub source_wacn: u32,
    /// Home system id of the source's SUID.
    pub source_system: u16,
    /// The transmitting unit (FROM).
    pub source: u32,
    /// The unit being called (TO).
    pub target: u32,
    /// The granted voice channel.
    pub channel: ChannelRef,
    /// True for opcode 6 (the dwell-keeping update), false for opcode 4 (the
    /// call start).
    pub update: bool,
    /// The PDU header's direction bit: outbound (site to subscriber) for every
    /// grant heard on a downlink capture.
    pub outbound: bool,
}

impl UnitGrantAmbt {
    /// The equivalent single-block trunking message, so a unit-to-unit AMBT
    /// grant reaches the trunking tracker by exactly the path a TSBK grant of
    /// the same opcode takes.
    pub fn as_tsbk_message(&self) -> TsbkMessage {
        TsbkMessage::UnitToUnitGrant {
            update: self.update,
            channel: self.channel,
            target: self.target,
            source: self.source,
        }
    }
}

/// Decode a unit-to-unit voice channel grant (or grant update) out of a
/// reassembled AMBT payload, per [`UnitGrantAmbt`].
///
/// `None` unless the header is a standard-vendor AMBT carrying opcode 4 or 6,
/// the payload is long enough, and the reverse-channel sentinel is where the
/// layout says it is. That sentinel is the one field whose value is known in
/// advance, and so the only cheap check that a mis-assembled payload is not
/// about to be read as somebody's call.
pub fn parse_ambt_unit_grant(header: &PduHeader, payload: &[u8]) -> Option<UnitGrantAmbt> {
    if header.format != PDU_FORMAT_AMBT || !is_standard_vendor(header.vendor) {
        return None;
    }
    let update = match header.ambt_opcode? {
        AMBT_OP_UU_VCH_GRANT => false,
        AMBT_OP_UU_VCH_GRANT_UPDATE => true,
        _ => return None,
    };
    let b: &[u8] = payload.get(..AMBT_UU_LEN)?;
    if u16::from_be_bytes([b[12], b[13]]) != CHANNEL_NULL {
        return None;
    }
    let suid = u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
    let channel = u16::from_be_bytes([b[10], b[11]]);
    let unit = |at: usize| u32::from_be_bytes([0, b[at], b[at + 1], b[at + 2]]);
    Some(UnitGrantAmbt {
        source_wacn: suid >> 12,
        source_system: (suid & 0xFFF) as u16,
        source: unit(4),
        target: unit(7),
        channel: ChannelRef {
            band: (channel >> 12) as u8,
            channel: channel & 0x0FFF,
        },
        update,
        outbound: header.outbound,
    })
}

/// Parse a decoded, CRC-valid 96-bit PDU header at `[start, start+96)`.
pub fn parse_pdu_header(b: &BitBuf, start: usize) -> PduHeader {
    let format = b.get_u32(start + 3, 5) as u8;
    PduHeader {
        confirmed: b.get(start + 1),
        outbound: b.get(start + 2),
        format,
        vendor: b.get_u32(start + 16, 8) as u8,
        llid: b.get_u32(start + 24, 24),
        blocks_to_follow: b.get_u32(start + 49, 7) as u8,
        ambt_opcode: (format == PDU_FORMAT_AMBT).then(|| b.get_u32(start + 58, 6) as u8),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buf96() -> BitBuf {
        let mut b = BitBuf::new();
        for _ in 0..96 {
            b.push(false);
        }
        b
    }

    /// Build a synthetic standard-vendor TSBK carrying `opcode`.
    fn osp(opcode: u8) -> BitBuf {
        let mut b = buf96();
        b.set(0, true);
        b.set_u64(2, 6, u64::from(opcode));
        b.set_u64(8, 8, 0);
        b
    }

    /// The AMBT header (PDU format 0x17) carries the trunking message's MFID
    /// in octet 2 — the ordinary PDU header's vendor field — and its opcode in
    /// octet 7's low six bits (message bits 58-63). Every other PDU format
    /// leaves `ambt_opcode` empty, because octet 7 means something else there.
    #[test]
    fn an_ambt_header_carries_the_trunking_mfid_and_opcode() {
        let mut b = buf96();
        b.set(2, true); // outbound
        b.set_u64(3, 5, u64::from(PDU_FORMAT_AMBT));
        b.set_u64(16, 8, 0); // standard vendor
        b.set_u64(24, 24, 9_000_001);
        b.set_u64(49, 7, 2);
        b.set_u64(58, 6, 0); // GRP_VCH_GRANT
        let h = parse_pdu_header(&b, 0);
        assert_eq!(h.format, PDU_FORMAT_AMBT);
        assert_eq!(h.vendor, 0);
        assert_eq!(h.llid, 9_000_001);
        assert_eq!(h.blocks_to_follow, 2);
        assert_eq!(h.ambt_opcode, Some(0));
        assert_eq!(h.ambt_opcode_name(), Some("GRP_VCH_GRANT"));

        // The reserved two bits above the opcode are not part of it.
        b.set_u64(56, 8, 0xFF);
        let h = parse_pdu_header(&b, 0);
        assert_eq!(h.ambt_opcode, Some(0x3F));
        assert_eq!(h.ambt_opcode_name(), Some(osp_opcode_name(63)));

        // A vendor AMBT names no standard opcode.
        b.set_u64(16, 8, u64::from(MFID_HARRIS));
        assert_eq!(
            parse_pdu_header(&b, 0).ambt_opcode_name(),
            Some("UNKNOWN VENDOR/OPCODE OSP")
        );

        // Confirmed data (format 22): octet 7 is the pad-octet count, so
        // there is no AMBT opcode to report.
        b.set_u64(3, 5, 22);
        let h = parse_pdu_header(&b, 0);
        assert_eq!(h.format, 22);
        assert_eq!(h.ambt_opcode, None);
        assert_eq!(h.ambt_opcode_name(), None);
    }

    /// An AMBT header for a standard-vendor trunking `opcode`, as the framer
    /// would have parsed it off the control channel.
    fn uu_header(opcode: u8) -> PduHeader {
        PduHeader {
            confirmed: false,
            outbound: true,
            format: PDU_FORMAT_AMBT,
            vendor: 0,
            llid: 9_000_001,
            blocks_to_follow: 2,
            ambt_opcode: Some(opcode),
        }
    }

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex"))
            .collect()
    }

    /// Four payloads with the layout of event-log `pdu` lines whose header carried
    /// `ambt_opcode` 4 or 6 (unit IDs synthetic, PDU CRC recomputed). That layout
    /// is what [`UnitGrantAmbt`] was measured against, so they are what pins it.
    #[test]
    fn a_unit_to_unit_ambt_grant_decodes_both_units_and_the_channel() {
        // 0:4 — 9000001 calls 9000002 on band 1, channel 0x112.
        let g = parse_ambt_unit_grant(
            &uu_header(AMBT_OP_UU_VCH_GRANT),
            &hex("455640348954418954421112ffff56403489544242fc0d6b"),
        )
        .expect("a unit grant");
        assert_eq!(g.source_wacn, 0x45564);
        assert_eq!(g.source_system, 0x034);
        assert_eq!(g.source, 9_000_001);
        assert_eq!(g.target, 9_000_002);
        assert_eq!(
            g.channel,
            ChannelRef {
                band: 1,
                channel: 0x112
            }
        );
        assert!(!g.update, "opcode 4 is the grant, not the update");
        assert!(g.outbound);

        // 0:4 with the pair the other way round: 9000007 is now the talker.
        let g = parse_ambt_unit_grant(
            &uu_header(AMBT_OP_UU_VCH_GRANT),
            &hex("45564034895447895441105affff564034895441babcc920"),
        )
        .expect("a unit grant");
        assert_eq!((g.source, g.target), (9_000_007, 9_000_001));
        assert_eq!(
            g.channel,
            ChannelRef {
                band: 1,
                channel: 0x05a
            }
        );

        // 0:6 — the update that keeps the dwell open, same shape.
        let g = parse_ambt_unit_grant(
            &uu_header(AMBT_OP_UU_VCH_GRANT_UPDATE),
            &hex("45564034895441895442107affff5640348954421db0f6f7"),
        )
        .expect("a unit grant update");
        assert_eq!((g.source, g.target), (9_000_001, 9_000_002));
        assert_eq!(
            g.channel,
            ChannelRef {
                band: 1,
                channel: 0x07a
            }
        );
        assert!(g.update);
        assert_eq!(
            g.as_tsbk_message(),
            TsbkMessage::UnitToUnitGrant {
                update: true,
                channel: ChannelRef {
                    band: 1,
                    channel: 0x07a
                },
                target: 9_000_002,
                source: 9_000_001,
            },
            "it reaches the tracker as the TSBK grant it is the multi-block form of"
        );

        let g = parse_ambt_unit_grant(
            &uu_header(AMBT_OP_UU_VCH_GRANT_UPDATE),
            &hex("4556403489544289544112ceffff564034895441fc99365f"),
        )
        .expect("a unit grant update");
        assert_eq!((g.source, g.target), (9_000_002, 9_000_001));
        assert_eq!(
            g.channel,
            ChannelRef {
                band: 1,
                channel: 0x2ce
            }
        );
    }

    /// Everything that is not a standard-vendor unit-to-unit grant decodes to
    /// nothing, including a truncated payload and one whose reverse-channel
    /// sentinel is missing (the cheap "this was mis-assembled" check).
    #[test]
    fn only_a_well_formed_standard_unit_grant_decodes() {
        let good = hex("455640348954418954421112ffff56403489544242fc0d6b");
        assert!(
            parse_ambt_unit_grant(&uu_header(0), &good).is_none(),
            "0:0 is a group grant"
        );
        assert!(parse_ambt_unit_grant(&uu_header(20), &good).is_none());

        let mut vendor = uu_header(AMBT_OP_UU_VCH_GRANT);
        vendor.vendor = MFID_HARRIS;
        assert!(
            parse_ambt_unit_grant(&vendor, &good).is_none(),
            "a vendor AMBT is not this"
        );

        let mut data = uu_header(AMBT_OP_UU_VCH_GRANT);
        data.format = 22;
        data.ambt_opcode = None;
        assert!(parse_ambt_unit_grant(&data, &good).is_none());

        let h = uu_header(AMBT_OP_UU_VCH_GRANT);
        assert!(
            parse_ambt_unit_grant(&h, &good[..19]).is_none(),
            "short payload"
        );
        let mut broken = good.clone();
        broken[12] = 0;
        assert!(
            parse_ambt_unit_grant(&h, &broken).is_none(),
            "no reverse-channel sentinel: not the layout we measured"
        );
    }

    #[test]
    fn the_raw_block_is_kept_verbatim() {
        let mut b = osp(44);
        b.set_u64(16, 64, 0x0123_4567_89AB_CDEF);
        let t = parse_tsbk(&b, 0);
        // Header (last block, opcode 44, MFID 0) then the 8 payload octets;
        // the CRC octets at 80–95 are zero in this synthetic word.
        assert_eq!(t.hex(), "ac000123456789abcdef0000");
        // Byte 0 = last-block 1, encrypted 0, opcode 44 (0b101100).
        assert_eq!(t.raw[0], 0b1010_1100);
    }

    #[test]
    fn every_standard_opcode_is_named_the_way_sdrtrunk_names_it() {
        assert_eq!(osp_opcode_name(0), "GRP_VCH_GRANT");
        assert_eq!(osp_opcode_name(44), "UNIT_REG_RESPONS");
        assert_eq!(osp_opcode_name(47), "DE_REGIST_ACK");
        assert_eq!(osp_opcode_name(56), "SYS_SVC_BCAST");
        assert_eq!(osp_opcode_name(60), "ADJ SITE STATUS");
        // Masked to 6 bits, so nothing can index out of the table.
        assert_eq!(osp_opcode_name(0xFF), osp_opcode_name(63));
        // An unparsed standard opcode still names itself.
        assert_eq!(parse_tsbk(&osp(56), 0).name(), "SYS_SVC_BCAST");
        assert_eq!(parse_tsbk(&osp(24), 0).message.name(), "STATUS_UPDATE");
    }

    #[test]
    fn a_vendor_tsbk_is_named_by_its_vendor_not_the_standard_table() {
        let mut b = osp(15);
        b.set_u64(8, 8, u64::from(MFID_HARRIS));
        let t = parse_tsbk(&b, 0);
        assert_eq!(t.name(), "HARRIS OSP 15");
        assert_eq!(t.message, TsbkMessage::HarrisOsp { opcode: 15 });
    }

    #[test]
    fn unit_registration_response_layout() {
        let mut b = osp(44);
        b.set_u64(18, 2, 0); // response: accepted
        b.set_u64(20, 12, 57); // system id
        b.set_u64(32, 24, 9_100_025); // source id
        b.set_u64(56, 24, 1_620_719); // source address
        match parse_tsbk(&b, 0).message {
            TsbkMessage::UnitRegistrationResponse {
                response,
                system_id,
                source_id,
                source_address,
            } => {
                assert_eq!(response_name(response), "accepted");
                assert_eq!(system_id, 57);
                assert_eq!(source_id, 9_100_025);
                assert_eq!(source_address, 1_620_719);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_unit_registration_response_field_is_at_18_not_22() {
        // Opcodes 40 and 43 put the response at 22–23; opcode 44 puts it at
        // 18–19 (sdrtrunk `UnitRegistrationResponse`). Setting 22–23 must not
        // move it.
        let mut b = osp(44);
        b.set_u64(18, 2, 3); // refused
        b.set_u64(22, 2, 1);
        match parse_tsbk(&b, 0).message {
            TsbkMessage::UnitRegistrationResponse { response, .. } => {
                assert_eq!(response_name(response), "refused");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn unit_registration_command_layout() {
        let mut b = osp(45);
        b.set_u64(32, 24, 1_620_719);
        b.set_u64(56, 24, 0xFF_FFFF);
        assert_eq!(
            parse_tsbk(&b, 0).message,
            TsbkMessage::UnitRegistrationCommand {
                target: 1_620_719,
                source: 0xFF_FFFF,
            }
        );
    }

    #[test]
    fn unit_deregistration_acknowledge_layout() {
        let mut b = osp(47);
        b.set_u64(24, 20, 284_004);
        b.set_u64(44, 12, 82);
        b.set_u64(56, 24, 1_620_755);
        assert_eq!(
            parse_tsbk(&b, 0).message,
            TsbkMessage::UnitDeRegistrationAck {
                wacn: 284_004,
                system_id: 82,
                source: 1_620_755,
            }
        );
    }

    #[test]
    fn group_affiliation_response_layout() {
        let mut b = osp(40);
        b.set(16, true); // global
        b.set_u64(22, 2, 0); // accepted
        b.set_u64(24, 16, 65_535); // announcement group
        b.set_u64(40, 16, 52_101); // group
        b.set_u64(56, 24, 1_620_719); // target radio
        match parse_tsbk(&b, 0).message {
            TsbkMessage::GroupAffiliationResponse {
                global_affiliation,
                response,
                announcement_group,
                group,
                target,
            } => {
                assert!(global_affiliation);
                assert_eq!(response_name(response), "accepted");
                assert_eq!(announcement_group, 65_535);
                assert_eq!(group, 52_101);
                assert_eq!(target, 1_620_719);
            }
            other => panic!("{other:?}"),
        }
        // Bit 16 clear is a local affiliation.
        b.set(16, false);
        match parse_tsbk(&b, 0).message {
            TsbkMessage::GroupAffiliationResponse {
                global_affiliation, ..
            } => assert!(!global_affiliation),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn group_affiliation_query_layout() {
        let mut b = osp(42);
        b.set_u64(32, 24, 9_000_045);
        b.set_u64(56, 24, 0xFF_FFFF);
        assert_eq!(
            parse_tsbk(&b, 0).message,
            TsbkMessage::GroupAffiliationQuery {
                target: 9_000_045,
                source: 0xFF_FFFF,
            }
        );
    }

    #[test]
    fn location_registration_response_layout() {
        let mut b = osp(43);
        b.set_u64(22, 2, 2); // denied
        b.set_u64(24, 16, 52_101);
        b.set_u64(40, 8, 10);
        b.set_u64(48, 8, 1);
        b.set_u64(56, 24, 1_620_719);
        match parse_tsbk(&b, 0).message {
            TsbkMessage::LocationRegistrationResponse {
                response,
                group,
                rfss,
                site,
                target,
            } => {
                assert_eq!(response_name(response), "denied");
                assert_eq!(group, 52_101);
                assert_eq!(rfss, 10);
                assert_eq!(site, 1);
                assert_eq!(target, 1_620_719);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn sndcp_data_channel_grant_layout() {
        let mut b = osp(20);
        b.set_u64(16, 8, 0x93); // emergency + packet mode, NSAPI 3
        b.set_u64(24, 4, 1);
        b.set_u64(28, 12, 320);
        b.set_u64(40, 4, 1);
        b.set_u64(44, 12, 4095); // no explicit uplink
        b.set_u64(56, 24, 9_000_040);
        match parse_tsbk(&b, 0).message {
            TsbkMessage::SndcpDataChannelGrant {
                options,
                downlink,
                uplink,
                target,
            } => {
                assert!(options.emergency);
                assert!(!options.encrypted);
                assert!(options.packet_mode);
                assert_eq!(options.nsapi, 3);
                assert_eq!(options.raw, 0x93);
                assert_eq!(
                    downlink,
                    ChannelRef {
                        band: 1,
                        channel: 320
                    }
                );
                assert_eq!(uplink.channel, 4095);
                assert_eq!(target, 9_000_040);
            }
            other => panic!("{other:?}"),
        }
    }

    /// The paged subscriber is at bits 56–79, not 32–55: bits 32–55 are the
    /// reserved field that holds `0x00ffff` on every page request the
    /// 2026-09-03 inventory measured, so reading it as the target made all
    /// 2,279 of them a page of radio 65535.
    /// The control-channel extended function command, from the field values
    /// the 2026-09-03 inventory measured: function 0x007d, arguments 0xfffffd,
    /// a real radio as the target.
    #[test]
    fn extended_function_command_layout() {
        let mut b = osp(36);
        b.set_u64(16, 16, 0x007D);
        b.set_u64(32, 24, 0xFF_FFFD);
        b.set_u64(56, 24, 9_000_065);
        let t = parse_tsbk(&b, 0);
        assert_eq!(t.name(), "EXTNDED_FUNC_CMD");
        match t.message {
            TsbkMessage::ExtendedFunctionCommand {
                function,
                arguments,
                target,
            } => {
                assert_eq!(function, ExtendedFunction::RadioDetach);
                assert_eq!(function.name(), "radio_detach");
                assert_eq!(function.code(), 0x007D);
                assert_eq!(arguments, 0xFF_FFFD);
                assert_eq!(target, 9_000_065);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn sndcp_data_page_request_layout() {
        let mut b = osp(21);
        b.set_u64(16, 8, 0x40); // encrypted
        b.set_u64(32, 24, 0x00_FFFF); // reserved, as seen on the air
        b.set_u64(56, 24, 9_000_040);
        match parse_tsbk(&b, 0).message {
            TsbkMessage::SndcpDataPageRequest {
                options,
                target,
                reserved,
            } => {
                assert!(options.encrypted);
                assert_eq!(target, 9_000_040);
                assert_eq!(reserved, 0x00_FFFF);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn sndcp_data_channel_announcement_explicit_layout() {
        let mut b = osp(22);
        b.set_u64(16, 8, 0x00);
        b.set(25, true); // requested access
        b.set_u64(32, 4, 0);
        b.set_u64(36, 12, 0);
        b.set_u64(48, 4, 0);
        b.set_u64(52, 12, 0);
        b.set_u64(64, 16, 65_535); // DAC 65535, as seen on the air
        match parse_tsbk(&b, 0).message {
            TsbkMessage::SndcpDataChannelAnnounceExplicit {
                autonomous_access,
                requested_access,
                data_access_control,
                downlink,
                ..
            } => {
                assert!(!autonomous_access);
                assert!(requested_access);
                assert_eq!(data_access_control, 65_535);
                assert_eq!(downlink.channel, 0);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn acknowledge_response_reads_bits_24_to_55_three_ways() {
        // Both flags set: a fully qualified WACN + system.
        let mut b = osp(32);
        b.set(16, true);
        b.set(17, true);
        b.set_u64(18, 6, 44); // acknowledging a unit registration request
        b.set_u64(24, 20, 284_004);
        b.set_u64(44, 12, 57);
        b.set_u64(56, 24, 1_620_719);
        match parse_tsbk(&b, 0).message {
            TsbkMessage::AcknowledgeResponse {
                service_type,
                wacn,
                system_id,
                source,
                target,
                ..
            } => {
                assert_eq!(service_type, 44);
                assert_eq!(wacn, Some(284_004));
                assert_eq!(system_id, Some(57));
                assert_eq!(source, None, "the source address is not this encoding");
                assert_eq!(target, 1_620_719);
            }
            other => panic!("{other:?}"),
        }

        // Additional information only: the same bits are a 24-bit source.
        b.set(17, false);
        b.set_u64(32, 24, 9_100_025);
        match parse_tsbk(&b, 0).message {
            TsbkMessage::AcknowledgeResponse {
                wacn,
                system_id,
                source,
                ..
            } => {
                assert_eq!(wacn, None);
                assert_eq!(system_id, None);
                assert_eq!(source, Some(9_100_025));
            }
            other => panic!("{other:?}"),
        }

        // Neither flag: nothing but the target is valid.
        b.set(16, false);
        match parse_tsbk(&b, 0).message {
            TsbkMessage::AcknowledgeResponse {
                wacn,
                system_id,
                source,
                target,
                ..
            } => {
                assert_eq!((wacn, system_id, source), (None, None, None));
                assert_eq!(target, 1_620_719);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn deny_and_queued_responses_share_a_layout() {
        for opcode in [33u8, 39] {
            let mut b = osp(opcode);
            b.set(16, true);
            b.set_u64(18, 6, 4); // unit-to-unit voice service request
            b.set_u64(24, 8, 0x30); // reason
            b.set_u64(32, 24, 0xABCDEF);
            b.set_u64(56, 24, 1_620_719);
            let got = parse_tsbk(&b, 0).message;
            let (service_type, reason, additional_info, target) = match got {
                TsbkMessage::DenyResponse {
                    service_type,
                    reason,
                    additional_info,
                    target,
                    ..
                }
                | TsbkMessage::QueuedResponse {
                    service_type,
                    reason,
                    additional_info,
                    target,
                    ..
                } => (service_type, reason, additional_info, target),
                other => panic!("opcode {opcode}: {other:?}"),
            };
            assert_eq!(service_type, 4);
            assert_eq!(reason, 0x30);
            assert_eq!(additional_info, 0xABCDEF);
            assert_eq!(target, 1_620_719);
        }
    }

    #[test]
    fn a_harris_tsbk_never_takes_a_standard_opcode_arm() {
        // Opcode 44 under MFID 164 is a Harris message, not a unit
        // registration response: the vendor selects the table.
        let mut b = osp(44);
        b.set_u64(8, 8, u64::from(MFID_HARRIS));
        b.set_u64(32, 24, 1_620_719);
        assert_eq!(
            parse_tsbk(&b, 0).message,
            TsbkMessage::HarrisOsp { opcode: 44 }
        );
    }

    #[test]
    fn group_grant_layout() {
        let mut b = buf96();
        b.set(0, true); // last block
        b.set_u64(2, 6, 0); // opcode 0
        b.set_u64(8, 8, 0); // standard vendor
        b.set_u64(16, 8, 0xA4); // svc: emergency + mode, priority 4
        b.set_u64(24, 4, 0x1);
        b.set_u64(28, 12, 0x140); // channel 320
        b.set_u64(40, 16, 0x4E21); // talkgroup 20001
        b.set_u64(56, 24, 0xBEEF01);
        let t = parse_tsbk(&b, 0);
        assert!(t.last_block);
        match t.message {
            TsbkMessage::GroupVoiceGrant {
                options,
                channel,
                talkgroup,
                source,
            } => {
                assert!(options.emergency);
                assert!(!options.encrypted);
                assert!(options.duplex);
                assert!(!options.packet_mode);
                assert_eq!(options.priority, 4);
                assert_eq!(
                    channel,
                    ChannelRef {
                        band: 1,
                        channel: 320
                    }
                );
                assert_eq!(talkgroup, 20001);
                assert_eq!(source, 0xBEEF01);
            }
            m => panic!("{m:?}"),
        }
    }

    #[test]
    fn iden_61_layout_and_sentinel() {
        let mut b = buf96();
        b.set_u64(2, 6, 61);
        b.set_u64(16, 4, 0x1);
        b.set_u64(20, 9, 0x64); // 100 × 125 = 12500 Hz bandwidth
        b.set(29, false); // negative offset
        b.set_u64(30, 8, 180); // 180 × 250000 = 45 MHz
        b.set_u64(38, 10, 100); // 100 × 125 = 12500 Hz spacing
        b.set_u64(48, 32, 170_235_100); // × 5 = 851_175_500 Hz
        match parse_tsbk(&b, 0).message {
            TsbkMessage::IdenUpdate(band) => {
                assert_eq!(band.id, 1);
                assert_eq!(band.bandwidth_hz, 12_500);
                assert_eq!(band.offset_hz, -45_000_000);
                assert_eq!(band.spacing_hz, 12_500);
                assert_eq!(band.base_hz, 851_175_500);
            }
            m => panic!("{m:?}"),
        }
        // Sentinel: raw offset 0x80 → no offset.
        b.set_u64(30, 8, 0x80);
        match parse_tsbk(&b, 0).message {
            TsbkMessage::IdenUpdate(band) => assert_eq!(band.offset_hz, 0),
            m => panic!("{m:?}"),
        }
    }

    #[test]
    fn grant_update_pair_b_rules() {
        let mut b = buf96();
        b.set_u64(2, 6, 2);
        b.set_u64(16, 4, 1);
        b.set_u64(20, 12, 100);
        b.set_u64(32, 16, 500);
        // Pair B same group → ignored.
        b.set_u64(48, 4, 1);
        b.set_u64(52, 12, 101);
        b.set_u64(64, 16, 500);
        match parse_tsbk(&b, 0).message {
            TsbkMessage::GroupVoiceGrantUpdate { b: pb, .. } => assert!(pb.is_none()),
            m => panic!("{m:?}"),
        }
        // Pair B different group → present.
        b.set_u64(64, 16, 501);
        match parse_tsbk(&b, 0).message {
            TsbkMessage::GroupVoiceGrantUpdate { b: pb, .. } => {
                assert_eq!(pb.unwrap().1, 501);
            }
            m => panic!("{m:?}"),
        }
    }

    #[test]
    fn telephone_opcode8_vs_9_address_semantic() {
        // Same wire bits, differing address type (spec §18.4).
        let mut b = buf96();
        b.set_u64(24, 4, 1); // band
        b.set_u64(28, 12, 200); // channel
        b.set_u64(40, 16, 300); // call timer
        b.set_u64(56, 24, 0xABCDEF); // address field

        b.set_u64(2, 6, 8); // opcode 8 → address is a source radio
        match parse_tsbk(&b, 0).message {
            TsbkMessage::TelephoneInterconnectGrant {
                update,
                address,
                address_is_talkgroup,
                call_timer,
                ..
            } => {
                assert!(!update);
                assert!(!address_is_talkgroup);
                assert_eq!(address, 0xABCDEF);
                assert_eq!(call_timer, 300);
            }
            m => panic!("{m:?}"),
        }

        b.set_u64(2, 6, 9); // opcode 9 → address is a talkgroup
        match parse_tsbk(&b, 0).message {
            TsbkMessage::TelephoneInterconnectGrant {
                update,
                address,
                address_is_talkgroup,
                ..
            } => {
                assert!(update);
                assert!(address_is_talkgroup);
                assert_eq!(address, 0xABCDEF);
            }
            m => panic!("{m:?}"),
        }
    }

    #[test]
    fn service_options_bit_by_bit() {
        // Each named flag is one bit of the byte, MSB-first, priority = low 3.
        assert_eq!(ServiceOptions::from_byte(0), ServiceOptions::default());
        let e = ServiceOptions::from_byte(0x80);
        assert!(e.emergency);
        assert!(!e.encrypted && !e.duplex && !e.packet_mode);
        assert_eq!(e.priority, 0);
        assert!(ServiceOptions::from_byte(0x40).encrypted);
        assert!(ServiceOptions::from_byte(0x20).duplex);
        assert!(ServiceOptions::from_byte(0x10).packet_mode);
        // Bit 0x08 is reserved and must not leak into priority.
        assert_eq!(ServiceOptions::from_byte(0x08).priority, 0);
        for p in 0..8u8 {
            assert_eq!(ServiceOptions::from_byte(p).priority, p);
        }
        let all = ServiceOptions::from_byte(0xFF);
        assert!(all.emergency && all.encrypted && all.duplex && all.packet_mode);
        assert_eq!(all.priority, 7);
    }

    #[test]
    fn grant_update_explicit_opcode3_layout() {
        let mut b = buf96();
        b.set_u64(2, 6, 3);
        b.set_u64(16, 8, 0xC0); // emergency + encrypted
        b.set_u64(32, 4, 1); // downlink band
        b.set_u64(36, 12, 456); // downlink channel
        b.set_u64(48, 4, 1); // uplink band
        b.set_u64(52, 12, 0xFFF); // uplink channel = none
        b.set_u64(64, 16, 52101);
        match parse_tsbk(&b, 0).message {
            TsbkMessage::GroupVoiceGrantUpdateExplicit {
                options,
                downlink,
                uplink,
                talkgroup,
            } => {
                assert!(options.emergency);
                assert!(options.encrypted);
                assert_eq!(
                    downlink,
                    ChannelRef {
                        band: 1,
                        channel: 456
                    }
                );
                assert_eq!(
                    uplink,
                    ChannelRef {
                        band: 1,
                        channel: 0xFFF
                    }
                );
                assert_eq!(talkgroup, 52101);
            }
            m => panic!("{m:?}"),
        }
    }

    #[test]
    fn unit_to_unit_opcode4_and_6_keep_24_bit_addresses() {
        let mut b = buf96();
        b.set_u64(16, 4, 1); // band
        b.set_u64(20, 12, 456); // channel
        b.set_u64(32, 24, 0xAB_CDEF); // target
        b.set_u64(56, 24, 0x12_3456); // source

        b.set_u64(2, 6, 4); // opcode 4 = grant
        match parse_tsbk(&b, 0).message {
            TsbkMessage::UnitToUnitGrant {
                update,
                channel,
                target,
                source,
            } => {
                assert!(!update);
                assert_eq!(
                    channel,
                    ChannelRef {
                        band: 1,
                        channel: 456
                    }
                );
                assert_eq!(target, 0xAB_CDEF);
                assert_eq!(source, 0x12_3456);
            }
            m => panic!("{m:?}"),
        }

        b.set_u64(2, 6, 6); // opcode 6 = update
        match parse_tsbk(&b, 0).message {
            TsbkMessage::UnitToUnitGrant { update, target, .. } => {
                assert!(update);
                assert_eq!(target, 0xAB_CDEF);
            }
            m => panic!("{m:?}"),
        }
    }

    #[test]
    fn iden_52_vhf_uhf_layout_and_sentinel() {
        let mut b = buf96();
        b.set_u64(2, 6, 52);
        b.set_u64(16, 4, 3); // band id
        b.set_u64(20, 4, 0x5); // 12.5 kHz bandwidth
        b.set(24, false); // negative offset
        b.set_u64(25, 13, 1000); // 1000 × spacing
        b.set_u64(38, 10, 50); // 50 × 125 = 6250 Hz spacing
        b.set_u64(48, 32, 30_000_000); // × 5 = 150 MHz base
        match parse_tsbk(&b, 0).message {
            TsbkMessage::IdenUpdate(band) => {
                assert_eq!(band.id, 3);
                assert_eq!(band.bandwidth_hz, 12_500);
                assert_eq!(band.spacing_hz, 6_250);
                assert_eq!(band.base_hz, 150_000_000);
                assert_eq!(band.offset_hz, -(1000 * 6_250));
                assert!(!band.tdma);
                assert_eq!(band.slots_per_carrier, 1);
                // Sanity: the derived channel math.
                assert_eq!(band.downlink_hz(4), 150_025_000);
                assert_eq!(band.uplink_hz(4), 150_025_000 - 6_250_000);
            }
            m => panic!("{m:?}"),
        }
        // Positive offset.
        b.set(24, true);
        match parse_tsbk(&b, 0).message {
            TsbkMessage::IdenUpdate(band) => assert_eq!(band.offset_hz, 1000 * 6_250),
            m => panic!("{m:?}"),
        }
        // Sentinel: raw offset 0x80 → no transmit offset, uplink == downlink
        // (sdrtrunk FrequencyBandUpdateVUHF.isTransmitOffsetValid).
        b.set_u64(25, 13, 0x80);
        match parse_tsbk(&b, 0).message {
            TsbkMessage::IdenUpdate(band) => {
                assert_eq!(band.offset_hz, 0);
                assert_eq!(band.uplink_hz(4), band.downlink_hz(4));
            }
            m => panic!("{m:?}"),
        }
        // 6.25 kHz bandwidth code, and an unknown code → 0.
        b.set_u64(20, 4, 0x4);
        match parse_tsbk(&b, 0).message {
            TsbkMessage::IdenUpdate(band) => assert_eq!(band.bandwidth_hz, 6_250),
            m => panic!("{m:?}"),
        }
        b.set_u64(20, 4, 0x7);
        match parse_tsbk(&b, 0).message {
            TsbkMessage::IdenUpdate(band) => assert_eq!(band.bandwidth_hz, 0),
            m => panic!("{m:?}"),
        }
    }

    #[test]
    fn iden_51_tdma_layout_and_sentinel() {
        let mut b = buf96();
        b.set_u64(2, 6, 51);
        b.set_u64(16, 4, 2); // band id
        b.set_u64(20, 4, 3); // channel type 3 → TDMA, 12.5 kHz, 2 slots
        b.set(24, true); // positive offset
        b.set_u64(25, 13, 360);
        b.set_u64(38, 10, 100); // 100 × 125 = 12 500 Hz spacing
        b.set_u64(48, 32, 170_201_250); // × 5 = 851_006_250 Hz
        match parse_tsbk(&b, 0).message {
            TsbkMessage::IdenUpdate(band) => {
                assert_eq!(band.id, 2);
                assert!(band.tdma);
                assert_eq!(band.slots_per_carrier, 2);
                assert_eq!(band.bandwidth_hz, 12_500);
                assert_eq!(band.spacing_hz, 12_500);
                assert_eq!(band.base_hz, 851_006_250);
                assert_eq!(band.offset_hz, 360 * 12_500);
                // TDMA channel math: two channels share a carrier.
                assert_eq!(band.downlink_hz(0), 851_006_250);
                assert_eq!(band.downlink_hz(1), 851_006_250);
                assert_eq!(band.downlink_hz(2), 851_018_750);
                assert_eq!(band.timeslot(1), 2);
            }
            m => panic!("{m:?}"),
        }
        // Sentinel: raw offset 0x80 → no transmit offset.
        b.set_u64(25, 13, 0x80);
        match parse_tsbk(&b, 0).message {
            TsbkMessage::IdenUpdate(band) => {
                assert_eq!(band.offset_hz, 0);
                assert_eq!(band.uplink_hz(2), band.downlink_hz(2));
            }
            m => panic!("{m:?}"),
        }
        // Channel type 0 is FDMA 12.5 kHz, 1 slot.
        b.set_u64(20, 4, 0);
        match parse_tsbk(&b, 0).message {
            TsbkMessage::IdenUpdate(band) => {
                assert!(!band.tdma);
                assert_eq!(band.slots_per_carrier, 1);
                assert_eq!(band.bandwidth_hz, 12_500);
            }
            m => panic!("{m:?}"),
        }
    }

    #[test]
    fn mfid_one_is_standard() {
        // sdrtrunk `Vendor.STANDARD_V1`: MFID 1 parses with the standard opcode
        // table, exactly like MFID 0 (and like `LinkControl::parse` does).
        let mut b = buf96();
        b.set_u64(2, 6, 0); // opcode 0 = group voice grant
        b.set_u64(8, 8, 1); // MFID 1
        b.set_u64(24, 4, 1);
        b.set_u64(28, 12, 456);
        b.set_u64(40, 16, 52101);
        b.set_u64(56, 24, 0x4F_5890);
        match parse_tsbk(&b, 0).message {
            TsbkMessage::GroupVoiceGrant {
                channel,
                talkgroup,
                source,
                ..
            } => {
                assert_eq!(
                    channel,
                    ChannelRef {
                        band: 1,
                        channel: 456
                    }
                );
                assert_eq!(talkgroup, 52101);
                assert_eq!(source, 0x4F_5890);
            }
            m => panic!("MFID 1 must parse as standard, got {m:?}"),
        }
        // An unrelated vendor still short-circuits to `Other`.
        b.set_u64(8, 8, 42);
        match parse_tsbk(&b, 0).message {
            TsbkMessage::Other { opcode, vendor } => {
                assert_eq!(opcode, 0);
                assert_eq!(vendor, 42);
            }
            m => panic!("{m:?}"),
        }
    }

    /// The reference fixture's 800 MHz band identifier (band 1), used to check that
    /// the site broadcasts' channel references resolve to the frequencies the
    /// reference decoder logs.
    fn ref_band1() -> IdenBand {
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

    #[test]
    fn service_class_bit_by_bit() {
        assert_eq!(ServiceClass::from_byte(0), ServiceClass::default());
        assert!(ServiceClass::from_byte(0x01).composite);
        assert!(ServiceClass::from_byte(0x02).no_service_requests);
        assert!(ServiceClass::from_byte(0x04).backup_control);
        assert!(ServiceClass::from_byte(0x10).data);
        assert!(ServiceClass::from_byte(0x20).voice);
        assert!(ServiceClass::from_byte(0x40).registration);
        assert!(ServiceClass::from_byte(0x80).authentication);
        // The raw byte is kept so reserved bits (0x08) survive decoding.
        let r = ServiceClass::from_byte(0x08);
        assert_eq!(r.raw, 0x08);
        assert_eq!(
            ServiceClass::from_byte(0x08),
            ServiceClass {
                raw: 0x08,
                ..ServiceClass::default()
            }
        );
        // [DATA, VOICE, REGISTRATION] — what the fixture's site advertises.
        let svc = ServiceClass::from_byte(0x70);
        assert!(svc.data && svc.voice && svc.registration);
        assert!(!svc.authentication && !svc.backup_control && !svc.composite);
    }

    #[test]
    fn secondary_control_channel_opcode57_layout() {
        // Ground truth (reference decode of the fixture capture):
        // SEC_CCH_BROADCST RFSS:10 SITE:1 CHAN A:1-638 [BACKUP CONTROL
        // CHANNEL] CHAN B:1-718 -> 858.9875 / 859.9875 MHz.
        let mut b = buf96();
        b.set_u64(2, 6, 57);
        b.set_u64(16, 8, 10); // RFSS
        b.set_u64(24, 8, 1); // SITE
        b.set_u64(32, 4, 1); // band A
        b.set_u64(36, 12, 638); // channel A
        b.set_u64(48, 8, 0x04); // service class A: backup control channel
        b.set_u64(56, 4, 1); // band B
        b.set_u64(60, 12, 718); // channel B
        b.set_u64(72, 8, 0x04); // service class B
        let band = ref_band1();
        match parse_tsbk(&b, 0).message {
            TsbkMessage::SecondaryControlChannel {
                rfss,
                site,
                channel_a,
                service_class_a,
                channel_b,
                service_class_b,
            } => {
                assert_eq!(rfss, 10);
                assert_eq!(site, 1);
                assert_eq!(
                    channel_a,
                    ChannelRef {
                        band: 1,
                        channel: 638
                    }
                );
                assert!(service_class_a.backup_control);
                assert_eq!(channel_b.unwrap().channel, 718);
                assert!(service_class_b.backup_control);
                assert_eq!(band.downlink_hz(channel_a.channel), 858_987_500);
                assert_eq!(band.downlink_hz(channel_b.unwrap().channel), 859_987_500);
            }
            m => panic!("{m:?}"),
        }
        // A zero band *and* channel means "no second channel".
        b.set_u64(56, 4, 0);
        b.set_u64(60, 12, 0);
        match parse_tsbk(&b, 0).message {
            TsbkMessage::SecondaryControlChannel { channel_b, .. } => assert!(channel_b.is_none()),
            m => panic!("{m:?}"),
        }
    }

    #[test]
    fn rfss_status_opcode58_layout() {
        // Ground truth: RFSS_STATUS_BCST SYSTEM:52 RFSS:10 SITE:1 LRA:10
        // CHAN 1-3 ACTIVE NETWORK CONNECTION [DATA, VOICE, REGISTRATION]
        // -> 851.050000 MHz, the live control channel.
        let mut b = buf96();
        b.set_u64(2, 6, 58);
        b.set_u64(16, 8, 10); // LRA
        b.set(27, true); // active network connection
        b.set_u64(28, 12, 52); // SYSTEM
        b.set_u64(40, 8, 10); // RFSS
        b.set_u64(48, 8, 1); // SITE
        b.set_u64(56, 4, 1); // band
        b.set_u64(60, 12, 3); // channel
        b.set_u64(72, 8, 0x70); // data + voice + registration
        match parse_tsbk(&b, 0).message {
            TsbkMessage::RfssStatus {
                lra,
                active_network_connection,
                system_id,
                rfss,
                site,
                channel,
                service_class,
            } => {
                assert_eq!(lra, 10);
                assert!(active_network_connection);
                assert_eq!(system_id, 52);
                assert_eq!(rfss, 10);
                assert_eq!(site, 1);
                assert_eq!(
                    channel,
                    ChannelRef {
                        band: 1,
                        channel: 3
                    }
                );
                assert!(service_class.data && service_class.voice && service_class.registration);
                assert_eq!(ref_band1().downlink_hz(channel.channel), 851_050_000);
            }
            m => panic!("{m:?}"),
        }
        // Bit 27 clear -> no active network connection (and it must not bleed
        // into SYSTEM, which starts at bit 28).
        b.set(27, false);
        match parse_tsbk(&b, 0).message {
            TsbkMessage::RfssStatus {
                active_network_connection,
                system_id,
                ..
            } => {
                assert!(!active_network_connection);
                assert_eq!(system_id, 52);
            }
            m => panic!("{m:?}"),
        }
    }

    #[test]
    fn network_status_opcode59_layout() {
        // Ground truth: NET_STATUS_BCAST WACN:284004 SYSTEM:52 LRA:10 CHAN:1-3.
        let mut b = buf96();
        b.set_u64(2, 6, 59);
        b.set_u64(16, 8, 10); // LRA
        b.set_u64(24, 20, 284_004); // WACN (20 bits)
        b.set_u64(44, 12, 52); // SYSTEM
        b.set_u64(56, 4, 1); // band
        b.set_u64(60, 12, 3); // channel
        b.set_u64(72, 8, 0x70);
        match parse_tsbk(&b, 0).message {
            TsbkMessage::NetworkStatus {
                lra,
                wacn,
                system_id,
                channel,
                service_class,
            } => {
                assert_eq!(lra, 10);
                assert_eq!(wacn, 284_004);
                assert_eq!(system_id, 52);
                assert_eq!(
                    channel,
                    ChannelRef {
                        band: 1,
                        channel: 3
                    }
                );
                assert_eq!(service_class.raw, 0x70);
                assert_eq!(ref_band1().downlink_hz(channel.channel), 851_050_000);
            }
            m => panic!("{m:?}"),
        }
        // The full 20-bit WACN range survives (no truncation to 16 bits).
        b.set_u64(24, 20, 0xF_FFFF);
        match parse_tsbk(&b, 0).message {
            TsbkMessage::NetworkStatus {
                wacn, system_id, ..
            } => {
                assert_eq!(wacn, 0xF_FFFF);
                assert_eq!(system_id, 52);
            }
            m => panic!("{m:?}"),
        }
    }

    #[test]
    fn adjacent_status_opcode60_layout() {
        // Ground truth: ADJ SITE STATUS SYSTEM:57 LRA:20 RFSS:20 SITE:20
        // CHANNEL:1-6 [VALID INFORMATION, ACTIVE RFSS CONNECTION].
        let mut b = buf96();
        b.set_u64(2, 6, 60);
        b.set_u64(16, 8, 20); // LRA
        b.set(26, true); // valid information
        b.set(27, true); // active RFSS connection
        b.set_u64(28, 12, 57); // SYSTEM
        b.set_u64(40, 8, 20); // RFSS
        b.set_u64(48, 8, 20); // SITE
        b.set_u64(56, 4, 1); // band
        b.set_u64(60, 12, 6); // channel
        b.set_u64(72, 8, 0xF0); // data + voice + registration + authentication
        match parse_tsbk(&b, 0).message {
            TsbkMessage::AdjacentStatus {
                lra,
                flags,
                system_id,
                rfss,
                site,
                channel,
                service_class,
            } => {
                assert_eq!(lra, 20);
                assert!(!flags.conventional);
                assert!(!flags.site_failure);
                assert!(flags.valid_information);
                assert!(flags.active_rfss_connection);
                assert_eq!(system_id, 57);
                assert_eq!(rfss, 20);
                assert_eq!(site, 20);
                assert_eq!(
                    channel,
                    ChannelRef {
                        band: 1,
                        channel: 6
                    }
                );
                assert!(service_class.authentication);
                assert_eq!(ref_band1().downlink_hz(channel.channel), 851_087_500);
            }
            m => panic!("{m:?}"),
        }
        // Each flag is its own bit and none leaks into SYSTEM (bit 28+).
        b.set(24, true);
        b.set(25, true);
        match parse_tsbk(&b, 0).message {
            TsbkMessage::AdjacentStatus {
                flags, system_id, ..
            } => {
                assert!(flags.conventional && flags.site_failure);
                assert_eq!(system_id, 57);
            }
            m => panic!("{m:?}"),
        }
    }

    #[test]
    fn site_broadcasts_are_vendor_gated() {
        // A non-standard MFID must not be parsed with the standard opcode
        // table, exactly like the grant opcodes.
        let mut b = buf96();
        b.set_u64(2, 6, 58);
        b.set_u64(8, 8, 42);
        match parse_tsbk(&b, 0).message {
            TsbkMessage::Other { opcode, vendor } => {
                assert_eq!((opcode, vendor), (58, 42));
            }
            m => panic!("{m:?}"),
        }
    }

    #[test]
    fn harris_opcode48_and_generic() {
        let mut b = buf96();
        b.set_u64(8, 8, u64::from(MFID_HARRIS)); // vendor 164
        b.set_u64(2, 6, 48); // opcode 0x30
        b.set_u64(16, 8, 0x00); // regroup options: group flag (0x40) clear → radio target
        b.set_u64(24, 16, 0x1234); // supergroup
        b.set_u64(40, 16, 0x5678); // key id
        b.set_u64(56, 24, 0x0ABBCC); // target radio
        match parse_tsbk(&b, 0).message {
            TsbkMessage::HarrisGroupRegroup {
                regroup_options,
                supergroup,
                key_id,
                target,
                target_is_talkgroup,
            } => {
                assert_eq!(regroup_options, 0x00);
                assert_eq!(supergroup, 0x1234);
                assert_eq!(key_id, 0x5678);
                assert!(!target_is_talkgroup);
                assert_eq!(target, 0x0ABBCC);
            }
            m => panic!("{m:?}"),
        }

        // Group flag (0x40, absolute bit 17) set → talkgroup target (16-bit at 64–79).
        b.set_u64(16, 8, 0x40);
        b.set_u64(64, 16, 0x9ABC);
        match parse_tsbk(&b, 0).message {
            TsbkMessage::HarrisGroupRegroup {
                target,
                target_is_talkgroup,
                ..
            } => {
                assert!(target_is_talkgroup);
                assert_eq!(target, 0x9ABC);
            }
            m => panic!("{m:?}"),
        }

        // Any other Harris opcode is generic, not mis-binned as standard.
        b.set_u64(16, 8, 0);
        b.set_u64(2, 6, 0); // opcode 0 would be a group-voice grant if standard
        match parse_tsbk(&b, 0).message {
            TsbkMessage::HarrisOsp { opcode } => assert_eq!(opcode, 0),
            m => panic!("standard opcode misfired on vendor TSBK: {m:?}"),
        }
    }

    /// Load a 12-byte hex TSBK (as printed in the raw event log's `hex` field)
    /// into a bit buffer, MSB-first.
    fn tsbk_from_hex(hex: &str) -> BitBuf {
        let mut b = buf96();
        let bytes: Vec<u8> = (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect();
        assert_eq!(bytes.len(), 12, "a TSBK is 12 octets");
        for (i, byte) in bytes.iter().enumerate() {
            b.set_u64(i * 8, 8, u64::from(*byte));
        }
        b
    }

    #[test]
    fn system_service_broadcast_decodes_the_live_exemplar() {
        // The one and only opcode-56 payload of a captured control channel
        // (627,474 of 627,478 in five days of event logs; the other four
        // are one-off CRC survivors). Layout from sdrtrunk
        // `tsbk/standard/osp/SystemServiceBroadcast.java`: available 24-47,
        // supported 48-71, request priority 72-79.
        let t = parse_tsbk(&tsbk_from_hex("b800040c2c680c2c6801aa30"), 0);
        assert_eq!(t.opcode, 56);
        assert_eq!(t.name(), "SYS_SVC_BCAST");
        match t.message {
            TsbkMessage::SystemServiceBroadcast {
                available,
                supported,
                priority,
            } => {
                assert_eq!(available.raw, 0x0C_2C68);
                assert_eq!(supported.raw, 0x0C_2C68);
                assert_eq!(priority, 1);
                assert_eq!(
                    available.names(),
                    vec![
                        "group_voice",
                        "individual_voice",
                        "individual_data",
                        "unit_registration",
                        "group_affiliation",
                        "user_status",
                        "user_message",
                        "user_status_query",
                    ]
                );
                // Bits the site does *not* advertise, so a future change is
                // visible: no telephone interconnect, no group data, no
                // authentication, no emergency alarm.
                assert!(!available.has(0x02_0000));
                assert!(!available.has(0x00_4000));
                assert!(!available.has(0x00_0100));
                assert!(!available.has(0x00_0001));
            }
            m => panic!("{m:?}"),
        }
    }

    #[test]
    fn system_service_broadcast_fields_are_independent() {
        // No second distinct real payload exists in the archive (the site
        // transmits one constant value), so the field boundaries are pinned
        // synthetically: three disjoint patterns must land in three fields.
        let t = parse_tsbk(&tsbk_from_hex("b800ffaaaaaa5555557f0000"), 0);
        match t.message {
            TsbkMessage::SystemServiceBroadcast {
                available,
                supported,
                priority,
            } => {
                assert_eq!(available.raw, 0xAA_AAAA);
                assert_eq!(supported.raw, 0x55_5555);
                assert_eq!(priority, 0x7F);
            }
            m => panic!("{m:?}"),
        }
    }

    #[test]
    fn extended_function_command_decodes_the_live_shape() {
        // A radio detach shaped as seen on a control channel (target synthetic):
        // function 0x007d, arguments 0xfffffd (the system controller), target 9_000_008.
        let t = parse_tsbk(&tsbk_from_hex("a400007dfffffd895448e73f"), 0);
        assert_eq!(t.opcode, 36);
        assert_eq!(t.name(), "EXTNDED_FUNC_CMD");
        match t.message {
            TsbkMessage::ExtendedFunctionCommand {
                function,
                arguments,
                target,
            } => {
                assert_eq!(function.code(), 0x007D);
                assert_eq!(arguments, 0xFF_FFFD);
                assert_eq!(target, 0x89_5448);
            }
            m => panic!("{m:?}"),
        }
    }

    #[test]
    fn harris_osp_names_carry_their_opcode() {
        // The two constant L3Harris beacons of a busy site stay unparsed but
        // are catalogued: each names its own opcode.
        assert_eq!(
            parse_tsbk(&tsbk_from_hex("81a4110dfffffffffffff79d"), 0).name(),
            "HARRIS OSP 1"
        );
        assert_eq!(
            parse_tsbk(&tsbk_from_hex("8fa40a57000a00000000edda"), 0).name(),
            "HARRIS OSP 15"
        );
        assert_eq!(harris_osp_name(63), "HARRIS OSP 63");
        // Masked to six bits, so every input names something.
        assert_eq!(harris_osp_name(0xFF), harris_osp_name(63));
    }

    #[test]
    fn harris_regroup_decodes_captured_shapes() {
        // An activate shaped as captured off the air (member talkgroup synthetic):
        // patch supergroup 0x36b1 gains member talkgroup 0x06aa.
        match parse_tsbk(&tsbk_from_hex("b0a4ee36b100008006aa1f21"), 0).message {
            TsbkMessage::HarrisGroupRegroup {
                regroup_options,
                supergroup,
                target,
                target_is_talkgroup,
                ..
            } => {
                assert_eq!(regroup_options, 0xee);
                assert_eq!(supergroup, 0x36b1);
                assert!(target_is_talkgroup);
                assert_eq!(target, 0x06aa);
                assert!(regroup::is_activate(regroup_options));
                assert!(regroup::is_patch(regroup_options));
            }
            m => panic!("{m:?}"),
        }

        // Its cancel counterpart: activate bit clear, target zeroed. The vendor
        // also clears the talkgroup-target flag on a cancel, so the tracker must
        // key a teardown on the supergroup alone.
        match parse_tsbk(&tsbk_from_hex("b0a48036b1000000000089d4"), 0).message {
            TsbkMessage::HarrisGroupRegroup {
                regroup_options,
                supergroup,
                target,
                ..
            } => {
                assert_eq!(regroup_options, 0x80);
                assert_eq!(supergroup, 0x36b1);
                assert_eq!(target, 0);
                assert!(!regroup::is_activate(regroup_options));
            }
            m => panic!("{m:?}"),
        }

        // A simulselect (one-way) activate: 0x64 has the patch bit clear.
        match parse_tsbk(&tsbk_from_hex("b0a464fcfa00008006ac3ff3"), 0).message {
            TsbkMessage::HarrisGroupRegroup {
                regroup_options,
                supergroup,
                target,
                ..
            } => {
                assert_eq!(supergroup, 0xfcfa);
                assert_eq!(target, 0x06ac);
                assert!(regroup::is_activate(regroup_options));
                assert!(!regroup::is_patch(regroup_options));
            }
            m => panic!("{m:?}"),
        }
    }
}
