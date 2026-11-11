// SPDX-License-Identifier: GPL-3.0-or-later
//! The recorder-link wire format: message types, framing, authentication.
//!
//! One datagram carries one [`LinkMsg`]. The frame is
//!
//! ```text
//! ┌────────┬─────────┬────────┬────────────┬───────────────┬───────────┬──────────────┐
//! │ "TKL1" │ version │  seq   │  sent_ms   │ postcard body │ sender id │ HMAC tag (16)│
//! │  4 B   │  1 B    │  4 B   │    8 B     │      n B      │  0 / 8 B  │              │
//! └────────┴─────────┴────────┴────────────┴───────────────┴───────────┴──────────────┘
//! ```
//!
//! The tag is the first 16 bytes of HMAC-SHA256 over everything before it,
//! keyed with the shared `[link] key`. Truncation to 128 bits is the usual
//! trade for a datagram this small: forging one costs 2^128 work, and the
//! window a forgery would open (one bogus grant) is not worth 32 bytes on
//! every packet.
//!
//! The **sender id** is a random 64-bit number a [`LinkNode`](crate::LinkNode)
//! picks when it binds: the authenticated name of *who sent this frame*, which
//! the UDP source address is not (anyone on the LAN can resend a captured
//! datagram from another port, or another spoofed address). It sits after the
//! body and inside the tag, so an older build — whose postcard decoder stops at
//! the end of the message and ignores trailing bytes — still accepts these
//! frames unchanged, and no version bump was needed. A frame from such an older
//! build simply has no sender id.
//!
//! `seq` is a per-sender frame counter and `sent_ms` the sender's wall clock.
//! Together with the sender id they are the frame's nonce: the receiver rejects
//! a frame whose `sent_ms` is outside its clock window and remembers every
//! nonce it accepted inside that window (see `replay.rs`), which is what stops
//! a captured datagram being replayed — from any address.
//!
//! Nothing here knows about the pipeline. The recorder maps its own grant type
//! onto [`LinkMsg::Grant`] on the way out and back on the way in, so this crate
//! stays a description of the protocol and can be tested without a radio.

use std::fmt;

use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

/// Frame magic: "trunkd link, version 1".
pub const MAGIC: [u8; 4] = *b"TKL1";

/// Protocol version carried in the frame header of everything this build
/// sends.
///
/// Version 2 added [`LinkMsg::GrantAck`]: a peer sending version 2 or later
/// answers every `Grant` it authenticates, which is what lets the sender stop
/// retrying. The version byte *is* the capability flag — a `GrantAck` is never
/// sent to a peer whose frames say version 1, because that build would fail to
/// parse the variant and count it as a bad frame.
pub const PROTOCOL_VERSION: u8 = 2;

/// The oldest frame version this build still accepts on the wire.
///
/// A version-1 peer is understood (it just never acks); its grants still route
/// and its hellos still populate the peer table.
pub const MIN_PROTOCOL_VERSION: u8 = 1;

/// The first frame version whose sender answers grants with [`LinkMsg::GrantAck`].
pub const GRANT_ACK_VERSION: u8 = 2;

/// Bytes of HMAC-SHA256 kept as the authentication tag.
pub const TAG_LEN: usize = 16;

/// Fixed header: magic, version, sequence, send time.
pub const HEADER_LEN: usize = 4 + 1 + 4 + 8;

/// Bytes of sender id carried after the body (inside the tag).
pub const SENDER_ID_LEN: usize = 8;

/// Largest datagram the link will send or accept.
///
/// Chosen to fit inside the smallest MTU a real deployment is likely to cross
/// (1280 for IPv6, less any tunnel header) so a link frame is never fragmented:
/// a fragmented UDP datagram is an all-or-nothing loss with a much worse loss
/// rate, and the payloads here are tiny by design.
pub const MAX_DATAGRAM: usize = 1200;

/// The largest body that still leaves room for the header, sender id and tag.
pub const MAX_PAYLOAD: usize = MAX_DATAGRAM - HEADER_LEN - SENDER_ID_LEN - TAG_LEN;

