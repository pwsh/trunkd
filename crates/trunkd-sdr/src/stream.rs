// SPDX-License-Identifier: GPL-3.0-or-later
//! Ring-buffer plumbing shared by every backend.
//!
//! The reader thread owns a [`RingWriter`] and pushes batches with
//! [`RingWriter::write_batch`], which drops-and-counts on a full ring. The
//! consumer side and lifecycle live in [`TunerStream`].

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use rtrb::{Consumer, Producer};
use trunkd_core::{DeviceId, Hz, IqSample, Metric, Metrics};

use crate::device::TunerEvents;

/// Tuning knobs for [`crate::TunerDevice::start`].
#[derive(Debug, Clone, Copy)]
pub struct StreamConfig {
    /// Ring-buffer capacity in complex samples.
    pub ring_capacity: usize,
    /// Bounded event-channel capacity.
    pub event_capacity: usize,
}

impl Default for StreamConfig {
    fn default() -> Self {
        StreamConfig {
            // ~0.4 s at 2.4 Msps; generous headroom for a well-behaved DSP thread.
            ring_capacity: 1 << 20,
            event_capacity: 64,
        }
    }
}

/// Fully-qualified metric names for one device's ring.
#[derive(Debug, Clone)]
pub struct RingMetricNames {
    /// Counter: samples dropped because the ring was full.
    pub overflow: String,
    /// Counter: samples successfully delivered into the ring.
    pub produced: String,
}

/// Build the per-device ring metric names (stable across restarts).
pub fn ring_metric_names(id: &DeviceId) -> RingMetricNames {
    RingMetricNames {
        overflow: format!("sdr.{}.overflow_samples", id.as_str()),
        produced: format!("sdr.{}.produced_samples", id.as_str()),
    }
}

/// One-slot retune mailbox between the DSP thread and a backend's reader
/// thread.
///
/// Retuning has to happen on the thread that owns the device handle, but the
/// decision is made on the DSP thread, which must never block. So the request
/// is a single atomic store: the DSP thread leaves the wanted *nominal*
/// frequency here and the reader thread picks it up on its next pass. A
/// request that has not been applied yet is simply overwritten — a stale
/// frequency is never worth applying once a newer one is known — which is also
/// why one slot is enough and no queue can grow.
///
/// The frequency stored is the nominal one the caller wants to receive; the
/// backend applies its own ppm correction on top, exactly as it does at open.
/// 0 means "nothing pending" (no tuner is ever asked for DC).
pub struct RetuneSlot {
    pending: AtomicU64,
    supported: AtomicBool,
}

impl RetuneSlot {
    fn new() -> Self {
        RetuneSlot {
            pending: AtomicU64::new(0),
            supported: AtomicBool::new(false),
        }
    }

    /// Declare that a reader thread will service this slot. Called by the
    /// backend before it spawns that thread; a backend that cannot retune a
    /// running stream simply never calls it.
    pub fn mark_supported(&self) {
        self.supported.store(true, Ordering::Relaxed);
    }

    /// Whether a backend is servicing this slot.
    pub fn is_supported(&self) -> bool {
        self.supported.load(Ordering::Relaxed)
    }

    /// Ask for `nominal` on the next reader pass, replacing any request that
    /// has not been applied. Returns whether the backend supports it at all —
    /// `false` tells the caller to stop asking.
    pub fn request(&self, nominal: Hz) -> bool {
        self.pending.store(nominal.as_u64(), Ordering::Relaxed);
        self.is_supported()
    }

    /// Take the pending request, if any. Called only by the reader thread.
    pub fn take(&self) -> Option<Hz> {
        match self.pending.swap(0, Ordering::Relaxed) {
            0 => None,
            hz => Some(Hz(hz)),
        }
    }
}

/// One runtime gain/correction change, as the control plane wants it applied.
///
/// Every field is optional and means "leave this alone": a request that only
/// carries `agc` flips AGC and touches no gain. The map is by the driver's own
/// element names, exactly as `[[device]] gains` is.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GainRequest {
    /// Backend-generic overall gain in dB.
    pub gain: Option<f64>,
    /// Per-element gains, applied after `gain` so these win (the same order
    /// the open plan uses).
    pub gains: BTreeMap<String, f64>,
    /// Hardware AGC.
    pub agc: Option<bool>,
    /// Frequency correction in ppm.
    pub ppm: Option<f64>,
}

