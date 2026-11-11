// SPDX-License-Identifier: GPL-3.0-or-later
//! `iqfile` backend: play back an interleaved IQ file as a virtual tuner.
//!
//! Supports HackRF-style signed 8-bit interleaved (`.iq`/`.c8`), 32-bit float
//! interleaved (`.cf32`), and (optionally) float WAV. Playback is either
//! real-time-paced (honoring the sample rate) or unthrottled for tests.

use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Instant;

use num_complex::Complex;
use trunkd_core::chan::bounded;
use trunkd_core::{DeviceId, Error, Hz, IqSample, Metrics, Result, SampleRate};

use crate::device::{DeviceDescriptor, TunerConfig, TunerDevice, TunerEvent};
use crate::stream::{StreamConfig, StreamParts, TunerStream};

/// On-disk sample encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IqFormat {
    /// Signed 8-bit interleaved I/Q (HackRF `hackrf_transfer` output, `.c8`).
    Sc8,
    /// 32-bit little-endian float interleaved I/Q (`.cf32`).
    Cf32,
    /// Float WAV, two channels = I/Q (requires the `iqfile` feature's `hound`).
    WavFloat,
}

impl IqFormat {
    /// Guess a format from a file extension; defaults to [`IqFormat::Sc8`].
    pub fn from_extension(path: &Path) -> IqFormat {
        match path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .as_deref()
        {
            Some("cf32") | Some("fc32") => IqFormat::Cf32,
            Some("wav") => IqFormat::WavFloat,
            _ => IqFormat::Sc8,
        }
    }
}

/// Playback pacing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pacing {
    /// Emit samples at the configured sample rate (approximates real hardware).
    RealTime,
    /// Emit as fast as possible (tests, offline replay).
    Unthrottled,
}

/// One clock for several unthrottled file readers in the same run.
///
/// Two tuners replaying captures of the same air must see the same moment at
/// about the same time, or whatever one of them learns from the air — a grant
/// on the control channel — reaches the other at a point in *its* file that is
/// arbitrarily far away. Real-time pacing gets that for free from the wall
/// clock; unthrottled readers each run at the speed of their own DSP thread,
/// so a lightly loaded voice front can race seconds ahead of the control front
/// and miss every call it is handed.
///
/// Every reader that [joins](Lockstep::join) a group publishes how far its
/// *consumer* has got (samples taken out of its ring, not merely read from
/// disk) and writes a batch only while that batch ends within a
/// [window](lockstep_window) of the slowest other member's consumer. So no
/// DSP thread in the group ever gets more than one window plus one batch
/// ahead of another. A member leaves when its reader thread ends (stop, error,
/// or end of file once its consumer has drained the ring), so a finished or
/// restarted tuner never holds the others up.
///
/// Readers start one after another (in the recorder, each device thread joins
/// when it opens its tuner, after building its engines), so the group must be
/// told up front whom to wait for: [`expect`](Lockstep::expect) reserves a
/// place for a member that has not joined yet, and a reserved place counts as
/// a peer whose consumer is still at 0. A reader that joins early therefore
/// writes one window and waits rather than running free through its file.
/// A reservation ends when its member joins, when it is
/// [withdrawn](Lockstep::withdraw) (the member failed to open, is disabled, or
/// will not replay unthrottled), or at the [join
/// deadline](Lockstep::set_join_deadline) counted from the first join, after
/// which the absent members are dropped with a warning and the rest go on
/// without them. A group with no reservations behaves as before.
///
/// Live tuners have no use for it and real-time playback ignores it.
#[derive(Debug, Default)]
pub struct Lockstep {
    state: Mutex<LockstepState>,
}

/// How long members that have joined wait for reserved members that have not,
/// counted from the first join. Generous next to the time a device thread
/// takes from spawn to open; a reservation nobody withdraws (a code path this
/// misses) costs at most this much replay time, never samples.
pub const DEFAULT_JOIN_DEADLINE: std::time::Duration = std::time::Duration::from_secs(5);

#[derive(Debug)]
struct LockstepState {
    /// Consumer positions of the members that have joined.
    members: Vec<Arc<AtomicU64>>,
    /// Names reserved with [`Lockstep::expect`] that have not joined yet.
    expected: Vec<String>,
    /// When the first member joined: the join deadline counts from here.
    first_join: Option<Instant>,
    join_deadline: std::time::Duration,
}

impl Default for LockstepState {
    fn default() -> Self {
        LockstepState {
            members: Vec::new(),
            expected: Vec::new(),
            first_join: None,
            join_deadline: DEFAULT_JOIN_DEADLINE,
        }
    }
}

impl Lockstep {
    /// An empty group.
    pub fn new() -> Arc<Lockstep> {
        Arc::new(Lockstep::default())
    }

