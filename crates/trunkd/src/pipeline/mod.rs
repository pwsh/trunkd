// SPDX-License-Identifier: GPL-3.0-or-later
//! The running recorder, split across threads (ARCHITECTURE §2).
//!
//! ```text
//!   SDR thread            DSP thread                call thread        uplink thread
//! ┌──────────────┐ rtrb ┌──────────────────┐ chan ┌────────────────┐ chan ┌──────────┐
//! │ hackrf/iqfile├─────▶│ SiteEngine ×site ├─────▶│ IMBE → PCM →   ├─────▶│ HTTP +   │
//! │ drop+count   │ ring │ channelizer      │ 8192 │ CallBuilder    │  64  │ spool    │
//! └──────────────┘      └────────┬─────────┘      └────────────────┘      └──────────┘
//!                                │ chan 4096
//!                                ▼
//!                       ┌──────────────────┐
//!                       │ observations     │  SiteInventory + a POST every
//!                       │ thread           │  `observations_interval_s`
//!                       └──────────────────┘
//! ```
//!
//! Every queue is bounded and drop-oldest ([`trunkd_core::chan::bounded`]), so
//! a stalled consumer costs samples or events and never memory. Nothing on the
//! DSP thread allocates per sample or touches the network.
//!
//! # Time
//!
//! [`SiteEngine`](trunkd_p25::SiteEngine) reports *stream* time: consumed
//! samples × 1000 / sample rate. Each device's DSP thread stamps the wall clock
//! into its own [`StreamEpoch`] when the first block arrives, and maps every
//! event's stream `at_ms` to wall clock (`epoch + at_ms`) **before** it enqueues
//! the message. The `at_ms` a [`CallMsg`] or [`ObsMsg`] carries is therefore
//! already wall-clock milliseconds, and the call/observation threads use it
//! directly. One epoch per device means a two-tuner run has one timeline per
//! tuner, converted at the source, so calls from different tuners share a wall
//! clock without sharing a stream clock.
//!
//! A sample counter cannot see samples that never arrived, so a host stall
//! leaves the epoch permanently early (2026-09-08: an OOM stall cost ~12 s of
//! samples and every timestamp for the next 7.7 h was 11.6 s early). Each DSP
//! thread therefore checks its own estimate against the wall clock once a
//! second and re-anchors when they disagree by more than a threshold — see
//! [`streamclock`] for the rule and its baseline.

pub(crate) mod autoppm;
pub(crate) mod calls;
pub(crate) mod control;
pub(crate) mod coordinator;
pub(crate) mod device;
pub(crate) mod devicecfg;
pub(crate) mod eventarchive;
pub(crate) mod eventlog;
pub(crate) mod failrun;
pub(crate) mod link;
pub(crate) mod lrrp_gate;
pub(crate) mod monitor;
pub(crate) mod nonvoice;
pub(crate) mod observations;
pub(crate) mod panel;
pub(crate) mod panelhttp;
pub mod ppmmemory;
pub(crate) mod priority;
pub(crate) mod spectrum;
pub(crate) mod stream;
pub(crate) mod streamclock;

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use trunkd_p25::call::CallRecord;
use trunkd_p25::event::CallKind;
use trunkd_p25::site::{CallId, EndReason};

/// The `origin_device` a grant carries when it came from somewhere that is not
/// one of this process's tuners — a peer recorder over the `[link]`. No route
/// matches it, which is exactly right: the coordinator's local-preference
/// tie-break has nothing to prefer for a grant heard on another box.
pub(crate) const ORIGIN_NONE: usize = usize::MAX;

/// Wall-clock origin of a device's sample stream, published by its DSP thread
/// and read by the call and observation threads.
///
/// Zero until the first block of samples is drained; every reader treats zero
/// as "not yet known" and falls back to the current time.
#[derive(Debug, Default)]
pub(crate) struct StreamEpoch(AtomicI64);

impl StreamEpoch {
    /// Stamp the epoch, unless it is already set.
    ///
    /// Idempotent so a supervised restart of the DSP thread does not shift a
    /// timeline that other threads have already used.
    pub(crate) fn set_once(&self, unix_ms: i64) {
        let _ = self
            .0
            .compare_exchange(0, unix_ms, Ordering::Relaxed, Ordering::Relaxed);
    }

    /// Reset so the next block re-stamps the epoch (a device restart: the new
    /// stream's time base starts again at zero).
    pub(crate) fn clear(&self) {
        self.0.store(0, Ordering::Relaxed);
    }

