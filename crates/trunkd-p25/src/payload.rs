// SPDX-License-Identifier: GPL-3.0-or-later
//! Voice-path message payload field layouts (spec §18).
//!
//! These parse the error-corrected words produced by the LDU/HDU pipelines in
//! [`crate::frames`] into typed values: the LDU1 Link Control word, the LDU2
//! Encryption Sync word, and the HDU header. All fields are MSB-first (§1) and
//! bit indices are absolute within the error-corrected container, inclusive.

use crate::bits::bits_msb_first;
use crate::message::{ChannelRef, ServiceOptions};

/// Manufacturer id (MFID) for standard (non-vendor) messages.
pub const MFID_STANDARD: u8 = 0;
/// Manufacturer id (MFID) 1 — also "standard" (sdrtrunk `Vendor.STANDARD_V1`).
pub const MFID_STANDARD_V1: u8 = 1;
/// Manufacturer id (MFID) for L3Harris.
pub const MFID_HARRIS: u8 = 164;

/// True when `vendor` denotes a standard (non-vendor) message: MFID 0
/// (STANDARD) or MFID 1 (STANDARD_V1). Both parse with the standard opcode
/// tables (sdrtrunk `Vendor`). Used by the TSBK parser and the Link Control
/// parser alike so one MFID is never "standard" in one place and vendor in the
/// other.
#[inline]
pub fn is_standard_vendor(vendor: u8) -> bool {
    vendor == MFID_STANDARD || vendor == MFID_STANDARD_V1
}

/// Read a big-endian (MSB-first) bit field `[start, start+width)` from a byte
/// slice. Bit 0 is the MSB of byte 0 (§1). `width` ≤ 32. A `u32` view of the
/// crate's single MSB-first reader, [`bits_msb_first`].
#[inline]
fn field(bytes: &[u8], start: usize, width: usize) -> u32 {
    bits_msb_first(bytes, start, width) as u32
}

/// Named L3Harris (MFID 164) Link Control opcodes (spec §18.1). Payloads are
/// emitted raw rather than fully parsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HarrisLcOpcode {
    /// LCO 10 — return to control channel.
    ReturnToControlChannel,
    /// LCO 42 — talker GPS block 1.
    TalkerGps1,
    /// LCO 43 — talker GPS block 2.
    TalkerGps2,
    /// LCO 50 — talker alias block 1.
    TalkerAlias1,
    /// LCO 51 — talker alias block 2.
    TalkerAlias2,
    /// LCO 52 — talker alias block 3.
    TalkerAlias3,
    /// LCO 53 — talker alias block 4.
    TalkerAlias4,
    /// Any other Harris LC opcode (raw payload only).
    Unknown(u8),
}

impl HarrisLcOpcode {
    fn from_lco(lco: u8) -> Self {
        match lco {
            10 => HarrisLcOpcode::ReturnToControlChannel,
            42 => HarrisLcOpcode::TalkerGps1,
            43 => HarrisLcOpcode::TalkerGps2,
            50 => HarrisLcOpcode::TalkerAlias1,
            51 => HarrisLcOpcode::TalkerAlias2,
            52 => HarrisLcOpcode::TalkerAlias3,
            53 => HarrisLcOpcode::TalkerAlias4,
            other => HarrisLcOpcode::Unknown(other),
        }
    }
}

/// Extended-function command code (LC opcode 23 / TSBK, sdrtrunk
/// `ExtendedFunction`). Only the codes the reference decoder names are
/// enumerated; anything else keeps its raw 16-bit value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtendedFunction {
    /// `0x0000` — radio check: is the target on the air?
    RadioCheck,
    /// `0x007D` — radio detach (Harris "network detach"): drop the target's
    /// registration/affiliation.
    RadioDetach,
    /// `0x007F` — radio inhibit: administratively disable the target.
    RadioInhibit,
    /// `0x00FF` — radio uninhibit: re-enable a previously inhibited target.
    RadioUninhibit,
    /// Any other extended-function code (raw value).
    Unknown(u16),
}

impl ExtendedFunction {
    /// Classify a 16-bit extended-function code.
    pub fn from_code(code: u16) -> Self {
        match code {
            0x0000 => ExtendedFunction::RadioCheck,
            0x007D => ExtendedFunction::RadioDetach,
            0x007F => ExtendedFunction::RadioInhibit,
            0x00FF => ExtendedFunction::RadioUninhibit,
            other => ExtendedFunction::Unknown(other),
        }
    }

    /// The raw 16-bit code.
    pub fn code(self) -> u16 {
        match self {
            ExtendedFunction::RadioCheck => 0x0000,
            ExtendedFunction::RadioDetach => 0x007D,
            ExtendedFunction::RadioInhibit => 0x007F,
            ExtendedFunction::RadioUninhibit => 0x00FF,
            ExtendedFunction::Unknown(n) => n,
        }
    }

    /// A stable snake_case name for the event stream; unknown codes render as
    /// `"unknown"`.
    pub fn name(self) -> &'static str {
        match self {
            ExtendedFunction::RadioCheck => "radio_check",
            ExtendedFunction::RadioDetach => "radio_detach",
            ExtendedFunction::RadioInhibit => "radio_inhibit",
            ExtendedFunction::RadioUninhibit => "radio_uninhibit",
            ExtendedFunction::Unknown(_) => "unknown",
        }
    }
}

