// SPDX-License-Identifier: GPL-3.0-or-later
//! Best-effort LRRP (Location Request/Response Protocol) extraction from
//! reassembled SNDCP user-data PDU payloads.
//!
//! This is a **prototype-grade heuristic**, not a conformant SNDCP/LRRP stack. It
//! answers one question: does a P25 data PDU on this system carry a decodable
//! GPS fix? It never allocates, never panics on garbage, and returns `None`
//! whenever the bytes do not look like a location.
//!
//! # What it looks for
//!
//! LRRP is an IP datagram — UDP — carried inside the SNDCP user-data octets. The
//! extractor:
//!
//! 1. scans the payload for a plausible IPv4 header (version 4, sane IHL);
//! 2. if the datagram is **UDP** to a known LRRP port ([`LRRP_PORTS`]: Motorola
//!    4001, or the L3Harris port 64414 observed on a test capture), treats the UDP payload
//!    as the LRRP body;
//! 3. if the datagram is **ICMP** "destination unreachable / port unreachable"
//!    (type 3, code 3), recovers the *original* datagram quoted inside the ICMP
//!    payload and re-checks it (the known LRRP-in-ICMP quirk);
//! 4. parses the LRRP body for a latitude/longitude pair.
//!
//! # Coordinate encoding
//!
//! Coordinates are 32-bit fixed-point. A signed big-endian `i32` maps the full
//! range onto the earth:
//!
//! ```text
//! latitude  = value * 180 / 2^32   (value in [-2^31, 2^31) → [-90, 90) degrees)
//! longitude = value * 360 / 2^32   (value → [-180, 180) degrees)
//! ```
//!
//! Motorola LRRP tags the coordinate triplet with a leading token byte
//! (`0x66`/`0x67`) before three big-endian words (lat, lon, and a
//! height/accuracy word). [`extract_lrrp`] — the function the pipeline uses —
//! decodes **only** token-guided coordinates, because a blind byte scan
//! false-positives on real data (it read IP-address bytes of a captured location poll
//! as a fix). [`extract_lrrp_scan`] adds that bounded scan for offline
//! exploration of an unknown body layout, and is never wired into the live path.

/// Motorola's well-known LRRP UDP port.
pub const LRRP_PORT: u16 = 4001;

/// UDP destination ports that carry LRRP on the systems we have observed.
///
/// `4001` is Motorola's documented port. `64414` (`0xfb9e`) is what **an L3Harris
/// system** actually uses: replaying a wideband test capture through the
/// data-channel follower reassembled outbound SNDCP PDUs whose IP/UDP
/// datagrams (FNE host `<pool>.2.50` to a subscriber address) were UDP src==dst
/// 64414, never 4001. This is the "field details may differ on Harris" that needed
/// pinning down — a 4001-only filter finds nothing on such a system (matching a
/// deployment's event log: heavy data, zero port-4001 payloads).
pub const LRRP_PORTS: &[u16] = &[4001, 0xfb9e];

/// True when `port` is a known LRRP transport port.
#[inline]
pub fn is_lrrp_port(port: u16) -> bool {
    LRRP_PORTS.contains(&port)
}

/// A decoded location fix.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LrrpFix {
    /// Latitude in degrees, `[-90, 90)`.
    pub latitude: f64,
    /// Longitude in degrees, `[-180, 180)`.
    pub longitude: f64,
    /// The raw signed lat word, for auditing the decode.
    pub lat_raw: i32,
    /// The raw signed lon word.
    pub lon_raw: i32,
    /// UDP destination port the datagram was addressed to (one of
    /// [`LRRP_PORTS`]).
    pub port: u16,
    /// True when the fix came from the ICMP-wrapped fallback path.
    pub via_icmp: bool,
    /// True when a coordinate token (`0x66`/`0x67`) preceded the words; a
    /// token-guided decode is far less likely to be a false positive than the
    /// bounded scan.
    pub token_guided: bool,
}

/// Convert a signed 32-bit fixed-point word to degrees latitude.
#[inline]
pub fn fixed_to_latitude(v: i32) -> f64 {
    f64::from(v) * 180.0 / 4_294_967_296.0
}

/// Convert a signed 32-bit fixed-point word to degrees longitude.
#[inline]
pub fn fixed_to_longitude(v: i32) -> f64 {
    f64::from(v) * 360.0 / 4_294_967_296.0
}

/// Inverse of [`fixed_to_latitude`] — for building test vectors.
#[inline]
pub fn latitude_to_fixed(deg: f64) -> i32 {
    (deg / 180.0 * 4_294_967_296.0).round() as i32
}

