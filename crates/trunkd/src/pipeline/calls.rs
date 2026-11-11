// SPDX-License-Identifier: GPL-3.0-or-later
//! The call thread: IMBE frames in, finished calls out.
//!
//! One [`CallBuilder`] and one [`ImbeDecoder`] per open call. Vocoding happens
//! here rather than on the DSP thread (it is ~1 ms of work per 20 ms frame and
//! must never sit between the ring and the channelizer) and rather than on the
//! uplink thread (which is allowed to block on the network for 30 s at a time).
//!
//! # Encryption
//!
//! P25 encrypts only the IMBE payload; the HDU, Link Control and Encryption
//! Sync are always in the clear. So an encrypted call is still assembled in
//! full — talkgroup, radios, timing, algorithm, key id — and only the voice is
//! dropped. Frames are discarded **as they arrive**, the moment
//! [`CallMsg::Encryption`] reports an ALGID other than `0x80`: decoding
//! ciphertext through the vocoder would burn CPU to produce noise, and
//! `CallBuilder::finish` would throw the result away regardless.
//!
//! A call that reports nothing identifying and carries almost no voice is a
//! fragment, not a call, and is dropped rather than uploaded — see
//! [`is_worth_keeping`].
//!
//! # Calls with no voice at all
//!
//! A grant the traffic channel never delivered ends as a properly identified
//! call carrying zero voice frames, so it clears [`is_worth_keeping`] and used
//! to be encoded anyway: 288 bytes of LAME header and no audio, which Keyup's
//! decoder rejects. `[recorder] min_voice_frames` (default 1) is the floor;
//! below it the call is counted (`calls.discarded_empty`) and not encoded.
//! Only the audio is dropped — the observation and radio events for the call
//! come off the DSP side on their own path and are untouched.
//!
//! # Calls whose end never arrives
//!
//! The DSP → call-thread queue is drop-oldest: a call thread that falls far
//! enough behind loses the oldest messages rather than stalling a DSP thread,
//! and each one lost is counted in [`QUEUE_DROPPED`]. When the lost message is
//! a call's `Ended`, nothing else would ever close that call: it would keep its
//! vocoder, up to half an hour of PCM, its Activity row and its recorder-link
//! claim for the rest of the run. So the thread sweeps its open calls, and one
//! that has had no message at all for the orphan bound (see [`run`]) is closed
//! with reason `orphaned` ([`ORPHANED`]) — its claim released, and what it has
//! delivered under exactly the rules a normal end applies.
//!
//! # Conventional (analog) calls
//!
//! A `[[channel]]`'s calls arrive as [`CallMsg::ConvStarted`], `ConvPcm` and
//! `ConvEnded`, already demodulated to 8 kHz PCM on the DSP thread, so there is
//! no vocoder here: the PCM is appended, published to the live monitor in
//! 20 ms slices, and the call is filed per the keyup upload contract — a group call under the
//! channel's system, site, talkgroup and name, with the analog extras
//! ([`trunkd_call::ConventionalInfo`]). One call is open per channel at a
//! time; a `ConvStarted` arriving while one is still open means its `ConvEnded`
//! was evicted, so the old call is closed as orphaned first. The orphan sweep
//! covers them like any other call. They are counted under
//! `calls.conventional_*`, apart from the P25 totals.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use trunkd_call::{CallBuilder, ConventionalInfo, FinishedCall};
use trunkd_core::chan::BoundedReceiver;
use trunkd_mbe::ImbeDecoder;
use trunkd_p25::call::CallRecord;
use trunkd_p25::event::CallKind;
use trunkd_p25::payload::algorithm_name;
use trunkd_p25::site::{CallId, EndReason};

use crate::pipeline::{CallMsg, Shared};

/// ALGID for "not encrypted" (spec §18.2).
const ALGID_CLEAR: u8 = 0x80;

/// Counter: messages the DSP → call-thread queue evicted to make room.
pub(crate) const QUEUE_DROPPED: &str = "calls.queue_dropped";

/// Counter: open calls closed by the sweep because their end never arrived.
pub(crate) const ORPHANED: &str = "calls.orphaned";

/// The Activity view's spelling of a call closed by the orphan sweep.
const ORPHANED_REASON: &str = "orphaned";

/// The shortest orphan bound the recorder will run with.
///
/// The engine itself closes a silent call after `hang_ms +
/// call_inactivity_ms` (a few seconds), so a call that has had no message at
/// all for a minute has lost its `Ended`, not paused. The recorder raises the
/// bound further when a site is configured with a long inactivity timer.
pub(crate) const MIN_ORPHAN_AFTER: Duration = Duration::from_secs(60);

/// Whether an announced (algid, key id) pair means the call is enciphered.
///
/// This has to agree with the engine's rule
/// ([`trunkd_p25::payload::is_clear_params`]), and once did not. The engine
/// treats an **all-zero encryption word** as "no parameters were sent", not as
/// the ACCORDION cipher; this thread tested `algid != 0x80` and so called
/// `algid == 0` encrypted. The two then disagreed with real consequences: the
/// voice was discarded here as ciphertext, while the engine's `CallRecord` said
/// `encrypted: false`, and `apply_record` stamped that back onto the builder.
/// The call was uploaded as **clear with no audio**, and Keyup answered
/// `400 incomplete call data: no audio` — three times on 2026-08-31, on
/// talkgroups 50061, 41601 and 57012.
///
/// The MI is not on this path (only algid and key id cross the queue), so the
/// all-zero case is recognised by `algid == 0 && key_id == 0`, which is the
/// reachable half of the engine's rule.
fn is_enciphered(algid: u8, key_id: u16) -> bool {
    algid != ALGID_CLEAR && !(algid == 0 && key_id == 0)
}

/// Voice frames a call must carry before it counts as a call on its own.
///
/// Mirrors [`CallRecord::is_substantive`]'s own threshold: 8 frames is 160 ms,
/// below which a burst between two calls is noise rather than speech.
const MIN_VOICE_FRAMES: u32 = 8;

/// The Activity view's spelling of a call kind — the same words Keyup's
/// `call_kind` uses, plus the telephone kind the uplink never sees.
fn kind_name(kind: CallKind) -> &'static str {
    match kind {
        CallKind::Group => "group",
        CallKind::UnitToUnit => "unit",
        CallKind::Telephone => "telephone",
    }
}

/// The Activity view's spelling of why a call ended.
fn reason_name(reason: EndReason) -> &'static str {
    match reason {
        EndReason::Terminator => "terminator",
        EndReason::Inactivity => "inactivity",
        EndReason::PoolReassigned => "pool reassigned",
        EndReason::Shutdown => "shutdown",
    }
}

/// Where a finished call goes.
pub(crate) trait CallDelivery: Send {
    /// Deliver one finished call. Must not block for long.
    fn deliver(&self, call: FinishedCall);
}

/// One call being assembled.
struct OpenCall {
    builder: CallBuilder,
    vocoder: ImbeDecoder,
    /// True once the control channel or an HDU reported a real algorithm.
    encrypted: bool,
    /// True for a unit-to-unit (private) call, so its end is counted apart
    /// from the group traffic.
    unit: bool,
    /// Voice frames seen, including the ones discarded as encrypted.
    voice_frames: u32,
    /// Talker alias last reported, kept so `CallEnded` can fill it in even if
    /// the engine's own record missed the last block.
    alias: Option<String>,
    /// Radio the kept `alias` belongs to. A call can change talkers, and the
    /// engine reports the new one with `alias: None` until its own blocks
    /// arrive; without this the previous talker's alias would be the one the
    /// call is uploaded with.
    alias_of: Option<u32>,
    /// How this call's audio is announced to the live monitor, built once at
    /// grant. `None` unless `[server] live_audio` is on.
    monitor: Option<crate::pipeline::monitor::CallAudioMeta>,
    /// The recording device's wideband level gauge, read once at teardown.
    ///
    /// Read at *call end* rather than at grant: it is the level the receiver
    /// was seeing while the call was actually on the air, and reading a live
    /// handle costs one atomic load instead of a per-frame sample.
    rssi: Option<trunkd_core::Metric>,
    /// When the last message for this call arrived, for the orphan sweep.
    last_seen: Instant,
    /// What the grant and the talker updates said about the call — the
    /// stand-in for the engine's own record when its `Ended` was lost.
    known: CallRecord,
}

/// One conventional call being assembled.
struct ConvCall {
    /// Call id, as the DSP thread tagged it.
    id: u64,
    /// The channel and its labels.
    spec: std::sync::Arc<crate::pipeline::conventional::ChannelSpec>,
    builder: CallBuilder,
    /// The analog extras so far; the end measurements land at `ConvEnded`.
    info: ConventionalInfo,
    /// Live-monitor identity, when `[server] live_audio` is on.
    monitor: Option<crate::pipeline::monitor::CallAudioMeta>,
    /// The device's wideband level gauge, read at call end.
    rssi: Option<trunkd_core::Metric>,
    /// When the last message for this call arrived, for the orphan sweep.
    last_seen: Instant,
}

/// The Activity view's spelling of why a conventional call ended.
fn conv_reason_name(reason: trunkd_analog::EndReason) -> &'static str {
    match reason {
        trunkd_analog::EndReason::SquelchClosed => "squelch closed",
        trunkd_analog::EndReason::ToneLost => "tone lost",
        trunkd_analog::EndReason::MaxDuration => "max duration",
        trunkd_analog::EndReason::Shutdown => "shutdown",
    }
}