    /// Reserve a place for `member`, which will join later under that name
    /// (see [`IqFileTuner::with_lockstep`]). Until it joins, is withdrawn, or
    /// the join deadline passes, every member that has joined is held within
    /// one window of stream position 0. Reserving a name twice is one
    /// reservation.
    pub fn expect(&self, member: impl Into<String>) {
        let member = member.into();
        let mut state = self.lock();
        if !state.expected.contains(&member) {
            state.expected.push(member);
        }
    }

    /// Give up a reservation: `member` will not join (it failed to open, is
    /// disabled, or is not replaying unthrottled). A no-op for a name that is
    /// not reserved, including one that has already joined.
    pub fn withdraw(&self, member: &str) {
        let mut state = self.lock();
        let before = state.expected.len();
        state.expected.retain(|m| m != member);
        if state.expected.len() != before {
            tracing::debug!(member, "replay clock: reservation withdrawn");
        }
    }

    /// How long joined members wait for reserved ones, counted from the first
    /// join (default [`DEFAULT_JOIN_DEADLINE`]).
    pub fn set_join_deadline(&self, deadline: std::time::Duration) {
        self.lock().join_deadline = deadline;
    }

    /// Names reserved that have not joined (or been dropped) yet.
    pub fn pending(&self) -> Vec<String> {
        self.lock().expected.clone()
    }

    /// Join the group at stream position 0, taking `member`'s reservation if
    /// there is one.
    fn join(self: &Arc<Self>, member: Option<&str>) -> LockstepMember {
        let position = Arc::new(AtomicU64::new(0));
        let mut state = self.lock();
        state.members.push(position.clone());
        if let Some(name) = member {
            state.expected.retain(|m| m != name);
        }
        state.first_join.get_or_insert_with(Instant::now);
        drop(state);
        LockstepMember {
            group: self.clone(),
            position,
        }
    }

    /// Poison is ignored: a reader that panicked must not wedge the others.
    fn lock(&self) -> std::sync::MutexGuard<'_, LockstepState> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }
}

/// How far ahead of the slowest peer's consumer a reader may write, in samples:
/// 50 ms of stream, and never less than two batches, so that a group whose
/// consumers have all drained their rings can always make progress (the
/// reader furthest behind is then never blocked).
pub fn lockstep_window(sample_rate: SampleRate, batch_size: usize) -> u64 {
    (u64::from(sample_rate.0) / 20).max(2 * batch_size as u64)
}

/// A reader's place in a [`Lockstep`] group; leaves the group on drop.
struct LockstepMember {
    group: Arc<Lockstep>,
    position: Arc<AtomicU64>,
}

impl LockstepMember {
    /// Where the slowest *other* member's consumer is, or `None` alone. A
    /// reserved member that has not joined counts as a peer at 0 until the
    /// join deadline, when the group drops every such reservation.
    fn slowest_peer(&self) -> Option<u64> {
        let mut state = self.group.lock();
        if !state.expected.is_empty()
            && state
                .first_join
                .is_some_and(|t| t.elapsed() >= state.join_deadline)
        {
            tracing::warn!(
                missing = %state.expected.join(", "),
                deadline_ms = state.join_deadline.as_millis() as u64,
                "replay clock: expected iqfile tuners did not start in time; \
                 replaying without them (a late start is not kept in step)"
            );
            state.expected.clear();
        }
        if !state.expected.is_empty() {
            return Some(0);
        }
        state
            .members
            .iter()
            .filter(|p| !Arc::ptr_eq(p, &self.position))
            .map(|p| p.load(Ordering::Acquire))
            .min()
    }

    /// Block until writing `n` more samples after `written` keeps this reader
    /// within `window` of the slowest peer's consumer, publishing this reader's
    /// own consumer position while it waits (a peer may be waiting on it).
    /// Returns `false` if the wait was cut short by `stop`.
    fn wait_turn(
        &self,
        writer: &crate::stream::RingWriter,
        written: u64,
        n: usize,
        window: u64,
        stop: &std::sync::atomic::AtomicBool,
    ) -> bool {
        loop {
            self.position.store(writer.consumed(), Ordering::Release);
            match self.slowest_peer() {
                Some(peer) if written + n as u64 > peer.saturating_add(window) => {}
                _ => return true,
            }
            if stop.load(Ordering::Relaxed) {
                return false;
            }
            thread::sleep(std::time::Duration::from_micros(200));
        }
    }

