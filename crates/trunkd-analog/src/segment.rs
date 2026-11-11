// SPDX-License-Identifier: GPL-3.0-or-later
//! Call segmentation: squelch + tone state into `ChannelEvent`s, holding audio
//! through the tone-confirm window.
//!
//! [`CallSegmenter::block`] runs once per squelch block ([`BLOCK_MS`] ms,
//! [`PCM_PER_BLOCK`] PCM samples) with the block's audio, the
//! [`PowerSquelch`](crate::squelch::PowerSquelch) decision and the tone the
//! detectors currently hear. States ([`ChannelState`]):
//!
//! - **Closed** — the last few blocks are kept as pre-roll. When the squelch
//!   opens, the call is taken to have started where the opening run began
//!   (carrier-up), and the pre-roll from that block on becomes the start of
//!   the held audio. On a tone-gated channel whose squelch is already open
//!   (a carrier that failed the tone, or a call that ended on tone loss), a
//!   block on which the configured tone is heard starts an opening too.
//! - **Opening** — audio is held, nothing is emitted. The opening is kept
//!   when the call is at least `min_call_ms` long, the carrier is present
//!   and (tone-gated) the configured tone is heard; then `Started` is
//!   emitted and the held audio flushed, so the PCM starts at carrier-up.
//!   It is dropped without events if the tone has not been heard by
//!   `tone_timeout_ms` (`tone_mismatch`) or if the carrier goes away first
//!   (`calls_too_short`; `tone_mismatch` instead if a tone-gated channel
//!   never heard its tone).
//! - **Open** — audio is committed to the call and emitted in batches of
//!   [`PCM_BATCH_SAMPLES`].
//! - **Hang** — the carrier is below `close_db` (or, tone-gated, the tone is
//!   not heard). Audio is held: blocks without carrier as silence, blocks
//!   with carrier but no tone as received. A return within `hang_ms` flushes
//!   the held audio (the gap stays in the call as wall time); otherwise the
//!   call ends at the start of the gap ([`EndReason::SquelchClosed`] when the
//!   squelch closed, [`EndReason::ToneLost`] when only the tone went) and the
//!   held audio is dropped, so a call never carries its squelch tail.
//!
//! At `max_call_s` the committed audio is split: `Ended(MaxDuration)` and a
//! new `Started` at the same instant, no sample lost. `max_call_s · 8000` is
//! a whole number of frames, so the split needs no padding.

use crate::squelch::{SquelchBlock, BLOCK_MS, BLOCK_SAMPLES};
use crate::tone::Tone;
use crate::{
    CallEnd, CallStart, ChannelEvent, ChannelParams, ChannelState, EndReason, AUDIO_RATE_HZ,
    FRAME_SAMPLES, INPUT_RATE_HZ, PCM_BATCH_SAMPLES,
};

/// PCM samples per squelch block (5 ms at 8 kHz).
pub const PCM_PER_BLOCK: usize = BLOCK_SAMPLES * AUDIO_RATE_HZ as usize / INPUT_RATE_HZ as usize;

/// Distinct tones a call's histogram tracks (the first ones heard).
const TONE_SLOTS: usize = 4;

/// A CTCSS or DCS detector as the engine drives it: fed the sub-audio
/// stream ([`SUBAUDIO_RATE_HZ`](crate::SUBAUDIO_RATE_HZ), normalised units,
/// positive = positive deviation, DC tracked), asked for its standing
/// decision once per block.
///
/// The `ctcss` and `dcs` detectors implement it; tests can substitute a
/// double. Implementations must not allocate in [`ToneDetector::push`].
pub trait ToneDetector: Send {
    /// Feed sub-audio samples (any length).
    fn push(&mut self, subaudio: &[f32]);
    /// The tone currently detected, if any. DCS detectors may report either
    /// member of an on-air-identical pair (see [`Tone::same_signal`]).
    fn current(&self) -> Option<Tone>;
    /// Forget everything; called at the start of each carrier run.
    fn reset(&mut self);
}

impl ToneDetector for crate::ctcss::CtcssDetector {
    fn push(&mut self, subaudio: &[f32]) {
        let _ = crate::ctcss::CtcssDetector::push(self, subaudio);
    }
    fn current(&self) -> Option<Tone> {
        crate::ctcss::CtcssDetector::current(self)
    }
    fn reset(&mut self) {
        crate::ctcss::CtcssDetector::reset(self);
    }
}