/// Inverse of [`fixed_to_longitude`] — for building test vectors.
#[inline]
pub fn longitude_to_fixed(deg: f64) -> i32 {
    (deg / 360.0 * 4_294_967_296.0).round() as i32
}

/// Read a big-endian `i32` at `off`, or `None` if it runs off the end.
#[inline]
fn be_i32(b: &[u8], off: usize) -> Option<i32> {
    let e = off.checked_add(4)?;
    let s = b.get(off..e)?;
    Some(i32::from_be_bytes([s[0], s[1], s[2], s[3]]))
}

/// Read a big-endian `u16` at `off`.
#[inline]
fn be_u16(b: &[u8], off: usize) -> Option<u16> {
    let e = off.checked_add(2)?;
    let s = b.get(off..e)?;
    Some(u16::from_be_bytes([s[0], s[1]]))
}

/// Octets of SNDCP header that precede the IP datagram on this system.
///
/// An early survey found "no payload that parses as IPv4" because it only looked
/// at octet 0. A later one measured 1,376 datagrams on two independent recorders:
/// **every** IPv4 header starts at octet 2, and none checksums at octet 0. The two
/// octets in front are the SNDCP header.
pub const SNDCP_HEADER_LEN: usize = 2;

/// The IPv4 address the FNE data host occupies in every observed pool:
/// `<pool>.2.50` (e.g. 10.20.2.50 and 10.30.2.50 for two pools). 1,374 of 1,376
/// datagrams have one of those as an endpoint, and none has two subscriber
/// addresses — it is the poll/response server, not a peer radio.
pub const FNE_HOST_SUFFIX: [u8; 2] = [2, 50];

/// True when `ip` looks like the FNE data host (`<pool>.2.50`).
#[inline]
pub fn is_fne_host(ip: [u8; 4]) -> bool {
    ip[2] == FNE_HOST_SUFFIX[0] && ip[3] == FNE_HOST_SUFFIX[1]
}

/// The SNDCP header and IP/UDP envelope of a reassembled data PDU payload.
///
/// This is deliberately a *description*, not a decode: it names the fields the
/// identifier registry needs (NSAPI, the two addresses, the two ports, the
/// transport payload length) and leaves the body alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SndcpDatagram {
    /// Low nibble of SNDCP octet 0 — the NSAPI, when the header is present.
    /// `None` when the datagram was found at offset 0 with no header in front.
    pub nsapi: Option<u8>,
    /// High nibble of SNDCP octet 0 — the SNDCP PDU type, when present.
    pub pdu_type: Option<u8>,
    /// Offset of the IPv4 header within the PDU payload (2, or 0).
    pub ip_off: usize,
    /// IPv4 protocol number (17 UDP, 1 ICMP, …).
    pub protocol: u8,
    /// IPv4 source address.
    pub ip_src: [u8; 4],
    /// IPv4 destination address.
    pub ip_dst: [u8; 4],
    /// UDP source port, when the protocol is UDP.
    pub udp_src: Option<u16>,
    /// UDP destination port, when the protocol is UDP.
    pub udp_dst: Option<u16>,
    /// Transport payload length in octets (UDP length minus its 8-octet
    /// header), when the UDP length field is usable.
    pub payload_len: Option<usize>,
    /// Offset of the transport payload within the PDU payload, when the
    /// protocol is UDP.
    pub body_off: Option<usize>,
}

/// Parse the two-octet SNDCP header and the IPv4 (and UDP) envelope of a
/// reassembled data-PDU payload.
///
/// Tries offset [`SNDCP_HEADER_LEN`] first — where every datagram in the
/// measured corpus lives — then offset 0, so a system that does not use the
/// header still parses. Returns `None` when neither offset holds a sane IPv4
/// header.
pub fn parse_sndcp_datagram(payload: &[u8]) -> Option<SndcpDatagram> {
    [SNDCP_HEADER_LEN, 0]
        .into_iter()
        .find_map(|off| describe_ip(payload, off))
}