    /// Move the epoch by `delta_ms`, re-anchoring the whole timeline.
    ///
    /// The self-correcting stream clock's one write (see
    /// [`streamclock`]): a positive delta pushes every future timestamp later,
    /// which is what a stream that fell behind wall time needs. A no-op while
    /// the epoch is unset — there is no timeline yet to move, and the first
    /// block is about to stamp one from the current clock anyway.
    ///
    /// Applied at a block boundary, so timestamps stay monotonic within a call:
    /// a call already on the air keeps the start it was given and only its end
    /// moves, making a call that spans a resync up to one correction long or
    /// short.
    pub(crate) fn shift(&self, delta_ms: i64) {
        let mut cur = self.0.load(Ordering::Relaxed);
        while cur != 0 {
            match self.0.compare_exchange_weak(
                cur,
                cur.saturating_add(delta_ms),
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return,
                Err(seen) => cur = seen,
            }
        }
    }

    /// Unix milliseconds for a stream time, or `now` while the epoch is unset.
    pub(crate) fn wall_ms(&self, at_ms: u64) -> i64 {
        match self.0.load(Ordering::Relaxed) {
            0 => trunkd_core::Timestamp::now().unix_millis(),
            e => e.saturating_add(at_ms as i64),
        }
    }
}

/// What the DSP thread hands the call thread.
///
/// A projection of [`SiteEvent`](trunkd_p25::site::SiteEvent) rather than the
/// enum itself: the call thread has no use for the control-channel
/// pass-through or the stats variant, and leaving them out keeps the queued
/// element small enough that 8 192 of them are cheap.
#[derive(Debug)]
pub(crate) enum CallMsg {
    /// A traffic channel started following a grant.
    Started {
        /// Which site on this device.
        site: usize,
        /// Engine-assigned call id (unique per site).
        id: CallId,
        /// Downlink frequency.
        frequency_hz: u64,
        /// Talkgroup, 0 for unit-to-unit / telephone.
        talkgroup: u16,
        /// Full 24-bit TO address.
        target: u32,
        /// Source radio at grant time (0 when unknown).
        source: u32,
        /// Group / unit-to-unit / telephone.
        kind: CallKind,
        /// Patch member talkgroups when the call is on an active supergroup;
        /// empty otherwise.
        patches: Vec<u32>,
        /// The recording device's `device.{id}.rssi_dbfs` gauge, so the call
        /// thread can read the wideband level at *call end* without knowing
        /// which tuner the call came from. A cloned handle to the same atomic
        /// cell, not a copy of its value.
        rssi: Option<trunkd_core::Metric>,
        /// Stream time.
        at_ms: u64,
    },
    /// The talker changed, or its alias became known.
    Talker {
        /// Which site.
        site: usize,
        /// Call id.
        id: CallId,
        /// Current source radio.
        source: u32,
        /// Emergency flag seen so far.
        emergency: bool,
        /// L3Harris talker alias.
        alias: Option<String>,
        /// WACN of this talker, when a Source ID Extension qualified it.
        source_wacn: Option<u32>,
        /// Home system id of this talker, from the same word.
        source_system: Option<u16>,
        /// Stream time.
        at_ms: u64,
    },
    /// One raw 144-bit IMBE frame.
    Voice {
        /// Which site.
        site: usize,
        /// Call id.
        id: CallId,
        /// The 144 bits, MSB-first in 18 bytes.
        frame: [u8; 18],
    },
    /// Encryption parameters announced (HDU or Encryption Sync).
    Encryption {
        /// Which site.
        site: usize,
        /// Call id.
        id: CallId,
        /// Algorithm id; `0x80` is clear.
        algid: u8,
        /// Key id.
        key_id: u16,
    },
    /// The call ended; `record` is the engine's assembled metadata.
    Ended {
        /// Which site.
        site: usize,
        /// Call id.
        id: CallId,
        /// Assembled metadata.
        record: Box<CallRecord>,
        /// Stream time.
        at_ms: u64,
        /// Why it ended.
        reason: EndReason,
    },
}

/// What the DSP thread hands the observations thread: one signaling event,
/// with the channel it was heard on and the stream time of its block.
///
/// Traffic-channel events ride the same queue as control ones so the event log
/// sees a single ordered stream; `traffic_hz` is what tells them apart. Voice
/// never appears here — the engine drops IMBE frames from its traffic
/// pass-through, so a vocoder frame cannot reach this thread at all.
#[derive(Debug)]
pub(crate) struct ObsMsg {
    /// Which site on this device.
    pub(crate) site: usize,
    /// The tuner (device unit) whose engine heard it. A site served by two
    /// tuners has two engines decoding the same control channel; the
    /// observations thread keeps one of their streams and drops the other.
    pub(crate) tuner: usize,
    /// Frequency the control decoder is tuned to (0 while hunting).
    pub(crate) control_hz: u64,
    /// Downlink frequency when this came off a **traffic** channel; 0 for a
    /// control-channel event.
    pub(crate) traffic_hz: u64,
    /// Stream time at the end of the block that produced the event.
    pub(crate) at_ms: u64,
    /// The event itself.
    pub(crate) event: trunkd_p25::P25Event,
}