/// A parsed 72-bit LDU1 / TDULC Link Control word (spec §18.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkControl {
    /// LCO 0 — Group Voice Channel User (carries TG + radio on a group call).
    GroupVoiceChannelUser {
        /// Service options (bits 16–23).
        options: ServiceOptions,
        /// Group address / talkgroup (bits 32–47).
        talkgroup: u16,
        /// Source-address extension flag (bit 31).
        source_ext_flag: bool,
        /// Source radio address (bits 48–71).
        source: u32,
    },
    /// LCO 3 — Unit-to-Unit Voice Channel User.
    UnitToUnitVoiceChannelUser {
        /// Service options (bits 16–23).
        options: ServiceOptions,
        /// Target address (bits 24–47).
        target: u32,
        /// Source address (bits 48–71).
        source: u32,
    },
    /// LCO 5 — Unit-to-Unit Answer Request: the network paging `target`
    /// because `source` wants a private call with it (sdrtrunk
    /// `LCUnitToUnitAnswerRequest`). Sent on the traffic channels so a radio
    /// busy on a call hears the page; it is not itself a call, and names no
    /// talker. Same layout as LCO 3.
    ///
    /// Example (synthetic IDs): `450000895441895442` → target = 0x895441, source = 0x895442.
    /// On a busy site it comes in bursts of a few minutes across most traffic channels, with
    /// the pair in both orders.
    UnitToUnitAnswerRequest {
        /// Service options (bits 16–23).
        options: ServiceOptions,
        /// The radio being paged (bits 24–47).
        target: u32,
        /// The radio asking for the call (bits 48–71).
        source: u32,
    },
    /// LCO 6 — Telephone Interconnect Voice Channel User.
    TelephoneInterconnectVoiceChannelUser {
        /// Service options (bits 16–23).
        options: ServiceOptions,
        /// Call timer (bits 32–47, ×100 ms).
        call_timer: u16,
        /// Landline-side talkgroup / address (bits 48–71).
        address: u32,
    },
    /// LCO 9 — Source ID Extension: the fully qualified SUID of the current
    /// talker (sdrtrunk `LCSourceIDExtension`).
    ///
    /// Sent alongside the Group Voice Channel User word when the talker is a
    /// roaming subscriber from another WACN/system: LCO 0 carries only the
    /// 24-bit radio id, which is ambiguous across systems, and this word
    /// qualifies it.
    SourceIdExtension {
        /// 20-bit wide-area communications network id (bits 16–35).
        wacn: u32,
        /// 12-bit system id (bits 36–47).
        system: u16,
        /// 24-bit source radio address (bits 48–71).
        source: u32,
    },
    /// LCO 2 — Group Voice Channel Update: one or two (channel, talkgroup)
    /// pairs announcing which talkgroups are up on which channels. The
    /// voice-path twin of the TSBK `GroupVoiceGrantUpdate` (opcode 2), with the
    /// same one-or-two-slot shape (sdrtrunk `LCGroupVoiceChannelUpdate`).
    ///
    /// Layout: `FREQ_BAND_A`(4)@8, `CHANNEL_A`(12)@12, `TALKGROUP_A`(16)@24,
    /// `FREQ_BAND_B`(4)@40, `CHANNEL_B`(12)@44, `TALKGROUP_B`(16)@56.
    ///
    /// Example (synthetic talkgroup): `4211c806a511c806a5` → band A = 1, channel A = 456,
    /// TG A = 0x06a5 (1701); slot B duplicates A.
    GroupVoiceChannelUpdate {
        /// Slot A: (channel, talkgroup).
        a: (ChannelRef, u16),
        /// Slot B: only meaningful when its talkgroup is non-zero and differs
        /// from slot A (slot B commonly duplicates A).
        b: Option<(ChannelRef, u16)>,
    },
    /// LCO 15 — Call Termination / Cancellation: the network telling the
    /// traffic channel the call is over (sdrtrunk `LCCallTermination`). Also
    /// carried as the final TDULC of a call.
    ///
    /// Layout: `ADDRESS`(24)@48 = the source radio being released. An `address`
    /// of 0 is the Harris network-commanded-teardown sentinel — see
    /// [`LinkControl::is_network_commanded_teardown`].
    ///
    /// Example (synthetic radio, TDULC): `4f0000000000895443` → address = 0x895443
    /// (9000003); teardown sentinel: `4f0000000000000000` → address = 0.
    CallTermination {
        /// Source radio being released; 0 = network-commanded teardown.
        source: u32,
    },
    /// LCO 16 — Group Affiliation Query: the network (or a radio) asking which
    /// talkgroup a radio is affiliated to (sdrtrunk `LCGroupAffiliationQuery`).
    /// The voice-path twin of the TSBK `GroupAffiliationQuery` (opcode 42).
    ///
    /// Layout: `TARGET_ADDRESS`(24)@24 = radio being queried,
    /// `SOURCE_ADDRESS`(24)@48 = who is asking (`0xFFFFFD` = system controller).
    ///
    /// Example (synthetic radio): `500000895444fffffd` → target = 0x895444 (9000004),
    /// source = 0xfffffd (system controller).
    GroupAffiliationQuery {
        /// The radio being queried.
        target: u32,
        /// The FNE / radio asking (`0xFFFFFD` = system controller).
        source: u32,
    },
    /// LCO 23 — Extended Function Command: an administrative command to a radio
    /// (radio check / detach / inhibit / uninhibit — sdrtrunk
    /// `LCExtendedFunctionCommand`).
    ///
    /// Layout: `EXTENDED_FUNCTION`(16)@8, `ARGUMENTS`(24)@24,
    /// `TARGET_ADDRESS`(24)@48 = the radio commanded.
    ///
    /// Example (synthetic radio): `57007dfffffd895445` → function = 0x007d (radio detach),
    /// arguments = 0xfffffd, target = 0x895445 (9000005).
    ExtendedFunctionCommand {
        /// The command.
        function: ExtendedFunction,
        /// Command arguments (function-specific; often `0xFFFFFD`).
        arguments: u32,
        /// The radio being commanded.
        target: u32,
    },
    /// L3Harris vendor LC (MFID 164): named opcode, raw 72-bit payload.
    Harris {
        /// Named Harris LC opcode.
        opcode: HarrisLcOpcode,
        /// Raw 72-bit LC word (9 bytes, MSB-first).
        word: [u8; 9],
    },
    /// Any other LC: an unhandled standard LCO, or a non-Harris vendor LC.
    Other {
        /// Encryption flag (bit 0).
        encrypted: bool,
        /// Standard/vendor flag (bit 1): true = standard format.
        standard: bool,
        /// Link Control Opcode (bits 2–7).
        lco: u8,
        /// Manufacturer id (bits 8–15; meaningful only when `!standard`).
        vendor: u8,
        /// Raw 72-bit LC word (9 bytes, MSB-first).
        word: [u8; 9],
    },
}