/// Describe the IPv4 datagram at `off`, or `None` if there is not one there.
fn describe_ip(b: &[u8], off: usize) -> Option<SndcpDatagram> {
    let first = *b.get(off)?;
    if first >> 4 != 4 {
        return None;
    }
    let ihl = (first & 0x0f) as usize;
    if !(5..=15).contains(&ihl) {
        return None;
    }
    let ip_end = off.checked_add(ihl * 4)?;
    if ip_end > b.len() {
        return None;
    }
    let protocol = *b.get(off + 9)?;
    let ip_src = [
        *b.get(off + 12)?,
        *b.get(off + 13)?,
        *b.get(off + 14)?,
        *b.get(off + 15)?,
    ];
    let ip_dst = [
        *b.get(off + 16)?,
        *b.get(off + 17)?,
        *b.get(off + 18)?,
        *b.get(off + 19)?,
    ];
    let (nsapi, pdu_type) = if off == SNDCP_HEADER_LEN {
        let h = *b.first()?;
        (Some(h & 0x0f), Some(h >> 4))
    } else {
        (None, None)
    };
    let mut d = SndcpDatagram {
        nsapi,
        pdu_type,
        ip_off: off,
        protocol,
        ip_src,
        ip_dst,
        udp_src: None,
        udp_dst: None,
        payload_len: None,
        body_off: None,
    };
    if protocol == 17 {
        d.udp_src = be_u16(b, ip_end);
        d.udp_dst = be_u16(b, ip_end + 2);
        d.body_off = Some(ip_end + 8);
        d.payload_len = udp_body(b, ip_end, true).map(<[u8]>::len);
    }
    Some(d)
}

/// The IPv4 address an SNDCP **context accept** assigns to the radio whose LLID
/// is in the PDU header.
///
/// The payload is `01 03 F1 <a.b.c.d>` — 636 of them in one day on a busy site, and
/// every datagram whose LLID has one carries that address as an endpoint. This
/// is the LLID → IPv4 map, straight off the downlink.
pub fn parse_sndcp_context_accept(payload: &[u8]) -> Option<[u8; 4]> {
    let p = payload.get(..7)?;
    if p[0] != 0x01 || p[1] != 0x03 || p[2] != 0xf1 {
        return None;
    }
    Some([p[3], p[4], p[5], p[6]])
}

/// Locate and decode an LRRP fix in a reassembled PDU payload — **high
/// confidence** only.
///
/// Finds a UDP datagram to a known LRRP port (directly or ICMP-wrapped) and
/// decodes a coordinate **only** when it is introduced by a Motorola coordinate
/// token (`0x66`/`0x67`). It does **not** run the bounded byte scan, because on
/// real data that scan mistakes IP-address and payload bytes for coordinates
/// (verified against an outbound PDU from a test capture). This is the function the
/// pipeline uses, so a reported location is trustworthy.
pub fn extract_lrrp(payload: &[u8]) -> Option<LrrpFix> {
    (0..payload.len()).find_map(|start| try_ip_datagram(payload, start, false, Opts::STRICT))
}

/// A coordinate the extractor *found*, together with whether it survives the
/// UDP length bound — everything a caller needs to decide whether to believe it.
///
/// [`extract_lrrp`] is the strict answer and is what the pipeline emits. This
/// is the strict answer plus the near-misses, so the recorder can **count and
/// log the rejections** instead of silently finding nothing: the seven false
/// fixes of one four-day survey were all coordinates whose longitude
/// word lay in the `0xF5` block filler behind a 33-octet poll, and a gate that
/// cannot see them cannot report that it stopped them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LrrpCandidate {
    /// The decoded coordinate.
    pub fix: LrrpFix,
    /// False when the coordinate words run past the datagram's UDP length —
    /// i.e. the decode read reassembly padding, not payload.
    pub within_udp_length: bool,
}

/// Find a coordinate candidate, ignoring the UDP length bound, and report
/// whether the strict decode agrees with it.
///
/// Callers that want only trustworthy fixes should use [`extract_lrrp`]. This
/// exists for the recorder's location gate, which wants the rejected ones too.
pub fn extract_lrrp_candidate(payload: &[u8]) -> Option<LrrpCandidate> {
    let fix = (0..payload.len())
        .find_map(|start| try_ip_datagram(payload, start, false, Opts::UNBOUNDED))?;
    Some(LrrpCandidate {
        within_udp_length: extract_lrrp(payload) == Some(fix),
        fix,
    })
}

/// Like [`extract_lrrp`], but additionally runs the **low-confidence** bounded
/// byte scan when no coordinate token is present — for offline exploration of an
/// unknown (e.g. L3Harris) body layout. High false-positive rate: never wire
/// this into the live path. A fix it returns has `token_guided == false`.
pub fn extract_lrrp_scan(payload: &[u8]) -> Option<LrrpFix> {
    (0..payload.len()).find_map(|start| try_ip_datagram(payload, start, false, Opts::SCAN))
}

/// How much licence the extractor is given on one payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Opts {
    /// Fall back to the low-confidence bounded byte scan when no coordinate
    /// token is present.
    scan: bool,
    /// Stop the body at the datagram's UDP length instead of at the end of the
    /// reassembled payload.
    bounded: bool,
}

