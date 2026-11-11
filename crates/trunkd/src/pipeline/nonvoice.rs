// SPDX-License-Identifier: GPL-3.0-or-later
//! What the site says that is not a call: radio events, window histograms, and
//! the raw event-log line.
//!
//! `docs/NON-VOICE-DATA.md` measured 3 956 TSBKs over 150 s of which the
//! recorder read six kinds and dropped the rest at one `_ => {}`. This module
//! is the fold that replaces it. It produces three different things from the
//! same event stream, because they have three different costs:
//!
//! * **[`RadioEvent`]s** — the registration/affiliation/data family, ~600/hour.
//!   Batched and POSTed to Keyup: presence data worth a row each.
//! * **Window counters** — [`NonVoice::stats_json`], a histogram folded into
//!   the observations POST. Twenty-odd numbers per window instead of 95 000
//!   rows/hour, which is enough to notice a new opcode appearing.
//! * **Log lines** — [`NonVoice::log_line`], one JSON object per non-voice
//!   event for the on-disk analysis log.
//!
//! Nothing here runs on the DSP thread: the events cross the bounded
//! observations queue first (ARCHITECTURE §2).
//!
//! **Voice never reaches this module.** `SiteEvent::Traffic` drops
//! `P25Event::ImbeFrame` at the engine, so a vocoder frame cannot be logged
//! however this code is written.

use std::collections::BTreeMap;

use serde_json::{json, Map, Value};
use trunkd_p25::event::SiteBroadcastKind;
use trunkd_p25::iden::BandMap;
use trunkd_p25::lrrp;
use trunkd_p25::message::{
    parse_ambt_unit_grant, response_name, ChannelRef, PduHeader, Tsbk, TsbkMessage,
};
use trunkd_p25::payload::{is_standard_vendor, HarrisLcOpcode, LinkControl, MFID_HARRIS};
use trunkd_p25::P25Event;
use trunkd_uplink::events::RadioEvent;

use crate::pipeline::lrrp_gate;

/// Most unknown (mfid, opcode) exemplars kept per window.
///
/// One exemplar is enough to notice the day a vendor beacon changes; the
/// measured site has exactly two, so 20 is headroom, not a budget.
const MAX_EXEMPLARS: usize = 20;

/// Which channel an event was heard on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Channel {
    /// The site's control channel.
    Control,
    /// A traffic channel, by downlink frequency.
    Traffic(u64),
}

impl Channel {
    fn label(self) -> String {
        match self {
            Channel::Control => "control".to_owned(),
            Channel::Traffic(hz) => hz.to_string(),
        }
    }
}

/// Per-site non-voice fold: window counters plus the band plan needed to
/// resolve a data-channel grant to a frequency.
#[derive(Debug, Default)]
pub(crate) struct NonVoice {
    bands: BandMap,
    /// `(mfid, opcode) -> count` over the window, every TSBK including parsed.
    message_mix: BTreeMap<(u8, u8), u64>,
    /// First raw hex seen per unknown `(mfid, opcode)` this window.
    unknown_exemplars: BTreeMap<(u8, u8), String>,
    /// `(vendor, lco) -> count` over the window.
    link_control_mix: BTreeMap<(u8, u8), u64>,
    /// `(mfid, opcode) -> count` for AMBT (PDU format 0x17) headers, the
    /// multi-block trunking messages the TSBK census cannot see.
    ambt_mix: BTreeMap<(u8, u8), u64>,
    data_grants: u64,
    data_pages: u64,
    pdu_headers: u64,
    pdu_blocks_valid: u64,
    /// PDU payloads carrying a UDP datagram to a known LRRP port (Motorola 4001
    /// or the L3Harris port seen on a test capture; see `trunkd_p25::lrrp::LRRP_PORTS`).
    lrrp_transport: u64,
    /// PDU payloads from which a latitude/longitude fix decoded.
    lrrp_fixes: u64,
    lsd_nonzero: u64,
    /// Private calls followed to completion this window.
    private_calls: u64,
}

impl NonVoice {
    /// A fresh fold with an empty band plan.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Fold one control-channel event.
    ///
    /// Returns the [`RadioEvent`] it produced, if the message was one of the
    /// family worth a row. The band plan is updated here too, so a data-channel
    /// grant that arrives after its identifier update resolves to a frequency.
    pub(crate) fn observe_control(&mut self, e: &P25Event, at_ms: u64) -> Option<RadioEvent> {
        match e {
            P25Event::Tsbk { tsbk, .. } => {
                let key = (tsbk.vendor, tsbk.opcode);
                *self.message_mix.entry(key).or_default() += 1;
                if is_unknown(&tsbk.message) && self.unknown_exemplars.len() < MAX_EXEMPLARS {
                    self.unknown_exemplars
                        .entry(key)
                        .or_insert_with(|| tsbk.hex());
                }
                match &tsbk.message {
                    TsbkMessage::SndcpDataChannelGrant { .. } => self.data_grants += 1,
                    TsbkMessage::SndcpDataPageRequest { .. } => self.data_pages += 1,
                    TsbkMessage::IdenUpdate(band) => self.bands.insert(*band),
                    _ => {}
                }
                radio_event(tsbk, at_ms, &self.bands)
            }
            P25Event::IdenUpdate { .. } => None,
            P25Event::PduHeader { header, .. } => {
                self.observe_pdu_header(header);
                None
            }
            P25Event::Pdu {
                header,
                blocks,
                valid: true,
                payload,
                ..
            } => self.observe_pdu(header, *blocks, payload, at_ms),
            // A finished private call is a row on both radios' timelines even
            // when no audio was worth uploading — which is the point: before
            // this, a conversation between two units left no trace on Keyup at
            // all. Only the *end* is sent, because only the end knows how long
            // the call was, and one row per call beats two.
            P25Event::PrivateCall {
                start: false,
                frequency_hz,
                source,
                target,
                duration_ms,
            } => {
                self.private_calls += 1;
                Some(RadioEvent {
                    at_ms,
                    kind: "private_call",
                    // The AMBT opcode the grant arrived under; there is no
                    // TSBK behind this event.
                    opcode: trunkd_p25::message::AMBT_OP_UU_VCH_GRANT,
                    mfid: 0,
                    radio_id: Some(*source),
                    target_radio_id: Some(*target),
                    frequency_hz: Some(*frequency_hz),
                    meta: json!({
                        // Wall clock, like every other time on this thread:
                        // the event's own instant less the call's length.
                        "started_at_ms": at_ms.saturating_sub(*duration_ms),
                        "duration_ms": duration_ms,
                    }),
                    ..RadioEvent::default()
                })
            }
            _ => None,
        }
    }

    /// Fold one traffic-channel event (signaling only: no voice arrives here).
    ///
    /// Returns a [`RadioEvent`] for the Link Control words that name a
    /// subscriber administrative action — affiliation query (LCO 16) and
    /// extended-function command (LCO 23) — the voice-path twins of the control
    /// channel's registration/affiliation family.
    pub(crate) fn observe_traffic(&mut self, e: &P25Event, at_ms: u64) -> Option<RadioEvent> {
        match e {
            P25Event::LinkControl { word, parsed, .. } => {
                *self.link_control_mix.entry(lc_key(word)).or_default() += 1;
                radio_event_lc(parsed, lc_key(word).1, at_ms)
            }
            P25Event::TduLc {
                valid: true,
                link_control,
                ..
            } => {
                *self
                    .link_control_mix
                    .entry(lc_key(link_control))
                    .or_default() += 1;
                let parsed = LinkControl::parse(link_control);
                radio_event_lc(&parsed, lc_key(link_control).1, at_ms)
            }
            P25Event::LowSpeedData { data, valid, .. } => {
                // "Non-zero" means a real octet carrying something: an octet the
                // FEC could not correct is not evidence of data.
                if (valid[0] && data[0] != 0) || (valid[1] && data[1] != 0) {
                    self.lsd_nonzero += 1;
                }
                None
            }
            P25Event::PduHeader { header, .. } => {
                self.observe_pdu_header(header);
                None
            }
            P25Event::Pdu {
                header,
                blocks,
                valid: true,
                payload,
                ..
            } => self.observe_pdu(header, *blocks, payload, at_ms),
            _ => None,
        }
    }

    /// Count a PDU header, and — for AMBT (format 0x17) — census the trunking
    /// message it announces by (MFID, opcode). Those never reach the TSBK
    /// path, so without this the 1,980 control-channel AMBTs of the 2026-09-03
    /// window show up only as an undifferentiated `pdu_headers` total.
    fn observe_pdu_header(&mut self, header: &PduHeader) {
        self.pdu_headers += 1;
        if let Some(op) = header.ambt_opcode {
            *self.ambt_mix.entry((header.vendor, op)).or_default() += 1;
        }
    }

