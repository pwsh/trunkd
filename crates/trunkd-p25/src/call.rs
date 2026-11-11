// SPDX-License-Identifier: GPL-3.0-or-later
//! Per-call metadata aggregation.
//!
//! The decoder emits a flat stream of [`P25Event`]s. On a traffic channel a
//! single voice call spans an HDU header, many LDU1/LDU2 superframes (each
//! carrying Link Control, Encryption Sync, and Low-Speed Data), and a
//! terminating TDU/TDULC. [`CallAssembler`] folds that stream into one
//! [`CallRecord`] per call.
//!
//! **Encryption does not hide any of this.** P25 encrypts only the IMBE voice
//! payload; the HDU, Link Control, Encryption Sync and LSD signaling are
//! always sent in the clear. So a `CallRecord` is fully populated — talkgroup,
//! talker(s), emergency flag, encryption algorithm/key, message indicator,
//! L3Harris talker alias, and voice-frame count — whether or not the audio is
//! encrypted. The [`CallRecord::encrypted`] flag simply records which case it
//! was.

use crate::alias::TalkerAliasAssembler;
use crate::event::P25Event;
use crate::payload::{algorithm_name, is_clear_params, LinkControl};

/// Aggregated metadata for one voice call, assembled from its signaling.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CallRecord {
    /// Talkgroup (group calls). `None` for unit-to-unit or if never seen.
    pub talkgroup: Option<u16>,
    /// Most recent talker (source radio address).
    pub source: Option<u32>,
    /// Distinct talkers, in first-seen order (a call can change talkers).
    pub talkers: Vec<u32>,
    /// Unit-to-unit target address, if this was a unit-to-unit call.
    pub target: Option<u32>,
    /// Emergency flag from the call's service options.
    pub emergency: bool,
    /// True if any superframe reported encryption (ALGID ≠ UNENCRYPTED).
    pub encrypted: bool,
    /// Encryption algorithm id (last seen). `None` if no ES/HDU carried one.
    pub algid: Option<u8>,
    /// Encryption key id (last seen).
    pub key_id: Option<u16>,
    /// Most recent 9-octet message indicator (encryption sync).
    pub mi: Option<[u8; 9]>,
    /// L3Harris talker alias (assembled ASCII), if the call carried one.
    pub talker_alias: Option<String>,
    /// WACN of the current talker, from a Source ID Extension (LC 9).
    ///
    /// Present only when the talker is fully qualified over the air — a
    /// roaming subscriber from another WACN/system. Latched per talker and
    /// cleared on a talker change, exactly like the alias.
    pub source_wacn: Option<u32>,
    /// Home system id of the current talker, from a Source ID Extension.
    pub source_system: Option<u16>,
    /// Count of raw IMBE voice frames observed (≈ 20 ms each).
    pub voice_frames: u32,
    /// True if a TDU/TDULC terminator closed the call (vs. cut off at EOF).
    pub terminated: bool,
}

impl CallRecord {
    /// Encryption algorithm name (`"UNENCRYPTED"` when clear/unknown-none).
    pub fn algorithm(&self) -> &'static str {
        self.algid.map_or("UNENCRYPTED", algorithm_name)
    }

    /// Approximate call duration in milliseconds (20 ms per IMBE frame).
    pub fn duration_ms(&self) -> u32 {
        self.voice_frames * 20
    }

    /// True if the record carries identifying metadata or enough voice to be a
    /// real call, rather than a stray fragment (a lone terminator or a few
    /// spurious voice frames between calls). Consumers can use this to filter
    /// noise; the assembler itself stays faithful and reports everything.
    pub fn is_substantive(&self) -> bool {
        self.talkgroup.is_some()
            || self.source.is_some()
            || self.talker_alias.is_some()
            || self.voice_frames >= 8
    }
}

/// True if this Link Control word is an L3Harris talker-alias block.
pub(crate) fn is_alias_lc(lc: &LinkControl) -> bool {
    matches!(lc, LinkControl::Harris { opcode, .. } if TalkerAliasAssembler::is_alias_block(*opcode))
}