impl GainRequest {
    /// Whether this request would do nothing at all.
    pub fn is_empty(&self) -> bool {
        self == &GainRequest::default()
    }

    /// Fold `other` over this request, so two changes that arrive before the
    /// reader thread gets a pass are applied as one rather than the newer
    /// silently dropping the older's untouched fields.
    pub fn merge(&mut self, other: GainRequest) {
        if other.gain.is_some() {
            self.gain = other.gain;
        }
        for (k, v) in other.gains {
            self.gains.insert(k, v);
        }
        if other.agc.is_some() {
            self.agc = other.agc;
        }
        if other.ppm.is_some() {
            self.ppm = other.ppm;
        }
    }
}

/// One-slot gain mailbox between the control plane and a backend's reader
/// thread — the sibling of [`RetuneSlot`], and for the same reason.
///
/// Gain, AGC and ppm all have to be set on the thread that owns the device
/// handle, but the decision is made on the control thread, which must never
/// block on a reader that is inside a 250 ms `readStream`. So the request is
/// left here and picked up on the reader's next pass.
///
/// Unlike a retune, an unapplied request is *merged* rather than replaced: two
/// changes in one window are two different fields as often as they are the
/// same one, and dropping the older would silently lose a setting the operator
/// pushed.
pub struct GainSlot {
    pending: Mutex<Option<GainRequest>>,
    supported: AtomicBool,
}