impl Opts {
    /// Token-guided only, body bounded by the UDP length: what the pipeline uses.
    const STRICT: Opts = Opts {
        scan: false,
        bounded: true,
    };
    /// Token-guided only, body unbounded: the pre-gate behaviour, kept so the
    /// gate can see (and count) what it rejects.
    const UNBOUNDED: Opts = Opts {
        scan: false,
        bounded: false,
    };
    /// Offline exploration: byte scan, unbounded body.
    const SCAN: Opts = Opts {
        scan: true,
        bounded: false,
    };
}

/// True when the payload carries a UDP datagram (directly, or quoted inside an
/// ICMP port-unreachable) destined for a known LRRP port ([`LRRP_PORTS`]),
/// regardless of whether a coordinate could be decoded from it. Lets a caller
/// distinguish "LRRP transport is present" from "a fix was decodable".
pub fn contains_lrrp_transport(payload: &[u8]) -> bool {
    (0..payload.len()).any(|start| has_lrrp_udp(payload, start, false))
}

/// Whether an IPv4 datagram at `start` is (or quotes) UDP to an LRRP port.
fn has_lrrp_udp(b: &[u8], start: usize, via_icmp: bool) -> bool {
    let Some(&first) = b.get(start) else {
        return false;
    };
    if first >> 4 != 4 {
        return false;
    }
    let ihl = (first & 0x0f) as usize;
    if !(5..=15).contains(&ihl) {
        return false;
    }
    let Some(ip_end) = start.checked_add(ihl * 4) else {
        return false;
    };
    if ip_end > b.len() {
        return false;
    }
    match b.get(start + 9) {
        Some(17) => be_u16(b, ip_end + 2).is_some_and(is_lrrp_port),
        Some(1) if !via_icmp => {
            b.get(ip_end) == Some(&3)
                && b.get(ip_end + 1) == Some(&3)
                && ip_end
                    .checked_add(8)
                    .is_some_and(|inner| has_lrrp_udp(b, inner, true))
        }
        _ => false,
    }
}

/// Try to read an IPv4 datagram at `start`. `via_icmp` marks a recursion into
/// an ICMP-quoted original datagram (bounded to one level); `scan` enables the
/// low-confidence bounded body scan.
fn try_ip_datagram(b: &[u8], start: usize, via_icmp: bool, opts: Opts) -> Option<LrrpFix> {
    let first = *b.get(start)?;
    // IPv4, IHL in [5, 15] words.
    if first >> 4 != 4 {
        return None;
    }
    let ihl = (first & 0x0f) as usize;
    if !(5..=15).contains(&ihl) {
        return None;
    }
    let ip_hdr_len = ihl * 4;
    let ip_end = start.checked_add(ip_hdr_len)?;
    if ip_end > b.len() {
        return None;
    }
    let protocol = *b.get(start + 9)?;
    match protocol {
        // UDP.
        17 => try_udp(b, ip_end, via_icmp, opts),
        // ICMP: only the port-unreachable case wraps an LRRP datagram.
        1 if !via_icmp => {
            let icmp_type = *b.get(ip_end)?;
            let icmp_code = *b.get(ip_end + 1)?;
            if icmp_type != 3 || icmp_code != 3 {
                return None;
            }
            // ICMP header is 8 bytes; the quoted original IP datagram follows.
            let inner = ip_end.checked_add(8)?;
            try_ip_datagram(b, inner, true, opts)
        }
        _ => None,
    }
}

/// Parse a UDP datagram starting at `udp_start`; decode the body when the
/// destination port is a known LRRP port.
fn try_udp(b: &[u8], udp_start: usize, via_icmp: bool, opts: Opts) -> Option<LrrpFix> {
    let dst_port = be_u16(b, udp_start + 2)?;
    if !is_lrrp_port(dst_port) {
        return None;
    }
    let body = udp_body(b, udp_start, opts.bounded)?;
    let (lat_raw, lon_raw, token_guided) = parse_lrrp_body(body, opts.scan)?;
    Some(LrrpFix {
        latitude: fixed_to_latitude(lat_raw),
        longitude: fixed_to_longitude(lon_raw),
        lat_raw,
        lon_raw,
        port: dst_port,
        via_icmp,
        token_guided,
    })
}

