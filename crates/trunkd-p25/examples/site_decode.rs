// SPDX-License-Identifier: GPL-3.0-or-later
//! Decode a whole P25 Phase 1 trunked **site** from one wideband IQ capture
//! with [`SiteEngine`](trunkd_p25::SiteEngine): hunt the control channel among
//! the configured candidates, follow every grant on a pooled traffic channel,
//! and print one JSON line per `SiteEvent` plus a final summary.
//!
//! Where `channelizer_decode` needs the traffic frequencies up front, this
//! learns them from the control channel and subscribes/unsubscribes as calls
//! come and go.
//!
//! Usage:
//!   site_decode <file.iq> --rate 20000000 --format int8 \
//!       --center 855500000 --control 851050000 [--control ...] \
//!       [--mode lsm|c4fm] [--pool 20] [--hang 2000] [--taps-per-channel 12] \
//!       [--raw-control] [--quiet]
//!
//! `--quiet` suppresses the per-voice-frame lines (they are still counted).

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufReader, Read, Write};
use std::time::Instant;

use num_complex::Complex;
use trunkd_p25::dsp::channelizer::DEFAULT_TAPS_PER_CHANNEL;
use trunkd_p25::dsp::DemodMode;
use trunkd_p25::event::CallKind;
use trunkd_p25::site::{EndReason, SiteConfig, SiteEngine, SiteEvent};

struct Args {
    file: String,
    rate: u32,
    format: String,
    center_hz: f64,
    controls: Vec<u64>,
    mode: DemodMode,
    pool: usize,
    taps_per_channel: usize,
    hang_ms: u64,
    raw_control: bool,
    quiet: bool,
}

fn parse_args() -> Result<Args, String> {
    let argv: Vec<String> = std::env::args().collect();
    if argv.len() < 2 {
        return Err(format!(
            "usage: {} <file.iq> --rate <sps> --format int8|f32le --center <hz> \
             --control <hz> [--control <hz> ...] [--mode lsm|c4fm] [--pool <n>] \
             [--hang <ms>] [--taps-per-channel <n>] [--raw-control] [--quiet]",
            argv[0]
        ));
    }
    let mut a = Args {
        file: argv[1].clone(),
        rate: 20_000_000,
        format: "int8".into(),
        center_hz: 855_500_000.0,
        controls: Vec::new(),
        mode: DemodMode::Lsm,
        pool: 20,
        taps_per_channel: DEFAULT_TAPS_PER_CHANNEL,
        hang_ms: SiteConfig::default().hang_ms,
        raw_control: false,
        quiet: false,
    };
    let mut i = 2;
    while i < argv.len() {
        let key = argv[i].as_str();
        let mut val = || -> Result<String, String> {
            i += 1;
            argv.get(i)
                .cloned()
                .ok_or_else(|| format!("missing value for {key}"))
        };
        match key {
            "--rate" => a.rate = val()?.parse().map_err(|e| format!("--rate: {e}"))?,
            "--format" => a.format = val()?,
            "--center" => a.center_hz = val()?.parse().map_err(|e| format!("--center: {e}"))?,
            "--control" => a
                .controls
                .push(val()?.parse().map_err(|e| format!("--control: {e}"))?),
            "--pool" => a.pool = val()?.parse().map_err(|e| format!("--pool: {e}"))?,
            "--hang" => a.hang_ms = val()?.parse().map_err(|e| format!("--hang: {e}"))?,
            "--taps-per-channel" => {
                a.taps_per_channel = val()?
                    .parse()
                    .map_err(|e| format!("--taps-per-channel: {e}"))?
            }
            "--mode" => {
                a.mode = match val()?.as_str() {
                    "lsm" => DemodMode::Lsm,
                    "c4fm" => DemodMode::C4fm,
                    m => return Err(format!("unknown mode {m}")),
                }
            }
            "--raw-control" => a.raw_control = true,
            "--quiet" => a.quiet = true,
            other => return Err(format!("unknown argument {other}")),
        }
        i += 1;
    }
    if a.format != "int8" && a.format != "f32le" {
        return Err(format!("unknown format {} (int8|f32le)", a.format));
    }
    if a.controls.is_empty() {
        return Err("at least one --control is required".into());
    }
    Ok(a)
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn opt_str(v: &Option<String>) -> String {
    v.as_deref().map_or("null".to_string(), json_str)
}

fn opt_num(v: Option<impl std::fmt::Display>) -> String {
    v.map_or("null".to_string(), |n| n.to_string())
}

fn kind_str(k: CallKind) -> &'static str {
    match k {
        CallKind::Group => "group",
        CallKind::UnitToUnit => "unit_to_unit",
        CallKind::Telephone => "telephone",
    }
}