/// How many private calls the panel remembers.
///
/// A handful, not a history: the site produces ~45-60 a day, and the panel's
/// job is "what is happening now", not an archive — that is Keyup's.
const PRIVATE_CALL_ROWS: usize = 16;

/// One unit-to-unit call, as the panel shows it.
#[derive(Clone, Debug)]
pub(crate) struct PrivateCallRow {
    /// Engine call id, so the end can find the row the start made.
    pub(crate) id: u64,
    /// The talking unit (A).
    pub(crate) source: u32,
    /// The unit being called (B).
    pub(crate) target: u32,
    /// The granted channel.
    pub(crate) frequency_hz: u64,
    /// Call start, unix milliseconds.
    pub(crate) started_at_ms: i64,
    /// Call end, unix milliseconds; `None` while it is still on the air.
    pub(crate) ended_at_ms: Option<i64>,
}

/// The last few private calls, newest last, for the panel's live view.
///
/// A group call is already visible as a talkgroup on the sites table; a
/// private call has no talkgroup to appear under, so without this the panel
/// could say only how many there had been.
#[derive(Debug, Default)]
pub(crate) struct PrivateCallLog(std::sync::Mutex<std::collections::VecDeque<PrivateCallRow>>);

impl PrivateCallLog {
    /// Record a private call that has just started.
    pub(crate) fn started(&self, id: u64, source: u32, target: u32, frequency_hz: u64, at: i64) {
        let Ok(mut rows) = self.0.lock() else { return };
        while rows.len() >= PRIVATE_CALL_ROWS {
            rows.pop_front();
        }
        rows.push_back(PrivateCallRow {
            id,
            source,
            target,
            frequency_hz,
            started_at_ms: at,
            ended_at_ms: None,
        });
    }

    /// Close the row a matching [`Self::started`] opened. Unknown ids are
    /// ignored: the row may already have aged out of the window.
    pub(crate) fn ended(&self, id: u64, at: i64) {
        let Ok(mut rows) = self.0.lock() else { return };
        if let Some(row) = rows.iter_mut().rev().find(|r| r.id == id) {
            row.ended_at_ms = Some(at);
        }
    }

    /// The window, newest first.
    pub(crate) fn rows(&self) -> Vec<PrivateCallRow> {
        let Ok(rows) = self.0.lock() else {
            return Vec::new();
        };
        rows.iter().rev().cloned().collect()
    }
}

/// How many finished calls the panel's Activity view remembers.
///
/// A minute or two of a busy site: enough that an operator glancing at the
/// page sees the traffic that just went by, small enough that the ring is
/// copied under the lock in microseconds. The archive is Keyup's job.
const CALL_ROWS: usize = 60;

/// One call — group, unit-to-unit or telephone — as the Activity view shows it.
#[derive(Clone, Debug)]
pub(crate) struct CallRow {
    /// Engine call id, so the later messages find the row the grant made.
    pub(crate) id: u64,
    /// `system/site` display of the site that granted it.
    pub(crate) site: String,
    /// Wire spelling of the kind: `group`, `unit` or `telephone`.
    pub(crate) kind: &'static str,
    /// Talkgroup for a group call; `None` for the unit and telephone kinds,
    /// which are filed under the radios instead.
    pub(crate) talkgroup: Option<u32>,
    /// L3Harris talker alias, once one has been heard.
    pub(crate) alias: Option<String>,
    /// The talking radio, 0 until one is reported.
    pub(crate) source: u32,
    /// The called radio, for a unit-to-unit call.
    pub(crate) target: Option<u32>,
    /// The granted channel.
    pub(crate) frequency_hz: u64,
    /// Whether the call was ever announced as enciphered (it latches).
    pub(crate) encrypted: bool,
    /// Call start, unix milliseconds.
    pub(crate) started_at_ms: i64,
    /// Call end, unix milliseconds; `None` while it is still on the air.
    pub(crate) ended_at_ms: Option<i64>,
    /// Voice frames seen, encrypted ones included.
    pub(crate) voice_frames: u32,
    /// Why the engine closed it.
    pub(crate) reason: Option<&'static str>,
    /// What this recorder did with the finished call: `delivered` (handed to
    /// the uplink), `no voice`, `dropped` or `no audio`. What the *uplink*
    /// then did with it is not here — see the module note on `outcome`.
    pub(crate) outcome: Option<&'static str>,
}

