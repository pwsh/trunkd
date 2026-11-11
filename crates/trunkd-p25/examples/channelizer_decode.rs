// SPDX-License-Identifier: GPL-3.0-or-later
//! Decode one or many P25 Phase 1 channels out of a single wideband IQ
//! capture using the Fred-Harris M2 polyphase channelizer, printing one JSON
//! line per event (like `p25_decode`) plus a per-channel summary.
//!
//! The point of this example versus `p25_decode` is cost: `p25_decode` runs
//! one full-rate DDC per channel (`O(N)`); this runs the filterbank once for
//! the whole span (`O(1)`) and every extra `--freq` is nearly free.
//!
//! Usage:
//!   channelizer_decode <file.iq> --rate 20000000 --format int8 \
//!       --center 855500000 --freq 851050000 [--freq ...] \
//!       [--mode lsm|c4fm] [--taps-per-channel 12] [--shift <hz>] \
//!       [--inventory] [--quiet]
//!
//! `--inventory` folds each channel's events into a `SiteInventory` and prints
//! `{"inventory": <snapshot>}` per channel at end of run.
//!
//! `--shift` pre-mixes the wideband input by the given offset before the
//! filterbank, which models a tuner LO placed off the channel raster. Shifting
//! by half a bin (6250 Hz) moves every on-raster channel onto a bin seam and so
//! exercises the two-bin synthesis path on real signals.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{BufReader, Read};
use std::time::Instant;

use num_complex::Complex;
use trunkd_p25::dsp::channelizer::{ChannelHandle, Channelizer, PolyphaseChannelizerM2};
use trunkd_p25::dsp::DemodMode;
use trunkd_p25::event::P25Event;
use trunkd_p25::payload::LinkControl;
use trunkd_p25::{P25P1Channel, SiteInventory};

struct Args {
    file: String,
    rate: u32,
    format: String,
    center_hz: f64,
    freqs: Vec<u64>,
    mode: DemodMode,
    taps_per_channel: usize,
    shift_hz: f64,
    quiet: bool,
    /// Print each channel's observed site inventory at end of run.
    inventory: bool,
}

