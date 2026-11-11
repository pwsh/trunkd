// SPDX-License-Identifier: GPL-3.0-or-later
//! `hackrf` backend: a HackRF One driven through the `hackrf_transfer` CLI.
//!
//! # Why a CLI pipe and not libhackrf
//!
//! Linking libhackrf (or SoapySDR's HackRF module) would put a native library
//! on the *build* path of every trunkd build, which is exactly the posture the
//! crate avoids for [`soapy`](crate::soapy): backends that need a C library at
//! build time are non-default features. `hackrf_transfer` is a stock part of
//! every HackRF install, speaks a stable command line, and writes raw
//! interleaved signed-8-bit I/Q to stdout at full rate. Piping it costs one
//! `memcpy`-class conversion pass and gives us a crash-isolated USB driver: if
//! the child wedges, we kill and respawn it instead of corrupting our own
//! address space. That makes `hackrf` a *default* feature — it needs nothing at
//! build time beyond `std`.
//!
//! The one thing libhackrf is used for is resetting the board back into the
//! PortaPack UI; see [`crate::portapack`], which `dlopen`s it at run time.
//!
//! # Shape
//!
//! [`HackRfTuner::start`] spawns
//!
//! ```text
//! hackrf_transfer -r - -f <hz> -s <rate> -a <0|1> -l <lna> -g <vga> [-d <serial>] [-C <ppm>]
//! ```
//!
//! with stdout piped, and one reader thread that reads 512 KiB chunks, converts
//! them to [`IqSample`]s scaled by `1/128`, and pushes them into the ring with
//! drop-and-count on overflow (ARCHITECTURE §2). `hackrf_transfer` writes its
//! per-second throughput lines to stderr; a second thread logs them at `debug`
//! and publishes the MiB/s figure as a gauge.
//!
//! # Retuning is a stream gap
//!
//! `hackrf_transfer` takes its frequency and gains only on the command line, so
//! [`TunerDevice::set_center_freq`] and [`TunerDevice::set_gain`] (and the
//! post-`start` [`HackRfControl`]) work by **killing the child and respawning it
//! with new arguments**. That is a real gap in the sample stream — tens of
//! milliseconds of USB setup plus the front-end settling time — and it is
//! reported as [`TunerEvent::Retuned`] once the new child is running. Callers
//! that need glitch-free retuning want the `soapy` backend.

use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use num_complex::Complex;
use trunkd_core::chan::{bounded, BoundedSender};
use trunkd_core::{DeviceId, Error, Hz, IqSample, Metric, Metrics, Result, SampleRate};

use crate::device::{DeviceDescriptor, TunerConfig, TunerDevice, TunerEvent};
use crate::stream::{RingWriter, StreamConfig, StreamParts, TunerStream};

/// LNA (IF) gain step in dB accepted by the hardware.
pub const LNA_STEP_DB: u32 = 8;
/// Maximum LNA (IF) gain in dB.
pub const LNA_MAX_DB: u32 = 40;
/// VGA (baseband) gain step in dB accepted by the hardware.
pub const VGA_STEP_DB: u32 = 2;
/// Maximum VGA (baseband) gain in dB.
pub const VGA_MAX_DB: u32 = 62;

/// Bytes read from the child per `read` call. 512 KiB of `sc8` is 256 Ki
/// complex samples — ~13 ms at 20 Msps, so syscall overhead is negligible and
/// latency stays well under the ring's depth.
const READ_CHUNK_BYTES: usize = 512 * 1024;

/// How often the killer thread checks for a stop request or a retune. Bounds
/// how long [`TunerStream::stop`] can block if the child stops producing.
const SUPERVISE_POLL: Duration = Duration::from_millis(50);

/// Configuration for opening a HackRF through `hackrf_transfer`.
#[derive(Debug, Clone, PartialEq)]
pub struct HackRfConfig {
    /// Serial number to select a specific board (`-d`). `None` takes the first.
    pub serial: Option<String>,
    /// Center (tune) frequency.
    pub center_freq: Hz,
    /// Complex sample rate; the hardware supports 2–20 Msps.
    pub sample_rate: SampleRate,
    /// LNA (IF) gain in dB: 0–40 in 8 dB steps.
    pub lna_db: u32,
    /// VGA (baseband) gain in dB: 0–62 in 2 dB steps.
    pub vga_db: u32,
    /// Front-end RF amplifier (+14 dB) enable.
    pub amp: bool,
    /// Frequency correction in ppm, passed to `hackrf_transfer -C` when it
    /// rounds to a non-zero integer.
    pub ppm: f64,
    /// Path to the `hackrf_transfer` binary. Defaults to `hackrf_transfer`
    /// (resolved on `PATH`); tests point it at a stub.
    pub transfer_bin: PathBuf,
}