/// Folds a traffic channel's [`P25Event`] stream into [`CallRecord`]s.
///
/// Feed every event to [`observe`](Self::observe); it returns a completed
/// [`CallRecord`] when a call ends (a terminator, or a fresh HDU that starts a
/// new call while one was open). Call [`take`](Self::take) at end-of-stream to
/// flush a still-open call.
#[derive(Debug, Default)]
pub struct CallAssembler {
    active: bool,
    rec: CallRecord,
    /// L3Harris talker-alias assembly for the current talker (see
    /// [`crate::alias`]).
    alias: TalkerAliasAssembler,
}

impl CallAssembler {
    /// A fresh assembler with no call in progress.
    pub fn new() -> Self {
        Self::default()
    }

    /// True if a call is currently being accumulated.
    pub fn is_active(&self) -> bool {
        self.active
    }

    /// Observe one event. Returns a finished [`CallRecord`] when a call
    /// completes; otherwise `None`.
    pub fn observe(&mut self, ev: &P25Event) -> Option<CallRecord> {
        match ev {
            // An invalid HDU is a false-positive NID: its 120 bits did not
            // survive Golay18 + RS(63,47), so it carries no header and is not
            // evidence of a call boundary. Treating it as one fragments a live
            // call in two mid-stream.
            P25Event::Hdu { valid: false, .. } => None,
            // A valid HDU marks the start of a call. If one is already open
            // (missed terminator), close it out first and return it.
            P25Event::Hdu { header, .. } => {
                let finished = if self.active { self.finish() } else { None };
                self.active = true;
                if let Some(h) = header {
                    if self.rec.talkgroup.is_none() && h.talkgroup != 0 {
                        self.rec.talkgroup = Some(h.talkgroup);
                    }
                    self.apply_crypto(h.algid, h.kid, h.mi);
                }
                finished
            }
            // A talker-alias block with no call open is the tail of the
            // *previous* transmission on this frequency (the alias cycle runs
            // on past the terminator). Opening a call for it would buffer one
            // talker's alias under the next talker's call.
            P25Event::LinkControl { parsed, .. } if !self.active && is_alias_lc(parsed) => None,
            P25Event::LinkControl { parsed, .. } => {
                self.ensure_active();
                self.apply_link_control(parsed);
                None
            }
            P25Event::EncryptionSync { parsed, .. } => {
                self.ensure_active();
                self.apply_crypto(parsed.algid, parsed.kid, parsed.mi);
                None
            }
            P25Event::ImbeFrame { .. } => {
                self.ensure_active();
                self.rec.voice_frames += 1;
                None
            }
            // TDULC carries a final Link Control; both TDU and TDULC end a call.
            P25Event::TduLc { .. } | P25Event::Tdu => {
                if self.active {
                    self.rec.terminated = true;
                    self.finish()
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    /// Flush a call still in progress at end-of-stream (`terminated` = false).
    pub fn take(&mut self) -> Option<CallRecord> {
        if self.active {
            self.finish()
        } else {
            None
        }
    }

    fn ensure_active(&mut self) {
        self.active = true;
    }

    fn apply_link_control(&mut self, lc: &LinkControl) {
        match lc {
            LinkControl::GroupVoiceChannelUser {
                options,
                talkgroup,
                source,
                ..
            } => {
                if *talkgroup != 0 {
                    self.rec.talkgroup = Some(*talkgroup);
                }
                self.set_source(*source);
                self.rec.emergency |= options.emergency;
            }
            LinkControl::UnitToUnitVoiceChannelUser {
                options,
                target,
                source,
            } => {
                self.rec.target = Some(*target);
                self.set_source(*source);
                self.rec.emergency |= options.emergency;
            }
            LinkControl::TelephoneInterconnectVoiceChannelUser { options, .. } => {
                self.rec.emergency |= options.emergency;
            }
            // LCO 9 qualifies the 24-bit radio id LCO 0 carries. It names its
            // own radio, so a word for a *different* radio than the current
            // talker is a stale/foreign word and must not overwrite the latch.
            LinkControl::SourceIdExtension {
                wacn,
                system,
                source,
            } => {
                if self.rec.source.is_none_or(|cur| cur == *source) {
                    self.rec.source_wacn = Some(*wacn);
                    self.rec.source_system = Some(*system);
                }
            }
            LinkControl::Harris { opcode, word } => {
                self.alias.observe(*opcode, word);
            }
            // LCO 2 announces the (channel, talkgroup) pairs up on the site.
            // Sent on the traffic channel, slot A carries this channel's
            // talkgroup. Fold it only as a fallback: LCO 0 (Group Voice User)
            // already latches the talkgroup reliably, so this only fills the gap
            // when that word was missed, and it never overwrites a known TG (so
            // slot A naming a *different* group cannot mislabel the call).
            LinkControl::GroupVoiceChannelUpdate { a, .. } => {
                if self.rec.talkgroup.is_none() && a.1 != 0 {
                    self.rec.talkgroup = Some(a.1);
                }
            }
            // Call termination ends a call, but the TDU/TDULC frame that carries
            // it already drives the call boundary (see `observe`), so folding it
            // here would double-count. Affiliation queries and extended-function
            // commands are network signaling about a radio, not this call's
            // content. So is a unit-to-unit answer request: the network pages
            // the called radio on every busy traffic channel, inside whatever
            // group call is up there, and its source is not this call's talker.
            // All four are no-ops for the call record.
            LinkControl::CallTermination { .. }
            | LinkControl::UnitToUnitAnswerRequest { .. }
            | LinkControl::GroupAffiliationQuery { .. }
            | LinkControl::ExtendedFunctionCommand { .. }
            | LinkControl::Other { .. } => {}
        }
    }

    fn set_source(&mut self, source: u32) {
        if source == 0 {
            return;
        }
        // A talker change mid-call invalidates the previous talker's L3Harris
        // alias blocks: they are only reset in finish(), so a leftover half
        // from "ENGINE1"/"2-LONG " would be read as the new talker's tail
        // ("CAR5" → "CAR5   2-LONG"). The new talker re-sends its own blocks
        // on the following superframes.
        if self.rec.source.is_some_and(|prev| prev != source) {
            self.alias.reset();
            // The SUID extension qualifies one talker; carrying it across a
            // talker change would attribute a roaming radio's WACN/system to
            // the next, local, talker.
            self.rec.source_wacn = None;
            self.rec.source_system = None;
        }
        self.rec.source = Some(source);
        if !self.rec.talkers.contains(&source) {
            self.rec.talkers.push(source);
        }
    }

    fn apply_crypto(&mut self, algid: u8, kid: u16, mi: [u8; 9]) {
        self.rec.algid = Some(algid);
        self.rec.key_id = Some(kid);
        self.rec.mi = Some(mi);
        // Shared rule with `EncryptionSync::is_clear` / `HduHeader::is_clear`
        // (see `payload::is_clear_params`): an all-zero encryption word means
        // "no parameters sent", not the ACCORDION 3 cipher.
        if !is_clear_params(algid, kid, &mi) {
            self.rec.encrypted = true;
        }
    }

    /// Finalize the in-progress record and reset for the next call.
    fn finish(&mut self) -> Option<CallRecord> {
        let mut rec = std::mem::take(&mut self.rec);
        rec.talker_alias = self.alias.to_alias();
        self.alias.reset();
        self.active = false;
        Some(rec)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::LduKind;
    use crate::message::ServiceOptions;
    use crate::payload::{EncryptionSync, HarrisLcOpcode, HduHeader};

    fn suid_lc(wacn: u32, system: u16, source: u32) -> P25Event {
        P25Event::LinkControl {
            word: [0u8; 9],
            parsed: LinkControl::SourceIdExtension {
                wacn,
                system,
                source,
            },
            corrected: 0,
        }
    }

    #[test]
    fn the_suid_extension_rides_the_finished_record() {
        let mut a = CallAssembler::new();
        a.observe(&group_lc(52_101, 9_100_025, false));
        a.observe(&suid_lc(284_004, 57, 9_100_025));
        let rec = a
            .observe(&P25Event::Tdu)
            .expect("the terminator ends the call");
        assert_eq!(rec.source, Some(9_100_025));
        assert_eq!(rec.source_wacn, Some(284_004));
        assert_eq!(rec.source_system, Some(57));
    }

    #[test]
    fn a_talker_change_clears_the_previous_talkers_suid() {
        let mut a = CallAssembler::new();
        a.observe(&group_lc(52_101, 9_100_025, false));
        a.observe(&suid_lc(284_004, 57, 9_100_025));
        // A local radio takes over: its 24-bit id must not inherit the roaming
        // radio's WACN/system.
        a.observe(&group_lc(52_101, 1_620_719, false));
        let rec = a.observe(&P25Event::Tdu).expect("terminator");
        assert_eq!(rec.source, Some(1_620_719));
        assert_eq!(rec.source_wacn, None);
        assert_eq!(rec.source_system, None);
    }

    #[test]
    fn a_suid_naming_another_radio_does_not_overwrite_the_latch() {
        let mut a = CallAssembler::new();
        a.observe(&group_lc(52_101, 1_620_719, false));
        a.observe(&suid_lc(284_004, 57, 9_100_025));
        let rec = a.observe(&P25Event::Tdu).expect("terminator");
        assert_eq!(rec.source_wacn, None, "the word named a different radio");
        assert_eq!(rec.source_system, None);
    }

    #[test]
    fn a_suid_before_any_talker_is_kept() {
        // LCO 9 can arrive on the superframe before the LCO 0 that names the
        // talker; with no talker yet there is nothing to contradict it.
        let mut a = CallAssembler::new();
        a.observe(&suid_lc(284_004, 57, 9_100_025));
        a.observe(&group_lc(52_101, 9_100_025, false));
        let rec = a.observe(&P25Event::Tdu).expect("terminator");
        assert_eq!(rec.source_wacn, Some(284_004));
        assert_eq!(rec.source_system, Some(57));
    }

    #[test]
    fn an_answer_request_inside_a_group_call_changes_nothing() {
        // The network pages a called radio on every busy traffic channel, so
        // the page lands inside unrelated group calls. Neither radio it names
        // is this call's talker or target. Word shape from a test capture (IDs synthetic).
        let mut a = CallAssembler::new();
        a.observe(&group_lc(52_101, 1_620_719, false));
        a.observe(&P25Event::LinkControl {
            word: [0x45, 0x00, 0x00, 0x89, 0x54, 0x41, 0x89, 0x54, 0x42],
            parsed: LinkControl::parse(&[0x45, 0x00, 0x00, 0x89, 0x54, 0x41, 0x89, 0x54, 0x42]),
            corrected: 0,
        });
        let rec = a.observe(&P25Event::Tdu).expect("terminator");
        assert_eq!(rec.talkgroup, Some(52_101));
        assert_eq!(rec.source, Some(1_620_719));
        assert_eq!(rec.talkers, vec![1_620_719]);
        assert_eq!(rec.target, None);
    }

    fn group_lc(tg: u16, src: u32, emergency: bool) -> P25Event {
        P25Event::LinkControl {
            word: [0u8; 9],
            parsed: LinkControl::GroupVoiceChannelUser {
                options: ServiceOptions {
                    emergency,
                    encrypted: false,
                    duplex: false,
                    packet_mode: false,
                    priority: 0,
                },
                talkgroup: tg,
                source_ext_flag: false,
                source: src,
            },
            corrected: 0,
        }
    }

    fn harris_alias(opcode: HarrisLcOpcode, text: &[u8; 7]) -> P25Event {
        let mut word = [0u8; 9];
        word[1] = 0xA4;
        word[2..9].copy_from_slice(text);
        P25Event::LinkControl {
            word,
            parsed: LinkControl::Harris { opcode, word },
            corrected: 0,
        }
    }

    fn es(algid: u8, kid: u16) -> P25Event {
        P25Event::EncryptionSync {
            word: [0u8; 12],
            parsed: EncryptionSync {
                mi: [1, 2, 3, 4, 5, 6, 7, 8, 9],
                algid,
                kid,
            },
            corrected: 0,
        }
    }

    fn imbe() -> P25Event {
        P25Event::ImbeFrame {
            ldu: LduKind::Ldu1,
            index: 0,
            frame: [0u8; 18],
        }
    }

    fn invalid_hdu() -> P25Event {
        P25Event::Hdu {
            valid: false,
            data: [0u8; 15],
            header: None,
            corrected: 0,
        }
    }

    fn hdu(tg: u16, algid: u8) -> P25Event {
        P25Event::Hdu {
            valid: true,
            data: [0u8; 15],
            header: Some(HduHeader {
                mi: [0u8; 9],
                mfid: 0,
                algid,
                kid: 0,
                talkgroup: tg,
            }),
            corrected: 0,
        }
    }

    #[test]
    fn assembles_clear_group_call() {
        let mut a = CallAssembler::new();
        assert!(a.observe(&hdu(52101, 0x80)).is_none());
        assert!(a.observe(&group_lc(52101, 0x0A1B2C, false)).is_none());
        assert!(a.observe(&es(0x80, 0)).is_none());
        for _ in 0..18 {
            a.observe(&imbe());
        }
        let rec = a.observe(&P25Event::Tdu).expect("call closes on TDU");
        assert_eq!(rec.talkgroup, Some(52101));
        assert_eq!(rec.source, Some(0x0A1B2C));
        assert_eq!(rec.talkers, vec![0x0A1B2C]);
        assert!(!rec.encrypted);
        assert_eq!(rec.algorithm(), "UNENCRYPTED");
        assert_eq!(rec.voice_frames, 18);
        assert_eq!(rec.duration_ms(), 360);
        assert!(rec.terminated);
        assert!(!a.is_active());
    }

    #[test]
    fn flags_encryption_and_key() {
        let mut a = CallAssembler::new();
        a.observe(&hdu(1234, 0x84));
        a.observe(&group_lc(1234, 42, false));
        a.observe(&es(0x84, 0x044c));
        let rec = a.take().expect("EOF flush");
        assert!(rec.encrypted);
        assert_eq!(rec.algid, Some(0x84));
        assert_eq!(rec.algorithm(), "AES-256");
        assert_eq!(rec.key_id, Some(0x044c));
        assert!(!rec.terminated); // flushed, not terminated
    }

    #[test]
    fn assembles_harris_talker_alias() {
        let mut a = CallAssembler::new();
        a.observe(&group_lc(7, 5, false));
        // "UNIT_5\0" — the NUL terminates the field inside block 1.
        a.observe(&harris_alias(HarrisLcOpcode::TalkerAlias1, b"UNIT_5\0"));
        let rec = a.observe(&P25Event::Tdu).unwrap();
        assert_eq!(rec.talker_alias.as_deref(), Some("UNIT_5"));
    }

    #[test]
    fn a_full_first_block_waits_for_its_tail() {
        // Block 1 filling all 7 octets means the field runs on into block 2;
        // publishing it alone is where the archive's bare `UNIT_X_` came from.
        let mut a = CallAssembler::new();
        a.observe(&group_lc(7, 5, false));
        a.observe(&harris_alias(HarrisLcOpcode::TalkerAlias1, b"UNIT_X_"));
        let rec = a.observe(&P25Event::Tdu).unwrap();
        assert_eq!(rec.talker_alias, None);
    }

    #[test]
    fn an_alias_block_with_no_call_open_starts_nothing() {
        // The alias cycle runs on past the previous call's terminator.
        let mut a = CallAssembler::new();
        assert!(a
            .observe(&harris_alias(HarrisLcOpcode::TalkerAlias2, b"BASE1\0\0"))
            .is_none());
        assert!(!a.is_active(), "a stray alias block must not open a call");
        // The next call must not inherit it.
        a.observe(&hdu(7, 0x80));
        a.observe(&group_lc(7, 5, false));
        a.observe(&harris_alias(HarrisLcOpcode::TalkerAlias1, b"DISPATC"));
        a.observe(&harris_alias(
            HarrisLcOpcode::TalkerAlias2,
            b"H\0\0\0\0\0\0",
        ));
        let rec = a.observe(&P25Event::Tdu).unwrap();
        assert_eq!(rec.talker_alias.as_deref(), Some("DISPATCH"));
    }

    #[test]
    fn tracks_talker_changes_and_emergency() {
        let mut a = CallAssembler::new();
        a.observe(&group_lc(100, 11, false));
        a.observe(&group_lc(100, 22, true)); // new talker, emergency
        a.observe(&group_lc(100, 11, false)); // back to first talker (no dup)
        let rec = a.observe(&P25Event::Tdu).unwrap();
        assert_eq!(rec.talkers, vec![11, 22]);
        assert_eq!(rec.source, Some(11));
        assert!(rec.emergency);
    }

    #[test]
    fn invalid_hdu_does_not_split_a_live_call() {
        // A false-positive NID mid-call must not fragment the call record.
        let mut a = CallAssembler::new();
        a.observe(&hdu(100, 0x80));
        a.observe(&group_lc(100, 11, false));
        for _ in 0..9 {
            a.observe(&imbe());
        }
        assert!(
            a.observe(&invalid_hdu()).is_none(),
            "an invalid HDU must not close the open call"
        );
        for _ in 0..9 {
            a.observe(&imbe());
        }
        let rec = a.observe(&P25Event::Tdu).expect("call closes on TDU");
        assert_eq!(rec.talkgroup, Some(100));
        assert_eq!(rec.source, Some(11));
        assert_eq!(rec.voice_frames, 18, "the call must not have been split");

        // With no call open, an invalid HDU starts nothing either.
        let mut b = CallAssembler::new();
        assert!(b.observe(&invalid_hdu()).is_none());
        assert!(!b.is_active());
    }

    #[test]
    fn talker_change_clears_stale_alias_blocks() {
        // Blocks may arrive out of order, and a mid-call talker change must not
        // let the previous talker's blocks bleed into the new alias.
        let mut a = CallAssembler::new();
        a.observe(&group_lc(7, 11, false));
        a.observe(&harris_alias(HarrisLcOpcode::TalkerAlias2, b"2-LONG "));
        a.observe(&harris_alias(HarrisLcOpcode::TalkerAlias1, b"ENGINE1"));
        // Talker changes; the new talker sends a single (shorter) alias block.
        a.observe(&group_lc(7, 22, false));
        a.observe(&harris_alias(HarrisLcOpcode::TalkerAlias1, b"CAR5\0\0\0"));
        let rec = a.observe(&P25Event::Tdu).unwrap();
        assert_eq!(rec.talker_alias.as_deref(), Some("CAR5"));
        assert_eq!(rec.talkers, vec![11, 22]);
    }

    #[test]
    fn out_of_order_alias_blocks_assemble_in_field_order() {
        // A captured block sequence (unit name replaced),
        // with the tail arriving first: the field is octets 0..7 then 7..14,
        // never the arrival order.
        let mut a = CallAssembler::new();
        a.observe(&group_lc(7, 5, false));
        a.observe(&harris_alias(HarrisLcOpcode::TalkerAlias2, b"BASE1\0\0"));
        a.observe(&harris_alias(HarrisLcOpcode::TalkerAlias1, b"UNIT_X_"));
        // Blocks 3/4 repeat 1/2 verbatim and must not double-append.
        a.observe(&harris_alias(HarrisLcOpcode::TalkerAlias3, b"UNIT_X_"));
        a.observe(&harris_alias(HarrisLcOpcode::TalkerAlias4, b"BASE1\0\0"));
        let rec = a.observe(&P25Event::Tdu).unwrap();
        assert_eq!(rec.talker_alias.as_deref(), Some("UNIT_X_BASE1"));
    }

    #[test]
    fn an_alias_never_carries_into_the_next_call() {
        let mut a = CallAssembler::new();
        a.observe(&hdu(7, 0x80));
        a.observe(&group_lc(7, 5, false));
        a.observe(&harris_alias(HarrisLcOpcode::TalkerAlias1, b"UNIT_X_"));
        a.observe(&harris_alias(HarrisLcOpcode::TalkerAlias2, b"BASE1\0\0"));
        let first = a.observe(&P25Event::Tdu).unwrap();
        assert_eq!(first.talker_alias.as_deref(), Some("UNIT_X_BASE1"));

        a.observe(&hdu(7, 0x80));
        a.observe(&group_lc(7, 9, false));
        let second = a.observe(&P25Event::Tdu).unwrap();
        assert_eq!(second.talker_alias, None, "no carry-over into call two");
    }

    #[test]
    fn clear_contract_agrees_across_hdu_es_and_call_record() {
        use crate::payload::is_clear_params;

        // (algid, kid, mi, expected_clear)
        let zero = [0u8; 9];
        let mi = [1u8, 2, 3, 4, 5, 6, 7, 8, 9];
        let cases: [(u8, u16, [u8; 9], bool); 6] = [
            (0x80, 0, zero, true),       // UNENCRYPTED
            (0x80, 0x1234, mi, true),    // UNENCRYPTED wins over stray KID/MI
            (0x00, 0, zero, true),       // all-zero word = nothing sent
            (0x00, 0x0001, zero, false), // ACCORDION 3 with a real key
            (0x00, 0, mi, false),        // ACCORDION 3 with a real MI
            (0x84, 0x044c, mi, false),   // AES-256
        ];

        for (algid, kid, mi, expect_clear) in cases {
            assert_eq!(
                is_clear_params(algid, kid, &mi),
                expect_clear,
                "is_clear_params({algid:#04x}, {kid:#06x})"
            );

            let mut w = [0u8; 12];
            w[0..9].copy_from_slice(&mi);
            w[9] = algid;
            w[10] = (kid >> 8) as u8;
            w[11] = kid as u8;
            assert_eq!(
                EncryptionSync::parse(&w).is_clear(),
                expect_clear,
                "EncryptionSync disagrees for algid {algid:#04x}"
            );

            let mut d = [0u8; 15];
            d[0..9].copy_from_slice(&mi);
            d[10] = algid;
            d[11] = (kid >> 8) as u8;
            d[12] = kid as u8;
            assert_eq!(
                HduHeader::parse(&d).is_clear(),
                expect_clear,
                "HduHeader disagrees for algid {algid:#04x}"
            );

            // ...and the CallAssembler's `encrypted` flag is its negation.
            let mut a = CallAssembler::new();
            a.observe(&P25Event::EncryptionSync {
                word: w,
                parsed: EncryptionSync::parse(&w),
                corrected: 0,
            });
            let rec = a.take().expect("flush");
            assert_eq!(
                rec.encrypted, !expect_clear,
                "CallRecord.encrypted disagrees for algid {algid:#04x} kid {kid:#06x}"
            );
        }
    }

    #[test]
    fn new_hdu_closes_previous_call() {
        let mut a = CallAssembler::new();
        a.observe(&hdu(100, 0x80));
        a.observe(&group_lc(100, 11, false));
        // Second HDU with no terminator between → previous call is returned.
        let first = a.observe(&hdu(200, 0x80)).expect("prior call flushed");
        assert_eq!(first.talkgroup, Some(100));
        assert!(!first.terminated);
        a.observe(&group_lc(200, 22, false));
        let second = a.observe(&P25Event::Tdu).unwrap();
        assert_eq!(second.talkgroup, Some(200));
        assert_eq!(second.source, Some(22));
    }
}
