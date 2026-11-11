// SPDX-License-Identifier: GPL-3.0-or-later
//! `trunkd-uplink` — delivery of finished calls to Keyup (ARCHITECTURE §3).
//!
//! One [`KeyupUplink`] owns one worker thread. The DSP side hands it finished
//! calls with [`KeyupUplink::submit`], which is non-blocking and bounded: the
//! queue is a `trunkd-core` bounded channel, so a stalled uplink can never
//! stall a decoder and can never grow the process (ARCHITECTURE §2). Everything
//! slow — MP3 encoding, HTTP, disk — happens on the worker, except for a call
//! the queue has no room for: that one is spooled on the submitting thread
//! rather than dropped.
//!
//! # The Keyup contract
//!
//! `POST /api/call-upload`, one `multipart/form-data` request per call, in the
//! Rdio-Scanner dialect that Keyup's `app.api.call_upload` implements. The
//! exact field set is documented on [`build_multipart`]. Two details are worth
//! repeating:
//!
//! * `systemLabel` is what Keyup files the call under; the numeric `system` id
//!   is accepted and ignored.
//! * Sending `sources`/`frequencies` selects the **trunk-recorder dialect**,
//!   which is what makes `patches` a bare member list rather than sdrtrunk's
//!   supergroup-first list. trunkd always sends them.
//!
//! Keyup replies with a literal body, and that body — not the status alone —
//! decides what happens next:
//!
//! | reply | meaning | what the uplink does |
//! |---|---|---|
//! | 200 `Call imported successfully.` | stored | count `uplink.uploaded` |
//! | 200 `duplicate call rejected` | already had it | count `uplink.duplicates`, log |
//! | 200/400 `incomplete call data: …` | the call will never be accepted | log at error, count `uplink.failed`, **do not spool** |
//! | 401/403, `invalid key` | misconfiguration | log once at error, back off (default 60 s), spool |
//! | 429, 507 | flow control | spool, pause for `Retry-After` |
//! | 408, 5xx, timeout, connection refused | Keyup is down or hung | spool and retry; open the circuit |
//! | any other status (404 from a proxy, 409, 413, 422, …) | unknown | spool and retry; quarantine if it keeps failing alone |
//!
//! Only the rows Keyup documents as final delete a call; everything else is
//! kept. The full table, and the one function that implements it, is
//! `worker::classify_with_retry_after`. While the circuit is open (Keyup timing
//! out or answering 5xx), live calls are spooled without a request; the next
//! drain that gets a call through closes it. See the `worker` module.
//!
//! An **encrypted call carries no audio**: no `audio` part is sent, while
//! `audioName`, `audioType` and `encrypted=1` still are. Keyup is being
//! extended to accept that; until it is, it answers `incomplete call data: no
//! audio`, which lands in the permanent-failure row above — the call is logged
//! and dropped rather than spooled forever.
//!
//! # Offline spool
//!
//! ```text
//! <spool_dir>/YYYY/MM/DD/<system>_<talkgroup>_<start-unix-ms>.mp3
//! <spool_dir>/YYYY/MM/DD/<system>_<talkgroup>_<start-unix-ms>.json
//! ```
//!
//! Audio plus a trunk-recorder-style JSON sidecar
//! ([`trunkd_call::sidecar_json`]) — the format Keyup's *file* ingest already
//! reads, so a spool directory is a valid archive on its own. Files are
//! written temp-and-renamed (crash-safe), a drain pass retries oldest-first
//! every [`UplinkConfig::drain_interval`], successful calls are deleted, and
//! the spool is capped at [`UplinkConfig::spool_cap_bytes`] with oldest-first
//! eviction. A spooled call that keeps failing while Keyup takes the others is
//! moved to `<spool_dir>/quarantine/` (still under the cap, evicted first).
//! See the [`spool`] module for the details.
//!
//! The cap is only half of the storage story. It bounds what *trunkd* keeps; it
//! cannot stop the volume filling up from anything else on the box, and a full
//! volume loses calls that were already decoded. So the same drain timer also
//! enforces a **free-space floor** ([`UplinkConfig::spool_min_free_bytes`]):
//! whenever the filesystem holding the spool has less free space than that, the
//! oldest spooled calls are evicted until it does — even when nothing new has
//! been spooled. See the [`disk`] module.
//!
//! # Metrics
//!
//! Registered on the [`Metrics`](trunkd_core::Metrics) registry passed to
//! [`KeyupUplink::start`]: `uplink.uploaded`, `uplink.duplicates`,
//! `uplink.failed`, `uplink.spooled`, `uplink.spool_bytes` (gauge),
//! `uplink.disk_free_bytes` (gauge, refreshed on every sweep),
//! `uplink.spool_evicted` (cap- and floor-driven evictions alike),
//! `uplink.spooled_on_pressure` (spooled without an attempt: the queue was
//! full or the circuit open), `uplink.spool_quarantined`,
//! `uplink.circuit_open` (gauge, 0/1) and `uplink.queue_dropped` (calls lost
//! at submit: the overflow could not be written).
//!
//! ```no_run
//! use std::time::Duration;
//! use trunkd_core::Metrics;
//! use trunkd_uplink::{KeyupUplink, UplinkConfig};
//!
//! let metrics = Metrics::new();
//! let uplink = KeyupUplink::start(
//!     UplinkConfig {
//!         url: "http://192.0.2.60/api/call-upload".into(),
//!         api_key: "kup_…".into(),
//!         spool_dir: "/var/lib/trunkd/spool".into(),
//!         ..UplinkConfig::default()
//!     },
//!     &metrics,
//! )?;
//! # let call: trunkd_call::FinishedCall = unimplemented!();
//! uplink.submit(call);
//! # Ok::<_, trunkd_core::Error>(())
//! ```