impl ToneDetector for crate::dcs::DcsDetector {
    fn push(&mut self, subaudio: &[f32]) {
        let _ = crate::dcs::DcsDetector::push(self, subaudio);
    }
    fn current(&self) -> Option<Tone> {
        crate::dcs::DcsDetector::current(self)
    }
    fn reset(&mut self) {
        crate::dcs::DcsDetector::reset(self);
    }
}

/// Per-block measurements for the call's extras.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct BlockMeasure {
    /// Mean discriminator output over the block, Hz (carrier offset).
    pub mean_dev_hz: f32,
    /// Largest |deviation| in the block, offset removed, Hz.
    pub peak_dev_hz: f32,
}

/// One block's input to the segmenter.
#[derive(Clone, Copy, Debug)]
pub struct BlockInput<'a> {
    /// Block index since the engine was built (block `b` covers input
    /// samples `b·125 .. (b+1)·125`).
    pub index: u64,
    /// The block's PCM, [`PCM_PER_BLOCK`] samples.
    pub pcm: &'a [i16],
    /// Squelch decision for the block.
    pub squelch: SquelchBlock,
    /// Tone the detectors hear now.
    pub tone: Option<Tone>,
    /// Measurements.
    pub measure: BlockMeasure,
}

/// Accumulated measurements of the current call.
#[derive(Clone, Debug)]
struct Measure {
    power_dbfs: f32,
    snr_db: f32,
    dev_sum: f64,
    dev_n: u64,
    peak_dev_hz: f32,
    tones: [(Option<Tone>, u32); TONE_SLOTS],
    /// The newest block, folded in only once the next block is present too
    /// (the block in which the carrier drops is mostly noise).
    last: Option<(BlockMeasure, Option<Tone>)>,
}

impl Measure {
    fn new(sq: &SquelchBlock) -> Self {
        Measure {
            power_dbfs: sq.power_dbfs,
            snr_db: sq.snr_db,
            dev_sum: 0.0,
            dev_n: 0,
            peak_dev_hz: 0.0,
            tones: [(None, 0); TONE_SLOTS],
            last: None,
        }
    }

    fn add(&mut self, m: &BlockMeasure, tone: Option<Tone>) {
        if let Some((pm, pt)) = self.last.replace((*m, tone)) {
            self.fold(&pm, pt);
        }
    }

    fn drop_last(&mut self) {
        self.last = None;
    }

    fn fold(&mut self, m: &BlockMeasure, tone: Option<Tone>) {
        self.dev_sum += f64::from(m.mean_dev_hz);
        self.dev_n += 1;
        self.peak_dev_hz = self.peak_dev_hz.max(m.peak_dev_hz);
        let Some(t) = tone else { return };
        for slot in self.tones.iter_mut() {
            match slot.0 {
                Some(s) if s == t => {
                    slot.1 += 1;
                    return;
                }
                None => {
                    *slot = (Some(t), 1);
                    return;
                }
                _ => {}
            }
        }
    }

    fn offset_hz(&self) -> f32 {
        if self.dev_n == 0 {
            0.0
        } else {
            (self.dev_sum / self.dev_n as f64) as f32
        }
    }

    fn main_tone(&self) -> Option<Tone> {
        self.tones
            .iter()
            .filter(|s| s.1 > 0)
            .max_by_key(|s| s.1)
            .and_then(|s| s.0)
    }
}

/// Counters the segmenter keeps for [`ChannelStats`](crate::ChannelStats).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct SegmentCounters {
    /// `Started` events emitted.
    pub calls: u64,
    /// Openings dropped for being shorter than `min_call_ms`.
    pub calls_too_short: u64,
    /// Openings dropped because the configured tone was not heard.
    pub tone_mismatch: u64,
    /// Stream time of the last `Ended`, ms.
    pub last_call_end_ms: Option<u64>,
    /// Carrier offset of the last call, Hz.
    pub last_offset_hz: f32,
}