    /// After end of file: stay in the group, publishing this reader's consumer
    /// position, until that consumer has emptied the ring (or `stop`). Leaving
    /// at end of file would release every peer while this consumer still had
    /// a ring's worth of backlog to work through, and a peer could then run
    /// that far past the window.
    fn stay_until_drained(
        &self,
        writer: &crate::stream::RingWriter,
        stop: &std::sync::atomic::AtomicBool,
    ) {
        loop {
            self.position.store(writer.consumed(), Ordering::Release);
            if writer.queued() == 0 || stop.load(Ordering::Relaxed) {
                return;
            }
            thread::sleep(std::time::Duration::from_micros(200));
        }
    }
}

impl Drop for LockstepMember {
    fn drop(&mut self) {
        self.group
            .lock()
            .members
            .retain(|p| !Arc::ptr_eq(p, &self.position));
    }
}

/// A virtual tuner backed by an IQ file.
pub struct IqFileTuner {
    descriptor: DeviceDescriptor,
    config: TunerConfig,
    path: PathBuf,
    format: IqFormat,
    pacing: Pacing,
    batch_size: usize,
    /// The group this reader keeps step with, and the name it joins under.
    lockstep: Option<(Arc<Lockstep>, String)>,
}

impl IqFileTuner {
    /// Open an IQ file as a virtual tuner.
    ///
    /// `config.sample_rate` governs real-time pacing; `config.center_freq` is
    /// carried through as metadata (the file is already at baseband).
    pub fn open(
        path: impl Into<PathBuf>,
        format: IqFormat,
        config: TunerConfig,
        pacing: Pacing,
    ) -> Result<Self> {
        let path = path.into();
        if !path.exists() {
            return Err(Error::device(format!(
                "iq file not found: {}",
                path.display()
            )));
        }
        let id = DeviceId::new(format!("iqfile:{}", path.display()));
        let descriptor = DeviceDescriptor {
            id,
            driver: "iqfile".to_owned(),
            label: format!("IQ file {}", path.display()),
            args: format!("path={};format={:?}", path.display(), format),
        };
        Ok(IqFileTuner {
            descriptor,
            config,
            path,
            format,
            pacing,
            batch_size: 8192,
            lockstep: None,
        })
    }

    /// Keep this reader in step with the other members of `group` (see
    /// [`Lockstep`]), joining under `member`: the name its place was reserved
    /// under with [`Lockstep::expect`], if it was. Only unthrottled playback
    /// honours it; a real-time reader withdraws its reservation at start.
    pub fn with_lockstep(mut self, group: Arc<Lockstep>, member: impl Into<String>) -> Self {
        self.lockstep = Some((group, member.into()));
        self
    }

    /// Override the per-batch sample count (mostly for tests).
    pub fn with_batch_size(mut self, n: usize) -> Self {
        self.batch_size = n.max(1);
        self
    }
}

impl TunerDevice for IqFileTuner {
    fn enumerate() -> Vec<DeviceDescriptor> {
        // Files are not discoverable without a path; open them explicitly.
        Vec::new()
    }

    fn descriptor(&self) -> &DeviceDescriptor {
        &self.descriptor
    }

    fn config(&self) -> &TunerConfig {
        &self.config
    }

    fn set_center_freq(&mut self, freq: Hz) -> Result<()> {
        self.config.center_freq = freq;
        Ok(())
    }

    fn set_gain(&mut self, gain: f64) -> Result<()> {
        self.config.gain = Some(gain);
        Ok(())
    }