/// What the call thread reports at a grant.
#[derive(Clone, Debug)]
pub(crate) struct CallStart {
    /// Engine call id.
    pub(crate) id: u64,
    /// `system/site` display.
    pub(crate) site: String,
    /// Wire spelling of the kind.
    pub(crate) kind: &'static str,
    /// Talkgroup, for a group call.
    pub(crate) talkgroup: Option<u32>,
    /// Talking radio, 0 when the grant named none.
    pub(crate) source: u32,
    /// Called radio, for a unit-to-unit call.
    pub(crate) target: Option<u32>,
    /// Granted channel.
    pub(crate) frequency_hz: u64,
    /// Call start, unix milliseconds.
    pub(crate) started_at_ms: i64,
}

/// Every call this recorder is on or has just been on, for the panel's
/// Activity view.
///
/// Two halves under one lock: the calls currently up (so the page can show
/// "on air" in real time rather than only at teardown) and a bounded ring of
/// the ones that have finished, newest last. The lock is held for the length
/// of a push or a field update — microseconds — because the writer is the call
/// thread, which must never wait on an HTTP reader.
///
/// The `outcome` recorded here is this thread's own disposition. The uplink
/// worker's verdict (uploaded / spooled / duplicate) happens in another crate
/// on another thread and is deliberately not plumbed back.
#[derive(Debug, Default)]
pub(crate) struct CallLog(std::sync::Mutex<CallLogInner>);

/// The two halves [`CallLog`] keeps.
#[derive(Debug, Default)]
struct CallLogInner {
    /// Calls on the air right now, in grant order.
    active: Vec<CallRow>,
    /// The last [`CALL_ROWS`] finished calls, oldest first.
    recent: std::collections::VecDeque<CallRow>,
}

impl CallLog {
    /// Record a call that has just been granted.
    pub(crate) fn started(&self, start: CallStart) {
        let Ok(mut inner) = self.0.lock() else { return };
        // A re-grant of a live id (the engine replacing a call in place) keeps
        // one row rather than growing a second: the panel would otherwise show
        // a call that never ends.
        inner.active.retain(|r| r.id != start.id);
        inner.active.push(CallRow {
            id: start.id,
            site: start.site,
            kind: start.kind,
            talkgroup: start.talkgroup,
            alias: None,
            source: start.source,
            target: start.target,
            frequency_hz: start.frequency_hz,
            encrypted: false,
            started_at_ms: start.started_at_ms,
            ended_at_ms: None,
            voice_frames: 0,
            reason: None,
            outcome: None,
        });
    }

    /// The talker changed, or its alias became known.
    pub(crate) fn talker(&self, id: u64, source: u32, alias: Option<&str>) {
        let Ok(mut inner) = self.0.lock() else { return };
        if let Some(row) = inner.active.iter_mut().find(|r| r.id == id) {
            if source != 0 {
                row.source = source;
            }
            if let Some(a) = alias {
                row.alias = Some(a.to_owned());
            }
        }
    }

    /// The call was announced as enciphered. Latches, as the call thread's own
    /// flag does.
    pub(crate) fn encrypted(&self, id: u64) {
        let Ok(mut inner) = self.0.lock() else { return };
        if let Some(row) = inner.active.iter_mut().find(|r| r.id == id) {
            row.encrypted = true;
        }
    }

    /// Close the row a matching [`Self::started`] opened, moving it to the
    /// recent ring. Unknown ids are ignored: the grant may predate this log.
    pub(crate) fn ended(&self, id: u64, at: i64, voice_frames: u32, reason: &'static str) {
        let Ok(mut inner) = self.0.lock() else { return };
        let Some(pos) = inner.active.iter().position(|r| r.id == id) else {
            return;
        };
        let mut row = inner.active.remove(pos);
        row.ended_at_ms = Some(at);
        row.voice_frames = voice_frames;
        row.reason = Some(reason);
        while inner.recent.len() >= CALL_ROWS {
            inner.recent.pop_front();
        }
        inner.recent.push_back(row);
    }

    /// Record what this recorder did with a call it has just closed.
    pub(crate) fn outcome(&self, id: u64, outcome: &'static str) {
        let Ok(mut inner) = self.0.lock() else { return };
        if let Some(row) = inner.recent.iter_mut().rev().find(|r| r.id == id) {
            row.outcome = Some(outcome);
        }
    }