/// The call segmentation state machine for one channel. See the module docs.
pub struct CallSegmenter {
    tone: Option<Tone>,
    min_call_blocks: u64,
    tone_timeout_blocks: u64,
    hang_blocks: u32,
    max_samples: u64,
    state: ChannelState,
    /// Closed: ring of the last blocks' PCM.
    preroll: Vec<[i16; PCM_PER_BLOCK]>,
    pre_head: usize,
    pre_len: usize,
    /// Opening: the whole call so far; Hang: the gap so far.
    hold: Vec<i16>,
    /// Consecutive blocks without (effective) presence.
    absent: u32,
    /// Opening: the configured tone has been heard.
    tone_seen: bool,
    /// First block of the current call (opening).
    start_block: u64,
    /// Stream time of the current call's first sample, ms.
    start_ms: u64,
    /// Samples committed to the current call.
    committed: u64,
    /// Committed but not yet emitted.
    pending: Vec<i16>,
    /// Open: the newest present block, committed only once the next block
    /// is present too, so the block in which the carrier drops (mostly
    /// noise) goes into the hang hold and is trimmed with it.
    delayed: Option<[i16; PCM_PER_BLOCK]>,
    measure: Option<Measure>,
    counters: SegmentCounters,
}

fn block_ms(anchor_ms: f64, block: u64) -> u64 {
    (anchor_ms + block as f64 * f64::from(BLOCK_MS))
        .round()
        .max(0.0) as u64
}

impl CallSegmenter {
    /// Segmenter for a channel with `params` (squelch timing, tone).
    pub fn new(params: &ChannelParams, attack_blocks: u32) -> Self {
        let s = &params.squelch;
        let bms = u64::from(BLOCK_MS);
        let min_call_blocks = u64::from(s.min_call_ms).div_ceil(bms);
        let tone_timeout_blocks = u64::from(s.tone_timeout_ms).div_ceil(bms).max(1);
        let hang_blocks = s.hang_ms.div_ceil(BLOCK_MS).max(1);
        let pre_blocks = attack_blocks as usize + 2;
        // Opening holds up to max(timeout, min_call + hang) plus the attack.
        let hold_blocks = tone_timeout_blocks.max(min_call_blocks + u64::from(hang_blocks))
            + u64::from(attack_blocks)
            + 2;
        CallSegmenter {
            tone: params.tone,
            min_call_blocks,
            tone_timeout_blocks,
            hang_blocks,
            max_samples: u64::from(s.max_call_s) * u64::from(AUDIO_RATE_HZ),
            state: ChannelState::Closed,
            preroll: vec![[0; PCM_PER_BLOCK]; pre_blocks],
            pre_head: 0,
            pre_len: 0,
            hold: Vec::with_capacity(hold_blocks as usize * PCM_PER_BLOCK),
            absent: 0,
            tone_seen: false,
            start_block: 0,
            start_ms: 0,
            committed: 0,
            pending: Vec::with_capacity(2 * PCM_BATCH_SAMPLES),
            delayed: None,
            measure: None,
            counters: SegmentCounters::default(),
        }
    }

    /// Current state.
    pub fn state(&self) -> ChannelState {
        self.state
    }

    /// Counters so far.
    pub fn counters(&self) -> SegmentCounters {
        self.counters
    }

    fn tone_ok(&self, heard: Option<Tone>) -> bool {
        match (self.tone, heard) {
            (None, _) => true,
            (Some(want), Some(h)) => want.same_signal(&h),
            (Some(_), None) => false,
        }
    }