/// Shortest shared key the link accepts, in bytes. Enforced by config
/// validation too, so a deployment cannot start with a guessable one.
pub const MIN_KEY_LEN: usize = 16;

type HmacSha256 = Hmac<Sha256>;

/// Which site a message is about, as advertised on the wire.
///
/// Labels *and* NAC travel together because peers may name a site differently
/// (two operators, two config files) while the NAC is the site's own idea of
/// who it is. The receiver matches on either.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SiteKey {
    /// System label, as the sender configured it.
    pub system: String,
    /// Site label, as the sender configured it.
    pub site: String,
    /// Network Access Code the sender is locked to, 0 when unknown.
    pub nac: u16,
}

impl fmt::Display for SiteKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{} nac={:03X}", self.system, self.site, self.nac)
    }
}

/// Per-grant flags, packed into one byte so the common message stays small.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrantFlags(pub u8);

impl GrantFlags {
    /// The grant carries the emergency bit.
    pub const EMERGENCY: u8 = 1 << 0;
    /// The call is encrypted (announced at grant time).
    pub const ENCRYPTED: u8 = 1 << 1;
    /// The talkgroup is an active patch/regroup supergroup.
    pub const PATCH: u8 = 1 << 2;
    /// Unit-to-unit or telephone rather than a group call.
    pub const PRIVATE: u8 = 1 << 3;
    /// The grant is for an SNDCP data channel.
    pub const DATA: u8 = 1 << 4;

    /// Build from individual bits.
    pub fn new(bits: u8) -> Self {
        GrantFlags(bits)
    }

    /// Whether `bit` (one of the associated constants) is set.
    pub fn has(self, bit: u8) -> bool {
        self.0 & bit != 0
    }

    /// Set or clear `bit`, builder-style.
    pub fn with(self, bit: u8, on: bool) -> Self {
        GrantFlags(if on { self.0 | bit } else { self.0 & !bit })
    }
}

/// One tuner a peer is running, as advertised in [`LinkMsg::Hello`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TunerAd {
    /// The peer's tuner key (its `[[device]] id`).
    pub key: String,
    /// `control`, `voice`, `data` or `auto`.
    pub role: String,
    /// Centre frequency in Hz, 0 when the tuner has none.
    pub center_hz: u64,
    /// Usable capture span in Hz.
    pub span_hz: u64,
    /// Inclusive downlink ranges the tuner will record.
    pub bands: Vec<(u64, u64)>,
}

/// One site a peer is serving, as advertised in [`LinkMsg::Hello`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SiteAd {
    /// System label.
    pub system: String,
    /// Site label.
    pub site: String,
    /// NAC, 0 when not locked.
    pub nac: u16,
    /// Control frequency currently held, 0 while hunting.
    pub control_hz: u64,
    /// Whether the control channel is locked.
    pub locked: bool,
    /// TSBKs per second, rounded — a coarse health signal, not a measurement.
    pub tsbk_rate: u16,
    /// The tuners serving this site.
    pub tuners: Vec<TunerAd>,
}

impl SiteAd {
    /// The site key this advertisement describes.
    pub fn key(&self) -> SiteKey {
        SiteKey {
            system: self.system.clone(),
            site: self.site.clone(),
            nac: self.nac,
        }
    }
}