/// The UDP body of the datagram whose header starts at `udp_start`, **bounded
/// by the UDP length field**.
///
/// The bound is not cosmetic. The L3Harris location poll seen on a test capture is a 33-octet
/// datagram (25 octets of body) that a reassembled PDU pads out to the block
/// boundary with the system's `0xF5` filler. Reading past the UDP length let
/// the coordinate scan take `f5 f5 f5 f5` — filler — as a longitude word, which
/// is exactly the constant (-72.711650, -14.117647) "fix" the four-day event
/// log reported seven times. When the length field is unusable (too small, or
/// longer than what was reassembled) the remaining octets are used, as before.
fn udp_body(b: &[u8], udp_start: usize, bounded: bool) -> Option<&[u8]> {
    let body_start = udp_start.checked_add(8)?;
    let avail = b.len().checked_sub(body_start)?;
    let declared = be_u16(b, udp_start + 4).map(usize::from);
    let len = match declared {
        Some(l) if bounded && (8..=avail + 8).contains(&l) => l - 8,
        _ => avail,
    };
    b.get(body_start..body_start + len)
}

/// True when a fixed-point pair lands inside valid lat/lon ranges.
#[inline]
fn in_range(lat_raw: i32, lon_raw: i32) -> bool {
    let lat = fixed_to_latitude(lat_raw);
    let lon = fixed_to_longitude(lon_raw);
    (-90.0..90.0).contains(&lat) && (-180.0..180.0).contains(&lon)
}

/// Find a `(lat_raw, lon_raw, token_guided)` coordinate pair in an LRRP body.
///
/// Always honours the Motorola coordinate token (`0x66`/`0x67` immediately
/// followed by two in-range words). When `scan` is set it additionally falls
/// back to a bounded scan for any adjacent in-range `i32` pair — high
/// false-positive, offline-only (see [`extract_lrrp_scan`]).
fn parse_lrrp_body(body: &[u8], scan: bool) -> Option<(i32, i32, bool)> {
    // Token-guided: a coordinate token followed by lat, lon words.
    for i in 0..body.len() {
        let tok = body[i];
        if tok == 0x66 || tok == 0x67 {
            if let (Some(lat), Some(lon)) = (be_i32(body, i + 1), be_i32(body, i + 5)) {
                if in_range(lat, lon) && !near_null_island(lat, lon) {
                    return Some((lat, lon, true));
                }
            }
        }
    }
    if !scan {
        return None;
    }
    // Bounded scan fallback (low confidence).
    for i in 0..body.len() {
        if let (Some(lat), Some(lon)) = (be_i32(body, i), be_i32(body, i + 4)) {
            if in_range(lat, lon) && !near_null_island(lat, lon) {
                return Some((lat, lon, false));
            }
        }
    }
    None
}