    fn start(self: Box<Self>, stream: StreamConfig, metrics: &Metrics) -> Result<TunerStream> {
        let (event_tx, event_rx) = bounded::<TunerEvent>(stream.event_capacity);
        let (mut writer, parts) = StreamParts::new(&self.descriptor.id, stream, metrics, event_rx);
        let stop = parts.stop.clone();
        // The retune slot is deliberately left unsupported: a file has one
        // fixed center frequency, already baked into its samples.

        let path = self.path.clone();
        let format = self.format;
        let pacing = self.pacing;
        let sample_rate = self.config.sample_rate;
        let batch_size = self.batch_size;
        let label = self.descriptor.label.clone();
        // Joined here, before the reader thread exists, so this reader is
        // counted from its first batch. It is bounded by the members that have
        // joined and by those reserved but not yet joined (see `Lockstep`), so
        // members started one after another still begin together.
        let member = match (&self.lockstep, pacing) {
            (Some((group, name)), Pacing::Unthrottled) => Some(group.join(Some(name))),
            (Some((group, name)), Pacing::RealTime) => {
                group.withdraw(name);
                None
            }
            (None, _) => None,
        };
        let window = lockstep_window(sample_rate, batch_size);

        let handle = thread::Builder::new()
            .name(format!("iqfile:{}", self.descriptor.id.as_str()))
            .spawn(move || {
                let mut reader = match SampleReader::open(&path, format) {
                    Ok(r) => r,
                    Err(e) => {
                        event_tx.send(TunerEvent::Error(e.to_string()));
                        return;
                    }
                };
                let mut pacer = Pacer::new(pacing, sample_rate);
                let mut batch: Vec<IqSample> = Vec::with_capacity(batch_size);
                let mut written: u64 = 0;

                loop {
                    if stop.load(std::sync::atomic::Ordering::Relaxed) {
                        return;
                    }
                    batch.clear();
                    match reader.next_batch(batch_size, &mut batch) {
                        Ok(0) => {
                            // The event goes first: a consumer that takes only
                            // whole blocks takes the last partial one only once
                            // it has seen end of stream, so waiting for the
                            // drain before sending it could never finish.
                            event_tx.send(TunerEvent::EndOfStream);
                            if let Some(m) = &member {
                                m.stay_until_drained(&writer, &stop);
                            }
                            return;
                        }
                        Ok(n) => {
                            // Unthrottled playback would otherwise stream at
                            // disk speed and lose most of the file to ring
                            // overflow. A file has no real-time deadline, so
                            // waiting for space is strictly better than
                            // dropping: an offline replay must be able to
                            // decode every sample it was given.
                            if let Some(m) = &member {
                                if !m.wait_turn(&writer, written, n, window, &stop) {
                                    return;
                                }
                            }
                            if pacing == Pacing::Unthrottled
                                && !wait_for_space(&writer, batch.len(), &stop)
                            {
                                return;
                            }
                            writer.write_batch(&batch);
                            written += n as u64;
                            pacer.wait_for(n as u64);
                        }
                        Err(e) => {
                            tracing::warn!(device = %label, error = %e, "iqfile read error");
                            event_tx.send(TunerEvent::Error(e.to_string()));
                            return;
                        }
                    }
                }
            })
            .map_err(|e| Error::device(format!("failed to spawn iqfile thread: {e}")))?;

        Ok(parts.into_stream(handle))
    }
}

/// Block until the ring can take `wanted` samples, or the tuner is stopped.
///
/// Returns `false` if the wait was cut short by `stop`, in which case the
/// caller should abandon the batch and exit.
fn wait_for_space(
    writer: &crate::stream::RingWriter,
    wanted: usize,
    stop: &std::sync::atomic::AtomicBool,
) -> bool {
    while writer.free_slots() < wanted {
        if stop.load(std::sync::atomic::Ordering::Relaxed) {
            return false;
        }
        // Short enough that the consumer is never starved waiting for us, long
        // enough that a slow DSP thread does not cost a spinning core.
        thread::sleep(std::time::Duration::from_micros(200));
    }
    true
}

/// Paces batch emission to the sample rate when in real-time mode.
struct Pacer {
    pacing: Pacing,
    sample_rate: SampleRate,
    start: Instant,
    emitted: u64,
}

impl Pacer {
    fn new(pacing: Pacing, sample_rate: SampleRate) -> Self {
        Pacer {
            pacing,
            sample_rate,
            start: Instant::now(),
            emitted: 0,
        }
    }

    fn wait_for(&mut self, n: u64) {
        self.emitted += n;
        if self.pacing == Pacing::Unthrottled {
            return;
        }
        let target = self.sample_rate.duration_for(self.emitted);
        let elapsed = self.start.elapsed();
        if let Some(rem) = target.checked_sub(elapsed) {
            thread::sleep(rem);
        }
    }
}

/// Streaming reader that yields `Complex<f32>` batches from disk.
enum SampleReader {
    Raw {
        reader: BufReader<File>,
        format: IqFormat,
    },
    #[cfg(feature = "iqfile")]
    Wav {
        reader: hound::WavReader<BufReader<File>>,
    },
}

impl SampleReader {
    fn open(path: &Path, format: IqFormat) -> Result<Self> {
        match format {
            IqFormat::Sc8 | IqFormat::Cf32 => {
                let file = File::open(path)?;
                Ok(SampleReader::Raw {
                    reader: BufReader::new(file),
                    format,
                })
            }
            IqFormat::WavFloat => {
                let reader = hound::WavReader::open(path)
                    .map_err(|e| Error::device(format!("wav open: {e}")))?;
                let spec = reader.spec();
                if spec.channels != 2 {
                    return Err(Error::unsupported(format!(
                        "wav IQ needs 2 channels, found {}",
                        spec.channels
                    )));
                }
                Ok(SampleReader::Wav { reader })
            }
        }
    }

