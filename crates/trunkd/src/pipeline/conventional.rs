// SPDX-License-Identifier: GPL-3.0-or-later
//! Conventional analog (NBFM) channels on a device's DSP thread.
//!
//! A `[[channel]]` is one fixed frequency with no control channel: the device
//! that covers it subscribes the frequency on its one
//! [`ChannelBank`](trunkd_p25::dsp::ChannelBank) — the same filterbank its P25
//! sites read, so a channel costs one more output of a filterbank that is
//! running anyway — and a [`ConvEngine`] turns that 25 ksps IQ into calls with
//! [`trunkd_analog::NbfmChannel`]. Per block, after the site engines,
//! [`ConvEngine::process`] drains its channel and hands every
//! [`ChannelEvent`] to the device's router, which stamps it with the device's
//! wall clock and queues it to the call thread as [`CallMsg::ConvStarted`],
//! [`CallMsg::ConvPcm`] or [`CallMsg::ConvEnded`].
//!
//! # Zero cost without channels
//!
//! [`channel_specs`] of a config with no enabled NBFM `[[channel]]` is an
//! empty `Vec` (no allocation), so a device builds no engine, subscribes
//! nothing, and every per-block loop over its engines iterates an empty slice.
//! A device with sites builds its bank from its first site exactly as before;
//! only a device with channels and **no** site builds the bank from its own
//! rate and centre ([`channel_only_bank`]).
//!
//! # Call ids
//!
//! A conventional call id has bit 63 set, the channel index in bits 32..48 and
//! a per-engine sequence below, so it can never equal a P25 call id (those are
//! namespaced by tuner index in bits 48..) on the panel's Activity log, which
//! keys rows by id.
//!
//! [`CallMsg::ConvStarted`]: crate::pipeline::CallMsg::ConvStarted
//! [`CallMsg::ConvPcm`]: crate::pipeline::CallMsg::ConvPcm
//! [`CallMsg::ConvEnded`]: crate::pipeline::CallMsg::ConvEnded

use std::sync::Arc;

use num_complex::Complex;
use trunkd_analog::{ChannelEvent, ChannelParams, ChannelState, NbfmChannel, INPUT_RATE_HZ};
use trunkd_p25::dsp::bank::{BankHandle, ChannelBank, SharedChannelBank};

use crate::config::{ChannelMode, Config, Device};

/// Bit 63 marks a conventional call id (see the module docs).
pub(crate) const CONV_ID_FLAG: u64 = 1 << 63;

/// The extra call-queue depth each enabled channel buys: one open
/// channel produces at most ~10 messages a second.
pub(crate) const QUEUE_DEPTH_PER_CHANNEL: usize = 8;

/// Immutable description of one enabled NBFM channel, shared by the DSP
/// thread that runs it and (through [`CallMsg::ConvStarted`]) the call thread
/// that assembles its calls.
///
/// [`CallMsg::ConvStarted`]: crate::pipeline::CallMsg::ConvStarted
#[derive(Debug, Clone)]
pub(crate) struct ChannelSpec {
    /// Position among the enabled NBFM channels, in config order: the `i` of
    /// the `channel.{i}.*` metrics.
    pub(crate) index: usize,
    /// `name`, sent as `talkgroupLabel`.
    pub(crate) name: String,
    /// `systemLabel` the calls are filed under ([`Config::channel_system`]).
    pub(crate) system_label: String,
    /// `site` recorded on the calls ([`Config::channel_site`]).
    pub(crate) site_label: String,
    /// The device serving it ([`Config::channel_device_for`]).
    pub(crate) device_id: String,
    /// Channel centre, Hz.
    pub(crate) frequency_hz: u64,
    /// Talkgroup the calls are filed under ([`crate::config::Channel::talkgroup_id`]).
    pub(crate) talkgroup: u32,
    /// Configured tone, canonical spelling; `None` is carrier squelch.
    pub(crate) tone: Option<String>,
    /// `channel.{index}`.
    pub(crate) metric_prefix: String,
    /// `system/name` for logs and the Activity view.
    pub(crate) display: String,
    /// Engine settings, already validated.
    pub(crate) params: ChannelParams,
}