/// The wideband level a device's gauge is currently reporting, or `None` when
/// it has never published one.
///
/// A gauge is registered when its DSP thread starts and only written once the
/// `RssiMeter` has primed, so an unwritten
/// gauge reads as raw zero — and 0 dBFS is *full scale*, the loudest reading
/// there is. Reporting that as a measurement would tell a best-source
/// negotiation this recorder was the strongest of the lot precisely when it had
/// measured nothing at all, so raw zero is treated as "not measured".
fn rssi_dbfs(gauge: Option<&trunkd_core::Metric>) -> Option<f64> {
    let raw = gauge?.get();
    (raw != 0).then(|| f64::from_bits(raw))
}

/// Run the call thread until the event channel closes.
///
/// Returns when the DSP side has dropped its sender *and* the queue is drained,
/// which is what makes an orderly shutdown lossless.
///
/// `orphan_after` is how long an open call may go without a single message
/// before it is presumed to have lost its `Ended` and is closed as `orphaned`
/// (module docs). The recorder passes at least [`MIN_ORPHAN_AFTER`].
// Nine, two past clippy's taste. They are nine *unrelated* facts a call
// thread needs — its queue, the shared registry, where finished calls go, three
// thresholds, this instance's id, the peer link, the monitor — and bundling
// them into a struct would only move the list somewhere with less context.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run(
    rx: BoundedReceiver<CallMsg>,
    shared: Shared,
    delivery: Box<dyn CallDelivery>,
    max_call_duration: std::time::Duration,
    // Fewest voice frames a call must carry to be encoded at all
    // (`[recorder] min_voice_frames`). See [`CallMsg::Ended`] below.
    min_voice_frames: u32,
    orphan_after: Duration,
    instance_id: Option<String>,
    // The recorder link and this process's tuner keys in config order, when the
    // link is running. Every call a tuner here records is claimed to the peers
    // and released when it ends, so a peer can see the call is covered — the
    // one place in the pipeline that knows both facts.
    link: Option<(std::sync::Arc<crate::pipeline::link::LinkSlot>, Vec<String>)>,
    // The live-audio fan-out, when `[server] live_audio` built one. `None` in
    // every default deployment, which is what makes the publish below free.
    monitor: Option<std::sync::Arc<crate::pipeline::monitor::MonitorHub>>,
) {
    let mut open: HashMap<(usize, CallId), OpenCall> = HashMap::new();
    // Conventional calls, by channel index: at most one open per channel.
    let mut conv: HashMap<usize, ConvCall> = HashMap::new();
    let started = shared.metrics.counter("calls.started");
    let ended = shared.metrics.counter("calls.ended");
    let dropped = shared.metrics.counter("calls.dropped_insubstantive");
    let no_audio = shared.metrics.counter("calls.dropped_no_audio");
    let discarded_empty = shared.metrics.counter("calls.discarded_empty");
    // Private calls, counted apart from the group traffic they compete with:
    // they are the one thing on this site the recorder used to refuse, so
    // "is it following them?" has to be answerable from the status line.
    let unit_started = shared.metrics.counter("calls.unit_started");
    let unit_ended = shared.metrics.counter("calls.unit_ended");
    let voice = shared.metrics.counter("calls.voice_frames");
    let encrypted_frames = shared.metrics.counter("calls.encrypted_frames_discarded");
    let orphaned = shared.metrics.counter("calls.orphaned_events");
    let orphaned_calls = shared.metrics.counter(ORPHANED);
    // Registered up front so the status line and `/api/status` show a 0
    // rather than nothing on a recorder that has never dropped a message.
    let _ = shared.metrics.counter(QUEUE_DROPPED);

    // Everything that happens to a call once it is over, however it ended: the
    // Activity row, the private-call row, and the "is this worth keeping"
    // gates in front of the uplink. The link claim is released by the caller.
    let finish = |site: usize,
                  id: CallId,
                  mut call: OpenCall,
                  record: &CallRecord,
                  end_ms: i64,
                  reason: &'static str| {
        ended.inc();
        if call.unit {
            unit_ended.inc();
            shared.private_calls.ended(id.0, end_ms);
        }
        // Closed on the Activity ring before any of the "is this worth
        // keeping" tests below, so a call the recorder discards is still
        // visible as traffic that went by — with the outcome stamped on at
        // whichever of those tests claims it.
        shared.calls.ended(id.0, end_ms, call.voice_frames, reason);
        let spec = &shared.sites[site];
        apply_record(&mut call, record);

        // A call with no voice on it at all: the signaling ran its course but
        // the traffic channel delivered nothing. There is no audio to encode —
        // a LAME header and a flush is 288 bytes of nothing, which Keyup's
        // decoder then fails on — so it is not encoded and not uploaded.
        //
        // Only the *audio* is dropped. The call-ended observation and the
        // radio events come off the DSP side on their own path, so Keyup's
        // control-channel picture is exactly as it was.
        if call.voice_frames < min_voice_frames {
            discarded_empty.inc();
            shared.calls.outcome(id.0, "no voice");
            tracing::debug!(
                site = %spec.display,
                call = id.0,
                talkgroup = record.talkgroup.unwrap_or(0),
                voice_frames = call.voice_frames,
                min_voice_frames,
                reason,
                "not encoding a call that carried no voice"
            );
            return;
        }

        if !is_worth_keeping(record, call.voice_frames) {
            dropped.inc();
            shared.calls.outcome(id.0, "dropped");
            tracing::debug!(
                site = %spec.display,
                call = id.0,
                voice_frames = call.voice_frames,
                reason,
                "dropping a call with no identity and almost no voice"
            );
            return;
        }

        // The receiver's wideband level as the call ends — context for the
        // frame counts, not a measurement of this channel.
        let level = rssi_dbfs(call.rssi.as_ref());
        call.builder.set_rssi_dbfs(level);
        let finished = call.builder.finish(end_ms);

        // A clear call with no audio is one Keyup can only reject (`400
        // incomplete call data: no audio`), so it is not sent. An *encrypted*
        // call legitimately carries none — P25 enciphers the IMBE payload
        // only, and the metadata is the point.
        if !finished.encrypted && finished.pcm.is_empty() {
            no_audio.inc();
            shared.calls.outcome(id.0, "no audio");
            tracing::info!(
                site = %spec.display,
                call = id.0,
                talkgroup = finished.talkgroup,
                source = finished.source.unwrap_or(0),
                voice_frames = call.voice_frames,
                duration_s = format_args!("{:.1}", finished.duration_s()),
                reason,
                "not uploading a clear call with no audio"
            );
            // Frames counted but no PCM means the vocoder path was skipped
            // while the call was still considered clear — the encryption-latch
            // disagreement this module used to have. It should now be
            // unreachable; say so loudly if it is not.
            if call.voice_frames > 0 {
                tracing::warn!(
                    site = %spec.display,
                    call = id.0,
                    voice_frames = call.voice_frames,
                    "clear call decoded voice frames but produced no audio"
                );
            }
            return;
        }

        tracing::info!(
            site = %spec.display,
            call = id.0,
            talkgroup = finished.talkgroup,
            frequency_hz = finished.frequency_hz,
            duration_s = format_args!("{:.1}", finished.duration_s()),
            voice_frames = call.voice_frames,
            encrypted = finished.encrypted,
            alias = finished.talker_alias.as_deref().unwrap_or(""),
            reason,
            "call ended"
        );
        shared.calls.outcome(id.0, "delivered");
        delivery.deliver(finished);
    };

    // Conventional counters, apart from the P25 totals above. Resolved lazily
    // by name on these rare paths only, so a recorder with no channel never
    // registers them.
    let conv_counter = |name: &str| {
        shared
            .metrics
            .counter(&format!("calls.conventional_{name}"))
    };

    // A conventional call is over, however it ended: Activity row, extras,
    // and the one gate that applies to demodulated audio (it must have some).
    let finish_conv = |mut call: ConvCall, end_ms: i64, reason: &'static str| {
        conv_counter("ended").inc();
        let frames = (call.builder.sample_count() / 160) as u32;
        shared.calls.ended(call.id, end_ms, frames, reason);
        call.builder.set_conventional(Some(call.info));
        call.builder.set_rssi_dbfs(rssi_dbfs(call.rssi.as_ref()));
        let finished = call.builder.finish(end_ms);
        if finished.pcm.is_empty() {
            conv_counter("no_audio").inc();
            shared.calls.outcome(call.id, "no audio");
            tracing::debug!(channel = %call.spec.display, reason, "not uploading a conventional call with no audio");
            return;
        }
        tracing::info!(
            channel = %call.spec.display,
            talkgroup = finished.talkgroup,
            frequency_hz = finished.frequency_hz,
            duration_s = format_args!("{:.1}", finished.duration_s()),
            reason,
            "conventional call ended"
        );
        conv_counter("delivered").inc();
        shared.calls.outcome(call.id, "delivered");
        delivery.deliver(finished);
    };
    // The end of an orphaned call: when its last message arrived.
    let orphan_end_ms = |last_seen: Instant, now: Instant| {
        let idle = now.duration_since(last_seen);
        trunkd_core::Timestamp::now().unix_millis()
            - i64::try_from(idle.as_millis()).unwrap_or(i64::MAX)
    };

    // The sweep runs on its own cadence whether or not messages are flowing:
    // a busy queue never times out, and a busy queue is exactly when an
    // `Ended` gets evicted.
    let orphan_after = orphan_after.max(Duration::from_millis(1));
    let sweep_every = (orphan_after / 4).clamp(Duration::from_millis(5), Duration::from_secs(5));
    let mut next_sweep = Instant::now() + sweep_every;

    loop {
        let msg = match rx.recv_wait(sweep_every) {
            Ok(msg) => Some(msg),
            Err(trunkd_core::chan::RecvTimeoutError::Timeout) => None,
            Err(trunkd_core::chan::RecvTimeoutError::Disconnected) => break,
        };
        let now = Instant::now();
        if now >= next_sweep {
            next_sweep = now + sweep_every;
            let lost: Vec<(usize, CallId)> = open
                .iter()
                .filter(|(_, c)| now.duration_since(c.last_seen) >= orphan_after)
                .map(|(key, _)| *key)
                .collect();
            for (site, id) in lost {
                let Some(mut call) = open.remove(&(site, id)) else {
                    continue;
                };
                orphaned_calls.inc();
                if let Some((link, _)) = &link {
                    link.on_call_ended(site, id.0);
                }
                // The call went quiet when its last message arrived, so that
                // is its end, not the moment the sweep noticed.
                let idle = now.duration_since(call.last_seen);
                let end_ms = trunkd_core::Timestamp::now().unix_millis()
                    - i64::try_from(idle.as_millis()).unwrap_or(i64::MAX);
                tracing::warn!(
                    site = %shared.sites[site].display,
                    call = id.0,
                    idle_s = idle.as_secs(),
                    voice_frames = call.voice_frames,
                    queue_dropped = shared.metrics.counter(QUEUE_DROPPED).get(),
                    "closing a call whose end never arrived (orphaned)"
                );
                let known = std::mem::take(&mut call.known);
                finish(site, id, call, &known, end_ms, ORPHANED_REASON);
            }
            // Conventional calls the same way; an empty map on a recorder
            // with no channel.
            let lost: Vec<usize> = conv
                .iter()
                .filter(|(_, c)| now.duration_since(c.last_seen) >= orphan_after)
                .map(|(k, _)| *k)
                .collect();
            for channel in lost {
                let Some(mut call) = conv.remove(&channel) else {
                    continue;
                };
                orphaned_calls.inc();
                tracing::warn!(
                    channel = %call.spec.display,
                    call = call.id,
                    queue_dropped = shared.metrics.counter(QUEUE_DROPPED).get(),
                    "closing a conventional call whose end never arrived (orphaned)"
                );
                call.info.squelch_reason = Some(ORPHANED_REASON.to_owned());
                let end_ms = orphan_end_ms(call.last_seen, now);
                finish_conv(call, end_ms, ORPHANED_REASON);
            }
        }
        let Some(msg) = msg else {
            continue;
        };
        match msg {
            CallMsg::Started {
                site,
                id,
                frequency_hz,
                talkgroup,
                target,
                source,
                kind,
                patches,
                rssi,
                at_ms,
            } => {
                let spec = &shared.sites[site];
                // Tell the peers this one is covered here. The call id is
                // namespaced by tuner (`call_id_base`), so its top bits name the
                // tuner that is on it without threading a device index through
                // every message.
                let tuner = link
                    .as_ref()
                    .and_then(|(_, keys)| keys.get((id.0 >> 48) as usize).map(String::as_str));
                if let Some((link, _)) = &link {
                    link.on_call_started(site, id.0, frequency_hz, talkgroup, tuner.unwrap_or(""));
                }
                // `at_ms` is already wall-clock: the device's DSP thread mapped
                // it through that tuner's epoch before enqueueing.
                let start_ms = at_ms as i64;
                // A group call is filed under its talkgroup; a unit-to-unit or
                // telephone call has no talkgroup that fits 16 bits, so the
                // full 24-bit TO address stands in — the same identifier Keyup
                // will show for it.
                let tg = match kind {
                    CallKind::Group => u32::from(talkgroup),
                    _ => target,
                };
                let mut builder = CallBuilder::new(
                    spec.system_label.clone(),
                    spec.site_label.clone(),
                    start_ms,
                    frequency_hz,
                    tg,
                )
                .with_max_duration(max_call_duration);
                // A group call on a live supergroup carries the members hearing
                // it; the call is filed under the supergroup (its `tg`) and the
                // members ride the `patches` field to Keyup (§1.6).
                if !patches.is_empty() {
                    builder.set_patches(patches);
                }
                if source != 0 {
                    builder.talker(source, start_ms, false, None);
                }
                // A unit-to-unit call is filed under the two radios: no
                // talkgroup goes on the wire, `target` is the called radio,
                // and the file is named `UNIT_<A>_TO_<B>`.
                if kind == CallKind::UnitToUnit {
                    builder.set_unit_call(target);
                    unit_started.inc();
                    shared
                        .private_calls
                        .started(id.0, source, target, frequency_hz, start_ms);
                }
                // Every call, private or not, goes on the Activity ring the
                // panel polls: a grant is on the air the moment it is granted,
                // so the row is made here rather than at teardown.
                shared.calls.started(crate::pipeline::CallStart {
                    id: id.0,
                    site: spec.display.clone(),
                    kind: kind_name(kind),
                    talkgroup: (kind == CallKind::Group).then_some(tg),
                    source,
                    target: (kind == CallKind::UnitToUnit).then_some(target),
                    frequency_hz,
                    started_at_ms: start_ms,
                });
                // Which recorder this copy came from. Several trunkd instances
                // can hear one site, so the archive needs to know two copies of
                // a call are two receivers and not one duplicate.
                builder.set_instance(instance_id.clone());
                tracing::debug!(
                    site = %spec.display,
                    call = id.0,
                    frequency_hz,
                    talkgroup = tg,
                    source,
                    kind = ?kind,
                    "call started"
                );
                started.inc();
                // Built once per call rather than per frame: the monitor's
                // per-frame cost is then an `Arc` clone, and nothing at all
                // when no hub exists.
                let monitor_meta = monitor.as_ref().map(|_| {
                    crate::pipeline::monitor::CallAudioMeta::new(
                        id.0,
                        (kind == CallKind::Group).then_some(tg),
                        kind,
                        source,
                        tuner,
                        frequency_hz,
                    )
                });
                open.insert(
                    (site, id),
                    OpenCall {
                        builder,
                        vocoder: ImbeDecoder::new(),
                        encrypted: false,
                        unit: kind == CallKind::UnitToUnit,
                        voice_frames: 0,
                        alias: None,
                        alias_of: None,
                        monitor: monitor_meta,
                        rssi,
                        last_seen: Instant::now(),
                        known: CallRecord {
                            talkgroup: (kind == CallKind::Group && talkgroup != 0)
                                .then_some(talkgroup),
                            source: (source != 0).then_some(source),
                            target: (kind != CallKind::Group && target != 0).then_some(target),
                            ..CallRecord::default()
                        },
                    },
                );
            }

            CallMsg::Talker {
                site,
                id,
                source,
                emergency,
                alias,
                source_wacn,
                source_system,
                at_ms,
            } => {
                let Some(call) = open.get_mut(&(site, id)) else {
                    orphaned.inc();
                    continue;
                };
                call.last_seen = Instant::now();
                if source != 0 {
                    call.known.source = Some(source);
                }
                let at = at_ms as i64;
                // A talker change retires the previous talker's alias: the
                // engine reports the new talker with `alias: None` and only
                // names it once its own alias blocks have all arrived. Radio 0
                // is "not named yet", never a new talker, so it retires
                // nothing — and `CallBuilder::talker` keeps it out of `sources`.
                if source != 0 && call.alias_of.is_some_and(|prev| prev != source) {
                    call.alias = None;
                    call.builder.set_talker_alias(None);
                }
                if alias.is_some() {
                    call.alias = alias.clone();
                    call.alias_of = Some(source);
                    call.builder.set_talker_alias(alias.clone());
                }
                shared.calls.talker(id.0, source, alias.as_deref());
                call.builder.set_source_suid(source_wacn, source_system);
                call.builder.talker(source, at, emergency, alias);
                if emergency {
                    call.builder.set_flags(true, call.encrypted);
                }
            }

            CallMsg::Voice { site, id, frame } => {
                let Some(call) = open.get_mut(&(site, id)) else {
                    orphaned.inc();
                    continue;
                };
                call.last_seen = Instant::now();
                call.voice_frames += 1;
                voice.inc();
                if call.encrypted {
                    // Ciphertext: nothing to synthesize. The frame is still
                    // counted, so the call's duration and substance are right.
                    encrypted_frames.inc();
                    continue;
                }
                let pcm = call.vocoder.decode(&frame);
                // A muted frame is the vocoder's own "I could not recover
                // this": the running error rate was past repair, or the frame
                // had been repeated to the limit. Counting it here is what
                // makes two recorders' copies comparable.
                let muted = call.vocoder.last_frame_muted();
                // The live monitor sees exactly what the call file will: the
                // same synthesized frame, at the same instant, before anything
                // downstream can have changed it. With `live_audio` off both
                // sides of this are `None` and it compiles down to a branch.
                if let (Some(hub), Some(meta)) = (&monitor, &call.monitor) {
                    hub.publish(meta, &pcm);
                }
                call.builder.push_voice_frame(&pcm, muted);
            }

            CallMsg::Encryption {
                site,
                id,
                algid,
                key_id,
            } => {
                let Some(call) = open.get_mut(&(site, id)) else {
                    orphaned.inc();
                    continue;
                };
                call.last_seen = Instant::now();
                let now_encrypted = is_enciphered(algid, key_id);
                if now_encrypted && !call.encrypted {
                    tracing::debug!(
                        call = id.0,
                        algorithm = algorithm_name(algid),
                        key_id,
                        "call is encrypted; discarding voice from here on"
                    );
                }
                // Both latch: a call that was encrypted for one superframe is
                // reported — and has its audio dropped — for the whole call,
                // so once it is set there is nothing to gain by vocoding the
                // rest.
                call.encrypted |= now_encrypted;
                if now_encrypted {
                    shared.calls.encrypted(id.0);
                }
                call.builder.set_flags(false, now_encrypted);
                call.builder.set_algorithm(
                    Some(algorithm_name(algid).to_owned()),
                    (key_id != 0).then_some(key_id),
                );
            }

            CallMsg::Ended {
                site,
                id,
                record,
                at_ms,
                reason,
            } => {
                if let Some((link, _)) = &link {
                    link.on_call_ended(site, id.0);
                }
                let Some(call) = open.remove(&(site, id)) else {
                    orphaned.inc();
                    continue;
                };
                finish(site, id, call, &record, at_ms as i64, reason_name(reason));
            }

            CallMsg::ConvStarted {
                spec,
                id,
                start,
                rssi,
                at_ms,
            } => {
                // A call still open on this channel lost its `ConvEnded` to the
                // queue: close it before the new one takes the slot.
                if let Some(mut old) = conv.remove(&spec.index) {
                    orphaned_calls.inc();
                    tracing::warn!(
                        channel = %old.spec.display,
                        call = old.id,
                        "closing a conventional call whose end never arrived (orphaned)"
                    );
                    old.info.squelch_reason = Some(ORPHANED_REASON.to_owned());
                    let end_ms = orphan_end_ms(old.last_seen, Instant::now());
                    finish_conv(old, end_ms, ORPHANED_REASON);
                }
                let start_ms = at_ms as i64;
                let mut builder = CallBuilder::new(
                    spec.system_label.clone(),
                    spec.site_label.clone(),
                    start_ms,
                    spec.frequency_hz,
                    spec.talkgroup,
                )
                .with_max_duration(max_call_duration);
                builder.set_talkgroup_label(Some(spec.name.clone()));
                builder.set_instance(instance_id.clone());
                shared.calls.started(crate::pipeline::CallStart {
                    id,
                    site: spec.display.clone(),
                    kind: "conventional",
                    talkgroup: Some(spec.talkgroup),
                    source: 0,
                    target: None,
                    frequency_hz: spec.frequency_hz,
                    started_at_ms: start_ms,
                });
                conv_counter("started").inc();
                tracing::debug!(channel = %spec.display, call = id, "conventional call started");
                let monitor_meta = monitor.as_ref().map(|_| {
                    crate::pipeline::monitor::CallAudioMeta::new(
                        id,
                        Some(spec.talkgroup),
                        CallKind::Group,
                        0,
                        Some(spec.device_id.as_str()),
                        spec.frequency_hz,
                    )
                });
                let info = ConventionalInfo {
                    tone: spec.tone.clone(),
                    tone_detected: start.tone_detected.map(|t| t.to_string()),
                    snr_db: Some(start.snr_db),
                    ..ConventionalInfo::nbfm()
                };
                conv.insert(
                    spec.index,
                    ConvCall {
                        id,
                        spec,
                        builder,
                        info,
                        monitor: monitor_meta,
                        rssi,
                        last_seen: Instant::now(),
                    },
                );
            }

            CallMsg::ConvPcm { channel, id, pcm } => {
                let Some(call) = conv.get_mut(&channel).filter(|c| c.id == id) else {
                    orphaned.inc();
                    continue;
                };
                call.last_seen = Instant::now();
                // The monitor takes 20 ms frames, as the IMBE path gives it.
                if let (Some(hub), Some(meta)) = (&monitor, &call.monitor) {
                    for frame in pcm.chunks(trunkd_analog::FRAME_SAMPLES) {
                        hub.publish(meta, frame);
                    }
                }
                call.builder.push_pcm(&pcm);
            }

            CallMsg::ConvEnded {
                channel,
                id,
                end,
                at_ms,
            } => {
                if !conv.get(&channel).is_some_and(|c| c.id == id) {
                    orphaned.inc();
                    continue;
                }
                let Some(mut call) = conv.remove(&channel) else {
                    continue;
                };
                call.info.squelch_reason = Some(end.reason.as_str().to_owned());
                if let Some(t) = end.tone_detected {
                    call.info.tone_detected = Some(t.to_string());
                }
                call.info.snr_db = Some(end.snr_db);
                call.info.peak_deviation_hz = Some(end.peak_deviation_hz);
                call.info.offset_hz = Some(end.offset_hz);
                finish_conv(call, at_ms as i64, conv_reason_name(end.reason));
            }
        }
    }

    // The DSP side is gone. Anything still open was cut off mid-call; the
    // engine's own shutdown normally closes them first, so this is a
    // belt-and-braces flush rather than the usual path.
    let end_ms = trunkd_core::Timestamp::now().unix_millis();
    for ((_, id), mut call) in open.drain() {
        if call.voice_frames >= MIN_VOICE_FRAMES {
            tracing::debug!(call = id.0, "flushing a call left open at shutdown");
            let level = rssi_dbfs(call.rssi.as_ref());
            call.builder.set_rssi_dbfs(level);
            delivery.deliver(call.builder.finish(end_ms));
        }
    }
    // Conventional calls the engines' own `finish` did not close (their
    // `ConvEnded` was lost): delivered with what they have.
    for (_, mut call) in conv.drain() {
        call.info.squelch_reason = Some("shutdown".to_owned());
        finish_conv(call, end_ms, "shutdown");
    }
}