impl HackRfConfig {
    /// A config with sane receive defaults (LNA 24 dB, VGA 20 dB, amp off).
    pub fn new(center_freq: Hz, sample_rate: SampleRate) -> Self {
        HackRfConfig {
            serial: None,
            center_freq,
            sample_rate,
            lna_db: 24,
            vga_db: 20,
            amp: false,
            ppm: 0.0,
            transfer_bin: PathBuf::from("hackrf_transfer"),
        }
    }

    /// Reject gains the hardware cannot represent and a zero sample rate.
    pub fn validate(&self) -> Result<()> {
        validate_gains(self.lna_db, self.vga_db)?;
        if self.sample_rate.as_u32() == 0 {
            return Err(Error::config("hackrf: sample_rate must be > 0"));
        }
        Ok(())
    }
}

impl Default for HackRfConfig {
    fn default() -> Self {
        HackRfConfig::new(Hz::mhz(855), SampleRate(20_000_000))
    }
}

/// Validate an LNA/VGA gain pair against the hardware's ranges and steps.
///
/// Shared with the binary's config validation so both reject the same values.
pub fn validate_gains(lna_db: u32, vga_db: u32) -> Result<()> {
    if lna_db > LNA_MAX_DB || !lna_db.is_multiple_of(LNA_STEP_DB) {
        return Err(Error::config(format!(
            "hackrf: lna_db must be 0..={LNA_MAX_DB} in {LNA_STEP_DB} dB steps, got {lna_db}"
        )));
    }
    if vga_db > VGA_MAX_DB || !vga_db.is_multiple_of(VGA_STEP_DB) {
        return Err(Error::config(format!(
            "hackrf: vga_db must be 0..={VGA_MAX_DB} in {VGA_STEP_DB} dB steps, got {vga_db}"
        )));
    }
    Ok(())
}

/// Split a single total gain figure across LNA and VGA on their legal steps.
///
/// [`TunerDevice::set_gain`] carries one number; the HackRF has two knobs. The
/// LNA is filled first (it is ahead of the mixer, so it buys noise figure), then
/// the remainder goes to the VGA. Both are truncated to their step and clamped.
pub fn split_gain(total_db: f64) -> (u32, u32) {
    let total = total_db.clamp(0.0, (LNA_MAX_DB + VGA_MAX_DB) as f64) as u32;
    let lna = (total / LNA_STEP_DB * LNA_STEP_DB).min(LNA_MAX_DB);
    let vga = (total - lna) / VGA_STEP_DB * VGA_STEP_DB;
    (lna, vga.min(VGA_MAX_DB))
}

/// The knobs that live on the `hackrf_transfer` command line, i.e. the ones a
/// change to forces a child restart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TuneState {
    center_freq: Hz,
    lna_db: u32,
    vga_db: u32,
    amp: bool,
}

/// State shared between the owner of the tuner and its reader thread.
///
/// `generation` is bumped on every accepted change; the reader thread compares
/// it against the generation the running child was spawned with and restarts on
/// a mismatch.
#[derive(Debug)]
struct Shared {
    state: Mutex<TuneState>,
    generation: AtomicU64,
}

impl Shared {
    fn get(&self) -> TuneState {
        *self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn update(&self, f: impl FnOnce(&mut TuneState)) {
        let mut guard = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let before = *guard;
        f(&mut guard);
        let changed = *guard != before;
        drop(guard);
        if changed {
            self.generation.fetch_add(1, Ordering::AcqRel);
        }
    }
}

/// A retune handle that stays usable after [`TunerDevice::start`] consumed the
/// tuner.
///
/// Every setter restarts the `hackrf_transfer` child, which is a gap in the
/// sample stream; the reader thread emits [`TunerEvent::Retuned`] once the new
/// child is up. Cheap to clone and safe to share across threads.
#[derive(Debug, Clone)]
pub struct HackRfControl {
    shared: Arc<Shared>,
}

impl HackRfControl {
    /// Re-tune to `freq`, restarting the child.
    pub fn set_center_freq(&self, freq: Hz) {
        self.shared.update(|s| s.center_freq = freq);
    }

    /// Set both gain stages, restarting the child. Rejects illegal steps.
    pub fn set_gains(&self, lna_db: u32, vga_db: u32) -> Result<()> {
        validate_gains(lna_db, vga_db)?;
        self.shared.update(|s| {
            s.lna_db = lna_db;
            s.vga_db = vga_db;
        });
        Ok(())
    }

    /// Enable or disable the +14 dB front-end amplifier, restarting the child.
    pub fn set_amp(&self, amp: bool) {
        self.shared.update(|s| s.amp = amp);
    }

    /// The center frequency currently requested.
    pub fn center_freq(&self) -> Hz {
        self.shared.get().center_freq
    }

