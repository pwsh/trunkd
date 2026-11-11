// SPDX-License-Identifier: GPL-3.0-or-later
//! `trunkd run`: wire the whole recorder together and hold it up.
//!
//! [`run_once`] is the entry point for both the binary and the integration
//! tests. It builds, in order:
//!
//! 1. the **delivery sink** — a [`KeyupUplink`] and its worker thread, or a
//!    spool-only writer, or nothing at all;
//! 2. the **observations thread**, folding control events into a
//!    [`SiteInventory`](trunkd_p25::SiteInventory) and posting them;
//! 3. the **call thread**, vocoding IMBE and assembling calls;
//! 4. one **supervised device unit** per active device, each of which owns an
//!    SDR thread and a DSP thread;
//! 5. the **keyup control thread**, and — when `[server] enabled` — the
//!    **local control panel** on its own HTTP thread.
//!
//! and then sits in a status loop until it is told to stop, the sources run
//! out, or the process is interrupted. Teardown runs strictly in reverse:
//! devices stop (finishing their open calls), the call queue drains, the
//! uplink drains, and only then does the PortaPack get its board back.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use trunkd_call::{AudioFormat, FinishedCall};
use trunkd_core::chan::{self, BoundedReceiver, BoundedSender};
use trunkd_core::{Error, Metrics, Result};
use trunkd_supervisor::{AfterBudget, RestartPolicy, SupervisedHandle, Supervisor, UnitHooks};
use trunkd_uplink::{KeyupUplink, Spool, UplinkConfig, UplinkHold, UplinkStats};

use crate::config::{AudioFormatName, Backend, ChannelMode, Config, DeviceRole};
use crate::pipeline::calls::CallDelivery;
use crate::pipeline::control::{
    self, ControlConfig, DesiredState, RecorderControl, TunerControl, TunerDescriptor,
};
use crate::pipeline::coordinator::{
    self, GrantMsg, RouteState, SiteCoordinator, TrafficCmd, TunerRoute, GRANT_BUS_DEPTH,
    TRAFFIC_CMD_DEPTH,
};
use crate::pipeline::device::DeviceUnit;
use crate::pipeline::link::LinkSlot;
use crate::pipeline::observations::{observations_url, ObservationsConfig};
use crate::pipeline::spectrum::{SpectrumFrame, SpectrumHook};
use crate::pipeline::{CallMsg, ObsMsg, Shared, SiteSpec, StreamEpoch};

/// Depth of the DSP → call-thread queue.
///
/// 8 192 events is roughly 20 s of one busy site's voice frames: enough that a
/// GC-free but momentarily descheduled call thread loses nothing, bounded so a
/// wedged one costs frames rather than memory.
const CALL_QUEUE_DEPTH: usize = 8_192;

/// Depth of the DSP → control-thread spectrum queue (W4). Frames are produced at
/// a few Hz per tuner and coalesced to newest-per-tuner before posting, so a
/// small drop-oldest buffer is plenty; a slow POST costs stale frames, not RAM.
const SPECTRUM_QUEUE_DEPTH: usize = 16;

/// How often the status line is logged.
const STATUS_INTERVAL: Duration = Duration::from_secs(10);

/// Longest call audio kept, matching `trunkd-call`'s own cap.
const MAX_CALL_DURATION: Duration = Duration::from_secs(30 * 60);

/// What a finished run did. Returned by [`run_once`] and asserted on by the
/// integration tests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RunSummary {
    /// Calls the engine started.
    pub calls_started: u64,
    /// Calls the engine ended.
    pub calls_ended: u64,
    /// Calls dropped as fragments (no identity, almost no voice).
    pub calls_dropped: u64,
    /// Clear calls not uploaded because they carried no audio at all.
    pub calls_dropped_no_audio: u64,
    /// Calls not encoded because they carried fewer voice frames than
    /// `[recorder] min_voice_frames`.
    pub calls_discarded_empty: u64,
    /// IMBE voice frames decoded or discarded.
    pub voice_frames: u64,
    /// Messages the DSP → call-thread queue evicted to make room
    /// (`calls.queue_dropped`).
    pub call_queue_dropped: u64,
    /// Calls the call thread closed itself because their end never arrived
    /// (`calls.orphaned`).
    pub calls_orphaned: u64,
    /// Samples the SDR ring had to drop.
    pub overflow_samples: u64,
    /// Site-observation POSTs that got a response.
    pub observations_posted: u64,
    /// Radio events Keyup accepted.
    pub radio_events_sent: u64,
    /// Radio events written to the offline spool instead.
    pub radio_events_spooled: u64,
    /// Lines written to the raw event log.
    pub event_log_written: u64,
    /// Non-voice events dropped rather than logged (queue overflow or a write
    /// failure).
    pub event_log_dropped: u64,
    /// Uplink counters at shutdown.
    pub uplink: UplinkStatsSummary,
}

/// [`UplinkStats`] flattened into the summary (and `Copy`/`Eq` for tests).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UplinkStatsSummary {
    /// Calls Keyup stored.
    pub uploaded: u64,
    /// Calls Keyup already had.
    pub duplicates: u64,
    /// Calls Keyup will never accept, plus local encode/spool failures.
    pub failed: u64,
    /// Calls written to the offline spool.
    pub spooled: u64,
    /// Bytes the spool occupies.
    pub spool_bytes: u64,
}

impl From<UplinkStats> for UplinkStatsSummary {
    fn from(s: UplinkStats) -> Self {
        UplinkStatsSummary {
            uploaded: s.uploaded,
            duplicates: s.duplicates,
            failed: s.failed,
            spooled: s.spooled,
            spool_bytes: s.spool_bytes,
        }
    }
}

/// Run the recorder until `stop` is set or every source ends.
///
/// The whole pipeline lives inside this call, so a test can drive it in-process
/// rather than spawning the binary. Blocks the calling thread.
pub fn run_once(cfg: &Config, stop: Arc<AtomicBool>) -> Result<RunSummary> {
    run_once_with_reload(cfg, stop, Arc::new(AtomicBool::new(false)))
}

