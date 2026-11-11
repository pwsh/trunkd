// SPDX-License-Identifier: GPL-3.0-or-later
//! Framer state machine: soft sync correlation, NID capture, status-dibit
//! cadence, per-DUID assembly and dispatch (spec §4, §7).

use crate::bits::{BitBuf, Dibit};
use crate::edac::bch::{decode_nid, NacTracker};
use crate::edac::crc::{
    crc16_check_and_correct, crc32_pdu_check_and_correct, crc9_check, CrcResult,
};
use crate::edac::trellis;
use crate::event::{LduKind, P25Event};
use crate::frames;
use crate::interleave::deinterleave;
use crate::message::{parse_pdu_header, parse_tsbk, PduHeader};

/// Frame sync pattern dibit values, MSB-first (0x5575F5FF77FF, spec §4.2).
pub const SYNC_DIBITS: [u8; 24] = [
    1, 1, 1, 1, 1, 3, 1, 1, 3, 3, 1, 1, 3, 3, 3, 3, 1, 3, 1, 3, 3, 3, 3, 3,
];

/// Framer soft-sync gate threshold (spec §7.1).
pub const FRAMER_SYNC_THRESHOLD: f32 = 60.0;

/// Dibits ahead of a message body that belong to the frame preamble:
/// 24 sync dibits + 32 NID dibits (spec §7.4).
const SYNC_NID_DIBITS: usize = 56;

/// NID capture length: the 32 NID dibits plus the interstitial status dibit
/// that the transmitter inserts inside them (spec §7.1).
const DIBIT_LENGTH_NID: usize = 33;

/// Index of the status dibit inside the captured NID; skipped when the
/// 64-bit BCH codeword is rebuilt (spec §7.3).
const NID_STATUS_DIBIT_INDEX: usize = 11;

/// Free-counter value at which a message assembler starts: the 56 sync+NID
/// dibits plus the NID-internal status dibit (spec §7.3).
const ASSEMBLY_START_DIBIT: i64 = 57;

/// Status dibits recur as every 36th transmitted dibit (spec §7.2).
const STATUS_DIBIT_PERIOD: u32 = 36;

/// Status-counter value on the last data dibit before a status dibit. A
/// buffer that fills here is on cadence, so dispatch is delayed one dibit to
/// consume the trailing status dibit first (spec §7.3).
const STATUS_DIBIT_LAST_DATA: u32 = 35;

/// Status-counter restart value after a NID, so the cadence stays aligned
/// with the transmitter (spec §7.2, §17.6).
const POST_NID_STATUS_COUNTER: u32 = 21;

/// Free-running sync-loss interval: one second of dibits (spec §7.3).
const SYNC_LOSS_DIBITS: i64 = 4800;

/// Reject a BCH-decoded NID whose NAC disagrees with the established system
/// NAC once the decoder had to correct at least this many bits. BCH(63,16)
/// with t=11 accepts roughly 1% of random 63-bit words, and those accepts
/// cluster at 9–11 corrections with a nonsense NAC — exactly the profile of a
/// spurious mid-frame sync. A NID that agrees with the tracked NAC is
/// accepted at any correction count (spec §8.1, §17.8).
const NID_NAC_MISMATCH_CORRECTION_LIMIT: u32 = 9;

/// Ideal soft symbol for a sync dibit value (±3π/4).
#[inline]
fn sync_soft(v: u8) -> f32 {
    const P: f32 = 3.0 * std::f32::consts::FRAC_PI_4;
    if v == 1 {
        P
    } else {
        -P
    }
}

/// Data unit IDs, including internal continuation states (spec §7.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Duid {
    /// Header data unit.
    Hdu,
    /// Terminator data unit (no payload).
    Tdu,
    /// Logical link data unit 1.
    Ldu1,
    /// Trunking signal block, first block.
    Tsbk1,
    /// Logical link data unit 2.
    Ldu2,
    /// Packet data unit header.
    PduHeader,
    /// Terminator with link control.
    TduLc,
    /// Placeholder assembly (unknown DUID with tracked NAC).
    PlaceHolder,
    /// TSBK continuation (2 blocks).
    Tsbk2,
    /// TSBK continuation (3 blocks).
    Tsbk3,
    /// PDU continuation, block n (1..=5).
    PduBlock(u8),
}

impl Duid {
    /// Producible DUIDs from a NID value.
    fn from_nid(v: u8) -> Option<Duid> {
        match v {
            0 => Some(Duid::Hdu),
            3 => Some(Duid::Tdu),
            5 => Some(Duid::Ldu1),
            7 => Some(Duid::Tsbk1),
            10 => Some(Duid::Ldu2),
            12 => Some(Duid::PduHeader),
            15 => Some(Duid::TduLc),
            _ => None,
        }
    }

    /// The spec §7.4 length triple for this unit. Both the assembly buffer
    /// size and the elapsed-dibit charge are derived from it, so the table is
    /// stated exactly once.
    const fn lengths(self) -> UnitLengths {
        match self {
            Duid::Hdu => UnitLengths::new(648, 11, 10),
            Duid::Tdu => UnitLengths::new(0, 2, 28),
            Duid::Ldu1 | Duid::Ldu2 => UnitLengths::new(1568, 24, 0),
            Duid::Tsbk1 | Duid::PduHeader => UnitLengths::new(196, 5, 42),
            Duid::TduLc => UnitLengths::new(288, 6, 20),
            Duid::PlaceHolder => UnitLengths::new(1572, 24, 0),
            Duid::Tsbk2 => UnitLengths::new(392, 8, 56),
            Duid::Tsbk3 => UnitLengths::new(588, 10, 0),
            Duid::PduBlock(n) => match n {
                1 => UnitLengths::new(392, 8, 56),
                2 => UnitLengths::new(588, 10, 0),
                3 => UnitLengths::new(784, 13, 14),
                4 => UnitLengths::new(980, 16, 28),
                _ => UnitLengths::new(1176, 19, 42),
            },
        }
    }

    /// Assembly buffer size in bits (raw message + null pad, spec §7.4).
    fn buffer_bits(self) -> usize {
        self.lengths().buffer_bits()
    }

    /// Total elapsed dibits for a full frame of this unit (spec §7.4).
    /// Subtracted from the free dibit counter at dispatch so back-to-back
    /// frames account to zero sync loss (§17.5).
    fn elapsed_dibits(self) -> i64 {
        self.lengths().elapsed_dibits()
    }
}

/// The three transmitted lengths that define a data unit (spec §7.4): the
/// raw message bits, the status dibits interleaved into the frame, and the
/// trailing null-pad bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct UnitLengths {
    raw_bits: usize,
    status_dibits: usize,
    null_bits: usize,
}