impl LinkControl {
    /// Parse a 72-bit Link Control word (9 bytes, MSB-first).
    pub fn parse(word: &[u8; 9]) -> LinkControl {
        let encrypted = field(word, 0, 1) != 0;
        // Bit 1 is sdrtrunk's "standard-vendor-format" flag: when set, the MFID
        // is implied STANDARD; when clear, it is read from octet 1. Either way
        // the *vendor* — not this flag — selects how to parse (matches
        // sdrtrunk `LinkControlWord.getVendor` + `LinkControlWordFactory`). In
        // practice standard LCs are sent with the flag clear and octet 1 = 0x00,
        // so gating on the flag drops them; gate on the vendor instead.
        let standard_flag = field(word, 1, 1) != 0;
        let lco = field(word, 2, 6) as u8;
        let vendor = if standard_flag {
            MFID_STANDARD
        } else {
            field(word, 8, 8) as u8
        };
        // MFID 0 and 1 are both "standard" (STANDARD / STANDARD_V1).
        let standard_vendor = is_standard_vendor(vendor);

        if vendor == MFID_HARRIS {
            return LinkControl::Harris {
                opcode: HarrisLcOpcode::from_lco(lco),
                word: *word,
            };
        }

        if standard_vendor {
            match lco {
                0 => LinkControl::GroupVoiceChannelUser {
                    options: ServiceOptions::from_byte(field(word, 16, 8) as u8),
                    source_ext_flag: field(word, 31, 1) != 0,
                    talkgroup: field(word, 32, 16) as u16,
                    source: field(word, 48, 24),
                },
                // Mirrors the TSBK `GroupVoiceGrantUpdate` (opcode 2): two
                // (channel, talkgroup) slots, slot B valid only when its TG is
                // non-zero and differs from slot A.
                2 => {
                    let a = (
                        ChannelRef {
                            band: field(word, 8, 4) as u8,
                            channel: field(word, 12, 12) as u16,
                        },
                        field(word, 24, 16) as u16,
                    );
                    let bpair = (
                        ChannelRef {
                            band: field(word, 40, 4) as u8,
                            channel: field(word, 44, 12) as u16,
                        },
                        field(word, 56, 16) as u16,
                    );
                    let b_valid = bpair.1 != 0 && bpair.1 != a.1;
                    LinkControl::GroupVoiceChannelUpdate {
                        a,
                        b: b_valid.then_some(bpair),
                    }
                }
                3 => LinkControl::UnitToUnitVoiceChannelUser {
                    options: ServiceOptions::from_byte(field(word, 16, 8) as u8),
                    target: field(word, 24, 24),
                    source: field(word, 48, 24),
                },
                5 => LinkControl::UnitToUnitAnswerRequest {
                    options: ServiceOptions::from_byte(field(word, 16, 8) as u8),
                    target: field(word, 24, 24),
                    source: field(word, 48, 24),
                },
                6 => LinkControl::TelephoneInterconnectVoiceChannelUser {
                    options: ServiceOptions::from_byte(field(word, 16, 8) as u8),
                    call_timer: field(word, 32, 16) as u16,
                    address: field(word, 48, 24),
                },
                // sdrtrunk `LCSourceIDExtension`: WACN 16–35, system 36–47,
                // radio 48–71.
                9 => LinkControl::SourceIdExtension {
                    wacn: field(word, 16, 20),
                    system: field(word, 36, 12) as u16,
                    source: field(word, 48, 24),
                },
                // LCO 15 — call termination: source radio at 48–71, 0 =
                // network-commanded teardown (sdrtrunk `LCCallTermination`).
                15 => LinkControl::CallTermination {
                    source: field(word, 48, 24),
                },
                // LCO 16 — group affiliation query: target 24–47, source 48–71.
                16 => LinkControl::GroupAffiliationQuery {
                    target: field(word, 24, 24),
                    source: field(word, 48, 24),
                },
                // LCO 23 — extended function command: function 8–23,
                // arguments 24–47, target 48–71.
                23 => LinkControl::ExtendedFunctionCommand {
                    function: ExtendedFunction::from_code(field(word, 8, 16) as u16),
                    arguments: field(word, 24, 24),
                    target: field(word, 48, 24),
                },
                _ => LinkControl::Other {
                    encrypted,
                    standard: standard_flag,
                    lco,
                    vendor,
                    word: *word,
                },
            }
        } else {
            LinkControl::Other {
                encrypted,
                standard: standard_flag,
                lco,
                vendor,
                word: *word,
            }
        }
    }

