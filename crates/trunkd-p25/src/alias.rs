// SPDX-License-Identifier: GPL-3.0-or-later
//! L3Harris talker-alias assembly (spec §18.1).
//!
//! An L3Harris (MFID 164) subscriber announces its alias on the traffic
//! channel with Link Control opcodes 50–53, each a 9-octet LC word whose 7
//! payload octets follow the LCO and MFID bytes. Observed on a live Harris
//! system, the four opcodes are **not** four successive pieces of a 28-byte
//! string: the alias is a **14-octet NUL-padded ASCII field carried by a pair
//! of blocks**, and the second pair repeats the first verbatim.
//!
//! ```text
//! LCO 50  32 a4 55 4e 49 54 5f 58 5f   "UNIT_X_"   field[0..7]
//! LCO 51  33 a4 42 41 53 45 31 00 00   "BASE1\0\0" field[7..14]
//! LCO 52  34 a4 55 4e 49 54 5f 58 5f   "UNIT_X_"   field[0..7]  (repeat)
//! LCO 53  35 a4 42 41 53 45 31 00 00   "BASE1\0\0" field[7..14] (repeat)
//! ```
//!
//! The blocks cycle 1,2,3,4,1,2,… once per LDU superframe (≈360 ms), so a
//! short transmission carries only a prefix of the cycle. Concatenating
//! whichever blocks happened to arrive — the decoder's previous behaviour —
//! is what produced `BASE1UNIT_X_` (blocks 2,3), `UNIT_X_BASE1BASE1`
//! (blocks 1,2,4), `UNIT_X_UNIT_X_BASE1` (blocks 1,3,4), the bare halves
//! `UNIT_X_` / `BASE1`, and one-letter aliases such as `H` (block 2 of
//! `DISPATCH` alone).
//!
//! [`TalkerAliasAssembler`] therefore folds 50/52 onto half 0 and 51/53 onto
//! half 1, and publishes an alias only once the field is known to be
//! *complete*: either half 0 contains the NUL that terminates the field, or
//! both halves are present. A block whose content contradicts what that half
//! already holds starts a new alias — the old one is dropped rather than
//! merged, because the only way a half changes mid-assembly is that the
//! talker changed or a block from the previous transmission was still in the
//! buffer.

use std::collections::HashMap;

use crate::payload::HarrisLcOpcode;

/// Payload octets in one talker-alias LC block.
pub const ALIAS_BLOCK_LEN: usize = 7;

/// Octets in the assembled talker-alias field (two blocks).
pub const ALIAS_FIELD_LEN: usize = 2 * ALIAS_BLOCK_LEN;

/// Assembles the L3Harris talker alias for **one talker of one call**.
///
/// Reset it on a talker change and at call end ([`reset`](Self::reset)); it
/// holds no channel- or call-spanning state of its own.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TalkerAliasAssembler {
    /// The two halves of the 14-octet field, by block parity.
    halves: [Option<[u8; ALIAS_BLOCK_LEN]>; 2],
    /// The alias, published only once the field is complete.
    alias: Option<String>,
}

impl TalkerAliasAssembler {
    /// A fresh assembler holding no blocks.
    pub fn new() -> Self {
        Self::default()
    }

    /// Which half of the 14-octet field an L3Harris LC opcode carries.
    ///
    /// Blocks 1 and 3 carry octets 0–6; blocks 2 and 4 carry octets 7–13.
    /// `None` for any opcode that is not a talker-alias block.
    pub fn half_of(opcode: HarrisLcOpcode) -> Option<usize> {
        match opcode {
            HarrisLcOpcode::TalkerAlias1 | HarrisLcOpcode::TalkerAlias3 => Some(0),
            HarrisLcOpcode::TalkerAlias2 | HarrisLcOpcode::TalkerAlias4 => Some(1),
            _ => None,
        }
    }

    /// True if `opcode` is one of the four talker-alias blocks.
    pub fn is_alias_block(opcode: HarrisLcOpcode) -> bool {
        Self::half_of(opcode).is_some()
    }

    /// Which half an L3Harris LC word carries and its 7 payload octets, or
    /// `None` for a word that is not a talker-alias block.
    pub fn block_of(
        opcode: HarrisLcOpcode,
        word: &[u8; 9],
    ) -> Option<(usize, [u8; ALIAS_BLOCK_LEN])> {
        let half = Self::half_of(opcode)?;
        let mut block = [0u8; ALIAS_BLOCK_LEN];
        // The 7 payload octets follow the LCO byte and the MFID byte.
        block.copy_from_slice(&word[2..9]);
        Some((half, block))
    }