/// One [`ChannelSpec`] per enabled NBFM channel, in config order.
///
/// AM and disabled channels get none (AM is warned about and skipped by the
/// recorder). A channel the config cannot resolve — which validation has
/// already refused, so only reachable for a config that skipped it — is left
/// out with an error rather than taking the device down.
pub(crate) fn channel_specs(cfg: &Config) -> Vec<ChannelSpec> {
    cfg.channel
        .iter()
        .filter(|c| c.enabled && c.mode == ChannelMode::Nbfm)
        .enumerate()
        .filter_map(|(index, c)| {
            let resolved = cfg
                .channel_device_for(c)
                .and_then(|d| Ok((d, c.params()?, c.parsed_tone()?)));
            let (device, params, tone) = match resolved {
                Ok(r) => r,
                Err(e) => {
                    tracing::error!(channel = %c.name, error = %e, "conventional channel not started");
                    return None;
                }
            };
            let system_label = cfg.channel_system(c).to_owned();
            Some(ChannelSpec {
                index,
                name: c.name.clone(),
                display: format!("{system_label}/{}", c.name),
                system_label,
                site_label: cfg.channel_site(c).to_owned(),
                device_id: device.id.clone(),
                frequency_hz: c.frequency,
                talkgroup: c.talkgroup_id(),
                tone: tone.map(|t| t.to_string()),
                metric_prefix: format!("channel.{index}"),
                params,
            })
        })
        .collect()
}

/// The bank for a device that carries channels and no P25 site: built from
/// the device's own rate and centre, with the prototype length a site engine
/// would ask for, so it is the very filterbank a site would have built.
pub(crate) fn channel_only_bank(device: &Device) -> Result<SharedChannelBank, String> {
    let taps = trunkd_p25::site::SiteConfig::default().taps_per_channel;
    ChannelBank::shared(
        device.sample_rate,
        device.center_frequency.unwrap_or(0) as f64,
        taps,
    )
    .map_err(|e| format!("device '{}': cannot be channelized: {e}", device.id))
}

/// The `channel.{i}.*` gauges, resolved once so the 1 Hz publish formats no
/// names.
struct ChannelGauges {
    open: trunkd_core::Metric,
    state: trunkd_core::Metric,
    snr_db: trunkd_core::Metric,
    power_dbfs: trunkd_core::Metric,
    noise_floor_dbfs: trunkd_core::Metric,
    offset_hz: trunkd_core::Metric,
    calls: trunkd_core::Metric,
    calls_too_short: trunkd_core::Metric,
    tone_mismatch: trunkd_core::Metric,
    false_opens: trunkd_core::Metric,
    last_call_end_ms: trunkd_core::Metric,
}

impl ChannelGauges {
    fn new(metrics: &trunkd_core::Metrics, prefix: &str) -> Self {
        let g = |name: &str| metrics.gauge(&format!("{prefix}.{name}"));
        ChannelGauges {
            open: g("open"),
            state: g("state"),
            snr_db: g("snr_db"),
            power_dbfs: g("power_dbfs"),
            noise_floor_dbfs: g("noise_floor_dbfs"),
            offset_hz: g("offset_hz"),
            calls: g("calls"),
            calls_too_short: g("calls_too_short"),
            tone_mismatch: g("tone_mismatch"),
            false_opens: g("false_opens"),
            last_call_end_ms: g("last_call_end_ms"),
        }
    }
}

/// `channel.{i}.state` encoding: 0 closed, 1 opening, 2 open, 3 hang.
pub(crate) fn state_code(state: ChannelState) -> u64 {
    match state {
        ChannelState::Closed => 0,
        ChannelState::Opening => 1,
        ChannelState::Open => 2,
        ChannelState::Hang => 3,
    }
}

/// Publish "not decoding" for a device's channels (it stopped or idles).
pub(crate) fn clear_channel_gauges(metrics: &trunkd_core::Metrics, specs: &[Arc<ChannelSpec>]) {
    for s in specs {
        for name in ["open", "state"] {
            metrics.gauge(&format!("{}.{name}", s.metric_prefix)).set(0);
        }
        for name in ["snr_db", "power_dbfs", "noise_floor_dbfs"] {
            metrics
                .gauge(&format!("{}.{name}", s.metric_prefix))
                .set_f64(0.0);
        }
    }
}

/// One channel's engine on its device's DSP thread: the bank subscription,
/// the demodulator, and the drain buffer.
pub(crate) struct ConvEngine {
    spec: Arc<ChannelSpec>,
    handle: BankHandle,
    engine: NbfmChannel,
    /// One block's channel output; sized at build so draining never allocates.
    buf: Vec<Complex<f32>>,
    /// Wideband rate of the bank, for the block's stream time.
    rate: u64,
    /// Sequence of the next call this engine starts.
    seq: u64,
    /// Id of the call now open, between its `Started` and `Ended`.
    open_id: u64,
    gauges: ChannelGauges,
}