    /// The gain pair currently requested, as `(lna_db, vga_db)`.
    pub fn gains(&self) -> (u32, u32) {
        let s = self.shared.get();
        (s.lna_db, s.vga_db)
    }
}

/// A HackRF One driven through the `hackrf_transfer` CLI.
pub struct HackRfTuner {
    descriptor: DeviceDescriptor,
    config: TunerConfig,
    hw: HackRfConfig,
    shared: Arc<Shared>,
}

impl HackRfTuner {
    /// Open a HackRF. Validates gains; does not touch the hardware (the child
    /// process claims the USB device only in [`TunerDevice::start`]).
    pub fn open(hw: HackRfConfig) -> Result<Self> {
        hw.validate()?;
        let id = DeviceId::new(match &hw.serial {
            Some(s) => format!("hackrf:{s}"),
            None => "hackrf:default".to_owned(),
        });
        let descriptor = DeviceDescriptor {
            id,
            driver: "hackrf".to_owned(),
            label: match &hw.serial {
                Some(s) => format!("HackRF One {s}"),
                None => "HackRF One".to_owned(),
            },
            args: format!(
                "serial={};lna={};vga={};amp={}",
                hw.serial.as_deref().unwrap_or(""),
                hw.lna_db,
                hw.vga_db,
                u8::from(hw.amp)
            ),
        };
        let config = TunerConfig {
            center_freq: hw.center_freq,
            sample_rate: hw.sample_rate,
            gain: Some((hw.lna_db + hw.vga_db) as f64),
            ppm: hw.ppm,
        };
        let shared = Arc::new(Shared {
            state: Mutex::new(TuneState {
                center_freq: hw.center_freq,
                lna_db: hw.lna_db,
                vga_db: hw.vga_db,
                amp: hw.amp,
            }),
            generation: AtomicU64::new(0),
        });
        Ok(HackRfTuner {
            descriptor,
            config,
            hw,
            shared,
        })
    }

    /// A retune handle that outlives [`TunerDevice::start`]. Take it before
    /// starting if the stream will need to be re-tuned.
    pub fn control(&self) -> HackRfControl {
        HackRfControl {
            shared: self.shared.clone(),
        }
    }

    /// The HackRF-specific configuration this tuner was opened with.
    pub fn hw_config(&self) -> &HackRfConfig {
        &self.hw
    }

    /// Enumerate boards by parsing the output of `info_bin` (`hackrf_info`).
    ///
    /// Returns an empty vector when the binary is missing or finds no board;
    /// enumeration is a discovery aid, never a hard failure.
    pub fn enumerate_with(info_bin: &str) -> Vec<DeviceDescriptor> {
        let out = match Command::new(info_bin).output() {
            Ok(o) => o,
            Err(e) => {
                tracing::debug!(bin = info_bin, error = %e, "hackrf_info not runnable");
                return Vec::new();
            }
        };
        let text = String::from_utf8_lossy(&out.stdout);
        parse_hackrf_info(&text)
    }
}

/// Parse `hackrf_info` output into one descriptor per board.
///
/// The format is a `Found HackRF` header followed by `Key: value` lines; only
/// the serial and board id are load-bearing here.
fn parse_hackrf_info(text: &str) -> Vec<DeviceDescriptor> {
    let mut out = Vec::new();
    let mut serial: Option<String> = None;
    let mut board: Option<String> = None;

    // Boards are separated by the `Found HackRF` banner; flush on each banner
    // and once more at the end.
    let mut flush = |serial: &mut Option<String>, board: &mut Option<String>| {
        if let Some(s) = serial.take() {
            let board = board.take().unwrap_or_else(|| "HackRF".to_owned());
            out.push(DeviceDescriptor {
                id: DeviceId::new(format!("hackrf:{s}")),
                driver: "hackrf".to_owned(),
                label: format!("{board} ({s})"),
                args: format!("serial={s}"),
            });
        } else {
            *board = None;
        }
    };

    for line in text.lines() {
        let line = line.trim();
        if line.starts_with("Found HackRF") {
            flush(&mut serial, &mut board);
        } else if let Some(v) = line.strip_prefix("Serial number:") {
            serial = Some(v.trim().to_owned());
        } else if let Some(v) = line.strip_prefix("Board ID Number:") {
            // "2 (HackRF One)" -> "HackRF One"
            let v = v.trim();
            let name = v
                .split_once('(')
                .and_then(|(_, rest)| rest.strip_suffix(')'))
                .unwrap_or(v)
                .trim();
            board = Some(name.to_owned());
        }
    }
    flush(&mut serial, &mut board);
    out
}

impl TunerDevice for HackRfTuner {
    fn enumerate() -> Vec<DeviceDescriptor> {
        HackRfTuner::enumerate_with("hackrf_info")
    }

    fn descriptor(&self) -> &DeviceDescriptor {
        &self.descriptor
    }

    fn config(&self) -> &TunerConfig {
        &self.config
    }

    /// Re-tune. Before [`TunerDevice::start`] this only records the frequency;
    /// on a running stream it restarts the child process, leaving a gap.
    fn set_center_freq(&mut self, freq: Hz) -> Result<()> {
        self.shared.update(|s| s.center_freq = freq);
        self.config.center_freq = freq;
        self.hw.center_freq = freq;
        Ok(())
    }