/// Everything one recorder says to another.
///
/// Plain owned data only: a message crosses a thread and a network, and never
/// borrows from the pipeline that produced it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LinkMsg {
    /// Periodic "I am here, and this is what I am hearing".
    Hello {
        /// The sender's instance name.
        instance: String,
        /// Recorder software's link protocol version (same as the frame's).
        version: u32,
        /// The sites it serves.
        sites: Vec<SiteAd>,
    },
    /// A grant seen on a control channel, offered to whoever can record it.
    Grant {
        /// Which site it was heard on.
        site: SiteKey,
        /// Downlink frequency granted.
        frequency_hz: u64,
        /// Talkgroup, 0 for unit-to-unit / telephone.
        talkgroup: u16,
        /// Source radio at grant time, 0 when unknown.
        source: u32,
        /// Emergency / encrypted / patch / private / data bits.
        flags: GrantFlags,
        /// Sender's wall clock at the grant, unix milliseconds.
        at_wall_ms: i64,
        /// Sender's monotonically increasing grant counter, for logging and
        /// for spotting loss on a link that is otherwise silent.
        seq: u64,
    },
    /// A tuner started following a call: "I have this one".
    Claim {
        /// Which site.
        site: SiteKey,
        /// Downlink frequency being recorded.
        frequency_hz: u64,
        /// Talkgroup.
        talkgroup: u16,
        /// Which of the sender's tuners is on it.
        tuner: String,
        /// Sender's wall clock, unix milliseconds.
        at_wall_ms: i64,
    },
    /// The call on a claimed frequency ended.
    Release {
        /// Which site.
        site: SiteKey,
        /// Downlink frequency released.
        frequency_hz: u64,
        /// Sender's wall clock, unix milliseconds.
        at_wall_ms: i64,
    },
    /// A control-channel lock or unlock.
    ControlState {
        /// Which site.
        site: SiteKey,
        /// Control frequency now held, 0 when hunting.
        control_hz: u64,
        /// Whether it is locked.
        locked: bool,
        /// TSBKs per second, rounded.
        tsbk_rate: u16,
    },
    /// Acknowledgement of one `Grant` datagram, sent straight back to the
    /// address it came from.
    ///
    /// `seq` is the *frame* sequence of the grant being acknowledged, not the
    /// grant's own counter: the frame sequence is unique per peer per
    /// transmission, so one ack names exactly one datagram to exactly one
    /// sender. Fire-and-forget — a lost ack costs one redelivered grant, which
    /// the receiver's replay cache discards while still acking it again.
    GrantAck {
        /// Frame sequence of the acknowledged `Grant`.
        seq: u32,
    },
}

/// A decoded, authenticated frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// The frame version the sender used; [`GRANT_ACK_VERSION`] or later means
    /// it understands [`LinkMsg::GrantAck`].
    pub version: u8,
    /// The sender's frame counter.
    pub seq: u32,
    /// The sender's wall clock when it sent, unix milliseconds.
    pub sent_ms: i64,
    /// The sender's authenticated id, `None` for a frame from a build that
    /// predates it.
    pub sender: Option<u64>,
    /// The frame's authentication tag: what names this exact datagram when
    /// there is no sender id to name its origin.
    pub tag: [u8; TAG_LEN],
    /// The message.
    pub msg: LinkMsg,
}

/// Why a datagram was not accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireError {
    /// Shorter than a header plus tag, or longer than [`MAX_DATAGRAM`].
    BadLength,
    /// Magic bytes are not [`MAGIC`].
    BadMagic,
    /// Version byte is not one this build speaks.
    BadVersion,
    /// The authentication tag does not match.
    BadTag,
    /// The body is not a `LinkMsg`.
    BadBody,
    /// The message would not fit in [`MAX_DATAGRAM`].
    TooLarge,
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            WireError::BadLength => "datagram length out of range",
            WireError::BadMagic => "not a trunkd link frame",
            WireError::BadVersion => "unsupported link protocol version",
            WireError::BadTag => "authentication failed",
            WireError::BadBody => "malformed link message",
            WireError::TooLarge => "message does not fit one datagram",
        };
        f.write_str(s)
    }
}

impl std::error::Error for WireError {}

/// Truncated HMAC-SHA256 over `bytes`.
fn tag(key: &[u8], bytes: &[u8]) -> [u8; TAG_LEN] {
    let mut mac = <HmacSha256 as KeyInit>::new_from_slice(key)
        .expect("HMAC-SHA256 accepts a key of any length");
    mac.update(bytes);
    let full = mac.finalize().into_bytes();
    let mut out = [0u8; TAG_LEN];
    out.copy_from_slice(&full[..TAG_LEN]);
    out
}

