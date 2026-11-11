// SPDX-License-Identifier: GPL-3.0-or-later
//! Plausibility gate for decoded LRRP location fixes.
//!
//! The 2026-09-04 phase-3 report (`private/docs/analysis/unknown-phase3-2026-09-04.md`)
//! found seven `location` records in a four-day event log and **all seven were
//! false**: the same coordinate (-72.711650, -14.117647, South Atlantic), from
//! the same byte-identical outbound poll, flagged `token_guided`. The extractor
//! had read the `0xF5` block filler behind a 33-octet datagram as a longitude
//! word.
//!
//! The extractor itself now stops at the UDP length
//! ([`trunkd_p25::lrrp::extract_lrrp`]). This module is the second line: it
//! takes the *candidate* — including the ones the strict decode already
//! refuses — and answers whether the coordinate is plausible at all, so a
//! rejection is **counted and logged** rather than silently absent.
//!
//! What the same report established about this site is what the gate leans on:
//!
//! * every IPv4 datagram starts at octet 2 of the SNDCP payload;
//! * the only service is UDP 64414 ↔ 64414 to the FNE data host `<pool>.2.50`,
//!   with datagrams of at most 33 octets;
//! * those datagrams are outbound *polls*. The answers ride the uplink, which a
//!   downlink-only recorder cannot hear — so a fix decoded from a datagram the
//!   FNE host sent is, by construction, not a fix.
//!
//! The gate is a process-global: one `[recorder] location_bounds` for the
//! recorder, and one short ring of recent coordinates shared by the radio-event
//! path and the event-log path, so the two cannot disagree about whether a
//! coordinate is a repeat. PDUs arrive a few thousand times a day on the
//! observations thread, so the mutex is free.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use trunkd_core::Metrics;
use trunkd_p25::lrrp::{self, LrrpCandidate};

/// Sanity box a fix must land in when `[recorder] location_bounds` is left at
/// its default: a generous CONUS rectangle, `[lat_min, lon_min, lat_max,
/// lon_max]`.
pub(crate) const DEFAULT_LOCATION_BOUNDS: [f64; 4] = [24.0, -125.0, 50.0, -66.0];

/// Largest datagram the L3Harris location service uses on this site (1,373 of
/// 1,376 in the phase-3 corpus). Anything this small that does not open with an
/// LRRP response type is a poll, not an answer.
const POLL_MAX_PAYLOAD: usize = 33;

/// Distinct non-zero octets a datagram body must carry before a coordinate
/// found inside it is believable. The constant poll shapes of the phase-3
/// corpus (`0000800000000000000000000026000800` and friends) carry three.
const MIN_DISTINCT_NONZERO: usize = 4;

/// Octet values a reassembled PDU pads with, or that a zeroed field decodes to.
/// A coordinate word made only of these is padding, not a position.
const PADDING_OCTETS: [u8; 3] = [0x00, 0xff, 0xf5];

/// LRRP message types that introduce a location *response*.
///
/// Motorola's immediate/triggered location responses. The exact set is not
/// pinned down for L3Harris — phase-3 "still open" §4, the answers being on the
/// uplink — so this is used only to spare a small datagram from the poll-shape
/// rule, never to accept one.
const LRRP_RESPONSE_TYPES: [u8; 5] = [0x07, 0x09, 0x0b, 0x0d, 0x0f];

/// How many recent coordinates the repeat check remembers.
const RING: usize = 16;

/// Why a candidate fix was thrown away.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Reject {
    /// The coordinate words lie past the datagram's UDP length: block filler.
    BeyondDatagram,
    /// Both words decode to exactly zero.
    NullCoordinate,
    /// The datagram body is one of the constant poll shapes, or a coordinate
    /// word is nothing but padding octets.
    ConstantPoll,
    /// The datagram came *from* the FNE data host — an outbound request.
    OutboundPoll,
    /// A poll-sized datagram that does not open with an LRRP response type.
    PollShaped,
    /// Outside the configured `[recorder] location_bounds` box.
    OutOfBounds,
    /// A coordinate already seen, byte-identical, from a different radio.
    RepeatFix,
}

impl Reject {
    /// Every reason, in the order they are tested. Also the order of
    /// [`REJECTED`].
    pub(crate) const ALL: [Reject; 7] = [
        Reject::BeyondDatagram,
        Reject::NullCoordinate,
        Reject::ConstantPoll,
        Reject::OutboundPoll,
        Reject::PollShaped,
        Reject::OutOfBounds,
        Reject::RepeatFix,
    ];