    /// Fold a completed, block-valid PDU: count it, and — best-effort — mine its
    /// reassembled payload for an LRRP location. Returns a `location`
    /// [`RadioEvent`] when a latitude/longitude fix decodes.
    ///
    /// This runs on the observations thread, never the DSP thread, so the scan
    /// is off the hot path (ARCHITECTURE §2).
    fn observe_pdu(
        &mut self,
        header: &PduHeader,
        blocks: u8,
        payload: &[u8],
        at_ms: u64,
    ) -> Option<RadioEvent> {
        self.pdu_blocks_valid += u64::from(blocks);
        if lrrp::contains_lrrp_transport(payload) {
            self.lrrp_transport += 1;
        }
        let candidate = lrrp::extract_lrrp_candidate(payload)?;
        if lrrp_gate::admit(payload, header.llid, &candidate).is_err() {
            return None;
        }
        let fix = candidate.fix;
        self.lrrp_fixes += 1;
        tracing::info!(
            radio = header.llid,
            lat = fix.latitude,
            lon = fix.longitude,
            via_icmp = fix.via_icmp,
            token_guided = fix.token_guided,
            "LRRP location decoded from SNDCP PDU"
        );
        Some(RadioEvent {
            at_ms,
            kind: "location",
            opcode: 20,
            mfid: header.vendor,
            radio_id: Some(header.llid),
            meta: json!({
                "lat": fix.latitude,
                "lon": fix.longitude,
                "lat_raw": fix.lat_raw,
                "lon_raw": fix.lon_raw,
                "via_icmp": fix.via_icmp,
                "token_guided": fix.token_guided,
            }),
            ..RadioEvent::default()
        })
    }

    /// The window's non-voice counters, for the observations body.
    pub(crate) fn stats_json(&self) -> Value {
        json!({
            "message_mix": message_mix_json(&self.message_mix, &self.ambt_mix),
            "unknown_exemplars": self
                .unknown_exemplars
                .iter()
                .map(|((mfid, opcode), hex)| json!({
                    "mfid": mfid,
                    "opcode": opcode,
                    "count": self.message_mix.get(&(*mfid, *opcode)).copied().unwrap_or(0),
                    "exemplar": hex,
                }))
                .collect::<Vec<_>>(),
            "link_control_mix": mix_json(&self.link_control_mix),
            "data_channel": {
                "grants": self.data_grants,
                "pages": self.data_pages,
                "pdu_headers": self.pdu_headers,
                "pdu_blocks_valid": self.pdu_blocks_valid,
                "lrrp_transport": self.lrrp_transport,
                "lrrp_fixes": self.lrrp_fixes,
            },
            "lsd_nonzero": self.lsd_nonzero,
            "private_calls": self.private_calls,
        })
    }

    /// Clear the window counters. The band plan is site state, not window
    /// state, and survives.
    pub(crate) fn reset_window(&mut self) {
        self.message_mix.clear();
        self.unknown_exemplars.clear();
        self.link_control_mix.clear();
        self.ambt_mix.clear();
        self.data_grants = 0;
        self.data_pages = 0;
        self.pdu_headers = 0;
        self.pdu_blocks_valid = 0;
        self.lrrp_transport = 0;
        self.lrrp_fixes = 0;
        self.lsd_nonzero = 0;
        self.private_calls = 0;
    }
}

/// `{"<mfid>:<opcode>": count}` for the observations body.
fn mix_json(mix: &BTreeMap<(u8, u8), u64>) -> Value {
    let mut o = Map::new();
    for ((a, b), n) in mix {
        o.insert(format!("{a}:{b}"), json!(n));
    }
    Value::Object(o)
}

/// The TSBK census, with the AMBT census folded in under `ambt:<mfid>:<opcode>`
/// keys. One map keeps the observation body's shape — Keyup stores it whole —
/// while the prefix keeps a multi-block message distinct from the single-block
/// TSBK that shares its opcode number.
fn message_mix_json(mix: &BTreeMap<(u8, u8), u64>, ambt: &BTreeMap<(u8, u8), u64>) -> Value {
    let mut o = match mix_json(mix) {
        Value::Object(o) => o,
        _ => unreachable!("mix_json builds an object"),
    };
    for ((a, b), n) in ambt {
        o.insert(format!("ambt:{a}:{b}"), json!(n));
    }
    Value::Object(o)
}

/// `(vendor, lco)` of a raw 72-bit link-control word.
///
/// Read from the wire bytes rather than the parsed enum: the parsed variants
/// that *are* understood (group voice user, and the rest) do not carry their
/// own opcode, and a histogram that could only count the unparsed ones would
/// answer the wrong question.
fn lc_key(word: &[u8; 9]) -> (u8, u8) {
    let standard_flag = word[0] & 0x40 != 0;
    let lco = word[0] & 0x3F;
    let vendor = if standard_flag { 0 } else { word[1] };
    (vendor, lco)
}

/// True when the engine did not parse this message's payload.
fn is_unknown(m: &TsbkMessage) -> bool {
    matches!(m, TsbkMessage::Other { .. } | TsbkMessage::HarrisOsp { .. })
}

/// Resolve a channel reference through the band plan.
fn resolve(bands: &BandMap, ch: ChannelRef) -> Option<u64> {
    bands.get(ch.band).map(|b| b.downlink_hz(ch.channel))
}

/// Build a [`RadioEvent`] from a TSBK, or `None` when the message says nothing
/// about a subscriber.
///
/// The `kind` strings are the recorder's contract with Keyup's
/// `radio_event_kind` enum; the fields Keyup has no column for ride in `meta`.
pub(crate) fn radio_event(tsbk: &Tsbk, at_ms: u64, bands: &BandMap) -> Option<RadioEvent> {
    let base = RadioEvent {
        at_ms,
        kind: "",
        opcode: tsbk.opcode,
        mfid: tsbk.vendor,
        meta: json!({}),
        ..RadioEvent::default()
    };
    let ev = match &tsbk.message {
        TsbkMessage::UnitRegistrationResponse {
            response,
            system_id,
            source_id,
            source_address,
        } => RadioEvent {
            kind: "unit_registration",
            radio_id: Some(*source_id),
            // sdrtrunk treats the two addresses as one radio unless they
            // differ, in which case the id is the radio and the address is its
            // local alias on this system.
            radio_system: (*system_id != 0).then_some(*system_id),
            result: Some(response_name(*response)),
            meta: json!({ "source_address": source_address }),
            ..base
        },
        TsbkMessage::UnitDeRegistrationAck {
            wacn,
            system_id,
            source,
        } => RadioEvent {
            kind: "unit_deregistration",
            radio_id: Some(*source),
            radio_wacn: (*wacn != 0).then_some(*wacn),
            radio_system: (*system_id != 0).then_some(*system_id),
            ..base
        },
        TsbkMessage::UnitRegistrationCommand { target, source } => RadioEvent {
            kind: "unit_registration_command",
            radio_id: Some(*target),
            target_radio_id: Some(*source),
            ..base
        },
        TsbkMessage::GroupAffiliationResponse {
            global_affiliation,
            response,
            announcement_group,
            group,
            target,
        } => RadioEvent {
            kind: "group_affiliation",
            radio_id: Some(*target),
            talkgroup: Some(u32::from(*group)),
            result: Some(response_name(*response)),
            meta: json!({
                "global": global_affiliation,
                "announcement_group": announcement_group,
            }),
            ..base
        },
        TsbkMessage::GroupAffiliationQuery { target, source } => RadioEvent {
            kind: "group_affiliation_query",
            radio_id: Some(*target),
            target_radio_id: Some(*source),
            ..base
        },
        TsbkMessage::LocationRegistrationResponse {
            response,
            group,
            rfss,
            site,
            target,
        } => RadioEvent {
            kind: "location_registration",
            radio_id: Some(*target),
            talkgroup: Some(u32::from(*group)),
            result: Some(response_name(*response)),
            meta: json!({ "rfss": rfss, "site": site }),
            ..base
        },
        TsbkMessage::SndcpDataChannelGrant {
            options,
            downlink,
            uplink,
            target,
        } => RadioEvent {
            kind: "sndcp_grant",
            radio_id: Some(*target),
            frequency_hz: resolve(bands, *downlink),
            meta: json!({
                "nsapi": options.nsapi,
                "encrypted": options.encrypted,
                "downlink_channel": downlink.channel,
                "uplink_channel": uplink.channel,
            }),
            ..base
        },
        // One address only: the subscriber being paged. What used to ride in
        // `target_radio_id` was the message's reserved field (0x00ffff on the
        // air), not a second party.
        TsbkMessage::SndcpDataPageRequest {
            options, target, ..
        } => RadioEvent {
            kind: "sndcp_page",
            radio_id: Some(*target),
            meta: json!({ "nsapi": options.nsapi }),
            ..base
        },
        // The control-channel copy of the extended function command. Keyup
        // does not know the kind, so it stores it as `other` with the opcode
        // kept — exactly as it already does for the Link Control copy (LCO
        // 23) this mirrors. The two are separate rows by design: Keyup's
        // identity index is (site, at, kind, opcode, radio_id, talkgroup), and
        // the opcode differs between the two channels' encodings of the same
        // command, so the fields are matched to the LC event but the rows are
        // not merged. Two recorders hearing the *same* control-channel TSBK do
        // dedupe, which is what the key is for.
        TsbkMessage::ExtendedFunctionCommand {
            function,
            arguments,
            target,
        } => RadioEvent {
            kind: "extended_function",
            radio_id: Some(*target),
            meta: json!({
                "function": function.name(),
                "function_code": function.code(),
                "arguments": arguments,
            }),
            ..base
        },
        TsbkMessage::AcknowledgeResponse {
            service_type,
            wacn,
            system_id,
            source,
            target,
            ..
        } => RadioEvent {
            kind: "ack",
            radio_id: Some(*target),
            radio_wacn: *wacn,
            radio_system: *system_id,
            target_radio_id: *source,
            meta: json!({ "service_type": service_type }),
            ..base
        },
        TsbkMessage::DenyResponse {
            service_type,
            reason,
            additional_info,
            target,
            ..
        } => RadioEvent {
            kind: "deny",
            radio_id: Some(*target),
            result: Some("denied"),
            meta: json!({
                "service_type": service_type,
                "reason": reason,
                "additional_info": additional_info,
            }),
            ..base
        },
        TsbkMessage::QueuedResponse {
            service_type,
            reason,
            additional_info,
            target,
            ..
        } => RadioEvent {
            kind: "queued",
            radio_id: Some(*target),
            result: Some("queued"),
            meta: json!({
                "service_type": service_type,
                "reason": reason,
                "additional_info": additional_info,
            }),
            ..base
        },
        _ => return None,
    };
    Some(ev)
}