impl GainSlot {
    fn new() -> Self {
        GainSlot {
            pending: Mutex::new(None),
            supported: AtomicBool::new(false),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<GainRequest>> {
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Declare that a reader thread will service this slot. Called by the
    /// backend before it spawns that thread; a backend that cannot change gain
    /// on a running stream simply never calls it.
    pub fn mark_supported(&self) {
        self.supported.store(true, Ordering::Relaxed);
    }

    /// Whether a backend is servicing this slot.
    pub fn is_supported(&self) -> bool {
        self.supported.load(Ordering::Relaxed)
    }

    /// Ask for `req` on the next reader pass, merged over anything still
    /// pending. Returns whether the backend services this at all — `false`
    /// tells the caller to restart the tuner instead.
    pub fn request(&self, req: GainRequest) -> bool {
        let mut slot = self.lock();
        match slot.as_mut() {
            Some(existing) => existing.merge(req),
            None => *slot = Some(req),
        }
        self.is_supported()
    }

    /// Take the pending request, if any. Called only by the reader thread.
    pub fn take(&self) -> Option<GainRequest> {
        self.lock().take()
    }
}

/// The producer side used by a reader thread. Drops-and-counts on overflow.
pub struct RingWriter {
    producer: Producer<IqSample>,
    overflow: Metric,
    produced: Metric,
    /// Samples ever pushed into the ring (the dropped ones excluded).
    written: u64,
}

impl RingWriter {
    /// Samples the ring can accept right now.
    ///
    /// Lets a backend that has no real-time deadline — file playback — wait for
    /// space instead of dropping into a full ring. A live tuner has nowhere to
    /// put the samples it cannot write and must keep using
    /// [`write_batch`](Self::write_batch)'s drop-and-count.
    pub fn free_slots(&self) -> usize {
        self.producer.slots()
    }

    /// Samples the consumer has taken out of the ring so far: everything
    /// written minus what still sits in it. Monotonic, and never ahead of the
    /// consumer — a sample read after this call only makes it stale-low.
    pub fn consumed(&self) -> u64 {
        self.written.saturating_sub(self.queued() as u64)
    }

    /// Samples written but not yet taken by the consumer.
    pub fn queued(&self) -> usize {
        self.producer.buffer().capacity() - self.producer.slots()
    }

    /// Push `batch` into the ring. Samples that do not fit are dropped and the
    /// overflow counter is incremented. Returns the number of dropped samples.
    pub fn write_batch(&mut self, batch: &[IqSample]) -> usize {
        let free = self.producer.slots();
        let take = free.min(batch.len());

        for &s in &batch[..take] {
            // `take <= free`, so these pushes cannot fail.
            let _ = self.producer.push(s);
        }
        if take > 0 {
            self.produced.add(take as u64);
            self.written += take as u64;
        }

        let dropped = batch.len() - take;
        if dropped > 0 {
            self.overflow.add(dropped as u64);
        }
        dropped
    }
}

/// Consumer + lifecycle handle for a running tuner.
pub struct TunerStream {
    /// Consumer side of the sample ring, drained by the DSP thread.
    pub consumer: Consumer<IqSample>,
    /// Out-of-band tuner events.
    pub events: TunerEvents,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    overflow: Metric,
    produced: Metric,
    retune: Arc<RetuneSlot>,
    gain: Arc<GainSlot>,
}

impl TunerStream {
    /// Ask the running stream to re-tune to `nominal` without restarting it.
    ///
    /// Returns whether the backend services runtime retunes at all; `false`
    /// means nothing will happen and the caller should disable whatever
    /// feature was driving it. Never blocks and never allocates — it is a
    /// single atomic store, safe to call from the DSP thread.
    pub fn retune(&self, nominal: Hz) -> bool {
        self.retune.request(nominal)
    }

    /// A handle on this stream's retune slot, for a caller that needs to ask
    /// for a retune from somewhere the stream itself is not reachable.
    pub fn retune_slot(&self) -> Arc<RetuneSlot> {
        self.retune.clone()
    }

    /// Ask the running stream to apply a gain/AGC/ppm change without
    /// restarting it. Returns whether the backend services runtime gain at
    /// all; `false` means nothing will happen and the caller must restart the
    /// tuner to apply it. Never blocks on the reader thread.
    pub fn set_gains(&self, req: GainRequest) -> bool {
        self.gain.request(req)
    }

    /// Whether this backend applies gain changes on a running stream.
    pub fn runtime_gain_supported(&self) -> bool {
        self.gain.is_supported()
    }

    /// A handle on this stream's gain slot, for a caller that cannot reach the
    /// stream itself.
    pub fn gain_slot(&self) -> Arc<GainSlot> {
        self.gain.clone()
    }

    /// Total samples dropped due to ring overflow.
    pub fn overflow_count(&self) -> u64 {
        self.overflow.get()
    }

    /// Total samples delivered into the ring.
    pub fn produced_count(&self) -> u64 {
        self.produced.get()
    }

    /// Whether the reader thread has finished.
    pub fn is_finished(&self) -> bool {
        self.handle.as_ref().is_none_or(|h| h.is_finished())
    }

    /// Signal the reader thread to stop and wait for it to exit.
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for TunerStream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// Wiring shared by backends: build the ring, metrics and stop flag. The
/// [`RingWriter`] is handed to the reader thread; [`StreamParts`] retains the
/// consumer side and produces the public [`TunerStream`] once the thread is
/// spawned.
pub(crate) struct StreamParts {
    pub consumer: Consumer<IqSample>,
    pub events: TunerEvents,
    pub stop: Arc<AtomicBool>,
    overflow: Metric,
    produced: Metric,
    retune: Arc<RetuneSlot>,
    gain: Arc<GainSlot>,
}

impl StreamParts {
    /// Build the ring and metrics, returning the writer (for the reader thread)
    /// and the retained parts (for [`StreamParts::into_stream`]).
    pub(crate) fn new(
        id: &DeviceId,
        cfg: StreamConfig,
        metrics: &Metrics,
        events: TunerEvents,
    ) -> (RingWriter, StreamParts) {
        let names = ring_metric_names(id);
        let overflow = metrics.counter(&names.overflow);
        let produced = metrics.counter(&names.produced);
        let (producer, consumer) = rtrb::RingBuffer::new(cfg.ring_capacity.max(1));
        let writer = RingWriter {
            producer,
            overflow: overflow.clone(),
            produced: produced.clone(),
            written: 0,
        };
        let parts = StreamParts {
            consumer,
            events,
            stop: Arc::new(AtomicBool::new(false)),
            overflow,
            produced,
            retune: Arc::new(RetuneSlot::new()),
            gain: Arc::new(GainSlot::new()),
        };
        (writer, parts)
    }

    /// The retune slot, for a backend to clone into its reader thread (and to
    /// [`RetuneSlot::mark_supported`]) before spawning it.
    pub(crate) fn retune_slot(&self) -> Arc<RetuneSlot> {
        self.retune.clone()
    }

    /// The gain slot, for a backend to clone into its reader thread (and to
    /// [`GainSlot::mark_supported`]) before spawning it.
    pub(crate) fn gain_slot(&self) -> Arc<GainSlot> {
        self.gain.clone()
    }

    pub(crate) fn into_stream(self, handle: JoinHandle<()>) -> TunerStream {
        TunerStream {
            consumer: self.consumer,
            events: self.events,
            stop: self.stop,
            handle: Some(handle),
            overflow: self.overflow,
            produced: self.produced,
            retune: self.retune,
            gain: self.gain,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_retune_slot_holds_one_request_and_reports_support() {
        let slot = RetuneSlot::new();
        // Nothing pending, and an unsupported backend says so on request.
        assert_eq!(slot.take(), None);
        assert!(!slot.is_supported());
        assert!(!slot.request(Hz(855_500_000)));
        // The request is still recorded, so a backend that marks itself
        // supported later still services it.
        assert_eq!(slot.take(), Some(Hz(855_500_000)));
        assert_eq!(slot.take(), None);

        slot.mark_supported();
        assert!(slot.request(Hz(855_500_000)));
        // A newer request replaces an unapplied one rather than queueing.
        assert!(slot.request(Hz(855_500_901)));
        assert_eq!(slot.take(), Some(Hz(855_500_901)));
        assert_eq!(slot.take(), None);
    }

    #[test]
    fn the_gain_slot_merges_requests_and_reports_support() {
        let slot = GainSlot::new();
        assert!(slot.take().is_none());
        assert!(!slot.is_supported());
        // An unsupported backend still records it, but says "no".
        assert!(!slot.request(GainRequest {
            gain: Some(20.0),
            ..GainRequest::default()
        }));

        slot.mark_supported();
        // A second request folds over the first rather than replacing it, so a
        // gain pushed one tick before an AGC push is not lost.
        assert!(slot.request(GainRequest {
            agc: Some(true),
            gains: BTreeMap::from([("LNA".to_owned(), 12.0)]),
            ..GainRequest::default()
        }));
        let got = slot.take().expect("a pending request");
        assert_eq!(got.gain, Some(20.0));
        assert_eq!(got.agc, Some(true));
        assert_eq!(got.gains.get("LNA"), Some(&12.0));
        assert!(slot.take().is_none());
    }

    #[test]
    fn a_reader_thread_drains_the_gain_slot_between_passes() {
        // The shape the Soapy backend uses: the slot is built by the stream,
        // the reader thread marks it supported and drains it on every pass,
        // and the control plane only ever stores into it.
        let slot = Arc::new(GainSlot::new());
        let stop = Arc::new(AtomicBool::new(false));
        let applied: Arc<std::sync::Mutex<Vec<GainRequest>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));

        let reader = {
            let (slot, stop, applied) = (slot.clone(), stop.clone(), applied.clone());
            std::thread::spawn(move || {
                slot.mark_supported();
                while !stop.load(Ordering::Relaxed) {
                    if let Some(req) = slot.take() {
                        applied.lock().expect("not poisoned").push(req);
                    }
                    // Stands in for the backend's blocking read.
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                // Whatever arrived in the last window is still applied.
                if let Some(req) = slot.take() {
                    applied.lock().expect("not poisoned").push(req);
                }
            })
        };

        // Wait for the reader to declare itself, then push two changes.
        while !slot.is_supported() {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(slot.request(GainRequest {
            gain: Some(18.0),
            ..GainRequest::default()
        }));
        assert!(slot.request(GainRequest {
            ppm: Some(-1.5),
            ..GainRequest::default()
        }));

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            let seen: Vec<GainRequest> = applied.lock().expect("not poisoned").clone();
            if seen.iter().any(|r| r.gain == Some(18.0)) && seen.iter().any(|r| r.ppm == Some(-1.5))
            {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        stop.store(true, Ordering::Relaxed);
        reader.join().expect("reader thread");

        let seen = applied.lock().expect("not poisoned").clone();
        assert!(!seen.is_empty(), "the reader drained the slot");
        // Whether the two arrived in one pass or two, both fields were applied
        // exactly once and neither was lost to the other.
        assert_eq!(
            seen.iter().filter(|r| r.gain == Some(18.0)).count(),
            1,
            "{seen:?}"
        );
        assert_eq!(
            seen.iter().filter(|r| r.ppm == Some(-1.5)).count(),
            1,
            "{seen:?}"
        );
        assert!(slot.take().is_none(), "and nothing is left behind");
    }

    #[test]
    fn an_empty_gain_request_is_recognised_as_a_no_op() {
        assert!(GainRequest::default().is_empty());
        assert!(!GainRequest {
            ppm: Some(0.5),
            ..GainRequest::default()
        }
        .is_empty());
    }
}