    /// Append up to `max` complex samples to `out`; returns how many were read.
    fn next_batch(&mut self, max: usize, out: &mut Vec<IqSample>) -> Result<usize> {
        match self {
            SampleReader::Raw { reader, format } => match format {
                IqFormat::Sc8 => read_sc8(reader, max, out),
                IqFormat::Cf32 => read_cf32(reader, max, out),
                IqFormat::WavFloat => unreachable!("wav uses the Wav variant"),
            },
            #[cfg(feature = "iqfile")]
            SampleReader::Wav { reader } => {
                let mut count = 0usize;
                let mut samples = reader.samples::<f32>();
                while count < max {
                    match (samples.next(), samples.next()) {
                        (Some(Ok(i)), Some(Ok(q))) => {
                            out.push(Complex::new(i, q));
                            count += 1;
                        }
                        (None, _) => break,
                        (Some(Err(e)), _) | (_, Some(Err(e))) => {
                            return Err(Error::device(format!("wav read: {e}")));
                        }
                        (Some(Ok(_)), None) => break, // trailing half-sample
                    }
                }
                Ok(count)
            }
        }
    }
}

fn read_sc8(reader: &mut impl Read, max: usize, out: &mut Vec<IqSample>) -> Result<usize> {
    let mut buf = vec![0u8; max * 2];
    let filled = fill(reader, &mut buf)?;
    let pairs = filled / 2;
    for k in 0..pairs {
        let i = buf[2 * k] as i8 as f32 / 128.0;
        let q = buf[2 * k + 1] as i8 as f32 / 128.0;
        out.push(Complex::new(i, q));
    }
    Ok(pairs)
}

fn read_cf32(reader: &mut impl Read, max: usize, out: &mut Vec<IqSample>) -> Result<usize> {
    let mut buf = vec![0u8; max * 8];
    let filled = fill(reader, &mut buf)?;
    let pairs = filled / 8;
    for k in 0..pairs {
        let i = f32::from_le_bytes(buf[8 * k..8 * k + 4].try_into().unwrap_or([0; 4]));
        let q = f32::from_le_bytes(buf[8 * k + 4..8 * k + 8].try_into().unwrap_or([0; 4]));
        out.push(Complex::new(i, q));
    }
    Ok(pairs)
}