/// [`run_once`], plus a second flag that ends the run so the caller can start
/// another one from a re-read config file.
///
/// A reload is a *stop that is not the end of the process*: the panel writes
/// `trunkd.toml`, sets this flag, and the wait loop returns down exactly the
/// same teardown path Ctrl+C takes — devices finish their open calls, the call
/// thread drains, the uplink drains. Nothing about the shutdown is special, and
/// that is the point: the alternative, resizing the per-device arrays under a
/// live DSP thread, is a class of bug this design does not have. The caller
/// ([`crate::main`]'s `run_recorder`) then loops and builds the pipeline again,
/// in the same process, with the same PID — which is what keeps a Windows
/// service RUNNING across a reload.
pub fn run_once_with_reload(
    cfg: &Config,
    stop: Arc<AtomicBool>,
    reload: Arc<AtomicBool>,
) -> Result<RunSummary> {
    cfg.validate()?;
    // AM is parsed (the sdrtrunk importer produces it) but not demodulated.
    for c in cfg
        .channel
        .iter()
        .filter(|c| c.enabled && c.mode == ChannelMode::Am)
    {
        tracing::warn!(
            channel = %c.name,
            frequency = c.frequency,
            mode = ?c.mode,
            "conventional analog channels are not implemented yet; skipping"
        );
    }

    let metrics = Metrics::new();
    let sites = Arc::new(site_specs(cfg));
    // Enabled NBFM channels, which the device units build their engines from.
    // Empty — and unallocated — for a config with none.
    let channels = crate::pipeline::conventional::channel_specs(cfg);
    if sites.is_empty() && channels.is_empty() {
        return Err(Error::config(
            "no enabled P25 site or conventional channel to record",
        ));
    }
    // Talkgroups this box never records. The TOML half is fixed for the run;
    // keyup's half arrives on the desired state and is applied through
    // `RecorderControl` below, into this very `Arc` — the site engines hold a
    // clone of it and test every grant against it, so a change takes effect on
    // the next grant with no restart and no rebuild of anything.
    let skip = trunkd_core::SkipList::new(cfg.recorder.skip_talkgroups.iter().copied());
    if !skip.is_empty() {
        tracing::info!(
            talkgroups = skip.len(),
            "[recorder] skip_talkgroups: these talkgroups will not be recorded"
        );
    }
    let shared = Shared {
        private_calls: Default::default(),
        calls: Default::default(),
        skip: skip.clone(),
        replay_clock: Default::default(),
        metrics: metrics.clone(),
        sites: sites.clone(),
    };
    // One stream timeline per tuner (multi-tuner core, W1): a two-tuner run
    // must not share one epoch. Indexed by device position in `cfg.device`.
    let epochs: Vec<Arc<StreamEpoch>> = cfg
        .device
        .iter()
        .map(|_| Arc::new(StreamEpoch::default()))
        .collect();

    let audio_format = audio_format(cfg);
    // The uplink hold: `[uplink] spool_only` is only its boot value. keyup and
    // the local panel can move it at runtime, which is why the control plane
    // below no longer switches itself off when it is set.
    let hold = Arc::new(UplinkHold::new(cfg.uplink.spool_only));
    // The event-log archive (`[recorder] event_log_archive`, or keyup's push of
    // the same). The state block exists whether or not the feature is on,
    // because the status POST reports `archive: false` either way — "it is off"
    // is the answer to most questions about a missing day — and because the
    // flag is live: the archive thread reads it every pass.
    let archive_state =
        crate::pipeline::eventarchive::ArchiveState::new(cfg.recorder.event_log_archive);
    // The recorder link, in its swappable slot. Nothing binds until
    // `LinkSlot::start` below, which is after the desired-state prefetch — so a
    // box whose TOML has no `[link]` at all can be given one entirely by keyup,
    // key included, on its very first bind.
    let link = crate::pipeline::link::LinkSlot::new(
        cfg,
        sites.as_ref(),
        metrics.clone(),
        cfg.recorder.resolved_instance_id(),
    );
    // Direct-to-Broadcastify delivery (the uplink worker does the work; this
    // is where keyup's block lands and where the counters live). Built for
    // every run whether or not the feature is ever turned on: the panel reports
    // it either way, and keyup can push a block a second after boot.
    let broadcastify =
        trunkd_uplink::BroadcastifyState::new(cfg.uplink.direct_broadcastify, &metrics);
    let recorder_control = RecorderControl::new(
        hold.clone(),
        archive_state.clone(),
        link.clone(),
        broadcastify.clone(),
        skip,
    );
    let delivery = Delivery::start(cfg, audio_format, &metrics, &hold, &broadcastify)?;
    let uplink_handle = delivery.uplink_handle();

    // The senders are shared rather than cloned: `BoundedSender` is
    // deliberately not `Clone` (drop-oldest assumes one logical producer), and
    // an `Arc` keeps that shape while letting the supervisor hand a fresh
    // handle to each rebuilt device unit.
    // Depth of the DSP → observations queue is a knob ([recorder]
    // event_queue_depth): a data session makes it the one queue that actually
    // sheds, and the operator sees that as `event_log_dropped` on the status
    // line.
    let (obs_tx, obs_rx) = chan::bounded::<ObsMsg>(cfg.recorder.event_queue_depth);
    let (calls_tx, calls_rx) = chan::bounded::<CallMsg>(call_queue_depth(channels.len()));
    let (obs_tx, calls_tx) = (Arc::new(obs_tx), Arc::new(calls_tx));

    // Opt-in spectrum frames (W4): DSP threads → the control thread, which POSTs
    // them. Drop-oldest and bounded, so a wedged POST never stalls a DSP thread.
    let (spectrum_tx, spectrum_rx) = chan::bounded::<SpectrumFrame>(SPECTRUM_QUEUE_DEPTH);
    let spectrum_tx = Arc::new(spectrum_tx);

    // "Offline" now means only what it says: this recorder has nobody to talk
    // to. `spool_only` is a hold on *uploads*, not on the control plane — a held
    // recorder still registers with keyup, still reports status, and still polls
    // desired state, which is the only way the hold can be released from
    // anywhere but a text editor. An empty upload URL is the one case that is
    // still fully offline: every control-plane route is derived from it.
    let offline = !cfg.uplink.enabled || cfg.uplink.url.trim().is_empty();
    // The panel's read-only copy of each site's observation document. The
    // observations thread owns the inventories; this is the only thing the HTTP
    // thread is allowed to see of them, and it refreshes on that thread's clock.
    let site_details = crate::pipeline::observations::SiteDetails::new(shared.sites.len());
    let obs_cfg = ObservationsConfig {
        url: if offline {
            String::new()
        } else {
            observations_url(&cfg.uplink.url)
        },
        api_key: cfg.uplink.api_key.clone(),
        interval: Duration::from_secs(cfg.uplink.observations_interval_s),
        events: trunkd_uplink::events::EventUplinkConfig {
            url: if offline {
                String::new()
            } else {
                trunkd_uplink::events::events_url(&cfg.uplink.url)
            },
            api_key: cfg.uplink.api_key.clone(),
            instance: cfg.recorder.resolved_instance_id(),
            // A sibling of the call spool, so one directory is the whole of
            // what an offline recorder is holding.
            spool_dir: std::path::Path::new(&cfg.uplink.spool_path).join("events"),
            spool_cap_bytes: cfg.recorder.event_spool_cap_bytes,
            // The event spool is a subdirectory of the call spool, so it is on
            // the same volume and answers to the same floor.
            spool_min_free_bytes: cfg.uplink.spool_min_free_bytes,
            ..trunkd_uplink::events::EventUplinkConfig::default()
        },
        radio_events: cfg.recorder.radio_events,
        hold: hold.clone(),
        event_log_dir: (cfg.recorder.event_log && !cfg.recorder.event_log_dir.is_empty())
            .then(|| std::path::PathBuf::from(&cfg.recorder.event_log_dir)),
        event_log_keep_days: cfg.recorder.event_log_keep_days,
        // The same block the status POST and the panel report from, so
        // `event_log.last_written_at` is the log's own answer rather than a
        // file mtime somebody has to SSH in to read.
        event_log_state: Some(archive_state.clone()),
        instance_id: cfg.recorder.resolved_instance_id(),
        details: Some(site_details.clone()),
    };
    let event_log_dir = obs_cfg.event_log_dir.clone();
    let obs_thread = {
        let shared = shared.clone();
        std::thread::Builder::new()
            .name("trunkd-observations".into())
            .spawn(move || crate::pipeline::observations::run(obs_rx, shared, obs_cfg))
            .map_err(Error::Io)?
    };
    // The live-audio fan-out (`[server] live_audio`). Built only when the panel
    // is serving *and* the operator asked for it, so the call thread's publish
    // is a `None` check in every default deployment.
    let monitor = (cfg.server.enabled && cfg.server.live_audio)
        .then(crate::pipeline::monitor::MonitorHub::new);
    if monitor.is_some() {
        tracing::info!("live audio monitor enabled: /api/monitor/stream on the control panel");
    }

    let call_thread = {
        let shared = shared.clone();
        let monitor = monitor.clone();
        let sink = delivery.sink();
        let instance_id = cfg.recorder.resolved_instance_id();
        let min_voice_frames = cfg.recorder.min_voice_frames;
        let orphan_after = orphan_after(cfg);
        let link = link.clone();
        let tuner_keys: Vec<String> = cfg.device.iter().map(|d| d.id.clone()).collect();
        std::thread::Builder::new()
            .name("trunkd-calls".into())
            .spawn(move || {
                crate::pipeline::calls::run(
                    calls_rx,
                    shared,
                    sink,
                    MAX_CALL_DURATION,
                    min_voice_frames,
                    orphan_after,
                    // Stamped onto every call so keyup can tell copies of one
                    // call apart by the recorder that heard them; the same id
                    // the tuner-status POST reports under.
                    Some(instance_id),
                    Some((link, tuner_keys)),
                    monitor,
                )
            })
            .map_err(Error::Io)?
    };

    // The keyup control loop (W2). The registry is seeded from the TOML devices
    // (boot defaults) and layered with keyup's desired-state at runtime; the
    // device factory below reads it on every (re)build, so a config change takes
    // effect by restarting one DeviceUnit rather than the whole process.
    let desired = DesiredState::from_config(cfg);
    // The keyup push channel (W6): a WebSocket that only ever says "fetch the
    // desired state now". It is latency, not a second control path — every
    // change still arrives through the GET below — so it is skipped entirely
    // offline, and the poll loop is unchanged whether it runs or not.
    let stream_enabled = !offline && cfg.uplink.stream;
    let stream_state = crate::pipeline::stream::StreamState::new(stream_enabled);
    let fetch_now = Arc::new(AtomicBool::new(false));
    let control_cfg = ControlConfig {
        desired_url: if offline {
            String::new()
        } else {
            control::desired_state_url(&cfg.uplink.url)
        },
        status_url: if offline {
            String::new()
        } else {
            control::tuner_status_url(&cfg.uplink.url)
        },
        spectrum_url: if offline {
            String::new()
        } else {
            control::spectrum_url(&cfg.uplink.url)
        },
        api_key: cfg.uplink.api_key.clone(),
        instance_id: cfg.recorder.resolved_instance_id(),
        // Reported verbatim on every status POST; keyup never sends it back.
        station: cfg.station.clone(),
        desired_interval: Duration::from_secs(10),
        status_interval: Duration::from_secs(15),
        recorder: recorder_control.clone(),
        upload_url: cfg.uplink.url.clone(),
        link: link.clone(),
        event_log: archive_state.clone(),
        ppm_memory_path: crate::pipeline::ppmmemory::memory_path(cfg),
        fetch_now: fetch_now.clone(),
        stream: stream_state.clone(),
        // The panel and its live tap, so Keyup can offer a "listen live" link
        // for this recorder. `None` when no panel is served at all.
        panel: cfg.server.enabled.then(|| control::PanelLink {
            base_url: cfg.server.public_base_url(),
            live_audio: cfg.server.live_audio,
            token_required: !cfg.server.tokens.is_empty(),
            hub: monitor.clone(),
        }),
    };

    // keyup's desired state, fetched once and synchronously *before* the first
    // tuner is built. Any field keyup manages that differs from the TOML would
    // otherwise be applied by the poll loop a few hundred ms in and restart the
    // tuner about a second after its first lock — a recording gap at every
    // start. Bounded by `desired_prefetch_timeout_s`; a failure falls back to
    // the TOML and the poll loop behaves exactly as it always has.
    let descriptors = tuner_descriptors(cfg, sites.as_ref(), &channels);
    control::prefetch_desired(
        &control::PrefetchConfig {
            desired_url: control_cfg.desired_url.clone(),
            api_key: cfg.uplink.api_key.clone(),
            instance_id: cfg.recorder.resolved_instance_id(),
            timeout: Duration::from_secs(cfg.uplink.desired_prefetch_timeout_s),
            ppm_memory_path: control_cfg.ppm_memory_path.clone(),
        },
        &descriptors,
        &desired,
        &recorder_control,
    );

    // Every source ending is what stops an offline replay; hardware never sets
    // this and runs until interrupted.
    let finished = Arc::new(AtomicBool::new(false));
    let (mut supervisor, handles, coordinator) = spawn_devices(
        cfg,
        &shared,
        &epochs,
        &calls_tx,
        &obs_tx,
        &spectrum_tx,
        finished.clone(),
        &desired,
        link.clone(),
    );
    if handles.is_empty() {
        return Err(Error::config(
            "no device serves an enabled site or conventional channel",
        ));
    }

    // The link's own socket and threads: receive, ingest and hello. Started
    // after the coordinator so a peer's grant has a routing table to land in
    // from the first datagram, and after the prefetch so the first bind already
    // uses keyup's settings.
    link.start(coordinator.as_ref().map(|c| c.coord.clone()));

    // The control thread: status up on a ~15 s cadence, desired-state down on a
    // ~10 s cadence. A sibling of the observations thread; never blocks the DSP
    // threads and never crashes the recorder.
    let control_stop = Arc::new(AtomicBool::new(false));
    let control_thread = {
        let shared = shared.clone();
        let descriptors = descriptors.clone();
        let desired = desired.clone();
        let stop = control_stop.clone();
        std::thread::Builder::new()
            .name("trunkd-control".into())
            .spawn(move || {
                control::run(shared, descriptors, desired, control_cfg, spectrum_rx, stop)
            })
            .map_err(Error::Io)?
    };

    // The push-channel thread. A sibling of the control thread with the same
    // discipline: its own stop flag, joined at teardown, and unable to take the
    // recorder down. It holds no state the control loop needs — losing it costs
    // the seconds between a keyup change and the next poll, nothing more.
    let stream_stop = Arc::new(AtomicBool::new(false));
    let stream_thread = if stream_enabled {
        let stream_cfg = crate::pipeline::stream::StreamConfig {
            url: control::stream_url(&cfg.uplink.url),
            api_key: cfg.uplink.api_key.clone(),
            instance_id: cfg.recorder.resolved_instance_id(),
            reconnect_max: Duration::from_secs(cfg.uplink.stream_reconnect_max_s.max(1)),
        };
        let state = stream_state.clone();
        let rc = recorder_control.clone();
        let fetch = fetch_now;
        let metrics = shared.metrics.clone();
        let stop = stream_stop.clone();
        Some(
            std::thread::Builder::new()
                .name("trunkd-stream".into())
                .spawn(move || {
                    crate::pipeline::stream::run(stream_cfg, state, rc, fetch, metrics, stop)
                })
                .map_err(Error::Io)?,
        )
    } else {
        None
    };

    // The archive thread. Started whenever there is a log directory at all,
    // whether or not archiving is on right now: the flag is live (keyup can
    // turn it on from the GUI), and the thread that is already scanning is what
    // makes that take effect without a restart. While it is off the thread
    // scans and counts the backlog and uploads nothing — so turning it on later
    // ships the days that accumulated rather than skipping them.
    let archive_stop = Arc::new(AtomicBool::new(false));
    let archive_thread = match &event_log_dir {
        Some(dir) => {
            let archive_cfg = crate::pipeline::eventarchive::ArchiveConfig {
                dir: dir.clone(),
                url: if offline {
                    String::new()
                } else {
                    crate::pipeline::eventarchive::event_log_url(&cfg.uplink.url)
                },
                api_key: cfg.uplink.api_key.clone(),
                instance: cfg.recorder.resolved_instance_id(),
                // The hold means "stop uploading", and a day of event log is
                // an upload like any other.
                hold,
            };
            let state = archive_state.clone();
            let stop = archive_stop.clone();
            Some(
                std::thread::Builder::new()
                    .name("trunkd-eventarchive".into())
                    .spawn(move || crate::pipeline::eventarchive::run(archive_cfg, state, stop))
                    .map_err(Error::Io)?,
            )
        }
        None => None,
    };

    // The local control panel (opt-in). A sibling of the control thread with the
    // same discipline: its own stop flag, joined at teardown, and unable to take
    // the recorder down — a failed bind logs and ends the thread.
    let panel_stop = Arc::new(AtomicBool::new(false));
    let panel_thread = if cfg.server.enabled {
        let state = crate::pipeline::panel::PanelState {
            shared,
            descriptors,
            desired,
            // A read-only view per unit: `SupervisedHandle::stop` consumes the
            // handle at teardown, so the panel is handed something that cannot
            // interfere with the lifecycle it is reporting on.
            units: handles.iter().map(|h| h.status()).collect(),
            tokens: cfg.server.tokens.clone(),
            // The port is the configured one until `panel::run` binds and
            // replaces it with the real one.
            origins: crate::pipeline::panel::OriginPolicy::new(&cfg.server),
            instance_id: cfg.recorder.resolved_instance_id(),
            station: cfg.station.clone(),
            config_json: crate::pipeline::panel::redacted_config(cfg),
            // The same redacted clone the JSON was built from, kept typed so
            // the device routes can answer from it when there is no file to
            // re-read. Redacted, not raw: nothing downstream of the panel ever
            // gets to hold the real upload key.
            config: Arc::new(crate::pipeline::panel::redact(cfg)),
            // The CLI is the only layer that knows the path (the recorder is
            // handed a parsed `Config`), so it is read back from where the CLI
            // stashed it. `None` for an env-only or in-process run, which is
            // what makes the config routes report "not writable".
            config_path: crate::about::config_path().map(|p| p.to_path_buf()),
            reload: reload.clone(),
            // Per-run, so a reload clears it: the pipeline that comes back was
            // built from the file the panel wrote.
            restart_pending: Arc::new(AtomicBool::new(false)),
            enumerate: Arc::new(trunkd_sdr::enumerate_all),
            recorder: recorder_control,
            upload_url: cfg.uplink.url.clone(),
            started: Instant::now(),
            link: link.clone(),
            event_log: archive_state,
            about: crate::about::About::new(cfg),
            stream: stream_state,
            monitor,
            site_details,
        };
        let bind = cfg.server.bind.clone();
        let stop = panel_stop.clone();
        Some(
            std::thread::Builder::new()
                .name("trunkd-http".into())
                .spawn(move || crate::pipeline::panel::run(&bind, state, stop))
                .map_err(Error::Io)?,
        )
    } else {
        None
    };

    if !channels.is_empty() {
        tracing::info!(
            channels = channels.len(),
            "conventional NBFM channels configured"
        );
    }
    tracing::info!(
        devices = handles.len(),
        sites = sites.len(),
        delivery = delivery.describe(),
        "recorder running; press Ctrl+C to stop"
    );

    status_loop(
        &stop,
        &reload,
        &finished,
        &metrics,
        sites.as_ref(),
        &uplink_handle,
    );

    // Teardown, strictly in reverse: the devices finish their open calls, the
    // call thread drains what they queued, and the uplink drains what the call
    // thread handed it. Dropping each sender is what ends the thread below it.
    // Neither the control thread nor the panel holds a queue, so both are
    // stopped by their own flags, and both go before the devices: each can ask
    // a DeviceUnit to restart, which is not a useful thing to be starting while
    // that unit is being torn down. The panel is first of the two — it is the
    // one an operator might be clicking on right now.
    panel_stop.store(true, Ordering::Relaxed);
    if let Some(t) = panel_thread {
        let _ = t.join();
    }

    // The push channel before the control loop it feeds: once nothing can ask
    // for an out-of-cycle fetch, the control loop's last pass is its own.
    stream_stop.store(true, Ordering::Relaxed);
    if let Some(t) = stream_thread {
        let _ = t.join();
    }

    control_stop.store(true, Ordering::Relaxed);
    let _ = control_thread.join();

    // The link before the devices, for the same reason as the control thread:
    // it can queue work to a DeviceUnit, and there is no point starting that
    // while the unit is being torn down.
    link.shutdown();
    archive_stop.store(true, Ordering::Relaxed);
    if let Some(t) = archive_thread {
        let _ = t.join();
    }

    tracing::info!("stopping devices");
    for h in handles {
        h.stop();
    }
    supervisor.shutdown();

    // The coordinator's grant senders live in the (now dropped) device units;
    // set its stop flag and join. It also exits on its own when the bus
    // disconnects, so this never hangs.
    if let Some(coord) = coordinator {
        coord.stop.store(true, Ordering::Relaxed);
        let _ = coord.thread.join();
    }

    drop(calls_tx);
    let _ = call_thread.join();
    drop(obs_tx);
    let _ = obs_thread.join();

    tracing::info!("draining uplink");
    // The status line's read-only handle has to go before the uplink can be
    // unwrapped and drained.
    drop(uplink_handle);
    let uplink_stats = delivery.shutdown();

    if let Some(port) = restore_target(cfg) {
        match trunkd_sdr::portapack::restore_portapack() {
            Ok(()) => tracing::info!(port, "board reset back into the PortaPack UI"),
            Err(e) => tracing::warn!(port, error = %e, "could not restore the PortaPack"),
        }
    }

    let summary = RunSummary {
        calls_started: metrics.counter("calls.started").get(),
        calls_ended: metrics.counter("calls.ended").get(),
        calls_dropped: metrics.counter("calls.dropped_insubstantive").get(),
        calls_dropped_no_audio: metrics.counter("calls.dropped_no_audio").get(),
        calls_discarded_empty: metrics.counter("calls.discarded_empty").get(),
        voice_frames: metrics.counter("calls.voice_frames").get(),
        call_queue_dropped: metrics.counter(crate::pipeline::calls::QUEUE_DROPPED).get(),
        calls_orphaned: metrics.counter(crate::pipeline::calls::ORPHANED).get(),
        overflow_samples: total_overflow(&metrics),
        observations_posted: metrics.counter("observations.posted").get(),
        radio_events_sent: metrics.counter("radio_events.sent").get(),
        radio_events_spooled: metrics.counter("radio_events.spooled").get(),
        event_log_written: metrics.gauge("event_log.written").get(),
        event_log_dropped: metrics.counter("event_log.dropped").get()
            + metrics
                .snapshot()
                .iter()
                .filter(|m| m.name.ends_with(".nonvoice_dropped"))
                .map(|m| m.value)
                .sum::<u64>(),
        uplink: uplink_stats.into(),
    };
    tracing::info!(
        calls_started = summary.calls_started,
        calls_ended = summary.calls_ended,
        uploaded = summary.uplink.uploaded,
        spooled = summary.uplink.spooled,
        failed = summary.uplink.failed,
        voice_frames = summary.voice_frames,
        overflow = summary.overflow_samples,
        "recorder stopped"
    );
    Ok(summary)
}