    /// The reason label used in logs, in the event-log record and in the
    /// `lrrp.rejected.<reason>` metric.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Reject::BeyondDatagram => "beyond_datagram",
            Reject::NullCoordinate => "null_coordinate",
            Reject::ConstantPoll => "constant_poll",
            Reject::OutboundPoll => "outbound_poll",
            Reject::PollShaped => "poll_shaped",
            Reject::OutOfBounds => "out_of_bounds",
            Reject::RepeatFix => "repeat_fix",
        }
    }

    /// Index into [`REJECTED`].
    fn slot(self) -> usize {
        Reject::ALL.iter().position(|r| *r == self).unwrap_or(0)
    }
}

/// Per-reason rejection counts, mirrored into the metrics registry by
/// [`publish_metrics`].
static REJECTED: [AtomicU64; 7] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];

/// The gate's mutable state: the configured box and the recent-fix ring.
struct GateState {
    /// `[lat_min, lon_min, lat_max, lon_max]`, or `None` for "no box".
    bounds: Option<[f64; 4]>,
    /// `(lat_raw, lon_raw, llid)` for the last [`RING`] admitted candidates.
    recent: VecDeque<(i32, i32, u32)>,
}

static GATE: Mutex<GateState> = Mutex::new(GateState {
    bounds: Some(DEFAULT_LOCATION_BOUNDS),
    recent: VecDeque::new(),
});

/// Install the configured sanity box. `None` — an empty `location_bounds`
/// list — turns the box off; every other rule still applies.
pub(crate) fn configure(bounds: Option<[f64; 4]>) {
    if let Ok(mut g) = GATE.lock() {
        g.bounds = bounds;
        g.recent.clear();
    }
}

/// Judge one candidate fix decoded from `payload`, reported by radio `llid`.
///
/// `Ok(())` means the coordinate is worth emitting. `Err(reason)` means it is
/// not: the reason is counted in `lrrp.rejected.<reason>` and logged at DEBUG,
/// which is what lets the phase-3 tool see that the gate is working rather than
/// that the site went quiet.
pub(crate) fn admit(payload: &[u8], llid: u32, candidate: &LrrpCandidate) -> Result<(), Reject> {
    let verdict = {
        let mut g = match GATE.lock() {
            Ok(g) => g,
            // A poisoned gate must not become a source of fixes.
            Err(_) => return Err(Reject::RepeatFix),
        };
        judge(&mut g, payload, llid, candidate)
    };
    if let Err(reason) = verdict {
        let count = REJECTED[reason.slot()].fetch_add(1, Ordering::Relaxed) + 1;
        tracing::debug!(
            radio = llid,
            lat = candidate.fix.latitude,
            lon = candidate.fix.longitude,
            reason = reason.as_str(),
            count,
            "LRRP location candidate rejected"
        );
    }
    verdict
}

/// Judge a candidate against an explicit gate state.
///
/// The checks run cheapest-and-most-decisive first, and the first one that
/// fires is the reason reported: a coordinate read out of block filler is
/// `beyond_datagram` even though it is also out of bounds.
fn judge(
    g: &mut GateState,
    payload: &[u8],
    llid: u32,
    candidate: &LrrpCandidate,
) -> Result<(), Reject> {
    let fix = candidate.fix;
    if !candidate.within_udp_length {
        return Err(Reject::BeyondDatagram);
    }
    if fix.lat_raw == 0 && fix.lon_raw == 0 {
        return Err(Reject::NullCoordinate);
    }
    if is_padding_word(fix.lat_raw) || is_padding_word(fix.lon_raw) {
        return Err(Reject::ConstantPoll);
    }
    if let Some(d) = lrrp::parse_sndcp_datagram(payload) {
        if lrrp::is_fne_host(d.ip_src) {
            return Err(Reject::OutboundPoll);
        }
        if let Some(body) = body_of(payload, &d) {
            if distinct_nonzero(body) < MIN_DISTINCT_NONZERO {
                return Err(Reject::ConstantPoll);
            }
            let answers = body
                .first()
                .is_some_and(|t| LRRP_RESPONSE_TYPES.contains(t));
            if body.len() <= POLL_MAX_PAYLOAD && !answers {
                return Err(Reject::PollShaped);
            }
        }
    }
    if let Some([lat_min, lon_min, lat_max, lon_max]) = g.bounds {
        let inside = (lat_min..=lat_max).contains(&fix.latitude)
            && (lon_min..=lon_max).contains(&fix.longitude);
        if !inside {
            return Err(Reject::OutOfBounds);
        }
    }
    if g.recent
        .iter()
        .any(|(lat, lon, id)| *lat == fix.lat_raw && *lon == fix.lon_raw && *id != llid)
    {
        return Err(Reject::RepeatFix);
    }
    if g.recent.len() == RING {
        g.recent.pop_front();
    }
    g.recent.push_back((fix.lat_raw, fix.lon_raw, llid));
    Ok(())
}