#![deny(missing_docs)]

pub mod broadcastify;
pub mod disk;
pub mod events;
pub mod hold;
mod multipart;
pub mod spool;
mod worker;

use std::path::PathBuf;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use crossbeam_channel::TrySendError;
use trunkd_call::{AudioFormat, FinishedCall};
use trunkd_core::chan::{self, BoundedSender};
use trunkd_core::{Error, Metrics, Result};

pub use broadcastify::{BroadcastifyDirect, BroadcastifyState, BroadcastifyUploader};
pub use disk::{available_space, FreeSpaceFn, Sweep};
pub use hold::{HoldSource, UplinkHold};
pub use multipart::{build_form, build_multipart, build_multipart_with_priority};
pub use spool::{call_from_sidecar, Spool, SpoolEntry, QUARANTINE_DIR};

/// Outcome of [`KeyupUplink::submit`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Submitted {
    /// Queued for the worker.
    Queued,
    /// The queue was full (or the worker gone), so the call was written to
    /// the spool instead; the next drain uploads it.
    Spooled,
    /// Neither: the spool write failed too. Counted in `uplink.queue_dropped`.
    Dropped,
}

/// How to reach Keyup, and what to do when it cannot be reached.
#[derive(Clone, Debug)]
pub struct UplinkConfig {
    /// Full endpoint URL, e.g. `http://192.0.2.60/api/call-upload`.
    pub url: String,
    /// API token with Keyup's `upload` scope.
    pub api_key: String,
    /// Numeric system id sent as `system`. Keyup files calls under
    /// `systemLabel` instead, so any stable number will do.
    pub system_id: u32,
    /// Root of the offline spool.
    pub spool_dir: PathBuf,
    /// Maximum bytes the spool may occupy; oldest calls are evicted first.
    pub spool_cap_bytes: u64,
    /// Free-space floor for the volume holding the spool. Whenever the
    /// filesystem has less than this available, the oldest spooled calls are
    /// evicted until it does — regardless of whether the spool is anywhere near
    /// [`UplinkConfig::spool_cap_bytes`], because the space is usually being
    /// eaten by something other than trunkd. 0 disables the floor.
    pub spool_min_free_bytes: u64,
    /// Timeout for one upload, connect through response. Connecting alone is
    /// held to a few seconds of it.
    pub timeout: Duration,
    /// Depth of the bounded submit queue. A call that finds it full is
    /// spooled instead.
    pub queue_capacity: usize,
    /// How often the spool is retried.
    pub drain_interval: Duration,
    /// How long to wait after `invalid key` before trying again.
    pub auth_backoff: Duration,
    /// How call audio is encoded for upload and for the spool.
    pub audio_format: AudioFormat,
    /// Ingestion priority class sent as the `priority` form field
    /// (`urgent` | `live` | `backlog` | `bulk`, or an integer 1–999). Sending
    /// it opts in to Keyup's flow-control contract: a full queue answers
    /// `429`/`507` with `Retry-After`, which the worker honours by spooling and
    /// pausing. Live recorders send `live`.
    pub priority: String,
    /// The runtime "hold uploads" switch (`[uplink] spool_only` at boot).
    ///
    /// While it is set the worker never opens a socket: every call it takes off
    /// the queue is written to the spool instead, and the drain pass is
    /// skipped. Clearing it resumes uploads and lets the next drain empty the
    /// backlog through the ordinary reconnect path.
    pub hold: Arc<UplinkHold>,
    /// Direct-to-Broadcastify delivery: the block keyup pushes, the counters,
    /// and the last response. Shared with the control thread and the panel.
    ///
    /// Always present; whether anything happens is entirely decided by what is
    /// (or is not) in it. See [`broadcastify`].
    pub broadcastify: Arc<BroadcastifyState>,
    /// This recorder's id, sent on the Broadcastify claim.
    pub instance_id: String,
}

impl Default for UplinkConfig {
    fn default() -> Self {
        UplinkConfig {
            url: "http://127.0.0.1/api/call-upload".into(),
            api_key: String::new(),
            system_id: 1,
            spool_dir: PathBuf::from("spool"),
            spool_cap_bytes: 1024 * 1024 * 1024,
            spool_min_free_bytes: 2 * 1024 * 1024 * 1024,
            timeout: Duration::from_secs(30),
            queue_capacity: 64,
            drain_interval: Duration::from_secs(30),
            auth_backoff: Duration::from_secs(60),
            audio_format: AudioFormat::default(),
            priority: "live".into(),
            hold: Arc::new(UplinkHold::new(false)),
            broadcastify: BroadcastifyState::new(false, &Metrics::new()),
            instance_id: String::new(),
        }
    }
}