/// How long the call thread lets an open call go without a single message
/// before closing it as orphaned (its `Ended` was evicted from the queue).
///
/// A minute, or ten times the longest silence any enabled site's engine will
/// itself sit through before ending a call (`hang_ms + call_inactivity_ms`),
/// whichever is longer: the sweep is a backstop for a lost message, and must
/// never race the engine's own end of a call that is merely quiet.
///
/// A conventional channel's engine can sit silent for its `hang_ms` (waiting
/// for a re-key) plus its `tone_timeout_ms` (holding a re-key's audio until
/// the tone confirms) before its next message, so those count too. With no
/// channel configured that term is empty and the bound is what it always was.
fn orphan_after(cfg: &Config) -> Duration {
    let engine = trunkd_p25::site::SiteConfig::default();
    let sites = cfg.site.iter().filter(|s| s.enabled).map(|s| {
        s.hang_ms
            .unwrap_or(engine.hang_ms)
            .saturating_add(s.call_inactivity_ms.unwrap_or(engine.call_inactivity_ms))
    });
    let channels = cfg
        .channel
        .iter()
        .filter(|c| c.enabled && c.mode == ChannelMode::Nbfm)
        .filter_map(|c| c.params().ok())
        .map(|p| u64::from(p.squelch.hang_ms) + u64::from(p.squelch.tone_timeout_ms));
    let longest_ms = sites.chain(channels).max().unwrap_or(0);
    crate::pipeline::calls::MIN_ORPHAN_AFTER
        .max(Duration::from_millis(longest_ms.saturating_mul(10)))
}

/// Depth of the DSP → call-thread queue: [`CALL_QUEUE_DEPTH`], plus
/// [`QUEUE_DEPTH_PER_CHANNEL`](crate::pipeline::conventional::QUEUE_DEPTH_PER_CHANNEL)
/// per enabled NBFM channel — unchanged when there are none.
fn call_queue_depth(channels: usize) -> usize {
    CALL_QUEUE_DEPTH
        + channels.saturating_mul(crate::pipeline::conventional::QUEUE_DEPTH_PER_CHANNEL)
}

/// One [`SiteSpec`] per enabled site, in config order.
fn site_specs(cfg: &Config) -> Vec<SiteSpec> {
    cfg.site
        .iter()
        .filter(|s| s.enabled)
        .enumerate()
        .map(|(i, s)| SiteSpec {
            // The `[uplink]` system label is what Keyup files calls under and
            // wins when set, so one recorder can present several sites under
            // one system. The site label is the other way round: each
            // `[[site]]` names itself, and `[uplink] site_label` is only the
            // fallback for a site block that left its name empty. With the
            // uplink label winning, a recorder decoding one site plus a
            // neighbour's control channel posted the neighbour's observations
            // and radio events under the first site's name too, and Keyup filed a foreign
            // NAC's message mix under the wrong site (2026-09-16).
            system_label: first_non_empty(&cfg.uplink.system_label, &s.system),
            site_label: first_non_empty(&s.site, &cfg.uplink.site_label),
            control_frequencies: s.control_frequencies.clone(),
            metric_prefix: format!("site.{i}"),
            display: format!("{}/{}", s.system, s.site),
        })
        .collect()
}

fn first_non_empty(a: &str, b: &str) -> String {
    if a.trim().is_empty() {
        b.to_owned()
    } else {
        a.to_owned()
    }
}

/// One [`TunerDescriptor`] per configured device, for the control-loop status
/// report. Each device is mapped to the first enabled site it serves, so the
/// report can read that site's per-tuner gauges (`site.N.*`). A device that
/// serves no site but carries conventional channels reports the site label of
/// its first channel, with no site gauges to read.
fn tuner_descriptors(
    cfg: &Config,
    sites: &[SiteSpec],
    channels: &[crate::pipeline::conventional::ChannelSpec],
) -> Vec<TunerDescriptor> {
    // Enabled sites in the same order `site_specs`/`sites` used, so the metric
    // prefix `site.{i}` lines up with the gauges the DSP thread publishes.
    let enabled_sites: Vec<&crate::config::Site> = cfg.site.iter().filter(|s| s.enabled).collect();
    cfg.device
        .iter()
        .map(|d| {
            let site_idx = enabled_sites
                .iter()
                .position(|s| s.device_ids().contains(&d.id.as_str()));
            let (site_label, site_metric_prefix) = match site_idx {
                Some(i) => (
                    sites.get(i).map(|s| s.site_label.clone()),
                    sites.get(i).map(|s| s.metric_prefix.clone()),
                ),
                None => (
                    channels
                        .iter()
                        .find(|c| c.device_id == d.id)
                        .map(|c| c.site_label.clone()),
                    None,
                ),
            };
            TunerDescriptor {
                id: d.id.clone(),
                serial: d.serial.clone(),
                backend: d.backend,
                driver: d.driver.clone(),
                sample_rate: d.sample_rate,
                site_label,
                site_metric_prefix,
                // Operator-entered hardware facts: static for the life of the
                // process, so resolved here once rather than re-read from
                // config on every status POST. The *settings* a tuner runs are
                // not static and come from the applied config instead.
                description: crate::pipeline::control::TunerDescription {
                    receiver: d.receiver.clone(),
                    antenna: d.antenna.clone(),
                    cable: d.cable.clone(),
                    amplifier: d.amplifier.clone(),
                    height_m: d.height_m,
                    notes: d.notes.clone(),
                },
                channels: channels
                    .iter()
                    .filter(|c| c.device_id == d.id)
                    .map(crate::pipeline::control::ChannelDescriptor::from)
                    .collect(),
            }
        })
        .collect()
}

/// The coordinator thread and its stop flag, held for teardown.
struct CoordinatorHandle {
    thread: std::thread::JoinHandle<()>,
    stop: Arc<AtomicBool>,
    /// The routing table itself, shared with the link's ingest thread so a
    /// peer's grant takes exactly the same path a local one does.
    coord: Arc<SiteCoordinator>,
}