/// Fold the engine's assembled [`CallRecord`] into the builder at teardown.
///
/// The record is the authority on everything the signaling carried, because
/// it saw the whole call including the parts that arrived before the traffic
/// channel produced its first event.
fn apply_record(call: &mut OpenCall, record: &CallRecord) {
    // The encryption flag is a **latch**, and merging it must not unlatch it.
    // `call.encrypted` is why the PCM is empty; a record that disagreed would
    // otherwise produce a call marked clear that carries no audio — which is
    // precisely the upload Keyup rejects.
    call.encrypted |= record.encrypted;
    call.builder.set_flags(record.emergency, call.encrypted);
    if let Some(algid) = record.algid {
        call.builder.set_algorithm(
            Some(algorithm_name(algid).to_owned()),
            record.key_id.filter(|&k| k != 0),
        );
    }
    let alias = record.talker_alias.clone().or_else(|| call.alias.clone());
    if alias.is_some() {
        call.builder.set_talker_alias(alias);
    }
    call.builder
        .set_source_suid(record.source_wacn, record.source_system);
}

/// Whether a finished call is worth uploading.
///
/// The engine reports faithfully, including the stray fragments a traffic
/// channel picks up between calls: a lone terminator, or a handful of voice
/// frames with no HDU and no Link Control behind them. Keyup should not have
/// to store those, so a call is kept only if the signaling identified it
/// ([`CallRecord::is_substantive`]) **or** it carried real speech.
fn is_worth_keeping(record: &CallRecord, voice_frames: u32) -> bool {
    record.is_substantive() || voice_frames >= MIN_VOICE_FRAMES
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::SiteSpec;

    fn record(talkgroup: Option<u16>, voice_frames: u32) -> CallRecord {
        CallRecord {
            talkgroup,
            voice_frames,
            ..CallRecord::default()
        }
    }

    #[test]
    fn an_all_zero_encryption_word_is_not_a_cipher() {
        // The bug this fixes: `algid == 0, key_id == 0` is "no encryption
        // parameters were sent", which the engine's `is_clear_params` has
        // always said. Calling it encrypted here discarded the voice while the
        // record still called the call clear, and Keyup rejected the result
        // with `incomplete call data: no audio`.
        assert!(!is_enciphered(0, 0));
        // Unencrypted, explicitly.
        assert!(!is_enciphered(ALGID_CLEAR, 0));
        assert!(!is_enciphered(ALGID_CLEAR, 1100));
        // Real ciphers stay encrypted.
        assert!(is_enciphered(0x84, 1100), "AES-256");
        assert!(is_enciphered(0xAA, 575), "ADP");
        // A zero algid with a real key id is not the all-zero word.
        assert!(is_enciphered(0, 1100));
    }

    #[test]
    fn the_record_cannot_unlatch_an_encrypted_call() {
        // `apply_record` merges; it must never turn an encrypted call clear,
        // because `call.encrypted` is the reason the PCM is empty.
        let mut call = OpenCall {
            builder: CallBuilder::new("S", "T", 1_788_012_000_000, 851_050_000, 52101),
            vocoder: ImbeDecoder::new(),
            encrypted: true,
            unit: false,
            voice_frames: 9,
            alias: None,
            alias_of: None,
            monitor: None,
            rssi: None,
            last_seen: Instant::now(),
            known: CallRecord::default(),
        };
        let rec = CallRecord {
            encrypted: false,
            ..CallRecord::default()
        };
        apply_record(&mut call, &rec);
        assert!(
            call.encrypted,
            "the live latch wins over a record that missed it"
        );
        let finished = call.builder.finish(1_788_012_001_000);
        assert!(
            finished.encrypted,
            "an encrypted call must not be posted as clear"
        );
    }

    #[test]
    fn a_record_can_still_latch_a_call_encrypted() {
        // The merge runs both ways: a call the live path never saw encrypted
        // still becomes encrypted if the assembler saw it.
        let mut call = OpenCall {
            builder: CallBuilder::new("S", "T", 1_788_012_000_000, 851_050_000, 52101),
            vocoder: ImbeDecoder::new(),
            encrypted: false,
            unit: false,
            voice_frames: 0,
            alias: None,
            alias_of: None,
            monitor: None,
            rssi: None,
            last_seen: Instant::now(),
            known: CallRecord::default(),
        };
        let rec = CallRecord {
            encrypted: true,
            algid: Some(0x84),
            key_id: Some(1100),
            ..CallRecord::default()
        };
        apply_record(&mut call, &rec);
        assert!(call.encrypted);
        assert_eq!(
            call.builder.finish(1_788_012_001_000).algorithm.as_deref(),
            Some("AES-256")
        );
    }

    #[test]
    fn a_call_the_signaling_identified_is_kept() {
        assert!(is_worth_keeping(&record(Some(52101), 0), 0));
    }

    #[test]
    fn an_anonymous_burst_of_speech_is_kept() {
        assert!(is_worth_keeping(&record(None, 0), MIN_VOICE_FRAMES));
    }

    #[test]
    fn a_nameless_fragment_is_dropped() {
        assert!(!is_worth_keeping(&record(None, 0), 0));
        assert!(!is_worth_keeping(&record(None, 0), MIN_VOICE_FRAMES - 1));
    }

    #[test]
    fn the_record_decides_encryption_even_when_no_event_arrived() {
        let mut call = OpenCall {
            builder: CallBuilder::new("S", "T", 0, 851_050_000, 1),
            vocoder: ImbeDecoder::new(),
            encrypted: false,
            unit: false,
            voice_frames: 0,
            alias: None,
            alias_of: None,
            monitor: None,
            rssi: None,
            last_seen: Instant::now(),
            known: CallRecord::default(),
        };
        let rec = CallRecord {
            encrypted: true,
            algid: Some(0x84),
            key_id: Some(7),
            ..CallRecord::default()
        };
        apply_record(&mut call, &rec);
        assert!(call.encrypted);
        let finished = call.builder.finish(1_000);
        assert!(finished.encrypted);
        assert_eq!(finished.algorithm.as_deref(), Some("AES-256"));
        assert_eq!(finished.key_id, Some(7));
        assert!(finished.pcm.is_empty(), "encrypted calls carry no audio");
    }

    #[test]
    fn a_clear_record_leaves_the_audio_alone() {
        let mut call = OpenCall {
            builder: CallBuilder::new("S", "T", 0, 851_050_000, 1),
            vocoder: ImbeDecoder::new(),
            encrypted: false,
            unit: false,
            voice_frames: 0,
            alias: Some("UNIT_A1".into()),
            alias_of: Some(9_000_044),
            monitor: None,
            rssi: None,
            last_seen: Instant::now(),
            known: CallRecord::default(),
        };
        call.builder.push_pcm(&[1i16; 160]);
        apply_record(
            &mut call,
            &CallRecord {
                algid: Some(ALGID_CLEAR),
                ..CallRecord::default()
            },
        );
        let finished = call.builder.finish(1_000);
        assert!(!finished.encrypted);
        assert_eq!(finished.pcm.len(), 160);
        assert_eq!(finished.algorithm.as_deref(), Some("UNENCRYPTED"));
        assert_eq!(
            finished.talker_alias.as_deref(),
            Some("UNIT_A1"),
            "an alias seen live survives a record that never carried one"
        );
    }

    // -- end to end through `run` ------------------------------------------

    /// Collects what the call thread decided to deliver.
    #[derive(Clone, Default)]
    struct Collector(std::sync::Arc<std::sync::Mutex<Vec<FinishedCall>>>);

    impl CallDelivery for Collector {
        fn deliver(&self, call: FinishedCall) {
            self.0.lock().expect("collector").push(call);
        }
    }

    /// Drive the call thread over `msgs` and return what it delivered, with the
    /// metrics registry so counters can be asserted on.
    fn drive(msgs: Vec<CallMsg>) -> (Vec<FinishedCall>, trunkd_core::Metrics) {
        drive_with_min(msgs, default_min_voice_frames())
    }

    /// The shipped default for `[recorder] min_voice_frames`, so these tests
    /// exercise what a deployment actually runs.
    fn default_min_voice_frames() -> u32 {
        crate::config::Recorder::default().min_voice_frames
    }

    /// [`drive`] with an explicit `min_voice_frames`.
    fn drive_with_min(
        msgs: Vec<CallMsg>,
        min_voice_frames: u32,
    ) -> (Vec<FinishedCall>, trunkd_core::Metrics) {
        let (calls, metrics, _) = drive_all(msgs, min_voice_frames);
        (calls, metrics)
    }

    /// [`drive_with_min`], also handing back the [`Shared`] block, for the
    /// tests that care what the panel was told.
    fn drive_all(
        msgs: Vec<CallMsg>,
        min_voice_frames: u32,
    ) -> (Vec<FinishedCall>, trunkd_core::Metrics, Shared) {
        let metrics = trunkd_core::Metrics::new();
        let shared = Shared {
            private_calls: Default::default(),
            calls: Default::default(),
            skip: trunkd_core::SkipList::empty(),
            replay_clock: Default::default(),
            metrics: metrics.clone(),
            sites: std::sync::Arc::new(vec![SiteSpec {
                system_label: "Example Network".into(),
                site_label: "Central".into(),
                control_frequencies: vec![851_050_000],
                metric_prefix: "site.0".into(),
                display: "Example Network/Central".into(),
            }]),
        };
        let (tx, rx) = trunkd_core::chan::bounded::<CallMsg>(msgs.len().max(256));
        for m in msgs {
            tx.send(m);
        }
        drop(tx);
        let collector = Collector::default();
        let kept = shared.clone();
        run(
            rx,
            shared,
            Box::new(collector.clone()),
            std::time::Duration::from_secs(180),
            min_voice_frames,
            MIN_ORPHAN_AFTER,
            Some("rec-test-01".to_owned()),
            None,
            None,
        );
        let out = collector.0.lock().expect("collector").clone();
        (out, metrics, kept)
    }

    /// [`started`] with a device RSSI gauge attached, as the DSP thread sends
    /// it.
    fn started_with_rssi(rssi: Option<trunkd_core::Metric>) -> CallMsg {
        match started() {
            CallMsg::Started {
                site,
                id,
                frequency_hz,
                talkgroup,
                target,
                source,
                kind,
                patches,
                at_ms,
                ..
            } => CallMsg::Started {
                site,
                id,
                frequency_hz,
                talkgroup,
                target,
                source,
                kind,
                patches,
                rssi,
                at_ms,
            },
            other => other,
        }
    }

    fn voice(n: usize) -> Vec<CallMsg> {
        (0..n)
            .map(|_| CallMsg::Voice {
                site: 0,
                id: trunkd_p25::site::CallId(1),
                frame: [0u8; 18],
            })
            .collect()
    }

    fn started() -> CallMsg {
        CallMsg::Started {
            site: 0,
            id: trunkd_p25::site::CallId(1),
            frequency_hz: 851_050_000,
            talkgroup: 50061,
            target: 50061,
            source: 1_620_719,
            kind: trunkd_p25::event::CallKind::Group,
            patches: Vec::new(),
            rssi: None,
            at_ms: 0,
        }
    }

    fn ended(record: CallRecord) -> CallMsg {
        CallMsg::Ended {
            site: 0,
            id: trunkd_p25::site::CallId(1),
            record: Box::new(record),
            at_ms: 1_000,
            reason: trunkd_p25::site::EndReason::Terminator,
        }
    }

    /// A unit-to-unit grant becomes a unit call all the way to the finished
    /// article: the talker in `source`, the called radio in `target_radio`, no
    /// talkgroup, and a filename naming the pair. Counted apart from the group
    /// traffic, and offered to the panel as a live row.
    #[test]
    fn a_unit_to_unit_grant_finishes_as_a_private_call() {
        let start = CallMsg::Started {
            site: 0,
            id: trunkd_p25::site::CallId(1),
            frequency_hz: 851_125_000,
            talkgroup: 0,
            target: 9_000_007,
            source: 9_000_001,
            kind: CallKind::UnitToUnit,
            patches: Vec::new(),
            rssi: None,
            at_ms: 1_788_012_000_000,
        };
        let mut msgs = vec![start];
        msgs.extend(voice(40));
        msgs.push(CallMsg::Ended {
            site: 0,
            id: trunkd_p25::site::CallId(1),
            record: Box::new(CallRecord {
                source: Some(9_000_001),
                target: Some(9_000_007),
                ..CallRecord::default()
            }),
            at_ms: 1_788_012_001_000,
            reason: trunkd_p25::site::EndReason::Terminator,
        });
        let (calls, metrics, shared) = drive_all(msgs, default_min_voice_frames());

        assert_eq!(calls.len(), 1);
        let call = &calls[0];
        assert_eq!(call.call_kind, trunkd_call::CallKind::Unit);
        assert_eq!(call.target_radio, Some(9_000_007));
        assert_eq!(call.source, Some(9_000_001));
        assert_eq!(
            call.audio_stem(),
            "Example-Network_UNIT_9000001_TO_9000007_1788012000000"
        );

        let n = |name: &str| {
            metrics
                .snapshot()
                .iter()
                .find(|m| m.name == name)
                .map(|m| m.value)
                .unwrap_or(0)
        };
        assert_eq!(n("calls.started"), 1);
        assert_eq!(n("calls.unit_started"), 1);
        assert_eq!(n("calls.unit_ended"), 1);

        let rows = shared.private_calls.rows();
        assert_eq!(rows.len(), 1, "the panel has a row for it");
        assert_eq!((rows[0].source, rows[0].target), (9_000_001, 9_000_007));
        assert_eq!(rows[0].frequency_hz, 851_125_000);
        assert_eq!(
            rows[0].ended_at_ms,
            Some(1_788_012_001_000),
            "and it is closed, not left showing on air"
        );

        // The Activity ring sees the same call as ordinary traffic: no
        // talkgroup, the pair of radios, and the disposition this thread gave
        // it once it had been handed to the uplink.
        assert!(shared.calls.active().is_empty(), "nothing left on air");
        let act = shared.calls.recent();
        assert_eq!(act.len(), 1);
        assert_eq!(act[0].kind, "unit");
        assert_eq!(act[0].talkgroup, None);
        assert_eq!((act[0].source, act[0].target), (9_000_001, Some(9_000_007)));
        assert_eq!(act[0].reason, Some("terminator"));
        assert_eq!(act[0].outcome, Some("delivered"));
        assert_eq!(act[0].voice_frames, 40);
    }

    // -- calls whose end never arrives ---------------------------------------

    /// A call thread running on its own thread with a short orphan bound and a
    /// live recorder link (loopback, no peers), the way the recorder runs it.
    struct Live {
        tx: Option<trunkd_core::chan::BoundedSender<CallMsg>>,
        thread: Option<std::thread::JoinHandle<()>>,
        collector: Collector,
        shared: Shared,
        link: std::sync::Arc<crate::pipeline::link::LinkSlot>,
    }

    impl Live {
        fn start(orphan_after: Duration) -> Live {
            let metrics = trunkd_core::Metrics::new();
            let shared = Shared {
                private_calls: Default::default(),
                calls: Default::default(),
                skip: trunkd_core::SkipList::empty(),
                replay_clock: Default::default(),
                metrics: metrics.clone(),
                sites: std::sync::Arc::new(vec![SiteSpec {
                    system_label: "Example Network".into(),
                    site_label: "Central".into(),
                    control_frequencies: vec![851_050_000],
                    metric_prefix: "site.0".into(),
                    display: "Example Network/Central".into(),
                }]),
            };
            let mut cfg = crate::config::Config::default();
            cfg.link.enabled = true;
            cfg.link.bind = "127.0.0.1:0".into();
            cfg.link.key = "a-sixteen-byte-key-or-longer".into();
            let link = crate::pipeline::link::LinkSlot::new(
                &cfg,
                shared.sites.as_ref(),
                metrics,
                "rec-test-01".into(),
            );
            link.start(None);
            assert!(link.handle().is_some(), "the loopback link is up");

            let (tx, rx) = trunkd_core::chan::bounded::<CallMsg>(256);
            let collector = Collector::default();
            let thread = {
                let (shared, collector, link) = (shared.clone(), collector.clone(), link.clone());
                std::thread::spawn(move || {
                    run(
                        rx,
                        shared,
                        Box::new(collector),
                        Duration::from_secs(180),
                        default_min_voice_frames(),
                        orphan_after,
                        Some("rec-test-01".to_owned()),
                        Some((link, vec!["t0".to_owned()])),
                        None,
                    )
                })
            };
            Live {
                tx: Some(tx),
                thread: Some(thread),
                collector,
                shared,
                link,
            }
        }

        fn send(&self, msg: CallMsg) {
            self.tx.as_ref().expect("still running").send(msg);
        }

        fn counter(&self, name: &str) -> u64 {
            self.shared.metrics.counter(name).get()
        }

        fn delivered(&self) -> Vec<FinishedCall> {
            self.collector.0.lock().expect("collector").clone()
        }

        /// Wait (bounded) until `done` holds.
        fn wait_for(&self, what: &str, done: impl Fn(&Live) -> bool) {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !done(self) {
                assert!(Instant::now() < deadline, "timed out waiting for {what}");
                std::thread::sleep(Duration::from_millis(5));
            }
        }

        /// Close the queue and wait for the thread to drain it and return.
        fn stop(&mut self) {
            drop(self.tx.take());
            if let Some(t) = self.thread.take() {
                t.join().expect("the call thread does not panic");
            }
        }
    }

    impl Drop for Live {
        fn drop(&mut self) {
            self.stop();
            self.link.shutdown();
        }
    }

    /// The queue evicted this call's `Ended`. Nothing else will ever close it,
    /// so the sweep does: reason `orphaned`, its recorder-link claim released,
    /// and the voice it has delivered like any other call's.
    #[test]
    fn a_call_whose_end_was_lost_is_closed_as_orphaned() {
        let mut live = Live::start(Duration::from_millis(500));
        live.send(started());
        for m in voice(20) {
            live.send(m);
        }
        live.wait_for("the call to open", |l| l.link.open_claims() == 1);
        assert_eq!(live.counter("link.claims_tx"), 1, "claimed to the peers");
        assert_eq!(live.shared.calls.active().len(), 1, "and on the air");

        // No `Ended` ever comes.
        live.wait_for("the sweep", |l| !l.delivered().is_empty());
        assert_eq!(live.counter(ORPHANED), 1);
        assert_eq!(live.counter("calls.ended"), 1);
        assert_eq!(live.link.open_claims(), 0, "the claim is released");
        assert_eq!(live.counter("link.releases_tx"), 1);

        let calls = live.delivered();
        assert_eq!(calls.len(), 1, "what it had is uploaded");
        assert_eq!(calls[0].talkgroup, 50061);
        assert_eq!(calls[0].source, Some(1_620_719));
        assert!(!calls[0].pcm.is_empty(), "its voice with it");

        assert!(live.shared.calls.active().is_empty(), "off the air");
        let recent = live.shared.calls.recent();
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].reason, Some("orphaned"));
        assert_eq!(recent[0].outcome, Some("delivered"));
        assert_eq!(recent[0].voice_frames, 20);

        // The `Ended` turning up after all (it was late, not lost) changes
        // nothing: the call is not delivered twice.
        live.send(ended(CallRecord {
            talkgroup: Some(50061),
            voice_frames: 20,
            ..CallRecord::default()
        }));
        live.stop();
        assert_eq!(live.delivered().len(), 1);
        assert_eq!(live.counter("calls.orphaned_events"), 1);
        assert_eq!(live.counter("calls.ended"), 1);
    }

    /// An orphaned call with no voice follows the ordinary no-voice rule: it
    /// is closed (and its claim released) but nothing is encoded.
    #[test]
    fn an_orphaned_call_with_no_voice_is_closed_but_not_uploaded() {
        let mut live = Live::start(Duration::from_millis(100));
        live.send(started());
        live.wait_for("the sweep", |l| l.counter(ORPHANED) == 1);
        assert_eq!(live.link.open_claims(), 0);
        assert_eq!(live.counter("calls.discarded_empty"), 1);
        live.stop();
        assert!(live.delivered().is_empty());
        assert_eq!(live.shared.calls.recent()[0].reason, Some("orphaned"));
    }

    /// The sweep never races a call that is still talking: a call whose
    /// messages keep arriving outlives the orphan bound several times over and
    /// ends the ordinary way.
    #[test]
    fn a_call_that_keeps_talking_is_never_orphaned() {
        let mut live = Live::start(Duration::from_millis(300));
        live.send(started());
        let until = Instant::now() + Duration::from_millis(900);
        let mut frames = 0;
        while Instant::now() < until {
            live.send(voice(1).remove(0));
            frames += 1;
            std::thread::sleep(Duration::from_millis(10));
        }
        live.send(ended(CallRecord::default()));
        live.wait_for("the call to end", |l| !l.delivered().is_empty());
        live.stop();
        assert_eq!(live.counter(ORPHANED), 0);
        let recent = live.shared.calls.recent();
        assert_eq!(recent[0].reason, Some("terminator"));
        assert_eq!(recent[0].voice_frames, frames);
    }

    /// A group call is untouched by any of it: no unit counters move, no panel
    /// row appears, and the name is still the talkgroup's.
    #[test]
    fn a_group_call_is_not_counted_as_a_private_one() {
        let mut msgs = vec![started()];
        msgs.extend(voice(40));
        msgs.push(ended(CallRecord::default()));
        let (calls, metrics, shared) = drive_all(msgs, default_min_voice_frames());
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].call_kind, trunkd_call::CallKind::Group);
        assert!(calls[0].target_radio.is_none());
        assert!(calls[0].audio_stem().contains("_50061_"));
        assert!(shared.private_calls.rows().is_empty());
        // It is still on the Activity ring, though — that one is every call.
        let act = shared.calls.recent();
        assert_eq!(act.len(), 1);
        assert_eq!(act[0].kind, "group");
        assert_eq!(act[0].talkgroup, Some(50_061));
        assert_eq!(act[0].outcome, Some("delivered"));
        assert!(!metrics
            .snapshot()
            .iter()
            .any(|m| m.name.starts_with("calls.unit_") && m.value > 0));
    }

    #[test]
    fn a_finished_call_reports_who_recorded_it_and_how_well() {
        // The facts keyup's best-source negotiation needs when two recorders
        // hand it the same call: which one this copy came from, how much voice
        // it actually decoded, and how much of that the vocoder gave up on.
        let mut msgs = vec![started()];
        msgs.extend(voice(20));
        msgs.push(ended(CallRecord {
            talkgroup: Some(50061),
            voice_frames: 20,
            ..CallRecord::default()
        }));

        let (delivered, _) = drive(msgs);
        assert_eq!(delivered.len(), 1);
        let q = delivered[0]
            .source_quality
            .clone()
            .expect("a recorded call reports its quality");
        assert_eq!(q.instance.as_deref(), Some("rec-test-01"));
        assert_eq!(
            q.voice_frames, 20,
            "every frame that reached the vocoder produced audio"
        );
        assert!(
            q.bad_frames <= q.voice_frames,
            "bad frames are a subset of the frames decoded, got {q:?}"
        );
        // No device gauge was attached, so no level is claimed.
        assert_eq!(q.rssi_dbfs, None);
    }

    #[test]
    fn the_recording_devices_level_rides_the_call() {
        let metrics = trunkd_core::Metrics::new();
        let gauge = metrics.gauge("device.hackrf-0.rssi_dbfs");
        gauge.set_f64(-38.4);

        let mut msgs = vec![started_with_rssi(Some(gauge))];
        msgs.extend(voice(9));
        msgs.push(ended(CallRecord {
            talkgroup: Some(50061),
            voice_frames: 9,
            ..CallRecord::default()
        }));

        let (delivered, _) = drive(msgs);
        assert_eq!(delivered.len(), 1);
        let q = delivered[0].source_quality.clone().expect("quality");
        assert_eq!(q.rssi_dbfs, Some(-38.4));
    }

    #[test]
    fn a_gauge_that_never_measured_anything_reports_no_level() {
        // A registered but unwritten gauge reads as raw zero, and 0 dBFS is
        // full scale — the loudest reading there is. Sending it would tell a
        // best-source negotiation this recorder was the strongest of the lot
        // exactly when it had measured nothing.
        let metrics = trunkd_core::Metrics::new();
        let gauge = metrics.gauge("device.hackrf-0.rssi_dbfs");
        assert_eq!(rssi_dbfs(Some(&gauge)), None);
        assert_eq!(rssi_dbfs(None), None);
        gauge.set_f64(-38.4);
        assert_eq!(rssi_dbfs(Some(&gauge)), Some(-38.4));
    }

    #[test]
    fn an_encrypted_call_reports_no_decoded_frames_but_still_names_its_recorder() {
        // The ciphertext never reaches the vocoder, so `voice_frames` is
        // honestly 0 — but which receiver heard the call is still the fact that
        // tells two copies apart.
        let mut msgs = vec![
            started(),
            CallMsg::Encryption {
                site: 0,
                id: trunkd_p25::site::CallId(1),
                algid: 0x84,
                key_id: 1100,
            },
        ];
        msgs.extend(voice(12));
        msgs.push(ended(CallRecord {
            talkgroup: Some(50061),
            encrypted: true,
            algid: Some(0x84),
            key_id: Some(1100),
            voice_frames: 12,
            ..CallRecord::default()
        }));

        let (delivered, _) = drive(msgs);
        assert_eq!(delivered.len(), 1);
        let q = delivered[0].source_quality.clone().expect("quality");
        assert_eq!(q.voice_frames, 0);
        assert_eq!(q.bad_frames, 0);
        assert_eq!(q.instance.as_deref(), Some("rec-test-01"));
    }

    #[test]
    fn a_clear_call_with_no_audio_is_not_uploaded() {
        // Exactly what Keyup rejected on 2026-08-31 with
        // `400 incomplete call data: no audio`: a grant gave the call a
        // talkgroup, so it is "substantive", but no voice frame ever arrived.
        // The `min_voice_frames` floor now catches it first, one step earlier
        // and before anything is encoded; the no-audio guard behind it stays
        // as the backstop for a call that decoded frames into no PCM at all.
        let (delivered, metrics) = drive(vec![
            started(),
            ended(CallRecord {
                talkgroup: Some(50061),
                source: Some(1_620_719),
                ..CallRecord::default()
            }),
        ]);
        assert!(
            delivered.is_empty(),
            "a clear call with no audio must not be posted: {delivered:?}"
        );
        assert_eq!(metrics.counter("calls.discarded_empty").get(), 1);
    }

    #[test]
    fn a_call_with_no_voice_is_not_encoded_and_one_frame_is_enough() {
        // Both sides of the default floor. Zero frames is the 288-byte MP3
        // Keyup's decoder fails on — a LAME header and nothing else — and it
        // is never built; one frame is a real, if very short, call.
        let (none, metrics) = drive(vec![
            started(),
            ended(CallRecord {
                talkgroup: Some(50061),
                ..CallRecord::default()
            }),
        ]);
        assert!(none.is_empty(), "nothing to encode, nothing sent: {none:?}");
        assert_eq!(metrics.counter("calls.discarded_empty").get(), 1);
        assert_eq!(
            metrics.counter("calls.ended").get(),
            1,
            "the call still ended; only its audio was discarded"
        );

        let mut msgs = vec![started()];
        msgs.extend(voice(1));
        msgs.push(ended(CallRecord {
            talkgroup: Some(50061),
            voice_frames: 1,
            ..CallRecord::default()
        }));
        let (one, metrics) = drive(msgs);
        assert_eq!(one.len(), 1, "one frame clears the default floor");
        assert!(!one[0].pcm.is_empty());
        assert_eq!(metrics.counter("calls.discarded_empty").get(), 0);
    }

    #[test]
    fn the_floor_is_configurable_in_both_directions() {
        let call = || {
            let mut msgs = vec![started()];
            msgs.extend(voice(3));
            msgs.push(ended(CallRecord {
                talkgroup: Some(50061),
                voice_frames: 3,
                ..CallRecord::default()
            }));
            msgs
        };
        // Three frames, floor of four: discarded.
        let (delivered, metrics) = drive_with_min(call(), 4);
        assert!(delivered.is_empty(), "under the floor: {delivered:?}");
        assert_eq!(metrics.counter("calls.discarded_empty").get(), 1);

        // The same call at the floor exactly: kept.
        let (delivered, metrics) = drive_with_min(call(), 3);
        assert_eq!(delivered.len(), 1, "at the floor is not under it");
        assert_eq!(metrics.counter("calls.discarded_empty").get(), 0);

        // Zero turns the whole thing off, for an operator who wants what the
        // recorder used to send.
        let (delivered, _) = drive_with_min(
            vec![
                started(),
                ended(CallRecord {
                    talkgroup: Some(50061),
                    encrypted: true,
                    ..CallRecord::default()
                }),
            ],
            0,
        );
        assert_eq!(delivered.len(), 1, "the floor is off");
    }

    #[test]
    fn an_encrypted_grant_that_delivered_no_voice_is_discarded_too() {
        // An encrypted call's metadata is worth uploading, but only if there
        // was a call: a grant whose traffic channel never keyed up has nothing
        // to say that the call-ended observation has not already said.
        let (delivered, metrics) = drive(vec![
            started(),
            CallMsg::Encryption {
                site: 0,
                id: trunkd_p25::site::CallId(1),
                algid: 0x84,
                key_id: 1100,
            },
            ended(CallRecord {
                talkgroup: Some(50061),
                encrypted: true,
                algid: Some(0x84),
                key_id: Some(1100),
                ..CallRecord::default()
            }),
        ]);
        assert!(
            delivered.is_empty(),
            "no voice, nothing sent: {delivered:?}"
        );
        assert_eq!(metrics.counter("calls.discarded_empty").get(), 1);
    }

    #[test]
    fn an_encrypted_call_with_no_audio_is_still_uploaded() {
        // P25 enciphers only the IMBE payload, so an encrypted call's metadata
        // is the whole point of recording it. The guard must not catch these:
        // the voice frames arrived and were discarded as ciphertext, which is
        // a call that happened, not an empty one.
        let mut msgs = vec![
            started(),
            CallMsg::Encryption {
                site: 0,
                id: trunkd_p25::site::CallId(1),
                algid: 0x84,
                key_id: 1100,
            },
        ];
        msgs.extend(voice(20));
        msgs.push(ended(CallRecord {
            talkgroup: Some(50061),
            encrypted: true,
            algid: Some(0x84),
            key_id: Some(1100),
            voice_frames: 20,
            ..CallRecord::default()
        }));
        let (delivered, metrics) = drive(msgs);
        assert_eq!(delivered.len(), 1);
        assert!(delivered[0].encrypted);
        assert!(delivered[0].pcm.is_empty());
        assert_eq!(delivered[0].algorithm.as_deref(), Some("AES-256"));
        assert_eq!(metrics.counter("calls.dropped_no_audio").get(), 0);
    }

    #[test]
    fn an_all_zero_encryption_word_no_longer_silences_a_clear_call() {
        // The regression itself, end to end. An `algid == 0, key_id == 0`
        // announcement means "no parameters sent"; before the fix it was read
        // as a cipher, the voice was thrown away, and the call went out clear
        // and silent. Now the frames are vocoded and the call carries audio.
        let mut msgs = vec![
            started(),
            CallMsg::Encryption {
                site: 0,
                id: trunkd_p25::site::CallId(1),
                algid: 0,
                key_id: 0,
            },
        ];
        msgs.extend(voice(9));
        msgs.push(ended(CallRecord {
            talkgroup: Some(50061),
            voice_frames: 9,
            ..CallRecord::default()
        }));

        let (delivered, metrics) = drive(msgs);
        assert_eq!(delivered.len(), 1, "the call must still be uploaded");
        assert!(!delivered[0].encrypted);
        assert!(
            !delivered[0].pcm.is_empty(),
            "the voice must have been vocoded, not discarded as ciphertext"
        );
        assert_eq!(metrics.counter("calls.dropped_no_audio").get(), 0);
    }

    /// The per-frame path — vocode, count,
    /// append — makes no heap allocation of its own. What is left is the PCM
    /// buffer, which is sized once for a typical call at its first frame and
    /// grows at most a few times after, so a 10 s call costs the same number
    /// of allocations as a one-second one.
    #[test]
    fn voice_frames_do_not_allocate_per_frame() {
        let run_call = |frames: usize| {
            let mut msgs = vec![started()];
            msgs.extend(voice(frames));
            msgs.push(ended(CallRecord {
                talkgroup: Some(50061),
                voice_frames: frames as u32,
                ..CallRecord::default()
            }));
            let ((delivered, _), allocs) =
                crate::testalloc::count(|| drive_with_min(msgs, default_min_voice_frames()));
            assert_eq!(delivered.len(), 1);
            assert_eq!(delivered[0].pcm.len(), frames * 160);
            allocs
        };
        let one_s = run_call(50);
        let ten_s = run_call(500);
        let thirty_s = run_call(1500);
        eprintln!("allocations: 1 s call {one_s}, 10 s call {ten_s}, 30 s call {thirty_s}");
        assert_eq!(ten_s, one_s, "a 10 s call allocates no more than a 1 s one");
        assert!(
            thirty_s <= one_s + 2,
            "past 10 s the buffer grows at most twice"
        );
    }

    // -- conventional (analog) calls ----------------------------------------

    use crate::pipeline::conventional::{call_id, ChannelSpec};

    fn channel(index: usize) -> std::sync::Arc<ChannelSpec> {
        std::sync::Arc::new(ChannelSpec {
            index,
            name: "Fire Dispatch".into(),
            system_label: "Example County Fire".into(),
            site_label: "Example Hill".into(),
            device_id: "vhf".into(),
            frequency_hz: 154_250_000,
            talkgroup: 14,
            tone: Some("167.9".into()),
            metric_prefix: format!("channel.{index}"),
            display: "Example County Fire/Fire Dispatch".into(),
            params: trunkd_analog::ChannelParams::default(),
        })
    }

    fn tone(s: &str) -> trunkd_analog::Tone {
        s.parse().expect("tone")
    }

    fn conv_started(index: usize, seq: u64, at_ms: u64) -> CallMsg {
        CallMsg::ConvStarted {
            spec: channel(index),
            id: call_id(index, seq),
            start: trunkd_analog::CallStart {
                at_ms: 1_000,
                power_dbfs: -30.0,
                snr_db: 21.5,
                tone_detected: Some(tone("167.9")),
            },
            rssi: None,
            at_ms,
        }
    }

    fn conv_pcm(index: usize, seq: u64, frames: usize) -> CallMsg {
        CallMsg::ConvPcm {
            channel: index,
            id: call_id(index, seq),
            pcm: vec![4_000i16; frames * 160],
        }
    }

    fn conv_ended(index: usize, seq: u64, at_ms: u64) -> CallMsg {
        CallMsg::ConvEnded {
            channel: index,
            id: call_id(index, seq),
            end: trunkd_analog::CallEnd {
                at_ms: 3_000,
                reason: trunkd_analog::EndReason::SquelchClosed,
                duration_ms: 2_000,
                tone_detected: Some(tone("167.9")),
                power_dbfs: -30.0,
                snr_db: 20.04,
                peak_deviation_hz: 2_480.0,
                offset_hz: -55.0,
            },
            at_ms,
        }
    }

    /// Drive the call thread with a live monitor hub attached.
    fn drive_monitored(
        msgs: Vec<CallMsg>,
        hub: std::sync::Arc<crate::pipeline::monitor::MonitorHub>,
    ) -> (Vec<FinishedCall>, Shared) {
        let shared = Shared {
            private_calls: Default::default(),
            calls: Default::default(),
            skip: trunkd_core::SkipList::empty(),
            replay_clock: Default::default(),
            metrics: trunkd_core::Metrics::new(),
            sites: std::sync::Arc::new(Vec::new()),
        };
        let (tx, rx) = trunkd_core::chan::bounded::<CallMsg>(msgs.len().max(16));
        for m in msgs {
            tx.send(m);
        }
        drop(tx);
        let collector = Collector::default();
        run(
            rx,
            shared.clone(),
            Box::new(collector.clone()),
            Duration::from_secs(180),
            default_min_voice_frames(),
            MIN_ORPHAN_AFTER,
            Some("rec-test-01".to_owned()),
            None,
            Some(hub),
        );
        let out = collector.0.lock().expect("collector").clone();
        (out, shared)
    }

    /// A conventional call is filed per the keyup upload contract — a group call under the
    /// channel's system, site, talkgroup and name, one frequency, no radios —
    /// with the analog extras, and the monitor hears it in 20 ms frames.
    #[test]
    fn a_conventional_call_is_assembled_per_the_keyup_contract() {
        let hub = crate::pipeline::monitor::MonitorHub::new();
        let sub = hub
            .subscribe(crate::pipeline::monitor::Filter::Talkgroup(14))
            .expect("a monitor slot");
        let t0 = 1_788_012_000_000;
        let msgs = vec![
            conv_started(0, 0, t0),
            conv_pcm(0, 0, 5),
            conv_pcm(0, 0, 5),
            conv_ended(0, 0, t0 + 200),
        ];
        let (calls, shared) = drive_monitored(msgs, hub);
        assert_eq!(calls.len(), 1);
        let c = &calls[0];
        assert_eq!(c.system, "Example County Fire");
        assert_eq!(c.site, "Example Hill");
        assert_eq!(c.talkgroup, 14);
        assert_eq!(c.talkgroup_label.as_deref(), Some("Fire Dispatch"));
        assert_eq!(c.call_kind, trunkd_call::CallKind::Group);
        assert_eq!(c.frequency_hz, 154_250_000);
        assert_eq!(c.frequencies.len(), 1);
        assert!(c.sources.is_empty() && c.source.is_none(), "no radio ids");
        assert_eq!(
            (c.started_at_ms, c.ended_at_ms),
            (t0 as i64, t0 as i64 + 200)
        );
        assert_eq!(c.pcm.len(), 1_600);
        assert_eq!(
            c.source_quality
                .as_ref()
                .and_then(|q| q.instance.as_deref()),
            Some("rec-test-01")
        );
        let info = c.conventional.as_ref().expect("analog extras");
        assert_eq!(info.mode, "nbfm");
        assert_eq!(info.decoder, "NBFM");
        assert_eq!(info.tone.as_deref(), Some("167.9"));
        assert_eq!(info.tone_detected.as_deref(), Some("167.9"));
        assert_eq!(info.squelch_reason.as_deref(), Some("squelch_closed"));
        assert_eq!(info.snr_db, Some(20.04));
        assert_eq!(info.peak_deviation_hz, Some(2_480.0));
        assert_eq!(info.offset_hz, Some(-55.0));

        // The monitor got every 20 ms frame, each exactly 160 samples.
        let mut frames = 0;
        while let Some(f) = sub.try_next() {
            assert_eq!(f.pcm.len(), 160);
            frames += 1;
        }
        assert_eq!(frames, 10);

        let m = &shared.metrics;
        assert_eq!(m.counter("calls.conventional_started").get(), 1);
        assert_eq!(m.counter("calls.conventional_delivered").get(), 1);
        assert_eq!(m.counter("calls.started").get(), 0, "P25 totals untouched");
        let recent = shared.calls.recent();
        assert_eq!(recent[0].kind, "conventional");
        assert_eq!(recent[0].talkgroup, Some(14));
        assert_eq!(recent[0].reason, Some("squelch closed"));
        assert_eq!(recent[0].outcome, Some("delivered"));
        assert_eq!(recent[0].voice_frames, 10);
    }

    /// The queue evicted a call's `ConvEnded`; the channel's next
    /// `ConvStarted` closes it as orphaned (with its audio) before opening.
    /// Messages for a call that is not open are counted, not applied.
    #[test]
    fn a_lost_conventional_end_is_closed_by_the_next_start() {
        let hub = crate::pipeline::monitor::MonitorHub::new();
        let msgs = vec![
            conv_started(0, 0, 1_000),
            conv_pcm(0, 0, 10),
            // seq 0's ConvEnded was evicted.
            conv_started(0, 1, 5_000),
            conv_pcm(0, 0, 3), // late audio for the closed call
            conv_pcm(0, 1, 10),
            conv_ended(0, 1, 5_400),
            conv_ended(0, 0, 9_000), // late end for the closed call
        ];
        let (calls, shared) = drive_monitored(msgs, hub);
        assert_eq!(calls.len(), 2, "both calls are delivered once");
        let orphan = &calls[0];
        assert_eq!(orphan.pcm.len(), 1_600);
        assert_eq!(
            orphan
                .conventional
                .as_ref()
                .and_then(|i| i.squelch_reason.as_deref()),
            Some("orphaned")
        );
        assert_eq!(
            calls[1]
                .conventional
                .as_ref()
                .and_then(|i| i.squelch_reason.as_deref()),
            Some("squelch_closed")
        );
        assert_eq!(
            calls[1].pcm.len(),
            1_600,
            "no stray audio from the old call"
        );
        let m = &shared.metrics;
        assert_eq!(m.counter(ORPHANED).get(), 1);
        assert_eq!(m.counter("calls.orphaned_events").get(), 2);
        assert_eq!(m.counter("calls.conventional_ended").get(), 2);
    }

    /// Two channels are independent; a call still open when the DSP side
    /// goes away is delivered at shutdown, and one with no audio is not.
    #[test]
    fn open_conventional_calls_are_flushed_at_shutdown() {
        let hub = crate::pipeline::monitor::MonitorHub::new();
        let msgs = vec![
            conv_started(0, 0, 1_000),
            conv_started(1, 0, 1_000),
            conv_pcm(1, 0, 4),
        ];
        let (calls, shared) = drive_monitored(msgs, hub);
        assert_eq!(calls.len(), 1, "only the call with audio");
        assert_eq!(calls[0].pcm.len(), 640);
        assert_eq!(
            calls[0]
                .conventional
                .as_ref()
                .and_then(|i| i.squelch_reason.as_deref()),
            Some("shutdown")
        );
        assert_eq!(
            shared.metrics.counter("calls.conventional_no_audio").get(),
            1
        );
    }

    /// The sweep closes a conventional call whose end never arrived, like a
    /// P25 one: reason `orphaned`, its audio delivered.
    #[test]
    fn the_sweep_closes_an_orphaned_conventional_call() {
        let mut live = Live::start(Duration::from_millis(200));
        live.send(conv_started(2, 0, 1_000));
        live.send(conv_pcm(2, 0, 5));
        live.wait_for("the sweep", |l| l.counter(ORPHANED) == 1);
        live.stop();
        let calls = live.delivered();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].pcm.len(), 800);
        assert_eq!(
            calls[0]
                .conventional
                .as_ref()
                .and_then(|i| i.squelch_reason.as_deref()),
            Some("orphaned")
        );
        assert_eq!(live.shared.calls.recent()[0].reason, Some("orphaned"));
        assert_eq!(
            live.link.open_claims(),
            0,
            "conventional calls claim nothing"
        );
    }
}