    /// True when this is a [`LinkControl::CallTermination`] with a zero source
    /// address — the Harris network-commanded-teardown sentinel (sdrtrunk's
    /// `isNetworkCommandedTeardown`). `false` for every other word, including a
    /// call termination that names a real radio.
    pub fn is_network_commanded_teardown(&self) -> bool {
        matches!(self, LinkControl::CallTermination { source: 0 })
    }
}

/// A decoded L3Harris talker GPS report, assembled from LC opcodes 42 and 43.
///
/// Layout is sdrtrunk's `L3HarrisGPS`, which the two Phase 1 block classes
/// (`LCHarrisTalkerGPSBlock1` / `Block2`) feed through
/// `LCHarrisTalkerGPSComplete`: each block contributes its 56 payload bits
/// (its bits 16–71, i.e. octets 2–8), block 1 first, giving one 112-bit word.
///
/// **Not observed on the measured system.** Neither wideband fixture nor the
/// narrowband one carried a single GPS block, so this parser is written from
/// the reference layout and exercised only by synthetic words — treat a live
/// decode as unverified until a capture shows the blocks. It is also the most
/// sensitive thing this crate can produce: live subscriber location.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HarrisGps {
    /// Latitude in signed decimal degrees (south negative).
    pub latitude: f64,
    /// Longitude in signed decimal degrees (west negative).
    pub longitude: f64,
    /// Seconds since UTC midnight.
    ///
    /// The field is 16 bits but a day is 86 400 seconds, so the reference
    /// decoder doubles the value when bit 80 is set. That reconstruction is
    /// sdrtrunk's stated guess at the missing 17th bit, not a spec rule.
    pub time_of_day_s: u32,
    /// Heading, 9 bits as sdrtrunk reads it. Its own source calls this
    /// "may not be accurate"; it is surfaced raw and unscaled.
    pub heading: u16,
}

/// Assemble and decode a talker GPS report from L3Harris LC blocks 1 and 2.
///
/// Both arguments are whole 72-bit LC words (9 bytes each), in block order.
/// The caller is responsible for pairing blocks from the same talker: nothing
/// in the words themselves says which report a block belongs to.
pub fn decode_harris_gps(block1: &[u8; 9], block2: &[u8; 9]) -> HarrisGps {
    // 112 bits = each block's payload (bits 16–71) concatenated.
    let mut m = [0u8; 14];
    let mut load = |dst: usize, src: &[u8; 9]| {
        for i in 0..56 {
            if bits_msb_first(src, 16 + i, 1) != 0 {
                m[(dst + i) / 8] |= 0x80 >> ((dst + i) % 8);
            }
        }
    };
    load(0, block1);
    load(56, block2);

    // Degrees + (minutes + fraction/10000) / 60; the hemisphere bit negates.
    let coord = |deg_at: usize, min_at: usize, frac_at: usize, hemi_at: usize| -> f64 {
        let deg = field(&m, deg_at, 8) as f64;
        let minutes = field(&m, min_at, 7) as f64;
        let frac = field(&m, frac_at, 16) as f64;
        let v = deg + (minutes + frac / 10_000.0) / 60.0;
        if field(&m, hemi_at, 1) != 0 {
            -v
        } else {
            v
        }
    };

    HarrisGps {
        latitude: coord(24, 17, 0, 16),
        longitude: coord(56, 49, 32, 48),
        time_of_day_s: field(&m, 64, 16) * if field(&m, 80, 1) != 0 { 2 } else { 1 },
        heading: field(&m, 95, 9) as u16,
    }
}

/// Single source of truth for "is this call clear?" (spec §18.2).
///
/// The rule, shared by [`EncryptionSync::is_clear`], [`HduHeader::is_clear`]
/// and [`crate::call::CallAssembler`] so the three can never disagree:
///
/// * `algid == 0x80` (UNENCRYPTED) ⇒ clear, whatever the KID/MI say.
/// * an all-zero encryption word (`algid == 0`, `kid == 0`, `mi` all zero) ⇒
///   clear: that is "no encryption parameters were sent", not a cipher.
/// * anything else ⇒ encrypted. In particular `algid == 0x00` with a non-zero
///   KID or MI is the ACCORDION 3 cipher, **not** clear.
pub fn is_clear_params(algid: u8, kid: u16, mi: &[u8; 9]) -> bool {
    algid == ALGID_UNENCRYPTED || (algid == 0 && kid == 0 && mi.iter().all(|&b| b == 0))
}