    /// Fold one L3Harris LC word in. Returns true when the published alias
    /// changed (including a change back to `None` when a contradicting block
    /// discarded a complete alias). Non-alias opcodes are ignored.
    pub fn observe(&mut self, opcode: HarrisLcOpcode, word: &[u8; 9]) -> bool {
        match Self::block_of(opcode, word) {
            Some((half, block)) => self.put(half, block),
            None => false,
        }
    }

    /// The two halves held so far, by block parity.
    pub fn halves(&self) -> [Option<[u8; ALIAS_BLOCK_LEN]>; 2] {
        self.halves
    }

    /// The first half, when it is all that arrived of a field that runs on
    /// into the second: seven octets with no NUL and no tail. On its own that
    /// is never an alias (see [`assemble`](Self::assemble)); an
    /// [`AliasMemory`] can still recognise it as the head of one it knows.
    pub fn head_only(&self) -> Option<[u8; ALIAS_BLOCK_LEN]> {
        match self.halves {
            [Some(head), None] if !head.contains(&0) => Some(head),
            _ => None,
        }
    }

    /// Fold one half's payload in directly. Returns true if the published
    /// alias changed.
    pub fn put(&mut self, half: usize, block: [u8; ALIAS_BLOCK_LEN]) -> bool {
        debug_assert!(half < 2);
        // The same half re-sent unchanged is the protocol's own repetition
        // (blocks 3/4 repeat 1/2, and the cycle repeats every superframe).
        if self.halves[half] == Some(block) {
            return false;
        }
        let before = self.alias.take();
        // A half that changes content is a *different* alias: a new talker, or
        // a block left over from the previous transmission on this frequency.
        // Appending it is what cross-contaminated radios; drop what we had.
        if self.halves[half].is_some() {
            self.halves = Default::default();
        }
        self.halves[half] = Some(block);
        self.alias = self.assemble();
        self.alias != before
    }

    /// Forget every block and the published alias.
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// The assembled alias, or `None` while the field is still incomplete.
    pub fn alias(&self) -> Option<&str> {
        self.alias.as_deref()
    }

    /// The assembled alias as an owned `String`, or `None`.
    pub fn to_alias(&self) -> Option<String> {
        self.alias.clone()
    }

    /// The alias text, but only when the field is known to be complete.
    ///
    /// Half 1 on its own is never an alias — it is the *tail* of one, and
    /// publishing it is where `BASE1`, `H` and `HP` came from. Half 0 alone is
    /// the whole alias only when it carries the field's NUL terminator; a full
    /// 7 octets means the field runs on into half 1, which must arrive first.
    fn assemble(&self) -> Option<String> {
        let first = self.halves[0]?;
        let mut field = Vec::with_capacity(ALIAS_FIELD_LEN);
        field.extend_from_slice(&first);
        if !first.contains(&0) {
            field.extend_from_slice(&self.halves[1]?);
        }
        let end = field.iter().position(|&b| b == 0).unwrap_or(field.len());
        let text: String = field[..end]
            .iter()
            .copied()
            .filter(|b| (0x20..0x7f).contains(b))
            .map(char::from)
            .collect();
        let text = text.trim();
        if text.is_empty() {
            None
        } else {
            Some(text.to_owned())
        }
    }
}

/// Radios [`AliasMemory`] keeps by default: several days of a county's
/// aliased radios (a busy day names ~730), in about half a megabyte.
pub const ALIAS_MEMORY_CAPACITY: usize = 4096;

/// How long a remembered alias stays usable after it was last heard complete.
pub const ALIAS_MEMORY_TTL_MS: u64 = 24 * 60 * 60 * 1000;

/// The last complete talker alias heard from each radio, so a call that
/// carried only the first half of a longer field can still be named.
///
/// An alias exactly filling its first half (`UNIT_A1`, `UNIT_B2`) or running
/// on into the second (`FIRE_ENGINE_8`) is complete only once the second half
/// arrives, and a short transmission often ends before it does: the
/// assembler then publishes nothing, rightly, because seven octets with no
/// NUL could be the start of anything. If *this radio* has been heard with a
/// complete alias whose first seven octets are exactly those, the call names
/// that alias. Nothing else is ever inferred: a radio with no complete alias
/// on record, or whose head does not match, publishes nothing.
///
/// Bounded: at most `capacity` radios, the least recently heard evicted
/// first, and an entry older than `ttl_ms` is never used.
#[derive(Debug, Clone)]
pub struct AliasMemory {
    entries: HashMap<u32, Remembered>,
    capacity: usize,
    ttl_ms: u64,
}

