// SPDX-License-Identifier: GPL-3.0-or-later
//! Live HackRF smoke test: stream for N seconds and report signal statistics.
//!
//! ```text
//! cargo run --release --example hackrf_probe -- \
//!     --seconds 10 --center 855500000 --rate 20000000 --lna 40 --vga 40 --amp
//! ```
//!
//! Reports produced/overflow sample counts, sustained throughput, RMS and peak
//! |I|/|Q| (the numbers that say whether the gain is sane: RMS ≈ 0.2 with peaks
//! well under 1.0 is a healthy, unclipped front end), and optionally a coarse
//! spectrum. Zero overflow while the drain loop keeps up is the pass condition
//! for the ring plumbing.
//!
//! `--portapack <tty>` switches a PortaPack out of its UI first;
//! `--restore-portapack` resets the board back into the UI on the way out.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use rustfft::num_complex::Complex as FftComplex;
use rustfft::FftPlanner;
use trunkd_core::{Hz, IqSample, Metrics, SampleRate};
use trunkd_sdr::hackrf::{HackRfConfig, HackRfTuner};
use trunkd_sdr::{StreamConfig, TunerDevice, TunerEvent};

/// Samples captured for the optional spectrum estimate.
const SPECTRUM_N: usize = 4096;
/// Number of FFT frames averaged for the spectrum estimate.
const SPECTRUM_FRAMES: usize = 16;

struct Args {
    seconds: f64,
    center: u64,
    rate: u32,
    lna: u32,
    vga: u32,
    amp: bool,
    serial: Option<String>,
    transfer_bin: PathBuf,
    portapack: Option<PathBuf>,
    restore: bool,
    spectrum: bool,
}

impl Default for Args {
    fn default() -> Self {
        Args {
            seconds: 10.0,
            center: 855_500_000,
            rate: 20_000_000,
            lna: 40,
            vga: 40,
            amp: false,
            serial: None,
            transfer_bin: PathBuf::from("hackrf_transfer"),
            portapack: None,
            restore: false,
            spectrum: false,
        }
    }
}

fn usage() -> ! {
    eprintln!(
        "usage: hackrf_probe [--seconds N] [--center HZ] [--rate SPS] [--lna DB] [--vga DB]\n\
         \x20                   [--amp] [--serial S] [--bin PATH] [--portapack TTY]\n\
         \x20                   [--restore-portapack] [--spectrum]"
    );
    std::process::exit(2);
}

fn parse_args() -> Args {
    let mut a = Args::default();
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut value = || it.next().unwrap_or_else(|| usage());
        match flag.as_str() {
            "--seconds" => a.seconds = value().parse().unwrap_or_else(|_| usage()),
            "--center" => a.center = value().parse().unwrap_or_else(|_| usage()),
            "--rate" => a.rate = value().parse().unwrap_or_else(|_| usage()),
            "--lna" => a.lna = value().parse().unwrap_or_else(|_| usage()),
            "--vga" => a.vga = value().parse().unwrap_or_else(|_| usage()),
            "--amp" => a.amp = true,
            "--serial" => a.serial = Some(value()),
            "--bin" => a.transfer_bin = PathBuf::from(value()),
            "--portapack" => a.portapack = Some(PathBuf::from(value())),
            "--restore-portapack" => a.restore = true,
            "--spectrum" => a.spectrum = true,
            "-h" | "--help" => usage(),
            _ => usage(),
        }
    }
    a
}

/// Running signal statistics over the drained stream.
#[derive(Default)]
struct Stats {
    n: u64,
    sum_sq: f64,
    peak_i: f32,
    peak_q: f32,
    clipped: u64,
}

impl Stats {
    fn add(&mut self, batch: &[IqSample]) {
        for s in batch {
            let (i, q) = (s.re, s.im);
            self.sum_sq += (i * i + q * q) as f64;
            self.peak_i = self.peak_i.max(i.abs());
            self.peak_q = self.peak_q.max(q.abs());
            // int8 saturation: |x| >= 127/128.
            if i.abs() >= 0.9921875 || q.abs() >= 0.9921875 {
                self.clipped += 1;
            }
        }
        self.n += batch.len() as u64;
    }

