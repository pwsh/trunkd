// SPDX-License-Identifier: GPL-3.0-or-later
//! Decode a P25 Phase 1 IQ capture and print one JSON line per event plus a
//! final JSON summary (for parity diffing against a reference decode).
//!
//! Usage:
//!   p25_decode <file.iq> --rate 2400000 --format int8 --offset 400500 \
//!       [--mode lsm|c4fm] [--channel-rate 50000] [--freq 851050500] \
//!       [--inventory] [--quiet]
//!
//! `--inventory` folds every event into a `SiteInventory` and prints
//! `{"inventory": <snapshot>}` — the observed site channel plan — at end of
//! run.
//!
//! The DDC is a simple complex mix at `--offset` Hz followed by FIR
//! decimation (15-tap half-band ÷2 stages then a Kaiser stage) down to
//! `--channel-rate`, which must divide the input rate.

use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};

use num_complex::Complex;
use trunkd_p25::dsp::DemodMode;
use trunkd_p25::event::P25Event;
use trunkd_p25::payload::LinkControl;
use trunkd_p25::{CallAssembler, CallRecord, P25P1WidebandChannel, SiteInventory};

struct Args {
    file: String,
    rate: u32,
    format: String,
    offset_hz: f64,
    mode: DemodMode,
    channel_rate: u32,
    freq_hz: u64,
    quiet: bool,
    /// Input samples to discard before decoding (sample-phase repro knob).
    skip_samples: u64,
    /// Print the observed site inventory at end of run.
    inventory: bool,
}

fn parse_args() -> Result<Args, String> {
    let argv: Vec<String> = std::env::args().collect();
    if argv.len() < 2 {
        return Err(format!(
            "usage: {} <file.iq> --rate <sps> --format int8|f32le --offset <hz> \
             [--mode lsm|c4fm] [--channel-rate <sps>] [--freq <hz>] [--skip-samples <n>] \
             [--inventory] [--quiet]",
            argv[0]
        ));
    }
    let mut a = Args {
        file: argv[1].clone(),
        rate: 2_400_000,
        format: "int8".into(),
        offset_hz: 0.0,
        mode: DemodMode::Lsm,
        channel_rate: 50_000,
        freq_hz: 0,
        quiet: false,
        skip_samples: 0,
        inventory: false,
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
            "--offset" => a.offset_hz = val()?.parse().map_err(|e| format!("--offset: {e}"))?,
            "--channel-rate" => {
                a.channel_rate = val()?.parse().map_err(|e| format!("--channel-rate: {e}"))?
            }
            "--freq" => a.freq_hz = val()?.parse().map_err(|e| format!("--freq: {e}"))?,
            "--skip-samples" => {
                a.skip_samples = val()?.parse().map_err(|e| format!("--skip-samples: {e}"))?
            }
            "--mode" => {
                a.mode = match val()?.as_str() {
                    "lsm" => DemodMode::Lsm,
                    "c4fm" => DemodMode::C4fm,
                    m => return Err(format!("unknown mode {m}")),
                }
            }
            "--quiet" => a.quiet = true,
            "--inventory" => a.inventory = true,
            other => return Err(format!("unknown argument {other}")),
        }
        i += 1;
    }
    if a.format != "int8" && a.format != "f32le" {
        return Err(format!("unknown format {} (int8|f32le)", a.format));
    }
    if !a.rate.is_multiple_of(a.channel_rate) {
        return Err(format!(
            "--channel-rate {} must divide --rate {}",
            a.channel_rate, a.rate
        ));
    }
    Ok(a)
}

fn json_escape_free(s: &str) -> &str {
    s // all our strings are static identifiers
}