    /// Process one block. `anchor_ms` is the stream time of block 0 (the
    /// engine's current time base).
    pub fn block(
        &mut self,
        inp: &BlockInput<'_>,
        anchor_ms: f64,
        sink: &mut dyn FnMut(ChannelEvent),
    ) {
        debug_assert_eq!(inp.pcm.len(), PCM_PER_BLOCK);
        let sq = inp.squelch;
        let gated = self.tone.is_some();
        let tone_ok = self.tone_ok(inp.tone);
        let silence = [0i16; PCM_PER_BLOCK];
        let audio: &[i16] = if sq.present { inp.pcm } else { &silence };

        match self.state {
            ChannelState::Closed => {
                let mut blk = [0i16; PCM_PER_BLOCK];
                blk.copy_from_slice(inp.pcm);
                let cap = self.preroll.len();
                self.preroll[self.pre_head] = blk;
                self.pre_head = (self.pre_head + 1) % cap;
                self.pre_len = (self.pre_len + 1).min(cap);

                let back = if let Some(k) = sq.rose {
                    Some((k as usize).min(self.pre_len - 1))
                } else if gated && sq.open && sq.present && tone_ok {
                    Some(0)
                } else {
                    None
                };
                if let Some(k) = back {
                    self.begin_opening(inp, k, anchor_ms);
                    self.opening_step(inp, tone_ok, sink);
                }
            }
            ChannelState::Opening => {
                self.hold.extend_from_slice(audio);
                self.opening_step(inp, tone_ok, sink);
            }
            ChannelState::Open => {
                if !sq.open {
                    self.end_call(EndReason::SquelchClosed, sink);
                } else if sq.present && tone_ok {
                    self.add_measure(inp);
                    if let Some(d) = self.delayed.take() {
                        self.commit_slice(&d, &sq, inp.tone, sink);
                    }
                    self.delay(inp.pcm);
                } else {
                    self.state = ChannelState::Hang;
                    self.hold.clear();
                    if let Some(d) = self.delayed.take() {
                        self.hold.extend_from_slice(&d);
                    }
                    if let Some(m) = self.measure.as_mut() {
                        m.drop_last();
                    }
                    self.hold.extend_from_slice(audio);
                    self.absent = 1;
                    self.hang_expiry(&sq, sink);
                }
            }
            ChannelState::Hang => {
                if !sq.open {
                    self.end_call(EndReason::SquelchClosed, sink);
                } else if sq.present && tone_ok {
                    self.state = ChannelState::Open;
                    self.absent = 0;
                    let held = std::mem::take(&mut self.hold);
                    self.commit_slice(&held, &sq, inp.tone, sink);
                    self.hold = held;
                    self.hold.clear();
                    self.add_measure(inp);
                    self.delay(inp.pcm);
                } else {
                    self.hold.extend_from_slice(audio);
                    self.absent += 1;
                    self.hang_expiry(&sq, sink);
                }
            }
        }
    }

    fn hang_expiry(&mut self, sq: &SquelchBlock, sink: &mut dyn FnMut(ChannelEvent)) {
        if self.absent >= self.hang_blocks {
            let reason = if sq.open {
                EndReason::ToneLost
            } else {
                EndReason::SquelchClosed
            };
            self.end_call(reason, sink);
        }
    }

    fn begin_opening(&mut self, inp: &BlockInput<'_>, back: usize, anchor_ms: f64) {
        self.state = ChannelState::Opening;
        self.hold.clear();
        let cap = self.preroll.len();
        // Oldest wanted block is `back` before the newest (pre_head - 1).
        for i in (0..=back).rev() {
            let idx = (self.pre_head + cap - 1 - i) % cap;
            self.hold.extend_from_slice(&self.preroll[idx]);
        }
        self.pre_len = 0;
        self.start_block = inp.index - back as u64;
        self.start_ms = block_ms(anchor_ms, self.start_block);
        self.absent = 0;
        self.tone_seen = false;
        self.measure = Some(Measure::new(&inp.squelch));
    }

    /// The Opening decision for the current block (its audio is already in
    /// `hold`).
    fn opening_step(
        &mut self,
        inp: &BlockInput<'_>,
        tone_ok: bool,
        sink: &mut dyn FnMut(ChannelEvent),
    ) {
        let sq = inp.squelch;
        let gated = self.tone.is_some();
        if gated && tone_ok && sq.present {
            self.tone_seen = true;
        }
        let present = sq.present && (!gated || !self.tone_seen || tone_ok);
        if present {
            self.absent = 0;
            self.add_measure(inp);
        } else {
            self.absent += 1;
            if let Some(m) = self.measure.as_mut() {
                m.drop_last();
            }
        }
        let elapsed = inp.index + 1 - self.start_block;

        if !sq.open || self.absent >= self.hang_blocks {
            if gated && !self.tone_seen {
                self.counters.tone_mismatch += 1;
            } else {
                self.counters.calls_too_short += 1;
            }
            self.drop_opening();
            return;
        }
        if gated && !self.tone_seen && elapsed >= self.tone_timeout_blocks {
            self.counters.tone_mismatch += 1;
            self.drop_opening();
            return;
        }
        if sq.present && tone_ok && elapsed >= self.min_call_blocks {
            self.state = ChannelState::Open;
            self.committed = 0;
            self.pending.clear();
            let tone = if gated { self.tone } else { inp.tone };
            self.emit_started(self.start_ms, &sq, tone, sink);
            // The current block (present) is the last in the hold.
            let held = std::mem::take(&mut self.hold);
            let cut = held.len() - PCM_PER_BLOCK;
            self.commit_slice(&held[..cut], &sq, inp.tone, sink);
            self.delay(&held[cut..]);
            self.hold = held;
            self.hold.clear();
        }
    }

