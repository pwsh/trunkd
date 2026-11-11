// SPDX-License-Identifier: GPL-3.0-or-later
//! Live audio monitoring: hearing the recorder's own decoded voice.
//!
//! trunkd's audio is *finished* audio — a call is assembled, encoded and
//! uploaded, and the place to listen to it is Keyup. That is still true, and
//! this module does not change it. What it adds is the one thing an archive
//! cannot answer: **is what this box is decoding, right now, actually
//! intelligible?** A misaligned tuner, a vocoder fed marginal frames, or an
//! antenna somebody moved all produce calls that upload perfectly and sound
//! like gravel, and finding that out an hour later from a transcript is finding
//! it out too late.
//!
//! So: an opt-in, off-by-default tap on the vocoder's output, served from the
//! panel as an MP3 stream a browser's `<audio>` element can play.
//!
//! # Shape, and what it must never cost
//!
//! [`MonitorHub::publish`] is called from the call thread, once per 20 ms voice
//! frame per open call. That thread is not a DSP thread, but it is on the path
//! every call's audio takes, and a monitor nobody is listening to must be
//! free. It is: `publish` opens with a single relaxed load of an
//! [`AtomicUsize`] and returns when it reads zero. No lock is taken, no
//! allocation is made, and the hub is not even constructed unless
//! `[server] live_audio` is on — the call thread holds an [`Option`] that is
//! `None` in every default deployment.
//!
//! With listeners, the cost is one mutex over a `Vec` of at most
//! [`MAX_STREAMS`] subscribers, and one `Vec<i16>` of 160 samples per matching
//! subscriber. Delivery is [`SyncSender::try_send`] into a bounded queue and
//! **drops on a full one** — a listener whose browser stalled must never be
//! able to slow down call assembly. Dropped frames are counted, not waited on.
//!
//! # Pacing
//!
//! Each stream owns a thread that ticks on a monotonic 20 ms deadline, drains
//! whatever arrived, mixes it and encodes it. When nothing arrived — the usual
//! case, since a site is silent most of the time — it emits one frame of
//! silence, which is what keeps a browser's audio element from deciding the
//! stream ended. See [`mix_tick`].
//!
//! # What it is not
//!
//! Not a scanner: there is no squelch, no priority, no call history, no
//! recording. Not a replacement for Keyup, which remains where calls are
//! listened to. And not something to leave running on a LAN-exposed panel:
//! it is off unless configured on, and it is served under the same token rule
//! as every other read.

use std::io::{self, Read};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use trunkd_call::Mp3Stream;
use trunkd_p25::event::CallKind;

/// Samples in one vocoder frame: 20 ms at 8 kHz.
pub(crate) const FRAME: usize = 160;

/// One frame's worth of wall-clock time.
const TICK: Duration = Duration::from_millis(20);

/// Frames a subscriber may fall behind before delivery starts dropping: 100
/// frames is two seconds, which is more slack than a browser needs to start
/// playing and less than a listener would sit through as latency.
const QUEUE_FRAMES: usize = 100;

/// Frames one tick will take off the queue. A stream that has fallen behind
/// catches up at real time (the deadline advances by what was emitted), so this
/// only bounds how much work one tick does.
const DRAIN_MAX: usize = 25;

/// Concurrent monitor streams. Four is more than the number of people who are
/// ever looking at one recorder's panel, and each one costs a thread, a LAME
/// encoder and a 2 s queue.
pub(crate) const MAX_STREAMS: usize = 4;

/// Monitor bitrate. 32 kbps mono is twice what a call is uploaded at, because
/// this stream is the one an operator is judging *audio quality* from and the
/// codec should not be the thing they hear.
pub(crate) const BITRATE_KBPS: u32 = 32;

/// How far behind the deadline may drift before it is resynchronised rather
/// than chased. A stream that was descheduled for a second should skip the
/// second, not sprint through it.
const RESYNC_AFTER: Duration = Duration::from_millis(500);

// ---------------------------------------------------------------------------
// What a frame is published with
// ---------------------------------------------------------------------------