/// A point-in-time reading of the uplink's counters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UplinkStats {
    /// Calls Keyup stored.
    pub uploaded: u64,
    /// Calls Keyup already had (still a success).
    pub duplicates: u64,
    /// Calls Keyup will never accept, plus local encode/spool failures.
    pub failed: u64,
    /// Calls written to the offline spool.
    pub spooled: u64,
    /// Bytes the spool currently occupies.
    pub spool_bytes: u64,
    /// Spooled calls dropped because the spool hit its cap or the volume fell
    /// below its free-space floor.
    pub spool_evicted: u64,
    /// Free bytes on the volume holding the spool, as of the last sweep. 0
    /// means "not measured" — an unsupported platform, or a failing free-space
    /// call.
    pub disk_free_bytes: u64,
    /// Calls lost at submit: the queue was full and the spool write failed.
    pub queue_dropped: u64,
    /// Calls spooled without an upload attempt because the queue was full or
    /// the circuit was open (Keyup failing). Included in `spooled`.
    pub spooled_on_pressure: u64,
    /// Spooled calls moved to `<spool>/quarantine/` after failing repeatedly
    /// while Keyup took others.
    pub quarantined: u64,
    /// Whether live calls are going straight to the spool right now.
    pub circuit_open: bool,
}

/// The uplink: a bounded queue and the worker thread that drains it.
///
/// Dropping it (or calling [`KeyupUplink::shutdown`]) closes the queue and the
/// worker exits after finishing whatever it has already taken.
pub struct KeyupUplink {
    tx: BoundedSender<FinishedCall>,
    handle: JoinHandle<()>,
    metrics: worker::UplinkMetrics,
    overflow: worker::Overflow,
}

impl KeyupUplink {
    /// Start the uplink: open the spool and spawn the worker thread.
    ///
    /// Fails only if the configuration is unusable (empty URL) or the spool
    /// directory cannot be created.
    pub fn start(cfg: UplinkConfig, metrics: &Metrics) -> Result<Self> {
        if cfg.url.trim().is_empty() {
            return Err(Error::config("uplink url is empty"));
        }
        let spool = Spool::open(cfg.spool_dir.clone(), cfg.spool_cap_bytes)?
            .with_min_free_bytes(cfg.spool_min_free_bytes);
        let m = worker::UplinkMetrics::new(metrics);
        let (tx, rx) = chan::bounded::<FinishedCall>(cfg.queue_capacity);
        let overflow = worker::Overflow {
            spool: spool.clone(),
            format: cfg.audio_format,
            metrics: m.clone(),
        };
        let worker = worker::Worker::new(cfg, spool, m.clone());
        let handle = std::thread::Builder::new()
            .name("trunkd-uplink".into())
            .spawn(move || worker.run(rx))
            .map_err(Error::Io)?;
        Ok(KeyupUplink {
            tx,
            handle,
            metrics: m,
            overflow,
        })
    }

    /// Hand a finished call to the uplink.
    ///
    /// Never blocks on the network and never grows the queue. If the worker is
    /// behind and the queue is full, the call is written to the spool right
    /// here (encode plus two file writes) and the next drain uploads it: the
    /// spool, not the floor, is the overflow. The calls already queued keep
    /// their place, so the live path stays live.
    pub fn submit(&self, call: FinishedCall) -> Submitted {
        let (call, why) = match self.tx.try_send(call) {
            Ok(()) => return Submitted::Queued,
            Err(TrySendError::Full(call)) => (call, "upload queue full"),
            Err(TrySendError::Disconnected(call)) => (call, "uplink worker gone"),
        };
        if self.overflow.store(&call, why) {
            Submitted::Spooled
        } else {
            Submitted::Dropped
        }
    }

    /// Read the counters.
    pub fn stats(&self) -> UplinkStats {
        UplinkStats {
            uploaded: self.metrics.uploaded.get(),
            duplicates: self.metrics.duplicates.get(),
            failed: self.metrics.failed.get(),
            spooled: self.metrics.spooled.get(),
            spool_bytes: self.metrics.spool_bytes.get(),
            spool_evicted: self.metrics.spool_evicted.get(),
            disk_free_bytes: self.metrics.disk_free_bytes.get(),
            queue_dropped: self.metrics.queue_dropped.get(),
            spooled_on_pressure: self.metrics.spooled_on_pressure.get(),
            quarantined: self.metrics.quarantined.get(),
            circuit_open: self.metrics.circuit_open.get() != 0,
        }
    }

    /// Close the queue and wait for the worker to finish the calls it has
    /// already accepted.
    pub fn shutdown(self) {
        let KeyupUplink { tx, handle, .. } = self;
        drop(tx);
        if handle.join().is_err() {
            tracing::error!("uplink worker panicked");
        }
    }
}