    /// Set the total gain, split across LNA and VGA by [`split_gain`]. Like
    /// [`TunerDevice::set_center_freq`], this restarts a running child.
    fn set_gain(&mut self, gain: f64) -> Result<()> {
        let (lna, vga) = split_gain(gain);
        self.shared.update(|s| {
            s.lna_db = lna;
            s.vga_db = vga;
        });
        self.config.gain = Some((lna + vga) as f64);
        self.hw.lna_db = lna;
        self.hw.vga_db = vga;
        Ok(())
    }

    fn start(self: Box<Self>, stream: StreamConfig, metrics: &Metrics) -> Result<TunerStream> {
        let (event_tx, event_rx) = bounded::<TunerEvent>(stream.event_capacity);
        let (writer, parts) = StreamParts::new(&self.descriptor.id, stream, metrics, event_rx);
        let stop = parts.stop.clone();
        // The retune slot is left unsupported: this backend re-tunes by
        // restarting `hackrf_transfer` (see `HackRfControl`), which is not the
        // uninterrupted mid-stream retune the slot promises.

        let id = self.descriptor.id.clone();
        let ctx = ReaderCtx {
            label: self.descriptor.label.clone(),
            bin: self.hw.transfer_bin.clone(),
            serial: self.hw.serial.clone(),
            sample_rate: self.hw.sample_rate,
            ppm: self.hw.ppm,
            shared: self.shared.clone(),
            stop,
            writer,
            event_tx,
            throughput: metrics.gauge(&format!("sdr.{}.hackrf_mib_per_sec", id.as_str())),
            restarts: metrics.counter(&format!("sdr.{}.hackrf_restarts", id.as_str())),
        };

        let handle = thread::Builder::new()
            .name(format!("hackrf:{}", id.as_str()))
            .spawn(move || reader_main(ctx))
            .map_err(|e| Error::device(format!("failed to spawn hackrf thread: {e}")))?;

        Ok(parts.into_stream(handle))
    }
}

/// Everything the reader thread owns.
struct ReaderCtx {
    label: String,
    bin: PathBuf,
    serial: Option<String>,
    sample_rate: SampleRate,
    ppm: f64,
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
    writer: RingWriter,
    event_tx: BoundedSender<TunerEvent>,
    throughput: Metric,
    restarts: Metric,
}

/// Build the `hackrf_transfer` argument vector for one child generation.
fn build_args(
    serial: Option<&str>,
    sample_rate: SampleRate,
    ppm: f64,
    state: &TuneState,
) -> Vec<String> {
    let mut args = vec![
        "-r".to_owned(),
        "-".to_owned(),
        "-f".to_owned(),
        state.center_freq.as_u64().to_string(),
        "-s".to_owned(),
        sample_rate.as_u32().to_string(),
        "-a".to_owned(),
        u8::from(state.amp).to_string(),
        "-l".to_owned(),
        state.lna_db.to_string(),
        "-g".to_owned(),
        state.vga_db.to_string(),
    ];
    if let Some(s) = serial {
        args.push("-d".to_owned());
        args.push(s.to_owned());
    }
    let ppm_i = ppm.round() as i64;
    if ppm_i != 0 {
        args.push("-C".to_owned());
        args.push(ppm_i.to_string());
    }
    args
}

/// Why one child's read loop ended.
enum Outcome {
    /// A retune bumped the generation; respawn with new arguments.
    Retune,
    /// [`TunerStream::stop`] was requested; shut down.
    Stopped,
    /// The child exited on its own.
    Exited(Option<i32>),
    /// The child could not be read or reaped.
    Failed(String),
}

/// The reader thread: keep one `hackrf_transfer` child alive, restarting it on
/// retune, until stopped or the child fails.
fn reader_main(mut ctx: ReaderCtx) {
    loop {
        if ctx.stop.load(Ordering::Relaxed) {
            return;
        }
        let generation = ctx.shared.generation.load(Ordering::Acquire);
        let state = ctx.shared.get();
        let args = build_args(ctx.serial.as_deref(), ctx.sample_rate, ctx.ppm, &state);

        tracing::debug!(
            device = %ctx.label,
            bin = %ctx.bin.display(),
            args = %args.join(" "),
            "spawning hackrf_transfer"
        );

        let child = Command::new(&ctx.bin)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();
        let child = match child {
            Ok(c) => c,
            Err(e) => {
                ctx.event_tx.send(TunerEvent::Error(format!(
                    "spawn {}: {e}",
                    ctx.bin.display()
                )));
                return;
            }
        };

        if generation > 0 {
            ctx.restarts.inc();
            ctx.event_tx.send(TunerEvent::Retuned(state.center_freq));
        }

        match run_child(child, generation, &mut ctx) {
            Outcome::Retune => continue,
            Outcome::Stopped => return,
            Outcome::Exited(code) => {
                ctx.throughput.set_f64(0.0);
                match code {
                    Some(0) => {
                        tracing::info!(device = %ctx.label, "hackrf_transfer finished");
                        ctx.event_tx.send(TunerEvent::EndOfStream);
                    }
                    other => {
                        let msg = match other {
                            Some(c) => format!("hackrf_transfer exited with status {c}"),
                            None => "hackrf_transfer killed by signal".to_owned(),
                        };
                        tracing::warn!(device = %ctx.label, "{msg}");
                        ctx.event_tx.send(TunerEvent::Error(msg));
                    }
                }
                return;
            }
            Outcome::Failed(msg) => {
                ctx.throughput.set_f64(0.0);
                tracing::warn!(device = %ctx.label, error = %msg, "hackrf child failed");
                ctx.event_tx.send(TunerEvent::Error(msg));
                return;
            }
        }
    }
}

/// Pump one child's stdout into the ring until it stops, is stopped, or is
/// superseded by a retune.
fn run_child(mut child: Child, generation: u64, ctx: &mut ReaderCtx) -> Outcome {
    let Some(stdout) = child.stdout.take() else {
        return Outcome::Failed("child stdout was not piped".to_owned());
    };
    let stderr = child.stderr.take();

    // stderr: hackrf_transfer's per-second throughput lines and setup chatter.
    let stderr_thread = stderr.map(|err| {
        let label = ctx.label.clone();
        let throughput = ctx.throughput.clone();
        thread::Builder::new()
            .name("hackrf-stderr".to_owned())
            .spawn(move || {
                for line in BufReader::new(err)
                    .lines()
                    .map_while(std::result::Result::ok)
                {
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }
                    if let Some(mib) = parse_throughput(line) {
                        throughput.set_f64(mib);
                    }
                    tracing::debug!(device = %label, "hackrf_transfer: {line}");
                }
            })
    });