impl ConvEngine {
    /// Subscribe `spec`'s frequency on `bank` and build its demodulator.
    ///
    /// Errors when the bank refuses the frequency (outside the capture or on
    /// its DC bin): possible only when keyup moved the device's centre or rate
    /// away from what the config was validated against.
    pub(crate) fn new(
        spec: Arc<ChannelSpec>,
        bank: &SharedChannelBank,
        block_samples: usize,
        metrics: &trunkd_core::Metrics,
    ) -> Result<Self, String> {
        let mut b = bank.borrow_mut();
        let handle = b.subscribe(spec.frequency_hz as f64).map_err(|e| {
            format!(
                "channel '{}' ({} Hz): the capture cannot reach it: {e}",
                spec.name, spec.frequency_hz
            )
        })?;
        let rate = u64::from(b.sample_rate()).max(1);
        let per_block = block_samples as u64 * u64::from(INPUT_RATE_HZ) / rate;
        let gauges = ChannelGauges::new(metrics, &spec.metric_prefix);
        Ok(ConvEngine {
            engine: NbfmChannel::new(spec.params),
            handle,
            buf: Vec::with_capacity(per_block as usize + 256),
            rate,
            seq: 0,
            open_id: 0,
            gauges,
            spec,
        })
    }

    /// The channel this engine records.
    pub(crate) fn spec(&self) -> &Arc<ChannelSpec> {
        &self.spec
    }

    /// The bank subscription (for tests).
    #[cfg(test)]
    pub(crate) fn handle(&self) -> BankHandle {
        self.handle
    }

    /// Read this block's samples from `bank` (which has just run
    /// `process_block` over `part_len` wideband samples) and demodulate them.
    /// Every event goes to `emit` with the call id it belongs to.
    pub(crate) fn process(
        &mut self,
        bank: &SharedChannelBank,
        part_len: usize,
        emit: &mut dyn FnMut(&Arc<ChannelSpec>, u64, ChannelEvent),
    ) {
        self.buf.clear();
        let consumed = {
            let mut b = bank.borrow_mut();
            b.drain(self.handle, &mut self.buf);
            b.samples_consumed()
        };
        if self.buf.is_empty() {
            return;
        }
        let at_ms = consumed.saturating_sub(part_len as u64) * 1000 / self.rate;
        let ConvEngine {
            spec,
            engine,
            buf,
            seq,
            open_id,
            ..
        } = self;
        engine.process(buf, at_ms, &mut |ev| {
            Self::tag(spec, seq, open_id, ev, emit);
        });
    }

    /// The recorder is stopping at stream time `at_ms`: end an open call with
    /// reason `shutdown`.
    pub(crate) fn finish(
        &mut self,
        at_ms: u64,
        emit: &mut dyn FnMut(&Arc<ChannelSpec>, u64, ChannelEvent),
    ) {
        let ConvEngine {
            spec,
            engine,
            seq,
            open_id,
            ..
        } = self;
        engine.finish(at_ms, &mut |ev| {
            Self::tag(spec, seq, open_id, ev, emit);
        });
    }

    /// Give `ev` its call id: a fresh one at `Started`, the open one after.
    fn tag(
        spec: &Arc<ChannelSpec>,
        seq: &mut u64,
        open_id: &mut u64,
        ev: ChannelEvent,
        emit: &mut dyn FnMut(&Arc<ChannelSpec>, u64, ChannelEvent),
    ) {
        if matches!(ev, ChannelEvent::Started(_)) {
            *open_id = call_id(spec.index, *seq);
            *seq = seq.wrapping_add(1);
        }
        emit(spec, *open_id, ev);
    }

    /// Publish the `channel.{i}.*` gauges (1 Hz, off the hot path).
    /// `wall_ms` maps a stream time to this device's wall clock.
    pub(crate) fn publish(&self, wall_ms: &dyn Fn(u64) -> i64) {
        let s = self.engine.stats();
        let g = &self.gauges;
        g.open.set(u64::from(s.is_open()));
        g.state.set(state_code(s.state));
        g.snr_db.set_f64(f64::from(s.snr_db));
        g.power_dbfs.set_f64(f64::from(s.power_dbfs));
        g.noise_floor_dbfs.set_f64(f64::from(s.noise_floor_dbfs));
        g.offset_hz.set_f64(f64::from(s.offset_hz));
        g.calls.set(s.calls);
        g.calls_too_short.set(s.calls_too_short);
        g.tone_mismatch.set(s.tone_mismatch);
        g.false_opens.set(s.false_opens);
        if let Some(end) = s.last_call_end_ms {
            g.last_call_end_ms.set(wall_ms(end).max(0) as u64);
        }
    }
}