    /// RMS of the complex magnitude — comparable to `int8 rms / 128`.
    fn rms(&self) -> f64 {
        if self.n == 0 {
            0.0
        } else {
            (self.sum_sq / self.n as f64).sqrt()
        }
    }
}

fn main() {
    tracing_init();
    let args = parse_args();

    if let Some(port) = &args.portapack {
        println!("switching {} into HackRF mode...", port.display());
        if let Err(e) = trunkd_sdr::portapack::ensure_hackrf_mode(port, Duration::from_secs(10)) {
            eprintln!("portapack: {e}");
            std::process::exit(1);
        }
    }

    for d in HackRfTuner::enumerate() {
        println!("found: {} [{}] {}", d.label, d.driver, d.args);
    }

    let hw = HackRfConfig {
        serial: args.serial.clone(),
        center_freq: Hz::new(args.center),
        sample_rate: SampleRate(args.rate),
        lna_db: args.lna,
        vga_db: args.vga,
        amp: args.amp,
        ppm: 0.0,
        transfer_bin: args.transfer_bin.clone(),
    };
    let tuner = match HackRfTuner::open(hw) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("open: {e}");
            std::process::exit(1);
        }
    };
    println!(
        "streaming {:.1} s @ {} Msps, center {}, LNA {} dB / VGA {} dB, amp {}",
        args.seconds,
        args.rate as f64 / 1e6,
        Hz::new(args.center),
        args.lna,
        args.vga,
        if args.amp { "on" } else { "off" }
    );

    let metrics = Metrics::new();
    // ~0.2 s of headroom at 20 Msps.
    let stream_cfg = StreamConfig {
        ring_capacity: 1 << 22,
        event_capacity: 64,
    };
    let mut stream = match Box::new(tuner).start(stream_cfg, &metrics) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("start: {e}");
            std::process::exit(1);
        }
    };

    let mut stats = Stats::default();
    let mut snapshot: Vec<IqSample> = Vec::with_capacity(SPECTRUM_N * SPECTRUM_FRAMES);
    let deadline = Instant::now() + Duration::from_secs_f64(args.seconds);
    let start = Instant::now();
    let mut first_sample_at: Option<Instant> = None;
    let mut fatal: Option<String> = None;

    while Instant::now() < deadline {
        let ready = stream.consumer.slots();
        if ready == 0 {
            if stream.is_finished() {
                break;
            }
            std::thread::yield_now();
            continue;
        }
        if let Ok(chunk) = stream.consumer.read_chunk(ready) {
            let (a, b) = chunk.as_slices();
            for part in [a, b] {
                if part.is_empty() {
                    continue;
                }
                if first_sample_at.is_none() {
                    first_sample_at = Some(Instant::now());
                }
                stats.add(part);
                if args.spectrum && snapshot.len() < SPECTRUM_N * SPECTRUM_FRAMES {
                    let want = (SPECTRUM_N * SPECTRUM_FRAMES - snapshot.len()).min(part.len());
                    snapshot.extend_from_slice(&part[..want]);
                }
            }
            chunk.commit_all();
        }
        while let Ok(ev) = stream.events.try_recv() {
            match ev {
                TunerEvent::Error(msg) => fatal = Some(msg),
                other => println!("event: {other:?}"),
            }
        }
    }

    let elapsed = first_sample_at
        .map(|t0| t0.elapsed())
        .unwrap_or_else(|| start.elapsed());
    let produced = stream.produced_count();
    let overflow = stream.overflow_count();
    let throughput = metrics
        .gauge(&format!(
            "sdr.hackrf:{}.hackrf_mib_per_sec",
            args.serial.as_deref().unwrap_or("default")
        ))
        .get_f64();
    let restarts = metrics
        .counter(&format!(
            "sdr.hackrf:{}.hackrf_restarts",
            args.serial.as_deref().unwrap_or("default")
        ))
        .get();
    stream.stop();

    println!("--- results ---");
    println!("drained          {} samples", stats.n);
    println!("produced         {produced} samples");
    println!("overflow         {overflow} samples");
    println!("child restarts   {restarts}");
    println!(
        "sample rate      {:.3} Msps measured ({:.3} Msps requested)",
        stats.n as f64 / elapsed.as_secs_f64() / 1e6,
        args.rate as f64 / 1e6
    );
    println!(
        "throughput       {:.1} MiB/s reported by hackrf_transfer, {:.1} MiB/s measured",
        throughput,
        stats.n as f64 * 2.0 / elapsed.as_secs_f64() / (1024.0 * 1024.0)
    );
    println!(
        "rms              {:.4}  (int8 rms {:.1})",
        stats.rms(),
        stats.rms() * 128.0
    );
    println!(
        "peak |I|,|Q|     {:.4}, {:.4}  (int8 {:.0}, {:.0})",
        stats.peak_i,
        stats.peak_q,
        stats.peak_i * 128.0,
        stats.peak_q * 128.0
    );
    println!(
        "clipped samples  {} ({:.4} %)",
        stats.clipped,
        100.0 * stats.clipped as f64 / stats.n.max(1) as f64
    );

    if args.spectrum && snapshot.len() >= SPECTRUM_N {
        print_spectrum(&snapshot, args.center, args.rate);
    }

    if let Some(msg) = fatal {
        eprintln!("tuner error: {msg}");
    }

    if args.restore {
        println!("resetting the board back into the PortaPack UI...");
        match trunkd_sdr::portapack::restore_portapack() {
            Ok(()) => println!("restore: ok"),
            Err(e) => {
                eprintln!("restore: {e}");
                std::process::exit(1);
            }
        }
    }
}