    // Killer thread: makes `stop()` and retunes bounded even if the child stops
    // producing (a blocking read would otherwise never return).
    let child_slot = Arc::new(Mutex::new(child));
    let done = Arc::new(AtomicBool::new(false));
    let killer = {
        let child_slot = child_slot.clone();
        let done = done.clone();
        let stop = ctx.stop.clone();
        let shared = ctx.shared.clone();
        thread::Builder::new()
            .name("hackrf-killer".to_owned())
            .spawn(move || {
                while !done.load(Ordering::Relaxed) {
                    if stop.load(Ordering::Relaxed)
                        || shared.generation.load(Ordering::Acquire) != generation
                    {
                        let mut c = child_slot
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        let _ = c.kill();
                        return;
                    }
                    thread::sleep(SUPERVISE_POLL);
                }
            })
    };

    let outcome = pump(stdout, generation, ctx);

    // Tear the child down before joining helpers: killing closes stderr, which
    // is what lets the stderr thread finish.
    done.store(true, Ordering::Relaxed);
    let status = {
        let mut c = child_slot
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _ = c.kill();
        c.wait()
    };
    if let Ok(k) = killer {
        let _ = k.join();
    }
    // The stderr thread is deliberately *not* joined: anything the child left
    // behind that inherited the pipe (a wrapper script's grandchild) would keep
    // it open and block shutdown. It ends on its own when the pipe closes, and
    // it holds nothing but a metric handle.
    drop(stderr_thread);

    match outcome {
        PumpEnd::Stopped => Outcome::Stopped,
        PumpEnd::Retune => Outcome::Retune,
        PumpEnd::Failed(msg) => Outcome::Failed(msg),
        PumpEnd::Eof => match status {
            Ok(s) => Outcome::Exited(s.code()),
            Err(e) => Outcome::Failed(format!("wait: {e}")),
        },
    }
}

/// Why [`pump`] returned.
enum PumpEnd {
    Eof,
    Stopped,
    Retune,
    Failed(String),
}