/// Constant-time comparison of two tags.
///
/// Written as an accumulate-and-compare rather than `==` so a forgery attempt
/// learns nothing from how long the check took.
fn tags_match(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Frame and authenticate `msg`.
///
/// Fails with [`WireError::TooLarge`] rather than sending a datagram that would
/// fragment; the caller shrinks the message (see [`fit_hello`]) and retries.
pub fn encode(
    msg: &LinkMsg,
    key: &[u8],
    sender: u64,
    seq: u32,
    sent_ms: i64,
) -> Result<Vec<u8>, WireError> {
    encode_versioned(msg, key, Some(sender), seq, sent_ms, PROTOCOL_VERSION)
}

/// Frame and authenticate `msg` claiming a specific protocol `version`.
///
/// Only [`encode`] is used in anger; this exists so a test (and, one day, a
/// deliberate downgrade for an older peer) can produce a frame that says it
/// comes from a build which does not acknowledge grants — or, with `sender`
/// `None`, one that predates the sender id.
pub fn encode_versioned(
    msg: &LinkMsg,
    key: &[u8],
    sender: Option<u64>,
    seq: u32,
    sent_ms: i64,
    version: u8,
) -> Result<Vec<u8>, WireError> {
    let body = postcard::to_stdvec(msg).map_err(|_| WireError::BadBody)?;
    if body.len() > MAX_PAYLOAD {
        return Err(WireError::TooLarge);
    }
    let mut out = Vec::with_capacity(HEADER_LEN + body.len() + SENDER_ID_LEN + TAG_LEN);
    out.extend_from_slice(&MAGIC);
    out.push(version);
    out.extend_from_slice(&seq.to_be_bytes());
    out.extend_from_slice(&sent_ms.to_be_bytes());
    out.extend_from_slice(&body);
    if let Some(id) = sender {
        out.extend_from_slice(&id.to_be_bytes());
    }
    let t = tag(key, &out);
    out.extend_from_slice(&t);
    Ok(out)
}

/// Verify and parse a datagram.
pub fn decode(buf: &[u8], key: &[u8]) -> Result<Frame, WireError> {
    if buf.len() < HEADER_LEN + TAG_LEN || buf.len() > MAX_DATAGRAM {
        return Err(WireError::BadLength);
    }
    if buf[..4] != MAGIC {
        return Err(WireError::BadMagic);
    }
    let version = buf[4];
    if !(MIN_PROTOCOL_VERSION..=PROTOCOL_VERSION).contains(&version) {
        return Err(WireError::BadVersion);
    }
    let split = buf.len() - TAG_LEN;
    let (signed, got) = buf.split_at(split);
    // Authenticate before parsing: postcard never sees a byte an attacker
    // chose without the key.
    if !tags_match(&tag(key, signed), got) {
        return Err(WireError::BadTag);
    }
    let seq = u32::from_be_bytes([buf[5], buf[6], buf[7], buf[8]]);
    let mut ms = [0u8; 8];
    ms.copy_from_slice(&buf[9..17]);
    let sent_ms = i64::from_be_bytes(ms);
    let (msg, rest): (LinkMsg, &[u8]) =
        postcard::take_from_bytes(&signed[HEADER_LEN..]).map_err(|_| WireError::BadBody)?;
    // Whatever follows the body is the sender id, or nothing from an older
    // build. Anything else is not a frame this build wrote.
    let sender = match rest.len() {
        0 => None,
        SENDER_ID_LEN => {
            let mut id = [0u8; SENDER_ID_LEN];
            id.copy_from_slice(rest);
            Some(u64::from_be_bytes(id))
        }
        _ => return Err(WireError::BadBody),
    };
    let mut tag = [0u8; TAG_LEN];
    tag.copy_from_slice(got);
    Ok(Frame {
        version,
        seq,
        sent_ms,
        sender,
        tag,
        msg,
    })
}

/// Shrink a `Hello` until it fits one datagram, dropping advertised tuners
/// (then whole sites) from the end.
///
/// A recorder with more tuners than fit says less about itself rather than
/// saying nothing: the peer list, the sites and the first tuners are what the
/// other end actually routes on.
///
/// Returns `true` if anything was dropped.
pub fn fit_hello(msg: &mut LinkMsg) -> bool {
    if !matches!(msg, LinkMsg::Hello { .. }) {
        return false;
    }
    let mut trimmed = false;
    // Cheap upper bound: postcard is a compact encoding, so measuring the real
    // thing each round is both exact and fast enough at Hello's cadence.
    while encoded_len(msg) > MAX_PAYLOAD {
        let LinkMsg::Hello { sites, .. } = msg else {
            return trimmed;
        };
        let Some(last) = sites.last_mut() else {
            // Nothing left to drop; the instance name alone is too long.
            return trimmed;
        };
        if last.tuners.pop().is_none() {
            sites.pop();
        }
        trimmed = true;
    }
    trimmed
}

/// Encoded body length of a message, or `usize::MAX` if it cannot be encoded.
fn encoded_len(msg: &LinkMsg) -> usize {
    postcard::to_stdvec(msg)
        .map(|v| v.len())
        .unwrap_or(usize::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> &'static [u8] {
        b"a-sixteen-byte-key-or-longer"
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// The tag is HMAC-SHA256 truncated to [`TAG_LEN`], whatever version of
    /// the `hmac`/`sha2` crates computes it: RFC 4231 test cases 2 (a short
    /// key) and 6 (a key longer than the block, which is hashed first).
    #[test]
    fn the_tag_is_truncated_hmac_sha256_per_rfc_4231() {
        assert_eq!(
            hex(&tag(b"Jefe", b"what do ya want for nothing?")),
            &"5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"[..TAG_LEN * 2]
        );
        assert_eq!(
            hex(&tag(
                &[0xaa; 131],
                b"Test Using Larger Than Block-Size Key - Hash Key First"
            )),
            &"60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"[..TAG_LEN * 2]
        );
    }

    /// A frame is byte-identical across builds, so two recorders on different
    /// builds keep authenticating each other: this one was encoded before the
    /// move to `hmac 0.13`/`sha2 0.11` and must never change. (Re-pinned once
    /// for the neutral site names, by the build that still reproduced the
    /// original pre-move frame byte for byte.)
    #[test]
    fn a_known_frame_encodes_to_the_same_bytes() {
        let msg = LinkMsg::Grant {
            site: site(),
            frequency_hz: 852_137_500,
            talkgroup: 1301,
            source: 1_620_719,
            flags: GrantFlags::default().with(GrantFlags::EMERGENCY, true),
            at_wall_ms: 1_788_177_600_000,
            seq: 7,
        };
        let frame =
            encode(&msg, key(), 0x0123_4567_89ab_cdef, 42, 1_788_177_600_000).expect("encode");
        assert_eq!(
            hex(&frame),
            "544b4c31020000002a000001a057b0ce0001076578616d706c650763656e7472616c\
             cf069cacaa9603950aeff5620180b886fb8a68070123456789abcdef47a90750ce33\
             a02f32cbb4f2f9df3230"
        );
    }

    fn site() -> SiteKey {
        SiteKey {
            system: "example".into(),
            site: "central".into(),
            nac: 0x34F,
        }
    }

    fn messages() -> Vec<LinkMsg> {
        vec![
            LinkMsg::Hello {
                instance: "recorder-a".into(),
                version: 1,
                sites: vec![SiteAd {
                    system: "example".into(),
                    site: "central".into(),
                    nac: 0x34F,
                    control_hz: 851_050_000,
                    locked: true,
                    tsbk_rate: 15,
                    tuners: vec![TunerAd {
                        key: "hackrf0".into(),
                        role: "control".into(),
                        center_hz: 852_000_000,
                        span_hz: 8_000_000,
                        bands: vec![(851_000_000, 856_000_000)],
                    }],
                }],
            },
            LinkMsg::Grant {
                site: site(),
                frequency_hz: 852_137_500,
                talkgroup: 1301,
                source: 0x12_3456,
                flags: GrantFlags::default()
                    .with(GrantFlags::EMERGENCY, true)
                    .with(GrantFlags::PATCH, true),
                at_wall_ms: 1_700_000_000_123,
                seq: 42,
            },
            LinkMsg::Claim {
                site: site(),
                frequency_hz: 852_137_500,
                talkgroup: 1301,
                tuner: "hackrf1".into(),
                at_wall_ms: 1_700_000_000_456,
            },
            LinkMsg::Release {
                site: site(),
                frequency_hz: 852_137_500,
                at_wall_ms: 1_700_000_002_000,
            },
            LinkMsg::ControlState {
                site: site(),
                control_hz: 851_050_000,
                locked: true,
                tsbk_rate: 15,
            },
            LinkMsg::GrantAck { seq: 0xDEAD_BEEF },
        ]
    }

    #[test]
    fn every_message_survives_a_round_trip() {
        for (i, msg) in messages().into_iter().enumerate() {
            let seq = i as u32 + 1;
            let bytes = encode(&msg, key(), 1, seq, 1_700_000_000_000).expect("encodes");
            assert!(bytes.len() <= MAX_DATAGRAM, "{} bytes", bytes.len());
            let frame = decode(&bytes, key()).expect("decodes");
            assert_eq!(frame.seq, seq);
            assert_eq!(frame.sent_ms, 1_700_000_000_000);
            assert_eq!(frame.msg, msg);
        }
    }

    #[test]
    fn a_grant_is_a_small_datagram() {
        // The message that flies most often has to stay cheap: a few hundred
        // bytes at worst, so a busy site costs kilobits per second, not
        // megabits.
        let g = &messages()[1];
        let bytes = encode(g, key(), 1, 1, 0).expect("encodes");
        assert!(bytes.len() < 128, "a grant frame is {} bytes", bytes.len());
    }

    #[test]
    fn the_wrong_key_is_rejected() {
        let bytes = encode(&messages()[1], key(), 1, 1, 0).expect("encodes");
        assert_eq!(
            decode(&bytes, b"a-different-sixteen-byte-key"),
            Err(WireError::BadTag)
        );
    }

    #[test]
    fn a_tampered_body_is_rejected() {
        let mut bytes = encode(&messages()[1], key(), 1, 1, 0).expect("encodes");
        let n = bytes.len();
        bytes[HEADER_LEN + 2] ^= 0x01;
        assert_eq!(decode(&bytes, key()), Err(WireError::BadTag));
        // Truncation and a mangled header are rejected too.
        assert_eq!(decode(&bytes[..n - 1], key()), Err(WireError::BadTag));
        bytes[0] = b'X';
        assert_eq!(decode(&bytes, key()), Err(WireError::BadMagic));
        let mut v = encode(&messages()[1], key(), 1, 1, 0).expect("encodes");
        v[4] = 9;
        assert_eq!(decode(&v, key()), Err(WireError::BadVersion));
        assert_eq!(decode(&[0u8; 4], key()), Err(WireError::BadLength));
    }

    #[test]
    fn an_ack_is_the_smallest_message_there_is() {
        // The ack is on the latency path of every grant, and one is sent per
        // grant per peer: it has to be a handful of bytes, not a message.
        let bytes = encode(&LinkMsg::GrantAck { seq: 9 }, key(), 1, 1, 0).expect("encodes");
        assert!(bytes.len() < 48, "an ack frame is {} bytes", bytes.len());
        // And never larger than the grant it answers: no amplification.
        let grant = encode(&messages()[1], key(), 1, 1, 0).expect("encodes");
        assert!(bytes.len() < grant.len());
        let frame = decode(&bytes, key()).expect("decodes");
        assert_eq!(frame.msg, LinkMsg::GrantAck { seq: 9 });
        assert_eq!(frame.version, PROTOCOL_VERSION);
    }

    #[test]
    fn an_older_peers_frames_are_still_understood() {
        // Version 1 is the build before grant acks. Its frames must still
        // decode — variant order was preserved and `GrantAck` appended — and
        // the version it claims is what tells the receiver not to ack it.
        for msg in messages() {
            if matches!(msg, LinkMsg::GrantAck { .. }) {
                continue; // a v1 peer never sends one
            }
            let bytes = encode_versioned(&msg, key(), Some(1), 3, 1_700_000_000_000, 1)
                .expect("encodes as v1");
            let frame = decode(&bytes, key()).expect("a v1 frame is accepted");
            assert_eq!(frame.version, 1);
            assert!(frame.version < GRANT_ACK_VERSION, "and it will not ack");
            assert_eq!(frame.msg, msg, "v1 bodies parse identically");
        }
        // A version this build has never heard of is still refused.
        let ahead = encode_versioned(&messages()[1], key(), Some(1), 3, 0, PROTOCOL_VERSION + 1)
            .expect("encodes");
        assert_eq!(decode(&ahead, key()), Err(WireError::BadVersion));
    }

    #[test]
    fn the_sender_id_is_authenticated_and_invisible_to_an_older_build() {
        let g = &messages()[1];
        let bytes = encode(g, key(), 0xFEED_F00D_0000_0001, 7, 1_700_000_000_000).expect("encodes");
        let frame = decode(&bytes, key()).expect("decodes");
        assert_eq!(frame.sender, Some(0xFEED_F00D_0000_0001));
        // The id is inside the tag: changing it is a forgery.
        let mut forged = bytes.clone();
        let at = forged.len() - TAG_LEN - 1;
        forged[at] ^= 0x01;
        assert_eq!(decode(&forged, key()), Err(WireError::BadTag));
        // What a build without sender ids does with the body: postcard stops
        // at the end of the message, so the trailing id is simply ignored.
        let body = &bytes[HEADER_LEN..bytes.len() - TAG_LEN];
        let old: LinkMsg = postcard::from_bytes(body).expect("an older decoder still parses it");
        assert_eq!(&old, g);
        // And a frame from such a build has no id.
        let anon = encode_versioned(g, key(), None, 7, 0, PROTOCOL_VERSION).expect("encodes");
        assert_eq!(decode(&anon, key()).expect("decodes").sender, None);
    }

    #[test]
    fn trailing_bytes_that_are_not_a_sender_id_are_refused() {
        let body = postcard::to_stdvec(&messages()[1]).expect("encodes");
        let mut out = Vec::new();
        out.extend_from_slice(&MAGIC);
        out.push(PROTOCOL_VERSION);
        out.extend_from_slice(&1u32.to_be_bytes());
        out.extend_from_slice(&0i64.to_be_bytes());
        out.extend_from_slice(&body);
        out.extend_from_slice(&[1, 2, 3]);
        let t = tag(key(), &out);
        out.extend_from_slice(&t);
        assert_eq!(decode(&out, key()), Err(WireError::BadBody));
    }

    #[test]
    fn an_oversize_hello_is_truncated_to_fit() {
        let tuners: Vec<TunerAd> = (0..200)
            .map(|i| TunerAd {
                key: format!("tuner-with-a-long-name-{i:03}"),
                role: "voice".into(),
                center_hz: 852_000_000,
                span_hz: 8_000_000,
                bands: vec![(851_000_000, 856_000_000), (860_000_000, 866_000_000)],
            })
            .collect();
        let mut msg = LinkMsg::Hello {
            instance: "recorder-a".into(),
            version: 1,
            sites: vec![
                SiteAd {
                    system: "example".into(),
                    site: "central".into(),
                    nac: 0x34F,
                    control_hz: 851_050_000,
                    locked: true,
                    tsbk_rate: 15,
                    tuners: tuners.clone(),
                },
                SiteAd {
                    system: "example".into(),
                    site: "east".into(),
                    nac: 0x350,
                    control_hz: 0,
                    locked: false,
                    tsbk_rate: 0,
                    tuners,
                },
            ],
        };
        assert!(encode(&msg, key(), 1, 1, 0).is_err(), "too big as built");
        assert!(fit_hello(&mut msg), "it had to drop something");
        let bytes = encode(&msg, key(), 1, 1, 0).expect("fits after trimming");
        assert!(bytes.len() <= MAX_DATAGRAM);
        // The first site keeps its identity and at least some of its tuners.
        let LinkMsg::Hello { sites, .. } = &msg else {
            unreachable!()
        };
        assert_eq!(sites[0].system, "example");
        assert!(
            !sites[0].tuners.is_empty(),
            "the first site still says what it runs"
        );
    }

    #[test]
    fn a_hello_that_already_fits_is_left_alone() {
        let mut msg = messages().remove(0);
        let before = msg.clone();
        assert!(!fit_hello(&mut msg));
        assert_eq!(msg, before);
    }
}
