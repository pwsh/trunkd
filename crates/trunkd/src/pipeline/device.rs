// SPDX-License-Identifier: GPL-3.0-or-later
//! The device unit: an SDR thread feeding a DSP thread, supervised as one.
//!
//! [`DeviceUnit`] is a [`Supervised`] whose `run` owns a whole capture session:
//! it opens the tuner (switching a PortaPack out of its UI first when asked),
//! builds one [`ChannelBank`](trunkd_p25::dsp::ChannelBank) and one [`SiteEngine`] per enabled site on the
//! device, and then drains the ring in large blocks until it is told to stop
//! or the stream fails.
//!
//! # One channelizer per device
//!
//! The polyphase filterbank is the DSP thread's largest cost (~4 GFLOP/s at
//! 20 Msps) and it does not depend on which site is listening, so the
//! **device** owns it: `build_engines` makes one `ChannelBank` for the
//! capture and attaches every site's engine to it
//! ([`SiteEngine::with_bank`]). Per block, `pump` runs the filterbank once
//! ([`ChannelBank::process_block`](trunkd_p25::dsp::ChannelBank::process_block)) and then lets each engine read the
//! channels it subscribed ([`SiteEngine::process_channels`]). Before this,
//! each `[[site]]` built its own filterbank over the same samples, and four
//! sites on one HackRF overran the thread (2026-09-16; review 2026-09-22
//! §3.1 O1).
//!
//! What stays per site is everything that was: each engine hunts its own
//! control candidates, follows its own grants within its own
//! `traffic_pool_size`, publishes out-of-band grants to the coordinator,
//! follows data, and keeps its own noise-floor window and metric prefix. The
//! bank is the device's shared slot table: a channel two sites both want (one
//! control frequency listed by two sites, a neighbour's traffic) is produced
//! once and read by both, and released when the last of them lets go. The
//! bank and the engines live and die together on this thread (`Rc`, not a
//! lock); a rebuild makes a fresh set of both.
//!
//! Failure is the supervisor's problem, not ours: a tuner error — or, for
//! hardware, end-of-stream — returns `Err`, and the supervisor rebuilds the
//! unit after a backoff. An IQ file running out is the *expected* end of an
//! offline replay, so it returns `Ok(())` and sets the `finished` flag, which
//! is what tells `run_once` the job is done.
//!
//! # Why blocks, and why this big
//!
//! The channelizer's cost is dominated by per-block bookkeeping, so the DSP
//! thread waits for at least [`BLOCK_SAMPLES`] (64 Ki, 3.3 ms at 20 Msps)
//! before doing anything. Samples are handed to the engine *in place* out of
//! the `rtrb` read chunk — a chunk that wraps the ring's end arrives as two
//! slices and is fed as two blocks — so no sample is ever copied and nothing
//! on this thread allocates once the engines are built.

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use trunkd_core::chan::{BoundedReceiver, BoundedSender};
use trunkd_core::{Hz, SampleRate};
use trunkd_p25::dsp::bank::SharedChannelBank;
use trunkd_p25::site::{EngineRole, SiteConfig, SiteEngine, SiteEvent};
use trunkd_sdr::{StreamConfig, TunerDevice, TunerEvent, TunerStream};
use trunkd_supervisor::{RunError, Supervised, SupervisionContext};

use crate::config::{resolve_band, Backend, Config, Device, DeviceRole, PacingName};
use crate::pipeline::autoppm::{AutoPpm, AutoPpmInbox};
use crate::pipeline::control::TunerControl;
use crate::pipeline::coordinator::{GrantMsg, TrafficCmd};
use crate::pipeline::ppmmemory;
use crate::pipeline::spectrum::{SpectrumHook, SpectrumTap};
use crate::pipeline::streamclock::{StampFloor, StreamClock};
use crate::pipeline::{CallMsg, ObsMsg, Shared, StreamEpoch};

/// Samples the DSP thread waits for before running a block.
///
/// 64 Ki complex samples: 3.3 ms at 20 Msps, 26 ms at 2.4 Msps.
pub(crate) const BLOCK_SAMPLES: usize = 64 * 1024;

/// Recorder-wide count of calls refused because they were granted on a Phase 2
/// (TDMA) channel. Reported to keyup as `counters.calls_tdma_refused`.
pub(crate) const TDMA_REFUSED: &str = "calls.tdma_refused";

/// The same count per site, under the site's metric prefix
/// (`site.N.calls_tdma_refused`); `/api/status` shows it on each site row.
pub(crate) const SITE_TDMA_REFUSED: &str = "calls_tdma_refused";

/// How long the DSP thread sleeps when the ring has nothing for it.
///
/// Short enough to stay well inside the ring's headroom, long enough that an
/// idle recorder does not spin a core.
const IDLE_SLEEP: Duration = Duration::from_micros(500);

/// Floor on the "the samples stopped coming" gap (see `stream_gaps`).
///
/// A block is 3.3 ms at 20 Msps, so twice a block duration is well inside
/// ordinary scheduler jitter and would count noise. Fifty milliseconds is
/// still two orders of magnitude below the stall this exists to notice.
const MIN_STREAM_GAP: Duration = Duration::from_millis(50);

/// How often a tuner that is idling (disabled, or serving no site) says so
/// again. Long enough to be silent in a log, often enough that an operator
/// reading any recent slice of one is told why the recorder is quiet.
const IDLE_REMINDER: Duration = Duration::from_secs(300);

/// Ring capacity for a device: `ring_seconds` of samples, rounded up to a
/// whole number of DSP blocks.
///
/// The ring is the only slack between an SDR that never pauses and a DSP thread
/// that can be descheduled. The original 0.2 s was measured to be too shallow:
/// a `cargo` build running beside a recorder on the same machine stalled
/// the DSP thread for about 2 s and the ring dropped 4.6 M samples. The default
/// is now 1 s (`[[device]] ring_seconds`), which the sizing below rounds up to
/// 2^25 samples — 256 MiB of `Complex<f32>` at 20 Msps. Real memory, which is
/// why it is configurable rather than simply large.
///
/// Sized as the power of two at or above `ring_seconds` of samples, then rounded
/// up to whole DSP blocks. The power of two is the depth every recorder has run
/// with since the ring was introduced (1.68 s at 20 Msps for `ring_seconds = 1`),
/// and it is kept on purpose: a sample the ring drops is audio lost for good,
/// while a backlogged DSP thread running behind the air only delays it — and
/// since the stream clock accounts for drops and backlog (review 2026-09-22, R1)
/// that delay no longer skews a single timestamp. Whole blocks keep the stream
/// clock's drop accounting exact. `rtrb` itself has no power-of-two requirement.
///
/// Never smaller than two DSP blocks, or the "wait for a full block" rule could
/// never be satisfied.
pub(crate) fn ring_capacity_for(sample_rate: u32, ring_seconds: f64) -> usize {
    let want = (f64::from(sample_rate) * ring_seconds.max(0.0)) as usize;
    want.max(BLOCK_SAMPLES * 2)
        .next_power_of_two()
        .div_ceil(BLOCK_SAMPLES)
        .saturating_mul(BLOCK_SAMPLES)
}

/// Stride between samples measured for the wideband RSSI gauge.
///
/// One sample in 64 — 1024 of a 64 Ki block. That is plenty to average the
/// modulation out of a wideband capture, and it is the cost ceiling: the meter
/// makes a single strided pass over a block the DSP thread is about to read
/// anyway, touching one 8-byte sample per 512-byte step, and never a second
/// full pass. Blocks shorter than the stride (the ragged last block of an IQ
/// file) are measured whole.
const RSSI_STRIDE: usize = 64;

/// EMA weight for one block's mean power.
///
/// A single 3.3 ms block is a noisy estimate — a strong burst inside it reads
/// as a hot receiver — so blocks are smoothed before the gauge is published.
/// 0.1 settles within roughly a dozen blocks (~40 ms at 20 Msps, well inside
/// the 1 s publish cadence) while still tracking a real level change between
/// two status POSTs.
const RSSI_ALPHA: f32 = 0.1;

/// Floor reported instead of `-inf` for a dead-silent capture.
///
/// A zeroed block (a source feeding nothing, a muted front end) has no dB
/// value at all; `-inf` would serialize as JSON `null` and read downstream as
/// "not measured" rather than "measured, and there is nothing there". -120
/// dBFS is far below any real capture and finite.
pub(crate) const RSSI_FLOOR_DBFS: f64 = -120.0;

/// dBFS for a mean power, where full scale is a unit-magnitude sample.
///
/// **What the scale is:** the ring carries [`trunkd_core::IqSample`]
/// (`Complex<f32>`) already normalized by the backend — the HackRF path divides
/// its i8 pairs by 128 — so a sample riding the converter's rails has
/// magnitude ~1.0 and reads 0 dBFS. This is therefore ADC-referenced, *not* an
/// absolute power: it moves with the LNA/VGA/amp settings and says nothing
/// about dBm at the antenna. Reported alongside those gain settings for exactly
/// that reason.
pub(crate) fn power_to_dbfs(mean_power: f64) -> f64 {
    // NaN is tested explicitly rather than by negating a comparison (the same
    // discipline as `ring_seconds` validation): a NaN power must land on the
    // floor, not slip through as a dB value.
    if mean_power.is_nan() || mean_power <= 0.0 {
        return RSSI_FLOOR_DBFS;
    }
    (10.0 * mean_power.log10()).max(RSSI_FLOOR_DBFS)
}

/// Wideband received-level meter over the raw capture, published once a second
/// as the `device.{id}.rssi_dbfs` gauge.
///
/// Wideband is the point: it is the level the front end actually sees across
/// the whole span, which is what tells an operator (and, later, keyup choosing
/// between recorders) whether a tuner is deaf, sensible, or being driven into
/// compression. Per-channel SNR is a different measurement and not this one.
#[derive(Debug, Default)]
pub(crate) struct RssiMeter {
    /// Smoothed mean power in linear units; dB conversion is deferred to the
    /// publish, so the hot path never calls `log10`.
    ema: f32,
    /// Whether any block has been measured yet (an unprimed meter reports
    /// nothing rather than a fabricated 0.0).
    primed: bool,
}

impl RssiMeter {
    /// Fold one block of samples into the EMA. Allocation-free, and a single
    /// strided read of a block that is already in cache.
    pub(crate) fn observe(&mut self, block: &[trunkd_core::IqSample]) {
        if block.is_empty() {
            return;
        }
        let stride = if block.len() >= RSSI_STRIDE {
            RSSI_STRIDE
        } else {
            1
        };
        let mut sum = 0.0f32;
        let mut n = 0u32;
        for s in block.iter().step_by(stride) {
            sum += s.re * s.re + s.im * s.im;
            n += 1;
        }
        let mean = sum / n as f32;
        if self.primed {
            self.ema += RSSI_ALPHA * (mean - self.ema);
        } else {
            self.ema = mean;
            self.primed = true;
        }
    }

    /// The current level in dBFS, or `None` when nothing has been measured yet.
    pub(crate) fn dbfs(&self) -> Option<f64> {
        self.primed.then(|| power_to_dbfs(f64::from(self.ema)))
    }
}

/// Resolve what this board remembers, and what to open it with.
///
/// Runs on every build (a keyup config push rebuilds the unit), which is
/// deliberate: a `ppm` the operator just changed has to re-base the remembered
/// delta on the very next open, not on the next process start.
///
/// The three gauges published here are how the status report and the panel see
/// the memory. They are published whether or not it was *applied*: an operator
/// who has turned `auto_ppm` off owns `ppm` outright, but still wants to see
/// the number the recorder measured — that is where the value they would type
/// into the TOML comes from.
fn resolve_ppm_memory(cfg: &Config, device: &Device, shared: &Shared) -> DevicePpmMemory {
    let path = ppmmemory::memory_path(cfg);
    let board = ppmmemory::board_key(
        device.backend,
        device.driver.as_deref(),
        device.serial.as_deref(),
        &device.id,
    );
    let mut resolved = DevicePpmMemory {
        path,
        board,
        open_ppm: device.ppm,
        seed_ppm: 0.0,
        applied_at_open: false,
    };
    // An IQ file has no oscillator: whatever error its capture carries belongs
    // to the board that recorded it, on a day this recorder will never see
    // again. Remembering it would poison a real board that later takes the
    // same device id.
    if device.backend == Backend::Iqfile {
        return resolved;
    }
    let Some(entry) = ppmmemory::load(&resolved.path)
        .get(&resolved.board)
        .cloned()
    else {
        return resolved;
    };

    let ppm_gauge = shared
        .metrics
        .gauge(&format!("device.{}.ppm_memory_ppm", device.id));
    let at_gauge = shared
        .metrics
        .gauge(&format!("device.{}.ppm_memory_at_ms", device.id));
    let open_gauge = shared
        .metrics
        .gauge(&format!("device.{}.ppm_memory_at_open", device.id));

    if device.auto_ppm {
        let (open_ppm, seed, clamped) =
            ppmmemory::open_ppm(&entry, device.ppm, device.auto_ppm_limit);
        if clamped {
            tracing::warn!(
                device = %device.id,
                board = %resolved.board,
                remembered_ppm = entry.ppm,
                limit_ppm = device.auto_ppm_limit,
                "remembered correction exceeds auto_ppm_limit; opening on the limit"
            );
        }
        resolved.open_ppm = open_ppm;
        resolved.seed_ppm = seed;
        resolved.applied_at_open = true;
        tracing::info!(
            device = %device.id,
            board = %resolved.board,
            config_ppm = device.ppm,
            remembered_ppm = entry.ppm,
            open_ppm,
            measured_at = %entry.measured_at,
            samples = entry.samples,
            "opening on this board's remembered frequency correction"
        );
    } else {
        // Not applied, and said out loud: an operator who turned the loop off
        // and then wonders why their board still needs a hunt should find the
        // number they could have typed sitting in the log.
        tracing::info!(
            device = %device.id,
            board = %resolved.board,
            remembered_ppm = entry.ppm,
            "auto_ppm is off; the remembered correction is reported but not applied"
        );
    }

    ppm_gauge.set_f64(entry.ppm);
    at_gauge.set(entry.measured_at_ms().unwrap_or(0).max(0) as u64);
    open_gauge.set(u64::from(resolved.applied_at_open));
    resolved
}

/// Map a config [`DeviceRole`] to the engine's [`EngineRole`].
fn engine_role(role: DeviceRole) -> EngineRole {
    match role {
        DeviceRole::Auto => EngineRole::Auto,
        DeviceRole::Control => EngineRole::Control,
        DeviceRole::Voice => EngineRole::Voice,
        DeviceRole::Data => EngineRole::Data,
    }
}

/// One device's frequency-correction loop and the gauges it publishes.
///
/// Built per stream. The board is re-opened with the configured `ppm` *plus*
/// whatever this board remembers (see [`crate::pipeline::ppmmemory`]), and the
/// loop is seeded with that remembered part — so a restart resumes from the
/// offset already measured rather than re-deriving it, while the limit
/// accounting still covers the whole correction in force.
struct AutoPpmLoop {
    state: AutoPpm,
    /// `device.{id}.auto_ppm` — the total correction as ppm, for the panel.
    ppm_gauge: trunkd_core::Metric,
    /// `device.{id}.auto_correction_hz` — the same total in Hz.
    correction_gauge: trunkd_core::Metric,
    /// Cleared for good the first time the backend refuses a runtime retune.
    enabled: bool,
    /// Where this board's correction is written down, and what to write it
    /// under. Every correction persists immediately; the drop below is the
    /// clean-shutdown re-assert.
    memory: PpmMemoryWriter,
}

