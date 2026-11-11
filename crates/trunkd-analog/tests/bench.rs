// SPDX-License-Identifier: GPL-3.0-or-later
//! Device-level cost of conventional channels: a synthetic 10 Msps capture
//! replayed in 64 Ki-sample blocks (the recorder's DSP block size) through one
//! shared [`ChannelBank`], (a) with nothing subscribed to it but the bank
//! running, and (b) with 11 [`NbfmChannel`] engines subscribed and fed, as the
//! device DSP thread does it (`process_block`, then per channel `drain` +
//! `process`). The target is that (b) costs at most 10 % more than (a).
//!
//! Run (release, single-threaded, nothing else heavy on the machine):
//!
//! ```text
//! cargo test --release -p trunkd-analog --test bench -- --ignored --nocapture
//! ```
//!
//! Measured 2026-11-11 on an Intel Core i7-9700 @ 3.00 GHz (8 cores, 6 usable
//! by the build, load average ~2.8 from other builds), Linux x86_64, release
//! profile, best of 3 alternating rounds over 20 s of capture each:
//!
//! ```text
//! bank only           : 0.1426 s DSP per 1 s of capture (14.3 % of one core)
//! bank + 11 channels  : 0.1844 s DSP per 1 s of capture (18.4 % of one core)
//! channels (in run b) : 0.0374 s DSP per 1 s of capture — 26.2 % of the bank
//! overhead (b - a) / a: 29.3 %
//! per channel         : 0.34 % of one core
//! ```
//!
//! ("channels" times only the drain + engine calls inside run (b); the rest
//! of (b - a) is the bank synthesising the 11 subscribed outputs.) Repeat
//! runs under load average 7–12 gave 38 % overhead with the same ratio of
//! channel to bank time.
//!
//! **A 10 % relative target is not met.** The per-channel cost is as
//! estimated (~3.4 ms of CPU per channel per second); the filterbank
//! measures far cheaper than its 2 GFLOP/s estimate, so
//! the same channel cost is a larger fraction of it. In
//! absolute terms 11 busy channels take ~4 % of one core. The test prints the
//! relative figure and asserts only a regression guard (one channel ≤ 1 % of
//! one core) plus decoding correctness.
//!
//! The 11 channels are a mix of the squelch kinds: 4 CTCSS-gated, 4
//! DCS-gated (normal and inverted codes) and 3 carrier-squelch channels (one
//! of them 25 kHz wide). Their centres sit both on and between filterbank
//! bins, so the single-bin and the two-bin synthesis paths are both in the
//! measurement. Every channel is keyed for 2.2 s of each 4 s loop (one kept
//! call per loop). The engine runs its whole chain on every sample, keyed or
//! not, so an idle channel costs about the same. All frequencies are
//! fictional.

// `NbfmSignal::keyed` is a list of ranges; one range is a normal value.
#![allow(clippy::single_range_in_vec_init)]

use std::time::{Duration, Instant};

use num_complex::Complex;
use trunkd_analog::testsig::{self, Audio, Deviation, NbfmSignal};
use trunkd_analog::{Bandwidth, ChannelParams, NbfmChannel, Tone};
use trunkd_p25::dsp::channelizer::DEFAULT_TAPS_PER_CHANNEL;
use trunkd_p25::dsp::{BankHandle, ChannelBank};

/// Capture rate of the simulated device.
const RATE: u32 = 10_000_000;
/// Capture centre (fictional).
const CENTER_HZ: f64 = 460_000_000.0;
/// The recorder's DSP block (`pipeline::device::BLOCK_SAMPLES`).
const BLOCK: usize = 64 * 1024;
/// Channel rate out of the bank.
const FS: usize = 25_000;
/// Length of the synthetic loop, seconds. Each channel is keyed for 2.2 s of
/// it (one call) and idle for the rest (longer than the 1 s squelch hang).
const LOOP_S: usize = 4;
/// How many times the loop is replayed per timed pass.
const REPLAYS: usize = 5;
/// Alternating (a)/(b) rounds; the best of each is reported.
const ROUNDS: usize = 3;