/// Build a [`RadioEvent`] from a traffic-channel Link Control word, or `None`
/// when the word says nothing about a subscriber action.
///
/// Only the administrative LCs are surfaced: affiliation query (LCO 16) and
/// extended-function command (LCO 23). Their `kind` strings match the
/// control-channel TSBK twins so Keyup sees one event class regardless of which
/// path carried it. `lco` is the raw opcode, for the event's `opcode` field.
pub(crate) fn radio_event_lc(lc: &LinkControl, lco: u8, at_ms: u64) -> Option<RadioEvent> {
    let base = RadioEvent {
        at_ms,
        kind: "",
        opcode: lco,
        mfid: 0,
        meta: json!({}),
        ..RadioEvent::default()
    };
    let ev = match lc {
        LinkControl::GroupAffiliationQuery { target, source } => RadioEvent {
            kind: "group_affiliation_query",
            radio_id: Some(*target),
            target_radio_id: Some(*source),
            ..base
        },
        LinkControl::ExtendedFunctionCommand {
            function,
            arguments,
            target,
        } => RadioEvent {
            kind: "extended_function",
            radio_id: Some(*target),
            meta: json!({
                "function": function.name(),
                "function_code": function.code(),
                "arguments": arguments,
            }),
            ..base
        },
        _ => return None,
    };
    Some(ev)
}

// ---------------------------------------------------------------------------
// the raw event log line
// ---------------------------------------------------------------------------

/// Stamp an AMBT header's trunking MFID, opcode and opcode name onto a log
/// line's `fields`. A no-op for every other PDU format, which carries no
/// trunking opcode in its header.
fn add_ambt_fields(fields: &mut Value, header: &PduHeader) {
    let (Some(op), Some(name)) = (header.ambt_opcode, header.ambt_opcode_name()) else {
        return;
    };
    fields["ambt_mfid"] = json!(header.vendor);
    fields["ambt_opcode"] = json!(op);
    fields["ambt_name"] = json!(name);
}

/// Stamp the decoded unit-to-unit grant onto a PDU record when the payload is
/// one: both units, the granted channel and the source's full SUID, so the
/// event log carries the decode and not only the hex it came from.
fn add_unit_grant_fields(fields: &mut Value, header: &PduHeader, payload: &[u8]) {
    let Some(g) = parse_ambt_unit_grant(header, payload) else {
        return;
    };
    fields["uu_source"] = json!(g.source);
    fields["uu_target"] = json!(g.target);
    fields["uu_band"] = json!(g.channel.band);
    fields["uu_channel"] = json!(g.channel.channel);
    fields["uu_source_wacn"] = json!(g.source_wacn);
    fields["uu_source_system"] = json!(g.source_system);
}

/// Render one non-voice event as an event-log line, or `None` for an event the
/// log does not carry (voice, sync, stats).
/// Render an IPv4 address as dotted quad.
fn ipv4(a: [u8; 4]) -> String {
    format!("{}.{}.{}.{}", a[0], a[1], a[2], a[3])
}

/// Add the SNDCP header and IP/UDP envelope of a data payload to a PDU record.
///
/// These are identifiers 1–5 of the 2026-09-04 phase-3 report: the NSAPI, the
/// two addresses (one of which is the FNE data host `<pool>.2.50`), the two
/// ports (64414 both ends, the L3Harris location transport) and the datagram
/// length. Writing them out means the registry generator reads fields rather
/// than re-dissecting payload hex. The raw hex stays exactly as it was.
fn add_sndcp_fields(fields: &mut Value, payload: &[u8]) {
    let Some(d) = lrrp::parse_sndcp_datagram(payload) else {
        return;
    };
    if let Some(nsapi) = d.nsapi {
        fields["sndcp_nsapi"] = json!(nsapi);
    }
    fields["ip_src"] = json!(ipv4(d.ip_src));
    fields["ip_dst"] = json!(ipv4(d.ip_dst));
    if let Some(p) = d.udp_src {
        fields["udp_src"] = json!(p);
    }
    if let Some(p) = d.udp_dst {
        fields["udp_dst"] = json!(p);
    }
    if let Some(n) = d.payload_len {
        fields["payload_len"] = json!(n);
    }
}

pub(crate) fn log_line(e: &P25Event, channel: Channel, at: &str) -> Option<Value> {
    let ch = channel.label();
    let line = match e {
        P25Event::Tsbk { tsbk, .. } => json!({
            "at": at,
            "ch": ch,
            "type": "tsbk",
            "opcode": tsbk.opcode,
            "mfid": tsbk.vendor,
            "name": tsbk.name(),
            "hex": tsbk.hex(),
            "fields": tsbk_fields(tsbk),
        }),
        P25Event::SiteBroadcast { kind, message, .. } => json!({
            "at": at,
            "ch": ch,
            "type": "site_broadcast",
            "name": message.name(),
            "kind": site_broadcast_kind(*kind),
        }),
        P25Event::LinkControl { word, parsed, .. } => {
            let (vendor, lco) = lc_key(word);
            json!({
                "at": at,
                "ch": ch,
                "type": "lc",
                "opcode": lco,
                "vendor": vendor,
                "name": lc_name(parsed, lco),
                "hex": hex(word),
                "fields": lc_fields(parsed),
            })
        }
        P25Event::TduLc {
            valid: true,
            link_control,
            ..
        } => {
            let (vendor, lco) = lc_key(link_control);
            let parsed = LinkControl::parse(link_control);
            json!({
                "at": at,
                "ch": ch,
                "type": "tdulc",
                "opcode": lco,
                "vendor": vendor,
                "name": lc_name(&parsed, lco),
                "hex": hex(link_control),
                "fields": lc_fields(&parsed),
            })
        }
        P25Event::Hdu {
            valid: true,
            header: Some(h),
            data,
            ..
        } => json!({
            "at": at,
            "ch": ch,
            "type": "hdu",
            "name": "HEADER",
            "hex": hex(data),
            "fields": {
                "talkgroup": h.talkgroup,
                "algid": h.algid,
                "key_id": h.kid,
                "mfid": h.mfid,
            },
        }),
        P25Event::PduHeader { header, .. } => {
            let mut fields = json!({
                "format": header.format,
                "confirmed": header.confirmed,
                "outbound": header.outbound,
                "vendor": header.vendor,
                "llid": header.llid,
                "blocks_to_follow": header.blocks_to_follow,
            });
            add_ambt_fields(&mut fields, header);
            json!({
                "at": at,
                "ch": ch,
                "type": "pdu_header",
                "name": "PDU HEADER",
                "fields": fields,
            })
        }
        P25Event::Pdu {
            header,
            blocks,
            valid,
            payload,
            ..
        } => {
            let mut fields = json!({
                "format": header.format,
                "llid": header.llid,
                "blocks": blocks,
                "valid": valid,
                "payload": hex(payload),
            });
            add_ambt_fields(&mut fields, header);
            add_unit_grant_fields(&mut fields, header, payload);
            add_sndcp_fields(&mut fields, payload);
            // Best-effort SNDCP/LRRP mining, so the on-disk log is self-contained
            // evidence for the spike: whether the payload carries UDP/4001 and
            // whether a coordinate decoded.
            if lrrp::contains_lrrp_transport(payload) {
                fields["lrrp_transport"] = json!(true);
            }
            // A candidate that the gate refuses is recorded as a refusal, not
            // dropped: seven false `location` records in the four-day log are
            // the reason the gate exists, and a silent gate is unauditable.
            if let Some(c) = lrrp::extract_lrrp_candidate(payload) {
                match lrrp_gate::admit(payload, header.llid, &c) {
                    Ok(()) => {
                        fields["location"] = json!({
                            "lat": c.fix.latitude,
                            "lon": c.fix.longitude,
                            "via_icmp": c.fix.via_icmp,
                            "token_guided": c.fix.token_guided,
                        });
                    }
                    Err(reason) => {
                        fields["location_rejected"] = json!(reason.as_str());
                    }
                }
            }
            // An SNDCP context accept is the LLID -> IPv4 map, and worth its own
            // record so the registry generator can read it without dissecting
            // payload hex. Every field of the PDU record is kept alongside.
            if let Some(ip) = lrrp::parse_sndcp_context_accept(payload) {
                return Some(json!({
                    "at": at,
                    "ch": ch,
                    "type": "sndcp_context",
                    "name": "SNDCP CONTEXT ACCEPT",
                    "llid": header.llid,
                    "ip": ipv4(ip),
                    "fields": fields,
                }));
            }
            json!({
                "at": at,
                "ch": ch,
                "type": "pdu",
                "name": "PDU",
                "fields": fields,
            })
        }
        // A private call the engine opened or closed. Two records rather than
        // one: the start is what shows the recorder acted on the grant, and
        // only the end knows how long the call lasted.
        P25Event::PrivateCall {
            start,
            frequency_hz,
            source,
            target,
            duration_ms,
        } => json!({
            "at": at,
            "ch": ch,
            "type": "private_call",
            "name": if *start { "PRIVATE CALL START" } else { "PRIVATE CALL END" },
            "fields": {
                "start": start,
                "frequency_hz": frequency_hz,
                "source": source,
                "target": target,
                "duration_ms": duration_ms,
            },
        }),
        // The recorder's own record of a stream-clock re-anchor, so a day of
        // timestamps read back out of the log shows where the seam is and how
        // wide it was.
        P25Event::ClockResync { correction_ms } => json!({
            "at": at,
            "ch": ch,
            "type": "clock_resync",
            "name": "CLOCK_RESYNC",
            "fields": { "correction_ms": correction_ms },
        }),
        // All-zero LSD is the overwhelming majority on a system that does not
        // use it (393 of 398 measured); logging those would be 20 lines a
        // second of nothing.
        P25Event::LowSpeedData { data, valid, .. }
            if (valid[0] && data[0] != 0) || (valid[1] && data[1] != 0) =>
        {
            json!({
                "at": at,
                "ch": ch,
                "type": "lsd",
                "name": "LOW SPEED DATA",
                "hex": hex(data),
            })
        }
        _ => return None,
    };
    Some(line)
}