/// The persistence half of [`AutoPpmLoop`]: which file, which board, and what
/// the `[[device]] ppm` was when the number was measured.
struct PpmMemoryWriter {
    path: PathBuf,
    board: String,
    /// The `[[device]] ppm` (TOML or keyup) this run was opened with, *not*
    /// the remembered total laid on top of it — that is what makes the stored
    /// delta re-basable when an operator edits `ppm`.
    config_ppm: f64,
    /// Whether this run has actually corrected anything worth flushing.
    corrected: bool,
}

impl PpmMemoryWriter {
    /// Persist `total_ppm` (the loop's correction) as this board's total.
    fn write(&self, total_ppm: f64, samples: ppmmemory::Samples) {
        ppmmemory::record(
            &self.path,
            &self.board,
            self.config_ppm + total_ppm,
            self.config_ppm,
            samples,
        );
    }
}

impl Drop for AutoPpmLoop {
    /// The clean-shutdown write. Every correction has already persisted, so
    /// this only re-asserts the final number — with the sample count left
    /// alone, because a shutdown is not a measurement.
    fn drop(&mut self) {
        if self.memory.corrected {
            self.memory
                .write(self.state.total_ppm(), ppmmemory::Samples::Keep);
        }
    }
}

/// One enabled site's engine parameters, resolved from config.
struct SiteEngineSpec {
    /// Index into [`Shared::sites`].
    index: usize,
    /// Whether this device is the only one serving the site. Only then may it
    /// zero the site's `site.{index}.*` decode gauges when it stops decoding:
    /// on a site served by two tuners those gauges belong to whichever is
    /// still up, and clearing them would report a live tuner as dead.
    sole_tuner: bool,
    /// Engine configuration.
    engine: SiteConfig,
}

/// A device and its DSP loop, supervised as one unit.
pub(crate) struct DeviceUnit {
    device: Device,
    /// This device's index in `cfg.device`, the tuner id used to namespace call
    /// ids and to pick this device's epoch.
    tuner_index: usize,
    sites: Vec<SiteEngineSpec>,
    shared: Shared,
    /// This device's own stream timeline (per-tuner, not shared).
    epoch: Arc<StreamEpoch>,
    calls_tx: Arc<BoundedSender<CallMsg>>,
    obs_tx: Arc<BoundedSender<ObsMsg>>,
    /// Where a control/auto front publishes grants it does not record itself.
    /// `None` in a single-tuner run: grants are followed inline on the fast
    /// path and never cross a thread.
    grant_tx: Option<Arc<BoundedSender<GrantMsg>>>,
    /// Where a voice/data front receives follow commands from the coordinator.
    /// `None` for a control/auto-only tuner.
    cmd_rx: Option<Arc<BoundedReceiver<TrafficCmd>>>,
    /// Set when the source ended cleanly and there is nothing left to record.
    finished: Arc<AtomicBool>,
    /// Overflow samples/sec at or above which a warning is logged.
    overflow_warn_per_sec: u64,
    /// keyup control-loop hook: idle when disabled, restart on a config change.
    /// `None` keeps the plain single-tuner behavior (always on, never
    /// self-restarts) for tests and configs with no control loop.
    control: Option<TunerControl>,
    /// Opt-in spectrum emission (W4): the live on/off flag and the frame sink.
    /// `None` when no control loop is wired (tests). When present, the tap does
    /// one atomic load per block and nothing else until keyup enables it.
    spectrum: Option<SpectrumHook>,
    /// The site whose control lock drives automatic frequency correction, if
    /// this device runs it at all. `None` when `auto_ppm` is off, when the
    /// device has no center frequency to correct, or when it follows traffic
    /// rather than hunting control — a voice front never sees the hunt.
    auto_ppm_site: Option<usize>,
    /// What this board remembers, resolved once per build: the file, the board
    /// key, the ppm the backend is opened with, and the part of it that came
    /// from memory. See [`crate::pipeline::ppmmemory`].
    ppm_memory: DevicePpmMemory,
}

/// One device's resolved ppm memory, fixed at build time.
struct DevicePpmMemory {
    /// The state file for the whole recorder.
    path: PathBuf,
    /// This board's key in it.
    board: String,
    /// The ppm actually handed to the backend at open: `[[device]] ppm` plus
    /// the remembered delta, or just `[[device]] ppm` when there is nothing to
    /// remember or `auto_ppm` is off.
    open_ppm: f64,
    /// The remembered delta seeded into the correction loop; 0 when none was
    /// applied.
    seed_ppm: f64,
    /// Whether the remembered entry was applied to this open, for the status
    /// report. False both when there is no entry and when `auto_ppm` is off —
    /// the operator owns `ppm` then, and memory is shown but not used.
    applied_at_open: bool,
}