/// Spawn a supervised unit per device that serves at least one enabled site,
/// plus the cross-tuner grant coordinator — which every run gets, single-tuner
/// or not, so a link keyup turns on later has a bus to route into.
///
/// The inline fast path is unchanged by that: a front records every grant it
/// can reach itself and only publishes the ones it cannot.
#[allow(clippy::too_many_arguments)]
fn spawn_devices(
    cfg: &Config,
    shared: &Shared,
    epochs: &[Arc<StreamEpoch>],
    calls_tx: &Arc<BoundedSender<CallMsg>>,
    obs_tx: &Arc<BoundedSender<ObsMsg>>,
    spectrum_tx: &Arc<BoundedSender<SpectrumFrame>>,
    finished: Arc<AtomicBool>,
    desired: &Arc<DesiredState>,
    link: Arc<LinkSlot>,
) -> (Supervisor, Vec<SupervisedHandle>, Option<CoordinatorHandle>) {
    let mut supervisor = Supervisor::new();
    let mut handles = Vec::new();
    let active = cfg.active_device_ids();

    // The site index a message carries is the site's position among *enabled*
    // sites, which is what `site_specs` indexed by.
    let enabled_index: Vec<Option<usize>> = {
        let mut next = 0usize;
        cfg.site
            .iter()
            .map(|s| {
                s.enabled.then(|| {
                    let i = next;
                    next += 1;
                    i
                })
            })
            .collect()
    };

    let wiring = build_cross_wiring(cfg, shared, &enabled_index, link.clone());
    // Before any device thread exists: the first to open must already know
    // which peers to wait for (the replay lockstep).
    expect_replay_members(cfg, &active, &shared.replay_clock);

    for (dev_idx, d) in cfg.device.iter().enumerate() {
        if !active.contains(&d.id.as_str()) {
            tracing::info!(device = %d.id, "skipping device: nothing enabled uses it");
            continue;
        }
        // The supervisor rebuilds the unit on every restart, so everything it
        // needs has to be cloneable into the factory. The command receiver and
        // grant sender are `Arc`, so a rebuilt unit shares the same queue.
        let (cfg, device, shared) = (cfg.clone(), d.clone(), shared.clone());
        let (calls_tx, obs_tx) = (calls_tx.clone(), obs_tx.clone());
        let spectrum_tx = spectrum_tx.clone();
        let epoch = epochs[dev_idx].clone();
        let grant_tx = wiring.grant_tx_by_device[dev_idx].clone();
        let cmd_rx = wiring.cmd_rx_by_device[dev_idx].clone();
        let route_state = wiring.route_state_by_device[dev_idx].clone();
        let finished = finished.clone();
        let desired = desired.clone();
        let enabled_index = enabled_index.clone();
        let link = link.clone();
        let name = format!("device:{}", d.id);
        let hooks = device_hooks(&desired, &d.id, &name);
        handles.push(supervisor.spawn_with(
            name,
            Box::new(move || {
                // Read the override in effect for this tuner (TOML seed, or a
                // keyup desired-state layered on top). Adopting it here — on
                // every build — is what makes "apply a config change = restart
                // this DeviceUnit" work, and what advances the applied version.
                let built = desired.take_for_build(&device.id);
                let mut device = device.clone();
                control::apply_override(&built.config, &mut device);
                // Two of the overridable fields are site-scoped rather than
                // device-scoped (`data_follow`, `control_frequencies`), so the
                // config this unit is built from is laid over too — on the same
                // rebuild, for the same reason the device is.
                let mut cfg = cfg.clone();
                control::apply_site_override(&built.config, &device.id, &mut cfg);
                // The engine asks `[link] enabled` whether a lone `auto` front
                // should publish the grants it cannot record (only a peer can
                // take them). That has to be the *effective* link config, not
                // the TOML's: keyup owns the link, and a box it has linked must
                // publish from the build that follows rather than from the next
                // time someone edits the file.
                cfg.link.enabled = link.config().enabled;
                // The engine reads `device.bands` (which grants this tuner
                // records itself); the coordinator reads the shared route state
                // (which grants are *sent* to it). Both have to move together
                // or a keyup change would only half apply, so republish it
                // here, on the same rebuild that adopts the config.
                publish_route_state(&route_state, &built.config, &device);
                let control = TunerControl {
                    desired: desired.clone(),
                    device_id: device.id.clone(),
                    built_generation: built.generation,
                    enabled: built.config.enabled,
                    config: built.config.clone(),
                    version: built.version,
                    // Lives in the registry, so a gain pushed while this unit
                    // was rebuilding is still waiting for it.
                    runtime: desired.runtime_slot(&device.id),
                };
                // The spectrum flag lives in the registry (set by the control
                // loop, persists across rebuilds); the tap shares it and the
                // frame sink. Off costs one atomic load per block.
                let spectrum = SpectrumHook {
                    enabled: desired.spectrum_flag(&device.id),
                    tx: spectrum_tx.clone(),
                };
                Box::new(DeviceUnit::new(
                    &cfg,
                    &device,
                    dev_idx,
                    shared.clone(),
                    epoch.clone(),
                    calls_tx.clone(),
                    obs_tx.clone(),
                    grant_tx.clone(),
                    cmd_rx.clone(),
                    finished.clone(),
                    Some(control),
                    Some(spectrum),
                    &|i| enabled_index.get(i).copied().flatten(),
                ))
            }),
            device_policy(),
            hooks,
        ));
    }

    (supervisor, handles, wiring.coordinator)
}

/// Reserve a place on the replay clock for every device this run starts that
/// will replay a file unthrottled, so that whichever opens first waits (one
/// lockstep window, bounded by the clock's join deadline) for the others
/// instead of racing through its file while they build their engines. A
/// device that then idles or fails to open withdraws its place. Live
/// backends and real-time playback are never reserved, and a single reserved
/// device has no one to wait for, so neither changes.
fn expect_replay_members(cfg: &Config, active: &[&str], clock: &trunkd_sdr::Lockstep) {
    for d in &cfg.device {
        if d.backend == Backend::Iqfile
            && d.pacing == crate::config::PacingName::Unthrottled
            && active.contains(&d.id.as_str())
        {
            clock.expect(d.id.clone());
        }
    }
}

/// How a device unit is restarted: quickly at first (a USB hiccup or a busy
/// board usually clears in seconds), then — once six failures land inside a
/// minute — on a backoff from 30 s up to 10 min, **for as long as it takes**.
///
/// A tuner is never given up on. It used to be: six fast open failures in ~31 s
/// (an SDR re-enumerating, `hackrf_transfer` held by another process, a keyup
/// setting the driver refuses) marked the unit failed for good while the
/// process kept running, so neither systemd nor the Windows service manager
/// ever stepped in and the tuner recorded nothing until someone restarted the
/// recorder by hand.
pub(crate) fn device_policy() -> RestartPolicy {
    RestartPolicy {
        base_backoff: Duration::from_secs(1),
        max_backoff: Duration::from_secs(30),
        heartbeat_timeout: Some(Duration::from_secs(30)),
        after_budget: AfterBudget::RetrySlowly {
            base: Duration::from_secs(30),
            max: Duration::from_secs(600),
        },
        ..RestartPolicy::default()
    }
}

/// The supervisor hooks that tie a device unit to the desired-state registry:
/// every failure is recorded there (so `/api/status` and keyup's tuner status
/// say why the tuner is down and when it will be tried again), and the
/// tuner's restart generation re-arms a unit waiting out its backoff — the
/// panel's restart button and a newer keyup version both move it.
pub(crate) fn device_hooks(desired: &Arc<DesiredState>, id: &str, name: &str) -> UnitHooks {
    let (on_failure, rearm) = (desired.clone(), desired.clone());
    let (failure_id, rearm_id) = (id.to_owned(), id.to_owned());
    let escalated = name.to_owned();
    UnitHooks {
        on_escalate: Some(Box::new(move |_| {
            tracing::error!(
                unit = %escalated,
                "device keeps failing; retrying every 30 s to 10 min until it opens \
                 (the panel's restart button or a new keyup setting retries at once)"
            );
        })),
        on_failure: Some(Box::new(move |report| {
            on_failure.note_failure(&failure_id, report);
        })),
        rearm: Some(Box::new(move || rearm.rearm_token(&rearm_id))),
    }
}

/// The grant/traffic channels wired between the control and voice tuners, plus
/// the coordinator thread routing between them. Built for every run, so a link
/// keyup turns on mid-flight has a bus to route a peer's grant into.
struct CrossWiring {
    /// Grant publisher per device index; `Some` for a control front, or an auto
    /// front serving an enabled site.
    grant_tx_by_device: Vec<Option<Arc<BoundedSender<GrantMsg>>>>,
    /// Command receiver per device index; `Some` for every recording front.
    cmd_rx_by_device: Vec<Option<Arc<BoundedReceiver<TrafficCmd>>>>,
    /// What the coordinator routes by, per device index; `Some` for a routable
    /// tuner. The device factory republishes it on every (re)build, which is
    /// how a keyup `bands` or `enabled` change reaches the routing table rather
    /// than leaving it on the boot-time TOML partition.
    route_state_by_device: Vec<Option<Arc<RouteState>>>,
    /// The routing thread, when one was started.
    coordinator: Option<CoordinatorHandle>,
}

/// Decide the cross-tuner wiring and build the coordinator.
fn build_cross_wiring(
    cfg: &Config,
    shared: &Shared,
    enabled_index: &[Option<usize>],
    link: Arc<LinkSlot>,
) -> CrossWiring {
    let n = cfg.device.len();
    let mut grant_tx_by_device: Vec<Option<Arc<BoundedSender<GrantMsg>>>> = vec![None; n];
    let mut cmd_rx_by_device: Vec<Option<Arc<BoundedReceiver<TrafficCmd>>>> = vec![None; n];

    // Every recording tuner of every enabled site is routable, whatever the
    // site's tuner count and whatever `[link] enabled` says right now.
    //
    // It used to depend on both: a single-tuner site with the link off got no
    // coordinator, no grant bus and no command queues, because its one tuner
    // records everything inline. But keyup can turn the link on at any moment
    // (it rebuilds the link's sockets in place, restarting nothing), and a
    // recorder with no bus has nowhere to route an arriving peer's grant — so
    // *incoming* grants were silently unroutable until the next restart. The
    // bus is one sleeping thread and two empty queues when nothing uses it,
    // which is a great deal cheaper than that surprise.
    //
    // The inline fast path is untouched: a front still records every grant it
    // can itself and publishes only what it cannot ([`SiteEvent::GrantOut`]).
    let is_multi = |d: &crate::config::Device| {
        cfg.site
            .iter()
            .any(|s| s.enabled && s.device_ids().contains(&d.id.as_str()))
    };

    // A command queue per *recording* tuner (auto/voice/data) that serves a
    // multi-tuner site — the tuners the coordinator can route grants to. Auto is
    // included so an auto+auto band partition routes rather than double-records:
    // without a coordinator both auto fronts would take the inline path, ignore
    // their bands, and record every in-capture grant twice.
    let mut cmd_tx_by_device: Vec<Option<Arc<BoundedSender<TrafficCmd>>>> = vec![None; n];
    for (i, d) in cfg.device.iter().enumerate() {
        let records = matches!(
            d.role,
            DeviceRole::Auto | DeviceRole::Voice | DeviceRole::Data
        );
        if records && is_multi(d) {
            let (tx, rx) = chan::bounded::<TrafficCmd>(TRAFFIC_CMD_DEPTH);
            cmd_tx_by_device[i] = Some(Arc::new(tx));
            cmd_rx_by_device[i] = Some(Arc::new(rx));
        }
    }

    // Routes per enabled multi-tuner site: the recording tuners serving it, each
    // with the span it will accept (usable capture ∩ its bands). The coordinator
    // prefers the publishing device's own route, so an auto tuner's in-band
    // grants still resolve to itself.
    let n_sites = shared.sites.len();
    let mut routes: Vec<Vec<TunerRoute>> = (0..n_sites).map(|_| Vec::new()).collect();
    let mut route_state_by_device: Vec<Option<Arc<RouteState>>> = vec![None; n];
    for (dev_idx, d) in cfg.device.iter().enumerate() {
        let Some(tx) = &cmd_tx_by_device[dev_idx] else {
            continue;
        };
        // A tuner that can reach nothing at boot (no center, or a rate the
        // channelizer cannot drive) is not routable, exactly as before: whether
        // a config gets a coordinator at all must not change here.
        let boot_ranges = device_route_ranges(d);
        if boot_ranges.is_empty() {
            continue;
        }
        // One route per (site, device), holding the device's *live* state: the
        // set of routable tuners is fixed at startup (site membership is TOML)
        // but what each accepts follows its applied bands, and whether it is
        // routed at all follows its applied `enabled`.
        let state = RouteState::new(true, boot_ranges, d.accepts_remote_grants());
        route_state_by_device[dev_idx] = Some(state.clone());
        for (cfg_idx, s) in cfg.site.iter().enumerate() {
            if !s.enabled || !s.device_ids().contains(&d.id.as_str()) {
                continue;
            }
            let Some(site) = enabled_index.get(cfg_idx).copied().flatten() else {
                continue;
            };
            routes[site].push(TunerRoute {
                device_index: dev_idx,
                state: state.clone(),
                tx: tx.clone(),
            });
        }
    }

    // The bus is built unconditionally — including for a box with no local
    // route to serve at all, such as one whose only tuner is a `control`
    // front, which has nothing to route to and everything to publish.

    // Hand a grant sender to every control front, and to an auto front serving
    // an enabled site.
    let (grant_tx, grant_rx) = chan::bounded::<GrantMsg>(GRANT_BUS_DEPTH);
    let grant_tx = Arc::new(grant_tx);
    for (i, d) in cfg.device.iter().enumerate() {
        let publishes = match d.role {
            DeviceRole::Control => cfg
                .site
                .iter()
                .any(|s| s.enabled && s.device_ids().contains(&d.id.as_str())),
            DeviceRole::Auto => cfg
                .site
                .iter()
                .any(|s| s.enabled && s.device_ids().contains(&d.id.as_str())),
            DeviceRole::Voice | DeviceRole::Data => false,
        };
        if publishes {
            grant_tx_by_device[i] = Some(grant_tx.clone());
        }
    }

    let coord = Arc::new(SiteCoordinator::new(routes, shared.metrics.clone()));
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();
    // The link, when running, is handed every routed grant to publish. It is a
    // `GrantPublisher` rather than a `LinkHandle` here so the coordinator keeps
    // knowing nothing about sockets.
    let publisher: Option<Arc<dyn coordinator::GrantPublisher>> =
        Some(link as Arc<dyn coordinator::GrantPublisher>);
    let route = coord.clone();
    let thread = std::thread::Builder::new()
        .name("trunkd-coordinator".into())
        .spawn(move || coordinator::run(grant_rx, route, stop_thread, publisher))
        .expect("spawn coordinator thread");
    tracing::info!(sites = n_sites, "cross-tuner coordinator running");

    CrossWiring {
        grant_tx_by_device,
        cmd_rx_by_device,
        route_state_by_device,
        coordinator: Some(CoordinatorHandle {
            thread,
            stop,
            coord,
        }),
    }
}