impl UnitLengths {
    const fn new(raw_bits: usize, status_dibits: usize, null_bits: usize) -> Self {
        UnitLengths {
            raw_bits,
            status_dibits,
            null_bits,
        }
    }

    /// Assembly buffer size = raw message length + null pad.
    const fn buffer_bits(self) -> usize {
        self.raw_bits + self.null_bits
    }

    /// `elapsed = 56 + msgLen/2 + statusDibits + nullBits/2` (spec §7.4).
    const fn elapsed_dibits(self) -> i64 {
        (SYNC_NID_DIBITS + self.raw_bits / 2 + self.status_dibits + self.null_bits / 2) as i64
    }
}

/// One in-flight message assembly.
struct Assembler {
    duid: Duid,
    buf: BitBuf,
    target: usize,
    /// Buffer is full; dispatch is delayed one dibit so the trailing status
    /// dibit is consumed first (spec §7.3).
    pending: bool,
    /// TSBK blocks already processed.
    tsbk_blocks_done: u8,
    /// PDU header once decoded.
    pdu_header: Option<PduHeader>,
    /// Reassembled PDU message: 96-bit header + 64 pad bits + decoded blocks.
    pdu_message: BitBuf,
    pdu_blocks_done: u8,
    pdu_valid: bool,
    pdu_corrected: u32,
}

impl Assembler {
    fn new(duid: Duid) -> Self {
        Assembler {
            duid,
            buf: BitBuf::new(),
            target: duid.buffer_bits(),
            pending: false,
            tsbk_blocks_done: 0,
            pdu_header: None,
            pdu_message: BitBuf::new(),
            pdu_blocks_done: 0,
            pdu_valid: true,
            pdu_corrected: 0,
        }
    }
}

/// Aggregate decode statistics, exposed for the summary output.
#[derive(Debug, Clone)]
pub struct FramerStats {
    /// Frame syncs detected (rising-edge crossings of the soft threshold).
    pub sync_count: u64,
    /// NIDs with a valid BCH decode.
    pub nid_count: u64,
    /// Per-NAC counts (NAC → count), small fixed table.
    pub nac_histogram: Vec<(u16, u64)>,
    /// Valid TSBKs decoded.
    pub tsbk_count: u64,
    /// Per-opcode TSBK counts.
    pub tsbk_opcode_histogram: [u64; 64],
    /// Total corrected bits across all EDAC stages.
    pub corrected_bits: u64,
    /// CRC-16 blocks rejected because their residual was the complement of
    /// the one their data parity allows — the class the old dual-residual
    /// acceptance let through (see `edac::crc::crc16_expected_residual`).
    pub crc16_wrong_parity: u64,
}

impl Default for FramerStats {
    fn default() -> Self {
        FramerStats {
            sync_count: 0,
            nid_count: 0,
            nac_histogram: Vec::new(),
            tsbk_count: 0,
            tsbk_opcode_histogram: [0; 64],
            corrected_bits: 0,
            crc16_wrong_parity: 0,
        }
    }
}

impl FramerStats {
    fn count_nac(&mut self, nac: u16) {
        if let Some(e) = self.nac_histogram.iter_mut().find(|e| e.0 == nac) {
            e.1 += 1;
        } else if self.nac_histogram.len() < 64 {
            self.nac_histogram.push((nac, 1));
        }
    }
}

/// The P25 Phase 1 framer.
pub struct Framer {
    /// Soft symbol ring, written twice for wrap-free reads (spec §4.3).
    soft: [f32; 96],
    ptr: usize,
    prev_score: f32,

    dibit_counter: i64,
    status_counter: u32,

    nid_active: bool,
    nid_dibits: [Dibit; DIBIT_LENGTH_NID],
    nid_count: usize,

    assembler: Option<Assembler>,
    nac_tracker: NacTracker,
    current_nac: u16,

    symbol_count: u64,
    /// Statistics counters.
    pub stats: FramerStats,
}

impl Default for Framer {
    fn default() -> Self {
        Self::new()
    }
}

impl Framer {
    /// New framer with the spec's startup counter values (§7.1).
    pub fn new() -> Self {
        Framer {
            soft: [0.0; 96],
            ptr: 0,
            prev_score: 0.0,
            dibit_counter: ASSEMBLY_START_DIBIT + 1,
            status_counter: STATUS_DIBIT_PERIOD,
            nid_active: false,
            nid_dibits: [Dibit::D00Plus1; DIBIT_LENGTH_NID],
            nid_count: 0,
            assembler: None,
            nac_tracker: NacTracker::new(),
            current_nac: 0,
            symbol_count: 0,
            stats: FramerStats::default(),
        }
    }

    /// Clear every piece of framing state so decoding can restart cleanly —
    /// intended for a retune, where the dibit stream is discontinuous and any
    /// carried-over state would be wrong.
    ///
    /// Cleared: the soft sync correlator ring and its previous score, any
    /// message assembly in flight (including a delayed dispatch), the NID
    /// capture, the free dibit counter and status-dibit cadence (back to the
    /// spec's §7.1 startup values), the NAC tracker and the current NAC.
    ///
    /// Deliberately preserved: the aggregate [`FramerStats`] (they are
    /// lifetime totals for the summary output) and the symbol counter that
    /// drives the millisecond clock (it is a monotonic sample clock; winding
    /// it back would corrupt the NAC tracker's timestamp ordering).
    pub fn reset(&mut self) {
        self.soft = [0.0; 96];
        self.ptr = 0;
        self.prev_score = 0.0;
        self.dibit_counter = ASSEMBLY_START_DIBIT + 1;
        self.status_counter = STATUS_DIBIT_PERIOD;
        self.nid_active = false;
        self.nid_dibits = [Dibit::D00Plus1; DIBIT_LENGTH_NID];
        self.nid_count = 0;
        self.assembler = None;
        self.nac_tracker = NacTracker::new();
        self.current_nac = 0;
    }

    /// Millisecond clock derived from the symbol counter (4800 sym/s).
    fn now_ms(&self) -> u64 {
        self.symbol_count * 1000 / 4800
    }