/// The id of call `seq` on channel `index` (see the module docs).
pub(crate) fn call_id(index: usize, seq: u64) -> u64 {
    CONV_ID_FLAG | ((index as u64 & 0xFFFF) << 32) | (seq & 0xFFFF_FFFF)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(toml: &str) -> Config {
        toml::from_str(toml).expect("parse")
    }

    const TWO_CHANNELS: &str = r#"
        [uplink]
        system_label = "Example County"
        [[device]]
        id = "v"
        backend = "iqfile"
        path = "/dev/null"
        sample_rate = 2000000
        center_frequency = 154500000
        [[channel]]
        name = "Fire Dispatch"
        system = "Example County Fire"
        site = "Example Hill"
        frequency = 154250000
        tone = "167.9"
        talkgroup = 14
        [[channel]]
        name = "Disabled"
        frequency = 154400000
        enabled = false
        [[channel]]
        name = "Airband"
        frequency = 154300000
        mode = "am"
        [[channel]]
        name = "Roads"
        frequency = 154800000
        tone = "D445I"
    "#;

    #[test]
    fn no_channels_means_no_specs_and_no_allocation() {
        let c = cfg(r#"
            [[device]]
            id = "a"
            backend = "iqfile"
            path = "/dev/null"
            sample_rate = 2000000
            center_frequency = 851000000
        "#);
        let (specs, allocs) = crate::testalloc::count(|| channel_specs(&c));
        assert!(specs.is_empty());
        assert_eq!(allocs, 0, "an empty config costs nothing");
    }

    #[test]
    fn specs_cover_enabled_nbfm_channels_with_their_labels() {
        let specs = channel_specs(&cfg(TWO_CHANNELS));
        assert_eq!(specs.len(), 2, "disabled and AM channels are not started");
        let fire = &specs[0];
        assert_eq!(fire.index, 0);
        assert_eq!(fire.metric_prefix, "channel.0");
        assert_eq!(fire.system_label, "Example County Fire");
        assert_eq!(fire.site_label, "Example Hill");
        assert_eq!(fire.device_id, "v");
        assert_eq!(fire.talkgroup, 14);
        assert_eq!(fire.tone.as_deref(), Some("167.9"));
        let roads = &specs[1];
        assert_eq!(roads.index, 1, "indices count enabled NBFM channels only");
        assert_eq!(
            roads.system_label, "Example County",
            "falls back to [uplink]"
        );
        assert_eq!(roads.site_label, "v", "then the device id");
        assert_eq!(roads.talkgroup, 154_800, "default talkgroup is kHz");
        assert_eq!(roads.tone.as_deref(), Some("D445I"), "canonical spelling");
    }

    #[test]
    fn call_ids_never_collide_with_p25_ids() {
        let a = call_id(0, 0);
        let b = call_id(3, 7);
        assert_ne!(a, b);
        assert!(a & CONV_ID_FLAG != 0 && b & CONV_ID_FLAG != 0);
        // P25 ids are `tuner << 48 | n` with a small tuner index.
        let p25 = (5u64 << 48) | 7;
        assert_eq!(p25 & CONV_ID_FLAG, 0);
    }

    #[test]
    fn an_engine_subscribes_its_channel_on_the_bank() {
        let c = cfg(TWO_CHANNELS);
        let bank = channel_only_bank(&c.device[0]).expect("bank");
        let metrics = trunkd_core::Metrics::new();
        let engines: Vec<ConvEngine> = channel_specs(&c)
            .into_iter()
            .map(|s| ConvEngine::new(Arc::new(s), &bank, 64 * 1024, &metrics).expect("engine"))
            .collect();
        let b = bank.borrow();
        assert_eq!(b.channel_count(), 2);
        assert_eq!(
            b.subscribed_frequency(engines[0].handle()),
            Some(154_250_000.0)
        );
        assert_eq!(b.sample_rate(), 2_000_000);
        assert_eq!(b.center_hz(), 154_500_000.0);
    }

    #[test]
    fn a_frequency_the_bank_cannot_reach_is_an_error_not_a_panic() {
        let c = cfg(TWO_CHANNELS);
        let bank = channel_only_bank(&c.device[0]).expect("bank");
        let mut spec = channel_specs(&c).remove(0);
        spec.frequency_hz = 460_000_000;
        let err = ConvEngine::new(Arc::new(spec), &bank, 1024, &trunkd_core::Metrics::new())
            .err()
            .expect("refused");
        assert!(err.contains("cannot reach"), "{err}");
    }
}