/// The identity of the call a published frame belongs to.
///
/// Built once when a call opens and held for its life, so the per-frame cost of
/// publishing is an `Arc` clone rather than any formatting. The `label` is what
/// the stream thread logs when the traffic it is carrying changes — "what am I
/// listening to" is the question a monitor exists to answer, and it is not one
/// the audio itself can answer for an unfamiliar system.
#[derive(Clone, Debug)]
pub(crate) struct CallAudioMeta {
    /// The engine's call id, unique per site and already namespaced by tuner.
    /// Used to keep one call's frames together when several are mixed.
    call: u64,
    /// The talkgroup a listener can filter on, for the calls that have one. A
    /// unit-to-unit or telephone call is filed under a radio id, not a
    /// talkgroup, so it is reachable only through the unfiltered stream.
    talkgroup: Option<u32>,
    /// Human-readable identity, rendered once.
    label: Arc<str>,
}

impl CallAudioMeta {
    /// Describe an open call for the monitor.
    pub(crate) fn new(
        call: u64,
        talkgroup: Option<u32>,
        kind: CallKind,
        source: u32,
        tuner: Option<&str>,
        frequency_hz: u64,
    ) -> Self {
        let kind = match kind {
            CallKind::Group => "group",
            CallKind::UnitToUnit => "unit",
            CallKind::Telephone => "phone",
        };
        let who = match talkgroup {
            Some(tg) => format!("tg {tg}"),
            None => format!("call {call:#x}"),
        };
        let label = format!(
            "{who} ({kind}) from {source} on {:.4} MHz{}",
            frequency_hz as f64 / 1e6,
            match tuner {
                Some(t) if !t.is_empty() => format!(" via {t}"),
                _ => String::new(),
            }
        );
        CallAudioMeta {
            call,
            talkgroup,
            label: Arc::from(label.as_str()),
        }
    }
}

/// One subscriber's 20 ms of audio, tagged with the call it came from so a
/// mixed tick can keep two simultaneous calls sample-aligned.
#[derive(Clone, Debug)]
pub(crate) struct Chunk {
    /// The call id, as [`CallAudioMeta::call`].
    pub call: u64,
    /// The call's rendered identity, for the "now playing" log line.
    pub label: Arc<str>,
    /// 8 kHz mono PCM, normally exactly [`FRAME`] samples.
    pub pcm: Vec<i16>,
}

/// What a listener asked to hear.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Filter {
    /// Everything this recorder decodes, mixed.
    All,
    /// One talkgroup.
    Talkgroup(u32),
}

impl Filter {
    /// Whether a call's audio belongs in this stream.
    fn matches(self, meta: &CallAudioMeta) -> bool {
        match self {
            Filter::All => true,
            Filter::Talkgroup(tg) => meta.talkgroup == Some(tg),
        }
    }

    /// How the filter reads in a log line.
    fn describe(self) -> String {
        match self {
            Filter::All => "all traffic".to_owned(),
            Filter::Talkgroup(tg) => format!("talkgroup {tg}"),
        }
    }
}

// ---------------------------------------------------------------------------
// The hub
// ---------------------------------------------------------------------------

struct Subscriber {
    id: u64,
    filter: Filter,
    tx: SyncSender<Chunk>,
}

/// The fan-out point between the call thread and the monitor streams.
///
/// Constructed only when `[server] live_audio` is set; everything downstream
/// holds `Option<Arc<MonitorHub>>` so that an unconfigured recorder does not
/// even have one.
pub(crate) struct MonitorHub {
    subs: Mutex<Vec<Subscriber>>,
    /// The publish path's fast exit: subscriber count, kept outside the mutex
    /// so the common case is one relaxed load.
    count: AtomicUsize,
    next_id: AtomicU64,
    /// Frames dropped because a listener's queue was full.
    dropped: AtomicU64,
}