/// P25 ALGID for an unencrypted (clear) call (TIA-102, sdrtrunk `Encryption`).
/// Note this is `0x80`, NOT `0x00` — `0x00` is the ACCORDION_3 cipher.
pub const ALGID_UNENCRYPTED: u8 = 0x80;

/// Human-readable name for a P25 encryption ALGID (common values; unknown
/// values render as `"ALG 0x??"`). Mirrors sdrtrunk's `Encryption` enum.
pub fn algorithm_name(algid: u8) -> &'static str {
    match algid {
        0x00 => "ACCORDION 3",
        0x01 => "BATON AUTO EVEN",
        0x02 => "FIREFLY",
        0x03 => "MAYFLY",
        0x04 => "SAVILLE",
        0x41 => "BATON AUTO ODD",
        0x80 => "UNENCRYPTED",
        0x81 => "DES-OFB",
        0x83 => "3-KEY TRIPLE DES",
        0x84 => "AES-256",
        0x85 => "AES-128",
        0x9F => "DES-XL",
        0xAA => "ADP (RC4)",
        0xAB => "CFX-256",
        _ => "UNKNOWN",
    }
}

/// A parsed 96-bit LDU2 Encryption Sync Parameters word (spec §18.2). No LCO
/// header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncryptionSync {
    /// Message indicator (bits 0–71, 9 octets).
    pub mi: [u8; 9],
    /// Algorithm id (bits 72–79).
    pub algid: u8,
    /// Key id (bits 80–95).
    pub kid: u16,
}

impl EncryptionSync {
    /// Parse a 96-bit Encryption Sync word (12 bytes, MSB-first).
    pub fn parse(word: &[u8; 12]) -> EncryptionSync {
        let mut mi = [0u8; 9];
        mi.copy_from_slice(&word[0..9]);
        EncryptionSync {
            mi,
            algid: word[9],
            kid: (u16::from(word[10]) << 8) | u16::from(word[11]),
        }
    }

    /// True when the call is unencrypted — see [`is_clear_params`], the shared
    /// rule: `ALGID == 0x80` (UNENCRYPTED, spec §18.2), or an all-zero word (no
    /// ES sent). `ALGID == 0x00` with a non-zero KID/MI is the ACCORDION 3
    /// cipher, so it is NOT clear.
    pub fn is_clear(&self) -> bool {
        is_clear_params(self.algid, self.kid, &self.mi)
    }

    /// Encryption algorithm name (see [`algorithm_name`]).
    pub fn algorithm(&self) -> &'static str {
        algorithm_name(self.algid)
    }
}

/// A parsed 120-bit HDU header (spec §18.3). No LCO header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HduHeader {
    /// Message indicator MI = A‖B (bits 0–71, 9 octets).
    pub mi: [u8; 9],
    /// Manufacturer id (bits 72–79).
    pub mfid: u8,
    /// Algorithm id (bits 80–87).
    pub algid: u8,
    /// Key id (bits 88–103).
    pub kid: u16,
    /// Talkgroup id (bits 104–119).
    pub talkgroup: u16,
}

impl HduHeader {
    /// Parse a 120-bit HDU header (15 bytes, MSB-first).
    pub fn parse(data: &[u8; 15]) -> HduHeader {
        let mut mi = [0u8; 9];
        mi.copy_from_slice(&data[0..9]);
        HduHeader {
            mi,
            mfid: data[9],
            algid: data[10],
            kid: (u16::from(data[11]) << 8) | u16::from(data[12]),
            talkgroup: (u16::from(data[13]) << 8) | u16::from(data[14]),
        }
    }

    /// True when the call is unencrypted (`ALGID == 0x80`, or an all-zero
    /// header). Same rule as [`EncryptionSync::is_clear`] — both defer to
    /// [`is_clear_params`].
    pub fn is_clear(&self) -> bool {
        is_clear_params(self.algid, self.kid, &self.mi)
    }

