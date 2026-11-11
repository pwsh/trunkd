// SPDX-License-Identifier: GPL-3.0-or-later
//! DSP cost of one device serving N sites: CPU time per 64 Ki-sample block.
//!
//! Replays a wideband int8 capture through the recorder's DSP loop shape — a
//! block of `BLOCK_SAMPLES` (the device loop's 64 Ki) at a time — and times
//! only the signal processing, not the file read or the int8 conversion.
//!
//! `--sites 1` is the reference site alone; `--sites 4` adds the three
//! neighbour sites that shared `hackrf0` on 2026-09-16 (control-only,
//! pool 0) — the configuration whose ring
//! overflowed.
//!
//! `--mode shared` (the default) is the recorder since T-C1: one
//! [`ChannelBank`](trunkd_p25::dsp::ChannelBank) for the device, run once per block, then every engine reads
//! its channels. `--mode private` is the shape before it: every engine builds
//! and runs a filterbank of its own over the same block.
//!
//! ```text
//! nice -n 19 cargo run --release -p trunkd-p25 --example site_bench -- \
//!     fixtures/<system>_wb855500k_20M_int8_run2.iq --sites 4 [--mode shared|private] [--seconds 60]
//! ```
//!
//! CPU time is the thread's own (`/proc/thread-self/schedstat`), so a busy
//! neighbour process does not inflate it the way it would a wall clock.

use std::fs::File;
use std::io::{BufReader, Read};
use std::time::Instant;

use num_complex::Complex;
use trunkd_p25::site::{SiteConfig, SiteEngine, SiteEvent};

/// The device loop's block (`trunkd::pipeline::device::BLOCK_SAMPLES`).
const BLOCK_SAMPLES: usize = 64 * 1024;
const RATE: u32 = 20_000_000;
const CENTER_HZ: f64 = 855_500_000.0;

/// `config/trunkd.toml` on 2026-09-16: the reference site plus its three neighbours.
fn sites(n: usize) -> Vec<SiteConfig> {
    let reference = vec![
        851_050_000,
        851_787_500,
        852_137_500,
        852_537_500,
        852_825_000,
        853_112_500,
        854_237_500,
        854_437_500,
        856_712_500,
        858_987_500,
        859_987_500,
    ];
    let all: [(Vec<u64>, usize); 4] = [
        (reference, 20),
        (vec![852_062_500], 0),
        (vec![851_825_000, 851_262_500, 851_225_000], 0),
        (vec![851_550_000], 0),
    ];
    all.into_iter()
        .take(n)
        .map(|(control_frequencies, traffic_pool_size)| SiteConfig {
            sample_rate: RATE,
            center_hz: CENTER_HZ,
            control_frequencies,
            traffic_pool_size,
            // As the recorder runs them (`device.rs`): the raw control stream
            // is always passed through for the observations fold.
            raw_control: true,
            ..SiteConfig::default()
        })
        .collect()
}

/// What the engines emitted: counts, plus a per-site FNV-1a digest of every
/// event's `Debug` form — equal digests mean bit-identical event streams
/// (the stats' `f64`s print their exact shortest round-trip value).
struct Tally {
    events: u64,
    voice: u64,
    digests: Vec<u64>,
}

impl Tally {
    fn observe(&mut self, site: usize, ev: &SiteEvent) {
        self.events += 1;
        if matches!(ev, SiteEvent::CallVoice { .. }) {
            self.voice += 1;
        }
        let h = &mut self.digests[site];
        for byte in format!("{ev:?}").bytes() {
            *h ^= u64::from(byte);
            *h = h.wrapping_mul(0x1000_0000_01b3);
        }
    }
}

/// This thread's CPU time in ns (Linux: first field of schedstat).
fn thread_cpu_ns() -> u64 {
    std::fs::read_to_string("/proc/thread-self/schedstat")
        .ok()
        .and_then(|s| s.split_whitespace().next()?.parse().ok())
        .unwrap_or(0)
}