struct Ch {
    offset_hz: f64,
    bandwidth: Bandwidth,
    /// Configured tone (`None` = carrier squelch).
    tone: Option<&'static str>,
    /// Tone actually transmitted.
    sent: Option<&'static str>,
}

const CHANNELS: [Ch; 11] = [
    ch(-4_806_250.0, Some("67.0"), Some("67.0")),
    ch(-3_900_000.0, Some("D023N"), Some("D023N")),
    ch(-2_506_250.0, None, None),
    ch(-1_250_000.0, Some("100.0"), Some("100.0")),
    ch(-606_250.0, Some("D445N"), Some("D445N")),
    ch(418_750.0, None, Some("123.0")),
    ch(1_312_500.0, Some("167.9"), Some("167.9")),
    ch(2_206_250.0, Some("D754I"), Some("D754I")),
    Ch {
        offset_hz: 3_100_000.0,
        bandwidth: Bandwidth::Wide,
        tone: None,
        sent: None,
    },
    ch(3_993_750.0, Some("250.3"), Some("250.3")),
    ch(4_731_250.0, Some("D131N"), Some("D131N")),
];

const fn ch(offset_hz: f64, tone: Option<&'static str>, sent: Option<&'static str>) -> Ch {
    Ch {
        offset_hz,
        bandwidth: Bandwidth::Narrow,
        tone,
        sent,
    }
}

fn parse(s: Option<&str>) -> Option<Tone> {
    s.map(|s| s.parse().expect("tone"))
}

/// The 10 Msps loop: every channel modulated at 25 ksps (12 dB SNR, its own
/// key-up time) and placed at its offset.
fn capture() -> Vec<Complex<f32>> {
    let n = LOOP_S * FS;
    let narrow: Vec<Vec<Complex<f32>>> = CHANNELS
        .iter()
        .enumerate()
        .map(|(i, c)| {
            // Stagger the key-ups so calls start and end on different blocks.
            let start = FS / 4 + i * FS / 20;
            NbfmSignal {
                snr_db: 12.0,
                keyed: vec![start..start + FS * 22 / 10],
                seed: 1 + i as u64,
                deviation: match c.bandwidth {
                    Bandwidth::Narrow => Deviation::NARROW,
                    Bandwidth::Wide => Deviation::WIDE,
                },
                amplitude: 0.05,
                ..NbfmSignal::new(Audio::Voice, parse(c.sent))
            }
            .generate(n)
        })
        .collect();
    let placed: Vec<(f64, &[Complex<f32>])> = CHANNELS
        .iter()
        .zip(&narrow)
        .map(|(c, iq)| (CENTER_HZ + c.offset_hz, iq.as_slice()))
        .collect();
    testsig::place_wideband(RATE, CENTER_HZ, &placed)
}

struct Engine {
    handle: BankHandle,
    channel: NbfmChannel,
    buf: Vec<Complex<f32>>,
}

/// One timed pass over `REPLAYS` loops. Returns (total DSP time, time spent
/// in the channel engines alone, per-channel kept calls and tone mismatches).
fn pass(wide: &[Complex<f32>], with_channels: bool) -> (Duration, Duration, Vec<(u64, u64)>) {
    let mut bank = ChannelBank::new(RATE, CENTER_HZ, DEFAULT_TAPS_PER_CHANNEL).expect("bank");
    let mut engines: Vec<Engine> = if with_channels {
        CHANNELS
            .iter()
            .map(|c| {
                let params = ChannelParams {
                    bandwidth: c.bandwidth,
                    ..ChannelParams::with_tone(parse(c.tone))
                };
                params.validate().expect("params");
                Engine {
                    handle: bank.subscribe(CENTER_HZ + c.offset_hz).expect("subscribe"),
                    channel: NbfmChannel::new(params),
                    buf: Vec::with_capacity(BLOCK / 400 + 16),
                }
            })
            .collect()
    } else {
        Vec::new()
    };

    let mut total = Duration::ZERO;
    let mut in_channels = Duration::ZERO;
    let mut consumed = 0u64;
    let mut events = 0usize;
    for _ in 0..REPLAYS {
        for block in wide.chunks(BLOCK) {
            let t0 = Instant::now();
            bank.process_block(block);
            let t1 = Instant::now();
            let at_ms = consumed * 1000 / u64::from(RATE);
            for e in &mut engines {
                e.buf.clear();
                bank.drain(e.handle, &mut e.buf);
                if !e.buf.is_empty() {
                    e.channel.process(&e.buf, at_ms, &mut |_| events += 1);
                }
            }
            let t2 = Instant::now();
            total += t2 - t0;
            in_channels += t2 - t1;
            consumed += block.len() as u64;
        }
    }
    std::hint::black_box(events);
    let stats = engines
        .iter()
        .map(|e| {
            let s = e.channel.stats();
            (s.calls, s.tone_mismatch)
        })
        .collect();
    (total, in_channels, stats)
}