    fn delay(&mut self, pcm: &[i16]) {
        let mut b = [0i16; PCM_PER_BLOCK];
        b.copy_from_slice(pcm);
        self.delayed = Some(b);
    }

    fn drop_opening(&mut self) {
        self.state = ChannelState::Closed;
        self.hold.clear();
        self.measure = None;
        self.absent = 0;
    }

    fn add_measure(&mut self, inp: &BlockInput<'_>) {
        if let Some(m) = self.measure.as_mut() {
            m.add(&inp.measure, inp.tone);
        }
    }

    fn emit_started(
        &mut self,
        at_ms: u64,
        sq: &SquelchBlock,
        tone: Option<Tone>,
        sink: &mut dyn FnMut(ChannelEvent),
    ) {
        self.counters.calls += 1;
        let m = self.measure.get_or_insert_with(|| Measure::new(sq));
        sink(ChannelEvent::Started(CallStart {
            at_ms,
            power_dbfs: m.power_dbfs,
            snr_db: m.snr_db,
            tone_detected: tone,
        }));
    }

    /// Commit audio to the open call, splitting at `max_call_s`.
    fn commit_slice(
        &mut self,
        mut samples: &[i16],
        sq: &SquelchBlock,
        heard: Option<Tone>,
        sink: &mut dyn FnMut(ChannelEvent),
    ) {
        while !samples.is_empty() {
            if self.committed >= self.max_samples {
                self.end_call_keep_state(EndReason::MaxDuration, sink);
                let at = self.counters.last_call_end_ms.unwrap_or(self.start_ms);
                self.start_ms = at;
                self.committed = 0;
                self.measure = Some(Measure::new(sq));
                let tone = if self.tone.is_some() {
                    self.tone
                } else {
                    heard
                };
                self.emit_started(at, sq, tone, sink);
            }
            let room = (self.max_samples - self.committed) as usize;
            let take = room.min(samples.len());
            self.pending.extend_from_slice(&samples[..take]);
            self.committed += take as u64;
            samples = &samples[take..];
            let whole = self.pending.len() / PCM_BATCH_SAMPLES * PCM_BATCH_SAMPLES;
            if whole > 0 {
                for batch in self.pending[..whole].chunks(PCM_BATCH_SAMPLES) {
                    sink(ChannelEvent::Pcm(batch.to_vec()));
                }
                self.pending.drain(..whole);
            }
        }
    }

    /// Emit the rest of the call's audio (last frame zero-padded) and
    /// `Ended`; leaves the state machine as it is.
    fn end_call_keep_state(&mut self, reason: EndReason, sink: &mut dyn FnMut(ChannelEvent)) {
        let pad = (FRAME_SAMPLES - self.pending.len() % FRAME_SAMPLES) % FRAME_SAMPLES;
        self.pending.resize(self.pending.len() + pad, 0);
        let total = self.committed + pad as u64;
        for batch in self.pending.chunks(PCM_BATCH_SAMPLES) {
            sink(ChannelEvent::Pcm(batch.to_vec()));
        }
        self.pending.clear();
        let duration_ms = total * 1000 / u64::from(AUDIO_RATE_HZ);
        let at_ms = self.start_ms + duration_ms;
        let m = self.measure.take();
        let (power_dbfs, snr_db, peak, offset, tone) = match &m {
            Some(m) => (
                m.power_dbfs,
                m.snr_db,
                m.peak_dev_hz,
                m.offset_hz(),
                m.main_tone(),
            ),
            None => (0.0, 0.0, 0.0, 0.0, None),
        };
        self.counters.last_call_end_ms = Some(at_ms);
        self.counters.last_offset_hz = offset;
        sink(ChannelEvent::Ended(CallEnd {
            at_ms,
            reason,
            duration_ms,
            tone_detected: tone,
            power_dbfs,
            snr_db,
            peak_deviation_hz: peak,
            offset_hz: offset,
        }));
    }