fn event_json(e: &P25Event) -> Option<String> {
    let j = match e {
        P25Event::SyncDetected { score } => format!(r#"{{"type":"sync","score":{score:.1}}}"#),
        P25Event::SyncLoss { bits } => format!(r#"{{"type":"sync_loss","bits":{bits}}}"#),
        P25Event::NidDecoded {
            nac,
            duid,
            corrected,
        } => format!(r#"{{"type":"nid","nac":{nac},"duid":{duid},"corrected":{corrected}}}"#),
        P25Event::Tsbk {
            tsbk,
            corrected,
            crc_corrected,
        } => format!(
            r#"{{"type":"tsbk","opcode":{},"vendor":{},"last":{},"corrected":{},"crc_corrected":{},"msg":"{}"}}"#,
            tsbk.opcode,
            tsbk.vendor,
            tsbk.last_block,
            corrected,
            crc_corrected,
            json_escape_free(tsbk_kind(&tsbk.message)),
        ),
        P25Event::TsbkFailed => r#"{"type":"tsbk_failed"}"#.to_string(),
        P25Event::PduHeader { header, corrected } => format!(
            r#"{{"type":"pdu_header","format":{},"llid":{},"blocks":{},"confirmed":{},"corrected":{}}}"#,
            header.format, header.llid, header.blocks_to_follow, header.confirmed, corrected
        ),
        P25Event::Pdu {
            header,
            blocks,
            valid,
            corrected,
            ..
        } => format!(
            r#"{{"type":"pdu","format":{},"llid":{},"blocks":{},"valid":{},"corrected":{}}}"#,
            header.format, header.llid, blocks, valid, corrected
        ),
        P25Event::Hdu {
            valid,
            header,
            corrected,
            ..
        } => {
            let fields = match header {
                Some(h) => format!(
                    r#","talkgroup":{},"algid":{},"algorithm":"{}","key_id":{},"encrypted":{}"#,
                    h.talkgroup,
                    h.algid,
                    h.algorithm(),
                    h.kid,
                    !h.is_clear()
                ),
                None => String::new(),
            };
            format!(r#"{{"type":"hdu","valid":{valid},"corrected":{corrected}{fields}}}"#)
        }
        P25Event::Tdu => r#"{"type":"tdu"}"#.to_string(),
        P25Event::TduLc {
            valid,
            link_control,
            corrected,
        } => format!(
            r#"{{"type":"tdulc","valid":{},"lc":"{}","corrected":{}}}"#,
            valid,
            hex(link_control),
            corrected
        ),
        P25Event::ImbeFrame { ldu, index, frame } => format!(
            r#"{{"type":"imbe","ldu":{},"index":{},"frame":"{}"}}"#,
            match ldu {
                trunkd_p25::event::LduKind::Ldu1 => 1,
                trunkd_p25::event::LduKind::Ldu2 => 2,
            },
            index,
            hex(frame)
        ),
        P25Event::LinkControl {
            word,
            parsed,
            corrected,
        } => format!(
            r#"{{"type":"link_control","lc":"{}","corrected":{}{}}}"#,
            hex(word),
            corrected,
            lc_fields(parsed),
        ),
        P25Event::EncryptionSync {
            word,
            parsed,
            corrected,
        } => format!(
            r#"{{"type":"encryption_sync","es":"{}","corrected":{},"algid":{},"algorithm":"{}","key_id":{},"mi":"{}","encrypted":{}}}"#,
            hex(word),
            corrected,
            parsed.algid,
            parsed.algorithm(),
            parsed.kid,
            hex(&parsed.mi),
            !parsed.is_clear(),
        ),
        P25Event::LowSpeedData {
            data,
            errors,
            valid,
        } => {
            format!(
                r#"{{"type":"lsd","data":"{}","errors":[{},{}],"valid":[{},{}]}}"#,
                hex(data),
                errors[0],
                errors[1],
                valid[0],
                valid[1]
            )
        }
        P25Event::Grant {
            kind,
            frequency_hz,
            talkgroup,
            target,
            source,
            update,
        } => format!(
            r#"{{"type":"grant","kind":"{}","freq":{},"talkgroup":{},"target":{},"source":{},"update":{}}}"#,
            match kind {
                trunkd_p25::event::CallKind::Group => "group",
                trunkd_p25::event::CallKind::UnitToUnit => "unit",
                trunkd_p25::event::CallKind::Telephone => "telephone",
            },
            frequency_hz,
            talkgroup,
            target,
            source,
            update
        ),
        P25Event::GrantUnresolvable { band } => {
            format!(r#"{{"type":"grant_unresolvable","band":{band}}}"#)
        }
        P25Event::CallStart {
            frequency_hz,
            talkgroup,
            target,
            source,
        } => format!(
            r#"{{"type":"call_start","freq":{frequency_hz},"talkgroup":{talkgroup},"target":{target},"source":{source}}}"#
        ),
        P25Event::CallContinue {
            frequency_hz,
            talkgroup,
            target,
            source,
        } => format!(
            r#"{{"type":"call_continue","freq":{frequency_hz},"talkgroup":{talkgroup},"target":{target},"source":{source}}}"#
        ),
        P25Event::CallEnd { frequency_hz } => {
            format!(r#"{{"type":"call_end","freq":{frequency_hz}}}"#)
        }
        P25Event::PrivateCall {
            start,
            frequency_hz,
            source,
            target,
            duration_ms,
        } => format!(
            r#"{{"type":"private_call","start":{start},"freq":{frequency_hz},"src":{source},"dst":{target},"duration_ms":{duration_ms}}}"#
        ),
        P25Event::TrafficChannelRequest { frequency_hz } => {
            format!(r#"{{"type":"traffic_request","freq":{frequency_hz}}}"#)
        }
        P25Event::DataGrant { frequency_hz } => {
            format!(r#"{{"type":"data_grant","freq":{frequency_hz}}}"#)
        }
        P25Event::PoolExhausted { frequency_hz } => {
            format!(r#"{{"type":"pool_exhausted","freq":{frequency_hz}}}"#)
        }
        P25Event::TdmaGrantRefused {
            frequency_hz,
            timeslot,
            talkgroup,
            source,
            ..
        } => format!(
            r#"{{"type":"tdma_refused","freq":{frequency_hz},"slot":{timeslot},"talkgroup":{talkgroup},"src":{source}}}"#
        ),
        P25Event::SiteBroadcast {
            kind,
            message,
            frequency_hz,
            frequency_b_hz,
        } => format!(
            r#"{{"type":"site_broadcast","kind":"{}","freq":{},"freq_b":{}{}}}"#,
            site_broadcast_kind(kind),
            opt_u64(*frequency_hz),
            opt_u64(*frequency_b_hz),
            site_broadcast_fields(message),
        ),
        P25Event::IdenUpdate {
            id,
            base_hz,
            spacing_hz,
            offset_hz,
            bandwidth_hz,
        } => format!(
            r#"{{"type":"iden","id":{id},"base":{base_hz},"spacing":{spacing_hz},"offset":{offset_hz},"bandwidth":{bandwidth_hz}}}"#
        ),
        P25Event::FrequencyError { hz } => {
            format!(r#"{{"type":"freq_error","hz":{hz:.1}}}"#)
        }
        // Recorder-generated, never produced by a decode of a capture file.
        P25Event::ClockResync { correction_ms } => {
            format!(r#"{{"type":"clock_resync","correction_ms":{correction_ms}}}"#)
        }
    };
    Some(j)
}

/// `"band-chan"`, the form the reference decoder logs.
fn chan_label(c: &trunkd_p25::message::ChannelRef) -> String {
    format!("{}-{}", c.band, c.channel)
}

fn opt_u64(v: Option<u64>) -> String {
    v.map_or("null".to_string(), |x| x.to_string())
}

/// JSON fragment for a service-class byte.
fn svc_class(s: &trunkd_p25::message::ServiceClass) -> String {
    format!(
        r#"{{"raw":{},"composite":{},"no_service_requests":{},"backup_control":{},"data":{},"voice":{},"registration":{},"authentication":{}}}"#,
        s.raw,
        s.composite,
        s.no_service_requests,
        s.backup_control,
        s.data,
        s.voice,
        s.registration,
        s.authentication
    )
}

/// Parsed-field JSON fragment for a site/network broadcast (leading comma
/// included); empty for any other message.
fn site_broadcast_fields(m: &trunkd_p25::message::TsbkMessage) -> String {
    use trunkd_p25::message::TsbkMessage as T;
    match m {
        T::SecondaryControlChannel {
            rfss,
            site,
            channel_a,
            service_class_a,
            channel_b,
            service_class_b,
        } => format!(
            r#","rfss":{},"site":{},"channel_a":"{}","service_class_a":{},"channel_b":{},"service_class_b":{}"#,
            rfss,
            site,
            chan_label(channel_a),
            svc_class(service_class_a),
            channel_b.map_or("null".to_string(), |c| format!(r#""{}""#, chan_label(&c))),
            svc_class(service_class_b),
        ),
        T::RfssStatus {
            lra,
            active_network_connection,
            system_id,
            rfss,
            site,
            channel,
            service_class,
        } => format!(
            r#","lra":{},"active_network_connection":{},"system":{},"rfss":{},"site":{},"channel":"{}","service_class":{}"#,
            lra,
            active_network_connection,
            system_id,
            rfss,
            site,
            chan_label(channel),
            svc_class(service_class),
        ),
        T::NetworkStatus {
            lra,
            wacn,
            system_id,
            channel,
            service_class,
        } => format!(
            r#","lra":{},"wacn":{},"system":{},"channel":"{}","service_class":{}"#,
            lra,
            wacn,
            system_id,
            chan_label(channel),
            svc_class(service_class),
        ),
        T::AdjacentStatus {
            lra,
            flags,
            system_id,
            rfss,
            site,
            channel,
            service_class,
        } => format!(
            r#","lra":{},"system":{},"rfss":{},"site":{},"channel":"{}","flags":{{"conventional":{},"site_failure":{},"valid_information":{},"active_rfss_connection":{}}},"service_class":{}"#,
            lra,
            system_id,
            rfss,
            site,
            chan_label(channel),
            flags.conventional,
            flags.site_failure,
            flags.valid_information,
            flags.active_rfss_connection,
            svc_class(service_class),
        ),
        _ => String::new(),
    }
}

fn site_broadcast_kind(k: &trunkd_p25::event::SiteBroadcastKind) -> &'static str {
    use trunkd_p25::event::SiteBroadcastKind as K;
    match k {
        K::SecondaryControl => "secondary_control",
        K::RfssStatus => "rfss_status",
        K::NetworkStatus => "network_status",
        K::AdjacentStatus => "adjacent_status",
    }
}

fn tsbk_kind(m: &trunkd_p25::message::TsbkMessage) -> &'static str {
    use trunkd_p25::message::TsbkMessage as T;
    match m {
        T::GroupVoiceGrant { .. } => "group_voice_grant",
        T::GroupVoiceGrantUpdate { .. } => "group_voice_grant_update",
        T::GroupVoiceGrantUpdateExplicit { .. } => "group_voice_grant_update_explicit",
        T::UnitToUnitGrant { .. } => "unit_to_unit_grant",
        T::TelephoneInterconnectGrant { .. } => "telephone_grant",
        T::SndcpDataChannelGrant { .. } => "sndcp_data_channel_grant",
        T::SndcpDataPageRequest { .. } => "sndcp_data_page_request",
        T::SndcpDataChannelAnnounceExplicit { .. } => "sndcp_data_channel_announce_explicit",
        T::AcknowledgeResponse { .. } => "acknowledge_response",
        T::QueuedResponse { .. } => "queued_response",
        T::DenyResponse { .. } => "deny_response",
        T::GroupAffiliationResponse { .. } => "group_affiliation_response",
        T::ExtendedFunctionCommand { .. } => "extended_function_command",
        T::SystemServiceBroadcast { .. } => "system_service_broadcast",
        T::GroupAffiliationQuery { .. } => "group_affiliation_query",
        T::LocationRegistrationResponse { .. } => "location_registration_response",
        T::UnitRegistrationResponse { .. } => "unit_registration_response",
        T::UnitRegistrationCommand { .. } => "unit_registration_command",
        T::UnitDeRegistrationAck { .. } => "unit_deregistration_ack",
        T::SecondaryControlChannel { .. } => "secondary_control_channel",
        T::RfssStatus { .. } => "rfss_status",
        T::NetworkStatus { .. } => "network_status",
        T::AdjacentStatus { .. } => "adjacent_status",
        T::IdenUpdate(_) => "iden_update",
        T::HarrisGroupRegroup { .. } => "harris_group_regroup",
        T::HarrisOsp { .. } => "harris_osp",
        T::Other { .. } => "other",
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Parsed-field JSON fragment for a Link Control word (leading comma included).
fn lc_fields(lc: &LinkControl) -> String {
    match lc {
        LinkControl::GroupVoiceChannelUser {
            options,
            talkgroup,
            source,
            ..
        } => format!(
            r#","lco":"group","talkgroup":{},"source":{},"emergency":{},"encrypted":{}"#,
            talkgroup, source, options.emergency, options.encrypted
        ),
        LinkControl::UnitToUnitVoiceChannelUser {
            options,
            target,
            source,
        } => format!(
            r#","lco":"unit_to_unit","target":{},"source":{},"emergency":{}"#,
            target, source, options.emergency
        ),
        LinkControl::UnitToUnitAnswerRequest { target, source, .. } => {
            format!(r#","lco":"unit_to_unit_answer_request","target":{target},"source":{source}"#)
        }
        LinkControl::TelephoneInterconnectVoiceChannelUser {
            call_timer,
            address,
            ..
        } => format!(r#","lco":"telephone","address":{address},"call_timer":{call_timer}"#),
        LinkControl::SourceIdExtension {
            wacn,
            system,
            source,
        } => format!(
            r#","lco":"source_id_extension","wacn":{wacn},"system":{system},"source":{source}"#
        ),
        LinkControl::GroupVoiceChannelUpdate { a, b } => format!(
            r#","lco":"group_update","talkgroup_a":{},"channel_a":{},"talkgroup_b":{}"#,
            a.1,
            a.0.channel,
            b.map_or(-1i64, |(_, tg)| i64::from(tg)),
        ),
        LinkControl::CallTermination { source } => {
            format!(r#","lco":"call_termination","source":{source}"#)
        }
        LinkControl::GroupAffiliationQuery { target, source } => {
            format!(r#","lco":"group_affiliation_query","target":{target},"source":{source}"#)
        }
        LinkControl::ExtendedFunctionCommand {
            function,
            arguments,
            target,
        } => format!(
            r#","lco":"extended_function","function":"{}","arguments":{arguments},"target":{target}"#,
            function.name(),
        ),
        LinkControl::Harris { opcode, word } => {
            let alias = ascii_of(&word[2..9]);
            format!(r#","lco":"harris","opcode":"{opcode:?}","alias":"{alias}""#)
        }
        LinkControl::Other {
            lco,
            vendor,
            standard,
            ..
        } => format!(r#","lco":{lco},"vendor":{vendor},"standard":{standard}"#),
    }
}

/// Printable-ASCII rendering of bytes (non-printable → '.').
fn ascii_of(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|&b| {
            if (0x20..0x7f).contains(&b) {
                b as char
            } else {
                '.'
            }
        })
        .collect()
}

/// One JSON line summarizing a completed call.
fn call_record_json(r: &CallRecord) -> String {
    let tg = r.talkgroup.map_or("null".to_string(), |v| v.to_string());
    let src = r.source.map_or("null".to_string(), |v| v.to_string());
    let target = r.target.map_or("null".to_string(), |v| v.to_string());
    let algid = r.algid.map_or("null".to_string(), |v| v.to_string());
    let kid = r.key_id.map_or("null".to_string(), |v| v.to_string());
    let mi =
        r.mi.map_or("null".to_string(), |m| format!("\"{}\"", hex(&m)));
    let alias = r
        .talker_alias
        .as_deref()
        .map_or("null".to_string(), |a| format!("\"{a}\""));
    let talkers = r
        .talkers
        .iter()
        .map(|t| t.to_string())
        .collect::<Vec<_>>()
        .join(",");
    format!(
        r#"{{"type":"call_record","talkgroup":{tg},"source":{src},"talkers":[{talkers}],"target":{target},"emergency":{},"encrypted":{},"algid":{algid},"algorithm":"{}","key_id":{kid},"mi":{mi},"talker_alias":{alias},"voice_frames":{},"duration_ms":{},"terminated":{}}}"#,
        r.emergency,
        r.encrypted,
        r.algorithm(),
        r.voice_frames,
        r.duration_ms(),
        r.terminated,
    )
}

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };

    let mut channel = match P25P1WidebandChannel::new(
        args.rate,
        args.channel_rate,
        args.offset_hz,
        args.mode,
        args.freq_hz,
    ) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("engine: {e}");
            std::process::exit(2);
        }
    };

    let file = match File::open(&args.file) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("{}: {e}", args.file);
            std::process::exit(2);
        }
    };
    let mut reader = BufReader::with_capacity(1 << 20, file);
    if args.skip_samples > 0 {
        let bytes_per_sample = if args.format == "int8" { 2u64 } else { 8 };
        if let Err(e) = reader.seek(SeekFrom::Start(args.skip_samples * bytes_per_sample)) {
            eprintln!("{}: seek: {e}", args.file);
            std::process::exit(2);
        }
    }

    // Summary accumulators.
    let mut grants: std::collections::BTreeSet<(u64, u16, u32)> = Default::default();
    let mut idens: std::collections::BTreeMap<u8, (u64, u32, i64, u32)> = Default::default();

    let quiet = args.quiet;
    // Per-call metadata aggregator (meaningful on a traffic channel).
    let mut assembler = CallAssembler::new();
    // Observed site channel plan (`--inventory`). The engine's clock is
    // derived from its symbol counter; the example's is derived from the input
    // sample count, which is the same wall time at chunk granularity.
    let mut inventory = SiteInventory::new(if args.freq_hz != 0 {
        vec![args.freq_hz]
    } else {
        Vec::new()
    });
    // `Cell` because the read chunk loop advances the clock while `on_event`
    // (which reads it) is still borrowed.
    let now_ms = std::cell::Cell::new(0u64);
    let freq_hz = args.freq_hz;
    {
        let mut on_event = |e: P25Event| {
            inventory.observe(freq_hz, &e, now_ms.get());
            match &e {
                P25Event::Grant {
                    frequency_hz,
                    talkgroup,
                    source,
                    ..
                } => {
                    grants.insert((*frequency_hz, *talkgroup, *source));
                }
                P25Event::IdenUpdate {
                    id,
                    base_hz,
                    spacing_hz,
                    offset_hz,
                    bandwidth_hz,
                } => {
                    idens.insert(*id, (*base_hz, *spacing_hz, *offset_hz, *bandwidth_hz));
                }
                _ => {}
            }
            if !quiet {
                if let Some(line) = event_json(&e) {
                    println!("{line}");
                }
            }
            // Fold voice-path events into per-call records; emit on completion.
            if let Some(rec) = assembler.observe(&e) {
                if !quiet && rec.is_substantive() {
                    println!("{}", call_record_json(&rec));
                }
            }
        };

        let mut raw = vec![0u8; 1 << 16];
        // Wideband samples for this read chunk, fed to the wideband channel
        // (which does the DDC internally). Reused across iterations.
        let mut wb_buf: Vec<Complex<f32>> = Vec::with_capacity(1 << 15);
        let bytes_per_sample = if args.format == "int8" { 2 } else { 8 };
        let mut leftover = 0usize;
        let mut input_samples = 0u64;
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
            wb_buf.clear();
            if args.format == "int8" {
                for pair in raw[..usable].as_chunks::<2>().0 {
                    wb_buf.push(Complex::new(
                        f32::from(pair[0] as i8) / 128.0,
                        f32::from(pair[1] as i8) / 128.0,
                    ));
                }
            } else {
                for quad in raw[..usable].as_chunks::<8>().0 {
                    let i = f32::from_le_bytes([quad[0], quad[1], quad[2], quad[3]]);
                    let q = f32::from_le_bytes([quad[4], quad[5], quad[6], quad[7]]);
                    wb_buf.push(Complex::new(i, q));
                }
            }
            input_samples += wb_buf.len() as u64;
            now_ms.set(input_samples * 1000 / u64::from(args.rate));
            channel.process(&wb_buf, &mut on_event);
            // Carry any partial sample bytes to the front for the next read.
            let rem = n - usable;
            for k in 0..rem {
                raw[k] = raw[usable + k];
            }
            leftover = rem;
        }
    }
    // Flush a call still open at end-of-stream (not cleanly terminated).
    if let Some(rec) = assembler.take() {
        if !quiet && rec.is_substantive() {
            println!("{}", call_record_json(&rec));
        }
    }

    // Final deterministic summary.
    let stats = channel.channel().stats().clone();
    let mut nac_hist: Vec<(u16, u64)> = stats.nac_histogram.clone();
    nac_hist.sort();
    let nac_json = nac_hist
        .iter()
        .map(|(nac, n)| format!(r#""{nac}":{n}"#))
        .collect::<Vec<_>>()
        .join(",");
    let opcode_json = stats
        .tsbk_opcode_histogram
        .iter()
        .enumerate()
        .filter(|(_, &n)| n > 0)
        .map(|(op, n)| format!(r#""{op}":{n}"#))
        .collect::<Vec<_>>()
        .join(",");
    let grants_json = grants
        .iter()
        .map(|(f, tg, src)| format!(r#"{{"freq":{f},"talkgroup":{tg},"source":{src}}}"#))
        .collect::<Vec<_>>()
        .join(",");
    let iden_json = idens
        .iter()
        .map(|(id, (base, spacing, offset, bw))| {
            format!(
                r#"{{"id":{id},"base":{base},"spacing":{spacing},"offset":{offset},"bandwidth":{bw}}}"#
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    println!(
        r#"{{"summary":{{"sync_count":{},"nid_count":{},"nac_histogram":{{{}}},"tsbk_count":{},"tsbk_opcode_histogram":{{{}}},"grants":[{}],"iden":[{}],"corrected_bits_total":{}}}}}"#,
        stats.sync_count,
        stats.nid_count,
        nac_json,
        stats.tsbk_count,
        opcode_json,
        grants_json,
        iden_json,
        stats.corrected_bits,
    );

    if args.inventory {
        let snapshot = inventory.snapshot(now_ms.get());
        match serde_json::to_string(&snapshot) {
            Ok(j) => println!(r#"{{"inventory":{j}}}"#),
            Err(e) => eprintln!("inventory: {e}"),
        }
    }
}