#[derive(Debug, Clone)]
struct Remembered {
    alias: String,
    /// The field's two halves as heard; the second is `None` when the alias
    /// ended inside the first.
    halves: [Option<[u8; ALIAS_BLOCK_LEN]>; 2],
    last_ms: u64,
}

impl Default for AliasMemory {
    fn default() -> Self {
        Self::new(ALIAS_MEMORY_CAPACITY, ALIAS_MEMORY_TTL_MS)
    }
}

impl AliasMemory {
    /// An empty memory holding at most `capacity` radios for `ttl_ms` each.
    pub fn new(capacity: usize, ttl_ms: u64) -> Self {
        AliasMemory {
            entries: HashMap::new(),
            capacity: capacity.max(1),
            ttl_ms,
        }
    }

    /// Radios remembered (expired entries included until they are evicted).
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when nothing is remembered.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Record `radio`'s complete alias and the halves it was assembled from
    /// (the second `None` when the alias ended inside the first).
    pub fn remember(
        &mut self,
        radio: u32,
        alias: &str,
        halves: [Option<[u8; ALIAS_BLOCK_LEN]>; 2],
        now_ms: u64,
    ) {
        if radio == 0 {
            return;
        }
        if !self.entries.contains_key(&radio) && self.entries.len() >= self.capacity {
            let ttl = self.ttl_ms;
            self.entries
                .retain(|_, e| now_ms.saturating_sub(e.last_ms) <= ttl);
            if self.entries.len() >= self.capacity {
                if let Some(oldest) = self
                    .entries
                    .iter()
                    .min_by_key(|(_, e)| e.last_ms)
                    .map(|(r, _)| *r)
                {
                    self.entries.remove(&oldest);
                }
            }
        }
        self.entries.insert(
            radio,
            Remembered {
                alias: alias.to_owned(),
                halves,
                last_ms: now_ms,
            },
        );
    }

    /// The alias `radio` was last heard with complete, if its field began with
    /// exactly `head` and it was heard within the time-to-live.
    pub fn complete(&self, radio: u32, head: &[u8; ALIAS_BLOCK_LEN], now_ms: u64) -> Option<&str> {
        let e = self.live(radio, now_ms)?;
        (e.halves[0] == Some(*head)).then_some(e.alias.as_str())
    }

    /// The `half` of `radio`'s remembered alias field, if it has one.
    pub fn own_half(&self, radio: u32, half: usize, now_ms: u64) -> Option<[u8; ALIAS_BLOCK_LEN]> {
        self.live(radio, now_ms)?
            .halves
            .get(half)
            .copied()
            .flatten()
    }

    /// Whether some radio other than `radio` was heard complete within the
    /// last `within_ms` with exactly `block` as the `half` of its alias field:
    /// the evidence that a block on `radio`'s call is another radio's alias,
    /// still being cycled through the site. Only a *recent* sighting counts —
    /// a fleet sharing a prefix (`UNIT_X_…`, `MEDIC_…`) would otherwise have
    /// every member's head claimed by all the others.
    pub fn claimed_by_other(
        &self,
        radio: u32,
        half: usize,
        block: &[u8; ALIAS_BLOCK_LEN],
        now_ms: u64,
        within_ms: u64,
    ) -> bool {
        self.entries.iter().any(|(r, e)| {
            *r != radio
                && e.halves.get(half).copied().flatten() == Some(*block)
                && now_ms.saturating_sub(e.last_ms) <= within_ms.min(self.ttl_ms)
        })
    }