/// Standard (MFID 0/1) TSBK opcodes that describe the site itself and name no
/// subscriber: SNDCP data-channel announcement (22), SCCB explicit (41), TDMA
/// sync (48), the TDMA/VHF-UHF band-plan updates (51, 52), time and date (53),
/// system services (56), secondary control (57), RFSS status (58), network
/// status (59), adjacent site (60), the band-plan update (61) and the
/// unconfirmed adjacent site (62).
const STANDARD_BROADCASTS: [u8; 13] = [22, 41, 48, 51, 52, 53, 56, 57, 58, 59, 60, 61, 62];

/// L3Harris TSBK opcodes of the same kind: the two beacons (1, 15) and the
/// regroup broadcast (48), which repeats one patch's state until it changes.
const HARRIS_BROADCASTS: [u8; 3] = [1, 15, 48];

/// Whether an event is a constant broadcast, whose exact repeats the event
/// log may write as back-references ([`crate::pipeline::eventlog`]).
///
/// An allow-list, deliberately: a message that carries something per-message —
/// a grant, a grant update, an affiliation, a registration, a data PDU, any
/// link control — is never coalesced, even when two copies happen to match.
/// `harness/eventlog_repeats.py` keeps the same list.
pub(crate) fn is_constant_broadcast(e: &P25Event) -> bool {
    match e {
        P25Event::SiteBroadcast { .. } => true,
        P25Event::Tsbk { tsbk, .. } => {
            if is_standard_vendor(tsbk.vendor) {
                STANDARD_BROADCASTS.contains(&tsbk.opcode)
            } else {
                tsbk.vendor == MFID_HARRIS && HARRIS_BROADCASTS.contains(&tsbk.opcode)
            }
        }
        _ => false,
    }
}

fn site_broadcast_kind(k: SiteBroadcastKind) -> &'static str {
    match k {
        SiteBroadcastKind::SecondaryControl => "secondary_control",
        SiteBroadcastKind::RfssStatus => "rfss_status",
        SiteBroadcastKind::NetworkStatus => "network_status",
        SiteBroadcastKind::AdjacentStatus => "adjacent_status",
    }
}

fn lc_name(lc: &LinkControl, lco: u8) -> String {
    match lc {
        LinkControl::GroupVoiceChannelUser { .. } => "GROUP VOICE CHANNEL USER".into(),
        LinkControl::UnitToUnitVoiceChannelUser { .. } => "UNIT TO UNIT VOICE CHANNEL USER".into(),
        LinkControl::UnitToUnitAnswerRequest { .. } => "UNIT TO UNIT ANSWER REQUEST".into(),
        LinkControl::TelephoneInterconnectVoiceChannelUser { .. } => {
            "TELEPHONE INTERCONNECT VOICE CHANNEL USER".into()
        }
        LinkControl::SourceIdExtension { .. } => "SOURCE ID EXTENSION".into(),
        LinkControl::GroupVoiceChannelUpdate { .. } => "GROUP VOICE CHANNEL UPDATE".into(),
        LinkControl::CallTermination { .. } => "CALL TERMINATION".into(),
        LinkControl::GroupAffiliationQuery { .. } => "GROUP AFFILIATION QUERY".into(),
        LinkControl::ExtendedFunctionCommand { .. } => "EXTENDED FUNCTION COMMAND".into(),
        LinkControl::Harris { opcode, .. } => match opcode {
            HarrisLcOpcode::ReturnToControlChannel => "L3HARRIS RETURN TO CONTROL CHANNEL".into(),
            HarrisLcOpcode::TalkerGps1 => "L3HARRIS TALKER GPS BLOCK 1".into(),
            HarrisLcOpcode::TalkerGps2 => "L3HARRIS TALKER GPS BLOCK 2".into(),
            HarrisLcOpcode::TalkerAlias1 => "L3HARRIS TALKER ALIAS BLOCK 1".into(),
            HarrisLcOpcode::TalkerAlias2 => "L3HARRIS TALKER ALIAS BLOCK 2".into(),
            HarrisLcOpcode::TalkerAlias3 => "L3HARRIS TALKER ALIAS BLOCK 3".into(),
            HarrisLcOpcode::TalkerAlias4 => "L3HARRIS TALKER ALIAS BLOCK 4".into(),
            HarrisLcOpcode::Unknown(n) => format!("L3HARRIS LC UNKNOWN {n}"),
        },
        LinkControl::Other { .. } => format!("LC OPCODE {lco}"),
    }
}

fn lc_fields(lc: &LinkControl) -> Value {
    match lc {
        LinkControl::GroupVoiceChannelUser {
            options,
            talkgroup,
            source,
            ..
        } => json!({
            "talkgroup": talkgroup,
            "source": source,
            "emergency": options.emergency,
            "encrypted": options.encrypted,
        }),
        LinkControl::UnitToUnitVoiceChannelUser { target, source, .. }
        | LinkControl::UnitToUnitAnswerRequest { target, source, .. } => {
            json!({ "target": target, "source": source })
        }
        LinkControl::SourceIdExtension {
            wacn,
            system,
            source,
        } => json!({ "wacn": wacn, "system": system, "source": source }),
        LinkControl::GroupVoiceChannelUpdate { a, b } => json!({
            "channel_a": a.0.channel,
            "band_a": a.0.band,
            "talkgroup_a": a.1,
            "channel_b": b.map(|(c, _)| c.channel),
            "band_b": b.map(|(c, _)| c.band),
            "talkgroup_b": b.map(|(_, tg)| tg),
        }),
        LinkControl::CallTermination { source } => json!({
            "source": source,
            "network_teardown": *source == 0,
        }),
        LinkControl::GroupAffiliationQuery { target, source } => {
            json!({ "target": target, "source": source })
        }
        LinkControl::ExtendedFunctionCommand {
            function,
            arguments,
            target,
        } => json!({
            "function": function.name(),
            "function_code": function.code(),
            "arguments": arguments,
            "target": target,
        }),
        _ => json!({}),
    }
}