/// Reject fixes within ~0.1° of (0, 0): the null island is where zero/padding
/// bytes decode to, and no real subscriber lives in the Gulf of Guinea.
#[inline]
fn near_null_island(lat_raw: i32, lon_raw: i32) -> bool {
    fixed_to_latitude(lat_raw).abs() < 0.1 && fixed_to_longitude(lon_raw).abs() < 0.1
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-trip the fixed-point coordinate math.
    #[test]
    fn coordinate_math_round_trips() {
        for (lat, lon) in [(40.0, -100.0), (-30.0, 150.0), (0.5, -0.5)] {
            let lat_raw = latitude_to_fixed(lat);
            let lon_raw = longitude_to_fixed(lon);
            assert!((fixed_to_latitude(lat_raw) - lat).abs() < 1e-6, "lat {lat}");
            assert!(
                (fixed_to_longitude(lon_raw) - lon).abs() < 1e-6,
                "lon {lon}"
            );
        }
    }

    /// The full-scale words map onto the coordinate extremes.
    #[test]
    fn fixed_point_extremes() {
        assert!((fixed_to_latitude(i32::MIN) - -90.0).abs() < 1e-9);
        assert!((fixed_to_longitude(i32::MIN) - -180.0).abs() < 1e-9);
        // +max approaches but never reaches +90 / +180.
        assert!(fixed_to_latitude(i32::MAX) < 90.0);
        assert!(fixed_to_latitude(i32::MAX) > 89.9);
    }

    /// Build a minimal IPv4/UDP datagram to `dst_port` carrying `body`.
    fn udp_datagram(dst_port: u16, body: &[u8]) -> Vec<u8> {
        let mut d = Vec::new();
        // IPv4 header: version 4, IHL 5 (20 bytes). Fields other than
        // version/IHL/protocol are irrelevant to the extractor.
        d.push(0x45);
        d.extend_from_slice(&[0x00, 0x00, 0x00]); // dscp + total len (ignored)
        d.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // id + flags/frag
        d.push(0x40); // ttl
        d.push(17); // protocol = UDP
        d.extend_from_slice(&[0x00, 0x00]); // header checksum (ignored)
        d.extend_from_slice(&[10, 0, 0, 1]); // src ip
        d.extend_from_slice(&[10, 0, 0, 2]); // dst ip
                                             // UDP header.
        d.extend_from_slice(&[0x30, 0x39]); // src port 12345
        d.extend_from_slice(&dst_port.to_be_bytes());
        d.extend_from_slice(&[0x00, 0x00]); // length (ignored)
        d.extend_from_slice(&[0x00, 0x00]); // checksum (ignored)
        d.extend_from_slice(body);
        d
    }

    /// A token-guided LRRP body: `0x66` then lat, lon, altitude words.
    fn lrrp_body(lat: f64, lon: f64) -> Vec<u8> {
        let mut b = vec![0x0d, 0x00, 0x66]; // msg type + request id + coord token
        b.extend_from_slice(&latitude_to_fixed(lat).to_be_bytes());
        b.extend_from_slice(&longitude_to_fixed(lon).to_be_bytes());
        b.extend_from_slice(&0i32.to_be_bytes()); // height/accuracy word
        b
    }

    /// A location poll with the layout of one an event log recorded (addresses and
    /// LLID synthetic, checksums and PDU CRC recomputed): two SNDCP octets, IPv4
    /// 10.20.2.50 -> 10.21.1.42, UDP 64414 -> 64414, a 33-octet datagram, then
    /// `0xF5` block filler and the PDU CRC. Polls of this shape produced the
    /// constant (-72.711650, -14.117647) "fix" in a four-day log.
    const REF_POLL: &str = "5100458800350\
25c00003c1164500a1402320a15012afb9efb9e0021bc7c0000800000000000000000000000\
2600080089546698967f00f5f5f5f5f580b98b52";

    fn ref_poll() -> Vec<u8> {
        (0..REF_POLL.len() / 2)
            .map(|i| u8::from_str_radix(&REF_POLL[i * 2..i * 2 + 2], 16).expect("hex"))
            .collect()
    }

    /// The UDP length field bounds the body, so the `0xF5` block filler behind
    /// the 25-octet poll body can no longer be read as a longitude word.
    #[test]
    fn the_ref_poll_no_longer_decodes_a_fix() {
        let p = ref_poll();
        assert!(
            contains_lrrp_transport(&p),
            "it is still a UDP/64414 datagram"
        );
        assert_eq!(
            extract_lrrp(&p),
            None,
            "the constant -72.711650/-14.117647 'fix' was block filler past the UDP length"
        );
    }

    /// The candidate API still *sees* the poll's phantom coordinate, and says
    /// it lies past the UDP length — which is how the gate can count it.
    #[test]
    fn the_ref_poll_is_a_candidate_outside_the_udp_length() {
        let c = extract_lrrp_candidate(&ref_poll()).expect("a candidate");
        assert!(!c.within_udp_length);
        assert!((c.fix.latitude - -72.711_650).abs() < 1e-5);
        assert!((c.fix.longitude - -14.117_647).abs() < 1e-5);
        assert!(c.fix.token_guided);
    }

    /// A coordinate wholly inside the UDP length is a candidate that agrees
    /// with the strict decode.
    #[test]
    fn an_in_bounds_coordinate_is_a_candidate_within_the_udp_length() {
        let d = udp_datagram(LRRP_PORT, &lrrp_body(40.0, -100.0));
        let c = extract_lrrp_candidate(&d).expect("a candidate");
        assert!(c.within_udp_length);
        assert_eq!(Some(c.fix), extract_lrrp(&d));
    }

    /// The two-octet SNDCP header and the IPv4/UDP envelope of that same poll.
    #[test]
    fn parses_the_sndcp_header_and_datagram() {
        let d = parse_sndcp_datagram(&ref_poll()).expect("a datagram");
        assert_eq!(d.ip_off, SNDCP_HEADER_LEN);
        assert_eq!(d.nsapi, Some(1), "low nibble of 0x51");
        assert_eq!(d.pdu_type, Some(5), "high nibble of 0x51");
        assert_eq!(d.protocol, 17);
        assert_eq!(d.ip_src, [10, 20, 2, 50]);
        assert_eq!(d.ip_dst, [10, 21, 1, 42]);
        assert_eq!(d.udp_src, Some(0xfb9e));
        assert_eq!(d.udp_dst, Some(0xfb9e));
        assert_eq!(
            d.payload_len,
            Some(25),
            "33-octet datagram less the UDP header"
        );
        assert!(is_fne_host(d.ip_src), "the poll comes from the FNE host");
        assert!(!is_fne_host(d.ip_dst));
    }

    /// A hand-built datagram parses the same way, header and all.
    #[test]
    fn parses_a_hand_built_datagram() {
        let mut p = vec![0x51, 0x00];
        p.extend_from_slice(&udp_datagram(0xfb9e, &[1, 2, 3, 4]));
        let d = parse_sndcp_datagram(&p).expect("a datagram");
        assert_eq!(d.ip_off, 2);
        assert_eq!(d.nsapi, Some(1));
        assert_eq!(d.udp_dst, Some(0xfb9e));
        assert_eq!(d.ip_dst, [10, 0, 0, 2]);
        // No usable UDP length in the helper, so the rest of the buffer is it.
        assert_eq!(d.payload_len, Some(4));
    }

    /// A datagram with no SNDCP header in front still parses, with no NSAPI.
    #[test]
    fn a_headerless_datagram_reports_no_nsapi() {
        let d = parse_sndcp_datagram(&udp_datagram(0xfb9e, &[0])).expect("a datagram");
        assert_eq!(d.ip_off, 0);
        assert_eq!(d.nsapi, None);
        assert_eq!(d.pdu_type, None);
    }

    /// `01 03 F1 <a.b.c.d>` is the SNDCP context accept: the LLID -> IPv4 map.
    #[test]
    fn parses_an_sndcp_context_accept() {
        // The layout of one from an event log (address synthetic).
        let p = [
            0x01, 0x03, 0xf1, 0x0a, 0x15, 0x01, 0xf4, 0x00, 0x00, 0x20, 0x00, 0x00, 0x40, 0x00,
        ];
        assert_eq!(parse_sndcp_context_accept(&p), Some([10, 21, 1, 244]));
        assert_eq!(parse_sndcp_context_accept(&p[..6]), None, "too short");
        assert_eq!(parse_sndcp_context_accept(&ref_poll()), None);
    }

    #[test]
    fn decodes_a_udp_4001_fix() {
        let (lat, lon) = (40.0, -100.0);
        let d = udp_datagram(LRRP_PORT, &lrrp_body(lat, lon));
        let fix = extract_lrrp(&d).expect("a fix");
        assert!((fix.latitude - lat).abs() < 1e-5);
        assert!((fix.longitude - lon).abs() < 1e-5);
        assert!(fix.token_guided);
        assert!(!fix.via_icmp);
    }

    #[test]
    fn a_leading_llc_prefix_does_not_stop_the_scan() {
        let (lat, lon) = (40.0, -100.0);
        let mut d = vec![0xaa, 0xbb, 0xcc]; // SNDCP/LLC framing junk
        d.extend_from_slice(&udp_datagram(LRRP_PORT, &lrrp_body(lat, lon)));
        let fix = extract_lrrp(&d).expect("a fix past the prefix");
        assert!((fix.latitude - lat).abs() < 1e-5);
    }

    #[test]
    fn a_non_lrrp_port_is_ignored() {
        let d = udp_datagram(53, &lrrp_body(40.0, -100.0));
        assert!(extract_lrrp(&d).is_none(), "port 53 is not LRRP");
    }

    #[test]
    fn decodes_a_fix_on_the_harris_port_64414() {
        // The L3Harris LRRP transport port (0xfb9e), not Motorola's 4001.
        let (lat, lon) = (40.25, -100.25); // a synthetic point
        let d = udp_datagram(0xfb9e, &lrrp_body(lat, lon));
        let fix = extract_lrrp(&d).expect("a fix on the Harris port");
        assert_eq!(fix.port, 0xfb9e);
        assert!((fix.latitude - lat).abs() < 1e-4);
        assert!((fix.longitude - lon).abs() < 1e-4);
    }

    /// An outbound SNDCP PDU with the layout of one reassembled from a wideband
    /// test capture (LLID and addresses synthetic, checksums and PDU CRC
    /// recomputed): an IP/UDP datagram `10.20.2.50 → 10.21.1.204`, UDP src==dst
    /// port 64414. It is a network→radio poll, so its 25-byte body carries no
    /// coordinate — but the transport is detected. This is the shape behind the
    /// port-64414 finding.
    #[test]
    fn an_outbound_harris_pdu_is_lrrp_transport_but_carries_no_fix() {
        let payload = hex_bytes(
            "510045880035053b00003c1160cf0a1402320a1501ccfb9efb9e0021b9da0000\
             8000000000000000000000002600080089546898967f00f5f5f5f5f5b33ab331",
        );
        assert!(
            contains_lrrp_transport(&payload),
            "the datagram is UDP to the Harris LRRP port"
        );
        assert!(
            extract_lrrp(&payload).is_none(),
            "an outbound poll carries no downlink coordinate"
        );
    }

    fn hex_bytes(s: &str) -> Vec<u8> {
        let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn recovers_lrrp_wrapped_in_icmp_port_unreachable() {
        let (lat, lon) = (39.5, -99.5);
        let inner = udp_datagram(LRRP_PORT, &lrrp_body(lat, lon));
        // Outer IPv4 header, protocol ICMP.
        let mut d = vec![
            0x45, 0, 0, 0, 0, 0, 0, 0, 0x40, 1, 0, 0, 10, 0, 0, 9, 10, 0, 0, 1,
        ];
        // ICMP: type 3 (dest unreachable), code 3 (port unreachable), 4 unused.
        d.extend_from_slice(&[3, 3, 0, 0, 0, 0, 0, 0]);
        d.extend_from_slice(&inner); // quoted original datagram
        let fix = extract_lrrp(&d).expect("a fix inside the ICMP quote");
        assert!((fix.latitude - lat).abs() < 1e-5);
        assert!((fix.longitude - lon).abs() < 1e-5);
        assert!(fix.via_icmp);
    }

    #[test]
    fn garbage_never_panics_and_never_decodes() {
        // Random-ish bytes, all lengths from 0..64.
        for len in 0..64usize {
            let junk: Vec<u8> = (0..len)
                .map(|i| (i as u8).wrapping_mul(37) ^ 0x5a)
                .collect();
            // Must not panic; a stray decode is acceptable only if it is a
            // valid in-range, non-null coordinate — assert no panic here.
            let _ = extract_lrrp(&junk);
        }
    }

    #[test]
    fn an_all_zero_body_does_not_decode_to_null_island() {
        let d = udp_datagram(LRRP_PORT, &[0u8; 24]);
        assert!(
            extract_lrrp(&d).is_none(),
            "all-zero body must not report (0,0)"
        );
    }

    #[test]
    fn bounded_scan_finds_an_untokenised_pair() {
        // No coordinate token: an L3Harris-shaped body might just be the words.
        // NB the bounded scan is deliberately best-effort — with unknown framing
        // bytes ahead of the coordinates a *different* in-range pair can match
        // first (this is why the token-guided path is preferred and why a fix's
        // `token_guided` flag matters). Here the words lead the body, so the scan
        // is unambiguous.
        let (lat, lon) = (40.5, -100.5);
        let mut body = Vec::new();
        body.extend_from_slice(&latitude_to_fixed(lat).to_be_bytes());
        body.extend_from_slice(&longitude_to_fixed(lon).to_be_bytes());
        let d = udp_datagram(LRRP_PORT, &body);
        // The token-guided default declines an untokenised body; the explicit
        // scan variant recovers it.
        assert!(
            extract_lrrp(&d).is_none(),
            "default path is token-guided only"
        );
        let fix = extract_lrrp_scan(&d).expect("a scanned fix");
        assert!((fix.latitude - lat).abs() < 1e-4);
        assert!((fix.longitude - lon).abs() < 1e-4);
        assert!(!fix.token_guided, "this body has no coordinate token");
    }

    #[test]
    fn the_scan_variant_false_positives_on_an_outbound_poll_body() {
        // Documents *why* the pipeline uses the token-guided `extract_lrrp`: the
        // aggressive scan decodes payload bytes of an outbound poll as a
        // bogus coordinate. Kept as a guardrail against anyone wiring the scan in.
        let payload = hex_bytes(
            "510045880035053b00003c1160cf0a1402320a1501ccfb9efb9e0021b9da0000\
             8000000000000000000000002600080089546898967f00f5f5f5f5f5b33ab331",
        );
        assert!(
            extract_lrrp(&payload).is_none(),
            "token-guided path is clean"
        );
        assert!(
            extract_lrrp_scan(&payload).is_some(),
            "the scan mis-decodes real bytes — hence it is offline-only"
        );
    }

    #[test]
    fn contains_lrrp_transport_detects_presence_without_a_fix() {
        // An LRRP-port datagram whose body has no decodable coordinate: presence
        // is still detectable, even though extract returns None.
        let d = udp_datagram(LRRP_PORT, &[0u8; 8]);
        assert!(contains_lrrp_transport(&d));
        assert!(extract_lrrp(&d).is_none());
        // A non-LRRP-port datagram is not LRRP traffic.
        assert!(!contains_lrrp_transport(&udp_datagram(53, &[1, 2, 3, 4])));
    }

    #[test]
    fn truncated_headers_are_safe() {
        // An IPv4 header that promises 20 bytes but is cut short.
        assert!(extract_lrrp(&[0x45, 0x00, 0x00]).is_none());
        // UDP header claimed but missing.
        let mut d = vec![
            0x45, 0, 0, 0, 0, 0, 0, 0, 0x40, 17, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8,
        ];
        d.push(0x30); // half a src port, then nothing
        assert!(extract_lrrp(&d).is_none());
    }
}