    /// Feed one symbol (soft phase + sliced dibit). Returns `true` when this
    /// symbol completed a validated NID (for frequency-error feedback, §14).
    pub fn process(&mut self, soft: f32, dibit: Dibit, sink: &mut dyn FnMut(P25Event)) -> bool {
        self.symbol_count += 1;

        // --- Soft sync correlator (48-entry ring written twice). ---
        self.soft[self.ptr] = soft;
        self.soft[self.ptr + 48] = soft;
        self.ptr = (self.ptr + 1) % 48;
        // The last 24 symbols end at (ptr - 1); window starts 24 back.
        let start = self.ptr + 48 - 24;
        let mut score = 0.0f32;
        for (x, &d) in SYNC_DIBITS.iter().enumerate() {
            score += sync_soft(d) * self.soft[start + x];
        }
        let mut sync_just_detected = false;
        if score > FRAMER_SYNC_THRESHOLD {
            if self.prev_score <= FRAMER_SYNC_THRESHOLD {
                self.stats.sync_count += 1;
                sink(P25Event::SyncDetected { score });
            }
            // (Re)start NID capture: the *next* 33 dibits (the current dibit
            // completed the sync pattern and is not part of the NID).
            self.nid_active = true;
            self.nid_count = 0;
            sync_just_detected = true;
        }
        self.prev_score = score;

        // --- Counters and status-dibit cadence (§7.2). ---
        self.dibit_counter += 1;
        self.status_counter += 1;
        let is_status = self.status_counter >= STATUS_DIBIT_PERIOD;
        if is_status {
            self.status_counter = 0;
        }

        // Free-running sync loss (§7.3).
        if self.dibit_counter >= SYNC_LOSS_DIBITS {
            self.dibit_counter -= SYNC_LOSS_DIBITS;
            self.nac_tracker.reset();
            sink(P25Event::SyncLoss {
                bits: 2 * SYNC_LOSS_DIBITS,
            });
        }

        // --- NID capture (raw dibits, including the status dibit at 11). ---
        if self.nid_active && !sync_just_detected {
            self.nid_dibits[self.nid_count] = dibit;
            self.nid_count += 1;
            if self.nid_count == DIBIT_LENGTH_NID {
                self.nid_active = false;
                if self.check_nid(sink) {
                    // A real NID: this dibit belongs to it and nothing else.
                    return true;
                }
                // BCH failed, so this was not a NID at all — almost always a
                // spurious correlator hit inside a frame already being
                // assembled. Fall through and feed the dibit to that assembly
                // as ordinary payload; swallowing it here would shift the
                // remainder of the frame by two bits and corrupt it (§7.3).
            }
        }

        // --- Delayed dispatch: the trailing (status) dibit after a full
        // buffer has now been consumed. ---
        if self.assembler.as_ref().is_some_and(|a| a.pending) {
            self.on_buffer_full(sink);
        }

        // --- Assembler feed (status dibits are diverted, not appended). ---
        if !is_status {
            if let Some(a) = self.assembler.as_mut() {
                a.buf.push_dibit(dibit);
                if a.buf.len() >= a.target {
                    if self.status_counter == STATUS_DIBIT_LAST_DATA {
                        // Completion aligns with the cadence: delay one dibit
                        // so the trailing status dibit is consumed first.
                        a.pending = true;
                    } else {
                        self.on_buffer_full(sink);
                    }
                }
            } else if self.dibit_counter == ASSEMBLY_START_DIBIT && self.current_nac > 0 {
                // No assembly is required here, but a NAC has been detected,
                // so start a placeholder to keep the frame clock honest. It
                // starts at dibit 57 — never earlier — so a placeholder cycle
                // costs exactly the dibits it occupies (§7.3).
                self.assembler = Some(Assembler::new(Duid::PlaceHolder));
            }
        }
        false
    }

    /// Validate a captured 33-dibit NID (skip index 11) and, on success,
    /// dispatch/force-complete any active assembly and start the next one.
    fn check_nid(&mut self, sink: &mut dyn FnMut(P25Event)) -> bool {
        let mut word = 0u64;
        for (i, d) in self.nid_dibits.iter().enumerate() {
            if i == NID_STATUS_DIBIT_INDEX {
                continue;
            }
            word = (word << 2) | u64::from(d.value());
        }

        let tracked = self.nac_tracker.tracked();
        let Some(nid) = decode_nid(word, tracked) else {
            return false; // stay in placeholder mode
        };

        // NAC gate: once the system NAC is established, a heavily corrected
        // codeword that resolves to a *different* NAC is a BCH false accept,
        // not a frame. Treat it exactly like a decode failure — stay in
        // placeholder mode and leave the assembly in flight alone (§8.1).
        //
        // The tracker is reset by every one-second sync-loss report (§17.9),
        // so on a traffic channel it spends the gaps between transmissions
        // unestablished. Fall back to the last NAC that actually produced a
        // frame, which survives those resets and is cleared only by `reset`.
        let system_nac = tracked.or((self.current_nac > 0).then_some(self.current_nac));
        if let Some(system_nac) = system_nac {
            if nid.nac != system_nac && nid.corrected >= NID_NAC_MISMATCH_CORRECTION_LIMIT {
                return false;
            }
        }

        let now = self.now_ms();
        self.nac_tracker.observe(nid.nac, now);
        self.stats.nid_count += 1;
        self.stats.count_nac(nid.nac);
        self.stats.corrected_bits += u64::from(nid.corrected);
        sink(P25Event::NidDecoded {
            nac: nid.nac,
            duid: nid.duid,
            corrected: nid.corrected,
        });

        let duid = match Duid::from_nid(nid.duid) {
            Some(d) => {
                // NAC 0 is not a system identity (the reference treats it as
                // "no NAC yet"), so it never replaces an established one.
                if nid.nac > 0 {
                    self.current_nac = nid.nac;
                }
                Some(d)
            }
            // Unknown DUID → placeholder, keep the previous NAC (§17.8).
            None => None,
        };

        // Dispatch or force-complete a still-active assembler.
        if let Some(a) = self.assembler.take() {
            if a.buf.len() >= a.target {
                // Complete, but the delayed dispatch had not fired yet. Run
                // the normal completion path so multi-block continuations
                // (TSBK 2/3, PDU blocks) are actually decoded and the elapsed
                // dibits are accounted; a bare `dispatch` would drop them.
                self.assembler = Some(a);
                self.on_buffer_full(sink);
                // Any continuation state the completion left behind is
                // superseded by the frame this NID introduces.
                self.assembler = None;
            } else {
                self.force_complete(a, sink);
            }
        }

        // Sync-loss accounting (§7.3).
        self.dibit_counter -= ASSEMBLY_START_DIBIT;
        if self.dibit_counter != 0 {
            sink(P25Event::SyncLoss {
                bits: 2 * self.dibit_counter,
            });
        }
        self.dibit_counter = ASSEMBLY_START_DIBIT;
        self.status_counter = POST_NID_STATUS_COUNTER;

        // Require assembly: explicit for a known DUID, placeholder otherwise
        // when a NAC has been detected.
        self.assembler = match duid {
            Some(d) => Some(Assembler::new(d)),
            None => (self.current_nac > 0).then(|| Assembler::new(Duid::PlaceHolder)),
        };
        true
    }