    fn end_call(&mut self, reason: EndReason, sink: &mut dyn FnMut(ChannelEvent)) {
        self.delayed = None;
        self.end_call_keep_state(reason, sink);
        self.state = ChannelState::Closed;
        self.hold.clear();
        self.absent = 0;
        self.committed = 0;
        self.pre_len = 0;
    }

    /// Recorder stopping: end an open call with [`EndReason::Shutdown`]
    /// (audio held in a hang is dropped, as on a normal close); an
    /// unconfirmed opening is dropped silently.
    pub fn finish(&mut self, sink: &mut dyn FnMut(ChannelEvent)) {
        match self.state {
            ChannelState::Open | ChannelState::Hang => {
                if let Some(d) = self.delayed.take() {
                    let sq = SquelchBlock {
                        power_dbfs: 0.0,
                        snr_db: 0.0,
                        open: true,
                        present: true,
                        rose: None,
                        closed: None,
                        run_started: false,
                    };
                    self.commit_slice(&d, &sq, None, sink);
                }
                self.end_call(EndReason::Shutdown, sink)
            }
            ChannelState::Opening => self.drop_opening(),
            ChannelState::Closed => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive the segmenter directly with scripted squelch blocks.
    struct Rig {
        seg: CallSegmenter,
        index: u64,
        events: Vec<ChannelEvent>,
    }

    #[derive(Clone, Copy)]
    enum B {
        /// Squelch closed.
        Off,
        /// Squelch opened this block, run began `k` blocks ago.
        Rise(u32),
        /// Open, carrier present.
        On,
        /// Open, carrier absent.
        Gap,
        /// Closed this block.
        Close,
    }

    impl Rig {
        fn new(p: ChannelParams) -> Self {
            Rig {
                seg: CallSegmenter::new(&p, 8),
                index: 0,
                events: Vec::new(),
            }
        }

        fn run(&mut self, b: B, n: usize, tone: Option<Tone>) {
            for _ in 0..n {
                let mut sq = SquelchBlock {
                    power_dbfs: -40.0,
                    snr_db: 20.0,
                    open: true,
                    present: true,
                    rose: None,
                    closed: None,
                    run_started: false,
                };
                match b {
                    B::Off => {
                        sq.open = false;
                        sq.present = false;
                    }
                    B::Rise(k) => sq.rose = Some(k),
                    B::On => {}
                    B::Gap => sq.present = false,
                    B::Close => {
                        sq.open = false;
                        sq.present = false;
                        sq.closed = Some(0);
                    }
                }
                // PCM = low 15 bits of the block index, so tests can tell
                // which blocks made it into the output.
                let pcm = [(self.index % 30_000) as i16 + 1; PCM_PER_BLOCK];
                let inp = BlockInput {
                    index: self.index,
                    pcm: &pcm,
                    squelch: sq,
                    tone,
                    measure: BlockMeasure {
                        mean_dev_hz: 100.0,
                        peak_dev_hz: 2_000.0,
                    },
                };
                let ev = &mut self.events;
                self.seg.block(&inp, 1_000.0, &mut |e| ev.push(e));
                self.index += 1;
            }
        }

        fn pcm(&self) -> Vec<i16> {
            self.events
                .iter()
                .filter_map(|e| match e {
                    ChannelEvent::Pcm(p) => Some(p.clone()),
                    _ => None,
                })
                .flatten()
                .collect()
        }

        fn starts(&self) -> Vec<CallStart> {
            self.events
                .iter()
                .filter_map(|e| match e {
                    ChannelEvent::Started(s) => Some(*s),
                    _ => None,
                })
                .collect()
        }

        fn ends(&self) -> Vec<CallEnd> {
            self.events
                .iter()
                .filter_map(|e| match e {
                    ChannelEvent::Ended(s) => Some(*s),
                    _ => None,
                })
                .collect()
        }
    }

    fn check_contract(events: &[ChannelEvent]) {
        let mut open = false;
        for e in events {
            match e {
                ChannelEvent::Started(_) => {
                    assert!(!open);
                    open = true;
                }
                ChannelEvent::Pcm(p) => {
                    assert!(open);
                    assert!(!p.is_empty() && p.len() <= PCM_BATCH_SAMPLES);
                    assert_eq!(p.len() % FRAME_SAMPLES, 0);
                }
                ChannelEvent::Ended(_) => {
                    assert!(open);
                    open = false;
                }
            }
        }
    }

    #[test]
    fn carrier_call_starts_at_run_start_and_trims_hang() {
        let mut r = Rig::new(ChannelParams::default());
        r.run(B::Off, 50, None);
        r.run(B::Rise(7), 1, None); // block 50, run began at block 43
        r.run(B::On, 400, None); // through block 450
        r.run(B::Gap, 199, None);
        r.run(B::Close, 1, None);
        check_contract(&r.events);
        let s = r.starts();
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].at_ms, 1_000 + 43 * 5);
        let e = r.ends();
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].reason, EndReason::SquelchClosed);
        // 408 blocks of carrier (43..=450) = 2040 ms, padded to a frame.
        assert_eq!(e[0].duration_ms, 2_040);
        assert_eq!(e[0].at_ms, s[0].at_ms + e[0].duration_ms);
        let pcm = r.pcm();
        assert_eq!(pcm.len(), 2_040 * 8);
        assert_eq!(pcm[0], 44); // block 43's marker
        assert_eq!(e[0].offset_hz, 100.0);
        assert_eq!(e[0].peak_deviation_hz, 2_000.0);
        assert_eq!(r.seg.counters().calls, 1);
    }

    #[test]
    fn gap_within_hang_is_silence_and_call_continues() {
        let mut r = Rig::new(ChannelParams::default());
        r.run(B::Off, 20, None);
        r.run(B::Rise(7), 1, None);
        r.run(B::On, 200, None);
        r.run(B::Gap, 50, None);
        r.run(B::On, 100, None);
        r.run(B::Gap, 199, None);
        r.run(B::Close, 1, None);
        check_contract(&r.events);
        assert_eq!(r.starts().len(), 1);
        let pcm = r.pcm();
        // (8 + 200 + 50 + 100) blocks, padded to a whole frame.
        assert_eq!(
            pcm.len(),
            (358 * PCM_PER_BLOCK).div_ceil(FRAME_SAMPLES) * FRAME_SAMPLES
        );
        assert!(pcm[358 * PCM_PER_BLOCK..].iter().all(|&s| s == 0));
        let gap = &pcm[208 * PCM_PER_BLOCK..258 * PCM_PER_BLOCK];
        assert!(gap.iter().all(|&s| s == 0));
        assert!(pcm[208 * PCM_PER_BLOCK - 1] != 0 && pcm[258 * PCM_PER_BLOCK] != 0);
    }

    #[test]
    fn short_call_dropped_without_events() {
        let mut r = Rig::new(ChannelParams::default());
        r.run(B::Off, 20, None);
        r.run(B::Rise(7), 1, None);
        r.run(B::On, 50, None); // 290 ms < 500 ms
        r.run(B::Gap, 199, None);
        r.run(B::Close, 1, None);
        assert!(r.events.is_empty());
        assert_eq!(r.seg.counters().calls_too_short, 1);
        assert_eq!(r.seg.state(), ChannelState::Closed);
    }

    #[test]
    fn tone_gate_flushes_from_carrier_up() {
        let t: Tone = "100.0".parse().unwrap();
        let mut r = Rig::new(ChannelParams::with_tone(Some(t)));
        r.run(B::Off, 20, None);
        r.run(B::Rise(7), 1, None); // run began at block 13
        r.run(B::On, 80, None); // tone not yet locked
        r.run(B::On, 200, Some(t)); // locks at ~445 ms after carrier-up
        r.run(B::Gap, 199, Some(t));
        r.run(B::Close, 1, None);
        check_contract(&r.events);
        let s = r.starts();
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].at_ms, 1_000 + 13 * 5);
        assert_eq!(s[0].tone_detected, Some(t));
        assert_eq!(r.pcm()[0], 14);
        assert_eq!(r.ends()[0].duration_ms, (8 + 280) * 5);
        assert_eq!(r.ends()[0].tone_detected, Some(t));
    }

    #[test]
    fn wrong_tone_times_out_once() {
        let t: Tone = "100.0".parse().unwrap();
        let other: Tone = "131.8".parse().unwrap();
        let mut r = Rig::new(ChannelParams::with_tone(Some(t)));
        r.run(B::Off, 20, None);
        r.run(B::Rise(7), 1, None);
        r.run(B::On, 2_000, Some(other));
        r.run(B::Gap, 199, Some(other));
        r.run(B::Close, 1, None);
        assert!(r.events.is_empty());
        assert_eq!(r.seg.counters().tone_mismatch, 1);
        assert_eq!(r.seg.counters().calls_too_short, 0);
    }

    #[test]
    fn dcs_equivalent_matches() {
        let want: Tone = "D445I".parse().unwrap();
        let heard = want.dcs_equivalent().unwrap();
        let mut r = Rig::new(ChannelParams::with_tone(Some(want)));
        r.run(B::Off, 10, None);
        r.run(B::Rise(7), 1, None);
        r.run(B::On, 200, Some(heard));
        assert_eq!(r.starts().len(), 1);
    }

    #[test]
    fn tone_lost_ends_after_hang_and_tone_return_reopens() {
        let t: Tone = "100.0".parse().unwrap();
        let mut r = Rig::new(ChannelParams::with_tone(Some(t)));
        r.run(B::Off, 10, None);
        r.run(B::Rise(7), 1, None);
        r.run(B::On, 300, Some(t));
        r.run(B::On, 200, None); // carrier stays, tone gone for hang_ms
        check_contract(&r.events);
        let e = r.ends();
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].reason, EndReason::ToneLost);
        assert_eq!(e[0].duration_ms, (8 + 300) * 5);
        assert_eq!(r.seg.state(), ChannelState::Closed);
        // Our tone comes back on the still-open carrier: a new call.
        r.run(B::On, 200, Some(t));
        assert_eq!(r.starts().len(), 2);
        assert_eq!(r.seg.state(), ChannelState::Open);
    }

    #[test]
    fn max_duration_splits_without_loss() {
        let mut p = ChannelParams::default();
        p.squelch.max_call_s = 2;
        let mut r = Rig::new(p);
        r.run(B::Off, 10, None);
        r.run(B::Rise(7), 1, None);
        r.run(B::On, 1_000, None); // 5.04 s of carrier
        r.run(B::Gap, 199, None);
        r.run(B::Close, 1, None);
        check_contract(&r.events);
        let s = r.starts();
        let e = r.ends();
        assert_eq!(s.len(), 3);
        assert_eq!(e.len(), 3);
        assert_eq!(e[0].reason, EndReason::MaxDuration);
        assert_eq!(e[1].reason, EndReason::MaxDuration);
        assert_eq!(e[2].reason, EndReason::SquelchClosed);
        assert_eq!(e[0].duration_ms, 2_000);
        assert_eq!(s[1].at_ms, e[0].at_ms);
        assert_eq!(s[2].at_ms, e[1].at_ms);
        assert_eq!(e[2].duration_ms, 1_040);
        assert_eq!(r.pcm().len(), 1_008 * PCM_PER_BLOCK);
    }

    #[test]
    fn finish_ends_open_call_and_drops_opening() {
        let mut r = Rig::new(ChannelParams::default());
        r.run(B::Off, 10, None);
        r.run(B::Rise(7), 1, None);
        r.run(B::On, 200, None);
        r.run(B::Gap, 20, None);
        let ev = &mut r.events;
        r.seg.finish(&mut |e| ev.push(e));
        check_contract(&r.events);
        let e = r.ends();
        assert_eq!(e[0].reason, EndReason::Shutdown);
        assert_eq!(e[0].duration_ms, 208 * 5);

        let mut r = Rig::new(ChannelParams::default());
        r.run(B::Off, 10, None);
        r.run(B::Rise(7), 1, None);
        r.run(B::On, 10, None);
        let ev = &mut r.events;
        r.seg.finish(&mut |e| ev.push(e));
        assert!(r.events.is_empty());
        assert_eq!(r.seg.state(), ChannelState::Closed);
    }
}