/// Averaged periodogram of the snapshot; prints the strongest offsets.
fn print_spectrum(snapshot: &[IqSample], center: u64, rate: u32) {
    let mut planner = FftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(SPECTRUM_N);
    // Hann window keeps a strong carrier from smearing across the whole band.
    let window: Vec<f32> = (0..SPECTRUM_N)
        .map(|k| 0.5 * (1.0 - (std::f32::consts::TAU * k as f32 / SPECTRUM_N as f32).cos()))
        .collect();
    let mut acc = vec![0.0f64; SPECTRUM_N];
    let frames = snapshot.len() / SPECTRUM_N;
    let mut buf = vec![FftComplex::new(0.0f32, 0.0); SPECTRUM_N];
    for f in 0..frames {
        for (k, slot) in buf.iter_mut().enumerate() {
            let s = snapshot[f * SPECTRUM_N + k];
            *slot = FftComplex::new(s.re * window[k], s.im * window[k]);
        }
        fft.process(&mut buf);
        for (a, c) in acc.iter_mut().zip(buf.iter()) {
            *a += c.norm_sqr() as f64;
        }
    }
    let scale = (frames * SPECTRUM_N) as f64;
    let mut bins: Vec<(usize, f64)> = acc
        .iter()
        .enumerate()
        .map(|(i, p)| (i, 10.0 * (p / scale + 1e-30).log10()))
        .collect();
    bins.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    println!("--- spectrum ({frames} x {SPECTRUM_N}-pt averaged) ---");
    for (bin, db) in bins.iter().take(5) {
        let signed = if *bin < SPECTRUM_N / 2 {
            *bin as i64
        } else {
            *bin as i64 - SPECTRUM_N as i64
        };
        let offset_hz = signed as f64 * rate as f64 / SPECTRUM_N as f64;
        println!(
            "  {:>+10.1} kHz  ({:.6} MHz)  {:>7.1} dB",
            offset_hz / 1e3,
            (center as f64 + offset_hz) / 1e6,
            db
        );
    }
}

/// Stderr logging so the backend's `debug` lines (including `hackrf_transfer`'s
/// own throughput chatter) are visible with `RUST_LOG=debug`.
fn tracing_init() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}