/// Republish what a tuner accepts into the routing table: its applied
/// `enabled`, and the ranges its applied bands work out to.
///
/// Called from the device factory on every (re)build, which is the moment the
/// unit adopts a config — so the coordinator stops routing to a tuner keyup has
/// disabled at exactly the point that tuner starts idling, and its grants fall
/// to whatever sibling can serve them (or count as unrouted). A tuner with no
/// route (single-tuner site, or nothing to partition against) has no state to
/// publish and is skipped.
fn publish_route_state(
    state: &Option<Arc<RouteState>>,
    config: &control::AppliedConfig,
    device: &crate::config::Device,
) {
    let Some(state) = state else {
        return;
    };
    state.publish(
        config.enabled,
        crate::config::band_ranges(device.center_frequency, device.sample_rate, &device.bands),
        device.accepts_remote_grants(),
    );
}

/// The downlink ranges a recording tuner (auto/voice/data) will accept grants
/// for: its bands (each clamped to the usable capture span), or the whole
/// usable span when no bands are configured. A device with no center or an
/// unusable rate yields no ranges (it can record nothing).
///
/// The boot-time value only: the same computation runs in the device factory on
/// every rebuild, against the *applied* config, so the routing table follows a
/// keyup `bands` push.
fn device_route_ranges(d: &crate::config::Device) -> Vec<(u64, u64)> {
    crate::config::band_ranges(d.center_frequency, d.sample_rate, &d.bands)
}