    /// Encryption algorithm name (see [`algorithm_name`]).
    pub fn algorithm(&self) -> &'static str {
        algorithm_name(self.algid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Write `value` into `word` as an MSB-first field at `[start, start+width)`.
    fn put(word: &mut [u8; 9], start: usize, width: usize, value: u64) {
        for k in 0..width {
            if (value >> (width - 1 - k)) & 1 != 0 {
                word[(start + k) / 8] |= 0x80 >> ((start + k) % 8);
            }
        }
    }

    #[test]
    fn source_id_extension_decodes_a_fully_qualified_suid() {
        // LCO 9, standard vendor: WACN 16–35, system 36–47, radio 48–71.
        let mut w = [0u8; 9];
        put(&mut w, 2, 6, 9);
        put(&mut w, 16, 20, 284_004);
        put(&mut w, 36, 12, 57);
        put(&mut w, 48, 24, 9_100_025);
        assert_eq!(
            LinkControl::parse(&w),
            LinkControl::SourceIdExtension {
                wacn: 284_004,
                system: 57,
                source: 9_100_025,
            }
        );
    }

    #[test]
    fn a_harris_lco_9_is_not_a_source_id_extension() {
        // The vendor, not the opcode, selects the table: MFID 164 keeps LCO 9
        // as a raw Harris word.
        let mut w = [0u8; 9];
        put(&mut w, 2, 6, 9);
        put(&mut w, 8, 8, u64::from(MFID_HARRIS));
        match LinkControl::parse(&w) {
            LinkControl::Harris { opcode, .. } => {
                assert_eq!(opcode, HarrisLcOpcode::Unknown(9));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn harris_talker_gps_assembles_two_blocks() {
        // Assembled bit i of the 112-bit word is bit 16+i of block 1 for
        // i < 56, and bit 16+(i-56) of block 2 beyond that.
        let mut b1 = [0u8; 9];
        let mut b2 = [0u8; 9];
        put(&mut b1, 2, 6, 42);
        put(&mut b1, 8, 8, u64::from(MFID_HARRIS));
        put(&mut b2, 2, 6, 43);
        put(&mut b2, 8, 8, u64::from(MFID_HARRIS));

        // Latitude 40° 30.5000' N  →  assembled bits 0–31, all inside block 1.
        put(&mut b1, 16, 16, 5_000); // fraction: 0.5000 of a minute
                                     // The hemisphere bit (assembled 16) stays clear: north.
        put(&mut b1, 16 + 17, 7, 30); // minutes
        put(&mut b1, 16 + 24, 8, 40); // degrees
                                      // Longitude 100° 15.2500' W  →  assembled bits 32–63: 32–55 in block 1,
                                      // 56–63 in block 2.
        put(&mut b1, 16 + 32, 16, 2_500);
        put(&mut b1, 16 + 48, 1, 1); // west
        put(&mut b1, 16 + 49, 7, 15);
        put(&mut b2, 16, 8, 100); // assembled 56–63 → block 2 bits 16–23
                                  // Time of day and heading live wholly in block 2.
        put(&mut b2, 16 + (64 - 56), 16, 30_000);
        put(&mut b2, 16 + (95 - 56), 9, 271);

        let gps = decode_harris_gps(&b1, &b2);
        assert!(
            (gps.latitude - (40.0 + 30.5 / 60.0)).abs() < 1e-9,
            "{gps:?}"
        );
        assert!(
            (gps.longitude + (100.0 + 15.25 / 60.0)).abs() < 1e-9,
            "{gps:?}"
        );
        assert_eq!(gps.time_of_day_s, 30_000);
        assert_eq!(gps.heading, 271);
    }

    #[test]
    fn the_gps_time_msb_doubles_the_seconds() {
        let b1 = [0u8; 9];
        let mut b2 = [0u8; 9];
        put(&mut b2, 16 + (64 - 56), 16, 50_000);
        assert_eq!(decode_harris_gps(&b1, &b2).time_of_day_s, 50_000);
        // Bit 80 is sdrtrunk's reconstruction of the missing 17th bit.
        put(&mut b2, 16 + (80 - 56), 1, 1);
        assert_eq!(decode_harris_gps(&b1, &b2).time_of_day_s, 100_000);
    }

    /// Pack a 72-bit value into a 9-byte MSB-first word.
    fn word72(v: u128) -> [u8; 9] {
        let mut w = [0u8; 9];
        for (k, b) in w.iter_mut().enumerate() {
            *b = ((v >> (8 * (8 - k))) & 0xFF) as u8;
        }
        w
    }

    #[test]
    fn group_voice_lc() {
        // Standard (P=1), LCO 0. options 0x00, source_ext_flag 0, TG=52101,
        // source=0x0A1B2C.
        // bits: [0]=enc 0, [1]=std 1, [2..7]=lco 0, [8..15]=0, [16..23]=svc,
        // [31]=ext, [32..47]=TG, [48..71]=source.
        let mut v: u128 = 0;
        v |= 1u128 << (71 - 1); // standard flag (bit 1)
        v |= (52101u128) << (71 - 47); // TG at 32..47
        v |= 0x0A_1B2Cu128; // source at 48..71 (LSBs)
        let w = word72(v);
        match LinkControl::parse(&w) {
            LinkControl::GroupVoiceChannelUser {
                talkgroup,
                source,
                source_ext_flag,
                ..
            } => {
                assert_eq!(talkgroup, 52101);
                assert_eq!(source, 0x0A_1B2C);
                assert!(!source_ext_flag);
            }
            m => panic!("{m:?}"),
        }
    }

    #[test]
    fn unit_to_unit_lc() {
        let mut v: u128 = 0;
        v |= 1u128 << (71 - 1); // standard
        v |= 3u128 << (71 - 7); // LCO 3 at bits 2..7
        v |= 0x00_ABCDu128 << (71 - 47); // target at 24..47
        v |= 0x12_3456u128; // source at 48..71 (LSBs)
        let w = word72(v);
        match LinkControl::parse(&w) {
            LinkControl::UnitToUnitVoiceChannelUser { target, source, .. } => {
                assert_eq!(target, 0x00_ABCD);
                assert_eq!(source, 0x12_3456);
            }
            m => panic!("{m:?}"),
        }
    }

    #[test]
    fn telephone_lc() {
        let mut v: u128 = 0;
        v |= 1u128 << (71 - 1); // standard
        v |= 6u128 << (71 - 7); // LCO 6
        v |= 1234u128 << (71 - 47); // call timer at 32..47
        v |= 0x00_5566u128; // address at 48..71
        let w = word72(v);
        match LinkControl::parse(&w) {
            LinkControl::TelephoneInterconnectVoiceChannelUser {
                call_timer,
                address,
                ..
            } => {
                assert_eq!(call_timer, 1234);
                assert_eq!(address, 0x00_5566);
            }
            m => panic!("{m:?}"),
        }
    }

    #[test]
    fn group_voice_lc_with_flag_clear() {
        // LDU1 LC shaped like one seen off air (IDs synthetic): standard-vendor flag CLEAR,
        // octet 1 = 0x00 (STANDARD), LCO 0 → must parse as Group Voice, not `Other`.
        let w = [0x00, 0x00, 0x44, 0x00, 0x06, 0xa6, 0x89, 0x54, 0x46];
        match LinkControl::parse(&w) {
            LinkControl::GroupVoiceChannelUser {
                talkgroup,
                source,
                options,
                ..
            } => {
                assert_eq!(talkgroup, 0x06a6); // 1702
                assert_eq!(source, 0x895446); // 9000006
                assert!(options.encrypted); // svc-opt encrypted bit (0x44)
            }
            m => panic!("expected GroupVoiceChannelUser, got {m:?}"),
        }
    }

    /// Parse an 18-char (9-octet) hex string, as printed in the event log's
    /// `hex` field, into a 72-bit LC word.
    fn lc_from_hex(hex: &str) -> [u8; 9] {
        let bytes: Vec<u8> = (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect();
        assert_eq!(bytes.len(), 9, "an LC word is 9 octets");
        let mut w = [0u8; 9];
        w.copy_from_slice(&bytes);
        w
    }

    #[test]
    fn group_voice_channel_update_lc_from_hex() {
        // Band 1, channel 456, TG 0x06a5 (synthetic); slot B duplicates slot A, so it
        // collapses to `None`.
        match LinkControl::parse(&lc_from_hex("4211c806a511c806a5")) {
            LinkControl::GroupVoiceChannelUpdate { a, b } => {
                assert_eq!(
                    a.0,
                    ChannelRef {
                        band: 1,
                        channel: 456
                    }
                );
                assert_eq!(a.1, 0x06a5);
                assert_eq!(b, None, "slot B duplicates A and must collapse");
            }
            m => panic!("expected GroupVoiceChannelUpdate, got {m:?}"),
        }
    }

    #[test]
    fn group_voice_channel_update_keeps_a_distinct_slot_b() {
        // A synthetic two-group word: slot A TG 0x1111 on band 1 ch 1, slot B TG
        // 0x2222 on band 2 ch 2 — both must survive.
        let mut w = [0u8; 9];
        put(&mut w, 1, 1, 1); // standard flag
        put(&mut w, 2, 6, 2); // LCO 2
        put(&mut w, 8, 4, 1); // band A
        put(&mut w, 12, 12, 1); // channel A
        put(&mut w, 24, 16, 0x1111); // TG A
        put(&mut w, 40, 4, 2); // band B
        put(&mut w, 44, 12, 2); // channel B
        put(&mut w, 56, 16, 0x2222); // TG B
        match LinkControl::parse(&w) {
            LinkControl::GroupVoiceChannelUpdate { a, b } => {
                assert_eq!(
                    a,
                    (
                        ChannelRef {
                            band: 1,
                            channel: 1
                        },
                        0x1111
                    )
                );
                assert_eq!(
                    b,
                    Some((
                        ChannelRef {
                            band: 2,
                            channel: 2
                        },
                        0x2222
                    ))
                );
            }
            m => panic!("expected GroupVoiceChannelUpdate, got {m:?}"),
        }
    }

    #[test]
    fn call_termination_lc_from_hex() {
        // TDULC releasing source 0x895443 (a synthetic radio id).
        match LinkControl::parse(&lc_from_hex("4f0000000000895443")) {
            LinkControl::CallTermination { source } => assert_eq!(source, 0x895443),
            m => panic!("expected CallTermination, got {m:?}"),
        }
        // The all-zero-address teardown sentinel.
        let teardown = LinkControl::parse(&lc_from_hex("4f0000000000000000"));
        assert_eq!(teardown, LinkControl::CallTermination { source: 0 });
        assert!(teardown.is_network_commanded_teardown());
        // A radio termination is NOT a network teardown.
        assert!(
            !LinkControl::parse(&lc_from_hex("4f0000000000895443")).is_network_commanded_teardown()
        );
    }

    #[test]
    fn unit_to_unit_answer_request_lc_from_hex() {
        // A page (synthetic IDs), and the same pair the other way round, as a site sends
        // them a few seconds apart.
        match LinkControl::parse(&lc_from_hex("450000895441895442")) {
            LinkControl::UnitToUnitAnswerRequest {
                options,
                target,
                source,
            } => {
                assert_eq!(target, 0x895441);
                assert_eq!(source, 0x895442);
                assert!(!options.emergency && !options.encrypted);
            }
            m => panic!("expected UnitToUnitAnswerRequest, got {m:?}"),
        }
        match LinkControl::parse(&lc_from_hex("450000895442895441")) {
            LinkControl::UnitToUnitAnswerRequest { target, source, .. } => {
                assert_eq!((target, source), (0x895442, 0x895441));
            }
            m => panic!("expected UnitToUnitAnswerRequest, got {m:?}"),
        }
    }

    #[test]
    fn group_affiliation_query_lc_from_hex() {
        // Target 0x895444 (synthetic), source 0xfffffd (system controller sentinel).
        match LinkControl::parse(&lc_from_hex("500000895444fffffd")) {
            LinkControl::GroupAffiliationQuery { target, source } => {
                assert_eq!(target, 0x895444);
                assert_eq!(source, 0xfffffd);
            }
            m => panic!("expected GroupAffiliationQuery, got {m:?}"),
        }
    }

    #[test]
    fn extended_function_command_lc_from_hex() {
        // Radio detach (0x007d) of 0x895445 (synthetic).
        match LinkControl::parse(&lc_from_hex("57007dfffffd895445")) {
            LinkControl::ExtendedFunctionCommand {
                function,
                arguments,
                target,
            } => {
                assert_eq!(function, ExtendedFunction::RadioDetach);
                assert_eq!(function.name(), "radio_detach");
                assert_eq!(arguments, 0xfffffd);
                assert_eq!(target, 0x895445);
            }
            m => panic!("expected ExtendedFunctionCommand, got {m:?}"),
        }
    }

    #[test]
    fn extended_function_codes_round_trip() {
        for code in [0x0000u16, 0x007D, 0x007F, 0x00FF, 0x1234] {
            assert_eq!(ExtendedFunction::from_code(code).code(), code);
        }
        assert_eq!(
            ExtendedFunction::from_code(0x1234),
            ExtendedFunction::Unknown(0x1234)
        );
        assert_eq!(ExtendedFunction::from_code(0x1234).name(), "unknown");
    }

    #[test]
    fn a_harris_lco_15_is_not_a_call_termination() {
        // The vendor selects the table: MFID 164 keeps LCO 15 as a raw Harris
        // word, never a standard call termination.
        let mut w = [0u8; 9];
        put(&mut w, 2, 6, 15);
        put(&mut w, 8, 8, u64::from(MFID_HARRIS));
        assert!(matches!(LinkControl::parse(&w), LinkControl::Harris { .. }));
    }

    #[test]
    fn harris_lc_named_and_unknown() {
        // Vendor LC (P=0), MFID 164, LCO 50 = talker alias 1.
        let mut v: u128 = 0;
        v |= 50u128 << (71 - 7); // LCO 50
        v |= (MFID_HARRIS as u128) << (71 - 15); // vendor at 8..15
        let w = word72(v);
        match LinkControl::parse(&w) {
            LinkControl::Harris { opcode, .. } => {
                assert_eq!(opcode, HarrisLcOpcode::TalkerAlias1);
            }
            m => panic!("{m:?}"),
        }
        // Unknown Harris LCO 7.
        let mut v2: u128 = 0;
        v2 |= 7u128 << (71 - 7);
        v2 |= (MFID_HARRIS as u128) << (71 - 15);
        match LinkControl::parse(&word72(v2)) {
            LinkControl::Harris { opcode, .. } => {
                assert_eq!(opcode, HarrisLcOpcode::Unknown(7));
            }
            m => panic!("{m:?}"),
        }
    }

    #[test]
    fn encryption_sync_clear_and_encrypted() {
        // All-zero (no ES sent) → clear.
        let empty = EncryptionSync::parse(&[0u8; 12]);
        assert!(empty.is_clear());

        // ALGID 0x80 = UNENCRYPTED → clear even with a non-zero MI/KID.
        let mut w = [0u8; 12];
        w[0] = 0xAB; // stray MI byte
        w[9] = ALGID_UNENCRYPTED;
        let clear = EncryptionSync::parse(&w);
        assert!(clear.is_clear());
        assert_eq!(clear.algorithm(), "UNENCRYPTED");

        // ALGID 0x00 is the ACCORDION_3 cipher, NOT clear.
        let mut w0 = [0u8; 12];
        w0[10] = 0x00;
        w0[11] = 0x01; // non-zero KID → an actual keyed call
        let acc = EncryptionSync::parse(&w0);
        assert!(!acc.is_clear());
        assert_eq!(acc.algorithm(), "ACCORDION 3");

        // MI = 0x0102..09, ALGID 0x84 (AES-256), KID 0x1234 → encrypted.
        let mut w = [0u8; 12];
        for (i, b) in w.iter_mut().take(9).enumerate() {
            *b = (i + 1) as u8;
        }
        w[9] = 0x84;
        w[10] = 0x12;
        w[11] = 0x34;
        let es = EncryptionSync::parse(&w);
        assert!(!es.is_clear());
        assert_eq!(es.mi, [1, 2, 3, 4, 5, 6, 7, 8, 9]);
        assert_eq!(es.algid, 0x84);
        assert_eq!(es.kid, 0x1234);
        assert_eq!(es.algorithm(), "AES-256");
    }

    #[test]
    fn hdu_header_fields() {
        let mut d = [0u8; 15];
        for (i, b) in d.iter_mut().take(9).enumerate() {
            *b = (0x10 + i) as u8;
        }
        d[9] = 0xA4; // MFID
        d[10] = 0x80; // ALGID (clear)
        d[11] = 0x00;
        d[12] = 0x00; // KID 0
        d[13] = 0xCB;
        d[14] = 0x85; // TG 0xCB85 = 52101
        let h = HduHeader::parse(&d);
        assert_eq!(h.mfid, 0xA4);
        assert_eq!(h.algid, 0x80);
        assert_eq!(h.kid, 0);
        assert_eq!(h.talkgroup, 0xCB85);
        assert_eq!(h.mi, [0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18]);
    }
}