fn main() {
    let argv: Vec<String> = std::env::args().collect();
    let path = argv
        .get(1)
        .expect("usage: site_bench <file.iq> [--sites N]");
    let mut n_sites = 1usize;
    let mut seconds = f64::INFINITY;
    let mut shared = true;
    let mut i = 2;
    while i < argv.len() {
        match argv[i].as_str() {
            "--sites" => {
                n_sites = argv[i + 1].parse().expect("--sites N");
                i += 1;
            }
            "--mode" => {
                shared = match argv[i + 1].as_str() {
                    "shared" => true,
                    "private" => false,
                    m => panic!("--mode shared|private, not {m}"),
                };
                i += 1;
            }
            "--seconds" => {
                seconds = argv[i + 1].parse().expect("--seconds S");
                i += 1;
            }
            other => panic!("unknown argument {other}"),
        }
        i += 1;
    }
    let limit = (seconds * f64::from(RATE)) as u64;

    let configs = sites(n_sites);
    let bank = shared.then(|| SiteEngine::bank_for(&configs[0]).expect("bank builds"));
    let mut engines: Vec<SiteEngine> = configs
        .into_iter()
        .map(|c| match &bank {
            Some(b) => SiteEngine::with_bank(c, b.clone()),
            None => SiteEngine::new(c),
        })
        .map(|e| e.expect("engine builds"))
        .collect();
    // CPU spent in the filterbank alone (shared mode only: private mode runs
    // it inside each engine's `process_block`).
    let mut bank_ns = 0u64;
    let mut bank_wall = std::time::Duration::ZERO;

    let mut reader = BufReader::with_capacity(1 << 22, File::open(path).expect("fixture opens"));
    let mut raw = vec![0u8; 2 * BLOCK_SAMPLES];
    let mut wb: Vec<Complex<f32>> = Vec::with_capacity(BLOCK_SAMPLES);
    let mut per_block_ns: Vec<u64> = Vec::new();
    let mut tally = Tally {
        events: 0,
        voice: 0,
        digests: vec![0xcbf2_9ce4_8422_2325; engines.len()],
    };
    let mut samples = 0u64;
    let started = Instant::now();
    let mut dsp_wall = std::time::Duration::ZERO;
    while samples < limit {
        if reader.read_exact(&mut raw).is_err() {
            break;
        }
        wb.clear();
        wb.extend(
            raw.as_chunks::<2>().0.iter().map(|p| {
                Complex::new(f32::from(p[0] as i8) / 128.0, f32::from(p[1] as i8) / 128.0)
            }),
        );
        let (t0, w0) = (thread_cpu_ns(), Instant::now());
        if let Some(bank) = &bank {
            bank.borrow_mut().process_block(&wb);
            bank_ns += thread_cpu_ns().saturating_sub(t0);
            bank_wall += w0.elapsed();
            for (k, engine) in engines.iter_mut().enumerate() {
                engine.process_channels(&mut |ev| tally.observe(k, &ev));
            }
        } else {
            for (k, engine) in engines.iter_mut().enumerate() {
                engine.process_block(&wb, &mut |ev| tally.observe(k, &ev));
            }
        }
        dsp_wall += w0.elapsed();
        per_block_ns.push(thread_cpu_ns().saturating_sub(t0));
        samples += wb.len() as u64;
    }
    let mode = if shared {
        "one shared channelizer"
    } else {
        "per-site channelizers"
    };
    let blocks = per_block_ns.len().max(1);
    report(n_sites, mode, &mut per_block_ns, samples, dsp_wall);
    if let Some(bank) = &bank {
        let b = bank.borrow();
        println!(
            "  filterbank alone: cpu {:.3} ms/block (wall {:.3}); channels produced at the end: \
             {} for {} subscriptions",
            bank_ns as f64 / 1e6 / blocks as f64,
            bank_wall.as_secs_f64() * 1e3 / blocks as f64,
            b.channel_count(),
            b.subscription_count(),
        );
    }
    for (k, engine) in engines.iter_mut().enumerate() {
        engine.shutdown(&mut |ev| tally.observe(k, &ev));
    }
    let digests: Vec<String> = tally.digests.iter().map(|d| format!("{d:016x}")).collect();
    println!(
        "  events={} voice_frames={} per-site event digests=[{}] total_wall={:.1}s",
        tally.events,
        tally.voice,
        digests.join(","),
        started.elapsed().as_secs_f64()
    );
}

fn report(
    n_sites: usize,
    mode: &str,
    per_block_ns: &mut [u64],
    samples: u64,
    dsp_wall: std::time::Duration,
) {
    let blocks = per_block_ns.len().max(1);
    let total_ns: u64 = per_block_ns.iter().sum();
    per_block_ns.sort_unstable();
    let pct = |p: f64| per_block_ns[((blocks - 1) as f64 * p) as usize] as f64 / 1e6;
    let audio_s = samples as f64 / f64::from(RATE);
    let block_ms = BLOCK_SAMPLES as f64 * 1000.0 / f64::from(RATE);
    let mean_ms = total_ns as f64 / 1e6 / blocks as f64;
    println!(
        "sites={n_sites} mode=\"{mode}\" blocks={blocks} audio={audio_s:.1}s \
         cpu_ms_per_block mean={mean_ms:.3} p50={:.3} p99={:.3} \
         (block is {block_ms:.3} ms of air) real_time_ratio={:.3} wall_ratio={:.3}",
        pct(0.5),
        pct(0.99),
        total_ns as f64 / 1e9 / audio_s,
        dsp_wall.as_secs_f64() / audio_s,
    );
}