    /// Buffer reached its target: process per DUID, possibly reconfiguring
    /// the live assembler for continuation blocks.
    fn on_buffer_full(&mut self, sink: &mut dyn FnMut(P25Event)) {
        let Some(mut a) = self.assembler.take() else {
            return;
        };
        a.pending = false;
        loop {
            match a.duid {
                Duid::Tsbk1 | Duid::Tsbk2 | Duid::Tsbk3 => {
                    let block = usize::from(a.tsbk_blocks_done);
                    let (last, _valid) = self.process_tsbk_block(&a.buf, 196 * block, sink);
                    a.tsbk_blocks_done += 1;
                    if last || a.tsbk_blocks_done >= 3 {
                        self.finish(a.duid);
                        return;
                    }
                    // Reconfigure to the next continuation state
                    // (forced-completion guard: an overfull buffer processes
                    // the next block immediately via the loop).
                    a.duid = if a.tsbk_blocks_done == 1 {
                        Duid::Tsbk2
                    } else {
                        Duid::Tsbk3
                    };
                    a.target = a.duid.buffer_bits();
                    if a.buf.len() < 196 * usize::from(a.tsbk_blocks_done + 1) {
                        self.assembler = Some(a);
                        return;
                    }
                }
                Duid::PduHeader => {
                    match self.process_pdu_header(&mut a, sink) {
                        // Continue into data blocks.
                        Some(blocks) if blocks > 0 => {
                            a.duid = Duid::PduBlock(1);
                            a.target = a.duid.buffer_bits();
                            if a.buf.len() < a.target {
                                self.assembler = Some(a);
                                return;
                            }
                        }
                        _ => {
                            self.finish(Duid::PduHeader);
                            return;
                        }
                    }
                }
                Duid::PduBlock(n) => {
                    self.process_pdu_block(&mut a, n, sink);
                    let blocks_to_follow = a.pdu_header.map(|h| h.blocks_to_follow).unwrap_or(0);
                    if n >= blocks_to_follow.min(5) || n >= 5 {
                        self.dispatch_pdu(&a, sink);
                        self.finish(a.duid);
                        return;
                    }
                    a.duid = Duid::PduBlock(n + 1);
                    a.target = a.duid.buffer_bits();
                    if a.buf.len() < a.target {
                        self.assembler = Some(a);
                        return;
                    }
                }
                _ => {
                    let duid = a.duid;
                    self.dispatch(a, sink);
                    self.finish(duid);
                    return;
                }
            }
        }
    }

    /// Terminal completion: charge the frame's elapsed dibits against the
    /// free counter (§17.5).
    ///
    /// A placeholder is *not* charged: it is filler for dibits that carried no
    /// frame, so leaving the counter running is what lets it reach the
    /// one-second free-running sync-loss report (and the NAC-tracker reset)
    /// during a real signal gap, matching the reference. Charging it, and
    /// restarting the next placeholder here instead of at dibit 57, is what
    /// drove the counter — and the reported sync loss — negative.
    fn finish(&mut self, duid: Duid) {
        if duid != Duid::PlaceHolder {
            self.dibit_counter -= duid.elapsed_dibits();
        }
    }

    /// Process a 196-bit TSBK block: deinterleave → 1/2-rate trellis →
    /// CRC-CCITT-16 → parse (spec §7.5). Returns (last_block, valid).
    fn process_tsbk_block(
        &mut self,
        buf: &BitBuf,
        start: usize,
        sink: &mut dyn FnMut(P25Event),
    ) -> (bool, bool) {
        let d = deinterleave(buf, start);
        let mut out = BitBuf::new();
        let tr = trellis::decode_1_2(&d, 0, &mut out);
        let crc = crc16_check_and_correct(&mut out, 0);
        if crc == CrcResult::FailedWrongParity {
            self.stats.crc16_wrong_parity += 1;
        }
        if crc.is_valid() {
            let tsbk = parse_tsbk(&out, 0);
            let crc_corrected = crc.corrected_bits();
            let corrected = tr.path_error + crc_corrected;
            self.stats.tsbk_count += 1;
            self.stats.tsbk_opcode_histogram[usize::from(tsbk.opcode & 0x3F)] += 1;
            self.stats.corrected_bits += u64::from(corrected);
            let last = tsbk.last_block;
            sink(P25Event::Tsbk {
                tsbk,
                corrected,
                crc_corrected,
            });
            (last, true)
        } else {
            sink(P25Event::TsbkFailed);
            (false, false)
        }
    }

    /// Process the PDU header block; returns Some(blocks_to_follow) when
    /// valid, None when invalid (spec §7.6).
    fn process_pdu_header(
        &mut self,
        a: &mut Assembler,
        sink: &mut dyn FnMut(P25Event),
    ) -> Option<u8> {
        let d = deinterleave(&a.buf, 0);
        let mut out = BitBuf::new();
        let tr = trellis::decode_1_2(&d, 0, &mut out);
        let crc = crc16_check_and_correct(&mut out, 0);
        if crc == CrcResult::FailedWrongParity {
            self.stats.crc16_wrong_parity += 1;
        }
        if !crc.is_valid() {
            return None;
        }
        let header = parse_pdu_header(&out, 0);
        let corrected = tr.path_error + crc.corrected_bits();
        self.stats.corrected_bits += u64::from(corrected);
        sink(P25Event::PduHeader { header, corrected });
        a.pdu_header = Some(header);
        a.pdu_corrected = corrected;
        // Reassembled message: 96 decoded header bits + 64 pad bits; the CRC
        // covered region starts at bit 160 (spec §8.4c).
        for i in 0..96 {
            a.pdu_message.push(out.get(i));
        }
        for _ in 0..64 {
            a.pdu_message.push(false);
        }
        Some(header.blocks_to_follow)
    }

    /// Process PDU data block `n` (1-based) at buffer bits [196n, 196(n+1)).
    fn process_pdu_block(&mut self, a: &mut Assembler, n: u8, _sink: &mut dyn FnMut(P25Event)) {
        let confirmed = a.pdu_header.map(|h| h.confirmed).unwrap_or(false);
        let d = deinterleave(&a.buf, 196 * usize::from(n));
        let mut out = BitBuf::new();
        if confirmed {
            // 3/4-rate → 144 bits; CRC-9 over the block; payload [16,144).
            let tr = trellis::decode_3_4(&d, 0, &mut out);
            a.pdu_corrected += tr.path_error;
            self.stats.corrected_bits += u64::from(tr.path_error);
            if !crc9_check(&out, 0).is_valid() {
                a.pdu_valid = false;
            }
            for i in 16..144 {
                a.pdu_message.push(out.get(i));
            }
        } else {
            // 1/2-rate → 96 bits, all payload; no block CRC.
            let tr = trellis::decode_1_2(&d, 0, &mut out);
            a.pdu_corrected += tr.path_error;
            self.stats.corrected_bits += u64::from(tr.path_error);
            for i in 0..96 {
                a.pdu_message.push(out.get(i));
            }
        }
        a.pdu_blocks_done = n;
    }