/// Log the status line every [`STATUS_INTERVAL`] until asked to stop.
///
/// `reload` ends the loop exactly as `stop` does. The two are deliberately not
/// merged into one flag: teardown is identical, but only the caller can tell
/// "the process is going away" from "build me again from the new file", and the
/// last log line has to say which happened.
fn status_loop(
    stop: &AtomicBool,
    reload: &AtomicBool,
    finished: &AtomicBool,
    metrics: &Metrics,
    sites: &[SiteSpec],
    uplink: &Option<UplinkHandle>,
) {
    let mut next = Instant::now() + STATUS_INTERVAL;
    let mut last = Throughput::default();
    while !stop.load(Ordering::Relaxed)
        && !reload.load(Ordering::Relaxed)
        && !finished.load(Ordering::Relaxed)
    {
        if Instant::now() >= next {
            next = Instant::now() + STATUS_INTERVAL;
            last = log_status(metrics, sites, uplink, last);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// The previous status line's throughput reading, so the next one can report a
/// rate rather than a total.
#[derive(Clone, Copy)]
struct Throughput {
    at: Instant,
    produced: u64,
}

impl Default for Throughput {
    fn default() -> Self {
        Throughput {
            at: Instant::now(),
            produced: 0,
        }
    }
}

fn log_status(
    metrics: &Metrics,
    sites: &[SiteSpec],
    uplink: &Option<UplinkHandle>,
    last: Throughput,
) -> Throughput {
    let snap = metrics.snapshot();
    let get = |name: &str| snap.iter().find(|m| m.name == name).map(|m| m.value);
    let num = |name: &str| get(name).unwrap_or(0);
    let float = |name: &str| get(name).map(f64::from_bits).unwrap_or(0.0);

    // Ring counters are registered per device id; summing them is right even
    // with several devices, and cheap with one.
    let produced: u64 = snap
        .iter()
        .filter(|m| m.name.ends_with(".produced_samples"))
        .map(|m| m.value)
        .sum();
    let overflow: u64 = snap
        .iter()
        .filter(|m| m.name.ends_with(".overflow_samples"))
        .map(|m| m.value)
        .sum();

    let now = Instant::now();
    let secs = now.duration_since(last.at).as_secs_f64().max(0.001);
    let sps = (produced.saturating_sub(last.produced)) as f64 / secs;

    let (uploaded, spooled, failed, queue_dropped, spool_bytes, disk_free) = match uplink {
        Some(h) => {
            let s = h.stats();
            (
                s.uploaded + s.duplicates,
                s.spooled,
                s.failed,
                s.queue_dropped,
                s.spool_bytes,
                s.disk_free_bytes,
            )
        }
        // `spool_only` publishes the same numbers under its own prefix.
        None => (
            0,
            num("spool.stored"),
            num("spool.failed"),
            0,
            num("spool.bytes"),
            num("spool.disk_free_bytes"),
        ),
    };
    // Registered by the uplink only; 0 in `spool_only`.
    let spooled_on_pressure = num("uplink.spooled_on_pressure");
    let quarantined = num("uplink.spool_quarantined");
    // Both in MiB: bytes are unreadable at a glance and the status line is read
    // by a human watching a recorder fill a disk.
    let mib = |b: u64| b as f64 / (1024.0 * 1024.0);

    for (i, site) in sites.iter().enumerate() {
        let p = format!("site.{i}");
        let control = num(&format!("{p}.control_frequency_hz"));
        let nac = num(&format!("{p}.nac"));
        tracing::info!(
            site = %site.display,
            control = if control == 0 {
                "hunting".to_owned()
            } else {
                format!("{:.4} MHz", control as f64 / 1e6)
            },
            nac = format_args!("0x{nac:03x}"),
            tsbk_per_s = format_args!("{:.1}", float(&format!("{p}.tsbk_rate"))),
            active_calls = num(&format!("{p}.active_calls")),
            pool = format_args!(
                "{}/{}",
                num(&format!("{p}.pool_in_use")),
                num(&format!("{p}.pool_size"))
            ),
            started = num("calls.started"),
            ended = num("calls.ended"),
            uploaded,
            spooled,
            spooled_on_pressure,
            quarantined,
            failed,
            spool_mib = format_args!("{:.1}", mib(spool_bytes)),
            // 0 = not measured (unsupported platform, or the call failed); see
            // trunkd_uplink::disk.
            disk_free_mib = format_args!("{:.0}", mib(disk_free)),
            radio_events = format_args!(
                "{}/{}",
                num("radio_events.sent"),
                num("radio_events.spooled")
            ),
            event_log_dropped = num("event_log.dropped") + num(&format!("{p}.nonvoice_dropped")),
            dropped_no_audio = num("calls.dropped_no_audio"),
            discarded_empty = num("calls.discarded_empty"),
            // Calls granted on a Phase 2 (TDMA) channel this site's Phase 1
            // decoder refused to follow; 0 on an FDMA-only site.
            tdma_refused = num(&format!(
                "{p}.{}",
                crate::pipeline::device::SITE_TDMA_REFUSED
            )),
            dropped = num("calls.dropped_insubstantive") + queue_dropped,
            // Messages the DSP → call-thread queue evicted, and the calls the
            // call thread then had to close itself because their end was one
            // of them. Both 0 on a call thread that keeps up.
            call_queue_dropped = num(crate::pipeline::calls::QUEUE_DROPPED),
            orphaned_calls = num(crate::pipeline::calls::ORPHANED),
            voice_frames = num("calls.voice_frames"),
            overflow,
            throughput = format_args!("{:.2} Msps", sps / 1e6),
            "status"
        );
    }
    Throughput { at: now, produced }
}

/// Total ring overflow across every device.
fn total_overflow(metrics: &Metrics) -> u64 {
    metrics
        .snapshot()
        .iter()
        .filter(|m| m.name.ends_with(".overflow_samples"))
        .map(|m| m.value)
        .sum()
}

/// The PortaPack console to restore on exit, when one is configured and the
/// operator asked for it.
fn restore_target(cfg: &Config) -> Option<&str> {
    cfg.device
        .iter()
        .find(|d| {
            d.backend == Backend::Hackrf
                && d.restore_portapack_on_exit
                && d.portapack_port.is_some()
        })
        .and_then(|d| d.portapack_port.as_deref())
}

/// The configured audio encoding.
fn audio_format(cfg: &Config) -> AudioFormat {
    match cfg.uplink.audio_format {
        AudioFormatName::Mp3 => AudioFormat::Mp3 {
            bitrate_kbps: cfg.uplink.mp3_bitrate_kbps,
        },
        AudioFormatName::Wav => AudioFormat::Wav,
    }
}

// ---------------------------------------------------------------------------
// delivery
// ---------------------------------------------------------------------------

/// A shareable view of the running uplink, for the status line.
struct UplinkHandle(Arc<KeyupUplink>);

impl UplinkHandle {
    fn stats(&self) -> UplinkStats {
        self.0.stats()
    }
}

/// Where finished calls go: Keyup, the spool alone, or nowhere.
enum Delivery {
    /// The normal path: a bounded queue into the uplink worker.
    Keyup {
        uplink: Arc<KeyupUplink>,
        metrics: Metrics,
    },
    /// `spool_only` with no `uplink.url`: write audio + sidecar to disk, with no
    /// endpoint to ever upload them to. A *held* uplink still runs the
    /// networked worker below — the hold is a runtime switch, and that worker
    /// is what drains the spool when it is released.
    SpoolOnly {
        thread: std::thread::JoinHandle<()>,
        tx: Arc<BoundedSender<FinishedCall>>,
        metrics: Metrics,
    },
    /// `enabled = false`: assemble calls, count them, throw them away.
    Disabled,
}

impl Delivery {
    fn start(
        cfg: &Config,
        format: AudioFormat,
        metrics: &Metrics,
        hold: &Arc<UplinkHold>,
        broadcastify: &Arc<trunkd_uplink::BroadcastifyState>,
    ) -> Result<Delivery> {
        if !cfg.uplink.enabled {
            tracing::warn!("[uplink] enabled = false: calls are assembled but not delivered");
            return Ok(Delivery::Disabled);
        }
        // A held uplink still runs the networked worker: the hold is a runtime
        // switch, and the worker is what drains the spool the moment it is
        // released. The disk-only writer is left for the one case that can
        // never upload — `spool_only` with no URL configured at all, where
        // starting the worker would only fail.
        if cfg.uplink.spool_only && cfg.uplink.url.trim().is_empty() {
            tracing::warn!(
                "[uplink] spool_only = true with no url: calls are written to disk and the \
                 keyup control plane is off. Set [uplink] url to manage this recorder remotely."
            );
            return SpoolWriter::start(cfg, format, metrics);
        }
        if hold.is_held() {
            tracing::warn!(
                url = %cfg.uplink.url,
                "[uplink] spool_only = true: uploads are held and calls are spooled; \
                 the keyup control plane stays connected and can release the hold"
            );
        }
        let uplink = KeyupUplink::start(
            UplinkConfig {
                url: cfg.uplink.url.clone(),
                api_key: cfg.uplink.api_key.clone(),
                system_id: cfg.uplink.system_id,
                spool_dir: cfg.uplink.spool_path.clone().into(),
                spool_cap_bytes: cfg.uplink.spool_cap_bytes,
                spool_min_free_bytes: cfg.uplink.spool_min_free_bytes,
                audio_format: format,
                hold: hold.clone(),
                // Keyup owns the switch and pushes the block; this is only the
                // place the worker reads it from.
                broadcastify: broadcastify.clone(),
                instance_id: cfg.recorder.resolved_instance_id(),
                ..UplinkConfig::default()
            },
            metrics,
        )?;
        Ok(Delivery::Keyup {
            uplink: Arc::new(uplink),
            metrics: metrics.clone(),
        })
    }

    fn describe(&self) -> &'static str {
        match self {
            Delivery::Keyup { .. } => "keyup",
            Delivery::SpoolOnly { .. } => "spool-only",
            Delivery::Disabled => "disabled",
        }
    }

    fn uplink_handle(&self) -> Option<UplinkHandle> {
        match self {
            Delivery::Keyup { uplink, .. } => Some(UplinkHandle(uplink.clone())),
            _ => None,
        }
    }

    fn sink(&self) -> Box<dyn CallDelivery> {
        match self {
            Delivery::Keyup { uplink, .. } => Box::new(KeyupSink(uplink.clone())),
            Delivery::SpoolOnly { tx, .. } => Box::new(SpoolSink(tx.clone())),
            Delivery::Disabled => Box::new(DropSink),
        }
    }

    /// Drain and stop, returning the final counters.
    ///
    /// The counters are read back from the metrics registry rather than from
    /// the uplink handle, because [`KeyupUplink::shutdown`] consumes the handle
    /// and the last few calls are delivered *during* that drain.
    fn shutdown(self) -> UplinkStats {
        match self {
            Delivery::Keyup { uplink, metrics } => {
                match Arc::try_unwrap(uplink) {
                    Ok(u) => u.shutdown(),
                    // Only reachable if a sink outlived the call thread, which
                    // we joined before getting here.
                    Err(u) => tracing::warn!(
                        stats = ?u.stats(),
                        "uplink still referenced at shutdown; not draining"
                    ),
                }
                UplinkStats {
                    uploaded: metrics.counter("uplink.uploaded").get(),
                    duplicates: metrics.counter("uplink.duplicates").get(),
                    failed: metrics.counter("uplink.failed").get(),
                    spooled: metrics.counter("uplink.spooled").get(),
                    spool_bytes: metrics.gauge("uplink.spool_bytes").get(),
                    spool_evicted: metrics.counter("uplink.spool_evicted").get(),
                    disk_free_bytes: metrics.gauge("uplink.disk_free_bytes").get(),
                    queue_dropped: metrics.counter("uplink.queue_dropped").get(),
                    spooled_on_pressure: metrics.counter("uplink.spooled_on_pressure").get(),
                    quarantined: metrics.counter("uplink.spool_quarantined").get(),
                    circuit_open: metrics.gauge("uplink.circuit_open").get() != 0,
                }
            }
            Delivery::SpoolOnly {
                thread,
                tx,
                metrics,
            } => {
                drop(tx);
                let _ = thread.join();
                UplinkStats {
                    spooled: metrics.counter("spool.stored").get(),
                    failed: metrics.counter("spool.failed").get(),
                    spool_bytes: metrics.gauge("spool.bytes").get(),
                    spool_evicted: metrics.counter("spool.evicted").get(),
                    disk_free_bytes: metrics.gauge("spool.disk_free_bytes").get(),
                    ..UplinkStats::default()
                }
            }
            Delivery::Disabled => UplinkStats::default(),
        }
    }
}

struct KeyupSink(Arc<KeyupUplink>);

impl CallDelivery for KeyupSink {
    fn deliver(&self, call: FinishedCall) {
        self.0.submit(call);
    }
}

struct SpoolSink(Arc<BoundedSender<FinishedCall>>);

impl CallDelivery for SpoolSink {
    fn deliver(&self, call: FinishedCall) {
        self.0.send(call);
    }
}

struct DropSink;

impl CallDelivery for DropSink {
    fn deliver(&self, _call: FinishedCall) {}
}

/// How often the `spool_only` writer re-checks the spool's limits with nothing
/// new stored. Matches [`UplinkConfig::drain_interval`]'s default, which is
/// when the networked path does the same sweep.
const SPOOL_SWEEP_INTERVAL: Duration = Duration::from_secs(30);

/// How long the `spool_only` writer naps between polls of an empty queue; the
/// uplink worker's `IDLE_POLL` by another name, and for the same reason — the
/// thread owes the sweep timer a wake-up that a blocking `recv` would not give.
const SPOOL_IDLE_POLL: Duration = Duration::from_millis(25);

/// The `spool_only` writer: the uplink's disk half without its network half.
///
/// It runs on its own thread for the same reason the uplink worker does —
/// encoding MP3 is tens of milliseconds and must not sit on the call thread
/// between two voice frames.
struct SpoolWriter;

impl SpoolWriter {
    fn start(cfg: &Config, format: AudioFormat, metrics: &Metrics) -> Result<Delivery> {
        let spool = Spool::open(cfg.uplink.spool_path.clone(), cfg.uplink.spool_cap_bytes)
            .map_err(Error::Io)?
            .with_min_free_bytes(cfg.uplink.spool_min_free_bytes);
        tracing::warn!(
            dir = %spool.dir().display(),
            "[uplink] spool_only = true: calls are written to disk and never uploaded"
        );
        let (tx, rx) = chan::bounded::<FinishedCall>(64);
        let m = metrics.clone();
        let thread = std::thread::Builder::new()
            .name("trunkd-spool".into())
            .spawn(move || {
                let stored = m.counter("spool.stored");
                let failed = m.counter("spool.failed");
                let bytes = m.gauge("spool.bytes");
                let evicted = m.counter("spool.evicted");
                let disk_free = m.gauge("spool.disk_free_bytes");
                let sweep = |reason: &str| {
                    let s = spool.enforce_limits();
                    if s.evicted() > 0 {
                        evicted.add(s.evicted());
                        tracing::warn!(
                            cap_evicted = s.cap_evicted,
                            floor_evicted = s.floor_evicted,
                            free_bytes = s.free_bytes.unwrap_or(0),
                            reason,
                            "spool over its limits; dropped the oldest calls"
                        );
                    }
                    bytes.set(s.bytes);
                    // 0 means "not measured", as in the uplink worker.
                    disk_free.set(s.free_bytes.unwrap_or(0));
                };
                sweep("startup");
                let mut next_sweep = Instant::now() + SPOOL_SWEEP_INTERVAL;
                loop {
                    // The same cadence as the uplink worker's drain timer, and
                    // for the same reason: the volume can run out of space
                    // without this recorder writing a single call, so the
                    // limits are re-checked on a timer and not only on a store.
                    if Instant::now() >= next_sweep {
                        sweep("periodic sweep");
                        next_sweep = Instant::now() + SPOOL_SWEEP_INTERVAL;
                    }
                    let call = match rx.try_recv() {
                        Ok(call) => call,
                        Err(crossbeam_channel::TryRecvError::Empty) => {
                            std::thread::sleep(SPOOL_IDLE_POLL);
                            continue;
                        }
                        Err(crossbeam_channel::TryRecvError::Disconnected) => break,
                    };
                    // An encrypted call has no PCM at all: it is spooled as a
                    // sidecar with no audio, exactly as it would be uploaded.
                    let audio = if call.pcm.is_empty() {
                        None
                    } else {
                        match trunkd_call::encode_audio(&call.pcm, format) {
                            Ok((b, _, _)) => Some(b),
                            Err(e) => {
                                failed.inc();
                                tracing::error!(error = %e, "encoding call audio failed");
                                continue;
                            }
                        }
                    };
                    match spool.store(&call, audio.as_deref(), format.extension()) {
                        Ok(_) => stored.inc(),
                        Err(e) => {
                            failed.inc();
                            tracing::error!(error = %e, "spooling call failed");
                        }
                    }
                    sweep("call spooled");
                }
            })
            .map_err(Error::Io)?;
        Ok(Delivery::SpoolOnly {
            thread,
            tx: Arc::new(tx),
            metrics: metrics.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Device, Site, Uplink};

    /// The observations queue is built at exactly the configured depth: it
    /// accepts that many events without evicting, and the next one evicts the
    /// oldest and is counted (the `event_log_dropped` signal).
    #[test]
    fn obs_queue_honors_configured_depth() {
        let mut cfg = Config::default();
        cfg.recorder.event_queue_depth = 512;
        let (tx, rx) = chan::bounded::<ObsMsg>(cfg.recorder.event_queue_depth);
        let msg = || ObsMsg {
            site: 0,
            tuner: 0,
            control_hz: 851_050_000,
            traffic_hz: 0,
            at_ms: 0,
            event: trunkd_p25::P25Event::TsbkFailed,
        };
        for _ in 0..cfg.recorder.event_queue_depth {
            assert_eq!(tx.send(msg()), chan::SendOutcome::Sent);
        }
        assert_eq!(tx.dropped_count(), 0);
        assert_eq!(tx.send(msg()), chan::SendOutcome::Evicted);
        assert_eq!(rx.dropped_count(), 1);
        // Still exactly `depth` items resident: drop-oldest never grows.
        let mut drained = 0;
        while rx.try_recv().is_ok() {
            drained += 1;
        }
        assert_eq!(drained, cfg.recorder.event_queue_depth);
    }

    /// Every unthrottled iqfile device the run will start is reserved on the
    /// replay clock up front; real-time playback, live backends and devices
    /// nothing enabled uses are not.
    #[test]
    fn unthrottled_replays_are_reserved_on_the_replay_clock() {
        let mut cfg = cfg_with(Uplink::default());
        let base = cfg.device[0].clone();
        let device = |id: &str, backend: Backend, pacing: crate::config::PacingName| {
            let mut d = base.clone();
            d.id = id.into();
            d.backend = backend;
            d.pacing = pacing;
            d
        };
        use crate::config::PacingName::{Realtime, Unthrottled};
        cfg.device = vec![
            device("fast0", Backend::Iqfile, Unthrottled),
            device("fast1", Backend::Iqfile, Unthrottled),
            device("paced", Backend::Iqfile, Realtime),
            device("live", Backend::Hackrf, Unthrottled),
            device("unused", Backend::Iqfile, Unthrottled),
        ];
        let active = ["fast0", "fast1", "paced", "live"];
        let clock = trunkd_sdr::Lockstep::new();
        expect_replay_members(&cfg, &active, &clock);
        assert_eq!(
            clock.pending(),
            vec!["fast0".to_owned(), "fast1".to_owned()]
        );
    }

    fn cfg_with(uplink: Uplink) -> Config {
        Config {
            uplink,
            device: vec![Device {
                id: "d0".into(),
                backend: Backend::Iqfile,
                path: Some("/tmp/x.iq".into()),
                sample_rate: 20_000_000,
                center_frequency: Some(855_500_000),
                ..default_device()
            }],
            site: vec![Site {
                system: "Example Network".into(),
                site: "Central".into(),
                device: "d0".into(),
                enabled: true,
                control_frequencies: vec![851_050_000],
                traffic_pool_size: 20,
                ..default_site()
            }],
            ..Config::default()
        }
    }

    fn default_device() -> Device {
        toml::from_str("id = \"x\"\nsample_rate = 1\n").expect("device defaults")
    }

    fn default_site() -> Site {
        toml::from_str("system = \"s\"\nsite = \"t\"\ndevice = \"d\"\ncontrol_frequencies = [1]\n")
            .expect("site defaults")
    }

    #[test]
    fn uplink_labels_override_the_site_labels() {
        let cfg = cfg_with(Uplink {
            system_label: "Example Network".into(),
            site_label: "Central".into(),
            ..Uplink::default()
        });
        let specs = site_specs(&cfg);
        assert_eq!(specs[0].system_label, "Example Network");
        assert_eq!(specs[0].site_label, "Central");
    }

    #[test]
    fn a_site_falls_back_to_its_own_labels() {
        let cfg = cfg_with(Uplink::default());
        let specs = site_specs(&cfg);
        assert_eq!(specs[0].system_label, "Example Network");
        assert_eq!(specs[0].site_label, "Central");
        assert_eq!(specs[0].display, "Example Network/Central");
        assert_eq!(specs[0].metric_prefix, "site.0");
    }

    #[test]
    fn the_audio_format_follows_the_config() {
        let mut cfg = cfg_with(Uplink::default());
        assert_eq!(audio_format(&cfg), AudioFormat::Mp3 { bitrate_kbps: 16 });
        cfg.uplink.mp3_bitrate_kbps = 32;
        assert_eq!(audio_format(&cfg), AudioFormat::Mp3 { bitrate_kbps: 32 });
        cfg.uplink.audio_format = AudioFormatName::Wav;
        assert_eq!(audio_format(&cfg), AudioFormat::Wav);
    }

    #[test]
    fn the_portapack_is_only_restored_when_asked() {
        let mut cfg = cfg_with(Uplink::default());
        cfg.device[0].backend = Backend::Hackrf;
        cfg.device[0].portapack_port = Some("/dev/ttyACM0".into());
        assert_eq!(restore_target(&cfg), None, "off by default");
        cfg.device[0].restore_portapack_on_exit = true;
        assert_eq!(restore_target(&cfg), Some("/dev/ttyACM0"));
    }

    #[test]
    fn disabled_sites_do_not_get_a_spec() {
        let mut cfg = cfg_with(Uplink::default());
        cfg.site[0].enabled = false;
        assert!(site_specs(&cfg).is_empty());
    }

    // -- cross-tuner wiring (multi-tuner core, W1) ----------------------------

    /// Build the wiring for `cfg` the way `spawn_devices` does.
    fn wiring(cfg: &Config) -> CrossWiring {
        let shared = Shared {
            private_calls: Default::default(),
            calls: Default::default(),
            skip: trunkd_core::SkipList::empty(),
            replay_clock: Default::default(),
            metrics: Metrics::new(),
            sites: Arc::new(site_specs(cfg)),
        };
        let enabled_index: Vec<Option<usize>> = {
            let mut next = 0usize;
            cfg.site
                .iter()
                .map(|s| {
                    s.enabled.then(|| {
                        let i = next;
                        next += 1;
                        i
                    })
                })
                .collect()
        };
        build_cross_wiring(
            cfg,
            &shared,
            &enabled_index,
            LinkSlot::new(
                cfg,
                shared.sites.as_ref(),
                shared.metrics.clone(),
                "rec-test".into(),
            ),
        )
    }

    /// A `[link]` section on a loopback port, appended to a test config.
    const LINK_TOML: &str = r#"
            [link]
            enabled = true
            bind = "127.0.0.1:0"
            key = "a-sixteen-byte-key-or-longer"
            "#;

    /// The same wiring, but with a link handle in hand.
    fn wiring_linked(cfg: &Config) -> CrossWiring {
        let metrics = Metrics::new();
        let shared = Shared {
            private_calls: Default::default(),
            calls: Default::default(),
            skip: trunkd_core::SkipList::empty(),
            replay_clock: Default::default(),
            metrics: metrics.clone(),
            sites: Arc::new(site_specs(cfg)),
        };
        let enabled_index: Vec<Option<usize>> = (0..cfg.site.len()).map(Some).collect();
        let link = LinkSlot::new(cfg, shared.sites.as_ref(), metrics, "rec-test".into());
        build_cross_wiring(cfg, &shared, &enabled_index, link)
    }

    /// Stop a coordinator thread a wiring test started.
    fn tear_down(mut w: CrossWiring) {
        if let Some(coord) = w.coordinator.take() {
            coord.stop.store(true, Ordering::Relaxed);
            drop(w);
            let _ = coord.thread.join();
        }
    }

    /// The orphan sweep never races the engine: a minute at least, and ten
    /// times the longest silence an enabled site will sit through.
    #[test]
    fn the_orphan_bound_outlasts_every_sites_own_inactivity_timer() {
        let toml = |extra: &str| {
            format!(
                r#"
                [[device]]
                id = "solo"
                backend = "iqfile"
                path = "cap.iq"
                sample_rate = 8000000
                center_frequency = 854000000
                [[site]]
                system = "S"
                site = "T"
                devices = ["solo"]
                control_frequencies = [851050000]
                {extra}
                "#
            )
        };
        assert_eq!(
            orphan_after(&parse(&toml(""))),
            crate::pipeline::calls::MIN_ORPHAN_AFTER
        );
        assert_eq!(
            orphan_after(&parse(&toml("hang_ms = 3000\ncall_inactivity_ms = 9000"))),
            Duration::from_secs(120)
        );
    }

    #[test]
    fn a_lone_auto_tuner_is_routable_with_or_without_the_link() {
        // The same one-device config, wired twice. It is routable either way:
        // the bus is not conditional on the link, because the link is not
        // conditional on a restart. With the link on, the "other tuner" can be
        // in another process, which is what that routability is for.
        let toml = r#"
            [[device]]
            id = "solo"
            backend = "iqfile"
            path = "cap.iq"
            sample_rate = 8000000
            center_frequency = 854000000
            [[site]]
            system = "S"
            site = "T"
            devices = ["solo"]
            control_frequencies = [851050000]
            "#;
        let plain = wiring(&parse(toml));
        assert!(plain.coordinator.is_some(), "the bus exists either way");
        assert!(plain.grant_tx_by_device[0].is_some());
        assert!(plain.cmd_rx_by_device[0].is_some());
        tear_down(plain);

        let linked = wiring_linked(&parse(&format!("{toml}{LINK_TOML}")));
        assert!(linked.coordinator.is_some(), "the link needs one");
        assert!(
            linked.grant_tx_by_device[0].is_some(),
            "it publishes what it cannot record"
        );
        assert!(
            linked.cmd_rx_by_device[0].is_some(),
            "and can be handed a peer's grant"
        );
        tear_down(linked);
    }

    #[test]
    fn a_link_keyup_turns_on_routes_a_peers_grant_without_a_restart() {
        // The regression this wiring exists for. The box starts with no
        // `[link]` at all and one tuner, so it used to get no grant bus; keyup
        // then pushes a recorder block turning the link on, which rebuilds the
        // link's sockets in place and restarts nothing. The peer's grant has to
        // reach the tuner *now*, not at whatever restart happens next.
        let cfg = parse(
            r#"
            [[device]]
            id = "solo"
            backend = "iqfile"
            path = "cap.iq"
            sample_rate = 8000000
            center_frequency = 854000000
            remote_grants = true
            [[site]]
            system = "example"
            site = "central"
            devices = ["solo"]
            control_frequencies = [851050000]
            "#,
        );
        let metrics = Metrics::new();
        let shared = Shared {
            private_calls: Default::default(),
            calls: Default::default(),
            skip: trunkd_core::SkipList::empty(),
            replay_clock: Default::default(),
            metrics: metrics.clone(),
            sites: Arc::new(site_specs(&cfg)),
        };
        let link = LinkSlot::new(
            &cfg,
            shared.sites.as_ref(),
            metrics.clone(),
            "rec-a".to_owned(),
        );
        assert!(!link.config().enabled, "the TOML has no link");
        let w = build_cross_wiring(&cfg, &shared, &[Some(0)], link.clone());
        let coord = w
            .coordinator
            .as_ref()
            .expect("the bus is built whatever the link says")
            .coord
            .clone();
        // What `run` does: hand the slot the coordinator it will route into,
        // even though nothing binds while the link is off.
        link.start(Some(coord.clone()));
        assert!(link.handle().is_none(), "nothing bound yet");

        // keyup's recorder block, arriving on the desired-state poll.
        let rc = crate::pipeline::control::RecorderControl::new(
            Arc::new(UplinkHold::new(false)),
            crate::pipeline::eventarchive::ArchiveState::new(false),
            link.clone(),
            trunkd_uplink::BroadcastifyState::new(true, &Metrics::new()),
            trunkd_core::SkipList::empty(),
        );
        assert!(
            rc.apply_desired(&crate::pipeline::control::DesiredRecorder {
                version: 1,
                link: Some(crate::pipeline::control::DesiredLink {
                    enabled: Some(true),
                    bind: Some("127.0.0.1:0".to_owned()),
                    key: Some("a-sixteen-byte-key-or-longer".to_owned()),
                    ..Default::default()
                }),
                ..Default::default()
            })
        );

        // A peer announces a grant on this site; it must land on the tuner.
        let handle = link.handle().expect("the pushed link bound");
        handle.ingest(
            "192.0.2.30:47123".parse().expect("a literal address"),
            trunkd_link::LinkMsg::Grant {
                site: trunkd_link::SiteKey {
                    system: "example".to_owned(),
                    site: "central".to_owned(),
                    nac: 0x34F,
                },
                frequency_hz: 852_137_500,
                talkgroup: 1301,
                source: 7,
                flags: trunkd_link::GrantFlags::default(),
                at_wall_ms: trunkd_core::Timestamp::now().unix_millis(),
                seq: 1,
            },
            Some(&coord),
        );
        let followed = match w.cmd_rx_by_device[0]
            .as_ref()
            .expect("the tuner has a command queue")
            .try_recv()
        {
            Ok(TrafficCmd::Follow { frequency_hz, .. }) => Some(frequency_hz),
            _ => None,
        };
        assert_eq!(followed, Some(852_137_500), "the peer's grant was routed");
        tear_down(w);
    }

    #[test]
    fn a_control_only_recorder_still_publishes_over_the_link() {
        // The headline topology: this box spends its one tuner on the control
        // channel and records nothing itself. There is no local route to build
        // and every grant to publish, so the bus has to exist anyway.
        let toml = r#"
            [[device]]
            id = "ctrl"
            backend = "iqfile"
            path = "c.iq"
            sample_rate = 8000000
            center_frequency = 851050000
            role = "control"
            [[site]]
            system = "S"
            site = "T"
            devices = ["ctrl"]
            control_frequencies = [851050000]
            "#;
        let w = wiring_linked(&parse(&format!("{toml}{LINK_TOML}")));
        assert!(w.coordinator.is_some());
        assert!(
            w.grant_tx_by_device[0].is_some(),
            "every grant it hears goes to the peers"
        );
        tear_down(w);
    }

    #[test]
    fn the_link_does_not_disturb_an_in_process_control_plus_voice_pair() {
        // The same two-tuner config, wired with and without the link: the local
        // routing is identical either way. The link adds peers, it does not
        // rearrange what is already there.
        let toml = r#"
            [[device]]
            id = "ctrl"
            backend = "iqfile"
            path = "c.iq"
            sample_rate = 8000000
            center_frequency = 851050000
            role = "control"
            [[device]]
            id = "voice"
            backend = "iqfile"
            path = "v.iq"
            sample_rate = 8000000
            center_frequency = 854000000
            role = "voice"
            [[site]]
            system = "S"
            site = "T"
            devices = ["ctrl", "voice"]
            control_frequencies = [851050000]
            "#;
        for (name, w) in [
            ("plain", wiring(&parse(toml))),
            (
                "linked",
                wiring_linked(&parse(&format!("{toml}{LINK_TOML}"))),
            ),
        ] {
            assert!(
                w.grant_tx_by_device[0].is_some(),
                "{name}: control publishes"
            );
            assert!(
                w.grant_tx_by_device[1].is_none(),
                "{name}: voice never does"
            );
            assert!(
                w.cmd_rx_by_device[0].is_none(),
                "{name}: control takes none"
            );
            assert!(w.cmd_rx_by_device[1].is_some(), "{name}: voice receives");
            tear_down(w);
        }
    }

    fn parse(toml: &str) -> Config {
        let cfg: Config = toml::from_str(toml).expect("parse");
        cfg.validate().expect("valid config");
        cfg
    }

    #[test]
    fn a_single_tuner_config_still_gets_a_coordinator_and_its_channels() {
        // One device, the link off in the TOML — and the bus is built anyway,
        // because keyup can turn the link on without restarting anything and a
        // peer's grant needs somewhere to land. The inline fast path is
        // untouched: the front records what it can reach and only publishes
        // what it cannot.
        let cfg = cfg_with(Uplink::default());
        assert_eq!(cfg.device.len(), 1);
        let w = wiring(&cfg);
        assert!(w.coordinator.is_some(), "the bus exists for every run");
        assert!(w.grant_tx_by_device[0].is_some(), "it can publish");
        assert!(w.cmd_rx_by_device[0].is_some(), "and can be handed a grant");
        tear_down(w);
    }

    #[test]
    fn the_legacy_device_field_is_wired_like_any_other() {
        let cfg = parse(
            r#"
            [[device]]
            id = "x"
            backend = "iqfile"
            path = "/dev/null"
            sample_rate = 20000000
            center_frequency = 855500000
            [[site]]
            system = "S"
            site = "T"
            device = "x"
            control_frequencies = [851050000]
            "#,
        );
        let w = wiring(&cfg);
        assert!(w.coordinator.is_some());
        assert!(w.grant_tx_by_device[0].is_some());
        assert!(w.cmd_rx_by_device[0].is_some());
        tear_down(w);
    }

    #[test]
    fn a_single_element_devices_list_is_wired_like_any_other() {
        let cfg = parse(
            r#"
            [[device]]
            id = "x"
            backend = "iqfile"
            path = "/dev/null"
            sample_rate = 20000000
            center_frequency = 855500000
            [[site]]
            system = "S"
            site = "T"
            devices = ["x"]
            control_frequencies = [851050000]
            "#,
        );
        let w = wiring(&cfg);
        assert!(
            w.coordinator.is_some(),
            "one tuner, one element, still a bus"
        );
        tear_down(w);
    }

    #[test]
    fn two_auto_tuners_get_a_coordinator_and_both_publish_and_receive() {
        // The band-partition case: both auto fronts publish and can receive, so
        // an out-of-band grant routes to the sibling instead of double-recording.
        let cfg = parse(
            r#"
            [[device]]
            id = "lo"
            backend = "iqfile"
            path = "/dev/null"
            sample_rate = 20000000
            center_frequency = 855500000
            role = "auto"
            bands = ["845000000-855000000"]
            [[device]]
            id = "hi"
            backend = "iqfile"
            path = "/dev/null"
            sample_rate = 20000000
            center_frequency = 855500000
            role = "auto"
            bands = ["855000000-866000000"]
            [[site]]
            system = "S"
            site = "T"
            devices = ["lo", "hi"]
            control_frequencies = [851050000]
            "#,
        );
        let mut w = wiring(&cfg);
        assert!(w.coordinator.is_some(), "two tuners need a coordinator");
        assert!(
            w.grant_tx_by_device.iter().all(Option::is_some),
            "both auto fronts publish out-of-band grants"
        );
        assert!(
            w.cmd_rx_by_device.iter().all(Option::is_some),
            "both auto fronts can receive routed grants"
        );
        // Do not leak the coordinator thread.
        let coord = w.coordinator.take().unwrap();
        coord.stop.store(true, Ordering::Relaxed);
        drop(w); // drops the grant senders so the thread also exits on disconnect
        let _ = coord.thread.join();
    }

    #[test]
    fn a_band_partition_gives_each_tuner_state_a_rebuild_can_republish() {
        // The keyup `bands` path end to end on the routing side: the boot
        // partition comes from the TOML, and the handle the device factory
        // holds is the same one the coordinator routes by — so republishing it
        // on a rebuild is what makes a pushed band list actually route.
        let cfg = parse(
            r#"
            [[device]]
            id = "lo"
            backend = "iqfile"
            path = "/dev/null"
            sample_rate = 20000000
            center_frequency = 855500000
            role = "auto"
            bands = ["845000000-855000000"]
            [[device]]
            id = "hi"
            backend = "iqfile"
            path = "/dev/null"
            sample_rate = 20000000
            center_frequency = 855500000
            role = "auto"
            bands = ["855000000-866000000"]
            [[site]]
            system = "S"
            site = "T"
            devices = ["lo", "hi"]
            control_frequencies = [851050000]
            "#,
        );
        let mut w = wiring(&cfg);
        let lo = w.route_state_by_device[0].clone().expect("lo is routable");
        let hi = w.route_state_by_device[1].clone().expect("hi is routable");
        assert!(lo.covers(852_137_500), "the TOML partition at boot");
        assert!(!lo.covers(858_987_500));
        assert!(hi.covers(858_987_500));

        // keyup clears `lo`'s restriction; the rebuild republishes its state.
        let applied = control::AppliedConfig {
            bands: Some(Vec::new()),
            enabled: true,
            ..Default::default()
        };
        let mut device = cfg.device[0].clone();
        control::apply_override(&applied, &mut device);
        publish_route_state(&Some(lo.clone()), &applied, &device);
        assert!(
            lo.covers(858_987_500),
            "the tuner now accepts its whole usable capture"
        );

        // ...and keyup disables it: the rebuild publishes that too, so the
        // coordinator stops routing to a unit that is idling. Nothing about its
        // bands changed — only whether it is recording.
        let disabled = control::AppliedConfig {
            enabled: false,
            ..applied.clone()
        };
        publish_route_state(&Some(lo.clone()), &disabled, &device);
        assert!(!lo.covers(852_137_500), "a disabled tuner accepts nothing");
        assert!(hi.covers(858_987_500), "its sibling is untouched");
        // Re-enabled by the next rebuild, on the same bands.
        publish_route_state(&Some(lo.clone()), &applied, &device);
        assert!(lo.covers(852_137_500));

        let coord = w.coordinator.take().unwrap();
        coord.stop.store(true, Ordering::Relaxed);
        drop(w);
        let _ = coord.thread.join();
    }

    #[test]
    fn a_control_plus_voice_site_wires_the_publisher_and_the_receiver() {
        let cfg = parse(
            r#"
            [[device]]
            id = "ctrl"
            backend = "iqfile"
            path = "/dev/null"
            sample_rate = 20000000
            center_frequency = 855500000
            role = "control"
            [[device]]
            id = "voice0"
            backend = "iqfile"
            path = "/dev/null"
            sample_rate = 20000000
            center_frequency = 855500000
            role = "voice"
            [[site]]
            system = "S"
            site = "T"
            devices = ["ctrl", "voice0"]
            control_frequencies = [851050000]
            "#,
        );
        let mut w = wiring(&cfg);
        assert!(w.coordinator.is_some());
        // ctrl publishes but records nothing; voice0 records but never publishes.
        assert!(w.grant_tx_by_device[0].is_some(), "control publishes");
        assert!(w.cmd_rx_by_device[0].is_none(), "control receives nothing");
        assert!(w.grant_tx_by_device[1].is_none(), "voice never publishes");
        assert!(
            w.cmd_rx_by_device[1].is_some(),
            "voice receives routed grants"
        );
        let coord = w.coordinator.take().unwrap();
        coord.stop.store(true, Ordering::Relaxed);
        drop(w);
        let _ = coord.thread.join();
    }

    // -- conventional channels ------------------------------------------------

    /// A temp directory removed on drop.
    struct Scratch(std::path::PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let p =
                std::env::temp_dir().join(format!("trunkd-{tag}-{}-{nanos}", std::process::id()));
            std::fs::create_dir_all(&p).expect("scratch dir");
            Scratch(p)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const CONV_RATE: u32 = 1_000_000;
    const CONV_CENTER: u64 = 154_500_000;
    const CONV_CHANNEL: u64 = 154_250_000;

    /// `secs` of a 1 Msps CF32 capture centred on [`CONV_CENTER`]: a low noise
    /// floor everywhere, and an NBFM carrier on [`CONV_CHANNEL`] keyed over
    /// `keyed` (seconds) with a 1 kHz tone at ±2 kHz and CTCSS 167.9 Hz at
    /// ±600 Hz. Synthesised directly at the wideband rate (one phase
    /// accumulator), which is cheap enough for a debug-build test.
    fn conventional_capture(path: &std::path::Path, secs: f64, keyed: std::ops::Range<f64>) {
        use std::io::Write;
        let fs = f64::from(CONV_RATE);
        let n = (secs * fs) as usize;
        let offset = CONV_CHANNEL as f64 - CONV_CENTER as f64;
        let tau = std::f64::consts::TAU;
        let mut phase = 0.0f64;
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut noise = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            ((seed >> 40) as f64 / f64::from(1u32 << 24) - 0.5) as f32 * 0.02
        };
        let mut out = std::io::BufWriter::new(std::fs::File::create(path).expect("capture"));
        for i in 0..n {
            let t = i as f64 / fs;
            let (mut re, mut im) = (noise(), noise());
            if keyed.contains(&t) {
                re += 0.1 * phase.cos() as f32;
                im += 0.1 * phase.sin() as f32;
            }
            let dev = 2_000.0 * (tau * 1_000.0 * t).sin() + 600.0 * (tau * 167.9 * t).sin();
            phase = (phase + tau * (offset + dev) / fs).rem_euclid(tau);
            out.write_all(&re.to_le_bytes()).expect("write");
            out.write_all(&im.to_le_bytes()).expect("write");
        }
        out.flush().expect("flush");
    }

    /// Every `.json` sidecar under the spool's `YYYY/MM/DD` tree.
    fn sidecars(dir: &std::path::Path) -> Vec<serde_json::Value> {
        let mut out = Vec::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().is_some_and(|x| x == "json") {
                    let text = std::fs::read_to_string(&p).expect("sidecar");
                    out.push(serde_json::from_str(&text).expect("sidecar json"));
                }
            }
        }
        out
    }

    fn conventional_config(capture: &std::path::Path, spool: &std::path::Path) -> Config {
        let toml = format!(
            r#"
            [uplink]
            spool_only = true
            spool_path = '{spool}'
            audio_format = "wav"
            [recorder]
            instance_id = "conv-test"
            [[device]]
            id = "vhf"
            backend = "iqfile"
            path = '{capture}'
            sample_rate = {CONV_RATE}
            center_frequency = {CONV_CENTER}
            pacing = "unthrottled"
            [[channel]]
            name = "Fire Dispatch"
            system = "Example County Fire"
            site = "Example Hill"
            frequency = {CONV_CHANNEL}
            tone = "167.9"
            talkgroup = 14
            [[channel]]
            name = "Airband"
            frequency = 154300000
            mode = "am"
            "#,
            spool = spool.display(),
            capture = capture.display(),
        );
        toml::from_str(&toml).expect("conventional config")
    }

    /// A config with conventional channels and no `[[site]]` runs: the
    /// channels-only device builds its own bank, streams, and records the
    /// keyed carrier as one analog call filed per the keyup upload contract. The AM channel is
    /// skipped.
    #[test]
    fn a_channels_only_recorder_streams_and_records() {
        let scratch = Scratch::new("conv-run");
        let capture = scratch.0.join("vhf.cf32");
        let spool = scratch.0.join("spool");
        conventional_capture(&capture, 5.0, 1.0..3.5);
        let cfg = conventional_config(&capture, &spool);
        cfg.validate().expect("a channels-only config validates");

        let summary =
            run_once(&cfg, Arc::new(AtomicBool::new(false))).expect("a channels-only run");
        assert_eq!(summary.calls_started, 0, "no P25 traffic");

        let calls = sidecars(&spool);
        assert_eq!(calls.len(), 1, "one keyed carrier, one call: {calls:#?}");
        let c = &calls[0];
        assert_eq!(c["audio_type"], "analog");
        assert_eq!(c["short_name"], "Example County Fire");
        assert_eq!(c["site"], "Example Hill");
        assert_eq!(c["talkgroup"], 14);
        assert_eq!(c["talkgroup_group_tag"], "Fire Dispatch");
        assert_eq!(c["freq"], CONV_CHANNEL);
        assert_eq!(c["srcList"], serde_json::json!([]));
        assert_eq!(c["mode"], "nbfm");
        assert_eq!(c["decoder"], "NBFM");
        assert_eq!(c["tone"], "167.9");
        assert_eq!(c["toneDetected"], "167.9");
        assert_eq!(c["squelchReason"], "squelch_closed");
        assert!(c["snrDb"].as_f64().is_some_and(|v| v > 10.0), "{c}");
        let len_ms = c["call_length_ms"].as_i64().expect("length");
        assert!(
            (2_250..=2_750).contains(&len_ms),
            "a 2.5 s transmission recorded as {len_ms} ms"
        );
        let wav = spool_audio_len(&spool);
        assert!(
            wav > 2 * 8_000 * 2,
            "the call carries its audio ({wav} bytes)"
        );
    }

    /// Total bytes of `.wav` files under the spool.
    fn spool_audio_len(dir: &std::path::Path) -> u64 {
        let mut total = 0;
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().is_some_and(|x| x == "wav") {
                    total += e.metadata().map(|m| m.len()).unwrap_or(0);
                }
            }
        }
        total
    }

    /// Without channels nothing about the run's sizing changes.
    #[test]
    fn the_call_queue_grows_only_with_channels() {
        assert_eq!(call_queue_depth(0), CALL_QUEUE_DEPTH);
        assert_eq!(call_queue_depth(11), CALL_QUEUE_DEPTH + 88);
        let p25 = cfg_with(Uplink::default());
        assert!(crate::pipeline::conventional::channel_specs(&p25).is_empty());
    }

    /// A channel's hang lengthens the orphan bound only when it would exceed
    /// the sites' own; a P25-only config keeps the bound it always had.
    #[test]
    fn the_orphan_bound_counts_channel_hang_only_when_channels_exist() {
        let p25 = cfg_with(Uplink::default());
        let before = orphan_after(&p25);
        let mut with_channel = p25.clone();
        with_channel.channel = conventional_config(
            std::path::Path::new("/dev/null"),
            std::path::Path::new("/tmp/unused"),
        )
        .channel;
        with_channel.channel[0].squelch = Some(crate::config::Squelch {
            hang_ms: Some(30_000),
            ..Default::default()
        });
        assert_eq!(orphan_after(&p25), before);
        assert_eq!(
            orphan_after(&with_channel),
            Duration::from_millis((30_000 + 750) * 10)
        );
    }

    /// A device carrying only channels is described to keyup with its first
    /// channel's site label (and no site gauges); P25 devices are unchanged.
    #[test]
    fn a_channels_only_tuner_reports_its_channel_site() {
        let cfg = conventional_config(
            std::path::Path::new("/dev/null"),
            std::path::Path::new("/tmp/unused"),
        );
        let channels = crate::pipeline::conventional::channel_specs(&cfg);
        let d = tuner_descriptors(&cfg, &[], &channels);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].site_label.as_deref(), Some("Example Hill"));
        assert_eq!(d[0].site_metric_prefix, None);
        assert_eq!(tuner_descriptors(&cfg, &[], &[])[0].site_label, None);
    }
}