/// Parsed fields of a TSBK, for the log line. Empty for a message the engine
/// does not decode — the `hex` beside it is the whole record in that case.
fn tsbk_fields(t: &Tsbk) -> Value {
    match &t.message {
        TsbkMessage::GroupVoiceGrant {
            talkgroup, source, ..
        } => json!({ "talkgroup": talkgroup, "source": source }),
        TsbkMessage::UnitRegistrationResponse {
            response,
            system_id,
            source_id,
            source_address,
        } => json!({
            "response": response_name(*response),
            "system_id": system_id,
            "source_id": source_id,
            "source_address": source_address,
        }),
        TsbkMessage::UnitDeRegistrationAck {
            wacn,
            system_id,
            source,
        } => json!({ "wacn": wacn, "system_id": system_id, "source": source }),
        TsbkMessage::UnitRegistrationCommand { target, source } => {
            json!({ "target": target, "source": source })
        }
        TsbkMessage::GroupAffiliationResponse {
            global_affiliation,
            response,
            group,
            target,
            ..
        } => json!({
            "global": global_affiliation,
            "response": response_name(*response),
            "group": group,
            "target": target,
        }),
        TsbkMessage::GroupAffiliationQuery { target, source } => {
            json!({ "target": target, "source": source })
        }
        TsbkMessage::LocationRegistrationResponse {
            response,
            group,
            rfss,
            site,
            target,
        } => json!({
            "response": response_name(*response),
            "group": group,
            "rfss": rfss,
            "site": site,
            "target": target,
        }),
        TsbkMessage::SndcpDataChannelGrant {
            downlink, target, ..
        } => json!({ "channel": downlink.channel, "band": downlink.band, "target": target }),
        TsbkMessage::SndcpDataPageRequest {
            target, reserved, ..
        } => json!({ "target": target, "reserved": reserved }),
        TsbkMessage::ExtendedFunctionCommand {
            function,
            arguments,
            target,
        } => json!({
            "function": format!("0x{:04x}", function.code()),
            "function_name": function.name(),
            "arguments": format!("0x{arguments:06x}"),
            "target": target,
        }),
        TsbkMessage::SystemServiceBroadcast {
            available,
            supported,
            priority,
        } => json!({
            "available": available.names(),
            "supported": supported.names(),
            "available_raw": format!("0x{:06x}", available.raw),
            "supported_raw": format!("0x{:06x}", supported.raw),
            "priority": priority,
        }),
        TsbkMessage::AcknowledgeResponse {
            service_type,
            target,
            ..
        } => json!({ "service_type": service_type, "target": target }),
        TsbkMessage::DenyResponse {
            service_type,
            reason,
            target,
            ..
        } => json!({ "service_type": service_type, "reason": reason, "target": target }),
        TsbkMessage::QueuedResponse {
            service_type,
            reason,
            target,
            ..
        } => json!({ "service_type": service_type, "reason": reason, "target": target }),
        _ => json!({}),
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use trunkd_p25::bits::BitBuf;
    use trunkd_p25::message::parse_tsbk;

    /// A synthetic standard-vendor TSBK of `opcode`, with `set` applied.
    fn tsbk(opcode: u8, set: impl Fn(&mut BitBuf)) -> Tsbk {
        let mut b = BitBuf::new();
        for _ in 0..96 {
            b.push(false);
        }
        b.set(0, true);
        b.set_u64(2, 6, u64::from(opcode));
        set(&mut b);
        parse_tsbk(&b, 0)
    }

    /// A 12-octet hex TSBK (the raw event log's `hex` field) as bits.
    fn from_hex(hex: &str) -> BitBuf {
        let mut b = BitBuf::new();
        for _ in 0..96 {
            b.push(false);
        }
        for i in (0..hex.len()).step_by(2) {
            let byte = u8::from_str_radix(&hex[i..i + 2], 16).expect("hex");
            b.set_u64(i * 4, 8, u64::from(byte));
        }
        b
    }

    fn event(t: Tsbk) -> P25Event {
        P25Event::Tsbk {
            tsbk: t,
            corrected: 0,
            crc_corrected: 0,
        }
    }

    /// Only the site's own broadcasts may be written as repeats. Block shapes
    /// from an event log; the two subscriber-bearing ones carry synthetic IDs.
    #[test]
    fn only_constant_broadcasts_may_repeat() {
        let is = |hex: &str| is_constant_broadcast(&event(parse_tsbk(&from_hex(hex), 0)));
        for (hex, what) in [
            ("ba000a30340a01103e70e839", "RFSS_STATUS_BCST"),
            ("bb000a45564034103e706988", "NET_STATUS_BCAST"),
            ("b800040c2c680c2c6801aa30", "SYS_SVC_BCAST"),
            ("bd001322d0640a251584d9c6", "IDEN_UPDATE"),
            ("9600004000000000ffff3c01", "SNDCP_DCH_ANN_EX"),
            ("81a4110dfffffffffffff79d", "HARRIS OSP 1"),
            ("8fa40a57000a00000000edda", "HARRIS OSP 15"),
            ("b0a477fe6a00008096c9409d", "HARRIS REGROUP"),
        ] {
            assert!(is(hex), "{what} is a constant broadcast");
        }
        for (hex, what) in [
            ("8200109106a7109106a72742", "GRP_VCH_GRNT_UPD"),
            ("a80080000006a88954498961", "GRP_AFFIL_RESP"),
        ] {
            assert!(!is(hex), "{what} carries a subscriber and never repeats");
        }
        assert!(!is_constant_broadcast(&P25Event::ClockResync {
            correction_ms: 0
        }));
    }

    #[test]
    fn a_unit_registration_becomes_a_radio_event() {
        let t = tsbk(44, |b| {
            b.set_u64(18, 2, 0);
            b.set_u64(20, 12, 57);
            b.set_u64(32, 24, 1_620_719);
            b.set_u64(56, 24, 1_620_719);
        });
        let ev = radio_event(&t, 1234, &BandMap::new()).expect("a radio event");
        assert_eq!(ev.kind, "unit_registration");
        assert_eq!(ev.radio_id, Some(1_620_719));
        assert_eq!(ev.radio_system, Some(57));
        assert_eq!(ev.result, Some("accepted"));
        assert_eq!(ev.at_ms, 1234);
    }

    #[test]
    fn a_group_affiliation_carries_the_radio_and_its_talkgroup() {
        let t = tsbk(40, |b| {
            b.set(16, true);
            b.set_u64(22, 2, 0);
            b.set_u64(40, 16, 52_101);
            b.set_u64(56, 24, 1_620_719);
        });
        let ev = radio_event(&t, 0, &BandMap::new()).expect("a radio event");
        assert_eq!(ev.kind, "group_affiliation");
        assert_eq!(ev.radio_id, Some(1_620_719));
        assert_eq!(ev.talkgroup, Some(52_101));
        assert_eq!(ev.result, Some("accepted"));
        assert_eq!(ev.meta["global"], true);
    }

    /// An SNDCP page names the paged subscriber at bits 56-79. The reserved
    /// field at 32-55 (0x00ffff on the air) is not a second party, so the
    /// event carries one radio id and no `target_radio_id`.
    #[test]
    fn an_sndcp_page_names_the_paged_radio_not_the_reserved_field() {
        let t = tsbk(21, |b| {
            b.set_u64(16, 8, 0x01);
            b.set_u64(32, 24, 0x00_FFFF); // reserved, as seen on the air
            b.set_u64(56, 24, 9_100_355);
        });
        let ev = radio_event(&t, 7, &BandMap::new()).expect("a radio event");
        assert_eq!(ev.kind, "sndcp_page");
        assert_eq!(ev.radio_id, Some(9_100_355));
        assert_eq!(ev.target_radio_id, None);
        assert_eq!(ev.meta["nsapi"], 1);
        let line = log_line(&event(t), Channel::Control, "t").expect("a line");
        assert_eq!(line["fields"]["target"], 9_100_355);
        assert_eq!(line["fields"]["reserved"], 0x00_FFFF);
    }

    /// The control channel's extended function command becomes a radio event
    /// too, not just a census entry: 752 of them in the 2026-09-03 window went
    /// unreported while only the traffic-channel LC copies reached Keyup.
    /// The event's shape matches the LC copy's exactly, so the two channels'
    /// views of one command read the same.
    #[test]
    fn a_control_channel_extended_function_becomes_a_radio_event() {
        let t = tsbk(36, |b| {
            b.set_u64(16, 16, 0x007D);
            b.set_u64(32, 24, 0xFF_FFFD);
            b.set_u64(56, 24, 9_000_065);
        });
        let ev = radio_event(&t, 99, &BandMap::new()).expect("a radio event");
        assert_eq!(ev.kind, "extended_function");
        assert_eq!(ev.opcode, 36);
        assert_eq!(ev.mfid, 0);
        assert_eq!(ev.radio_id, Some(9_000_065));
        assert_eq!(ev.meta["function"], "radio_detach");
        assert_eq!(ev.meta["function_code"], 0x007D);
        assert_eq!(ev.meta["arguments"], 0xFF_FFFD);

        // The same message on the traffic channel (LC opcode 23) produces the
        // same kind and the same meta — only the opcode, and so Keyup's
        // identity key, differs.
        // LCO 23: opcode, MFID, function 8-23, arguments 24-47, target 48-71.
        let t24 = 9_000_065u32.to_be_bytes();
        let word = [23, 0x00, 0x7D, 0xFF, 0xFF, 0xFD, t24[1], t24[2], t24[3]];
        let lc = LinkControl::parse(&word);
        if let LinkControl::ExtendedFunctionCommand {
            function,
            arguments,
            target,
        } = &lc
        {
            assert_eq!(function.name(), "radio_detach");
            assert_eq!(*arguments, 0xFF_FFFD);
            assert_eq!(*target, 9_000_065);
        } else {
            panic!("expected an extended function command, got {lc:?}");
        }
        let lc_ev = radio_event_lc(&lc, 23, 99).expect("a radio event");
        assert_eq!(lc_ev.kind, ev.kind);
        assert_eq!(lc_ev.meta["function"], ev.meta["function"]);
        assert_eq!(lc_ev.opcode, 23);

        let line = log_line(&event(t), Channel::Control, "t").expect("a line");
        assert_eq!(line["name"], "EXTNDED_FUNC_CMD");
        assert_eq!(line["fields"]["function"], "0x007d");
        assert_eq!(line["fields"]["function_name"], "radio_detach");
        assert_eq!(line["fields"]["arguments"], "0xfffffd");
        assert_eq!(line["fields"]["target"], 9_000_065);
    }

    #[test]
    fn a_data_grant_resolves_its_frequency_through_the_band_plan() {
        let mut nv = NonVoice::new();
        // Band 1: base 851.0 MHz, 6.25 kHz spacing — an identifier update must
        // be folded before the grant can name a frequency.
        let iden = tsbk(61, |b| {
            b.set_u64(16, 4, 1);
            b.set_u64(20, 9, 100); // 12.5 kHz bandwidth
            b.set(29, true);
            b.set_u64(30, 8, 0x80); // no transmit offset
            b.set_u64(38, 10, 50); // 6.25 kHz spacing
            b.set_u64(48, 32, 851_000_000 / 5);
        });
        let grant = tsbk(20, |b| {
            b.set_u64(24, 4, 1);
            b.set_u64(28, 12, 8);
            b.set_u64(44, 12, 4095);
            b.set_u64(56, 24, 9_000_040);
        });

        // Before the band plan is known the grant still becomes an event, just
        // without a frequency.
        assert_eq!(
            radio_event(&grant, 0, &BandMap::new())
                .expect("an event")
                .frequency_hz,
            None
        );

        nv.observe_control(&event(iden), 0);
        let ev = nv.observe_control(&event(grant), 0).expect("an event");
        assert_eq!(ev.kind, "sndcp_grant");
        assert_eq!(ev.radio_id, Some(9_000_040));
        assert_eq!(ev.frequency_hz, Some(851_000_000 + 8 * 6_250));
        assert_eq!(nv.stats_json()["data_channel"]["grants"], 1);
    }

    #[test]
    fn a_broadcast_is_counted_but_is_not_a_radio_event() {
        let mut nv = NonVoice::new();
        assert!(nv.observe_control(&event(tsbk(58, |_| {})), 0).is_none());
        assert_eq!(nv.stats_json()["message_mix"]["0:58"], 1);
    }

    #[test]
    fn the_message_mix_counts_every_tsbk_parsed_or_not() {
        let mut nv = NonVoice::new();
        for _ in 0..3 {
            nv.observe_control(&event(tsbk(56, |_| {})), 0);
        }
        nv.observe_control(&event(tsbk(0, |_| {})), 0);
        let stats = nv.stats_json();
        assert_eq!(stats["message_mix"]["0:56"], 3);
        assert_eq!(stats["message_mix"]["0:0"], 1, "a parsed grant counts too");
    }

    #[test]
    fn an_unknown_opcode_keeps_one_exemplar_with_its_count() {
        let mut nv = NonVoice::new();
        // Two identical Harris beacons: one exemplar, count 2.
        for _ in 0..2 {
            let t = tsbk(15, |b| {
                b.set_u64(8, 8, 164);
                b.set_u64(16, 16, 0x0A57);
            });
            nv.observe_control(&event(t), 0);
        }
        let stats = nv.stats_json();
        let ex = stats["unknown_exemplars"].as_array().expect("array");
        assert_eq!(ex.len(), 1);
        assert_eq!(ex[0]["mfid"], 164);
        assert_eq!(ex[0]["opcode"], 15);
        assert_eq!(ex[0]["count"], 2);
        // Byte 0 = last-block 1, opcode 15 (0b001111); byte 1 = MFID 164.
        assert_eq!(ex[0]["exemplar"], "8fa40a570000000000000000");
    }

    #[test]
    fn a_parsed_opcode_never_becomes_an_exemplar() {
        let mut nv = NonVoice::new();
        nv.observe_control(&event(tsbk(44, |_| {})), 0);
        assert!(nv.stats_json()["unknown_exemplars"]
            .as_array()
            .expect("array")
            .is_empty());
    }

    /// The two captured payloads the decoder leaves deliberately unknown, and the
    /// two it does not. `0:56` and `0:36` decode, so neither may keep an
    /// exemplar; `164:1` and `164:15` stay unknown *and* catalogued.
    #[test]
    fn the_only_unknowns_are_the_harris_beacons() {
        let mut nv = NonVoice::new();
        for hex in [
            "b800040c2c680c2c6801aa30", // 0:56 system service broadcast
            "a400007dfffffd895448e73f", // 0:36 extended function command
            "81a4110dfffffffffffff79d", // 164:1
            "8fa40a57000a00000000edda", // 164:15
        ] {
            nv.observe_control(&event(parse_tsbk(&from_hex(hex), 0)), 0);
        }
        let stats = nv.stats_json();
        let ex = stats["unknown_exemplars"].as_array().expect("array");
        let keys: Vec<(u64, u64)> = ex
            .iter()
            .map(|e| {
                (
                    e["mfid"].as_u64().expect("mfid"),
                    e["opcode"].as_u64().expect("opcode"),
                )
            })
            .collect();
        assert_eq!(keys, vec![(164, 1), (164, 15)]);
    }

    #[test]
    fn a_system_service_broadcast_logs_its_services() {
        let t = parse_tsbk(&from_hex("b800040c2c680c2c6801aa30"), 0);
        assert!(!is_unknown(&t.message));
        let line = log_line(&event(t), Channel::Control, "now").expect("a line");
        assert_eq!(line["name"], "SYS_SVC_BCAST");
        assert_eq!(line["fields"]["priority"], 1);
        assert_eq!(line["fields"]["available_raw"], "0x0c2c68");
        assert_eq!(line["fields"]["supported_raw"], "0x0c2c68");
        assert_eq!(line["fields"]["available"][0], "group_voice");
        assert_eq!(
            line["fields"]["available"].as_array().expect("array").len(),
            8
        );
    }

    /// The 0:36 exemplar Keyup still shows is stale: the parser decodes this
    /// shape, so the recorder no longer counts it unknown.
    #[test]
    fn an_extended_function_command_is_not_unknown() {
        let t = parse_tsbk(&from_hex("a400007dfffffd895448e73f"), 0);
        assert!(!is_unknown(&t.message));
        let line = log_line(&event(t), Channel::Control, "now").expect("a line");
        assert_eq!(line["name"], "EXTNDED_FUNC_CMD");
        assert_eq!(line["fields"]["function_name"], "radio_detach");
        assert_eq!(line["fields"]["target"], 9_000_008);
    }

    #[test]
    fn the_window_resets_but_the_band_plan_does_not() {
        let mut nv = NonVoice::new();
        let iden = tsbk(61, |b| {
            b.set_u64(16, 4, 1);
            b.set_u64(30, 8, 0x80);
            b.set_u64(38, 10, 50);
            b.set_u64(48, 32, 851_000_000 / 5);
        });
        nv.observe_control(&event(iden), 0);
        nv.observe_control(&event(tsbk(56, |_| {})), 0);
        nv.reset_window();

        let stats = nv.stats_json();
        assert_eq!(stats["message_mix"], json!({}));
        assert_eq!(stats["data_channel"]["grants"], 0);
        assert_eq!(stats["lsd_nonzero"], 0);

        // The band plan survives, so a grant after the reset still resolves.
        let grant = tsbk(20, |b| {
            b.set_u64(24, 4, 1);
            b.set_u64(28, 12, 8);
            b.set_u64(56, 24, 1);
        });
        assert!(nv
            .observe_control(&event(grant), 0)
            .expect("event")
            .frequency_hz
            .is_some());
    }

    #[test]
    fn link_control_is_counted_by_vendor_and_opcode() {
        let mut nv = NonVoice::new();
        // Standard LCO 0, MFID 0.
        let mut word = [0u8; 9];
        nv.observe_traffic(
            &P25Event::LinkControl {
                word,
                parsed: LinkControl::parse(&word),
                corrected: 0,
            },
            0,
        );
        // Harris LCO 50.
        word[0] = 50;
        word[1] = 164;
        nv.observe_traffic(
            &P25Event::LinkControl {
                word,
                parsed: LinkControl::parse(&word),
                corrected: 0,
            },
            0,
        );
        let mix = &nv.stats_json()["link_control_mix"];
        assert_eq!(mix["0:0"], 1);
        assert_eq!(mix["164:50"], 1);
    }

    #[test]
    fn administrative_link_control_becomes_a_radio_event() {
        let mut nv = NonVoice::new();
        // LC opcode 16 (group affiliation query), captured shape, synthetic target.
        let word = lc_word("500000895444fffffd");
        let ev = nv
            .observe_traffic(
                &P25Event::LinkControl {
                    word,
                    parsed: LinkControl::parse(&word),
                    corrected: 0,
                },
                7,
            )
            .expect("affiliation query is a radio event");
        assert_eq!(ev.kind, "group_affiliation_query");
        assert_eq!(ev.opcode, 16);
        assert_eq!(ev.radio_id, Some(0x895444));
        assert_eq!(ev.target_radio_id, Some(0xfffffd));
        assert_eq!(ev.at_ms, 7);

        // LC opcode 23 (extended function: radio detach), carried as a TDULC.
        let td = lc_word("57007dfffffd895445");
        let ev = nv
            .observe_traffic(
                &P25Event::TduLc {
                    valid: true,
                    link_control: td,
                    corrected: 0,
                },
                9,
            )
            .expect("extended function is a radio event");
        assert_eq!(ev.kind, "extended_function");
        assert_eq!(ev.opcode, 23);
        assert_eq!(ev.radio_id, Some(0x895445));
        assert_eq!(ev.meta["function"], "radio_detach");

        // An ordinary group-voice LC (opcode 0) is not a radio event.
        let gv = lc_word("0000440006a9895443");
        assert!(nv
            .observe_traffic(
                &P25Event::LinkControl {
                    word: gv,
                    parsed: LinkControl::parse(&gv),
                    corrected: 0,
                },
                1,
            )
            .is_none());
    }

    /// Parse an 18-char hex string into a 72-bit LC word.
    fn lc_word(hex: &str) -> [u8; 9] {
        let mut w = [0u8; 9];
        for (i, b) in w.iter_mut().enumerate() {
            *b = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap();
        }
        w
    }

    #[test]
    fn only_a_non_zero_correctable_lsd_octet_counts() {
        let mut nv = NonVoice::new();
        // All zero: the measured system's normal case.
        nv.observe_traffic(
            &P25Event::LowSpeedData {
                data: [0, 0],
                errors: [0, 0],
                valid: [true, true],
            },
            0,
        );
        // Non-zero but uncorrectable: not evidence of data.
        nv.observe_traffic(
            &P25Event::LowSpeedData {
                data: [0xAB, 0],
                errors: [0, 0],
                valid: [false, true],
            },
            0,
        );
        assert_eq!(nv.stats_json()["lsd_nonzero"], 0);
        nv.observe_traffic(
            &P25Event::LowSpeedData {
                data: [0xAB, 0],
                errors: [0, 0],
                valid: [true, true],
            },
            0,
        );
        assert_eq!(nv.stats_json()["lsd_nonzero"], 1);
    }

    #[test]
    fn a_log_line_names_the_message_and_keeps_its_bytes() {
        let t = tsbk(44, |b| {
            b.set_u64(18, 2, 0);
            b.set_u64(32, 24, 1_620_719);
        });
        let line = log_line(&event(t), Channel::Control, "2026-08-31T09:12:03Z").expect("a line");
        assert_eq!(line["type"], "tsbk");
        assert_eq!(line["ch"], "control");
        assert_eq!(line["name"], "UNIT_REG_RESPONS");
        assert_eq!(line["opcode"], 44);
        assert_eq!(line["fields"]["source_id"], 1_620_719);
        assert_eq!(line["fields"]["response"], "accepted");
        assert_eq!(line["hex"].as_str().expect("hex").len(), 24);
    }

    #[test]
    fn a_traffic_line_names_its_frequency() {
        let mut word = [0u8; 9];
        word[0] = 9; // LCO 9, standard
        let line = log_line(
            &P25Event::LinkControl {
                word,
                parsed: LinkControl::parse(&word),
                corrected: 0,
            },
            Channel::Traffic(851_050_000),
            "2026-08-31T09:12:03Z",
        )
        .expect("a line");
        assert_eq!(line["type"], "lc");
        assert_eq!(line["ch"], "851050000");
        assert_eq!(line["name"], "SOURCE ID EXTENSION");
    }

    /// Build an IPv4/UDP datagram to `dst_port` carrying `body`.
    fn udp_datagram(dst_port: u16, body: &[u8]) -> Vec<u8> {
        let mut d = vec![
            0x45, 0, 0, 0, 0, 0, 0, 0, 0x40, 17, 0, 0, 10, 0, 0, 1, 10, 0, 0, 2,
        ];
        d.extend_from_slice(&[0x30, 0x39]); // src port
        d.extend_from_slice(&dst_port.to_be_bytes());
        d.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // len + csum
        d.extend_from_slice(body);
        d
    }

    /// A token-guided LRRP body for (`lat`, `lon`).
    fn lrrp_body(lat: f64, lon: f64) -> Vec<u8> {
        let mut b = vec![0x0d, 0x00, 0x66];
        b.extend_from_slice(&trunkd_p25::lrrp::latitude_to_fixed(lat).to_be_bytes());
        b.extend_from_slice(&trunkd_p25::lrrp::longitude_to_fixed(lon).to_be_bytes());
        b.extend_from_slice(&0i32.to_be_bytes());
        b
    }

    fn pdu_event(llid: u32, payload: Vec<u8>) -> P25Event {
        P25Event::Pdu {
            header: PduHeader {
                confirmed: true,
                outbound: true,
                format: 22,
                vendor: 0,
                llid,
                blocks_to_follow: 2,
                ambt_opcode: None,
            },
            blocks: 2,
            valid: true,
            corrected: 0,
            payload,
        }
    }

    #[test]
    fn a_data_pdu_with_an_lrrp_fix_becomes_a_location_radio_event() {
        let mut nv = NonVoice::new();
        let (lat, lon) = (40.0, -100.0);
        let payload = udp_datagram(4001, &lrrp_body(lat, lon));
        let ev = nv
            .observe_traffic(&pdu_event(9_100_355, payload), 4242)
            .expect("an LRRP fix is a location radio event");
        assert_eq!(ev.kind, "location");
        assert_eq!(ev.radio_id, Some(9_100_355));
        assert_eq!(ev.at_ms, 4242);
        assert!((ev.meta["lat"].as_f64().unwrap() - lat).abs() < 1e-5);
        assert!((ev.meta["lon"].as_f64().unwrap() - lon).abs() < 1e-5);
        assert_eq!(ev.meta["token_guided"], true);
        let stats = nv.stats_json();
        assert_eq!(stats["data_channel"]["lrrp_transport"], 1);
        assert_eq!(stats["data_channel"]["lrrp_fixes"], 1);
        assert_eq!(stats["data_channel"]["pdu_blocks_valid"], 2);
    }

    #[test]
    fn a_data_pdu_without_a_fix_counts_but_is_not_a_location() {
        let mut nv = NonVoice::new();
        // UDP/4001 present, but the body has no decodable coordinate.
        let payload = udp_datagram(4001, &[0u8; 8]);
        assert!(nv.observe_traffic(&pdu_event(1, payload), 0).is_none());
        let stats = nv.stats_json();
        assert_eq!(stats["data_channel"]["lrrp_transport"], 1);
        assert_eq!(stats["data_channel"]["lrrp_fixes"], 0);

        // A non-data PDU (no IP/UDP at all) counts nothing on the LRRP path.
        assert!(nv
            .observe_traffic(&pdu_event(2, vec![0xde, 0xad, 0xbe, 0xef]), 0)
            .is_none());
        assert_eq!(nv.stats_json()["data_channel"]["lrrp_transport"], 1);
    }

    #[test]
    fn a_pdu_log_line_carries_payload_and_any_location() {
        let (lat, lon) = (40.0, -100.0);
        let payload = udp_datagram(4001, &lrrp_body(lat, lon));
        let line = log_line(
            &pdu_event(9_100_355, payload),
            Channel::Traffic(858_987_500),
            "2026-09-01T00:00:00Z",
        )
        .expect("a pdu line");
        assert_eq!(line["type"], "pdu");
        assert_eq!(line["fields"]["lrrp_transport"], true);
        assert!((line["fields"]["location"]["lat"].as_f64().unwrap() - lat).abs() < 1e-5);
        assert!(!line["fields"]["payload"].as_str().unwrap().is_empty());
    }

    /// An outbound location poll with the layout of the one that produced all
    /// seven false `location` records of a four-day survey (addresses and LLID
    /// synthetic, checksums and PDU CRC recomputed): SNDCP `51 00`, IPv4
    /// 10.20.2.50 -> 10.21.1.42, UDP 64414 -> 64414, a 33-octet datagram, then
    /// `0xF5` block filler and the PDU CRC. The "fix" was the filler.
    const REF_POLL: &str = "5100458800350\
25c00003c1164500a1402320a15012afb9efb9e0021bc7c0000800000000000000000000000\
2600080089546698967f00f5f5f5f5f580b98b52";

    fn ref_poll() -> Vec<u8> {
        (0..REF_POLL.len() / 2)
            .map(|i| u8::from_str_radix(&REF_POLL[i * 2..i * 2 + 2], 16).expect("hex"))
            .collect()
    }

    /// The false fix is refused, counted once, and the refusal is on the log
    /// line — beside the SNDCP/IP fields that say why it was never a fix.
    #[test]
    fn the_false_fix_poll_is_rejected_counted_and_dissected() {
        let before = lrrp_gate::rejected(lrrp_gate::Reject::BeyondDatagram);
        let mut nv = NonVoice::new();
        assert!(
            nv.observe_traffic(&pdu_event(9_000_038, ref_poll()), 0)
                .is_none(),
            "the poll padding is not a location"
        );
        assert_eq!(
            lrrp_gate::rejected(lrrp_gate::Reject::BeyondDatagram),
            before + 1,
            "and the refusal is counted, not silent"
        );
        let stats = nv.stats_json();
        assert_eq!(stats["data_channel"]["lrrp_transport"], 1);
        assert_eq!(stats["data_channel"]["lrrp_fixes"], 0);

        let line = log_line(
            &pdu_event(9_000_038, ref_poll()),
            Channel::Traffic(852_500_000),
            "2026-01-01T00:00:00.000Z",
        )
        .expect("a pdu line");
        let f = &line["fields"];
        assert!(f.get("location").is_none());
        assert_eq!(f["location_rejected"], "beyond_datagram");
        assert_eq!(f["sndcp_nsapi"], 1);
        assert_eq!(f["ip_src"], "10.20.2.50");
        assert_eq!(f["ip_dst"], "10.21.1.42");
        assert_eq!(f["udp_src"], 64414);
        assert_eq!(f["udp_dst"], 64414);
        assert_eq!(f["payload_len"], 25);
        assert!(!f["payload"].as_str().unwrap().is_empty(), "hex is kept");
    }

    /// A real-shaped LRRP response from a subscriber, inside the box, is still
    /// a fix: the gate refuses the poll padding, not location data.
    #[test]
    fn a_valid_response_inside_the_box_still_produces_a_fix() {
        use crate::pipeline::lrrp_gate::tests::{datagram, response_body};
        let (lat, lon) = (40.5, -99.5);
        let payload = datagram([10, 21, 1, 42], [10, 20, 2, 50], &response_body(lat, lon));
        let mut nv = NonVoice::new();
        let ev = nv
            .observe_traffic(&pdu_event(9_000_042, payload.clone()), 99)
            .expect("a location radio event");
        assert_eq!(ev.kind, "location");
        assert!((ev.meta["lat"].as_f64().unwrap() - lat).abs() < 1e-5);
        assert!((ev.meta["lon"].as_f64().unwrap() - lon).abs() < 1e-5);
        assert_eq!(nv.stats_json()["data_channel"]["lrrp_fixes"], 1);

        let line =
            log_line(&pdu_event(9_000_042, payload), Channel::Control, "t").expect("a pdu line");
        assert!(line["fields"].get("location_rejected").is_none());
        assert!((line["fields"]["location"]["lat"].as_f64().unwrap() - lat).abs() < 1e-5);
    }

    /// The SNDCP context accept gets its own record, so the LLID -> IPv4 map
    /// is readable without re-dissecting payload hex.
    #[test]
    fn an_sndcp_context_accept_gets_its_own_record() {
        // The layout of one from an event log (synthetic), LLID 9000041 -> 10.21.1.244.
        let payload = vec![
            0x01, 0x03, 0xf1, 0x0a, 0x15, 0x01, 0xf4, 0x00, 0x00, 0x20, 0x00, 0x00, 0x40, 0x00,
        ];
        let line = log_line(
            &pdu_event(9_000_041, payload),
            Channel::Traffic(853_750_000),
            "2026-01-01T00:00:01.000Z",
        )
        .expect("a line");
        assert_eq!(line["type"], "sndcp_context");
        assert_eq!(line["name"], "SNDCP CONTEXT ACCEPT");
        assert_eq!(line["llid"], 9_000_041);
        assert_eq!(line["ip"], "10.21.1.244");
        // The PDU fields are kept alongside, hex included.
        assert_eq!(line["fields"]["llid"], 9_000_041);
        assert!(line["fields"]["payload"]
            .as_str()
            .unwrap()
            .starts_with("0103f1"));
        // It is not an IP datagram, so there is nothing to dissect.
        assert!(line["fields"].get("ip_src").is_none());
    }

    fn ambt_header(opcode: u8, vendor: u8) -> PduHeader {
        PduHeader {
            confirmed: false,
            outbound: true,
            format: 23,
            vendor,
            llid: 9_000_001,
            blocks_to_follow: 2,
            ambt_opcode: Some(opcode),
        }
    }

    /// An AMBT header names its trunking MFID and opcode on the log line and
    /// gets its own census key, so a multi-block message is distinguishable
    /// from the single-block TSBK that shares the opcode number.
    #[test]
    fn an_ambt_header_is_named_and_censused_apart_from_the_tsbks() {
        let mut nv = NonVoice::new();
        let ambt = P25Event::PduHeader {
            header: ambt_header(0, 0),
            corrected: 0,
        };
        assert!(nv.observe_control(&ambt, 0).is_none());
        assert!(nv.observe_control(&ambt, 0).is_none());
        // A TSBK of the same (mfid, opcode) stays on its own key.
        nv.observe_control(&event(tsbk(0, |_| {})), 0);
        let stats = nv.stats_json();
        assert_eq!(stats["message_mix"]["ambt:0:0"], 2);
        assert_eq!(stats["message_mix"]["0:0"], 1);
        assert_eq!(stats["data_channel"]["pdu_headers"], 2);

        let line = log_line(&ambt, Channel::Control, "t").expect("a line");
        assert_eq!(line["type"], "pdu_header");
        assert_eq!(line["fields"]["ambt_mfid"], 0);
        assert_eq!(line["fields"]["ambt_opcode"], 0);
        assert_eq!(line["fields"]["ambt_name"], "GRP_VCH_GRANT");

        // The reassembled payload line carries them too, next to the hex.
        let pdu = P25Event::Pdu {
            header: ambt_header(0x21, 0),
            blocks: 2,
            valid: true,
            corrected: 0,
            payload: vec![0x45, 0x56, 0x40, 0x34],
        };
        let line = log_line(&pdu, Channel::Control, "t").expect("a line");
        assert_eq!(line["fields"]["ambt_opcode"], 0x21);
        assert_eq!(line["fields"]["payload"], "45564034");

        // Every other PDU format leaves the AMBT fields off entirely.
        let line = log_line(&pdu_event(1, vec![0]), Channel::Control, "t").expect("a line");
        assert!(line["fields"].get("ambt_opcode").is_none());
        nv.reset_window();
        assert!(nv.stats_json()["message_mix"]
            .as_object()
            .unwrap()
            .is_empty());
    }

    /// A unit-to-unit AMBT payload is decoded onto its own log record, so the
    /// event log carries both units, the channel and the source SUID rather
    /// than only the hex they came out of.
    #[test]
    fn a_unit_to_unit_ambt_payload_is_decoded_onto_the_log_line() {
        // The layout of an event-log payload (unit IDs synthetic): 9000001 -> 9000002 on
        // band 1, channel 0x112.
        let payload = vec![
            0x45, 0x56, 0x40, 0x34, 0x89, 0x54, 0x41, 0x89, 0x54, 0x42, 0x11, 0x12, 0xff, 0xff,
            0x56, 0x40, 0x34, 0x89, 0x54, 0x42, 0x42, 0xfc, 0x0d, 0x6b,
        ];
        let pdu = P25Event::Pdu {
            header: ambt_header(4, 0),
            blocks: 2,
            valid: true,
            corrected: 0,
            payload,
        };
        let line = log_line(&pdu, Channel::Control, "t").expect("a line");
        assert_eq!(line["fields"]["ambt_name"], "UU_VCH_GRANT");
        assert_eq!(line["fields"]["uu_source"], 9_000_001);
        assert_eq!(line["fields"]["uu_target"], 9_000_002);
        assert_eq!(line["fields"]["uu_band"], 1);
        assert_eq!(line["fields"]["uu_channel"], 0x112);
        assert_eq!(line["fields"]["uu_source_wacn"], 0x45564);
        assert_eq!(line["fields"]["uu_source_system"], 0x034);

        // A group-grant AMBT is not one, and gets none of the fields.
        let other = P25Event::Pdu {
            header: ambt_header(0, 0),
            blocks: 2,
            valid: true,
            corrected: 0,
            payload: vec![0x45, 0x56, 0x40, 0x34],
        };
        let line = log_line(&other, Channel::Control, "t").expect("a line");
        assert!(line["fields"].get("uu_source").is_none());
    }

    /// A private call is two log records and one radio event: the start shows
    /// the recorder acted on the grant, the end carries the length, and the
    /// radio event is what puts the call on both radios' timelines whether or
    /// not any audio was uploaded.
    #[test]
    fn a_private_call_is_logged_at_both_ends_and_sent_as_one_radio_event() {
        let mut nv = NonVoice::new();
        let opened = P25Event::PrivateCall {
            start: true,
            frequency_hz: 851_125_000,
            source: 9_000_001,
            target: 9_000_007,
            duration_ms: 0,
        };
        let closed = P25Event::PrivateCall {
            start: false,
            frequency_hz: 851_125_000,
            source: 9_000_001,
            target: 9_000_007,
            duration_ms: 4_200,
        };

        assert!(
            nv.observe_control(&opened, 1_788_012_000_000).is_none(),
            "the opening record is a log line only: the length is not known yet"
        );
        let line = log_line(&opened, Channel::Control, "t").expect("a line");
        assert_eq!(line["type"], "private_call");
        assert_eq!(line["name"], "PRIVATE CALL START");
        assert_eq!(line["fields"]["start"], true);
        assert_eq!(line["fields"]["source"], 9_000_001);
        assert_eq!(line["fields"]["target"], 9_000_007);
        assert_eq!(line["fields"]["frequency_hz"], 851_125_000u64);
        assert_eq!(line["fields"]["duration_ms"], 0);

        let ev = nv
            .observe_control(&closed, 1_788_012_004_200)
            .expect("a radio event");
        assert_eq!(ev.kind, "private_call");
        assert_eq!(ev.radio_id, Some(9_000_001));
        assert_eq!(ev.target_radio_id, Some(9_000_007));
        assert_eq!(ev.frequency_hz, Some(851_125_000));
        assert_eq!(ev.meta["duration_ms"], 4_200);
        assert_eq!(
            ev.meta["started_at_ms"], 1_788_012_000_000u64,
            "the call's start is the event's own instant less its length"
        );
        assert_eq!(nv.stats_json()["private_calls"], 1);

        let line = log_line(&closed, Channel::Control, "t").expect("a line");
        assert_eq!(line["name"], "PRIVATE CALL END");
        assert_eq!(line["fields"]["duration_ms"], 4_200);
    }

    #[test]
    fn voice_and_noise_never_produce_a_log_line() {
        for e in [
            P25Event::ImbeFrame {
                ldu: trunkd_p25::event::LduKind::Ldu1,
                index: 0,
                frame: [0xFF; 18],
            },
            P25Event::TsbkFailed,
            P25Event::Tdu,
            P25Event::SyncDetected { score: 1.0 },
            // An all-zero LSD pair is the system's idle state, not an event.
            P25Event::LowSpeedData {
                data: [0, 0],
                errors: [0, 0],
                valid: [true, true],
            },
        ] {
            assert!(
                log_line(&e, Channel::Control, "t").is_none(),
                "{e:?} must not be logged"
            );
        }
    }
}