    /// Emit the completed PDU (with CRC-32 over unconfirmed multi-block
    /// payloads where a table exists).
    fn dispatch_pdu(&mut self, a: &Assembler, sink: &mut dyn FnMut(P25Event)) {
        let Some(header) = a.pdu_header else { return };
        // A header that promised N blocks but was cut short by the next sync
        // is not a valid PDU, however clean the blocks we did get were.
        let mut valid = a.pdu_valid && a.pdu_blocks_done >= header.blocks_to_follow;
        let mut corrected = a.pdu_corrected;
        let blocks = usize::from(a.pdu_blocks_done);
        if !header.confirmed && (1..=3).contains(&blocks) {
            let mut msg = a.pdu_message.clone();
            let crc = crc32_pdu_check_and_correct(&mut msg, blocks);
            valid &= crc.is_valid();
            corrected += crc.corrected_bits();
            self.stats.corrected_bits += u64::from(crc.corrected_bits());
        }
        // Carry the reassembled user-data octets out: the block payloads live
        // after the 96-bit header and the 64 pad bits (bit 160 onward). This is
        // where an SNDCP data PDU's IP datagram lives, for LRRP inspection.
        let payload = if a.pdu_message.len() > 160 {
            let bits = a.pdu_message.len() - 160;
            let mut bytes = vec![0u8; bits.div_ceil(8)];
            a.pdu_message.copy_bits_to_bytes(160, bits, &mut bytes);
            bytes
        } else {
            Vec::new()
        };
        sink(P25Event::Pdu {
            header,
            blocks: a.pdu_blocks_done,
            valid,
            corrected,
            payload,
        });
    }

    /// Dispatch a completed simple (non-multi-block) assembly.
    fn dispatch(&mut self, a: Assembler, sink: &mut dyn FnMut(P25Event)) {
        match a.duid {
            Duid::Hdu => frames::process_hdu(&a.buf, sink),
            Duid::Tdu => sink(P25Event::Tdu),
            Duid::TduLc => frames::process_tdulc(&a.buf, sink),
            Duid::Ldu1 => frames::process_ldu(&a.buf, LduKind::Ldu1, sink),
            Duid::Ldu2 => frames::process_ldu(&a.buf, LduKind::Ldu2, sink),
            Duid::PlaceHolder => {} // discarded
            // Multi-block units are processed incrementally in
            // on_buffer_full; reaching here means a force-complete path
            // already handled them.
            _ => {}
        }
    }

    /// Force-complete an incomplete assembly (a new NID arrived): process
    /// any complete 196-bit sub-blocks; discard partial voice/header units.
    fn force_complete(&mut self, mut a: Assembler, sink: &mut dyn FnMut(P25Event)) {
        match a.duid {
            Duid::Tsbk1 | Duid::Tsbk2 | Duid::Tsbk3 => {
                while a.buf.len() >= 196 * (usize::from(a.tsbk_blocks_done) + 1)
                    && a.tsbk_blocks_done < 3
                {
                    let block = usize::from(a.tsbk_blocks_done);
                    let (last, _) = self.process_tsbk_block(&a.buf, 196 * block, sink);
                    a.tsbk_blocks_done += 1;
                    if last {
                        break;
                    }
                }
            }
            Duid::PduHeader => {} // partial header: nothing decodable
            Duid::PduBlock(_) => {
                // Header (and possibly some blocks) were decoded; emit what
                // we have.
                self.dispatch_pdu(&a, sink);
            }
            _ => {} // partial HDU/TDU/LDU/TDULC/placeholder: discard
        }

        // Dropped-bit accounting (§7.3): the bits this frame still needed and
        // never received. A placeholder is exempt — it stands in for dibits
        // that were never claimed by a frame, so nothing was lost.
        let dropped = a.target.saturating_sub(a.buf.len());
        if a.duid != Duid::PlaceHolder && dropped > 0 {
            sink(P25Event::SyncLoss {
                bits: dropped as i64,
            });
        }

        // Charge the elapsed dibits, capped at the time actually observed
        // since this frame's NID. The reference instead downgrades the unit
        // to one whose length fits the dibits collected; capping reaches the
        // same place — the interrupted frame is charged for exactly the air
        // time it occupied — without guessing at a DUID.
        let observed = (self.dibit_counter - ASSEMBLY_START_DIBIT).max(0);
        self.dibit_counter -= a.duid.elapsed_dibits().min(observed);
    }