    fn live(&self, radio: u32, now_ms: u64) -> Option<&Remembered> {
        self.entries
            .get(&radio)
            .filter(|e| now_ms.saturating_sub(e.last_ms) <= self.ttl_ms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build the LC word a talker-alias block arrives in.
    fn word(text: &[u8; 7]) -> [u8; 9] {
        let mut w = [0u8; 9];
        w[1] = 0xA4;
        w[2..9].copy_from_slice(text);
        w
    }

    fn feed(a: &mut TalkerAliasAssembler, op: HarrisLcOpcode, text: &[u8; 7]) -> bool {
        a.observe(op, &word(text))
    }

    // Block sequences as a live L3Harris system sends them (captured in an
    // event log; unit names replaced). Payload octets only (LCO + MFID stripped).
    const UNIT_X_1: &[u8; 7] = b"UNIT_X_";
    const UNIT_X_2: &[u8; 7] = b"BASE1\0\0";
    const UNIT9_1: &[u8; 7] = b"UNIT_9\0";
    const PAD: &[u8; 7] = b"\0\0\0\0\0\0\0";
    const DISPATCH_1: &[u8; 7] = b"DISPATC";
    const DISPATCH_2: &[u8; 7] = b"H\0\0\0\0\0\0";

    #[test]
    fn in_order_pair_assembles() {
        // Two blocks about a second apart on one traffic channel.
        //   32 a4 554e49545f585f   "UNIT_X_"
        //   33 a4 42415345310000   "BASE1\0\0"
        let mut a = TalkerAliasAssembler::new();
        assert!(!feed(&mut a, HarrisLcOpcode::TalkerAlias1, UNIT_X_1));
        assert_eq!(a.alias(), None, "half 0 is full — the field runs on");
        assert!(feed(&mut a, HarrisLcOpcode::TalkerAlias2, UNIT_X_2));
        assert_eq!(a.alias(), Some("UNIT_X_BASE1"));
    }

    #[test]
    fn nul_terminated_first_half_needs_no_second() {
        // One radio's alias that fits in the first half:
        //   32 a4 554e49545f3900   "UNIT_9\0"
        //   33 a4 00000000000000   all pad
        let mut a = TalkerAliasAssembler::new();
        assert!(feed(&mut a, HarrisLcOpcode::TalkerAlias1, UNIT9_1));
        assert_eq!(a.alias(), Some("UNIT_9"));
        assert!(!feed(&mut a, HarrisLcOpcode::TalkerAlias2, PAD));
        assert_eq!(a.alias(), Some("UNIT_9"), "the pad block changes nothing");
    }

    #[test]
    fn out_of_order_pair_still_assembles() {
        let mut a = TalkerAliasAssembler::new();
        feed(&mut a, HarrisLcOpcode::TalkerAlias2, UNIT_X_2);
        assert_eq!(a.alias(), None, "a tail alone is never an alias");
        feed(&mut a, HarrisLcOpcode::TalkerAlias1, UNIT_X_1);
        assert_eq!(a.alias(), Some("UNIT_X_BASE1"));
    }

    #[test]
    fn repeat_pair_does_not_double_append() {
        // About two seconds on one traffic channel: the full cycle
        // 3,4,1,2 of "DISPATCH" — blocks 3/4 repeat blocks 1/2 verbatim.
        //   34 a4 44495350415443   "DISPATC"
        //   35 a4 48000000000000   "H\0\0\0\0\0\0"
        //   32 a4 44495350415443   "DISPATC"
        //   33 a4 48000000000000   "H\0\0\0\0\0\0"
        let mut a = TalkerAliasAssembler::new();
        feed(&mut a, HarrisLcOpcode::TalkerAlias3, DISPATCH_1);
        feed(&mut a, HarrisLcOpcode::TalkerAlias4, DISPATCH_2);
        assert_eq!(a.alias(), Some("DISPATCH"));
        assert!(!feed(&mut a, HarrisLcOpcode::TalkerAlias1, DISPATCH_1));
        assert!(!feed(&mut a, HarrisLcOpcode::TalkerAlias2, DISPATCH_2));
        assert_eq!(
            a.alias(),
            Some("DISPATCH"),
            "the repeat cycle is idempotent"
        );
    }

    #[test]
    fn tail_only_never_publishes() {
        // `BASE1`, `H`, `E`, `HP` in the archive are all half 1 on its own.
        let mut a = TalkerAliasAssembler::new();
        feed(&mut a, HarrisLcOpcode::TalkerAlias2, UNIT_X_2);
        feed(&mut a, HarrisLcOpcode::TalkerAlias4, UNIT_X_2);
        assert_eq!(a.alias(), None);
    }

    #[test]
    fn missing_middle_block_never_publishes_a_splice() {
        // Blocks 1,2,4 with 3 lost used to yield "UNIT_X_BASE1BASE1"; blocks
        // 2,3 used to yield "BASE1UNIT_X_"; blocks 1,3,4 "UNIT_X_UNIT_X_BASE1".
        let mut a = TalkerAliasAssembler::new();
        feed(&mut a, HarrisLcOpcode::TalkerAlias1, UNIT_X_1);
        feed(&mut a, HarrisLcOpcode::TalkerAlias2, UNIT_X_2);
        feed(&mut a, HarrisLcOpcode::TalkerAlias4, UNIT_X_2);
        assert_eq!(a.alias(), Some("UNIT_X_BASE1"));

        let mut b = TalkerAliasAssembler::new();
        feed(&mut b, HarrisLcOpcode::TalkerAlias2, UNIT_X_2);
        feed(&mut b, HarrisLcOpcode::TalkerAlias3, UNIT_X_1);
        assert_eq!(b.alias(), Some("UNIT_X_BASE1"));

        let mut c = TalkerAliasAssembler::new();
        feed(&mut c, HarrisLcOpcode::TalkerAlias1, UNIT_X_1);
        feed(&mut c, HarrisLcOpcode::TalkerAlias3, UNIT_X_1);
        feed(&mut c, HarrisLcOpcode::TalkerAlias4, UNIT_X_2);
        assert_eq!(c.alias(), Some("UNIT_X_BASE1"));
    }

    #[test]
    fn incomplete_field_never_publishes_a_prefix() {
        // Only half 0, and it fills all 7 octets: the field continues. The
        // archive's bare `UNIT_X_` came from publishing this.
        let mut a = TalkerAliasAssembler::new();
        feed(&mut a, HarrisLcOpcode::TalkerAlias1, UNIT_X_1);
        feed(&mut a, HarrisLcOpcode::TalkerAlias3, UNIT_X_1);
        assert_eq!(a.alias(), None);
    }

    #[test]
    fn contradicting_block_starts_a_new_alias() {
        // A block from the previous transmission still in the buffer, or a new
        // talker: half 0 changes, so the half 1 we hold belongs to neither.
        let mut a = TalkerAliasAssembler::new();
        feed(&mut a, HarrisLcOpcode::TalkerAlias1, UNIT_X_1);
        feed(&mut a, HarrisLcOpcode::TalkerAlias2, UNIT_X_2);
        assert_eq!(a.alias(), Some("UNIT_X_BASE1"));
        assert!(
            feed(&mut a, HarrisLcOpcode::TalkerAlias1, DISPATCH_1),
            "a contradicting head retracts the alias"
        );
        assert_eq!(a.alias(), None, "must not read UNIT_X_'s tail onto DISPATC");
        feed(&mut a, HarrisLcOpcode::TalkerAlias2, DISPATCH_2);
        assert_eq!(a.alias(), Some("DISPATCH"));
    }

    #[test]
    fn reset_drops_everything() {
        let mut a = TalkerAliasAssembler::new();
        feed(&mut a, HarrisLcOpcode::TalkerAlias1, UNIT_X_1);
        feed(&mut a, HarrisLcOpcode::TalkerAlias2, UNIT_X_2);
        a.reset();
        assert_eq!(a.alias(), None);
        // The tail of the dropped alias must not resurrect it.
        feed(&mut a, HarrisLcOpcode::TalkerAlias2, UNIT_X_2);
        assert_eq!(a.alias(), None);
    }

    #[test]
    fn full_fourteen_octet_field_assembles() {
        // `UNIT_X_BUS9401` and `FIRE_BATTALION` fill both halves with no NUL.
        let mut a = TalkerAliasAssembler::new();
        feed(&mut a, HarrisLcOpcode::TalkerAlias1, b"FIRE_BA");
        feed(&mut a, HarrisLcOpcode::TalkerAlias2, b"TTALION");
        assert_eq!(a.alias(), Some("FIRE_BATTALION"));
    }

    #[test]
    fn trailing_pad_is_trimmed() {
        let mut a = TalkerAliasAssembler::new();
        feed(&mut a, HarrisLcOpcode::TalkerAlias1, b"CAR5   ");
        feed(&mut a, HarrisLcOpcode::TalkerAlias2, PAD);
        assert_eq!(a.alias(), Some("CAR5"));
    }

    #[test]
    fn all_pad_is_no_alias() {
        let mut a = TalkerAliasAssembler::new();
        feed(&mut a, HarrisLcOpcode::TalkerAlias1, PAD);
        assert_eq!(a.alias(), None);
    }

    #[test]
    fn non_alias_opcodes_are_ignored() {
        let mut a = TalkerAliasAssembler::new();
        assert!(!a.observe(HarrisLcOpcode::TalkerGps1, &word(b"1234567")));
        assert!(!a.observe(HarrisLcOpcode::Unknown(7), &word(b"1234567")));
        assert_eq!(a.alias(), None);
    }

    #[test]
    fn head_only_is_a_full_first_half_with_no_tail() {
        let mut a = TalkerAliasAssembler::new();
        assert_eq!(a.head_only(), None);
        feed(&mut a, HarrisLcOpcode::TalkerAlias1, b"UNIT_A1");
        assert_eq!(a.head_only(), Some(*b"UNIT_A1"));
        feed(&mut a, HarrisLcOpcode::TalkerAlias2, PAD);
        assert_eq!(a.head_only(), None, "the tail arrived");
        assert_eq!(a.alias(), Some("UNIT_A1"));

        let mut b = TalkerAliasAssembler::new();
        feed(&mut b, HarrisLcOpcode::TalkerAlias1, UNIT9_1);
        assert_eq!(
            b.head_only(),
            None,
            "a NUL-terminated head is a whole alias"
        );
    }

    /// `UNIT_A1` exactly fills its first half, so a short call that
    /// carries only that half publishes nothing on its own — but the radio's
    /// earlier complete sighting names it. A different head, another radio, or
    /// a stale entry names nothing.
    #[test]
    fn memory_completes_a_known_head_and_nothing_else() {
        let mut m = AliasMemory::new(8, 1_000);
        m.remember(9_000_102, "UNIT_A1", [Some(*b"UNIT_A1"), Some([0; 7])], 100);
        m.remember(
            9_100_521,
            "FIRE_ENGINE_8",
            [Some(*b"FIRE_EN"), Some(*b"GINE_8\0")],
            100,
        );
        assert_eq!(m.complete(9_000_102, b"UNIT_A1", 500), Some("UNIT_A1"));
        assert_eq!(
            m.complete(9_100_521, b"FIRE_EN", 500),
            Some("FIRE_ENGINE_8")
        );
        assert_eq!(m.complete(9_100_521, b"FIRE_BA", 500), None, "another head");
        assert_eq!(
            m.complete(9_000_106, b"UNIT_A1", 500),
            None,
            "another radio"
        );
        assert_eq!(m.complete(9_000_102, b"UNIT_A1", 1_101), None, "expired");
        m.remember(0, "NOBODY", [Some(*b"NOBODY_"), None], 100);
        assert_eq!(m.len(), 2, "radio 0 is nobody");

        // Who else a block belongs to: the console's head is another radio's
        // on the car's call, never the console's own; tails count too.
        assert!(m.claimed_by_other(9_000_048, 0, b"UNIT_A1", 500, 1_000));
        assert!(!m.claimed_by_other(9_000_102, 0, b"UNIT_A1", 500, 1_000));
        assert!(m.claimed_by_other(9_000_048, 1, b"GINE_8\0", 500, 1_000));
        assert!(
            !m.claimed_by_other(9_000_048, 0, b"GINE_8\0", 500, 1_000),
            "other half"
        );
        assert!(
            !m.claimed_by_other(9_000_048, 0, b"UNIT_A1", 500, 399),
            "not recent"
        );
        assert!(
            !m.claimed_by_other(9_000_048, 0, b"UNIT_A1", 1_101, 5_000),
            "expired"
        );
        assert_eq!(m.own_half(9_100_521, 1, 500), Some(*b"GINE_8\0"));
        assert_eq!(m.own_half(9_000_048, 0, 500), None);
    }

    #[test]
    fn memory_is_bounded_and_evicts_the_least_recently_heard() {
        let mut m = AliasMemory::new(2, u64::MAX);
        m.remember(
            1,
            "A_ONE__X",
            [Some(*b"A_ONE__"), Some(*b"X\0\0\0\0\0\0")],
            10,
        );
        m.remember(
            2,
            "B_TWO__X",
            [Some(*b"B_TWO__"), Some(*b"X\0\0\0\0\0\0")],
            20,
        );
        m.remember(
            1,
            "A_ONE__X",
            [Some(*b"A_ONE__"), Some(*b"X\0\0\0\0\0\0")],
            30,
        ); // 1 heard again
        m.remember(
            3,
            "C_THREEX",
            [Some(*b"C_THREE"), Some(*b"X\0\0\0\0\0\0")],
            40,
        ); // evicts 2
        assert_eq!(m.len(), 2);
        assert!(m.complete(2, b"B_TWO__", 50).is_none());
        assert_eq!(m.complete(1, b"A_ONE__", 50), Some("A_ONE__X"));
        assert_eq!(m.complete(3, b"C_THREE", 50), Some("C_THREEX"));
    }
}