/// Read as many bytes as possible into `buf`, returning the count. Handles short
/// reads; stops at EOF.
fn fill(reader: &mut impl Read, buf: &mut [u8]) -> Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(Error::Io(e)),
        }
    }
    Ok(filled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustfft::num_complex::Complex as FftComplex;
    use rustfft::FftPlanner;
    use std::f32::consts::TAU;

    /// Write a complex tone at `freq` to a temp `.c8` file; return its path.
    fn write_tone(dir: &Path, sr: u32, freq: f32, n: usize) -> PathBuf {
        let path = dir.join(format!("tone_{freq}.c8"));
        let mut bytes = Vec::with_capacity(n * 2);
        for k in 0..n {
            let phase = TAU * freq * k as f32 / sr as f32;
            let i = (phase.cos() * 120.0).round().clamp(-128.0, 127.0) as i8;
            let q = (phase.sin() * 120.0).round().clamp(-128.0, 127.0) as i8;
            bytes.push(i as u8);
            bytes.push(q as u8);
        }
        std::fs::write(&path, bytes).expect("write tone");
        path
    }

    fn drain(mut stream: TunerStream) -> Vec<IqSample> {
        let mut samples = Vec::new();
        let mut spins = 0;
        loop {
            match stream.consumer.pop() {
                Ok(s) => samples.push(s),
                Err(_) => {
                    if stream.is_finished() {
                        // Final drain to catch anything pushed before finish.
                        while let Ok(s) = stream.consumer.pop() {
                            samples.push(s);
                        }
                        break;
                    }
                    spins += 1;
                    assert!(spins < 1_000_000, "reader never finished");
                    std::thread::yield_now();
                }
            }
        }
        samples
    }

    #[test]
    fn reads_back_sample_count_and_frequency() {
        let dir = std::env::temp_dir().join(format!("trunkd-iqtest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");

        let sr = 48_000u32;
        let n = 1 << 14; // 16384 samples
        let freq = sr as f32 / 8.0; // 6 kHz -> peak at bin n/8
        let path = write_tone(&dir, sr, freq, n);

        let cfg = TunerConfig::new(Hz::mhz(100), SampleRate(sr));
        let tuner = Box::new(
            IqFileTuner::open(&path, IqFormat::Sc8, cfg, Pacing::Unthrottled)
                .expect("open")
                .with_batch_size(1024),
        );
        let metrics = Metrics::new();
        let stream_cfg = StreamConfig {
            ring_capacity: n * 2, // room for everything: no drops
            event_capacity: 8,
        };
        let stream = tuner.start(stream_cfg, &metrics).expect("start");
        let samples = drain(stream);

        // Sample count matches what we wrote.
        assert_eq!(samples.len(), n, "sample count");

        // FFT: peak bin should sit at n/8.
        let mut buf: Vec<FftComplex<f32>> = samples
            .iter()
            .map(|s| FftComplex::new(s.re, s.im))
            .collect();
        let mut planner = FftPlanner::new();
        let fft = planner.plan_fft_forward(n);
        fft.process(&mut buf);
        let (peak_bin, _) = buf
            .iter()
            .enumerate()
            .map(|(i, c)| (i, c.norm_sqr()))
            .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap())
            .unwrap();
        let expected = n / 8;
        assert!(
            (peak_bin as i64 - expected as i64).abs() <= 1,
            "peak bin {peak_bin} not near expected {expected}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The whole point of an offline replay is that it decodes *everything*.
    /// A ring far smaller than the file would, without back-pressure, mean the
    /// reader streams at disk speed and the consumer sees a few per cent of the
    /// capture — which is how a fixture replay silently decodes nothing.
    #[test]
    fn unthrottled_playback_waits_for_the_consumer_instead_of_dropping() {
        let dir = std::env::temp_dir().join(format!("trunkd-iqbp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");

        let sr = 48_000u32;
        let n = 1 << 16;
        let path = write_tone(&dir, sr, sr as f32 / 8.0, n);

        let cfg = TunerConfig::new(Hz::mhz(100), SampleRate(sr));
        let tuner = IqFileTuner::open(&path, IqFormat::Sc8, cfg, Pacing::Unthrottled)
            .expect("open")
            .with_batch_size(1024);
        let metrics = Metrics::new();
        let overflow = metrics.counter(&crate::ring_metric_names(&tuner.descriptor().id).overflow);
        let stream = Box::new(tuner)
            .start(
                StreamConfig {
                    // A sixteenth of the file: without waiting, most of it
                    // would be dropped.
                    ring_capacity: n / 16,
                    event_capacity: 8,
                },
                &metrics,
            )
            .expect("start");
        let samples = drain(stream);

        assert_eq!(
            samples.len(),
            n,
            "every sample in the file reached the consumer"
        );
        assert_eq!(overflow.get(), 0, "nothing was dropped");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Two unthrottled readers of one file in one [`Lockstep`] group, one
    /// consumed flat out and one slowly: the fast consumer is never let more
    /// than a window (plus the batch in flight) ahead of the slow one, and both
    /// still receive the whole file — the group costs time, never samples.
    #[test]
    fn a_lockstep_group_keeps_fast_and_slow_consumers_together() {
        use std::sync::atomic::AtomicU64;
        let dir = std::env::temp_dir().join(format!("trunkd-iqlock-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");

        let sr = 48_000u32;
        let n = 1 << 16;
        let batch = 1024;
        let path = write_tone(&dir, sr, sr as f32 / 8.0, n);
        let group = Lockstep::new();
        let window = lockstep_window(SampleRate(sr), batch);
        let metrics = Metrics::new();
        let start = |g: &Arc<Lockstep>, name: &str| {
            let cfg = TunerConfig::new(Hz::mhz(100), SampleRate(sr));
            Box::new(
                IqFileTuner::open(&path, IqFormat::Sc8, cfg, Pacing::Unthrottled)
                    .expect("open")
                    .with_batch_size(batch)
                    .with_lockstep(g.clone(), name),
            )
            .start(
                StreamConfig {
                    ring_capacity: n * 2, // room for the whole file: only the group holds it back
                    event_capacity: 8,
                },
                &metrics,
            )
            .expect("start")
        };
        // Both reserved up front, as the recorder does, so the first reader
        // waits for the second instead of running ahead while it starts.
        group.expect("fast");
        group.expect("slow");
        let fast = start(&group, "fast");
        let slow = start(&group, "slow");

        // The slow consumer publishes what it has popped, in chunks, so what
        // the fast side reads is never ahead of the ring's true position.
        let slow_seen = Arc::new(AtomicU64::new(0));
        let seen = slow_seen.clone();
        let slow_thread = std::thread::spawn(move || {
            let mut stream = slow;
            let mut total = 0u64;
            loop {
                let mut chunk = 0;
                while chunk < 256 && stream.consumer.pop().is_ok() {
                    chunk += 1;
                }
                total += chunk;
                seen.store(total, Ordering::Release);
                if chunk == 0 {
                    if stream.is_finished() && stream.consumer.is_empty() {
                        return total;
                    }
                    std::thread::yield_now();
                } else {
                    std::thread::sleep(std::time::Duration::from_micros(50));
                }
            }
        });

        let mut stream = fast;
        let mut fast_total = 0u64;
        let mut worst_lead = 0u64;
        let mut spins = 0u64;
        loop {
            match stream.consumer.pop() {
                Ok(_) => {
                    fast_total += 1;
                    let lead = fast_total.saturating_sub(slow_seen.load(Ordering::Acquire));
                    worst_lead = worst_lead.max(lead);
                }
                Err(_) if stream.is_finished() && stream.consumer.is_empty() => break,
                Err(_) => {
                    spins += 1;
                    assert!(spins < 100_000_000, "the fast reader never finished");
                    std::thread::yield_now();
                }
            }
        }
        let slow_total = slow_thread.join().expect("slow consumer");

        assert_eq!(
            (fast_total, slow_total),
            (n as u64, n as u64),
            "both got the whole file"
        );
        // A window, the batch the fast reader had already been let through
        // with, and the slow consumer's publishing chunk.
        let bound = window + batch as u64 + 256;
        assert!(
            worst_lead <= bound,
            "the fast consumer led by {worst_lead} samples, more than {bound}"
        );
        assert!(
            group.lock().members.is_empty(),
            "finished readers leave the group"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Start an unthrottled reader of `path` in `group` under `name`, with a
    /// ring that holds the whole file: nothing but the group holds it back,
    /// and as nothing is consumed, what is in its ring is what the group let
    /// through.
    fn start_member(
        path: &Path,
        group: &Arc<Lockstep>,
        name: &str,
        metrics: &Metrics,
        n: usize,
    ) -> TunerStream {
        let cfg = TunerConfig::new(Hz::mhz(100), SampleRate(48_000));
        Box::new(
            IqFileTuner::open(path, IqFormat::Sc8, cfg, Pacing::Unthrottled)
                .expect("open")
                .with_batch_size(1024)
                .with_lockstep(group.clone(), name),
        )
        .start(
            StreamConfig {
                ring_capacity: n * 2, // room for the whole file: only the group holds it back
                event_capacity: 8,
            },
            metrics,
        )
        .expect("start")
    }

    /// Wait (bounded) until `stream` holds `n` samples; returns what it holds.
    fn wait_for_slots(stream: &TunerStream, n: usize, within: std::time::Duration) -> usize {
        let t0 = Instant::now();
        while stream.consumer.slots() < n && t0.elapsed() < within {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        stream.consumer.slots()
    }

    /// The replay-lockstep defect this pins: each device thread joins at its own start, so
    /// a member started while its peer was still being set up found the first
    /// reader had already run free through the file. With both reserved up
    /// front, the early reader writes one window and waits for its peer.
    #[test]
    fn a_member_that_starts_late_is_not_left_behind() {
        let dir = std::env::temp_dir().join(format!("trunkd-iqlate-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");

        let sr = 48_000u32;
        let n = 1 << 16;
        let batch = 1024;
        let path = write_tone(&dir, sr, sr as f32 / 8.0, n);
        let group = Lockstep::new();
        group.expect("early");
        group.expect("late");
        let window = lockstep_window(SampleRate(sr), batch);
        let metrics = Metrics::new();
        let early = start_member(&path, &group, "early", &metrics, n);
        std::thread::sleep(std::time::Duration::from_millis(50));
        let before_peer = early.consumer.slots() as u64;
        let late = start_member(&path, &group, "late", &metrics, n);
        assert!(group.pending().is_empty(), "both reservations taken");

        std::thread::sleep(std::time::Duration::from_millis(50));
        let ahead = early.consumer.slots() as u64;
        let late_ahead = late.consumer.slots() as u64;
        drop((early, late));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            before_peer <= window,
            "the early reader wrote {before_peer} samples before its peer had joined"
        );
        assert!(
            ahead <= window && late_ahead <= window,
            "readers wrote {ahead} and {late_ahead} samples before either peer consumed any"
        );
    }

    /// A reserved member that never joins (and is never withdrawn) holds the
    /// others for the join deadline only: then it is dropped and the rest
    /// replay the whole file.
    #[test]
    fn a_reserved_member_that_never_starts_is_dropped_at_the_deadline() {
        let dir = std::env::temp_dir().join(format!("trunkd-iqdeadline-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");

        let sr = 48_000u32;
        let n = 1 << 16;
        let path = write_tone(&dir, sr, sr as f32 / 8.0, n);
        let group = Lockstep::new();
        let deadline = std::time::Duration::from_millis(400);
        group.set_join_deadline(deadline);
        group.expect("present");
        group.expect("absent");
        let window = lockstep_window(SampleRate(sr), 1024);
        let metrics = Metrics::new();
        let t0 = Instant::now();
        let present = start_member(&path, &group, "present", &metrics, n);

        // Well inside the deadline it is still held at one window.
        std::thread::sleep(std::time::Duration::from_millis(50));
        let held = present.consumer.slots() as u64;
        let held_at = t0.elapsed();
        assert_eq!(group.pending(), vec!["absent".to_owned()]);

        // After it, the absent member is forgotten and the file flows.
        let got = wait_for_slots(&present, n, std::time::Duration::from_secs(20));
        let done_at = t0.elapsed();
        drop(present);
        let _ = std::fs::remove_dir_all(&dir);
        if held_at < deadline {
            assert!(held <= window, "wrote {held} samples inside the deadline");
        }
        assert_eq!(got, n, "the whole file once the deadline passed");
        assert!(
            done_at >= deadline,
            "released after {done_at:?}, before the {deadline:?} deadline"
        );
        assert!(group.pending().is_empty(), "the absent member was dropped");
    }

    /// A member whose file cannot be opened withdraws its reservation, and the
    /// member already running goes on at once rather than at the deadline.
    #[test]
    fn a_member_that_fails_to_open_releases_the_others() {
        let dir = std::env::temp_dir().join(format!("trunkd-iqfail-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");

        let sr = 48_000u32;
        let n = 1 << 16;
        let path = write_tone(&dir, sr, sr as f32 / 8.0, n);
        let group = Lockstep::new();
        // Long enough that only the withdrawal can explain a prompt finish.
        group.set_join_deadline(std::time::Duration::from_secs(60));
        group.expect("good");
        group.expect("broken");
        let metrics = Metrics::new();
        let good = start_member(&path, &group, "good", &metrics, n);

        let cfg = TunerConfig::new(Hz::mhz(100), SampleRate(sr));
        let opened = IqFileTuner::open(
            dir.join("missing.c8"),
            IqFormat::Sc8,
            cfg,
            Pacing::Unthrottled,
        );
        assert!(opened.is_err(), "the broken member's file is missing");
        group.withdraw("broken");

        let got = wait_for_slots(&good, n, std::time::Duration::from_secs(20));
        drop(good);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(got, n, "the good member replayed the whole file");
        assert!(group.pending().is_empty());
    }

    /// A member that opens but whose reader fails inside its thread (here a
    /// file that is not WAV) has already joined; its thread ending is what
    /// takes it out of the group, so it does not hold the others either.
    #[test]
    fn a_member_whose_reader_fails_leaves_the_group() {
        let dir = std::env::temp_dir().join(format!("trunkd-iqbadwav-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");

        let sr = 48_000u32;
        let n = 1 << 16;
        let path = write_tone(&dir, sr, sr as f32 / 8.0, n);
        let not_wav = dir.join("not.wav");
        std::fs::write(&not_wav, b"definitely not RIFF").expect("write");
        let group = Lockstep::new();
        group.set_join_deadline(std::time::Duration::from_secs(60));
        group.expect("good");
        group.expect("bad");
        let metrics = Metrics::new();
        let good = start_member(&path, &group, "good", &metrics, n);
        let cfg = TunerConfig::new(Hz::mhz(100), SampleRate(sr));
        let bad = Box::new(
            IqFileTuner::open(&not_wav, IqFormat::WavFloat, cfg, Pacing::Unthrottled)
                .expect("the file exists, so open succeeds")
                .with_lockstep(group.clone(), "bad"),
        )
        .start(
            StreamConfig {
                ring_capacity: 1024,
                event_capacity: 8,
            },
            &metrics,
        )
        .expect("start");

        let got = wait_for_slots(&good, n, std::time::Duration::from_secs(20));
        drop((good, bad));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(got, n, "the good member replayed the whole file");
    }

    /// A reader reserved under a name but started with real-time pacing does
    /// not take part, so it gives its reservation back at start.
    #[test]
    fn a_realtime_reader_withdraws_its_reservation() {
        let dir = std::env::temp_dir().join(format!("trunkd-iqrt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = write_tone(&dir, 48_000, 6_000.0, 4096);
        let group = Lockstep::new();
        group.expect("rt");
        let cfg = TunerConfig::new(Hz::mhz(100), SampleRate(48_000));
        let stream = Box::new(
            IqFileTuner::open(&path, IqFormat::Sc8, cfg, Pacing::RealTime)
                .expect("open")
                .with_lockstep(group.clone(), "rt"),
        )
        .start(
            StreamConfig {
                ring_capacity: 8192,
                event_capacity: 8,
            },
            &Metrics::new(),
        )
        .expect("start");
        assert!(group.pending().is_empty());
        assert!(
            group.lock().members.is_empty(),
            "real-time readers never join"
        );
        drop(stream);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_file_errors() {
        let cfg = TunerConfig::new(Hz::mhz(100), SampleRate(48_000));
        let r = IqFileTuner::open("/no/such/file.c8", IqFormat::Sc8, cfg, Pacing::Unthrottled);
        assert!(r.is_err());
    }
}