    /// The most recently confirmed NAC.
    pub fn current_nac(&self) -> u16 {
        self.current_nac
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edac::bch::encode_nid;
    use crate::edac::crc::CRC16_TABLE;
    use crate::edac::golay::{golay18_encode, golay24_encode};
    use crate::edac::hamming::hamming10_encode;
    use crate::edac::{lsd, rs};
    use crate::frames::{IMBE_OFFSETS, LDU_HAMMING_OFFSETS};
    use crate::interleave::interleave;

    /// Feed a dibit stream through the framer with ideal soft symbols.
    fn feed(f: &mut Framer, dibits: &[Dibit], events: &mut Vec<P25Event>) {
        for &d in dibits {
            f.process(d.ideal_phase(), d, &mut |e| events.push(e));
        }
    }

    fn sync_dibits() -> Vec<Dibit> {
        SYNC_DIBITS.iter().map(|&v| Dibit::from_value(v)).collect()
    }

    /// Dibits (MSB-first pairs) for a 64-bit word.
    fn word_dibits(w: u64, bits: usize) -> Vec<Dibit> {
        (0..bits / 2)
            .map(|i| Dibit::from_value(((w >> (bits - 2 - 2 * i)) & 3) as u8))
            .collect()
    }

    /// Fill in the CCITT-16 field at bits [80,96) of a 96-bit block so the
    /// decoder's residual cancels (the identity-row trick of §17.1).
    fn seal_crc16(m: &mut BitBuf) {
        let mut acc = 0u16;
        let mut ones = 0u32;
        for (i, &row) in CRC16_TABLE.iter().enumerate().take(80) {
            if m.get(i) {
                acc ^= row;
                ones += 1;
            }
        }
        // The residual a correct codeword lands on depends on the data-bit
        // parity (see `edac::crc::crc16_expected_residual`).
        let field = acc ^ crate::edac::crc::crc16_expected_residual(ones);
        for k in 0..16 {
            m.set(80 + k, (field >> (15 - k)) & 1 != 0);
        }
    }

    /// Build a valid 96-bit TSBK (opcode 0 grant), trellis-encode and
    /// interleave to 196 bits.
    fn tsbk_block_bits() -> BitBuf {
        let mut m = BitBuf::new();
        for _ in 0..96 {
            m.push(false);
        }
        m.set(0, true); // last block
        m.set_u64(2, 6, 0); // opcode 0
        m.set_u64(24, 4, 0x1);
        m.set_u64(28, 12, 100);
        m.set_u64(40, 16, 12345);
        m.set_u64(56, 24, 0xCAFE01);
        seal_crc16(&mut m);
        let coded = trellis::encode_block_1_2(&m);
        interleave(&coded)
    }

    /// Build a valid 196-bit PDU header block announcing `blocks_to_follow`
    /// unconfirmed data blocks (spec §7.6).
    fn pdu_header_block_bits(blocks_to_follow: u8) -> BitBuf {
        let mut m = BitBuf::new();
        for _ in 0..96 {
            m.push(false);
        }
        m.set(2, true); // outbound
        m.set_u64(3, 5, 0x15); // format
        m.set_u64(16, 8, 0); // standard vendor
        m.set_u64(24, 24, 0xABCDEF); // logical link ID
        m.set_u64(49, 7, u64::from(blocks_to_follow));
        seal_crc16(&mut m);
        let coded = trellis::encode_block_1_2(&m);
        interleave(&coded)
    }

    /// Dibits for the bits of `buf`, MSB-first pairs.
    fn bit_dibits(buf: &BitBuf) -> Vec<Dibit> {
        (0..buf.len() / 2)
            .map(|i| {
                Dibit::from_value((u8::from(buf.get(2 * i)) << 1) | u8::from(buf.get(2 * i + 1)))
            })
            .collect()
    }

    /// Insert a status dibit as every 36th transmitted dibit (the
    /// transmitter-side cadence of §7.2).
    fn insert_status_dibits(payload: Vec<Dibit>) -> Vec<Dibit> {
        let mut out = Vec::new();
        let mut count = 0u32;
        for d in payload {
            out.push(d);
            count += 1;
            if count % 36 == 35 {
                out.push(Dibit::D00Plus1); // status dibit
                count += 1;
            }
        }
        out
    }

    /// Transmitted index of payload dibit `p` — one status dibit is inserted
    /// per 35 payload dibits.
    fn tx_index(p: usize) -> usize {
        p + p / 35
    }

    /// Assemble a full frame dibit stream: sync + NID (with the status dibit
    /// landing at NID index 11) + message body + null pad, with status dibits
    /// on the transmitter cadence.
    fn frame_dibits(nac: u16, duid: u8, body: &BitBuf, null_dibits: usize) -> Vec<Dibit> {
        let mut payload: Vec<Dibit> = Vec::new();
        payload.extend(sync_dibits());
        payload.extend(word_dibits(encode_nid(nac, duid), 64));
        payload.extend(bit_dibits(body));
        for _ in 0..null_dibits {
            payload.push(Dibit::D00Plus1);
        }
        insert_status_dibits(payload)
    }

    /// A full TSBK frame (DUID 7): one block plus the 42-bit null pad.
    fn tsbk_frame(nac: u16) -> Vec<Dibit> {
        frame_dibits(nac, 7, &tsbk_block_bits(), 21)
    }

    /// Deterministic, non-trivial IMBE bit pattern for voice frame `frame`.
    fn imbe_bit(frame: usize, k: usize) -> bool {
        (frame * 13 + k * 7) % 11 < 5
    }

    /// The 18 bytes `process_ldu` must hand back for voice frame `frame`.
    fn expected_imbe(frame: usize) -> [u8; 18] {
        let mut out = [0u8; 18];
        for k in 0..144 {
            if imbe_bit(frame, k) {
                out[k / 8] |= 1 << (7 - k % 8);
            }
        }
        out
    }

    /// Build an RS codeword for the LC packing and return its 24 hexes
    /// (12 data then 12 parity) for a 72-bit LC word.
    fn encode_lc(word: u128) -> [u8; 24] {
        let mut rs_in = [0u8; 63];
        for j in 0..12 {
            rs_in[12 + j] = ((word >> (66 - 6 * (11 - j))) & 0x3F) as u8;
        }
        rs::encode(&mut rs_in, 12);
        let mut hexes = [0u8; 24];
        for i in 0..12 {
            hexes[11 - i] = rs_in[12 + i]; // data
            hexes[12 + (11 - i)] = rs_in[i]; // parity
        }
        hexes
    }

    /// The 9 bytes of a 72-bit LC word, as the decoder reports them.
    fn lc_bytes(word: u128) -> [u8; 9] {
        let mut out = [0u8; 9];
        for (k, b) in out.iter_mut().enumerate() {
            *b = ((word >> (8 * (8 - k))) & 0xFF) as u8;
        }
        out
    }

    /// A complete 1568-bit LDU1: nine patterned IMBE frames, the link control
    /// word spread over 24 Hamming(10,6) words, and two LSD octets.
    fn ldu1_body(lc: u128, low_speed: [u8; 2]) -> BitBuf {
        let mut buf = BitBuf::new();
        for _ in 0..1568 {
            buf.push(false);
        }
        for (f, &off) in IMBE_OFFSETS.iter().enumerate() {
            for k in 0..144 {
                buf.set(off + k, imbe_bit(f, k));
            }
        }
        let hexes = encode_lc(lc);
        for (k, &off) in LDU_HAMMING_OFFSETS.iter().enumerate() {
            let cw = hamming10_encode(hexes[k]);
            for b in 0..10 {
                buf.set(off + b, (cw >> (9 - b)) & 1 != 0);
            }
        }
        for (o, &octet) in low_speed.iter().enumerate() {
            let cw = lsd::encode(octet);
            for b in 0..16 {
                buf.set(1392 + 16 * o + b, (cw >> (15 - b)) & 1 != 0);
            }
        }
        buf
    }

    /// Voice frames extracted from an LDU event stream, in order.
    fn imbe_frames(events: &[P25Event]) -> Vec<[u8; 18]> {
        events
            .iter()
            .filter_map(|e| match e {
                P25Event::ImbeFrame { frame, .. } => Some(*frame),
                _ => None,
            })
            .collect()
    }

    /// The link-control word from an LDU1/TDULC event stream.
    fn link_control(events: &[P25Event]) -> Option<[u8; 9]> {
        events.iter().find_map(|e| match e {
            P25Event::LinkControl { word, .. } => Some(*word),
            _ => None,
        })
    }

    /// Every sync-loss report in an event stream.
    fn sync_losses(events: &[P25Event]) -> Vec<i64> {
        events
            .iter()
            .filter_map(|e| match e {
                P25Event::SyncLoss { bits } => Some(*bits),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn status_dibit_cadence_and_tsbk_decode() {
        let mut f = Framer::new();
        let mut events = Vec::new();
        // Idle preamble to settle counters.
        feed(&mut f, &[Dibit::D00Plus1; 50], &mut events);
        for _ in 0..2 {
            let frame = tsbk_frame(0x293);
            feed(&mut f, &frame, &mut events);
        }
        // Trailing dibits so the last frame's assembler can complete.
        feed(&mut f, &[Dibit::D00Plus1; 200], &mut events);

        assert!(f.stats.sync_count >= 2, "syncs: {}", f.stats.sync_count);
        assert_eq!(f.stats.nid_count, 2);
        assert_eq!(f.stats.nac_histogram, vec![(0x293, 2)]);
        assert_eq!(f.stats.tsbk_count, 2, "events: {}", events.len());
        assert_eq!(f.stats.tsbk_opcode_histogram[0], 2);
        let grants: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                P25Event::Tsbk { tsbk, .. } => Some(tsbk.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(grants.len(), 2);
        match &grants[0].message {
            crate::message::TsbkMessage::GroupVoiceGrant {
                channel,
                talkgroup,
                source,
                ..
            } => {
                assert_eq!(channel.channel, 100);
                assert_eq!(*talkgroup, 12345);
                assert_eq!(*source, 0xCAFE01);
            }
            m => panic!("{m:?}"),
        }
    }

    #[test]
    fn misaligned_status_dibits_break_decode() {
        // Sanity check that the cadence logic matters: shift the status
        // insertion by one and the TSBK must fail.
        let mut f = Framer::new();
        let mut events = Vec::new();
        let nid = encode_nid(0x293, 7);
        let mut payload: Vec<Dibit> = Vec::new();
        payload.extend(sync_dibits());
        payload.extend(word_dibits(nid, 64));
        let block = tsbk_block_bits();
        for i in 0..98 {
            payload.push(Dibit::from_value(
                (u8::from(block.get(2 * i)) << 1) | u8::from(block.get(2 * i + 1)),
            ));
        }
        let mut out = Vec::new();
        let mut count = 0u32;
        for d in payload {
            out.push(d);
            count += 1;
            if count % 36 == 20 {
                // wrong cadence
                out.push(Dibit::D11Minus3);
                count += 1;
            }
        }
        feed(&mut f, &[Dibit::D00Plus1; 50], &mut events);
        feed(&mut f, &out, &mut events);
        feed(&mut f, &[Dibit::D00Plus1; 300], &mut events);
        assert_eq!(f.stats.tsbk_count, 0);
    }

    #[test]
    fn duid_lengths_match_the_spec_table() {
        // The hand-typed §7.4 columns, kept here so the derived accessors can
        // never drift from the table they came from.
        let table: [(Duid, usize, i64); 15] = [
            (Duid::Hdu, 658, 396),
            (Duid::Tdu, 28, 72),
            (Duid::Ldu1, 1568, 864),
            (Duid::Ldu2, 1568, 864),
            (Duid::Tsbk1, 238, 180),
            (Duid::PduHeader, 238, 180),
            (Duid::TduLc, 308, 216),
            (Duid::PlaceHolder, 1572, 866),
            (Duid::Tsbk2, 448, 288),
            (Duid::Tsbk3, 588, 360),
            (Duid::PduBlock(1), 448, 288),
            (Duid::PduBlock(2), 588, 360),
            (Duid::PduBlock(3), 798, 468),
            (Duid::PduBlock(4), 1008, 576),
            (Duid::PduBlock(5), 1218, 684),
        ];
        for (duid, buffer_bits, elapsed) in table {
            assert_eq!(duid.buffer_bits(), buffer_bits, "buffer bits {duid:?}");
            assert_eq!(duid.elapsed_dibits(), elapsed, "elapsed dibits {duid:?}");
        }
    }

    #[test]
    fn ldu1_round_trip_through_the_framer() {
        let lc: u128 = 0x0300_0056_4321_ABCD;
        let low_speed = [0x5A, 0xC3];
        let body = ldu1_body(lc, low_speed);
        let mut f = Framer::new();
        let mut events = Vec::new();
        feed(&mut f, &[Dibit::D00Plus1; 50], &mut events);
        feed(&mut f, &frame_dibits(0x293, 5, &body, 0), &mut events);

        assert_eq!(f.stats.nid_count, 1);
        assert!(events.iter().any(|e| matches!(
            e,
            P25Event::NidDecoded {
                nac: 0x293,
                duid: 5,
                ..
            }
        )));
        assert_eq!(link_control(&events), Some(lc_bytes(lc)));

        let frames = imbe_frames(&events);
        assert_eq!(frames.len(), 9);
        for (i, frame) in frames.iter().enumerate() {
            assert_eq!(*frame, expected_imbe(i), "voice frame {i}");
        }

        match events
            .iter()
            .find(|e| matches!(e, P25Event::LowSpeedData { .. }))
        {
            Some(P25Event::LowSpeedData {
                data,
                errors,
                valid,
            }) => {
                assert_eq!(*data, low_speed);
                assert_eq!(*errors, [0, 0]);
                assert_eq!(*valid, [true, true]);
            }
            e => panic!("{e:?}"),
        }
    }

    #[test]
    fn spurious_mid_ldu_sync_does_not_shift_the_frame() {
        let lc: u128 = 0x0300_0056_4321_ABCD;
        let low_speed = [0x5A, 0xC3];
        let body = ldu1_body(lc, low_speed);
        let mut stream = frame_dibits(0x293, 5, &body, 0);

        // Plant the 48-bit sync pattern inside voice frame 2, in a window that
        // carries no status dibit so the correlator sees it whole. The 33
        // dibits after it are ordinary voice bits, so the NID they form fails
        // BCH and the whole thing must stay payload.
        let first = tx_index(SYNC_NID_DIBITS + IMBE_OFFSETS[2] / 2);
        let last = tx_index(SYNC_NID_DIBITS + IMBE_OFFSETS[3] / 2);
        let start = (first..last - SYNC_DIBITS.len())
            .find(|&t| (t..t + SYNC_DIBITS.len()).all(|x| x % 36 != 35))
            .expect("status-free window inside voice frame 2");
        for (k, &v) in SYNC_DIBITS.iter().enumerate() {
            stream[start + k] = Dibit::from_value(v);
        }

        let mut f = Framer::new();
        let mut events = Vec::new();
        feed(&mut f, &[Dibit::D00Plus1; 50], &mut events);
        feed(&mut f, &stream, &mut events);

        // The correlator fired twice; only the real NID passed BCH.
        assert!(f.stats.sync_count >= 2, "syncs: {}", f.stats.sync_count);
        assert_eq!(f.stats.nid_count, 1);

        // Everything after the false sync must still line up — swallowing the
        // NID-completing dibit here shifts the rest of the frame by two bits.
        assert_eq!(link_control(&events), Some(lc_bytes(lc)));
        let frames = imbe_frames(&events);
        assert_eq!(frames.len(), 9);
        for i in [0usize, 1, 3, 4, 5, 6, 7, 8] {
            assert_eq!(frames[i], expected_imbe(i), "voice frame {i}");
        }
        match events
            .iter()
            .find(|e| matches!(e, P25Event::LowSpeedData { .. }))
        {
            Some(P25Event::LowSpeedData { data, valid, .. }) => {
                assert_eq!(*data, low_speed);
                assert_eq!(*valid, [true, true]);
            }
            e => panic!("{e:?}"),
        }
    }

    #[test]
    fn idle_gap_reports_positive_sync_loss() {
        let mut f = Framer::new();
        let mut events = Vec::new();
        feed(&mut f, &[Dibit::D00Plus1; 50], &mut events);
        feed(&mut f, &tsbk_frame(0x293), &mut events);
        events.clear();
        // A gap longer than the one-second free-running report.
        feed(&mut f, &[Dibit::D00Plus1; 5000], &mut events);
        feed(&mut f, &tsbk_frame(0x293), &mut events);

        let losses = sync_losses(&events);
        assert!(!losses.is_empty(), "no sync loss reported over the gap");
        assert!(
            losses.iter().all(|&b| b > 0),
            "negative sync loss: {losses:?}"
        );
        assert!(losses.contains(&9600), "no 1-second report: {losses:?}");
        // The frame after the gap still decodes.
        assert_eq!(f.stats.tsbk_count, 2);
    }

    #[test]
    fn truncated_pdu_is_reported_invalid() {
        let header = pdu_header_block_bits(2);
        let mut f = Framer::new();
        let mut events = Vec::new();
        feed(&mut f, &[Dibit::D00Plus1; 50], &mut events);
        // Header plus its 42-bit pad, then a new frame: the two announced
        // data blocks never arrive.
        feed(&mut f, &frame_dibits(0x293, 12, &header, 21), &mut events);
        feed(&mut f, &tsbk_frame(0x293), &mut events);

        assert!(events.iter().any(|e| matches!(
            e,
            P25Event::PduHeader {
                header: PduHeader {
                    blocks_to_follow: 2,
                    ..
                },
                ..
            }
        )));
        match events.iter().find(|e| matches!(e, P25Event::Pdu { .. })) {
            Some(P25Event::Pdu { blocks, valid, .. }) => {
                assert_eq!(*blocks, 0);
                assert!(!valid, "a PDU cut short must not be reported valid");
            }
            e => panic!("{e:?}"),
        }
    }

    #[test]
    fn hdu_round_trip_through_the_framer() {
        let data_hexes: [u8; 20] = [
            1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20,
        ];
        let mut rs_in = [0u8; 63];
        for i in 0..20 {
            rs_in[16 + i] = data_hexes[19 - i];
        }
        rs::encode(&mut rs_in, 16);
        let mut hexes = [0u8; 36];
        for i in 0..20 {
            hexes[19 - i] = rs_in[16 + i];
        }
        for i in 0..16 {
            hexes[20 + (15 - i)] = rs_in[i];
        }
        let mut body = BitBuf::new();
        for &h in hexes.iter() {
            let cw = golay18_encode(h);
            for b in (0..18).rev() {
                body.push((cw >> b) & 1 != 0);
            }
        }

        let mut f = Framer::new();
        let mut events = Vec::new();
        feed(&mut f, &[Dibit::D00Plus1; 50], &mut events);
        feed(&mut f, &frame_dibits(0x293, 0, &body, 5), &mut events);

        assert_eq!(f.stats.nid_count, 1);
        match events.iter().find(|e| matches!(e, P25Event::Hdu { .. })) {
            Some(P25Event::Hdu { valid, data, .. }) => {
                assert!(valid);
                assert_eq!(data[0] >> 2, 1);
            }
            e => panic!("{e:?}"),
        }
    }

    #[test]
    fn tdulc_round_trip_through_the_framer() {
        let lc: u128 = 0x1234_89AB_CD55_0123;
        let hexes = encode_lc(lc);
        let mut body = BitBuf::new();
        for w in 0..12 {
            let data12 = (u16::from(hexes[2 * w]) << 6) | u16::from(hexes[2 * w + 1]);
            let cw = golay24_encode(data12);
            for b in (0..24).rev() {
                body.push((cw >> b) & 1 != 0);
            }
        }

        let mut f = Framer::new();
        let mut events = Vec::new();
        feed(&mut f, &[Dibit::D00Plus1; 50], &mut events);
        feed(&mut f, &frame_dibits(0x293, 15, &body, 10), &mut events);

        assert_eq!(f.stats.nid_count, 1);
        match events.iter().find(|e| matches!(e, P25Event::TduLc { .. })) {
            Some(P25Event::TduLc {
                valid,
                link_control,
                ..
            }) => {
                assert!(valid);
                assert_eq!(*link_control, lc_bytes(lc));
            }
            e => panic!("{e:?}"),
        }
    }

    #[test]
    fn reset_clears_framing_state() {
        let mut f = Framer::new();
        let mut events = Vec::new();
        feed(&mut f, &[Dibit::D00Plus1; 50], &mut events);
        // Stop part-way into a frame so an assembly is in flight.
        let frame = tsbk_frame(0x293);
        feed(&mut f, &frame[..100], &mut events);
        assert!(f.assembler.is_some());
        assert_eq!(f.current_nac(), 0x293);

        f.reset();
        assert!(f.assembler.is_none());
        assert!(!f.nid_active);
        assert_eq!(f.nid_count, 0);
        assert_eq!(f.current_nac(), 0);
        assert_eq!(f.dibit_counter, ASSEMBLY_START_DIBIT + 1);
        assert_eq!(f.status_counter, STATUS_DIBIT_PERIOD);
        assert_eq!(f.nac_tracker.tracked(), None);
        assert_eq!(f.prev_score, 0.0);

        // Decoding works normally after the reset.
        let before = f.stats.tsbk_count;
        feed(&mut f, &[Dibit::D00Plus1; 50], &mut events);
        feed(&mut f, &tsbk_frame(0x293), &mut events);
        assert_eq!(f.stats.tsbk_count, before + 1);
    }
}