impl DeviceUnit {
    /// Assemble the unit for `device` from the loaded configuration.
    ///
    /// `site_index_of` maps a config site index to its slot in
    /// [`Shared::sites`], so message routing and the status line agree.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        cfg: &Config,
        device: &Device,
        tuner_index: usize,
        shared: Shared,
        epoch: Arc<StreamEpoch>,
        calls_tx: Arc<BoundedSender<CallMsg>>,
        obs_tx: Arc<BoundedSender<ObsMsg>>,
        grant_tx: Option<Arc<BoundedSender<GrantMsg>>>,
        cmd_rx: Option<Arc<BoundedReceiver<TrafficCmd>>>,
        finished: Arc<AtomicBool>,
        control: Option<TunerControl>,
        spectrum: Option<SpectrumHook>,
        site_index_of: &dyn Fn(usize) -> Option<usize>,
    ) -> Self {
        let center_hz = device.center_frequency.unwrap_or(0) as f64;
        let defaults = SiteConfig::default();
        let role = engine_role(device.role);
        // Band labels resolve to inclusive downlink ranges; validation has
        // already rejected any that do not.
        let bands: Vec<(u64, u64)> = device
            .bands
            .iter()
            .filter_map(|b| resolve_band(b))
            .collect();
        // Call ids are namespaced by tuner so two tuners serving one site never
        // collide (multi-tuner core, W1). The single-tuner run keeps base 0.
        let call_id_base = (tuner_index as u64) << 48;
        let sites: Vec<SiteEngineSpec> = cfg
            .site
            .iter()
            .enumerate()
            .filter(|(_, s)| s.enabled && s.device_ids().contains(&device.id.as_str()))
            .filter_map(|(cfg_idx, s)| {
                let index = site_index_of(cfg_idx)?;
                // An auto front publishes out-of-band grants only when the site
                // is served by more than one tuner and a grant bus exists; a
                // control front always publishes.
                // With the recorder link on, "another tuner" can be in
                // another process, so a lone auto front publishes too — that
                // is what gives a peer anything to record.
                let site_multi_tuner = s.device_ids().len() > 1 || cfg.link.enabled;
                let publish_grants = grant_tx.is_some()
                    && (role == EngineRole::Control
                        || (role == EngineRole::Auto && site_multi_tuner));
                Some(SiteEngineSpec {
                    index,
                    sole_tuner: s.device_ids().len() == 1,
                    engine: SiteConfig {
                        sample_rate: device.sample_rate,
                        center_hz,
                        control_frequencies: s.control_frequencies.clone(),
                        traffic_pool_size: s.traffic_pool_size,
                        hang_ms: s.hang_ms.unwrap_or(defaults.hang_ms),
                        call_inactivity_ms: s
                            .call_inactivity_ms
                            .unwrap_or(defaults.call_inactivity_ms),
                        role,
                        bands: bands.clone(),
                        publish_grants,
                        call_id_base,
                        // Opt-in SNDCP data-channel follow (default off). On a
                        // downlink capture this gives LRRP transport detection +
                        // PDU logging, not coordinates (those are uplink; W5).
                        data_follow: s.data_follow,
                        // Recorder-wide rather than per-tuner: whether private
                        // calls belong on the archive is one decision for the
                        // box, and a per-tuner switch would let one tuner of a
                        // site record half a conversation.
                        follow_unit_calls: cfg.recorder.follow_unit_calls,
                        // Recorder-wide, and shared rather than copied: keyup
                        // rewrites this list at runtime and every engine must
                        // see the change on its next grant.
                        skip_talkgroups: Some(shared.skip.clone()),
                        // Site observations are folded from the raw
                        // control-channel event stream, so the engine has to
                        // pass it through.
                        raw_control: true,
                        // Traffic-channel signaling is only wanted when the
                        // event log will write it; leaving it off keeps the
                        // events off the queue entirely rather than
                        // enqueueing them to be discarded.
                        raw_traffic: cfg.recorder.event_log
                            && !cfg.recorder.event_log_dir.is_empty()
                            && cfg.recorder.event_log_traffic,
                        ..defaults.clone()
                    },
                })
            })
            .collect();
        // Correction is driven by the control hunt, so it needs a site that
        // hunts and a frequency to move. A device with several sites corrects
        // from the first: they share one oscillator, so one measurement is
        // enough and two loops on one board would fight.
        let auto_ppm_site = (device.auto_ppm
            && device.center_frequency.unwrap_or(0) != 0
            && matches!(role, EngineRole::Auto | EngineRole::Control))
        .then(|| sites.first().map(|s: &SiteEngineSpec| s.index))
        .flatten();

        let ppm_memory = resolve_ppm_memory(cfg, device, &shared);

        DeviceUnit {
            device: device.clone(),
            tuner_index,
            sites,
            shared,
            epoch,
            calls_tx,
            obs_tx,
            grant_tx,
            cmd_rx,
            finished,
            overflow_warn_per_sec: cfg.health.overflow_warn_per_sec,
            control,
            spectrum,
            auto_ppm_site,
            ppm_memory,
        }
    }

    /// Build this device's spectrum tap, if spectrum emission is wired. The FFT
    /// machinery inside is not allocated until keyup first enables the flag.
    fn spectrum_tap(&self) -> Option<SpectrumTap> {
        self.spectrum.clone().map(|hook| {
            SpectrumTap::new(
                hook,
                self.device.id.clone(),
                self.device.center_frequency.unwrap_or(0),
                // The FFT covers the whole capture, so its span is the sample rate.
                u64::from(self.device.sample_rate),
                self.shared
                    .metrics
                    .counter(&format!("device.{}.spectrum_frames", self.device.id)),
            )
        })
    }

    /// Open the configured backend and start it streaming.
    ///
    /// Open and start are one step because [`TunerDevice::start`] consumes a
    /// sized `Box<Self>` and so cannot be called through a trait object.
    fn start_stream(&self) -> Result<TunerStream, RunError> {
        let rate = SampleRate(self.device.sample_rate);
        let center = Hz(self.device.center_frequency.unwrap_or(0));
        let stream_cfg = StreamConfig {
            ring_capacity: ring_capacity_for(self.device.sample_rate, self.device.ring_seconds),
            event_capacity: 64,
        };
        let started = match self.device.backend {
            Backend::Hackrf => self.start_hackrf(center, rate, stream_cfg),
            Backend::Iqfile => self.start_iqfile(center, rate, stream_cfg),
            Backend::Soapy => self.start_soapy(center, rate, stream_cfg),
        };
        started.map_err(|e| format!("device '{}': {e}", self.device.id))
    }

    fn start_hackrf(
        &self,
        center: Hz,
        rate: SampleRate,
        stream_cfg: StreamConfig,
    ) -> trunkd_core::Result<TunerStream> {
        use trunkd_sdr::hackrf::{HackRfConfig, HackRfTuner};

        // A board sitting in the PortaPack UI does not answer as a HackRF, so
        // the mode switch has to happen before the tuner is opened. It is a
        // no-op when a board is already present.
        if let Some(port) = &self.device.portapack_port {
            trunkd_sdr::portapack::ensure_hackrf_mode(Path::new(port), Duration::from_secs(10))?;
            tracing::info!(device = %self.device.id, port, "board is in HackRF mode");
        }

        let d = HackRfConfig::new(center, rate);
        let hw = HackRfConfig {
            serial: self.device.serial.clone(),
            lna_db: self.device.lna_db.unwrap_or(d.lna_db),
            vga_db: self.device.vga_db.unwrap_or(d.vga_db),
            amp: self.device.amp.unwrap_or(d.amp),
            // The remembered offset is already folded in, so the board comes
            // up on its known correction and locks without a hunt.
            ppm: self.ppm_memory.open_ppm,
            ..d
        };
        Box::new(HackRfTuner::open(hw)?).start(stream_cfg, &self.shared.metrics)
    }

    /// Open any SoapySDR-supported radio: Airspy, SDRplay, RTL-SDR, ...
    ///
    /// The library is `dlopen`ed inside the backend, so this costs nothing on a
    /// recorder that has no Soapy device configured. Everything driver-specific
    /// travels in [`SoapyOptions`]; the generic rate/frequency/gain/ppm come
    /// from [`trunkd_sdr::TunerConfig`] like every other backend.
    fn start_soapy(
        &self,
        center: Hz,
        rate: SampleRate,
        stream_cfg: StreamConfig,
    ) -> trunkd_core::Result<TunerStream> {
        use trunkd_sdr::{SoapyOptions, SoapyTuner};

        let mut tuner_cfg = trunkd_sdr::TunerConfig::new(center, rate);
        tuner_cfg.gain = self.device.gain;
        tuner_cfg.ppm = self.ppm_memory.open_ppm;

        let options = SoapyOptions {
            bandwidth_hz: self.device.bandwidth_hz.map(f64::from),
            rx_port: self.device.rx_port.clone(),
            agc: self.device.agc,
            gains: self.device.gains.clone(),
            settings: self.device.settings.clone(),
        };
        let args = self.device.effective_soapy_args();
        let tuner = SoapyTuner::open_with(&args, tuner_cfg, options)?;
        // What the board actually has, learned from the driver at open. Pushed
        // into the registry so the status POST can offer keyup the real element
        // names and limits, and so the next pushed `gains` map is validated
        // against this device rather than against a guess.
        if let Some(c) = &self.control {
            c.report_gain_elements(
                tuner
                    .gain_element_ranges()
                    .into_iter()
                    .map(|e| crate::pipeline::control::GainElement {
                        name: e.name,
                        min: e.min,
                        max: e.max,
                        step: e.step,
                    })
                    .collect(),
            );
        }
        Box::new(tuner).start(stream_cfg, &self.shared.metrics)
    }

    fn start_iqfile(
        &self,
        center: Hz,
        rate: SampleRate,
        stream_cfg: StreamConfig,
    ) -> trunkd_core::Result<TunerStream> {
        use trunkd_sdr::iqfile::{IqFileTuner, IqFormat, Pacing};

        let path = self
            .device
            .path
            .as_deref()
            .ok_or_else(|| trunkd_core::Error::config("iqfile backend needs a `path`"))?;
        let pacing = match self.device.pacing {
            PacingName::Realtime => Pacing::RealTime,
            PacingName::Unthrottled => Pacing::Unthrottled,
        };
        let mut tuner_cfg = trunkd_sdr::TunerConfig::new(center, rate);
        tuner_cfg.ppm = self.device.ppm;
        let tuner = IqFileTuner::open(
            path,
            IqFormat::from_extension(Path::new(path)),
            tuner_cfg,
            pacing,
        )?
        // A batch the size of the DSP block keeps producer and consumer in
        // step; the default 8 Ki would wake the DSP thread eight times per
        // block it can actually use.
        .with_batch_size(BLOCK_SAMPLES)
        // Every unthrottled replay of this run on one clock, so a grant the
        // control front decodes reaches a voice front replaying the same
        // capture at the same moment of its file — not seconds after its DSP
        // thread raced past the call (T-B8).
        .with_lockstep(self.shared.replay_clock.clone());
        Box::new(tuner).start(stream_cfg, &self.shared.metrics)
    }

    /// Build this device's one channel bank and one engine per enabled site on
    /// the device, every engine attached to that bank.
    ///
    /// Every site's [`SiteConfig`] is built from this device's rate and centre
    /// (see [`DeviceUnit::new`]), so the first one describes the capture for
    /// all of them; an engine whose config disagreed would be refused by
    /// [`SiteEngine::with_bank`] rather than silently decoding the wrong band.
    fn build_engines(&self) -> Result<(SharedChannelBank, Vec<(usize, SiteEngine)>), RunError> {
        let first = self
            .sites
            .first()
            .ok_or_else(|| format!("device '{}': no enabled site uses it", self.device.id))?;
        let bank = SiteEngine::bank_for(&first.engine)
            .map_err(|e| format!("site '{}': {e}", self.shared.sites[first.index].display))?;
        let engines = self
            .sites
            .iter()
            .map(|s| {
                let spec = &self.shared.sites[s.index];
                let engine = SiteEngine::with_bank(s.engine.clone(), bank.clone())
                    .map_err(|e| format!("site '{}': {e}", spec.display))?;
                let (reachable, configured) = (
                    engine.candidates().len(),
                    s.engine.control_frequencies.len(),
                );
                if reachable < configured {
                    tracing::warn!(
                        site = %spec.display,
                        reachable,
                        configured,
                        "some control candidates fall outside the capture's usable \
                         span (or on the DC bin) and were dropped"
                    );
                }
                self.shared
                    .metrics
                    .gauge(&format!("{}.pool_size", spec.metric_prefix))
                    .set(s.engine.traffic_pool_size as u64);
                Ok((s.index, engine))
            })
            .collect::<Result<Vec<_>, RunError>>()?;
        Ok((bank, engines))
    }

    /// The DSP loop proper: drain, decode, route, repeat.
    ///
    /// `last_block_ms` is left holding the stream time of the last block
    /// processed, so whatever is routed after the loop (the shutdown's call
    /// closes) is stamped at the end of the stream rather than at its start.
    fn pump(
        &self,
        ctx: &SupervisionContext,
        stream: &mut TunerStream,
        bank: &SharedChannelBank,
        engines: &mut [(usize, SiteEngine)],
        spectrum: &mut Option<SpectrumTap>,
        last_block_ms: &mut u64,
    ) -> Result<(), RunError> {
        let mut overflow_seen = stream.overflow_count();
        let mut last_overflow_check = Instant::now();
        let mut ending = false;
        let rate = u64::from(self.device.sample_rate).max(1);
        let overflow_reports = self
            .shared
            .metrics
            .counter(&format!("device.{}.overflow_reports", self.device.id));
        // Published for the keyup control loop: a per-second overflow rate under
        // the *config* id (the ring's own counter is keyed by the backend device
        // id, which the control loop cannot resolve) and a liveness heartbeat so
        // a dead tuner does not report a stale lock.
        let overflow_rate = self
            .shared
            .metrics
            .gauge(&format!("device.{}.overflow_per_s", self.device.id));
        let stream_beat = self
            .shared
            .metrics
            .gauge(&format!("device.{}.stream_beat_ms", self.device.id));
        stream_beat.set(trunkd_core::Timestamp::now().unix_millis().max(0) as u64);
        // Wideband received level, measured per block and published on the same
        // 1 s cadence: keyup reports it so an operator can see which of several
        // recorders is actually hearing a site.
        let rssi_gauge = self
            .shared
            .metrics
            .gauge(&format!("device.{}.rssi_dbfs", self.device.id));
        let mut rssi = RssiMeter::default();
        // Distinct channels the device's one filterbank is producing for all
        // its sites together (a channel two sites share counts once).
        let channels_gauge = self
            .shared
            .metrics
            .gauge(&format!("device.{}.channelizer_channels", self.device.id));
        // Self-correcting stream clock (see `crate::pipeline::streamclock`).
        // The epoch advances by sample count, so samples the host never
        // delivered leave it permanently early; this checks it against the wall
        // clock on the same 1 Hz off-hot-path cadence as the overflow report
        // and re-anchors when the two have genuinely parted company. It also
        // advances the epoch over every ring drop as the consumer reaches it,
        // and measures drift at the newest *produced* sample, so a backlogged
        // DSP thread neither stamps early nor triggers a resync.
        let mut clock = StreamClock::new(rate, BLOCK_SAMPLES, overflow_seen);
        let clock_resyncs = self
            .shared
            .metrics
            .counter(&format!("device.{}.clock_resyncs", self.device.id));
        let last_clock_resync = self
            .shared
            .metrics
            .gauge(&format!("device.{}.last_clock_resync_ms", self.device.id));
        // The slow trim (streamclock::DriftMonitor::trim_step): the averaged
        // error it is working against, and everything it has moved the epoch
        // by. Both signed, stored as two's-complement bits like the resync
        // size; positive means stamps were early.
        let clock_error = self
            .shared
            .metrics
            .gauge(&format!("device.{}.clock_error_ms", self.device.id));
        let clock_trim = self
            .shared
            .metrics
            .gauge(&format!("device.{}.clock_trim_ms", self.device.id));
        // Sample discontinuities: how long this thread *waited* for the next
        // full block, which is the producer's stall and not our own processing
        // time (the timestamp is taken after the previous block is finished).
        // The floor keeps ordinary scheduler jitter off the counter: a block is
        // 3.3 ms at 20 Msps, and twice that is inside the noise.
        let stream_gaps = self
            .shared
            .metrics
            .counter(&format!("device.{}.stream_gaps", self.device.id));
        let block_ms = BLOCK_SAMPLES as u64 * 1000 / rate;
        let gap_limit = Duration::from_millis(2 * block_ms).max(MIN_STREAM_GAP);
        let mut block_ready_at: Option<Instant> = None;
        // Automatic frequency correction, when this device runs it: the inbox
        // is filled by the router inside the engine's event closure and drained
        // below, where the tuner and the engines are reachable again.
        let auto_inbox = self.auto_ppm_site.map(AutoPpmInbox::new);
        let mut auto = self.auto_ppm_site.map(|_| {
            let loop_ = AutoPpmLoop {
                state: AutoPpm::new(
                    self.device.center_frequency.unwrap_or(0),
                    self.device.auto_ppm_limit,
                    self.ppm_memory.seed_ppm,
                ),
                ppm_gauge: self
                    .shared
                    .metrics
                    .gauge(&format!("device.{}.auto_ppm", self.device.id)),
                correction_gauge: self
                    .shared
                    .metrics
                    .gauge(&format!("device.{}.auto_correction_hz", self.device.id)),
                enabled: true,
                memory: PpmMemoryWriter {
                    path: self.ppm_memory.path.clone(),
                    board: self.ppm_memory.board.clone(),
                    config_ppm: self.device.ppm,
                    corrected: false,
                },
            };
            // A seeded correction is in force from the first sample, so it is
            // reported from the first sample: a panel showing "no correction"
            // on a board opened 1 ppm off would be describing a tuner that
            // does not exist.
            loop_.ppm_gauge.set_f64(loop_.state.total_ppm());
            loop_.correction_gauge.set_f64(loop_.state.total_hz());
            loop_
        });
        let router = self.router(auto_inbox.as_ref());

        loop {
            if ctx.should_stop() {
                return Ok(());
            }
            ctx.heartbeat();

            match stream.events.try_recv() {
                Ok(TunerEvent::EndOfStream) => ending = true,
                Ok(TunerEvent::Error(e)) => return Err(e),
                Ok(TunerEvent::Retuned(hz)) => {
                    tracing::info!(device = %self.device.id, %hz, "device retuned");
                }
                Err(_) => {}
            }

            // At end of stream take whatever is left, however small; otherwise
            // wait for a full block so the channelizer runs at its efficient
            // size.
            let available = stream.consumer.slots();
            let want = if ending {
                available
            } else {
                available - available % BLOCK_SAMPLES
            }
            .min(BLOCK_SAMPLES);

            if want == 0 {
                if ending {
                    return self.finish_stream();
                }
                if stream.is_finished() {
                    // The reader thread went away without saying why.
                    return Err("sample source stopped".to_owned());
                }
                std::thread::sleep(IDLE_SLEEP);
                continue;
            }

            // A full block is available: how long did we wait for it? Measured
            // from the end of the previous block's processing, so this is the
            // producer's gap alone. An operator watching `stream_gaps` climb
            // has independent evidence of the stall the clock resync corrects.
            if let Some(prev) = block_ready_at {
                if prev.elapsed() > gap_limit {
                    stream_gaps.inc();
                }
            }

            let block_end_ms = clock.block_end_ms(want);
            // The first block to arrive fixes this device's wall-clock origin.
            // Subtracting the block's own duration puts the origin at the
            // first sample rather than at the first block boundary.
            self.epoch
                .set_once(trunkd_core::Timestamp::now().unix_millis() - block_end_ms as i64);
            // Ring drops this block reaches, and a step of any owed backward
            // correction, move the epoch before a single event is stamped.
            let shift = clock.before_block(want, available, stream.overflow_count());
            if shift != 0 {
                self.epoch.shift(shift);
            }

            if let Ok(chunk) = stream.consumer.read_chunk(want) {
                let (head, tail) = chunk.as_slices();
                // Opt-in spectrum (W4): one atomic load per block when off; a
                // windowed FFT over the raw block only when keyup enabled it and
                // the duty cycle is due. `head` is the block's leading contiguous
                // slice (the whole block unless it wraps the ring's end).
                if let Some(tap) = spectrum.as_mut() {
                    tap.observe(head);
                }
                // Wideband level: one strided pass over the block already in
                // cache, no allocation, no log10 until the 1 s publish below.
                rssi.observe(head);
                if !tail.is_empty() {
                    rssi.observe(tail);
                }
                // The filterbank runs once for the whole device, then every
                // site reads the channels it subscribed. A chunk that wraps
                // the ring's end arrives as two slices; filterbank block
                // boundaries are arbitrary, so running both in order is
                // equivalent to one contiguous block — at no copy.
                for part in [head, tail] {
                    if part.is_empty() {
                        continue;
                    }
                    bank.borrow_mut().process_block(part);
                    for (index, engine) in engines.iter_mut() {
                        let index = *index;
                        engine.process_channels(&mut |ev| router.route(index, block_end_ms, ev));
                    }
                }
                chunk.commit_all();
                clock.commit(want);
                *last_block_ms = block_end_ms;
            }

            // Apply any coordinator commands (voice/data fronts) so the bins are
            // subscribed for the next block. A no-op for a control/auto tuner.
            self.drain_commands(engines, block_end_ms, &router);

            // Closed-loop frequency correction. Off the hot path in the sense
            // that matters: the inbox is empty on all but one block in a stats
            // window, and a decision is one atomic store plus a meter reset.
            if let (Some(inbox), Some(loop_)) = (auto_inbox.as_ref(), auto.as_mut()) {
                self.step_auto_ppm(loop_, inbox, stream, engines);
            }

            let elapsed = last_overflow_check.elapsed();
            if elapsed >= Duration::from_secs(1) {
                // Gain, AGC and ppm changes keyup pushed that this backend can
                // apply without a restart. Checked here, on the same off-hot-
                // path 1 Hz cadence as the overflow report: an empty mailbox
                // costs one uncontended lock and nothing else.
                self.drain_runtime_config(stream);
                // A keyup config change is applied by restarting this unit: the
                // registry's generation moved, so return and let the supervisor
                // rebuild us with the new params (checked off the hot path, on
                // the same ~1 s cadence as the overflow report).
                if self.control.as_ref().is_some_and(TunerControl::restart_due) {
                    tracing::info!(device = %self.device.id, "config change: restarting tuner");
                    ctx.request_restart();
                    return Ok(());
                }
                self.check_stream_clock(
                    &mut clock,
                    stream.consumer.slots(),
                    block_end_ms,
                    &clock_resyncs,
                    &last_clock_resync,
                );
                clock_error.set(clock.average_error_ms() as u64);
                clock_trim.set(clock.trimmed_ms() as u64);
                let now = stream.overflow_count();
                let delta = now.saturating_sub(overflow_seen);
                let per_sec = (delta as f64 / elapsed.as_secs_f64().max(0.001)) as u64;
                // Publish the rate every second (0 when healthy) so the control
                // loop reports a real figure, and bump the liveness heartbeat.
                overflow_rate.set(per_sec);
                stream_beat.set(trunkd_core::Timestamp::now().unix_millis().max(0) as u64);
                if let Some(dbfs) = rssi.dbfs() {
                    rssi_gauge.set_f64(dbfs);
                }
                channels_gauge.set(bank.borrow().channel_count() as u64);
                if delta > 0 && per_sec >= self.overflow_warn_per_sec {
                    overflow_reports.inc();
                    tracing::warn!(
                        device = %self.device.id,
                        dropped = delta,
                        per_sec,
                        threshold = self.overflow_warn_per_sec,
                        "SDR ring overflow: the DSP thread is not keeping up"
                    );
                }
                overflow_seen = now;
                last_overflow_check = Instant::now();
            }

            // End of this block's work. The next iteration measures its wait
            // for samples from here, so `stream_gaps` counts the producer's
            // stalls and never our own processing time.
            block_ready_at = Some(Instant::now());
        }
    }

    /// One drift check against the wall clock, and the re-anchor if it is due.
    ///
    /// Called on the DSP thread's 1 Hz off-hot-path cadence with the stream
    /// time of the block just processed, so a correction always lands on a
    /// block boundary: calls already on the air keep the start they were given
    /// and only their end moves, which makes a call spanning a resync up to one
    /// correction long or short and every call after it right.
    ///
    /// The drift is measured at the newest sample the SDR has *delivered*
    /// (`buffered` is what the ring holds now), not the newest consumed, so a
    /// backlog is never mistaken for loss. A backward correction is slewed in
    /// by [`StreamClock::before_block`] rather than applied here.
    ///
    /// The decision itself lives in
    /// [`DriftMonitor`](crate::pipeline::streamclock::DriftMonitor);
    /// everything here is the consequence of it.
    fn check_stream_clock(
        &self,
        clock: &mut StreamClock,
        buffered: usize,
        block_end_ms: u64,
        resyncs: &trunkd_core::Metric,
        last_correction: &trunkd_core::Metric,
    ) {
        let now_ms = trunkd_core::Timestamp::now().unix_millis();
        let newest = self.epoch.wall_ms(clock.newest_ms(buffered));
        let Some(resync) = clock.check(now_ms, newest) else {
            return;
        };
        let correction = resync.correction_ms;
        if resync.shift_now_ms != 0 {
            self.epoch.shift(resync.shift_now_ms);
        }
        resyncs.inc();
        // Signed, stored as its two's-complement bits: the panel reads it back
        // as an i64. Positive means the recorder had been stamping early.
        last_correction.set(correction as u64);
        tracing::warn!(
            device = %self.device.id,
            correction_ms = correction,
            "stream clock re-anchored: the sample count had fallen out of step \
             with wall time (samples lost before the ring), so timestamps up to \
             now were off by this much"
        );
        // And a record in the raw event log, so a day of timestamps read back
        // later shows where the seam is. Routed like any other control-channel
        // event, on the same bounded queue: dropped rather than blocking if the
        // observations thread is behind.
        if let Some(spec) = self.sites.first() {
            let _ = self.obs_tx.send(ObsMsg {
                site: spec.index,
                tuner: self.tuner_index,
                control_hz: 0,
                traffic_hz: 0,
                at_ms: self.epoch.wall_ms(block_end_ms).max(0) as u64,
                event: trunkd_p25::P25Event::ClockResync {
                    correction_ms: correction,
                },
            });
        }
    }

    /// Hand the running stream any runtime gain/AGC/ppm change keyup pushed.
    ///
    /// The control thread decided this needs no restart (see
    /// [`crate::pipeline::control::decide`]), so all that is left is to move it
    /// onto the thread that owns the device handle. A backend that turns out
    /// not to service it says so once and the change is left for the next
    /// restart to pick up out of the applied config, which is where it already
    /// lives.
    fn drain_runtime_config(&self, stream: &TunerStream) {
        let Some(control) = &self.control else { return };
        let Some(req) = control.take_runtime() else {
            return;
        };
        if req.is_empty() {
            return;
        }
        if !stream.set_gains(req) {
            tracing::info!(
                device = %self.device.id,
                backend = ?self.device.backend,
                "backend has no runtime gain control; the change applies on the next restart"
            );
        }
    }

    /// Drain the correction inbox and apply whatever it asks for.
    ///
    /// Runs on the DSP thread once per block, so it does nothing at all until
    /// a stats window lands: [`TunerStream::retune`] is an atomic store and the
    /// meter reset is a handful of field writes. The retune is *nominal* — the
    /// backend re-applies `ppm` on top exactly as it did at open, so the two
    /// corrections compose rather than replacing one another.
    fn step_auto_ppm(
        &self,
        auto: &mut AutoPpmLoop,
        inbox: &AutoPpmInbox,
        stream: &TunerStream,
        engines: &mut [(usize, SiteEngine)],
    ) {
        if !auto.enabled {
            return;
        }
        // Clock the hunt timer off the real transition rather than the next
        // stats window, which can be five seconds later.
        match inbox.take_transition() {
            Some(true) => auto.state.on_hunting_started(Instant::now()),
            Some(false) => auto.state.on_acquired(),
            None => {}
        }
        let Some(obs) = inbox.take_observation() else {
            return;
        };
        let Some(decision) = auto.state.observe(obs, Instant::now()) else {
            return;
        };
        if !stream.retune(Hz(decision.nominal_request_hz)) {
            // Said once: a backend with no runtime retune (a file, a HackRF)
            // can never satisfy this loop, so stop measuring against it.
            auto.enabled = false;
            tracing::info!(
                device = %self.device.id,
                backend = ?self.device.backend,
                "backend has no runtime retune; auto_ppm off"
            );
            return;
        }
        // Samples measured before the retune still argue for the correction
        // just applied; leaving them in place would double it.
        for (_, engine) in engines.iter_mut() {
            engine.reset_carrier_meters();
        }
        auto.ppm_gauge.set_f64(decision.total_ppm);
        auto.correction_gauge.set_f64(decision.total_hz);
        // Written down now rather than at shutdown: the restart this memory
        // exists to shorten is very often the one nobody got to run cleanly.
        auto.memory.corrected = true;
        auto.memory
            .write(decision.total_ppm, ppmmemory::Samples::Bump);
        tracing::info!(
            device = %self.device.id,
            step_hz = format_args!("{:+.0}", decision.step_hz),
            total_hz = format_args!("{:+.0}", decision.total_hz),
            total_ppm = format_args!("{:+.2}", decision.total_ppm),
            clamped = decision.clamped,
            "while" = if obs.hunting { "hunting" } else { "locked" },
            "auto frequency correction"
        );
    }

    /// The source reached its end. For a file that is success; for hardware it
    /// is a fault the supervisor should recover from.
    fn finish_stream(&self) -> Result<(), RunError> {
        if self.device.backend == Backend::Iqfile {
            tracing::info!(device = %self.device.id, "iq file exhausted; stopping cleanly");
            self.finished.store(true, Ordering::Relaxed);
            Ok(())
        } else {
            Err("sample stream ended unexpectedly".to_owned())
        }
    }

    /// Idle until asked to stop or until keyup changes this tuner's config
    /// (which moves the restart generation). Returning `Ok(())` after asking the
    /// supervisor to restart is what applies the new config on the next build.
    ///
    /// `reason` is repeated at warn on [`IDLE_REMINDER`], because the one line
    /// logged on the way in is at the top of an hour of silence: a tuner keyup
    /// disabled at 05:28 idled until 06:33 with nothing in the log after the
    /// first line to say why the recorder had gone quiet.
    fn idle_until_change(&self, ctx: &SupervisionContext, reason: &str) -> Result<(), RunError> {
        let mut next_reminder = Instant::now() + IDLE_REMINDER;
        while !ctx.should_stop() {
            ctx.heartbeat();
            if self.control.as_ref().is_some_and(TunerControl::restart_due) {
                ctx.request_restart();
                return Ok(());
            }
            if Instant::now() >= next_reminder {
                next_reminder = Instant::now() + IDLE_REMINDER;
                tracing::warn!(
                    device = %self.device.id,
                    reason,
                    "tuner is idle: it is decoding nothing, recording nothing, \
                     and writing no event log"
                );
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        Ok(())
    }

    /// Publish "this tuner is not decoding" into the metrics registry.
    ///
    /// Every gauge here is a *measurement over a window*, and the window has
    /// closed: leaving them at their last live values is what let a disabled
    /// tuner report `control=851.7875 MHz nac=0x293 tsbk_per_s=25.2` on every
    /// status line for the hour after it stopped, which is the whole reason a
    /// silent event log looked like a bug in the event log.
    ///
    /// The liveness gate in the status POST covers keyup and the panel; this
    /// covers everything that reads the registry directly, the console status
    /// line included.
    fn note_not_decoding(&self) {
        let metrics = &self.shared.metrics;
        // Zero the beat first: a reader that samples mid-clear must conclude
        // "stale", never "live and locked to nothing".
        metrics
            .gauge(&format!("device.{}.stream_beat_ms", self.device.id))
            .set(0);
        TunerQualityGauges::new(metrics, &self.device.id).clear();
        metrics
            .gauge(&format!("device.{}.rssi_dbfs", self.device.id))
            .set_f64(0.0);
        metrics
            .gauge(&format!("device.{}.overflow_per_s", self.device.id))
            .set(0);
        metrics
            .gauge(&format!("device.{}.channelizer_channels", self.device.id))
            .set(0);
        for spec in self.sites.iter().filter(|s| s.sole_tuner) {
            let prefix = &self.shared.sites[spec.index].metric_prefix;
            for name in [
                "control_frequency_hz",
                "nac",
                "tsbk_rate",
                "control_carrier_offset_valid",
                "control_frequency_error_hz",
                "pool_in_use",
            ] {
                metrics.gauge(&format!("{prefix}.{name}")).set(0);
            }
        }
    }

    /// Build the event router for this device.
    ///
    /// `auto_ppm` is where the router leaves the frequency evidence it sees;
    /// `None` outside the DSP loop (shutdown routing corrects nothing).
    fn router<'a>(&'a self, auto_ppm: Option<&'a AutoPpmInbox>) -> EventRouter<'a> {
        EventRouter {
            shared: &self.shared,
            epoch: &self.epoch,
            tuner_index: self.tuner_index,
            calls_tx: &self.calls_tx,
            obs_tx: &self.obs_tx,
            rssi: self
                .shared
                .metrics
                .gauge(&format!("device.{}.rssi_dbfs", self.device.id)),
            quality: TunerQualityGauges::new(&self.shared.metrics, &self.device.id),
            quality_site: self.sites.first().map_or(usize::MAX, |s| s.index),
            control_hz: self
                .shared
                .sites
                .iter()
                .map(|spec| {
                    self.shared
                        .metrics
                        .gauge(&format!("{}.control_frequency_hz", spec.metric_prefix))
                })
                .collect(),
            grant_tx: self.grant_tx.as_deref(),
            auto_ppm,
            floor: Cell::new(StampFloor::default()),
        }
    }

    /// Apply pending coordinator commands to the voice/data fronts. A no-op for
    /// a tuner with no command queue (control/auto-only).
    fn drain_commands(
        &self,
        engines: &mut [(usize, SiteEngine)],
        at_ms: u64,
        router: &EventRouter,
    ) {
        let Some(cmd_rx) = &self.cmd_rx else {
            return;
        };
        let find = |engines: &mut [(usize, SiteEngine)], site: usize| -> Option<usize> {
            engines.iter().position(|(i, _)| *i == site)
        };
        while let Ok(cmd) = cmd_rx.try_recv() {
            match cmd {
                TrafficCmd::Follow {
                    site,
                    frequency_hz,
                    info,
                    at_wall_ms,
                } => {
                    if let Some(pos) = find(engines, site) {
                        let idx = engines[pos].0;
                        engines[pos].1.follow(frequency_hz, info, 0, &mut |ev| {
                            router.route(idx, at_ms, ev)
                        });
                        self.shared
                            .metrics
                            .counter(&format!("device.{}.follows", self.device.id))
                            .inc();
                        let _ = at_wall_ms; // wall time is advisory; the engine times on its own stream
                    }
                }
                TrafficCmd::Update {
                    site,
                    frequency_hz,
                    info,
                    ..
                } => {
                    if let Some(pos) = find(engines, site) {
                        let idx = engines[pos].0;
                        engines[pos].1.follow(frequency_hz, info, 0, &mut |ev| {
                            router.route(idx, at_ms, ev)
                        });
                    }
                }
                TrafficCmd::Release {
                    site, frequency_hz, ..
                } => {
                    if let Some(pos) = find(engines, site) {
                        let idx = engines[pos].0;
                        engines[pos]
                            .1
                            .release(frequency_hz, &mut |ev| router.route(idx, at_ms, ev));
                    }
                }
                TrafficCmd::ChannelPlan { site, bands } => {
                    if let Some(pos) = find(engines, site) {
                        engines[pos].1.apply_channel_plan(&bands);
                    }
                }
            }
        }
    }
}

impl Supervised for DeviceUnit {
    fn run(&mut self, ctx: &SupervisionContext) -> Result<(), RunError> {
        // A tuner keyup has disabled records nothing: idle (keeping the board
        // open so re-enabling is a fast restart, not a cold tuner open) until
        // stop or until keyup re-enables it, which moves the restart generation.
        if self.control.as_ref().is_some_and(|c| !c.enabled) {
            tracing::warn!(
                device = %self.device.id,
                "tuner disabled by keyup; idling (it records nothing and writes no event log)"
            );
            self.note_not_decoding();
            // Settling into the requested disabled state *is* applying it, so
            // confirm the version (clears keyup's pending badge) but never beat
            // the liveness heartbeat — a disabled tuner must report unlocked.
            if let Some(c) = &self.control {
                c.confirm_applied();
            }
            return self.idle_until_change(ctx, "disabled by keyup");
        }
        if self.sites.is_empty() {
            // Nothing to decode. Idle rather than spin-restarting.
            tracing::warn!(device = %self.device.id, "no enabled P25 site uses this device");
            self.note_not_decoding();
            if let Some(c) = &self.control {
                c.confirm_applied();
            }
            return self.idle_until_change(ctx, "no enabled site uses this device");
        }

        // Best effort, never fatal: an unprivileged run says so once and
        // carries on at normal priority.
        crate::pipeline::priority::raise_current_thread(&self.device.id);

        let (bank, mut engines) = self.build_engines()?;
        let mut stream = self.start_stream()?;

        // The stream opened: this is the second phase of the two-phase adopt, so
        // the applied version now advances (a config that failed to open above
        // returned `?` before reaching here and never clears keyup's pending
        // badge).
        if let Some(c) = &self.control {
            c.confirm_applied();
        }

        tracing::info!(
            device = %self.device.id,
            backend = ?self.device.backend,
            sample_rate = self.device.sample_rate,
            center_hz = self.device.center_frequency.unwrap_or(0),
            sites = engines.len(),
            channelizer_channels = bank.borrow().channel_count(),
            ring_samples = ring_capacity_for(self.device.sample_rate, self.device.ring_seconds),
            "device streaming"
        );

        // A restarted device starts a fresh timeline.
        self.epoch.clear();
        let mut spectrum = self.spectrum_tap();
        let mut last_block_ms = 0;
        let outcome = self.pump(
            ctx,
            &mut stream,
            &bank,
            &mut engines,
            &mut spectrum,
            &mut last_block_ms,
        );

        // Finish every open call before the engines go away, so a shutdown
        // never silently loses audio that was already decoded. Stamped at the
        // last block the stream delivered (review 2026-09-22, R6): routing
        // them at stream time 0 put a shutdown's `PrivateCall` ends at the
        // stream's *start*, hours in the past.
        let router = self.router(None);
        for (index, engine) in &mut engines {
            let index = *index;
            engine.shutdown(&mut |ev| router.route(index, last_block_ms, ev));
        }
        stream.stop();
        // Zero the liveness heartbeat, and with it every figure this tuner was
        // measuring, so a tuner that has exited (a fault, an escalation that
        // leaves it down, or a restart) is reported unlocked and silent at
        // once rather than after the staleness window — or, for the readers
        // with no staleness window at all, never.
        self.note_not_decoding();
        outcome
    }
}

/// The per-tuner mirror of a site's [`SiteStats`] signal figures.
///
/// Every one of these is measured by one [`SiteEngine`] on one device's own
/// samples, but the `site.{index}.*` gauges they used to be published under are
/// keyed by *site*: two tuners on the same site share the handle and the last
/// one to emit a stats window wins. That is invisible on a single-tuner
/// recorder and completely wrong on a multi-tuner one — the panel drew one
/// tuner's SNR on every row. These are keyed by device, so they are not.
///
/// Resolved once, at router construction, for the same reason
/// [`EventRouter::rssi`] is: `route` runs on the DSP thread and does not format
/// metric names.
struct TunerQualityGauges {
    /// Set to 1 by the first publish. Gauges are created on first use, so a
    /// name that has never been written is *absent* from the registry — but
    /// these are created eagerly here, and a genuine reading of 0.0 is
    /// indistinguishable from an untouched cell. This flag is what tells the
    /// status report whether there is anything device-keyed to prefer.
    seen: trunkd_core::Metric,
    tsbk_rate: trunkd_core::Metric,
    freq_error_hz: trunkd_core::Metric,
    freq_error_valid: trunkd_core::Metric,
    pll_error_hz: trunkd_core::Metric,
    snr_db: trunkd_core::Metric,
    snr_valid: trunkd_core::Metric,
    noise_floor_dbfs: trunkd_core::Metric,
    tsbk_pass_pct: trunkd_core::Metric,
    tsbk_pass_valid: trunkd_core::Metric,
    sync_losses_per_min: trunkd_core::Metric,
}

impl TunerQualityGauges {
    fn new(metrics: &trunkd_core::Metrics, id: &str) -> Self {
        let g = |name: &str| metrics.gauge(&format!("device.{id}.{name}"));
        TunerQualityGauges {
            seen: g("site_stats_seen"),
            tsbk_rate: g("tsbk_rate"),
            freq_error_hz: g("freq_error_hz"),
            freq_error_valid: g("freq_error_valid"),
            pll_error_hz: g("pll_error_hz"),
            snr_db: g("snr_db"),
            snr_valid: g("snr_valid"),
            noise_floor_dbfs: g("noise_floor_dbfs"),
            tsbk_pass_pct: g("tsbk_pass_pct"),
            tsbk_pass_valid: g("tsbk_pass_valid"),
            sync_losses_per_min: g("sync_losses_per_min"),
        }
    }

    /// Zero every figure, for a tuner that has stopped decoding.
    ///
    /// `seen` is deliberately left set: this device *has* published stats, so
    /// the status report must keep preferring its device-keyed values — now
    /// zero — over the site-keyed ones a sibling tuner (or this tuner's own
    /// last live second) left behind. The `_valid` flags go to 0, so every
    /// optional figure reads as "not measured" rather than "measured as 0".
    fn clear(&self) {
        self.tsbk_rate.set_f64(0.0);
        self.freq_error_hz.set_f64(0.0);
        self.freq_error_valid.set(0);
        self.pll_error_hz.set_f64(0.0);
        self.snr_db.set_f64(0.0);
        self.snr_valid.set(0);
        self.noise_floor_dbfs.set_f64(0.0);
        self.tsbk_pass_pct.set_f64(0.0);
        self.tsbk_pass_valid.set(0);
        self.sync_losses_per_min.set_f64(0.0);
        self.seen.set(1);
    }

    /// Publish one site's stats window under this device's name.
    fn publish(&self, s: &trunkd_p25::site::SiteStats) {
        self.tsbk_rate.set_f64(s.control_tsbk_rate);
        self.pll_error_hz.set_f64(s.control_frequency_error_hz);
        self.sync_losses_per_min.set_f64(s.sync_losses_per_min);
        // Each `Option` gets a companion flag rather than a sentinel value: a
        // lingering 0.0 Hz would read as "measured, and spot on", and a
        // lingering 100% pass rate as a healthy site that is in fact hunting.
        match s.control_carrier_offset_hz {
            Some(hz) => {
                self.freq_error_hz.set_f64(hz);
                self.freq_error_valid.set(1);
            }
            None => self.freq_error_valid.set(0),
        }
        match (s.control_snr_db, s.noise_floor_dbfs) {
            (Some(snr), Some(floor)) => {
                self.snr_db.set_f64(snr);
                self.noise_floor_dbfs.set_f64(floor);
                self.snr_valid.set(1);
            }
            _ => self.snr_valid.set(0),
        }
        match s.tsbk_pass_pct {
            Some(pct) => {
                self.tsbk_pass_pct.set_f64(pct);
                self.tsbk_pass_valid.set(1);
            }
            None => self.tsbk_pass_valid.set(0),
        }
        self.seen.set(1);
    }
}

/// Splits the engine's event stream across the two bounded queues and the
/// metrics registry.
///
/// Runs on the DSP thread, so it does no work beyond moving values and bumping
/// counters — no I/O, no formatting on the hot path, no blocking send.
struct EventRouter<'a> {
    shared: &'a Shared,
    /// This device's stream epoch, for stamping wall-clock times into messages.
    epoch: &'a StreamEpoch,
    /// This device's tuner index (for the grant bus's local-preference tie).
    tuner_index: usize,
    calls_tx: &'a BoundedSender<CallMsg>,
    obs_tx: &'a BoundedSender<ObsMsg>,
    /// This device's wideband level gauge, handed to every call it starts so
    /// the upload can report which receiver heard the call and how hard.
    /// Resolved once here rather than formatted per call: `route` runs on the
    /// DSP thread.
    rssi: trunkd_core::Metric,
    /// This device's own copies of the per-site signal figures, so a site
    /// served by several tuners reports each tuner's own measurements.
    quality: TunerQualityGauges,
    /// The site this device publishes [`TunerQualityGauges`] for when no site
    /// on it holds a control lock: the first one it was built with. A locked
    /// site always wins over it (and, in the pathological case of a device
    /// holding two locks, the later of the two in `sites` order, because the
    /// engines are pumped in that order and the last write stands).
    quality_site: usize,
    /// Every site's `control_frequency_hz` gauge, by site index, resolved once:
    /// each control TSBK and followed-call signaling event reads it, and
    /// looking it up by name formatted a string and took the registry lock
    /// per event (review 2026-09-22 §3.1 O4).
    control_hz: Vec<trunkd_core::Metric>,
    /// Where a control/auto front publishes out-of-band grants; `None` on the
    /// single-tuner fast path.
    grant_tx: Option<&'a BoundedSender<GrantMsg>>,
    /// Where the frequency-correction loop's evidence is left. The router
    /// cannot act on it — neither the tuner nor the engines are reachable from
    /// inside an engine's own event closure — so it only records.
    auto_ppm: Option<&'a AutoPpmInbox>,
    /// No stamp goes backwards along the stream, whatever the epoch did.
    floor: Cell<StampFloor>,
}