fn parse_args() -> Result<Args, String> {
    let argv: Vec<String> = std::env::args().collect();
    if argv.len() < 2 {
        return Err(format!(
            "usage: {} <file.iq> --rate <sps> --format int8|f32le --center <hz> \
             --freq <hz> [--freq <hz> ...] [--mode lsm|c4fm] \
             [--taps-per-channel <n>] [--inventory] [--quiet]",
            argv[0]
        ));
    }
    let mut a = Args {
        file: argv[1].clone(),
        rate: 20_000_000,
        format: "int8".into(),
        center_hz: 855_500_000.0,
        freqs: Vec::new(),
        mode: DemodMode::Lsm,
        taps_per_channel: trunkd_p25::dsp::channelizer::DEFAULT_TAPS_PER_CHANNEL,
        shift_hz: 0.0,
        quiet: false,
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
            "--center" => a.center_hz = val()?.parse().map_err(|e| format!("--center: {e}"))?,
            "--freq" => a
                .freqs
                .push(val()?.parse().map_err(|e| format!("--freq: {e}"))?),
            "--taps-per-channel" => {
                a.taps_per_channel = val()?
                    .parse()
                    .map_err(|e| format!("--taps-per-channel: {e}"))?
            }
            "--shift" => a.shift_hz = val()?.parse().map_err(|e| format!("--shift: {e}"))?,
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
    if a.freqs.is_empty() {
        return Err("at least one --freq is required".into());
    }
    Ok(a)
}

/// Per-channel event tallies (the parity currency against the DDC path).
#[derive(Default)]
struct Counts {
    sync: u64,
    sync_loss: u64,
    nid: u64,
    tsbk: u64,
    tsbk_failed: u64,
    grant: u64,
    link_control: u64,
    encryption_sync: u64,
    imbe: u64,
    lsd: u64,
    hdu: u64,
    tdu: u64,
    tdulc: u64,
    grants: BTreeSet<(u64, u16, u32)>,
    idens: BTreeMap<u8, (u64, u32, i64, u32)>,
}

impl Counts {
    fn observe(&mut self, e: &P25Event) {
        match e {
            P25Event::SyncDetected { .. } => self.sync += 1,
            P25Event::SyncLoss { .. } => self.sync_loss += 1,
            P25Event::NidDecoded { .. } => self.nid += 1,
            P25Event::Tsbk { .. } => self.tsbk += 1,
            P25Event::TsbkFailed => self.tsbk_failed += 1,
            P25Event::LinkControl { .. } => self.link_control += 1,
            P25Event::EncryptionSync { .. } => self.encryption_sync += 1,
            P25Event::ImbeFrame { .. } => self.imbe += 1,
            P25Event::LowSpeedData { .. } => self.lsd += 1,
            P25Event::Hdu { .. } => self.hdu += 1,
            P25Event::Tdu => self.tdu += 1,
            P25Event::TduLc { .. } => self.tdulc += 1,
            P25Event::Grant {
                frequency_hz,
                talkgroup,
                source,
                ..
            } => {
                self.grant += 1;
                self.grants.insert((*frequency_hz, *talkgroup, *source));
            }
            P25Event::IdenUpdate {
                id,
                base_hz,
                spacing_hz,
                offset_hz,
                bandwidth_hz,
            } => {
                self.idens
                    .insert(*id, (*base_hz, *spacing_hz, *offset_hz, *bandwidth_hz));
            }
            _ => {}
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

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

fn event_json(e: &P25Event) -> String {
    match e {
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
            tsbk_kind(&tsbk.message),
        ),
        P25Event::TsbkFailed => r#"{"type":"tsbk_failed"}"#.to_string(),
        P25Event::PduHeader { header, corrected } => format!(
            r#"{{"type":"pdu_header","format":{},"llid":{},"blocks":{},"confirmed":{},"outbound":{},"corrected":{}}}"#,
            header.format,
            header.llid,
            header.blocks_to_follow,
            header.confirmed,
            header.outbound,
            corrected
        ),
        P25Event::Pdu {
            header,
            blocks,
            valid,
            corrected,
            payload,
        } => {
            let hex: String = payload.iter().map(|b| format!("{b:02x}")).collect();
            let lrrp_tp = trunkd_p25::lrrp::contains_lrrp_transport(payload);
            let loc = match trunkd_p25::lrrp::extract_lrrp(payload) {
                Some(f) => format!(
                    r#","location":{{"lat":{:.6},"lon":{:.6},"via_icmp":{},"token_guided":{}}}"#,
                    f.latitude, f.longitude, f.via_icmp, f.token_guided
                ),
                None => String::new(),
            };
            format!(
                r#"{{"type":"pdu","format":{},"llid":{},"blocks":{},"valid":{},"outbound":{},"corrected":{},"lrrp_transport":{},"payload":"{hex}"{loc}}}"#,
                header.format, header.llid, blocks, valid, header.outbound, corrected, lrrp_tp
            )
        }
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
        } => format!(
            r#"{{"type":"lsd","data":"{}","errors":[{},{}],"valid":[{},{}]}}"#,
            hex(data),
            errors[0],
            errors[1],
            valid[0],
            valid[1]
        ),
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
        P25Event::FrequencyError { hz } => format!(r#"{{"type":"freq_error","hz":{hz:.1}}}"#),
        // Recorder-generated, never produced by a decode of a capture file.
        P25Event::ClockResync { correction_ms } => {
            format!(r#"{{"type":"clock_resync","correction_ms":{correction_ms}}}"#)
        }
    }
}

struct Chan {
    freq_hz: u64,
    handle: ChannelHandle,
    decoder: P25P1Channel,
    counts: Counts,
    /// Observed site channel plan for this channel (`--inventory`).
    inventory: SiteInventory,
    /// Reused per read chunk; capacity settles after the first drain.
    samples: Vec<Complex<f32>>,
}

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };

    let mut channelizer = match PolyphaseChannelizerM2::with_taps(
        args.rate,
        args.center_hz,
        args.taps_per_channel,
        trunkd_p25::dsp::channelizer::DEFAULT_STOPBAND_DB,
    ) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("channelizer: {e}");
            std::process::exit(2);
        }
    };
    let out_rate = channelizer.output_rate();
    eprintln!(
        "channelizer: {} bins, {} taps/channel, bin spacing {:.1} Hz, output {} Hz",
        channelizer.bin_count(),
        channelizer.taps_per_channel(),
        channelizer.bin_spacing_hz(),
        out_rate
    );

    let mut chans: Vec<Chan> = Vec::new();
    for &f in &args.freqs {
        let handle = match channelizer.subscribe(f as f64) {
            Ok(h) => h,
            Err(e) => {
                eprintln!("subscribe {f}: {e}");
                std::process::exit(2);
            }
        };
        let decoder = match P25P1Channel::new(out_rate, args.mode, f) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("engine {f}: {e}");
                std::process::exit(2);
            }
        };
        eprintln!("  {f} Hz -> {:?}", channelizer.selection(handle).unwrap());
        chans.push(Chan {
            freq_hz: f,
            handle,
            decoder,
            counts: Counts::default(),
            inventory: SiteInventory::new(vec![f]),
            samples: Vec::with_capacity(4096),
        });
    }

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
    let mut wb_buf: Vec<Complex<f32>> = Vec::with_capacity(1 << 17);
    let mut leftover = 0usize;
    let mut total_samples = 0u64;
    // Optional pre-mix (recursive NCO, same shape as `Ddc`'s): only used when
    // `--shift` is non-zero.
    let mut osc = Complex::new(1.0f64, 0.0);
    let shift_step = {
        let w = std::f64::consts::TAU * args.shift_hz / f64::from(args.rate);
        Complex::new(w.cos(), w.sin())
    };
    let mut since_renorm = 0u32;
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
        if args.shift_hz != 0.0 {
            for s in wb_buf.iter_mut() {
                *s *= Complex::new(osc.re as f32, osc.im as f32);
                osc *= shift_step;
                since_renorm += 1;
                if since_renorm >= 2048 {
                    osc /= osc.norm();
                    since_renorm = 0;
                }
            }
        }
        total_samples += wb_buf.len() as u64;
        // The example's clock: input sample count, at chunk granularity.
        let now_ms = total_samples * 1000 / u64::from(args.rate);
        channelizer.process_block(&wb_buf);
        for c in chans.iter_mut() {
            c.samples.clear();
            channelizer.drain(c.handle, &mut c.samples);
            let counts = &mut c.counts;
            let inventory = &mut c.inventory;
            let freq = c.freq_hz;
            c.decoder.process(&c.samples, &mut |e: P25Event| {
                counts.observe(&e);
                inventory.observe(freq, &e, now_ms);
                if !quiet {
                    let body = event_json(&e);
                    println!(r#"{{"ch":{freq},{}"#, &body[1..]);
                }
            });
        }
        let rem = n - usable;
        raw.copy_within(usable..n, 0);
        leftover = rem;
    }

    let elapsed = started.elapsed().as_secs_f64();
    let capture_secs = total_samples as f64 / f64::from(args.rate);
    let now_ms = total_samples * 1000 / u64::from(args.rate);

    for c in &chans {
        let s = c.decoder.stats();
        let grants = c
            .counts
            .grants
            .iter()
            .map(|(f, tg, src)| format!(r#"{{"freq":{f},"talkgroup":{tg},"source":{src}}}"#))
            .collect::<Vec<_>>()
            .join(",");
        let idens = c
            .counts
            .idens
            .iter()
            .map(|(id, (base, spacing, offset, bw))| {
                format!(
                    r#"{{"id":{id},"base":{base},"spacing":{spacing},"offset":{offset},"bandwidth":{bw}}}"#
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        let mut nac_hist: Vec<(u16, u64)> = s.nac_histogram.clone();
        nac_hist.sort();
        let nac_json = nac_hist
            .iter()
            .map(|(nac, n)| format!(r#""{nac}":{n}"#))
            .collect::<Vec<_>>()
            .join(",");
        println!(
            r#"{{"summary":{{"freq":{},"sync_count":{},"nid_count":{},"nac_histogram":{{{}}},"tsbk_count":{},"tsbk_failed":{},"grant_count":{},"link_control":{},"encryption_sync":{},"imbe":{},"lsd":{},"hdu":{},"tdu":{},"tdulc":{},"sync_loss":{},"corrected_bits_total":{},"grants":[{}],"iden":[{}]}}}}"#,
            c.freq_hz,
            s.sync_count,
            s.nid_count,
            nac_json,
            s.tsbk_count,
            c.counts.tsbk_failed,
            c.counts.grant,
            c.counts.link_control,
            c.counts.encryption_sync,
            c.counts.imbe,
            c.counts.lsd,
            c.counts.hdu,
            c.counts.tdu,
            c.counts.tdulc,
            c.counts.sync_loss,
            s.corrected_bits,
            grants,
            idens,
        );
        if args.inventory {
            match serde_json::to_string(&c.inventory.snapshot(now_ms)) {
                Ok(j) => println!(r#"{{"ch":{},"inventory":{j}}}"#, c.freq_hz),
                Err(e) => eprintln!("inventory: {e}"),
            }
        }
    }

    println!(
        r#"{{"throughput":{{"channels":{},"input_samples":{},"capture_seconds":{:.3},"wall_seconds":{:.3},"times_realtime":{:.3}}}}}"#,
        chans.len(),
        total_samples,
        capture_secs,
        elapsed,
        capture_secs / elapsed,
    );
}