/// Read `sc8` bytes from the child and push converted samples into the ring.
///
/// Allocation-free after the two buffers: one `read` per 512 KiB, one pass of
/// table lookups, one `write_batch`.
fn pump(mut stdout: impl Read, generation: u64, ctx: &mut ReaderCtx) -> PumpEnd {
    let lut = sc8_lut();
    let mut bytes = vec![0u8; READ_CHUNK_BYTES];
    let mut samples: Vec<IqSample> = vec![Complex::new(0.0, 0.0); READ_CHUNK_BYTES / 2];
    // A read can split an I/Q pair; carry the odd byte to the next chunk.
    let mut carry: Option<u8> = None;
    // Overflow is counted by the ring; the log is rate-limited so a wedged DSP
    // thread cannot turn into a log flood.
    let mut dropped_since_log: u64 = 0;
    let mut last_log = std::time::Instant::now();

    loop {
        if ctx.stop.load(Ordering::Relaxed) {
            return PumpEnd::Stopped;
        }
        if ctx.shared.generation.load(Ordering::Acquire) != generation {
            return PumpEnd::Retune;
        }

        // Keep byte 0 free for a carried I sample so the pairs stay aligned.
        let offset = usize::from(carry.is_some());
        if let Some(b) = carry.take() {
            bytes[0] = b;
        }
        let n = match stdout.read(&mut bytes[offset..]) {
            Ok(0) => {
                if ctx.stop.load(Ordering::Relaxed) {
                    return PumpEnd::Stopped;
                }
                if ctx.shared.generation.load(Ordering::Acquire) != generation {
                    return PumpEnd::Retune;
                }
                return PumpEnd::Eof;
            }
            Ok(n) => n + offset,
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => {
                carry = (offset == 1).then(|| bytes[0]);
                continue;
            }
            Err(e) => return PumpEnd::Failed(format!("read: {e}")),
        };

        let (iq, odd) = bytes[..n].as_chunks::<2>();
        carry = odd.first().copied();
        let pairs = iq.len();
        for (out, pair) in samples[..pairs].iter_mut().zip(iq) {
            *out = Complex::new(lut[pair[0] as usize], lut[pair[1] as usize]);
        }
        dropped_since_log += ctx.writer.write_batch(&samples[..pairs]) as u64;
        if dropped_since_log > 0 && last_log.elapsed() >= Duration::from_secs(1) {
            tracing::warn!(
                device = %ctx.label,
                dropped = dropped_since_log,
                "ring overflow: DSP consumer is behind"
            );
            dropped_since_log = 0;
            last_log = std::time::Instant::now();
        }
    }
}

/// Build the 256-entry `i8 -> f32/128` conversion table.
fn sc8_lut() -> [f32; 256] {
    let mut lut = [0.0f32; 256];
    for (i, v) in lut.iter_mut().enumerate() {
        *v = (i as u8 as i8) as f32 / 128.0;
    }
    lut
}