/// The transport payload of `d` inside `payload`, when it is UDP.
fn body_of<'a>(payload: &'a [u8], d: &lrrp::SndcpDatagram) -> Option<&'a [u8]> {
    let off = d.body_off?;
    let len = d.payload_len?;
    payload.get(off..off.checked_add(len)?)
}

/// How many distinct non-zero octet values a body carries.
fn distinct_nonzero(body: &[u8]) -> usize {
    let mut seen = [false; 256];
    let mut n = 0;
    for &b in body {
        if b != 0 && !seen[b as usize] {
            seen[b as usize] = true;
            n += 1;
        }
    }
    n
}

/// True when every octet of a coordinate word is padding or filler.
fn is_padding_word(v: i32) -> bool {
    v.to_be_bytes().iter().all(|b| PADDING_OCTETS.contains(b))
}

/// Current rejection count for one reason.
pub(crate) fn rejected(reason: Reject) -> u64 {
    REJECTED[reason.slot()].load(Ordering::Relaxed)
}

/// Mirror the rejection counts into the metrics registry as
/// `lrrp.rejected.<reason>`.
///
/// The gate has no registry of its own — it is reached from free functions on
/// the observations thread — so the counts are copied across whenever the
/// registry is read.
pub(crate) fn publish_metrics(metrics: &Metrics) {
    for reason in Reject::ALL {
        metrics
            .counter(&format!("lrrp.rejected.{}", reason.as_str()))
            .set(rejected(reason));
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use trunkd_p25::lrrp::{latitude_to_fixed, longitude_to_fixed};

    /// Build the SNDCP header, IPv4 header and UDP header of a location
    /// datagram around `body`, the way this site frames one.
    pub(crate) fn datagram(src: [u8; 4], dst: [u8; 4], body: &[u8]) -> Vec<u8> {
        let mut p = vec![0x51, 0x00]; // SNDCP: PDU type 5, NSAPI 1
        p.push(0x45); // IPv4, IHL 5
        p.push(0x88);
        p.extend_from_slice(&((20 + 8 + body.len()) as u16).to_be_bytes());
        p.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // id + flags/frag
        p.push(0x3c); // ttl
        p.push(17); // UDP
        p.extend_from_slice(&[0x00, 0x00]); // header checksum, unchecked
        p.extend_from_slice(&src);
        p.extend_from_slice(&dst);
        p.extend_from_slice(&0xfb9eu16.to_be_bytes()); // src port 64414
        p.extend_from_slice(&0xfb9eu16.to_be_bytes()); // dst port 64414
        p.extend_from_slice(&((8 + body.len()) as u16).to_be_bytes());
        p.extend_from_slice(&[0x00, 0x00]); // UDP checksum, unchecked
        p.extend_from_slice(body);
        p
    }

    /// An LRRP immediate-location *response* body carrying a fix.
    pub(crate) fn response_body(lat: f64, lon: f64) -> Vec<u8> {
        let mut b = vec![0x07, 0x12, 0x34, 0x66];
        b.extend_from_slice(&latitude_to_fixed(lat).to_be_bytes());
        b.extend_from_slice(&longitude_to_fixed(lon).to_be_bytes());
        b.extend_from_slice(&[0x69, 0x00, 0x00, 0x0a, 0x55, 0x21]);
        b
    }

    /// A gate state private to one test: the process-global one is shared with
    /// the recorder threads and with every other test.
    fn state(bounds: Option<[f64; 4]>) -> GateState {
        GateState {
            bounds,
            recent: VecDeque::new(),
        }
    }

    /// A response from a subscriber address, inside the default box, is a fix.
    #[test]
    fn a_plausible_response_is_admitted() {
        let mut g = state(Some(DEFAULT_LOCATION_BOUNDS));
        let p = datagram(
            [10, 21, 1, 42],
            [10, 20, 2, 50],
            &response_body(40.0, -100.0),
        );
        let c = lrrp::extract_lrrp_candidate(&p).expect("a candidate");
        assert_eq!(judge(&mut g, &p, 9_000_038, &c), Ok(()));
    }

    /// The same coordinate from a *different* radio is a repeat; from the same
    /// radio it is a radio that has not moved.
    #[test]
    fn a_byte_identical_repeat_from_another_radio_is_refused() {
        let mut g = state(Some(DEFAULT_LOCATION_BOUNDS));
        let p = datagram(
            [10, 21, 1, 43],
            [10, 20, 2, 50],
            &response_body(39.0, -101.0),
        );
        let c = lrrp::extract_lrrp_candidate(&p).expect("a candidate");
        assert_eq!(judge(&mut g, &p, 1, &c), Ok(()));
        assert_eq!(judge(&mut g, &p, 1, &c), Ok(()));
        assert_eq!(judge(&mut g, &p, 2, &c), Err(Reject::RepeatFix));
    }

    /// A datagram the FNE host sent is a poll, whatever decodes out of it.
    #[test]
    fn a_datagram_from_the_fne_host_is_a_poll() {
        let mut g = state(Some(DEFAULT_LOCATION_BOUNDS));
        let p = datagram(
            [10, 20, 2, 50],
            [10, 21, 1, 42],
            &response_body(40.0, -100.0),
        );
        let c = lrrp::extract_lrrp_candidate(&p).expect("a candidate");
        assert_eq!(judge(&mut g, &p, 9_000_038, &c), Err(Reject::OutboundPoll));
    }

    /// A coordinate outside the box is refused; with the box off it is not.
    #[test]
    fn the_sanity_box_is_configurable() {
        let p = datagram(
            [10, 21, 1, 44],
            [10, 20, 2, 50],
            &response_body(-30.0, 150.0),
        );
        let c = lrrp::extract_lrrp_candidate(&p).expect("a candidate");
        let mut boxed = state(Some(DEFAULT_LOCATION_BOUNDS));
        assert_eq!(judge(&mut boxed, &p, 7, &c), Err(Reject::OutOfBounds));
        let mut open = state(None);
        assert_eq!(
            judge(&mut open, &p, 7, &c),
            Ok(()),
            "an empty list means no box"
        );
    }

    /// A poll-sized datagram that does not open with a response type is a poll.
    #[test]
    fn a_poll_shaped_datagram_is_refused() {
        let mut g = state(Some(DEFAULT_LOCATION_BOUNDS));
        let mut body = response_body(40.0, -100.0);
        body[0] = 0x00; // not an LRRP response type
        let p = datagram([10, 21, 1, 45], [10, 20, 2, 50], &body);
        let c = lrrp::extract_lrrp_candidate(&p).expect("a candidate");
        assert_eq!(judge(&mut g, &p, 8, &c), Err(Reject::PollShaped));
    }

    /// A body whose only content is framing and a coordinate is a poll shape:
    /// too few distinct non-zero octets to be carrying anything.
    #[test]
    fn a_sparse_body_is_refused_as_a_constant_poll() {
        let mut g = state(None);
        let mut body = vec![0x07, 0x00, 0x00, 0x00, 0x00, 0x00, 0x66];
        body.extend_from_slice(&[0x20, 0x20, 0x20, 0x20]); // "latitude"
        body.extend_from_slice(&[0x20, 0x20, 0x20, 0x20]); // "longitude"
        assert!(distinct_nonzero(&body) < MIN_DISTINCT_NONZERO);
        let p = datagram([10, 21, 1, 46], [10, 20, 2, 50], &body);
        let c = lrrp::extract_lrrp_candidate(&p).expect("a candidate");
        assert_eq!(judge(&mut g, &p, 9, &c), Err(Reject::ConstantPoll));
    }

    /// Filler words never pass, whatever else the datagram looks like.
    #[test]
    fn padding_words_are_not_coordinates() {
        assert!(is_padding_word(i32::from_be_bytes([
            0xf5, 0xf5, 0xf5, 0xf5
        ])));
        assert!(is_padding_word(0));
        assert!(!is_padding_word(latitude_to_fixed(40.0)));
    }

    /// Every reason has a distinct slot and a distinct label.
    #[test]
    fn reasons_are_distinct() {
        for (i, r) in Reject::ALL.iter().enumerate() {
            assert_eq!(r.slot(), i, "{}", r.as_str());
        }
    }
}