impl EventRouter<'_> {
    /// Wall clock for a stream time: this device's epoch, held to
    /// [`StampFloor`] so a slewed correction never stamps a later point of the
    /// stream before an earlier one.
    fn stamp(&self, stream_ms: u64) -> i64 {
        let mut floor = self.floor.get();
        let w = floor.stamp(stream_ms, self.epoch.wall_ms(stream_ms));
        self.floor.set(floor);
        w
    }

    /// Queue one message for the call thread.
    ///
    /// The queue is drop-oldest, so a call thread that falls behind loses the
    /// oldest message rather than stalling this (DSP) thread. What it lost is
    /// counted in `calls.queue_dropped` — on the status line and in
    /// `/api/status` — because a lost `Ended` is a call only the call thread's
    /// orphan sweep can close.
    fn send_call(&self, msg: CallMsg) {
        if self.calls_tx.send(msg) == trunkd_core::chan::SendOutcome::Evicted {
            self.shared
                .metrics
                .counter(crate::pipeline::calls::QUEUE_DROPPED)
                .inc();
        }
    }

    /// `block_ms` is the stream time of the block being routed, used only for
    /// the block-level observation events; per-call events carry their own
    /// stream `at_ms`. Both are converted to wall clock here, at the source,
    /// with this device's own epoch.
    fn route(&self, site: usize, block_ms: u64, ev: SiteEvent) {
        let spec = &self.shared.sites[site];
        let metrics = &self.shared.metrics;
        let gauge = |name: &str| metrics.gauge(&format!("{}.{name}", spec.metric_prefix));
        let counter = |name: &str| metrics.counter(&format!("{}.{name}", spec.metric_prefix));
        // Stream ms → wall-clock ms, this device's timeline.
        let wall = |stream_ms: u64| self.stamp(stream_ms) as u64;
        let at_ms = wall(block_ms);

        match ev {
            SiteEvent::ControlHunting { candidates } => {
                if candidates.is_empty() {
                    tracing::error!(
                        site = %spec.display,
                        "no configured control candidate is inside the capture; \
                         this site can never acquire"
                    );
                } else {
                    tracing::info!(
                        site = %spec.display,
                        candidates = candidates.len(),
                        "hunting for the control channel"
                    );
                }
                gauge("control_frequency_hz").set(0);
                gauge("nac").set(0);
                if let Some(a) = self.auto_ppm {
                    a.note_hunting(site);
                }
            }
            SiteEvent::ControlAcquired { frequency_hz, nac } => {
                tracing::info!(
                    site = %spec.display,
                    frequency_hz,
                    nac = format_args!("0x{nac:03x}"),
                    "control channel acquired"
                );
                gauge("control_frequency_hz").set(frequency_hz);
                gauge("nac").set(u64::from(nac));
                counter("control_acquisitions").inc();
                if let Some(a) = self.auto_ppm {
                    a.note_acquired(site);
                }
            }
            SiteEvent::ControlLost { frequency_hz } => {
                tracing::warn!(site = %spec.display, frequency_hz, "control channel lost");
                gauge("control_frequency_hz").set(0);
                gauge("nac").set(0);
                counter("control_losses").inc();
                if let Some(a) = self.auto_ppm {
                    a.note_hunting(site);
                }
            }
            SiteEvent::CallSkipped {
                frequency_hz,
                talkgroup,
            } => {
                // Recorder-wide, beside `calls.started`: the skip list is one
                // decision for the box, so the operator reads one number
                // whatever heard the grant. The per-site counter beside it is
                // what tells two sites apart on the panel.
                let total = metrics.counter("calls.skipped_by_policy").add(1) + 1;
                counter("calls_skipped_by_policy").inc();
                // One line per power of two, as the pool-exhausted warning
                // does: a skipped dispatch talkgroup can key up hundreds of
                // times an hour and none of them is news after the first.
                if total.is_power_of_two() {
                    tracing::info!(
                        site = %spec.display,
                        frequency_hz,
                        talkgroup,
                        total,
                        "talkgroup is on the skip list; not recording it"
                    );
                }
            }
            SiteEvent::TdmaRefused {
                kind,
                frequency_hz,
                timeslot,
                talkgroup,
                source,
                first_for_talkgroup,
            } => {
                // Phase 2 traffic this recorder heard granted and cannot
                // decode yet (review 2026-09-22 §4 gap 2): counted per site,
                // for the panel, and recorder-wide beside `calls.started`, for
                // keyup — so the size of the gap is a number, not a guess.
                metrics.counter(TDMA_REFUSED).inc();
                counter(SITE_TDMA_REFUSED).inc();
                // Once per talkgroup: a Phase 2 site grants the same few
                // talkgroups all day, and only the first of each is news.
                if first_for_talkgroup {
                    tracing::info!(
                        site = %spec.display,
                        frequency_hz,
                        timeslot,
                        talkgroup,
                        source,
                        kind = ?kind,
                        "call granted on a Phase 2 (TDMA) channel; not recording it                          (Phase 1 decoder only) — further calls on this talkgroup are                          counted in calls.tdma_refused"
                    );
                }
            }
            SiteEvent::PoolExhausted {
                frequency_hz,
                talkgroup,
            } => {
                let total = counter("pool_exhausted").add(1) + 1;
                // Once the pool is full every further grant reports it, so a
                // busy minute would otherwise be thousands of identical lines.
                if total.is_power_of_two() {
                    tracing::warn!(
                        site = %spec.display,
                        frequency_hz,
                        talkgroup,
                        total,
                        "traffic pool exhausted; the call is not recorded"
                    );
                }
            }
            SiteEvent::Stats(s) => {
                gauge("stream_ms").set(s.stream_ms);
                gauge("active_calls").set(s.active_calls as u64);
                gauge("pool_in_use").set(s.pool_in_use as u64);
                gauge("pool_size").set(s.pool_size as u64);
                gauge("tsbk_rate").set_f64(s.control_tsbk_rate);
                gauge("control_frequency_error_hz").set_f64(s.control_frequency_error_hz);
                // The carrier meter's offset for the locked control channel:
                // the measurement auto-ppm corrects on, and the one the status
                // report calls `freq_error_hz`. It is `None` while hunting and
                // until the meter has a full second of samples, so a companion
                // flag says whether the value beside it means anything — a
                // stale reading of 0 Hz would otherwise read as "spot on".
                match s.control_carrier_offset_hz {
                    Some(hz) => {
                        gauge("control_carrier_offset_hz").set_f64(hz);
                        gauge("control_carrier_offset_valid").set(1);
                    }
                    None => gauge("control_carrier_offset_valid").set(0),
                }
                gauge("engine_calls_started").set(s.calls_started);
                gauge("engine_calls_ended").set(s.calls_ended);
                gauge("engine_voice_frames").set(s.voice_frames);
                gauge("sync_losses").set(s.sync_losses);
                // The same window, keyed by *device* rather than by site. The
                // `site.{index}.*` gauges above stay exactly as they were —
                // Keyup, the observation thread and the status report's own
                // fallback all read them — but they collapse when two tuners
                // serve one site, so the signal figures are mirrored here where
                // they cannot. A device serving several sites publishes the one
                // it is actually following: the locked site, or `quality_site`
                // (the first it was built with) while none is locked.
                if s.control_frequency_hz != 0 || site == self.quality_site {
                    self.quality.publish(&s);
                }
                if let Some(a) = self.auto_ppm {
                    a.note_stats(
                        site,
                        s.hunt_carrier_offset_hz,
                        s.hunt_carrier_snr_db,
                        s.control_carrier_offset_hz,
                    );
                }
            }
            SiteEvent::Control(event) => {
                let outcome = self.obs_tx.send(ObsMsg {
                    site,
                    tuner: self.tuner_index,
                    control_hz: self.control_hz[site].get(),
                    traffic_hz: 0,
                    at_ms,
                    event,
                });
                if outcome == trunkd_core::chan::SendOutcome::Evicted {
                    counter("nonvoice_dropped").inc();
                }
            }
            SiteEvent::Traffic {
                frequency_hz,
                event,
            } => {
                let outcome = self.obs_tx.send(ObsMsg {
                    site,
                    tuner: self.tuner_index,
                    control_hz: self.control_hz[site].get(),
                    traffic_hz: frequency_hz,
                    at_ms,
                    event,
                });
                if outcome == trunkd_core::chan::SendOutcome::Evicted {
                    counter("nonvoice_dropped").inc();
                }
            }
            SiteEvent::CallStarted {
                id,
                frequency_hz,
                talkgroup,
                target,
                source,
                kind,
                patches,
                at_ms: stream_at,
            } => {
                self.send_call(CallMsg::Started {
                    site,
                    id,
                    frequency_hz,
                    talkgroup,
                    target,
                    source,
                    kind,
                    patches,
                    rssi: Some(self.rssi.clone()),
                    at_ms: wall(stream_at),
                });
            }
            SiteEvent::CallTalker {
                id,
                source,
                emergency,
                alias,
                source_wacn,
                source_system,
                at_ms: stream_at,
            } => {
                self.send_call(CallMsg::Talker {
                    site,
                    id,
                    source,
                    emergency,
                    alias,
                    source_wacn,
                    source_system,
                    at_ms: wall(stream_at),
                });
            }
            SiteEvent::CallVoice { id, frame, .. } => {
                self.send_call(CallMsg::Voice { site, id, frame });
            }
            SiteEvent::CallEncryption {
                id, algid, key_id, ..
            } => {
                self.send_call(CallMsg::Encryption {
                    site,
                    id,
                    algid,
                    key_id,
                });
            }
            SiteEvent::CallEnded {
                id,
                record,
                at_ms: stream_at,
                reason,
            } => {
                self.send_call(CallMsg::Ended {
                    site,
                    id,
                    record: Box::new(record),
                    at_ms: wall(stream_at),
                    reason,
                });
            }
            SiteEvent::GrantOut {
                frequency_hz,
                info,
                at_ms: stream_at,
            } => {
                // A grant this front does not record itself: publish it to the
                // coordinator to route to another tuner. Dropped (and counted)
                // if the bus is full — never queued unbounded.
                if let Some(tx) = self.grant_tx {
                    let outcome = tx.send(GrantMsg {
                        site,
                        origin_device: self.tuner_index,
                        frequency_hz,
                        info,
                        at_wall_ms: self.stamp(stream_at),
                    });
                    if outcome == trunkd_core::chan::SendOutcome::Evicted {
                        counter("grant_bus_dropped").inc();
                    }
                } else {
                    // No coordinator wired: nothing can record it. Count it the
                    // way an out-of-band grant is counted.
                    counter("grant_unpublished").inc();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- wideband RSSI ------------------------------------------------------

    /// A block of constant-magnitude samples: a complex tone at amplitude `a`,
    /// which has mean power `a^2` regardless of phase.
    fn tone(len: usize, amplitude: f32) -> Vec<trunkd_core::IqSample> {
        (0..len)
            .map(|i| {
                let phase = i as f32 * 0.37;
                trunkd_core::IqSample::new(amplitude * phase.cos(), amplitude * phase.sin())
            })
            .collect()
    }

    #[test]
    fn a_full_scale_tone_reads_zero_dbfs() {
        // Full scale is a unit-magnitude sample (the backends normalize to it),
        // so a tone riding the rails is 0 dBFS by definition.
        let mut m = RssiMeter::default();
        for _ in 0..64 {
            m.observe(&tone(BLOCK_SAMPLES, 1.0));
        }
        let db = m.dbfs().expect("measured");
        assert!(db.abs() < 0.05, "full-scale tone read {db} dBFS");
    }

    #[test]
    fn halving_the_amplitude_costs_six_db() {
        let mut m = RssiMeter::default();
        for _ in 0..64 {
            m.observe(&tone(BLOCK_SAMPLES, 0.5));
        }
        let db = m.dbfs().expect("measured");
        assert!(
            (db + 6.02).abs() < 0.05,
            "half-amplitude tone read {db} dBFS, expected ~-6.02"
        );
    }

    #[test]
    fn digital_silence_reports_the_floor_not_negative_infinity() {
        // -inf would serialize as JSON null and read downstream as "not
        // measured" rather than "measured, and there is nothing there".
        let mut m = RssiMeter::default();
        m.observe(&vec![trunkd_core::IqSample::new(0.0, 0.0); BLOCK_SAMPLES]);
        assert_eq!(m.dbfs(), Some(RSSI_FLOOR_DBFS));
        assert_eq!(power_to_dbfs(0.0), RSSI_FLOOR_DBFS);
        assert_eq!(power_to_dbfs(-1.0), RSSI_FLOOR_DBFS);
        assert_eq!(power_to_dbfs(f64::NAN), RSSI_FLOOR_DBFS);
    }

    #[test]
    fn an_unmeasured_meter_reports_nothing_rather_than_zero() {
        // 0 dBFS is a real (and very loud) level: never fabricate one.
        assert_eq!(RssiMeter::default().dbfs(), None);
        let mut m = RssiMeter::default();
        m.observe(&[]);
        assert_eq!(m.dbfs(), None, "an empty block measures nothing");
    }

    #[test]
    fn a_block_shorter_than_the_stride_is_measured_whole() {
        // The ragged last block of an IQ file is smaller than the stride;
        // striding past it would divide by zero.
        let mut m = RssiMeter::default();
        m.observe(&tone(RSSI_STRIDE / 2, 1.0));
        let db = m.dbfs().expect("a short block still measures");
        assert!(db.abs() < 0.05, "short block read {db} dBFS");
    }

    #[test]
    fn the_ema_tracks_a_level_change_within_a_second_of_blocks() {
        // Settling matters: a status POST every 15 s must not report a level
        // the tuner left behind minutes ago.
        let mut m = RssiMeter::default();
        for _ in 0..64 {
            m.observe(&tone(BLOCK_SAMPLES, 0.01));
        }
        assert!(m.dbfs().expect("measured") < -35.0);
        for _ in 0..64 {
            m.observe(&tone(BLOCK_SAMPLES, 1.0));
        }
        let db = m.dbfs().expect("measured");
        assert!(db.abs() < 0.05, "after 64 blocks at full scale, read {db}");
    }

    #[test]
    fn the_ring_holds_at_least_the_seconds_asked_for() {
        // Rounding is always *up* to a whole block, so the ring is never
        // shallower than configured — the whole point of the knob — and never
        // more than one block deeper.
        for (rate, secs) in [
            (20_000_000u32, 1.0f64),
            (20_000_000, 0.2),
            (20_000_000, 2.5),
            (2_400_000, 1.0),
        ] {
            let cap = ring_capacity_for(rate, secs);
            assert_eq!(cap % BLOCK_SAMPLES, 0, "{cap}");
            assert!(
                cap as f64 / f64::from(rate) >= secs,
                "{rate} sps x {secs} s asked for {cap} samples"
            );
            let pow2 = ((f64::from(rate) * secs) as usize)
                .max(BLOCK_SAMPLES * 2)
                .next_power_of_two();
            assert!(
                cap >= pow2 && cap - pow2 < BLOCK_SAMPLES,
                "{rate} sps x {secs} s keeps the power-of-two depth, at most one block over, got {cap} samples"
            );
        }
    }

    #[test]
    fn the_default_ring_keeps_its_power_of_two_depth_at_twenty_msps() {
        // The stall that motivated the default was ~2 s long; one second of
        // ring is the compromise between absorbing it and 160 MB of RAM.
        let cap = ring_capacity_for(20_000_000, 1.0);
        let secs = cap as f64 / 20_000_000.0;
        // 2^25 samples, 1.68 s — the depth every recorder has run with; a
        // dropped sample is lost audio, a backlog is only delay.
        assert!((1.67..1.69).contains(&secs), "{cap} samples = {secs} s");
    }

    #[test]
    fn a_slow_device_still_gets_a_usable_ring() {
        // 48 ksps would otherwise ask for a 48 000-sample ring — smaller than
        // one DSP block, so "wait for a full block" could never be satisfied.
        assert!(ring_capacity_for(48_000, 1.0) >= BLOCK_SAMPLES * 2);
    }

    // -- per-board ppm memory ----------------------------------------------

    /// A one-HackRF config whose ppm memory lives in a directory of its own.
    fn hackrf_config(dir: &Path, ppm: f64, auto_ppm: bool, limit: f64) -> Config {
        let toml = format!(
            r#"
            [recorder]
            ppm_memory_path = '{}'
            [[device]]
            id = "a"
            backend = "hackrf"
            serial = "0000cafe"
            sample_rate = 20000000
            center_frequency = 855500000
            ppm = {ppm}
            auto_ppm = {auto_ppm}
            auto_ppm_limit = {limit}
            [[site]]
            system = "Example"
            site = "Central"
            devices = ["a"]
            control_frequencies = [851050000]
            "#,
            dir.join("ppm-memory.json").display()
        );
        toml::from_str(&toml).expect("parse hackrf config")
    }

    fn ppm_tmpdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "trunkd-devppm-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[test]
    fn a_remembered_board_opens_on_its_correction_and_seeds_the_loop() {
        let dir = ppm_tmpdir("apply");
        let cfg = hackrf_config(&dir, 0.0, true, 10.0);
        let shared = shared_for(&cfg);
        let path = dir.join("ppm-memory.json");

        // Nothing remembered: the backend gets the configured ppm and the loop
        // starts from zero, exactly as it did before this existed.
        let m = resolve_ppm_memory(&cfg, &cfg.device[0], &shared);
        assert_eq!(m.board, "hackrf:0000cafe");
        assert_eq!(
            (m.open_ppm, m.seed_ppm, m.applied_at_open),
            (0.0, 0.0, false)
        );

        // Now the loop has measured +1.25 ppm on this board.
        ppmmemory::record(
            &path,
            "hackrf:0000cafe",
            1.25,
            0.0,
            ppmmemory::Samples::Bump,
        );
        let m = resolve_ppm_memory(&cfg, &cfg.device[0], &shared);
        assert_eq!((m.open_ppm, m.seed_ppm), (1.25, 1.25));
        assert!(m.applied_at_open);

        // ...and the status gauges say so, timestamp included.
        let snap = shared.metrics.snapshot();
        let g = |n: &str| {
            snap.iter()
                .find(|m| m.name == n)
                .map(|m| m.value)
                .unwrap_or(0)
        };
        assert_eq!(f64::from_bits(g("device.a.ppm_memory_ppm")), 1.25);
        assert_eq!(g("device.a.ppm_memory_at_open"), 1);
        assert!(g("device.a.ppm_memory_at_ms") > 0);

        // A loop seeded with that delta reports the whole correction from the
        // first sample and refines it rather than re-deriving it.
        let auto = AutoPpm::new(855_500_000, 10.0, m.seed_ppm);
        assert!((auto.total_ppm() - 1.25).abs() < 1e-9);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_edited_config_ppm_re_bases_the_remembered_delta() {
        let dir = ppm_tmpdir("rebase");
        let path = dir.join("ppm-memory.json");
        // Measured at +1.25 total against a config ppm of 0.
        ppmmemory::record(
            &path,
            "hackrf:0000cafe",
            1.25,
            0.0,
            ppmmemory::Samples::Bump,
        );

        // The operator then types `ppm = 2.0`: their number moves the open by
        // exactly 2, and the board's own measured error rides on top of it.
        let cfg = hackrf_config(&dir, 2.0, true, 10.0);
        let m = resolve_ppm_memory(&cfg, &cfg.device[0], &shared_for(&cfg));
        assert_eq!((m.open_ppm, m.seed_ppm), (3.25, 1.25));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_remembered_correction_past_the_limit_opens_on_the_limit() {
        let dir = ppm_tmpdir("clamp");
        let path = dir.join("ppm-memory.json");
        ppmmemory::record(&path, "hackrf:0000cafe", 8.0, 0.0, ppmmemory::Samples::Bump);
        let cfg = hackrf_config(&dir, 0.0, true, 1.5);
        let m = resolve_ppm_memory(&cfg, &cfg.device[0], &shared_for(&cfg));
        assert_eq!((m.open_ppm, m.seed_ppm), (1.5, 1.5));
        assert!(m.applied_at_open);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn with_auto_ppm_off_the_memory_is_reported_but_never_applied() {
        let dir = ppm_tmpdir("manual");
        let path = dir.join("ppm-memory.json");
        ppmmemory::record(
            &path,
            "hackrf:0000cafe",
            1.25,
            0.0,
            ppmmemory::Samples::Bump,
        );
        let cfg = hackrf_config(&dir, -0.5, false, 10.0);
        let shared = shared_for(&cfg);
        let m = resolve_ppm_memory(&cfg, &cfg.device[0], &shared);
        // The operator's `ppm` is the whole story...
        assert_eq!((m.open_ppm, m.seed_ppm), (-0.5, 0.0));
        assert!(!m.applied_at_open);
        // ...but the number they could type is still on the status report.
        let snap = shared.metrics.snapshot();
        let g = |n: &str| snap.iter().find(|m| m.name == n).map(|m| m.value);
        assert_eq!(g("device.a.ppm_memory_ppm").map(f64::from_bits), Some(1.25));
        assert_eq!(g("device.a.ppm_memory_at_open"), Some(0));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_iq_file_has_no_board_to_remember() {
        // Its offset belongs to whatever recorded the capture, on a day this
        // recorder will never see again.
        let dir = ppm_tmpdir("iqfile");
        let mut cfg = two_tuner_config();
        cfg.recorder.ppm_memory_path = dir.join("ppm-memory.json").display().to_string();
        ppmmemory::record(
            &dir.join("ppm-memory.json"),
            "iqfile:a",
            5.0,
            0.0,
            ppmmemory::Samples::Bump,
        );
        let m = resolve_ppm_memory(&cfg, &cfg.device[0], &shared_for(&cfg));
        assert_eq!(
            (m.open_ppm, m.seed_ppm, m.applied_at_open),
            (0.0, 0.0, false)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// One tuner, one site, so the tuner is the site's sole publisher.
    fn one_tuner_config() -> Config {
        let toml = r#"
            [[device]]
            id = "a"
            backend = "iqfile"
            path = "/dev/null"
            sample_rate = 20000000
            center_frequency = 855500000
            role = "auto"
            [[site]]
            system = "Example"
            site = "Central"
            device = "a"
            control_frequencies = [851050000]
        "#;
        toml::from_str(toml).expect("parse one-tuner config")
    }

    fn two_tuner_config() -> Config {
        let toml = r#"
            [[device]]
            id = "a"
            backend = "iqfile"
            path = "/dev/null"
            sample_rate = 20000000
            center_frequency = 855500000
            role = "auto"
            [[device]]
            id = "b"
            backend = "iqfile"
            path = "/dev/null"
            sample_rate = 20000000
            center_frequency = 855500000
            role = "voice"
            [[site]]
            system = "Example"
            site = "Central"
            devices = ["a", "b"]
            control_frequencies = [851050000]
        "#;
        toml::from_str(toml).expect("parse two-tuner config")
    }

    fn shared_for(_cfg: &Config) -> Shared {
        Shared {
            private_calls: Default::default(),
            calls: Default::default(),
            skip: trunkd_core::SkipList::empty(),
            replay_clock: Default::default(),
            metrics: trunkd_core::Metrics::new(),
            sites: Arc::new(vec![crate::pipeline::SiteSpec {
                system_label: "Example".into(),
                site_label: "Central".into(),
                control_frequencies: vec![851_050_000],
                metric_prefix: "site.0".into(),
                display: "Example/Central".into(),
            }]),
        }
    }

    fn unit(cfg: &Config, dev_idx: usize, epoch: Arc<StreamEpoch>) -> DeviceUnit {
        unit_for(cfg, dev_idx, epoch, None)
    }

    /// [`unit`], optionally sharing the caller's observation sender — which is
    /// how the supervisor's factory builds every rebuild of a real tuner.
    fn unit_for(
        cfg: &Config,
        dev_idx: usize,
        epoch: Arc<StreamEpoch>,
        obs_tx: Option<Arc<trunkd_core::chan::BoundedSender<ObsMsg>>>,
    ) -> DeviceUnit {
        let shared = shared_for(cfg);
        let (ctx, _cx) = trunkd_core::chan::bounded::<CallMsg>(4);
        let (otx, _ox) = trunkd_core::chan::bounded::<ObsMsg>(4);
        let obs_tx = obs_tx.unwrap_or_else(|| Arc::new(otx));
        DeviceUnit::new(
            cfg,
            &cfg.device[dev_idx],
            dev_idx,
            shared,
            epoch,
            Arc::new(ctx),
            obs_tx,
            None,
            None,
            Arc::new(AtomicBool::new(false)),
            None,
            None,
            &|i| (i == 0).then_some(0),
        )
    }

    #[test]
    fn each_device_unit_owns_its_own_epoch() {
        // Two tuners for one site must not share a timeline (the N>1 bug this
        // task fixes). Stamping one epoch must leave the other untouched.
        let cfg = two_tuner_config();
        let ea = Arc::new(StreamEpoch::default());
        let eb = Arc::new(StreamEpoch::default());
        let a = unit(&cfg, 0, ea.clone());
        let b = unit(&cfg, 1, eb.clone());
        assert!(!Arc::ptr_eq(&a.epoch, &b.epoch), "distinct epoch handles");
        a.epoch.set_once(1_700_000_000_000);
        assert_eq!(a.epoch.wall_ms(0), 1_700_000_000_000);
        // B's epoch is still unset (answers ~now, not A's stamp).
        let now = trunkd_core::Timestamp::now().unix_millis();
        assert!((b.epoch.wall_ms(0) - now).abs() < 5_000);
    }

    /// A tuner that has stopped decoding must stop *reporting* that it is
    /// decoding. Every figure here is a measurement over a window, and the
    /// window has closed: leaving them at their last live values is what let a
    /// disabled tuner print `control=851.7875 MHz nac=0x293 tsbk_per_s=25.2`
    /// on every status line for the hour after it went idle.
    #[test]
    fn a_tuner_that_stops_decoding_stops_reporting_that_it_is_decoding() {
        let cfg = two_tuner_config();
        let unit = unit(&cfg, 0, Arc::new(StreamEpoch::default()));
        let m = unit.shared.metrics.clone();

        // A live second's worth of gauges, device- and site-keyed.
        m.gauge("device.a.stream_beat_ms").set(1_788_499_730_000);
        m.gauge("device.a.tsbk_rate").set_f64(25.2);
        m.gauge("device.a.snr_db").set_f64(19.5);
        m.gauge("device.a.snr_valid").set(1);
        m.gauge("device.a.site_stats_seen").set(1);
        m.gauge("device.a.rssi_dbfs").set_f64(-42.5);
        m.gauge("site.0.control_frequency_hz").set(851_787_500);
        m.gauge("site.0.nac").set(0x293);
        m.gauge("site.0.tsbk_rate").set_f64(25.2);

        unit.note_not_decoding();

        let get = |name: &str| {
            m.snapshot()
                .iter()
                .find(|g| g.name == name)
                .map(|g| g.value)
        };
        assert_eq!(get("device.a.stream_beat_ms"), Some(0));
        assert_eq!(get("device.a.tsbk_rate").map(f64::from_bits), Some(0.0));
        assert_eq!(get("device.a.snr_valid"), Some(0));
        // `site_stats_seen` stays set: this device's own (now zero) figures
        // must keep winning over the site-keyed ones in the status report.
        assert_eq!(get("device.a.site_stats_seen"), Some(1));
        assert_eq!(get("device.a.rssi_dbfs").map(f64::from_bits), Some(0.0));
        // The site is served by two tuners in this config, so its gauges
        // belong to whichever is still up and are left alone.
        assert_eq!(get("site.0.control_frequency_hz"), Some(851_787_500));
        assert_eq!(get("site.0.tsbk_rate").map(f64::from_bits), Some(25.2));
    }

    /// The single-tuner case: nobody else is publishing the site's gauges, so
    /// the tuner that stops decoding clears them too — which is what the
    /// console status line, and everything else reading the registry directly
    /// with no staleness window of its own, reads.
    #[test]
    fn the_only_tuner_on_a_site_clears_the_sites_gauges_too() {
        let cfg = one_tuner_config();
        let unit = unit(&cfg, 0, Arc::new(StreamEpoch::default()));
        assert!(
            unit.sites.iter().all(|s| s.sole_tuner),
            "this fixture's site names one device"
        );
        let m = unit.shared.metrics.clone();
        m.gauge("site.0.control_frequency_hz").set(851_787_500);
        m.gauge("site.0.nac").set(0x293);
        m.gauge("site.0.tsbk_rate").set_f64(25.2);

        unit.note_not_decoding();

        let get = |name: &str| {
            m.snapshot()
                .iter()
                .find(|g| g.name == name)
                .map(|g| g.value)
        };
        assert_eq!(get("site.0.control_frequency_hz"), Some(0));
        assert_eq!(get("site.0.nac"), Some(0));
        assert_eq!(get("site.0.tsbk_rate").map(f64::from_bits), Some(0.0));
    }

    /// The supervisor rebuilds a `DeviceUnit` from a factory closure that
    /// clones one `Arc<BoundedSender>` per build, so every rebuild — the one
    /// that applies a keyup desired-state change in place, and the startup
    /// build from the prefetched desired state — publishes into the same queue
    /// the observations thread (and with it the event log) is reading. A
    /// rebuild given its own channel, or a receiver that ended when a
    /// torn-down unit dropped its handle, would silence the event log at the
    /// first restart and never bring it back.
    /// T-B5: events are stamped `epoch + stream time` — so a shutdown routed
    /// at the last block's time lands at the end of the stream, not its start
    /// (R6) — and a slewed epoch never stamps a later point of the stream
    /// before an earlier one.
    #[test]
    fn routed_stamps_follow_the_block_and_never_go_backwards() {
        let cfg = two_tuner_config();
        let (otx, orx) = trunkd_core::chan::bounded::<ObsMsg>(16);
        let epoch = Arc::new(StreamEpoch::default());
        epoch.set_once(1_790_000_000_000);
        let unit = unit_for(&cfg, 0, epoch.clone(), Some(Arc::new(otx)));
        let router = unit.router(None);
        let ev = || trunkd_p25::site::SiteEvent::Control(trunkd_p25::P25Event::TsbkFailed);
        let at = |block_ms: u64| {
            router.route(0, block_ms, ev());
            orx.try_recv().expect("routed").at_ms
        };

        // Four hours in: the shutdown path now passes the last block's time.
        let last = 4 * 3_600_000;
        assert_eq!(at(last), 1_790_000_000_000 + last);
        // A backward slew step of 3 ms: the next block, 2 ms later in the
        // stream, is held at the previous stamp rather than stepping back.
        epoch.shift(-3);
        assert_eq!(at(last + 2), 1_790_000_000_000 + last);
        assert_eq!(at(last + 10), 1_790_000_000_000 + last + 7);
    }

    #[test]
    fn every_rebuild_publishes_into_the_recorders_one_observation_queue() {
        let cfg = two_tuner_config();
        let (otx, orx) = trunkd_core::chan::bounded::<ObsMsg>(16);
        let obs_tx = Arc::new(otx);
        let epoch = Arc::new(StreamEpoch::default());

        for build in 0..3 {
            // Exactly what `spawn_devices`' factory closure does per build.
            let unit = unit_for(&cfg, 0, epoch.clone(), Some(obs_tx.clone()));
            unit.router(None).route(
                0,
                0,
                trunkd_p25::site::SiteEvent::Control(trunkd_p25::P25Event::TsbkFailed),
            );
            // The rebuilt unit replaces this one, and takes its handle with it.
            drop(unit);
            assert!(
                orx.try_recv().is_ok(),
                "build {build} reached the observations queue"
            );
        }
        // Still open for the build after those: the queue outlives every unit.
        assert_eq!(
            obs_tx.send(ObsMsg {
                site: 0,
                tuner: 0,
                control_hz: 0,
                traffic_hz: 0,
                at_ms: 0,
                event: trunkd_p25::P25Event::TsbkFailed,
            }),
            trunkd_core::chan::SendOutcome::Sent
        );
    }

    #[test]
    fn call_ids_are_namespaced_by_tuner_index() {
        let cfg = two_tuner_config();
        let a = unit(&cfg, 0, Arc::new(StreamEpoch::default()));
        let b = unit(&cfg, 1, Arc::new(StreamEpoch::default()));
        assert_eq!(a.tuner_index, 0);
        assert_eq!(b.tuner_index, 1);
        // The per-site engine config carries disjoint call-id bases.
        assert_eq!(a.sites[0].engine.call_id_base, 0);
        assert_eq!(b.sites[0].engine.call_id_base, 1u64 << 48);
    }

    /// A cheap 1 Msps two-tuner config: an auto control front plus a voice front,
    /// so a voice `DeviceUnit` can be built without a 20 Msps filterbank.
    fn small_two_tuner_config() -> Config {
        let toml = r#"
            [[device]]
            id = "a"
            backend = "iqfile"
            path = "/dev/null"
            sample_rate = 1000000
            center_frequency = 852000000
            role = "auto"
            [[device]]
            id = "v"
            backend = "iqfile"
            path = "/dev/null"
            sample_rate = 1000000
            center_frequency = 852000000
            role = "voice"
            [[site]]
            system = "S"
            site = "T"
            devices = ["a", "v"]
            control_frequencies = [851800000]
        "#;
        let cfg: Config = toml::from_str(toml).expect("parse small config");
        cfg.validate().expect("valid");
        cfg
    }

    /// Two tuners on one site write into one `site.0.*` namespace, so the last
    /// stats window routed silently overwrites the other's — the panel drew one
    /// tuner's SNR, TSBK rate and frequency error on every row that served the
    /// site. The device-keyed mirror is what keeps each tuner's own numbers.
    #[test]
    fn each_tuner_publishes_its_own_signal_quality() {
        use trunkd_core::chan;
        use trunkd_p25::site::SiteStats;

        let cfg = small_two_tuner_config();
        // One registry for both units: that is the condition under which the
        // site-keyed gauges collapse.
        let shared = shared_for(&cfg);
        let (ctx, _crx) = chan::bounded::<CallMsg>(8);
        let (otx, _orx) = chan::bounded::<ObsMsg>(8);
        let (ctx, otx) = (Arc::new(ctx), Arc::new(otx));
        let build = |idx: usize| {
            DeviceUnit::new(
                &cfg,
                &cfg.device[idx],
                idx,
                shared.clone(),
                Arc::new(StreamEpoch::default()),
                ctx.clone(),
                otx.clone(),
                None,
                None,
                Arc::new(AtomicBool::new(false)),
                None,
                None,
                &|i| (i == 0).then_some(0),
            )
        };
        let (a, v) = (build(0), build(1));

        let stats = |snr: f64, rate: f64| SiteStats {
            control_frequency_hz: 851_800_000,
            control_tsbk_rate: rate,
            control_frequency_error_hz: 88.0,
            control_carrier_offset_hz: Some(-12.0),
            noise_floor_dbfs: Some(-70.5),
            control_snr_db: Some(snr),
            tsbk_pass_pct: Some(97.5),
            sync_losses_per_min: 1.5,
            ..SiteStats::default()
        };
        a.router(None)
            .route(0, 1_000, SiteEvent::Stats(stats(21.0, 30.0)));
        v.router(None)
            .route(0, 1_000, SiteEvent::Stats(stats(9.0, 4.0)));

        let snap = shared.metrics.snapshot();
        let g = |name: &str| {
            snap.iter()
                .find(|m| m.name == name)
                .map(|m| f64::from_bits(m.value))
        };
        let flag = |name: &str| snap.iter().find(|m| m.name == name).map(|m| m.value);
        assert_eq!(g("device.a.snr_db"), Some(21.0));
        assert_eq!(g("device.v.snr_db"), Some(9.0));
        assert_eq!(g("device.a.tsbk_rate"), Some(30.0));
        assert_eq!(g("device.v.tsbk_rate"), Some(4.0));
        for id in ["a", "v"] {
            assert_eq!(g(&format!("device.{id}.noise_floor_dbfs")), Some(-70.5));
            assert_eq!(g(&format!("device.{id}.tsbk_pass_pct")), Some(97.5));
            assert_eq!(g(&format!("device.{id}.sync_losses_per_min")), Some(1.5));
            assert_eq!(g(&format!("device.{id}.freq_error_hz")), Some(-12.0));
            assert_eq!(g(&format!("device.{id}.pll_error_hz")), Some(88.0));
            for f in [
                "site_stats_seen",
                "snr_valid",
                "tsbk_pass_valid",
                "freq_error_valid",
            ] {
                assert_eq!(flag(&format!("device.{id}.{f}")), Some(1), "{id}.{f}");
            }
        }
        // The site-keyed gauge is untouched and still collapses to whoever
        // routed last — kept exactly as it was, because Keyup, the observation
        // thread and the status report's own fallback all read it.
        assert_eq!(g("site.0.tsbk_rate"), Some(4.0));
    }

    /// The `Option` figures clear their flags rather than lingering: a hunting
    /// tuner must not report the SNR it had before it lost the channel.
    #[test]
    fn an_unmeasured_figure_clears_its_flag() {
        use trunkd_core::chan;
        use trunkd_p25::site::SiteStats;

        let cfg = small_two_tuner_config();
        let shared = shared_for(&cfg);
        let (ctx, _crx) = chan::bounded::<CallMsg>(8);
        let (otx, _orx) = chan::bounded::<ObsMsg>(8);
        let a = DeviceUnit::new(
            &cfg,
            &cfg.device[0],
            0,
            shared.clone(),
            Arc::new(StreamEpoch::default()),
            Arc::new(ctx),
            Arc::new(otx),
            None,
            None,
            Arc::new(AtomicBool::new(false)),
            None,
            None,
            &|i| (i == 0).then_some(0),
        );
        let router = a.router(None);
        router.route(
            0,
            1_000,
            SiteEvent::Stats(SiteStats {
                control_frequency_hz: 851_800_000,
                control_snr_db: Some(21.0),
                noise_floor_dbfs: Some(-70.5),
                tsbk_pass_pct: Some(97.5),
                control_carrier_offset_hz: Some(-12.0),
                ..SiteStats::default()
            }),
        );
        // Control lost: the next window measures none of the three.
        router.route(0, 6_000, SiteEvent::Stats(SiteStats::default()));

        let snap = shared.metrics.snapshot();
        let flag = |name: &str| snap.iter().find(|m| m.name == name).map(|m| m.value);
        for f in ["snr_valid", "tsbk_pass_valid", "freq_error_valid"] {
            assert_eq!(flag(&format!("device.a.{f}")), Some(0), "{f}");
        }
        // ...but the device is still publishing, so the status path keeps
        // preferring its gauges over the shared site ones.
        assert_eq!(flag("device.a.site_stats_seen"), Some(1));
    }

    /// Review 2026-09-22, U6: the DSP → call queue evicts its oldest message
    /// when the call thread falls behind, and that used to go uncounted — a
    /// lost `Ended` left no trace anywhere. Every eviction is now counted in
    /// `calls.queue_dropped`.
    #[test]
    fn a_call_message_the_queue_evicts_is_counted() {
        use trunkd_core::chan;
        use trunkd_p25::event::CallKind;
        use trunkd_p25::site::{CallId, EndReason};

        let cfg = small_two_tuner_config();
        let shared = shared_for(&cfg);
        // Room for one message: the call thread is not reading.
        let (ctx, calls_rx) = chan::bounded::<CallMsg>(1);
        let (otx, _orx) = chan::bounded::<ObsMsg>(8);
        let unit = DeviceUnit::new(
            &cfg,
            &cfg.device[0],
            0,
            shared.clone(),
            Arc::new(StreamEpoch::default()),
            Arc::new(ctx),
            Arc::new(otx),
            None,
            None,
            Arc::new(AtomicBool::new(false)),
            None,
            None,
            &|i| (i == 0).then_some(0),
        );
        let router = unit.router(None);
        let dropped = || {
            shared
                .metrics
                .counter(crate::pipeline::calls::QUEUE_DROPPED)
                .get()
        };
        router.route(
            0,
            1_000,
            SiteEvent::CallStarted {
                id: CallId(1),
                frequency_hz: 852_100_000,
                talkgroup: 100,
                target: 100,
                source: 7,
                kind: CallKind::Group,
                patches: Vec::new(),
                at_ms: 1_000,
            },
        );
        assert_eq!(dropped(), 0, "the queue had room");
        router.route(
            0,
            2_000,
            SiteEvent::CallEnded {
                id: CallId(1),
                record: Default::default(),
                at_ms: 2_000,
                reason: EndReason::Terminator,
            },
        );
        assert_eq!(dropped(), 1, "the Started was evicted to make room");
        router.route(
            0,
            2_000,
            SiteEvent::CallVoice {
                id: CallId(2),
                frame: [0u8; 18],
                at_ms: 2_000,
            },
        );
        assert_eq!(dropped(), 2, "and now the Ended — counted, not silent");
        assert!(matches!(calls_rx.try_recv(), Ok(CallMsg::Voice { .. })));
    }

    /// Review 2026-09-22 §3.1 O4 / §3.2 (T-C3): every control-channel TSBK and
    /// every signaling event of a followed call is routed on the DSP thread.
    /// Routing one must not format a metric name or take the registry lock.
    #[test]
    fn routing_a_signaling_event_does_not_allocate() {
        use trunkd_core::chan;
        use trunkd_p25::event::P25Event;

        let cfg = small_two_tuner_config();
        let shared = shared_for(&cfg);
        let (ctx, _calls_rx) = chan::bounded::<CallMsg>(8);
        let (otx, orx) = chan::bounded::<ObsMsg>(8);
        let unit = DeviceUnit::new(
            &cfg,
            &cfg.device[0],
            0,
            shared.clone(),
            Arc::new(StreamEpoch::default()),
            Arc::new(ctx),
            Arc::new(otx),
            None,
            None,
            Arc::new(AtomicBool::new(false)),
            None,
            None,
            &|i| (i == 0).then_some(0),
        );
        let router = unit.router(None);
        shared
            .metrics
            .gauge("site.0.control_frequency_hz")
            .set(851_050_000);
        let ((), control) = crate::testalloc::count(|| {
            router.route(0, 1_000, SiteEvent::Control(P25Event::TsbkFailed))
        });
        let ((), traffic) = crate::testalloc::count(|| {
            router.route(
                0,
                1_000,
                SiteEvent::Traffic {
                    frequency_hz: 852_100_000,
                    event: P25Event::TsbkFailed,
                },
            )
        });
        eprintln!("allocations per routed event: control {control}, traffic {traffic}");
        assert_eq!((control, traffic), (0, 0));
        let first = orx.try_recv().expect("the control event was queued");
        assert_eq!(first.control_hz, 851_050_000);
        assert_eq!(
            orx.try_recv().expect("and the traffic one").traffic_hz,
            852_100_000
        );
    }

    #[test]
    fn drain_commands_follows_updates_and_releases_a_grant() {
        use trunkd_core::chan;
        use trunkd_p25::event::CallKind;
        use trunkd_p25::GrantInfo;

        let cfg = small_two_tuner_config();
        let (cmd_tx, cmd_rx) = chan::bounded::<TrafficCmd>(16);
        let (ctx, calls_rx) = chan::bounded::<CallMsg>(64);
        let (otx, _orx) = chan::bounded::<ObsMsg>(16);
        let voice = DeviceUnit::new(
            &cfg,
            &cfg.device[1],
            1,
            shared_for(&cfg),
            Arc::new(StreamEpoch::default()),
            Arc::new(ctx),
            Arc::new(otx),
            None,
            Some(Arc::new(cmd_rx)),
            Arc::new(AtomicBool::new(false)),
            None,
            None,
            &|i| (i == 0).then_some(0),
        );
        let (bank, mut engines) = voice.build_engines().expect("voice engine builds");
        let router = voice.router(None);
        let freq = 852_100_000;
        let info = GrantInfo {
            kind: CallKind::Group,
            talkgroup: 100,
            target: 100,
            source: 7,
        };

        // A channel plan is applied without opening a call.
        cmd_tx.send(TrafficCmd::ChannelPlan {
            site: 0,
            bands: Vec::new(),
        });
        // A follow opens a call the coordinator routed here.
        cmd_tx.send(TrafficCmd::Follow {
            site: 0,
            frequency_hz: freq,
            info,
            at_wall_ms: 0,
        });
        voice.drain_commands(&mut engines, 1_000, &router);

        let started = calls_rx.try_recv();
        assert!(
            matches!(started, Ok(CallMsg::Started { frequency_hz, .. }) if frequency_hz == freq),
            "the followed grant opened a call: {started:?}"
        );
        assert_eq!(
            bank.borrow().channel_count(),
            1,
            "the follow subscribed the device's filterbank"
        );

        // A release closes it (its audio is not lost).
        cmd_tx.send(TrafficCmd::Release {
            site: 0,
            frequency_hz: freq,
            at_wall_ms: 0,
        });
        voice.drain_commands(&mut engines, 2_000, &router);
        assert!(
            matches!(calls_rx.try_recv(), Ok(CallMsg::Ended { .. })),
            "release ends the followed call"
        );
        assert_eq!(
            bank.borrow().channel_count(),
            0,
            "and hands the channel back"
        );
    }

    /// Two `[[site]]`s on one tuner: the device builds one filterbank and both
    /// engines read it; the control frequency both sites list is produced once.
    /// (Review 2026-09-22 §3.1 O1: each site used to build its own, and four
    /// on one HackRF overran the DSP thread.)
    #[test]
    fn sites_on_one_device_share_one_channelizer() {
        use trunkd_p25::dsp::channelizer::channelizers_built_on_this_thread;

        let toml = r#"
            [[device]]
            id = "a"
            backend = "iqfile"
            path = "/dev/null"
            sample_rate = 20000000
            center_frequency = 855500000
            [[site]]
            system = "Example"
            site = "Central"
            device = "a"
            control_frequencies = [851050000, 851787500]
            traffic_pool_size = 20
            [[site]]
            system = "Example"
            site = "West"
            devices = ["a"]
            control_frequencies = [851050000, 851550000]
            traffic_pool_size = 0
        "#;
        let cfg: Config = toml::from_str(toml).expect("parse two-site config");
        let spec = |site: &str, prefix: &str, controls: Vec<u64>| crate::pipeline::SiteSpec {
            system_label: "Example".into(),
            site_label: site.into(),
            control_frequencies: controls,
            metric_prefix: prefix.into(),
            display: format!("Example/{site}"),
        };
        let shared = Shared {
            sites: Arc::new(vec![
                spec("Central", "site.0", vec![851_050_000, 851_787_500]),
                spec("West", "site.1", vec![851_050_000, 851_550_000]),
            ]),
            ..shared_for(&cfg)
        };
        let (ctx, _cx) = trunkd_core::chan::bounded::<CallMsg>(4);
        let (otx, _ox) = trunkd_core::chan::bounded::<ObsMsg>(4);
        let unit = DeviceUnit::new(
            &cfg,
            &cfg.device[0],
            0,
            shared,
            Arc::new(StreamEpoch::default()),
            Arc::new(ctx),
            Arc::new(otx),
            None,
            None,
            Arc::new(AtomicBool::new(false)),
            None,
            None,
            &Some,
        );

        let built = channelizers_built_on_this_thread();
        let (bank, engines) = unit.build_engines().expect("engines build");
        assert_eq!(engines.len(), 2, "one engine per site");
        assert_eq!(
            channelizers_built_on_this_thread() - built,
            1,
            "two sites on one device, one filterbank"
        );
        for (_, engine) in &engines {
            assert!(std::rc::Rc::ptr_eq(engine.channel_bank(), &bank));
        }
        let b = bank.borrow();
        assert_eq!(
            b.subscription_count(),
            4,
            "each site hunts its own candidates"
        );
        assert_eq!(b.channel_count(), 3, "851.05 MHz is produced once for both");
        // Per-site pools are untouched by the sharing.
        let pools: Vec<usize> = engines.iter().map(|(_, e)| e.stats().pool_size).collect();
        assert_eq!(pools, [20, 0]);
    }

    // -- supervision: a tuner that cannot open ------------------------------

    /// The real `DeviceUnit`, under the real supervisor with the recorder's
    /// own hooks, on a tuner whose open fails every time — only the backoff
    /// durations are shrunk so the test runs in milliseconds.
    mod never_down {
        use super::*;
        use crate::pipeline::control::{self, DesiredState, DesiredTuner};
        use std::sync::atomic::AtomicU32;
        use trunkd_supervisor::{
            AfterBudget, ComponentState, RestartPolicy, Supervised, SupervisedHandle, Supervisor,
        };

        /// One `iqfile` tuner whose capture does not exist: it fails to open at
        /// once, every time, exactly as an unplugged or busy SDR does.
        fn missing_board_config() -> Config {
            let toml = r#"
                [[device]]
                id = "a"
                backend = "iqfile"
                path = "/nonexistent/trunkd-never-down.iq"
                sample_rate = 1000000
                center_frequency = 852000000
                role = "auto"
                [[site]]
                system = "S"
                site = "T"
                devices = ["a"]
                control_frequencies = [851800000]
            "#;
            toml::from_str(toml).expect("parse config")
        }

        fn tuner(json: &str) -> DesiredTuner {
            control::parse_desired(&format!(r#"{{"tuners":{{"a":{json}}}}}"#))
                .tuners
                .into_iter()
                .next()
                .expect("one tuner")
                .1
        }

        struct Rig {
            desired: Arc<DesiredState>,
            builds: Arc<AtomicU32>,
            handle: Option<SupervisedHandle>,
            _sup: Supervisor,
        }

        impl Rig {
            fn state(&self) -> ComponentState {
                self.handle.as_ref().expect("running").state()
            }
            fn builds(&self) -> u32 {
                self.builds.load(Ordering::Relaxed)
            }
            fn applied_version(&self) -> i64 {
                self.desired.applied("a").expect("known tuner").version
            }
        }

        impl Drop for Rig {
            fn drop(&mut self) {
                if let Some(h) = self.handle.take() {
                    h.stop();
                }
            }
        }

        /// Build a rig whose registry `seed` prepares first (the prefetch),
        /// retrying on a slow backoff of `slow` once three quick failures
        /// have spent the budget.
        fn spawn(slow: Duration, seed: impl FnOnce(&DesiredState)) -> Rig {
            let cfg = missing_board_config();
            let desired = DesiredState::from_config(&cfg);
            seed(&desired);
            let builds = Arc::new(AtomicU32::new(0));
            let shared = shared_for(&cfg);
            let (ctx, _crx) = trunkd_core::chan::bounded::<CallMsg>(8);
            let (otx, _orx) = trunkd_core::chan::bounded::<ObsMsg>(8);
            let (ctx, otx) = (Arc::new(ctx), Arc::new(otx));
            let factory = {
                let (desired, builds) = (desired.clone(), builds.clone());
                Box::new(move || {
                    builds.fetch_add(1, Ordering::Relaxed);
                    // What `spawn_devices`' factory does, minus the cross-tuner
                    // wiring a single tuner has no use for.
                    let built = desired.take_for_build("a");
                    let mut device = cfg.device[0].clone();
                    control::apply_override(&built.config, &mut device);
                    let control = TunerControl {
                        desired: desired.clone(),
                        device_id: "a".into(),
                        built_generation: built.generation,
                        enabled: built.config.enabled,
                        config: built.config.clone(),
                        version: built.version,
                        runtime: desired.runtime_slot("a"),
                    };
                    Box::new(DeviceUnit::new(
                        &cfg,
                        &device,
                        0,
                        shared.clone(),
                        Arc::new(StreamEpoch::default()),
                        ctx.clone(),
                        otx.clone(),
                        None,
                        None,
                        Arc::new(AtomicBool::new(false)),
                        Some(control),
                        None,
                        &|i| (i == 0).then_some(0),
                    )) as Box<dyn Supervised>
                })
            };
            let policy = RestartPolicy {
                base_backoff: Duration::from_millis(1),
                max_backoff: Duration::from_millis(2),
                max_restarts: 2,
                heartbeat_timeout: None,
                after_budget: AfterBudget::RetrySlowly {
                    base: slow,
                    max: slow * 4,
                },
                ..crate::recorder::device_policy()
            };
            let mut sup = Supervisor::new();
            let handle = sup.spawn_with(
                "device:a",
                factory,
                policy,
                crate::recorder::device_hooks(&desired, "a", "device:a"),
            );
            Rig {
                desired,
                builds,
                handle: Some(handle),
                _sup: sup,
            }
        }

        fn wait_for(mut cond: impl FnMut() -> bool) -> bool {
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline {
                if cond() {
                    return true;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            cond()
        }

        /// The production policy itself: never gives up, 30 s to 10 min.
        #[test]
        fn the_device_policy_never_gives_up() {
            let p = crate::recorder::device_policy();
            assert_eq!(
                p.after_budget,
                AfterBudget::RetrySlowly {
                    base: Duration::from_secs(30),
                    max: Duration::from_secs(600),
                }
            );
        }

        /// Past its budget the tuner is still retried, on a growing delay, and
        /// is reported `retrying` with the error — never `failed`. The keyup
        /// version it was pushed never counts as applied (this is the version
        /// a prefetch seeds after a process restart).
        #[test]
        fn a_tuner_that_cannot_open_keeps_being_retried_and_is_never_applied() {
            let rig = spawn(Duration::from_millis(20), |d| {
                d.seed_applied(
                    "a",
                    &tuner(r#"{"version":3,"config":{"center_frequency":852500000}}"#),
                )
                .expect("seeded");
            });
            assert!(wait_for(|| rig.builds() >= 7), "retried past its budget");
            assert_ne!(rig.state(), ComponentState::Failed);
            assert!(wait_for(|| rig.state() == ComponentState::Retrying));

            assert_eq!(rig.applied_version(), 0, "v3 never opened");
            assert!(rig.desired.is_pending("a"));
            let fault = rig
                .desired
                .applied("a")
                .and_then(|a| a.fault)
                .expect("the failure is on the status");
            assert!(fault.retrying);
            assert_eq!(fault.version, 3);
            assert!(fault.error.contains("trunkd-never-down.iq"), "{fault:?}");
            assert!(fault.next_attempt_ms.is_some());
            let status = rig.handle.as_ref().expect("running").status();
            assert!(status.next_attempt_ms().is_some());
            assert!(status.last_error().is_some());
        }

        /// A tuner waiting out a ten-minute backoff is retried at once by the
        /// panel's restart button, and again by a newer keyup version — which
        /// is still not reported applied, because it did not open either.
        #[test]
        fn a_restart_request_or_a_new_version_retries_at_once() {
            let rig = spawn(Duration::from_secs(600), |_| {});
            assert!(wait_for(|| rig.state() == ComponentState::Retrying));
            let before = rig.builds();

            assert!(rig.desired.request_restart("a"));
            assert!(
                wait_for(|| rig.builds() > before),
                "the restart button retries now, not in ten minutes"
            );
            assert!(wait_for(|| rig.state() == ComponentState::Retrying));
            let before = rig.builds();

            let d = tuner(r#"{"version":4,"config":{"center_frequency":852500000}}"#);
            assert_ne!(
                rig.desired.apply_desired("a", &d),
                control::ApplyDecision::NoOp
            );
            assert!(
                wait_for(|| rig.builds() > before),
                "a new keyup version retries now"
            );
            assert!(wait_for(|| rig.state() == ComponentState::Retrying));
            assert_eq!(rig.applied_version(), 0);
        }

        /// A tuner keyup has disabled idles and is never retried — not at
        /// start, and not once keyup disables a tuner that was failing.
        #[test]
        fn a_disabled_tuner_never_retries() {
            let rig = spawn(Duration::from_millis(20), |d| {
                d.seed_applied("a", &tuner(r#"{"version":2,"config":{"enabled":false}}"#))
                    .expect("seeded");
            });
            assert!(wait_for(|| rig.applied_version() == 2), "idle is applied");
            std::thread::sleep(Duration::from_millis(300));
            assert_eq!(rig.builds(), 1, "built once, never retried");
            assert_eq!(rig.state(), ComponentState::Running);
            assert!(rig.desired.applied("a").and_then(|a| a.fault).is_none());
            drop(rig);

            let rig = spawn(Duration::from_secs(600), |_| {});
            assert!(wait_for(|| rig.state() == ComponentState::Retrying));
            let d = tuner(r#"{"version":5,"config":{"enabled":false}}"#);
            rig.desired.apply_desired("a", &d);
            assert!(wait_for(|| rig.applied_version() == 5), "disabled at once");
            let builds = rig.builds();
            std::thread::sleep(Duration::from_millis(300));
            assert_eq!(rig.builds(), builds, "an idle tuner is not retried");
            assert_eq!(rig.state(), ComponentState::Running);
            assert!(rig.desired.applied("a").and_then(|a| a.fault).is_none());
        }
    }
}