/// Parse `hackrf_transfer`'s throughput line, e.g.
/// `39.6 MiB / 1.000 sec = 39.6 MiB/second, average power -12.4 dBfs`.
fn parse_throughput(line: &str) -> Option<f64> {
    let rest = line.split_once('=')?.1.trim_start();
    let value = rest.split_whitespace().next()?;
    if !rest[value.len()..].trim_start().starts_with("MiB/second") {
        return None;
    }
    value.parse::<f64>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::path::Path;

    #[cfg(unix)]
    fn tmpdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("trunkd-hackrf-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    /// A stand-in for `hackrf_transfer`: ignores its arguments and cats a file.
    #[cfg(unix)]
    fn stub_transfer(dir: &Path, data: &Path, extra: &str) -> PathBuf {
        let script = dir.join("fake_hackrf_transfer.sh");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\n\
                 echo \"args: $*\" >&2\n\
                 echo \" 4.0 MiB / 1.000 sec =  4.0 MiB/second, average power -12.4 dBfs\" >&2\n\
                 cat '{}'\n{extra}\n",
                data.display()
            ),
        )
        .expect("write stub");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
                .expect("chmod");
        }
        script
    }

    /// Deterministic `sc8` payload: `n` samples of a ramp.
    #[cfg(unix)]
    fn write_sc8(dir: &Path, n: usize) -> (PathBuf, Vec<IqSample>) {
        let mut bytes = Vec::with_capacity(n * 2);
        let mut expect = Vec::with_capacity(n);
        for k in 0..n {
            let i = (k as i32 % 255 - 127) as i8;
            let q = (127 - (k as i32 % 255)) as i8;
            bytes.push(i as u8);
            bytes.push(q as u8);
            expect.push(Complex::new(i as f32 / 128.0, q as f32 / 128.0));
        }
        let path = dir.join("payload.c8");
        std::fs::write(&path, bytes).expect("write payload");
        (path, expect)
    }

    fn cfg_with(bin: PathBuf) -> HackRfConfig {
        HackRfConfig {
            transfer_bin: bin,
            ..HackRfConfig::new(Hz::mhz(855), SampleRate(2_000_000))
        }
    }

    #[cfg(unix)]
    fn drain(stream: &mut TunerStream, want: usize) -> Vec<IqSample> {
        let mut got = Vec::with_capacity(want);
        let mut spins = 0u64;
        loop {
            match stream.consumer.pop() {
                Ok(s) => got.push(s),
                Err(_) => {
                    if stream.is_finished() {
                        while let Ok(s) = stream.consumer.pop() {
                            got.push(s);
                        }
                        return got;
                    }
                    spins += 1;
                    assert!(spins < 200_000_000, "reader never finished");
                    std::thread::yield_now();
                }
            }
        }
    }

    #[test]
    fn rejects_illegal_gain_steps() {
        assert!(validate_gains(40, 62).is_ok());
        assert!(validate_gains(0, 0).is_ok());
        assert!(validate_gains(41, 20).is_err(), "lna off-step");
        assert!(validate_gains(48, 20).is_err(), "lna over max");
        assert!(validate_gains(24, 21).is_err(), "vga off-step");
        assert!(validate_gains(24, 64).is_err(), "vga over max");

        let bad = HackRfConfig {
            lna_db: 7,
            ..HackRfConfig::default()
        };
        assert!(HackRfTuner::open(bad).is_err());
    }

    #[test]
    fn splits_total_gain_onto_legal_steps() {
        assert_eq!(split_gain(0.0), (0, 0));
        assert_eq!(split_gain(40.0), (40, 0));
        assert_eq!(split_gain(45.0), (40, 4));
        assert_eq!(split_gain(23.0), (16, 6));
        assert_eq!(split_gain(1000.0), (40, 62));
        assert_eq!(split_gain(-5.0), (0, 0));
    }

    #[test]
    fn builds_the_expected_command_line() {
        let state = TuneState {
            center_freq: Hz::new(855_500_000),
            lna_db: 40,
            vga_db: 40,
            amp: true,
        };
        let args = build_args(Some("abc123"), SampleRate(20_000_000), 0.0, &state);
        assert_eq!(
            args.join(" "),
            "-r - -f 855500000 -s 20000000 -a 1 -l 40 -g 40 -d abc123"
        );
        let args = build_args(None, SampleRate(20_000_000), 2.4, &state);
        assert!(args.ends_with(&["-C".to_owned(), "2".to_owned()]));
    }

    #[test]
    fn parses_hackrf_info_output() {
        let text = "hackrf_info version: 2023.01.1\n\
                    libhackrf version: 2023.01.1 (0.8)\n\
                    Found HackRF\n\
                    Index: 0\n\
                    Serial number: 0000000000000000deadbeef00000001\n\
                    Board ID Number: 2 (HackRF One)\n\
                    Firmware Version: n_260726 (API:1.13)\n";
        let d = parse_hackrf_info(text);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].id.as_str(), "hackrf:0000000000000000deadbeef00000001");
        assert_eq!(d[0].driver, "hackrf");
        assert!(d[0].label.starts_with("HackRF One ("), "{}", d[0].label);
        assert_eq!(d[0].args, "serial=0000000000000000deadbeef00000001");

        assert!(parse_hackrf_info("No HackRF boards found.\n").is_empty());
        assert_eq!(
            parse_hackrf_info(
                "Found HackRF\nSerial number: aaa\nBoard ID Number: 2 (HackRF One)\n\
                 Found HackRF\nSerial number: bbb\nBoard ID Number: 2 (HackRF One)\n"
            )
            .len(),
            2
        );
    }

    #[test]
    fn parses_throughput_lines() {
        assert_eq!(
            parse_throughput("39.6 MiB / 1.000 sec = 39.6 MiB/second, average power -12.4 dBfs"),
            Some(39.6)
        );
        assert_eq!(
            parse_throughput(" 0.5 MiB / 0.013 sec = 40.7 MiB/second"),
            Some(40.7)
        );
        assert_eq!(parse_throughput("Stop with Ctrl-C"), None);
        assert_eq!(parse_throughput("Total time: 2.01298 s"), None);
    }

    // The spawn tests below stand `hackrf_transfer` up as a `#!/bin/sh` stub,
    // which Windows cannot execute; the production spawn path they cover is
    // exercised by the unix run.
    #[cfg(unix)]
    #[test]
    fn converts_samples_and_reports_end_of_stream() {
        let dir = tmpdir("convert");
        let n = 50_000;
        let (payload, expect) = write_sc8(&dir, n);
        let stub = stub_transfer(&dir, &payload, "");

        let tuner = Box::new(HackRfTuner::open(cfg_with(stub)).expect("open"));
        let metrics = Metrics::new();
        let mut stream = tuner
            .start(
                StreamConfig {
                    ring_capacity: 1 << 18,
                    event_capacity: 8,
                },
                &metrics,
            )
            .expect("start");

        let got = drain(&mut stream, n);
        assert_eq!(got.len(), n, "sample count");
        assert_eq!(&got[..64], &expect[..64], "leading samples");
        assert_eq!(&got[n - 64..], &expect[n - 64..], "trailing samples");
        assert_eq!(stream.produced_count(), n as u64);
        assert_eq!(stream.overflow_count(), 0);

        match stream.events.recv() {
            Some(TunerEvent::EndOfStream) => {}
            other => panic!("expected EndOfStream when the child exits, got {other:?}"),
        }

        // The throughput gauge is registered under the device id (its value is
        // zeroed when the child exits, so only registration is asserted here;
        // the parsing itself is covered by `parses_throughput_lines`).
        assert!(metrics
            .snapshot()
            .iter()
            .any(|m| m.name == "sdr.hackrf:default.hackrf_mib_per_sec"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn counts_overflow_when_the_ring_is_too_small() {
        let dir = tmpdir("overflow");
        let n = 40_000;
        let (payload, _) = write_sc8(&dir, n);
        let stub = stub_transfer(&dir, &payload, "");

        let tuner = Box::new(HackRfTuner::open(cfg_with(stub)).expect("open"));
        let metrics = Metrics::new();
        let stream = tuner
            .start(
                StreamConfig {
                    ring_capacity: 1024,
                    event_capacity: 8,
                },
                &metrics,
            )
            .expect("start");

        // Never drain: everything past the ring must be dropped and counted,
        // never buffered (ARCHITECTURE §2).
        let mut spins = 0u64;
        while !stream.is_finished() {
            spins += 1;
            assert!(spins < 200_000_000, "reader never finished");
            std::thread::yield_now();
        }
        assert_eq!(stream.produced_count(), 1024);
        assert_eq!(stream.overflow_count(), n as u64 - 1024);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn nonzero_child_exit_is_an_error_event() {
        let dir = tmpdir("exiterr");
        let (payload, _) = write_sc8(&dir, 1000);
        let stub = stub_transfer(&dir, &payload, "exit 7");

        let tuner = Box::new(HackRfTuner::open(cfg_with(stub)).expect("open"));
        let metrics = Metrics::new();
        let mut stream = tuner
            .start(StreamConfig::default(), &metrics)
            .expect("start");
        let _ = drain(&mut stream, 1000);

        match stream.events.recv() {
            Some(TunerEvent::Error(msg)) => assert!(msg.contains('7'), "{msg}"),
            other => panic!("expected Error, got {other:?}"),
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn stop_kills_a_child_that_never_ends() {
        let dir = tmpdir("stop");
        let script = dir.join("forever.sh");
        // Produces nothing at all: only the killer thread can unblock the read.
        std::fs::write(&script, "#!/bin/sh\nexec sleep 600\n").expect("write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
                .expect("chmod");
        }

        // Writing a script and exec'ing it while other tests fork can hit a
        // transient ETXTBSY, so give the spawn a few attempts before deciding
        // the child really failed to start.
        let metrics = Metrics::new();
        let mut stream = None;
        for attempt in 0..5 {
            let tuner = Box::new(HackRfTuner::open(cfg_with(script.clone())).expect("open"));
            let s = tuner
                .start(StreamConfig::default(), &metrics)
                .expect("start");
            std::thread::sleep(Duration::from_millis(150));
            if !s.is_finished() {
                stream = Some(s);
                break;
            }
            assert!(
                attempt < 4,
                "child never stayed up: {:?}",
                s.events.try_recv()
            );
        }
        let stream = stream.expect("a child stayed up");

        let t0 = std::time::Instant::now();
        stream.stop();
        assert!(
            t0.elapsed() < Duration::from_secs(5),
            "stop() took {:?}",
            t0.elapsed()
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_binary_reports_an_error_event() {
        let tuner = Box::new(
            HackRfTuner::open(cfg_with(PathBuf::from("/no/such/hackrf_transfer"))).expect("open"),
        );
        let metrics = Metrics::new();
        let stream = tuner
            .start(StreamConfig::default(), &metrics)
            .expect("start");
        match stream.events.recv() {
            Some(TunerEvent::Error(msg)) => assert!(msg.contains("spawn"), "{msg}"),
            other => panic!("expected Error, got {other:?}"),
        }
        assert_eq!(stream.produced_count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn retune_restarts_the_child_and_emits_retuned() {
        let dir = tmpdir("retune");
        let (payload, _) = write_sc8(&dir, 20_000);
        // Each child streams the payload then idles, so only a kill ends it.
        // `exec` keeps the idle process *as* the child: no grandchild is left
        // holding the pipes open, which is how the real (exec'd, un-wrapped)
        // `hackrf_transfer` behaves.
        let stub = stub_transfer(&dir, &payload, "exec sleep 600");

        let tuner = HackRfTuner::open(cfg_with(stub)).expect("open");
        let control = tuner.control();
        let metrics = Metrics::new();
        let mut stream = Box::new(tuner)
            .start(StreamConfig::default(), &metrics)
            .expect("start");

        // Wait for the first child's payload.
        let mut spins = 0u64;
        while stream.produced_count() < 20_000 {
            spins += 1;
            assert!(spins < 200_000_000, "first child never produced");
            std::thread::yield_now();
        }
        while stream.consumer.pop().is_ok() {}

        control.set_center_freq(Hz::new(851_000_000));
        // The second child re-streams the payload after the restart.
        let mut spins = 0u64;
        while stream.produced_count() < 40_000 {
            spins += 1;
            assert!(spins < 200_000_000, "second child never produced");
            std::thread::yield_now();
        }

        match stream.events.recv() {
            Some(TunerEvent::Retuned(f)) => assert_eq!(f, Hz::new(851_000_000)),
            other => panic!("expected Retuned, got {other:?}"),
        }
        assert_eq!(
            metrics.counter("sdr.hackrf:default.hackrf_restarts").get(),
            1
        );
        stream.stop();

        let _ = std::fs::remove_dir_all(&dir);
    }
}