    /// The calls on the air, in grant order.
    pub(crate) fn active(&self) -> Vec<CallRow> {
        let Ok(inner) = self.0.lock() else {
            return Vec::new();
        };
        inner.active.clone()
    }

    /// The finished calls, newest first.
    pub(crate) fn recent(&self) -> Vec<CallRow> {
        let Ok(inner) = self.0.lock() else {
            return Vec::new();
        };
        inner.recent.iter().rev().cloned().collect()
    }
}

/// Immutable per-site description shared by every thread.
#[derive(Debug, Clone)]
pub(crate) struct SiteSpec {
    /// System label sent to Keyup (`systemLabel`).
    pub(crate) system_label: String,
    /// Site label recorded on every call.
    pub(crate) site_label: String,
    /// Control candidates, as configured.
    pub(crate) control_frequencies: Vec<u64>,
    /// Metric name prefix, e.g. `site.0`.
    pub(crate) metric_prefix: String,
    /// Human-readable `system/site` for logs.
    pub(crate) display: String,
}

/// Shared handle bundle handed to each thread at construction.
///
/// The stream epoch is **not** here: it is per-device now (a multi-device run
/// has one timeline per tuner), so each [`DeviceUnit`](device::DeviceUnit) owns
/// its own [`StreamEpoch`] and stamps wall-clock times into the messages it
/// enqueues. The call and observation threads therefore receive wall-clock
/// times directly and need no epoch of their own.
#[derive(Clone)]
pub(crate) struct Shared {
    /// Metrics registry (also the status line's data source).
    pub(crate) metrics: trunkd_core::Metrics,
    /// Per-site descriptions, indexed by the `site` field of the messages.
    pub(crate) sites: Arc<Vec<SiteSpec>>,
    /// The last few unit-to-unit calls, for the panel.
    pub(crate) private_calls: Arc<PrivateCallLog>,
    /// Every call, on the air and just finished, for the panel's Activity
    /// view. A superset of `private_calls`, which keeps its own shape because
    /// the panel's private-call section is about the pair of radios rather
    /// than about traffic.
    pub(crate) calls: Arc<CallLog>,
    /// Talkgroups this recorder refuses to record at all: `[recorder]
    /// skip_talkgroups` unioned with the list keyup pushes. Lives here because
    /// every DSP thread needs the same one — the site engines test each grant
    /// against it — while the control thread is what writes it.
    pub(crate) skip: Arc<trunkd_core::SkipList>,
    /// The one clock every unthrottled `iqfile` tuner of this run replays on
    /// (see [`trunkd_sdr::Lockstep`]): two tuners reading captures of the same
    /// air stay within 50 ms of each other, so a grant one of them decodes
    /// reaches the other at the same moment of *its* file. Unused by live
    /// tuners and by real-time playback.
    pub(crate) replay_clock: Arc<trunkd_sdr::Lockstep>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_is_stamped_once_and_maps_stream_time() {
        let e = StreamEpoch::default();
        e.set_once(1_700_000_000_000);
        e.set_once(1_900_000_000_000);
        assert_eq!(e.wall_ms(0), 1_700_000_000_000, "the first stamp wins");
        assert_eq!(e.wall_ms(1_500), 1_700_000_001_500);
    }

    #[test]
    fn an_unset_epoch_answers_with_the_current_time() {
        let e = StreamEpoch::default();
        let now = trunkd_core::Timestamp::now().unix_millis();
        let got = e.wall_ms(60_000);
        assert!(
            (got - now).abs() < 5_000,
            "an unstamped epoch must not project stream time into the future"
        );
    }

    #[test]
    fn shifting_re_anchors_the_whole_timeline() {
        let e = StreamEpoch::default();
        e.set_once(1_700_000_000_000);
        e.shift(12_000);
        assert_eq!(e.wall_ms(0), 1_700_000_012_000, "every future stamp moves");
        e.shift(-2_000);
        assert_eq!(e.wall_ms(1_000), 1_700_000_011_000);
    }

    #[test]
    fn shifting_an_unset_epoch_does_nothing() {
        let e = StreamEpoch::default();
        e.shift(12_000);
        // Still unset: the next block stamps it from the clock, unshifted.
        e.set_once(1_700_000_000_000);
        assert_eq!(e.wall_ms(0), 1_700_000_000_000);
    }

    #[test]
    fn clearing_lets_a_restarted_stream_restamp() {
        let e = StreamEpoch::default();
        e.set_once(1_700_000_000_000);
        e.clear();
        e.set_once(1_800_000_000_000);
        assert_eq!(e.wall_ms(0), 1_800_000_000_000);
    }
}