impl MonitorHub {
    /// A hub with no listeners.
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(MonitorHub {
            subs: Mutex::new(Vec::new()),
            count: AtomicUsize::new(0),
            next_id: AtomicU64::new(1),
            dropped: AtomicU64::new(0),
        })
    }

    /// Offer one frame of a call's audio to whoever is listening.
    ///
    /// Never blocks and never fails. With no subscribers this is a single
    /// relaxed atomic load — see the module docs.
    pub(crate) fn publish(&self, meta: &CallAudioMeta, pcm: &[i16]) {
        if self.count.load(Ordering::Relaxed) == 0 {
            return;
        }
        let subs = match self.subs.lock() {
            Ok(g) => g,
            // A panicked stream thread must not stop the recorder's audio.
            Err(poisoned) => poisoned.into_inner(),
        };
        for sub in subs.iter() {
            if !sub.filter.matches(meta) {
                continue;
            }
            let chunk = Chunk {
                call: meta.call,
                label: Arc::clone(&meta.label),
                pcm: pcm.to_vec(),
            };
            if sub.tx.try_send(chunk).is_err() {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Take a subscription, or `None` when [`MAX_STREAMS`] are already open.
    ///
    /// The count is maintained inside the lock so two simultaneous requests
    /// cannot both see room for the last slot.
    pub(crate) fn subscribe(self: &Arc<Self>, filter: Filter) -> Option<MonitorSub> {
        let mut subs = match self.subs.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        if subs.len() >= MAX_STREAMS {
            return None;
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = sync_channel(QUEUE_FRAMES);
        subs.push(Subscriber { id, filter, tx });
        // Published *after* the subscriber is in the list, so a publisher that
        // sees a non-zero count always finds it.
        self.count.store(subs.len(), Ordering::Relaxed);
        drop(subs);
        Some(MonitorSub {
            hub: Arc::clone(self),
            id,
            filter,
            rx,
        })
    }

    /// Open streams right now.
    pub(crate) fn streams(&self) -> usize {
        self.count.load(Ordering::Relaxed)
    }

    /// Frames dropped over this run because a listener could not keep up.
    pub(crate) fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    fn unsubscribe(&self, id: u64) {
        let mut subs = match self.subs.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        subs.retain(|s| s.id != id);
        self.count.store(subs.len(), Ordering::Relaxed);
    }
}

/// A live subscription. Dropping it unsubscribes, which is what makes a client
/// disconnect — the only way a monitor stream normally ends — self-cleaning.
pub(crate) struct MonitorSub {
    hub: Arc<MonitorHub>,
    id: u64,
    filter: Filter,
    rx: Receiver<Chunk>,
}

impl Drop for MonitorSub {
    fn drop(&mut self) {
        self.hub.unsubscribe(self.id);
    }
}

// ---------------------------------------------------------------------------
// Mixing and pacing
// ---------------------------------------------------------------------------

/// Turn one tick's arrivals into the PCM to encode.
///
/// * **Nothing arrived** — one frame of silence. This is the common case (a
///   site is idle most of the time) and it is load-bearing: a stream that
///   emitted nothing while idle would stall the browser's decoder, and the
///   first call after the silence would be the one that got lost.
/// * **One call** — its frames, in order.
/// * **Several calls** — summed sample-by-sample with saturation. Frames are
///   grouped by call id first, so a tick that drained two frames of one call
///   and one of another lays the second call over the *first* 20 ms rather
///   than smearing it across both.
///
/// Saturating rather than averaging: two simultaneous calls are rare and brief
/// (a monitor listener wants to know both are happening), and averaging would
/// halve the level of every call on a busy site.
pub(crate) fn mix_tick(chunks: &[Chunk]) -> Vec<i16> {
    if chunks.is_empty() {
        return vec![0; FRAME];
    }
    // Group by call, preserving arrival order. At most a handful of calls are
    // ever live at once, so a linear scan beats a map.
    let mut groups: Vec<(u64, Vec<i16>)> = Vec::new();
    for chunk in chunks {
        match groups.iter_mut().find(|(call, _)| *call == chunk.call) {
            Some((_, pcm)) => pcm.extend_from_slice(&chunk.pcm),
            None => groups.push((chunk.call, chunk.pcm.clone())),
        }
    }
    if groups.len() == 1 {
        let mut pcm = groups.pop().expect("one group").1;
        pad_to_frame(&mut pcm);
        return pcm;
    }
    let longest = groups.iter().map(|(_, p)| p.len()).max().unwrap_or(0);
    let mut out = vec![0i16; longest];
    for (_, pcm) in &groups {
        for (o, s) in out.iter_mut().zip(pcm.iter()) {
            *o = o.saturating_add(*s);
        }
    }
    pad_to_frame(&mut out);
    out
}

/// Round a tick's output up to a whole number of frames, and never emit less
/// than one: the encoder is fed and the clock is advanced in frame units.
fn pad_to_frame(pcm: &mut Vec<i16>) {
    if pcm.is_empty() {
        pcm.resize(FRAME, 0);
        return;
    }
    let remainder = pcm.len() % FRAME;
    if remainder != 0 {
        pcm.resize(pcm.len() + (FRAME - remainder), 0);
    }
}

/// How far the tick clock advances for a block of `samples` samples.
///
/// The stream is paced by what it *emitted*, not by how many ticks it ran: a
/// tick that drained a backlog of five frames is five frames of audio, and
/// pretending otherwise is how a stream ends up playing faster than real time
/// and eventually running dry.
pub(crate) fn advance_for(samples: usize) -> Duration {
    TICK * (samples / FRAME).max(1) as u32
}

// ---------------------------------------------------------------------------
// The stream body
// ---------------------------------------------------------------------------

/// The `Read` half of a monitor response: ticks, mixes, encodes.
///
/// Blocking `sleep` inside `read` is exactly right here — this runs on a thread
/// of its own that exists only to serve one listener, and the alternative
/// (spinning, or an async runtime) would cost either a core or a dependency the
/// recorder deliberately does not have.
pub(crate) struct MonitorReader {
    sub: MonitorSub,
    encoder: Mp3Stream,
    /// Encoded bytes not yet handed to the socket.
    out: Vec<u8>,
    pos: usize,
    deadline: Instant,
    /// The call whose audio was last carried, so a change can be logged once
    /// rather than per frame.
    playing: Option<u64>,
}

impl MonitorReader {
    fn new(sub: MonitorSub) -> Result<Self, trunkd_call::AudioError> {
        Ok(MonitorReader {
            sub,
            encoder: Mp3Stream::new(BITRATE_KBPS)?,
            out: Vec::new(),
            pos: 0,
            deadline: Instant::now(),
            playing: None,
        })
    }

    /// One 20 ms step: wait for the deadline, drain, mix, encode.
    fn tick(&mut self) -> io::Result<()> {
        let now = Instant::now();
        if self.deadline > now {
            std::thread::sleep(self.deadline - now);
        } else if now.duration_since(self.deadline) > RESYNC_AFTER {
            // Descheduled for longer than the slack: skip the gap rather than
            // race to catch up on audio nobody can hear the point of anyway.
            self.deadline = now;
        }

        let mut chunks = Vec::new();
        while chunks.len() < DRAIN_MAX {
            match self.sub.rx.try_recv() {
                Ok(chunk) => chunks.push(chunk),
                Err(_) => break,
            }
        }
        self.note_playing(&chunks);

        let pcm = mix_tick(&chunks);
        self.deadline += advance_for(pcm.len());
        self.out.clear();
        self.pos = 0;
        self.encoder
            .encode(&pcm, &mut self.out)
            .map_err(io::Error::other)
    }

    /// Log the call being carried when it changes, and note when the stream
    /// falls back to silence.
    fn note_playing(&mut self, chunks: &[Chunk]) {
        let Some(first) = chunks.first() else {
            if self.playing.take().is_some() {
                tracing::debug!(filter = %self.sub.filter.describe(), "monitor: idle");
            }
            return;
        };
        if self.playing != Some(first.call) {
            self.playing = Some(first.call);
            tracing::debug!(
                filter = %self.sub.filter.describe(),
                call = %first.label,
                "monitor: carrying"
            );
        }
    }
}

impl Read for MonitorReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        // A tick often produces no bytes at all: an MPEG-2.5 Layer III frame is
        // 576 samples, so two ticks in three complete nothing. Returning that
        // as `Ok(0)` would tell the socket the response had ended, so keep
        // ticking — each one sleeps, so this loop runs at 50 Hz, not hot.
        while self.pos >= self.out.len() {
            self.tick()?;
        }
        let n = buf.len().min(self.out.len() - self.pos);
        buf[..n].copy_from_slice(&self.out[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

/// Build the reader for a subscription. Separate from [`MonitorSub`] so the
/// encoder's failure (which cannot happen in practice — LAME is statically
/// linked and the parameters are constants) is reported before a 200 is sent.
pub(crate) fn reader(sub: MonitorSub) -> Result<MonitorReader, trunkd_call::AudioError> {
    MonitorReader::new(sub)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(call: u64, tg: Option<u32>) -> CallAudioMeta {
        CallAudioMeta::new(
            call,
            tg,
            CallKind::Group,
            1234,
            Some("hackrf-0"),
            851_000_000,
        )
    }

    fn frame(v: i16) -> Vec<i16> {
        vec![v; FRAME]
    }

    #[test]
    fn publishing_with_no_subscribers_does_nothing() {
        let hub = MonitorHub::new();
        assert_eq!(hub.streams(), 0);
        let (meta, pcm) = (meta(1, Some(100)), frame(1));
        let ((), allocs) = crate::testalloc::count(|| hub.publish(&meta, &pcm));
        assert_eq!(
            allocs, 0,
            "the call thread pays nothing while nobody listens"
        );
        assert_eq!(hub.dropped(), 0);
        assert_eq!(hub.streams(), 0);
        // With a listener: one 160-sample copy per frame, on the call thread.
        let _sub = hub.subscribe(Filter::All).expect("a slot");
        let ((), allocs) = crate::testalloc::count(|| hub.publish(&meta, &pcm));
        assert_eq!(allocs, 1);
    }

    #[test]
    fn a_talkgroup_filter_takes_only_its_own_traffic() {
        let hub = MonitorHub::new();
        let sub = hub.subscribe(Filter::Talkgroup(100)).expect("a slot");
        hub.publish(&meta(1, Some(100)), &frame(7));
        hub.publish(&meta(2, Some(101)), &frame(9));
        hub.publish(&meta(3, None), &frame(11));
        let got = sub.rx.try_recv().expect("the matching frame");
        assert_eq!(got.pcm[0], 7);
        assert!(sub.rx.try_recv().is_err(), "the other calls leaked through");
    }

    #[test]
    fn the_unfiltered_stream_takes_everything() {
        let hub = MonitorHub::new();
        let sub = hub.subscribe(Filter::All).expect("a slot");
        hub.publish(&meta(1, Some(100)), &frame(1));
        hub.publish(&meta(2, None), &frame(2));
        assert_eq!(sub.rx.try_recv().expect("first").pcm[0], 1);
        assert_eq!(sub.rx.try_recv().expect("second").pcm[0], 2);
    }

    #[test]
    fn a_full_queue_drops_frames_and_never_blocks() {
        let hub = MonitorHub::new();
        let sub = hub.subscribe(Filter::All).expect("a slot");
        // Nobody is reading, so everything past the queue depth is dropped.
        // If this blocked the test would hang rather than fail, which is the
        // most honest possible assertion about a publish path that must not.
        for i in 0..(QUEUE_FRAMES + 50) {
            hub.publish(&meta(1, Some(100)), &frame(i as i16));
        }
        assert_eq!(hub.dropped(), 50);
        let mut received = 0;
        while sub.rx.try_recv().is_ok() {
            received += 1;
        }
        assert_eq!(received, QUEUE_FRAMES);
    }

    #[test]
    fn dropping_a_subscription_unsubscribes() {
        let hub = MonitorHub::new();
        let sub = hub.subscribe(Filter::All).expect("a slot");
        assert_eq!(hub.streams(), 1);
        drop(sub);
        assert_eq!(hub.streams(), 0);
        // And the publish path is back to its single-atomic no-op.
        hub.publish(&meta(1, Some(100)), &frame(1));
        assert_eq!(hub.dropped(), 0);
    }

    #[test]
    fn the_stream_cap_holds() {
        let hub = MonitorHub::new();
        let subs: Vec<_> = (0..MAX_STREAMS)
            .map(|_| hub.subscribe(Filter::All).expect("a slot"))
            .collect();
        assert!(hub.subscribe(Filter::All).is_none(), "cap not enforced");
        assert_eq!(hub.streams(), MAX_STREAMS);
        drop(subs);
        assert!(hub.subscribe(Filter::All).is_some(), "a slot did not free");
    }

    #[test]
    fn an_empty_tick_is_one_frame_of_silence() {
        let pcm = mix_tick(&[]);
        assert_eq!(pcm.len(), FRAME);
        assert!(pcm.iter().all(|&s| s == 0));
        assert_eq!(advance_for(pcm.len()), TICK);
    }

    #[test]
    fn one_call_passes_through_unchanged() {
        let chunks = vec![Chunk {
            call: 1,
            label: Arc::from("x"),
            pcm: frame(300),
        }];
        let pcm = mix_tick(&chunks);
        assert_eq!(pcm.len(), FRAME);
        assert!(pcm.iter().all(|&s| s == 300));
    }

    #[test]
    fn two_calls_are_summed_with_saturation() {
        let chunks = vec![
            Chunk {
                call: 1,
                label: Arc::from("a"),
                pcm: vec![i16::MAX; FRAME],
            },
            Chunk {
                call: 2,
                label: Arc::from("b"),
                pcm: vec![1000; FRAME],
            },
        ];
        let pcm = mix_tick(&chunks);
        assert_eq!(pcm.len(), FRAME);
        assert!(
            pcm.iter().all(|&s| s == i16::MAX),
            "the sum wrapped instead of saturating"
        );
    }

    #[test]
    fn a_backlog_stays_in_order_and_paces_itself() {
        // Two frames of call 1 and one of call 2 drained in one tick: call 2
        // lands on the first 20 ms, not smeared over both.
        let chunks = vec![
            Chunk {
                call: 1,
                label: Arc::from("a"),
                pcm: frame(10),
            },
            Chunk {
                call: 2,
                label: Arc::from("b"),
                pcm: frame(3),
            },
            Chunk {
                call: 1,
                label: Arc::from("a"),
                pcm: frame(20),
            },
        ];
        let pcm = mix_tick(&chunks);
        assert_eq!(pcm.len(), 2 * FRAME);
        assert_eq!(pcm[0], 13);
        assert_eq!(pcm[FRAME], 20, "the second frame picked up call 2's audio");
        // Two frames emitted means the clock moves two frames.
        assert_eq!(advance_for(pcm.len()), 2 * TICK);
    }

    #[test]
    fn a_short_block_is_padded_to_a_whole_frame() {
        let chunks = vec![Chunk {
            call: 1,
            label: Arc::from("a"),
            pcm: vec![5; 40],
        }];
        let pcm = mix_tick(&chunks);
        assert_eq!(pcm.len(), FRAME);
        assert_eq!(pcm[39], 5);
        assert_eq!(pcm[40], 0);
        assert_eq!(advance_for(pcm.len()), TICK);
    }

    #[test]
    fn the_reader_emits_mp3_and_never_reports_eof_while_idle() {
        let hub = MonitorHub::new();
        let sub = hub.subscribe(Filter::All).expect("a slot");
        let mut rd = reader(sub).expect("a LAME encoder");
        let mut buf = [0u8; 4096];
        // Silence alone must still produce frames: this blocks for a few ticks
        // and must never come back as `Ok(0)`.
        let n = rd.read(&mut buf).expect("a read");
        assert!(n > 0, "an idle monitor reported end of stream");
        assert_eq!(buf[0], 0xFF, "no mpeg frame sync: {:02X?}", &buf[..4]);
        assert_eq!(buf[1] & 0xE0, 0xE0, "no mpeg frame sync");
    }
}