fn reason_str(r: EndReason) -> &'static str {
    match r {
        EndReason::Terminator => "terminator",
        EndReason::Inactivity => "inactivity",
        EndReason::PoolReassigned => "pool_reassigned",
        EndReason::Shutdown => "shutdown",
    }
}

/// One line of JSON per event, or `None` when `--quiet` swallows it.
fn event_json(ev: &SiteEvent, quiet: bool) -> Option<String> {
    Some(match ev {
        SiteEvent::ControlHunting { candidates } => {
            let list = candidates
                .iter()
                .map(u64::to_string)
                .collect::<Vec<_>>()
                .join(",");
            format!(r#"{{"type":"control_hunting","candidates":[{list}]}}"#)
        }
        SiteEvent::ControlAcquired { frequency_hz, nac } => format!(
            r#"{{"type":"control_acquired","freq":{frequency_hz},"nac":{nac},"nac_hex":"{nac:03x}"}}"#
        ),
        SiteEvent::ControlLost { frequency_hz } => {
            format!(r#"{{"type":"control_lost","freq":{frequency_hz}}}"#)
        }
        SiteEvent::CallStarted {
            id,
            frequency_hz,
            talkgroup,
            target,
            source,
            kind,
            patches: _,
            at_ms,
        } => format!(
            r#"{{"type":"call_started","id":{},"freq":{frequency_hz},"talkgroup":{talkgroup},"target":{target},"source":{source},"kind":"{}","at_ms":{at_ms}}}"#,
            id.0,
            kind_str(*kind)
        ),
        SiteEvent::CallTalker {
            id,
            source,
            emergency,
            alias,
            source_wacn,
            source_system,
            at_ms,
        } => format!(
            r#"{{"type":"call_talker","id":{},"source":{source},"emergency":{emergency},"alias":{},"source_wacn":{},"source_system":{},"at_ms":{at_ms}}}"#,
            id.0,
            opt_str(alias),
            opt_num(*source_wacn),
            opt_num(*source_system)
        ),
        SiteEvent::CallVoice { id, frame, at_ms } => {
            if quiet {
                return None;
            }
            format!(
                r#"{{"type":"call_voice","id":{},"frame":"{}","at_ms":{at_ms}}}"#,
                id.0,
                hex(frame)
            )
        }
        SiteEvent::CallEncryption {
            id,
            algid,
            key_id,
            at_ms,
        } => format!(
            r#"{{"type":"call_encryption","id":{},"algid":{algid},"key_id":{key_id},"at_ms":{at_ms}}}"#,
            id.0
        ),
        SiteEvent::CallEnded {
            id,
            record,
            at_ms,
            reason,
        } => {
            let talkers = record
                .talkers
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(",");
            format!(
                r#"{{"type":"call_ended","id":{},"reason":"{}","at_ms":{at_ms},"talkgroup":{},"source":{},"talkers":[{talkers}],"target":{},"emergency":{},"encrypted":{},"algid":{},"algorithm":"{}","key_id":{},"alias":{},"voice_frames":{},"duration_ms":{},"terminated":{}}}"#,
                id.0,
                reason_str(*reason),
                record.talkgroup.map_or("null".into(), |v| v.to_string()),
                record.source.map_or("null".into(), |v| v.to_string()),
                record.target.map_or("null".into(), |v| v.to_string()),
                record.emergency,
                record.encrypted,
                record.algid.map_or("null".into(), |v| v.to_string()),
                record.algorithm(),
                record.key_id.map_or("null".into(), |v| v.to_string()),
                opt_str(&record.talker_alias),
                record.voice_frames,
                record.duration_ms(),
                record.terminated,
            )
        }
        SiteEvent::CallSkipped {
            frequency_hz,
            talkgroup,
        } => {
            format!(r#"{{"type":"call_skipped","freq":{frequency_hz},"talkgroup":{talkgroup}}}"#)
        }
        SiteEvent::TdmaRefused {
            frequency_hz,
            timeslot,
            talkgroup,
            source,
            ..
        } => format!(
            r#"{{"type":"tdma_refused","freq":{frequency_hz},"slot":{timeslot},"talkgroup":{talkgroup},"src":{source}}}"#
        ),
        SiteEvent::PoolExhausted {
            frequency_hz,
            talkgroup,
        } => {
            format!(r#"{{"type":"pool_exhausted","freq":{frequency_hz},"talkgroup":{talkgroup}}}"#)
        }
        SiteEvent::Control(e) => format!(r#"{{"type":"control","event":"{e:?}"}}"#),
        SiteEvent::Traffic {
            frequency_hz,
            event,
        } => format!(r#"{{"type":"traffic","freq":{frequency_hz},"event":"{event:?}"}}"#),
        SiteEvent::Stats(s) => format!(
            r#"{{"type":"stats","stream_ms":{},"control_freq":{},"nac":{},"tsbks":{},"tsbk_rate":{:.2},"active_calls":{},"pool_in_use":{},"pool_size":{},"calls_started":{},"calls_ended":{},"voice_frames":{},"pool_exhausted":{},"out_of_band_grants":{},"sync_losses":{},"control_freq_error_hz":{:.1},"traffic_freq_error_hz":{:.1}}}"#,
            s.stream_ms,
            s.control_frequency_hz,
            s.nac,
            s.control_tsbks,
            s.control_tsbk_rate,
            s.active_calls,
            s.pool_in_use,
            s.pool_size,
            s.calls_started,
            s.calls_ended,
            s.voice_frames,
            s.pool_exhausted,
            s.out_of_band_grants,
            s.sync_losses,
            s.control_frequency_error_hz,
            s.traffic_frequency_error_hz,
        ),
        SiteEvent::GrantOut {
            frequency_hz, info, ..
        } => format!(
            r#"{{"type":"grant_out","freq":{frequency_hz},"talkgroup":{}}}"#,
            info.talkgroup
        ),
    })
}

/// End-of-run tallies (what the fixture runs are graded on).
#[derive(Default)]
struct Summary {
    calls_started: u64,
    calls_ended: u64,
    voice_frames: u64,
    clear_calls: u64,
    encrypted_calls: u64,
    pool_exhausted: u64,
    control_acquired: Option<(u64, u16)>,
    control_lost: u64,
    /// freq → (calls, voice frames)
    per_freq: BTreeMap<u64, (u64, u64)>,
    /// talkgroup → voice frames
    per_talkgroup: BTreeMap<u16, u64>,
    /// call id → frequency, so voice frames can be attributed
    open: BTreeMap<u64, u64>,
    aliases: BTreeMap<String, u64>,
}

impl Summary {
    fn observe(&mut self, ev: &SiteEvent) {
        match ev {
            SiteEvent::ControlAcquired { frequency_hz, nac } => {
                self.control_acquired = Some((*frequency_hz, *nac))
            }
            SiteEvent::ControlLost { .. } => self.control_lost += 1,
            SiteEvent::CallStarted {
                id, frequency_hz, ..
            } => {
                self.calls_started += 1;
                self.open.insert(id.0, *frequency_hz);
                self.per_freq.entry(*frequency_hz).or_default().0 += 1;
            }
            SiteEvent::CallVoice { id, .. } => {
                self.voice_frames += 1;
                if let Some(f) = self.open.get(&id.0) {
                    self.per_freq.entry(*f).or_default().1 += 1;
                }
            }
            SiteEvent::CallEnded { id, record, .. } => {
                self.calls_ended += 1;
                if record.encrypted {
                    self.encrypted_calls += 1;
                } else {
                    self.clear_calls += 1;
                }
                if let Some(tg) = record.talkgroup {
                    *self.per_talkgroup.entry(tg).or_default() += u64::from(record.voice_frames);
                }
                if let Some(a) = &record.talker_alias {
                    *self.aliases.entry(a.clone()).or_default() += 1;
                }
                self.open.remove(&id.0);
            }
            SiteEvent::PoolExhausted { .. } => self.pool_exhausted += 1,
            _ => {}
        }
    }
}

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };

    let cfg = SiteConfig {
        sample_rate: args.rate,
        center_hz: args.center_hz,
        control_frequencies: args.controls.clone(),
        mode: args.mode,
        traffic_pool_size: args.pool,
        taps_per_channel: args.taps_per_channel,
        hang_ms: args.hang_ms,
        raw_control: args.raw_control,
        ..SiteConfig::default()
    };
    let mut engine = match SiteEngine::new(cfg) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("site engine: {e}");
            std::process::exit(2);
        }
    };
    eprintln!(
        "site: {} candidate(s) reachable of {} configured, channel rate {} Hz, pool {}",
        engine.candidates().len(),
        args.controls.len(),
        engine.output_rate(),
        args.pool,
    );

    let file = match File::open(&args.file) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("{}: {e}", args.file);
            std::process::exit(2);
        }
    };
    let mut reader = BufReader::with_capacity(1 << 20, file);

    let quiet = args.quiet;
    let bytes_per_sample = if args.format == "int8" { 2 } else { 8 };
    let mut raw = vec![0u8; 1 << 18];
    let mut wb: Vec<Complex<f32>> = Vec::with_capacity(1 << 17);
    let mut leftover = 0usize;
    let mut total_samples = 0u64;
    let mut summary = Summary::default();
    let stdout = std::io::stdout();
    let mut out = std::io::BufWriter::with_capacity(1 << 20, stdout.lock());
    let started = Instant::now();

    loop {
        let fresh = match reader.read(&mut raw[leftover..]) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => {
                eprintln!("read: {e}");
                std::process::exit(2);
            }
        };
        let n = leftover + fresh;
        let usable = n - n % bytes_per_sample;
        wb.clear();
        if args.format == "int8" {
            for pair in raw[..usable].as_chunks::<2>().0 {
                wb.push(Complex::new(
                    f32::from(pair[0] as i8) / 128.0,
                    f32::from(pair[1] as i8) / 128.0,
                ));
            }
        } else {
            for quad in raw[..usable].as_chunks::<8>().0 {
                let i = f32::from_le_bytes([quad[0], quad[1], quad[2], quad[3]]);
                let q = f32::from_le_bytes([quad[4], quad[5], quad[6], quad[7]]);
                wb.push(Complex::new(i, q));
            }
        }
        total_samples += wb.len() as u64;
        engine.process_block(&wb, &mut |ev| {
            summary.observe(&ev);
            if let Some(line) = event_json(&ev, quiet) {
                let _ = writeln!(out, "{line}");
            }
        });
        raw.copy_within(usable..n, 0);
        leftover = n - usable;
    }

    engine.shutdown(&mut |ev| {
        summary.observe(&ev);
        if let Some(line) = event_json(&ev, quiet) {
            let _ = writeln!(out, "{line}");
        }
    });

    let elapsed = started.elapsed().as_secs_f64();
    let capture_secs = total_samples as f64 / f64::from(args.rate);
    let per_freq = summary
        .per_freq
        .iter()
        .map(|(f, (calls, voice))| {
            format!(r#"{{"freq":{f},"calls":{calls},"voice_frames":{voice}}}"#)
        })
        .collect::<Vec<_>>()
        .join(",");
    let per_tg = summary
        .per_talkgroup
        .iter()
        .map(|(tg, voice)| format!(r#"{{"talkgroup":{tg},"voice_frames":{voice}}}"#))
        .collect::<Vec<_>>()
        .join(",");
    let aliases = summary
        .aliases
        .iter()
        .map(|(a, n)| format!(r#"{{"alias":{},"calls":{n}}}"#, json_str(a)))
        .collect::<Vec<_>>()
        .join(",");
    let (ctrl_freq, ctrl_nac) = summary.control_acquired.unwrap_or((0, 0));
    let _ = writeln!(
        out,
        r#"{{"summary":{{"control_freq":{ctrl_freq},"control_nac":{ctrl_nac},"control_lost":{},"calls_started":{},"calls_ended":{},"clear_calls":{},"encrypted_calls":{},"voice_frames":{},"pool_exhausted":{},"per_frequency":[{per_freq}],"per_talkgroup":[{per_tg}],"aliases":[{aliases}],"input_samples":{total_samples},"capture_seconds":{capture_secs:.3},"wall_seconds":{elapsed:.3},"times_realtime":{:.3}}}}}"#,
        summary.control_lost,
        summary.calls_started,
        summary.calls_ended,
        summary.clear_calls,
        summary.encrypted_calls,
        summary.voice_frames,
        summary.pool_exhausted,
        capture_secs / elapsed,
    );
    let _ = out.flush();
}