#[test]
#[ignore]
fn bench_bank_plus_11_channels_at_10msps() {
    let t = Instant::now();
    let wide = capture();
    println!(
        "bench: {} s of 10 Msps capture synthesised in {:.1} s; {} channels, {REPLAYS} replays per pass",
        LOOP_S,
        t.elapsed().as_secs_f64(),
        CHANNELS.len()
    );
    let seconds = (LOOP_S * REPLAYS) as f64;

    let mut best_a = Duration::MAX;
    let mut best_b = Duration::MAX;
    let mut best_ch = Duration::MAX;
    let mut stats = Vec::new();
    for _ in 0..ROUNDS {
        let (a, _, _) = pass(&wide, false);
        let (b, c, s) = pass(&wide, true);
        best_a = best_a.min(a);
        if b < best_b {
            best_b = b;
            stats = s;
        }
        best_ch = best_ch.min(c);
    }

    let per_s = |d: Duration| d.as_secs_f64() / seconds;
    let overhead = (best_b.as_secs_f64() - best_a.as_secs_f64()) / best_a.as_secs_f64() * 100.0;
    let share = best_ch.as_secs_f64() / best_a.as_secs_f64() * 100.0;
    println!(
        "bank only           : {:.4} s DSP per 1 s of capture ({:.1} % of one core)",
        per_s(best_a),
        per_s(best_a) * 100.0
    );
    println!(
        "bank + 11 channels  : {:.4} s DSP per 1 s of capture ({:.1} % of one core)",
        per_s(best_b),
        per_s(best_b) * 100.0
    );
    println!(
        "channels (in run b) : {:.4} s DSP per 1 s of capture — {share:.1} % of the bank",
        per_s(best_ch)
    );
    println!("overhead (b - a) / a: {overhead:.1} %");
    for (c, (calls, mismatch)) in CHANNELS.iter().zip(&stats) {
        println!(
            "  {:>+10.0} Hz {:>6} tone {:<6} calls {calls} tone_mismatch {mismatch}",
            c.offset_hz,
            c.bandwidth.hz(),
            c.tone.unwrap_or("-")
        );
    }

    // The engines really decoded: one call per channel per loop, never a
    // wrong-tone rejection.
    for (c, (calls, mismatch)) in CHANNELS.iter().zip(&stats) {
        assert!(
            *calls >= (REPLAYS - 1) as u64,
            "channel at {} Hz kept only {calls} calls",
            c.offset_hz
        );
        assert_eq!(*mismatch, 0, "channel at {} Hz", c.offset_hz);
    }
    // Plan target: 11 channels add at most 10 % to the device's DSP time.
    // Printed, not asserted (see the module docs for the measured result);
    // the assertion is a regression guard on the absolute per-channel cost.
    println!(
        "target (b - a) / a <= 10 %: {}",
        if overhead <= 10.0 { "met" } else { "NOT met" }
    );
    let per_channel_core = per_s(best_ch) / CHANNELS.len() as f64 * 100.0;
    println!("per channel         : {per_channel_core:.3} % of one core");
    assert!(
        per_channel_core <= 1.0,
        "one channel costs {per_channel_core:.3} % of one core (guard: 1 %)"
    );
}
