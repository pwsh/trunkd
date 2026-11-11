// SPDX-License-Identifier: GPL-3.0-or-later
//! Import an sdrtrunk playlist XML into an equivalent `trunkd.toml`.
//!
//! Parses the elements named in ARCHITECTURE (`<channel>` with
//! `sourceConfigTunerMultipleFrequency` / `sourceConfigTuner` sources and
//! `decodeConfigP25Phase1` / `decodeConfigNBFM` / `decodeConfigAM` decoders) and
//! produces devices/sites/channels. Alias data and other elements are ignored.
//!
//! sdrtrunk playlists don't record concrete tuner hardware, so placeholder
//! Soapy devices are synthesized: one per site, plus one per group of
//! conventional channels no site tuner can hear (see `place_devices`).

use serde::Deserialize;
use trunkd_core::{Error, Result};

use crate::config::{
    span_guard_hz, usable_span_hz, Backend, Channel, ChannelMode, Config, Device, PacingName, Site,
    Squelch, CHANNEL_CONTROL_SPACING_HZ,
};

const PLACEHOLDER_DEVICE: &str = "sdr0";
/// Center used when a playlist carries no frequency at all to derive one from.
const DEFAULT_CENTER_HZ: u64 = 855_000_000;
/// Capture width given to each imported placeholder tuner: a HackRF's full
/// 20 MHz, which is what one tuner per site needs to hold a P25 site's control
/// and traffic channels.
const PLACEHOLDER_SAMPLE_RATE: u32 = 20_000_000;

#[derive(Debug, Deserialize)]
struct Playlist {
    #[serde(rename = "channel", default)]
    channels: Vec<PlaylistChannel>,
}

#[derive(Debug, Deserialize)]
struct PlaylistChannel {
    #[serde(rename = "@system", default)]
    system: String,
    #[serde(rename = "@site", default)]
    site: String,
    #[serde(rename = "@name", default)]
    name: String,
    #[serde(rename = "@enabled", default)]
    enabled: Option<String>,
    #[serde(rename = "decode_configuration", default)]
    decode: Option<DecodeConfig>,
    #[serde(rename = "source_configuration", default)]
    source: Option<SourceConfig>,
}

#[derive(Debug, Deserialize)]
struct DecodeConfig {
    // Jackson serializes the subtype discriminator as a `type` attribute; some
    // exports use the xsi-namespaced form. Accept both.
    #[serde(rename = "@type", alias = "@xsi:type", default)]
    kind: String,
    /// P25: recorder pool size for traffic channels.
    #[serde(rename = "@traffic_channel_pool_size", default)]
    traffic_channel_pool_size: Option<usize>,
    /// NBFM: channel bandwidth as an enum name, e.g. `BW_12_5` / `BW_25_0`.
    #[serde(rename = "@bandwidth", default)]
    bandwidth: Option<String>,
    /// NBFM: synthetic talkgroup id assigned to calls from this channel.
    #[serde(rename = "@talkgroup", default)]
    talkgroup: Option<u32>,
    #[serde(rename = "@squelchNoiseOpenThreshold", default)]
    squelch_noise_open: Option<f32>,
    #[serde(rename = "@squelchNoiseCloseThreshold", default)]
    squelch_noise_close: Option<f32>,
    #[serde(rename = "@squelchHysteresisOpenThreshold", default)]
    squelch_hysteresis_open: Option<u32>,
    #[serde(rename = "@squelchHysteresisCloseThreshold", default)]
    squelch_hysteresis_close: Option<u32>,
}

impl DecodeConfig {
    /// Bandwidth in Hz, parsed from sdrtrunk's `BW_<int>_<frac>` kHz enum names.
    fn bandwidth_hz(&self) -> u32 {
        self.bandwidth
            .as_deref()
            .and_then(parse_bandwidth_hz)
            .unwrap_or(12_500)
    }

    /// sdrtrunk's squelch attributes, carried over as the legacy fields (which
    /// trunkd accepts and ignores) so the import loses nothing; `None` when the
    /// playlist has none of them.
    fn squelch(&self) -> Option<Squelch> {
        let sq = Squelch {
            noise_open: self.squelch_noise_open,
            noise_close: self.squelch_noise_close,
            hysteresis_open: self.squelch_hysteresis_open,
            hysteresis_close: self.squelch_hysteresis_close,
            ..Squelch::default()
        };
        sq.has_legacy_fields().then_some(sq)
    }
}

/// `BW_12_5` -> 12500, `BW_25_0` -> 25000.
fn parse_bandwidth_hz(name: &str) -> Option<u32> {
    let digits = name.strip_prefix("BW_")?;
    let (whole, frac) = digits.split_once('_')?;
    let khz: f64 = format!("{whole}.{frac}").parse().ok()?;
    if khz <= 0.0 {
        return None;
    }
    Some((khz * 1000.0).round() as u32)
}

#[derive(Debug, Deserialize)]
struct SourceConfig {
    /// Present on `sourceConfigTuner` (single frequency).
    #[serde(rename = "@frequency", default)]
    frequency: Option<u64>,
    /// Present on `sourceConfigTunerMultipleFrequency`: ms between control
    /// frequency rotations while hunting.
    #[serde(rename = "@frequency_rotation_delay", default)]
    frequency_rotation_delay: Option<u64>,
    /// Present on `sourceConfigTunerMultipleFrequency` (repeated element).
    #[serde(rename = "frequency", default)]
    frequencies: Vec<u64>,
}

impl PlaylistChannel {
    fn is_enabled(&self) -> bool {
        // Default to enabled if the attribute is absent.
        self.enabled
            .as_deref()
            .map(|v| v.eq_ignore_ascii_case("true"))
            .unwrap_or(true)
    }

    fn frequencies(&self) -> Vec<u64> {
        match &self.source {
            Some(src) if !src.frequencies.is_empty() => src.frequencies.clone(),
            Some(SourceConfig {
                frequency: Some(f), ..
            }) => vec![*f],
            _ => Vec::new(),
        }
    }
}

/// Parse a playlist XML string into a [`Config`].
pub fn playlist_to_config(xml: &str) -> Result<Config> {
    let playlist: Playlist = quick_xml::de::from_str(xml)
        .map_err(|e| Error::config(format!("parsing sdrtrunk playlist: {e}")))?;

    let mut cfg = Config {
        device: vec![Device {
            id: PLACEHOLDER_DEVICE.to_owned(),
            backend: Backend::Soapy,
            soapy: Some("driver=hackrf".to_owned()),
            driver: None,
            gains: Default::default(),
            agc: None,
            bandwidth_hz: None,
            rx_port: None,
            settings: Default::default(),
            path: None,
            sample_rate: PLACEHOLDER_SAMPLE_RATE,
            gain: None,
            ppm: 0.0,
            auto_ppm: true,
            auto_ppm_limit: 5.0,
            ring_seconds: 1.0,
            // Backend-specific fields the importer has no source for.
            center_frequency: None,
            lna_db: None,
            vga_db: None,
            amp: None,
            serial: None,
            portapack_port: None,
            restore_portapack_on_exit: false,
            pacing: PacingName::default(),
            role: crate::config::DeviceRole::default(),
            // Station self-description: an sdrtrunk playlist says nothing about
            // the antenna on the roof, so the operator fills these in.
            receiver: String::new(),
            antenna: String::new(),
            cable: String::new(),
            amplifier: String::new(),
            height_m: None,
            notes: String::new(),
            bands: Vec::new(),
            site: None,
            remote_grants: None,
        }],
        ..Config::default()
    };

    // Disabled entries are *kept*, not filtered: sdrtrunk users toggle channels
    // at runtime, so a migration importer that dropped them would silently lose
    // most of the playlist. `enabled` rides along and gates startup instead.
    for ch in &playlist.channels {
        let Some(decode) = ch.decode.as_ref() else {
            continue;
        };
        let enabled = ch.is_enabled();

        match decode.kind.as_str() {
            "decodeConfigP25Phase1" | "decodeConfigP25" => {
                let freqs = ch.frequencies();
                if freqs.is_empty() {
                    continue;
                }
                let site = Site {
                    system: nonempty(&ch.system, "System"),
                    site: nonempty(&ch.site, &ch.name),
                    device: PLACEHOLDER_DEVICE.to_owned(),
                    devices: Vec::new(),
                    enabled,
                    control_frequencies: freqs,
                    traffic_pool_size: decode.traffic_channel_pool_size.unwrap_or(20),
                    rotation_delay_ms: ch.source.as_ref().and_then(|s| s.frequency_rotation_delay),
                    // The engine's own defaults; a playlist says nothing about
                    // hang or inactivity timing.
                    hang_ms: None,
                    call_inactivity_ms: None,
                    phase2: false,
                    data_follow: false,
                };
                merge_site(&mut cfg.site, site);
            }
            "decodeConfigNBFM" | "decodeConfigAM" => {
                let freqs = ch.frequencies();
                let Some(freq) = freqs.first().copied() else {
                    continue;
                };
                let is_am = decode.kind == "decodeConfigAM";
                cfg.channel.push(Channel {
                    name: nonempty(&ch.name, "channel"),
                    // The playlist's labels are how sdrtrunk filed these calls;
                    // keeping them keeps the archive's talkgroups continuous.
                    system: ch.system.trim().to_owned(),
                    site: ch.site.trim().to_owned(),
                    // Assigned by `place_devices`.
                    device: String::new(),
                    frequency: freq,
                    mode: if is_am {
                        ChannelMode::Am
                    } else {
                        ChannelMode::Nbfm
                    },
                    // A 25 kHz channel is kept but disabled: trunkd does not
                    // record wideband channels yet, and an enabled one would
                    // fail `trunkd check`.
                    enabled: enabled && decode.bandwidth_hz() != 25_000,
                    bandwidth_hz: decode.bandwidth_hz(),
                    talkgroup: decode.talkgroup,
                    tone: None,
                    dcs_strict: false,
                    deemphasis_us: None,
                    squelch: decode.squelch(),
                });
            }
            // Out of scope for trunkd (DMR/NXDN/LTR/etc.) — skip.
            _ => {}
        }
    }

    place_devices(&mut cfg);
    Ok(cfg)
}

/// Give every imported site its own placeholder tuner, centered on that site's
/// control channels, and put every conventional channel on a tuner that can
/// actually hear it.
///
/// An sdrtrunk playlist says nothing at all about the SDR: it is a list of
/// frequencies. But trunkd will not start a site whose control channels fall
/// outside its tuner's capture, and a single 2.4 MHz placeholder could not hold
/// even one 700 MHz site — let alone the 700 and 800 MHz sites a real playlist
/// mixes. So the importer derives what it *can* derive: one 20 MHz tuner per
/// site, centered between that site's lowest and highest control channel.
///
/// A conventional NBFM channel goes on a site tuner whose capture already holds
/// it cleanly (inside the edge guard, off the DC bin, clear of that site's
/// control channels); the rest are grouped by frequency onto extra placeholder
/// tuners, each centered on its group and nudged off any channel's DC bin. AM
/// channels are not recorded, so they never cause a tuner of their own: they
/// stay on a covering tuner, else the first one. The operator still edits
/// `backend`/`driver`/gain, but the config that comes out of `trunkd import`
/// describes receivers that could actually hear what it lists.
fn place_devices(cfg: &mut Config) {
    let template = cfg.device[0].clone();
    let placeholder = |n: usize, center: u64| Device {
        id: format!("sdr{n}"),
        center_frequency: Some(center),
        ..template.clone()
    };
    let mut devices = Vec::with_capacity(cfg.site.len());
    for site in cfg.site.iter_mut() {
        let center = span_center(&site.control_frequencies).unwrap_or(DEFAULT_CENTER_HZ);
        let d = placeholder(devices.len(), center);
        site.device = d.id.clone();
        devices.push(d);
    }

    // NBFM channels no site tuner holds, grouped into captures of their own.
    let mut homeless: Vec<usize> = Vec::new();
    for (i, c) in cfg.channel.iter_mut().enumerate() {
        if c.mode != ChannelMode::Nbfm {
            continue;
        }
        match devices.iter().find(|d| holds(d, &cfg.site, c.frequency)) {
            Some(d) => c.device = d.id.clone(),
            None => homeless.push(i),
        }
    }
    homeless.sort_by_key(|&i| cfg.channel[i].frequency);
    let mut group: Vec<usize> = Vec::new();
    let flush = |group: &mut Vec<usize>, devices: &mut Vec<Device>, channels: &mut [Channel]| {
        if group.is_empty() {
            return;
        }
        let freqs: Vec<u64> = group.iter().map(|&i| channels[i].frequency).collect();
        let mut center = span_center(&freqs).unwrap_or(DEFAULT_CENTER_HZ);
        while freqs.iter().any(|f| f.abs_diff(center) < DC_CLEARANCE_HZ) {
            center += DC_CLEARANCE_HZ;
        }
        let d = placeholder(devices.len(), center);
        for &i in group.iter() {
            channels[i].device = d.id.clone();
        }
        devices.push(d);
        group.clear();
    };
    for &i in &homeless {
        let f = cfg.channel[i].frequency;
        if group
            .first()
            .is_some_and(|&g| f - cfg.channel[g].frequency > CHANNEL_GROUP_SPAN_HZ)
        {
            flush(&mut group, &mut devices, &mut cfg.channel);
        }
        group.push(i);
    }
    flush(&mut group, &mut devices, &mut cfg.channel);

    if devices.is_empty() {
        // Nothing but AM (or nothing at all): one placeholder centered on it.
        let freqs: Vec<u64> = cfg.channel.iter().map(|c| c.frequency).collect();
        devices.push(placeholder(
            0,
            span_center(&freqs).unwrap_or(DEFAULT_CENTER_HZ),
        ));
    }
    for c in cfg
        .channel
        .iter_mut()
        .filter(|c| c.mode != ChannelMode::Nbfm)
    {
        let covering = devices.iter().find(|d| {
            usable_span_hz(d.center_frequency, d.sample_rate)
                .is_some_and(|(lo, hi)| c.frequency >= lo && c.frequency <= hi)
        });
        c.device = covering.unwrap_or(&devices[0]).id.clone();
    }
    cfg.device = devices;
}

/// Widest spread of channel frequencies one imported 20 MHz placeholder is
/// given: leaves each edge its guard band plus room to step off a DC bin.
const CHANNEL_GROUP_SPAN_HZ: u64 = 18_000_000;

/// How far a channel must sit from its tuner's centre to be clear of the DC
/// bin on either the one- or the two-bin channelizer path.
const DC_CLEARANCE_HZ: u64 = 25_000;

/// Whether `d` already holds a conventional channel at `freq` cleanly: inside
/// its usable span less the edge guard, clear of its DC bin, and clear of the
/// control candidates of every site it serves.
fn holds(d: &Device, sites: &[Site], freq: u64) -> bool {
    let (Some(center), Some((lo, hi))) = (
        d.center_frequency,
        usable_span_hz(d.center_frequency, d.sample_rate),
    ) else {
        return false;
    };
    let guard = span_guard_hz(d.sample_rate);
    freq >= lo + guard
        && freq + guard <= hi
        && freq.abs_diff(center) >= DC_CLEARANCE_HZ
        && !sites
            .iter()
            .filter(|s| s.device_ids().contains(&d.id.as_str()))
            .flat_map(|s| &s.control_frequencies)
            .any(|cc| cc.abs_diff(freq) < CHANNEL_CONTROL_SPACING_HZ)
}

/// Midpoint of the lowest and highest frequency in `freqs`.
fn span_center(freqs: &[u64]) -> Option<u64> {
    let lo = *freqs.iter().min()?;
    let hi = *freqs.iter().max()?;
    Some(lo + (hi - lo) / 2)
}

/// Insert `incoming`, merging into an existing site with the same
/// (system, site) key.
///
/// sdrtrunk playlists routinely carry several channel definitions for one
/// physical site (e.g. an enabled entry plus a disabled duplicate with the same
/// control frequencies in a different rotation order). trunkd models a site
/// once, so those collapse into a single entry.
fn merge_site(sites: &mut Vec<Site>, incoming: Site) {
    let Some(existing) = sites
        .iter_mut()
        .find(|s| s.system == incoming.system && s.site == incoming.site)
    else {
        sites.push(incoming);
        return;
    };

    // Union of control frequencies, preserving first-seen order.
    for f in incoming.control_frequencies {
        if !existing.control_frequencies.contains(&f) {
            existing.control_frequencies.push(f);
        }
    }

    existing.rotation_delay_ms = merge_rotation_delay(
        existing.rotation_delay_ms,
        existing.enabled,
        incoming.rotation_delay_ms,
        incoming.enabled,
    );
    existing.enabled |= incoming.enabled;
    existing.traffic_pool_size = existing.traffic_pool_size.max(incoming.traffic_pool_size);
    existing.phase2 |= incoming.phase2;
}

/// The enabled entry's rotation delay wins; if both agree on enablement, take
/// the more responsive (smaller) delay.
fn merge_rotation_delay(a: Option<u64>, a_en: bool, b: Option<u64>, b_en: bool) -> Option<u64> {
    match (a_en, b_en) {
        (true, false) => a,
        (false, true) => b,
        _ => match (a, b) {
            (Some(x), Some(y)) => Some(x.min(y)),
            (Some(x), None) => Some(x),
            (None, y) => y,
        },
    }
}

fn nonempty(s: &str, fallback: &str) -> String {
    if s.trim().is_empty() {
        fallback.to_owned()
    } else {
        s.to_owned()
    }
}

/// Serialize a [`Config`] to a TOML string.
pub fn config_to_toml(cfg: &Config) -> Result<String> {
    toml::to_string_pretty(cfg).map_err(|e| Error::config(format!("serializing toml: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<playlist version="4">
  <channel system="Example Network" site="Site 1" name="Control" enabled="true">
    <decode_configuration type="decodeConfigP25Phase1" modulation="C4FM"/>
    <source_configuration type="sourceConfigTunerMultipleFrequency">
      <frequency>770056250</frequency>
      <frequency>771018750</frequency>
      <frequency>772293750</frequency>
    </source_configuration>
  </channel>
  <channel system="Local" site="Fire" name="Fire Dispatch" enabled="true">
    <decode_configuration type="decodeConfigNBFM"/>
    <source_configuration type="sourceConfigTuner" frequency="154250000"/>
  </channel>
  <channel system="Air" site="Twr" name="Tower" enabled="true">
    <decode_configuration type="decodeConfigAM"/>
    <source_configuration type="sourceConfigTuner" frequency="118300000"/>
  </channel>
  <channel system="Off" site="Off" name="Disabled" enabled="false">
    <decode_configuration type="decodeConfigNBFM" bandwidth="BW_25_0" talkgroup="7"
        squelchNoiseOpenThreshold="0.1" squelchNoiseCloseThreshold="0.19"
        squelchHysteresisOpenThreshold="4" squelchHysteresisCloseThreshold="6"/>
    <source_configuration type="sourceConfigTuner" frequency="155000000"/>
  </channel>
  <alias name="ignored"/>
</playlist>"#;

    #[test]
    fn parses_p25_site_and_analog_channels() {
        let cfg = playlist_to_config(SAMPLE).expect("parse");
        assert_eq!(cfg.site.len(), 1);
        assert_eq!(cfg.site[0].system, "Example Network");
        assert_eq!(cfg.site[0].control_frequencies.len(), 3);
        assert_eq!(cfg.site[0].control_frequencies[0], 770056250);

        // All three analog channels are kept, including the disabled one.
        assert_eq!(cfg.channel.len(), 3);
        assert_eq!(cfg.enabled_channels(), 2);
        let modes: Vec<_> = cfg.channel.iter().map(|c| c.mode).collect();
        assert!(modes.contains(&ChannelMode::Nbfm));
        assert!(modes.contains(&ChannelMode::Am));

        // One placeholder tuner per site, each centered on its own control
        // channels so the imported config actually validates, then one more
        // for the VHF NBFM channels the 770 MHz tuner cannot hear.
        assert_eq!(cfg.device.len(), cfg.site.len() + 1);
        assert_eq!(cfg.site[0].device, "sdr0");
        for (d, s) in cfg.device.iter().zip(&cfg.site) {
            let center = d.center_frequency.expect("importer sets a center");
            let half = u64::from(d.sample_rate) / 2;
            for f in &s.control_frequencies {
                assert!(
                    *f >= center - half && *f <= center + half,
                    "control {f} outside {}'s capture",
                    d.id
                );
            }
        }
        cfg.validate().expect("valid");
    }

    #[test]
    fn preserves_nbfm_decode_fields() {
        let cfg = playlist_to_config(SAMPLE).expect("parse");
        let disabled = cfg
            .channel
            .iter()
            .find(|c| c.name == "Disabled")
            .expect("disabled channel kept");
        assert!(!disabled.enabled);
        assert_eq!(disabled.bandwidth_hz, 25_000);
        assert_eq!(disabled.talkgroup, Some(7));
        // sdrtrunk's squelch numbers ride along as the ignored legacy fields.
        let sq = disabled.squelch.expect("squelch parsed");
        assert_eq!(sq.noise_open, Some(0.1));
        assert_eq!(sq.noise_close, Some(0.19));
        assert_eq!(sq.hysteresis_open, Some(4));
        assert_eq!(sq.hysteresis_close, Some(6));
        assert_eq!(sq.open_db, None, "no dB threshold is invented");

        // A channel with no bandwidth attribute defaults to narrowband.
        let fire = cfg
            .channel
            .iter()
            .find(|c| c.name == "Fire Dispatch")
            .expect("fire channel");
        assert_eq!(fire.bandwidth_hz, 12_500);
        assert!(fire.squelch.is_none());
        // The playlist's labels come across, so calls keep their filing.
        assert_eq!(
            (fire.system.as_str(), fire.site.as_str()),
            ("Local", "Fire")
        );
        assert_eq!(fire.tone, None);
    }

    /// Every channel's device; and an NBFM channel's device really covers it.
    fn assert_placed(cfg: &Config) {
        for c in &cfg.channel {
            let d = cfg
                .device
                .iter()
                .find(|d| d.id == c.device)
                .unwrap_or_else(|| panic!("channel '{}' on unknown device '{}'", c.name, c.device));
            if c.mode == ChannelMode::Nbfm {
                assert!(
                    holds(d, &cfg.site, c.frequency),
                    "channel '{}' ({} Hz) not held by '{}' (center {:?})",
                    c.name,
                    c.frequency,
                    d.id,
                    d.center_frequency
                );
            }
        }
    }

    #[test]
    fn places_channels_on_a_tuner_that_hears_them() {
        // A VHF/UHF analog-only playlist cannot share one 20 MHz capture.
        const ANALOG: &str = r#"<playlist version="4">
  <channel system="Example County Fire" site="Example Hill" name="Fire Dispatch" enabled="true">
    <decode_configuration type="decodeConfigNBFM" bandwidth="BW_12_5" talkgroup="1"/>
    <source_configuration type="sourceConfigTuner" frequency="154250000"/>
  </channel>
  <channel system="Example County Fire" site="Example Hill" name="Fireground" enabled="true">
    <decode_configuration type="decodeConfigNBFM" bandwidth="BW_12_5" talkgroup="2"/>
    <source_configuration type="sourceConfigTuner" frequency="154400000"/>
  </channel>
  <channel system="Example County Fire" site="Example Hill" name="Tac" enabled="true">
    <decode_configuration type="decodeConfigNBFM" bandwidth="BW_12_5" talkgroup="3"/>
    <source_configuration type="sourceConfigTuner" frequency="154325000"/>
  </channel>
  <channel system="Example Schools" site="Campus" name="Buses" enabled="true">
    <decode_configuration type="decodeConfigNBFM" bandwidth="BW_25_0" talkgroup="1"/>
    <source_configuration type="sourceConfigTuner" frequency="462500000"/>
  </channel>
  <channel system="Example Air" site="Field" name="Tower" enabled="true">
    <decode_configuration type="decodeConfigAM"/>
    <source_configuration type="sourceConfigTuner" frequency="118500000"/>
  </channel>
</playlist>"#;
        let cfg = playlist_to_config(ANALOG).expect("parse");
        assert!(cfg.site.is_empty());
        assert_eq!(
            cfg.device.len(),
            2,
            "one VHF and one UHF tuner: {:?}",
            cfg.device
        );
        let dev = |name: &str| {
            cfg.channel
                .iter()
                .find(|c| c.name == name)
                .map(|c| c.device.as_str())
                .expect("channel")
        };
        assert_eq!(dev("Fire Dispatch"), dev("Fireground"));
        assert_eq!(dev("Fire Dispatch"), dev("Tac"));
        assert_ne!(dev("Fire Dispatch"), dev("Buses"));
        // The 25 kHz channel comes across disabled (wideband is not supported).
        let buses = cfg
            .channel
            .iter()
            .find(|c| c.name == "Buses")
            .expect("buses");
        assert!(!buses.enabled);
        assert_eq!(buses.bandwidth_hz, 25_000);
        // The VHF group's midpoint is exactly the Tac channel: the centre is
        // stepped off it so the channel is not on the DC bin.
        let vhf = cfg
            .device
            .iter()
            .find(|d| d.id == dev("Tac"))
            .expect("tuner");
        assert_ne!(vhf.center_frequency, Some(154_325_000));
        // AM is not recorded, so it gets no tuner of its own.
        assert_eq!(dev("Tower"), "sdr0");
        assert_placed(&cfg);
        cfg.validate().expect("imported analog config is valid");
    }

    #[test]
    fn a_channel_inside_a_site_capture_shares_its_tuner() {
        const SHARED: &str = r#"<playlist version="4">
  <channel system="Example System" site="Example County" name="Control" enabled="true">
    <decode_configuration type="decodeConfigP25Phase1"/>
    <source_configuration type="sourceConfigTunerMultipleFrequency">
      <frequency>460100000</frequency>
      <frequency>460300000</frequency>
    </source_configuration>
  </channel>
  <channel system="Example Works" site="Yard" name="Works" enabled="true">
    <decode_configuration type="decodeConfigNBFM" talkgroup="5"/>
    <source_configuration type="sourceConfigTuner" frequency="462550000"/>
  </channel>
  <channel system="Example Works" site="Yard" name="On Control" enabled="false">
    <decode_configuration type="decodeConfigNBFM" talkgroup="6"/>
    <source_configuration type="sourceConfigTuner" frequency="460300000"/>
  </channel>
</playlist>"#;
        let cfg = playlist_to_config(SHARED).expect("parse");
        let works = cfg
            .channel
            .iter()
            .find(|c| c.name == "Works")
            .expect("works");
        assert_eq!(
            works.device, cfg.site[0].device,
            "inside the site's capture"
        );
        // On a control channel: the site tuner cannot take it, a new one does.
        let on_cc = cfg
            .channel
            .iter()
            .find(|c| c.name == "On Control")
            .expect("cc");
        assert_ne!(on_cc.device, cfg.site[0].device);
        assert_eq!(cfg.device.len(), 2);
        assert_placed(&cfg);
        cfg.validate().expect("valid");
    }

    #[test]
    fn parses_bandwidth_enum_names() {
        assert_eq!(parse_bandwidth_hz("BW_12_5"), Some(12_500));
        assert_eq!(parse_bandwidth_hz("BW_25_0"), Some(25_000));
        assert_eq!(parse_bandwidth_hz("garbage"), None);
    }

    const DUPES: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<playlist version="4">
  <channel system="Example Network" site="Site 1" name="Control" enabled="false">
    <decode_configuration type="decodeConfigP25Phase1" traffic_channel_pool_size="20"/>
    <source_configuration type="sourceConfigTunerMultipleFrequency" frequency_rotation_delay="1000">
      <frequency>851050000</frequency>
      <frequency>852537500</frequency>
    </source_configuration>
  </channel>
  <channel system="Example Network" site="Site 1" name="Control" enabled="true">
    <decode_configuration type="decodeConfigP25Phase1" traffic_channel_pool_size="30"/>
    <source_configuration type="sourceConfigTunerMultipleFrequency" frequency_rotation_delay="400">
      <frequency>852537500</frequency>
      <frequency>851050000</frequency>
      <frequency>859987500</frequency>
    </source_configuration>
  </channel>
</playlist>"#;

    #[test]
    fn merges_duplicate_sites() {
        let cfg = playlist_to_config(DUPES).expect("parse");
        assert_eq!(cfg.site.len(), 1, "duplicate (system, site) collapses");
        let s = &cfg.site[0];
        // Union, first-seen order, no duplicates.
        assert_eq!(s.control_frequencies, vec![851050000, 852537500, 859987500]);
        assert!(s.enabled, "enabled = OR of the merged entries");
        assert_eq!(s.traffic_pool_size, 30, "pool = max");
        assert_eq!(s.rotation_delay_ms, Some(400), "enabled entry's delay wins");
    }

    #[test]
    fn rotation_delay_merge_rules() {
        // The enabled side wins regardless of magnitude.
        assert_eq!(
            merge_rotation_delay(Some(1000), true, Some(400), false),
            Some(1000)
        );
        assert_eq!(
            merge_rotation_delay(Some(1000), false, Some(400), true),
            Some(400)
        );
        // Same enablement: the smaller (more responsive) delay wins.
        assert_eq!(
            merge_rotation_delay(Some(1000), true, Some(400), true),
            Some(400)
        );
        assert_eq!(
            merge_rotation_delay(None, false, Some(400), false),
            Some(400)
        );
        assert_eq!(merge_rotation_delay(None, false, None, false), None);
    }

    /// A whole sdrtrunk export in miniature: aliases ahead of the channels,
    /// the per-channel elements the importer ignores, one P25 site listed
    /// twice (as sdrtrunk does when a site is duplicated in the editor), a
    /// second P25 system, NBFM/AM channels, a DMR channel and two streaming
    /// entries. Every name, id and credential in it is made up.
    const MULTI_SITE: &str = r#"<playlist version="4">
  <alias name="Fire Dispatch" color="0" group="Fire" list="Example_List">
    <id type="talkgroup" value="1001" protocol="APCO25"/>
    <id type="priority" priority="-1"/>
  </alias>
  <alias name="Police Main" color="0" group="Police" list="Example_List">
    <id type="talkgroup" value="1101" protocol="APCO25"/>
    <id type="record"/>
  </alias>
  <alias name="Talkgroup Block" color="0" group="Misc" list="Example_List">
    <id type="talkgroupRange" min="1200" max="1299" protocol="APCO25"/>
  </alias>
  <alias name="UNIT_A1" color="0" group="Radios" list="Example_List">
    <id type="radio" value="9000001" protocol="APCO25"/>
  </alias>
  <alias name="Ops 1" color="0" group="Ops" list="Example_Analog">
    <id type="broadcastChannel" channel="Example Feed"/>
  </alias>
  <channel name="Control" order="1" enabled="false" system="Example Network" site="Site 1">
    <decode_configuration type="decodeConfigP25Phase1" modulation="CQPSK" traffic_channel_pool_size="20"/>
    <aux_decode_configuration/>
    <event_log_configuration>
      <logger>CALL_EVENT</logger>
    </event_log_configuration>
    <source_configuration type="sourceConfigTunerMultipleFrequency" frequency_rotation_delay="1000" source_type="TUNER_MULTIPLE_FREQUENCIES">
      <frequency>851100000</frequency>
      <frequency>852200000</frequency>
      <frequency>853300000</frequency>
      <frequency>854400000</frequency>
    </source_configuration>
    <record_configuration/>
    <alias_list_name>Example_List</alias_list_name>
  </channel>
  <channel name="Control" order="2" enabled="true" system="Example Network" site="Site 1">
    <decode_configuration type="decodeConfigP25Phase1" modulation="C4FM" traffic_channel_pool_size="30"/>
    <source_configuration type="sourceConfigTunerMultipleFrequency" frequency_rotation_delay="400" source_type="TUNER_MULTIPLE_FREQUENCIES">
      <frequency>854400000</frequency>
      <frequency>852200000</frequency>
      <frequency>851100000</frequency>
      <frequency>853300000</frequency>
    </source_configuration>
    <alias_list_name>Example_List</alias_list_name>
  </channel>
  <channel name="Control" order="1" enabled="false" system="Neighbor Network" site="Site 2">
    <decode_configuration type="decodeConfigP25Phase1" modulation="C4FM"/>
    <source_configuration type="sourceConfigTunerMultipleFrequency" source_type="TUNER_MULTIPLE_FREQUENCIES">
      <frequency>770100000</frequency>
    </source_configuration>
  </channel>
  <channel name="Ops 1" order="1" enabled="true" system="Example Analog" site="Ops">
    <decode_configuration type="decodeConfigNBFM" bandwidth="BW_12_5" talkgroup="1"/>
    <source_configuration type="sourceConfigTuner" frequency="155100000" source_type="TUNER"/>
  </channel>
  <channel name="Ops 2" order="2" enabled="false" system="Example Analog" site="Ops">
    <decode_configuration type="decodeConfigNBFM" bandwidth="BW_25_0" talkgroup="2"
        squelchNoiseOpenThreshold="0.1" squelchNoiseCloseThreshold="0.19"
        squelchHysteresisOpenThreshold="4" squelchHysteresisCloseThreshold="6"/>
    <source_configuration type="sourceConfigTuner" frequency="155200000" source_type="TUNER"/>
  </channel>
  <channel name="Ops 3" order="3" enabled="false" system="Example Analog" site="Ops">
    <decode_configuration type="decodeConfigNBFM"/>
    <source_configuration type="sourceConfigTuner" frequency="460300000" source_type="TUNER"/>
  </channel>
  <channel name="Tower" order="1" enabled="false" system="Example Air" site="Field">
    <decode_configuration type="decodeConfigAM"/>
    <source_configuration type="sourceConfigTuner" frequency="118500000" source_type="TUNER"/>
  </channel>
  <channel name="Example DMR" order="1" enabled="false" system="Example Business" site="Plant">
    <decode_configuration type="decodeConfigDMR" traffic_channel_pool_size="20"/>
    <source_configuration type="sourceConfigTuner" frequency="464500000" source_type="TUNER"/>
  </channel>
  <stream type="broadcastifyConfiguration" feed_id="1" mount_point="/example" host="audio.example.org" port="80" password="not-a-password" name="Example Feed" enabled="false"/>
  <stream type="OpenMHzConfiguration" api_key="not-a-key" system_name="example" name="Example OpenMHz" enabled="false"/>
</playlist>"#;

    #[test]
    fn imports_a_multi_site_playlist() {
        let cfg = playlist_to_config(MULTI_SITE).expect("parse playlist");
        cfg.validate().expect("imported config is valid");

        // The two duplicate entries merge into one site; the neighbour is separate.
        assert_eq!(cfg.site.len(), 2, "sites: {:?}", cfg.site);

        let main = cfg
            .site
            .iter()
            .find(|s| s.system == "Example Network")
            .expect("Example Network site imported");
        assert!(main.enabled, "enabled (one of the two entries is)");
        assert_eq!(
            main.control_frequencies.len(),
            4,
            "the two entries list the same 4 frequencies in different orders"
        );
        let mut uniq = main.control_frequencies.clone();
        uniq.sort_unstable();
        uniq.dedup();
        assert_eq!(uniq.len(), 4, "control frequencies are unique");

        let neighbour = cfg
            .site
            .iter()
            .find(|s| s.system == "Neighbor Network")
            .expect("neighbour site imported");
        assert!(!neighbour.enabled, "disabled but still imported");
        assert_eq!(neighbour.control_frequencies, vec![770_100_000]);

        // NBFM channels survive even though most are disabled.
        let nbfm: Vec<_> = cfg
            .channel
            .iter()
            .filter(|c| c.mode == ChannelMode::Nbfm)
            .collect();
        assert_eq!(nbfm.len(), 3, "NBFM channels: {nbfm:?}");
        for c in &nbfm {
            assert!(c.frequency > 0, "channel '{}' has no frequency", c.name);
            assert_eq!(c.system, "Example Analog");
        }
        assert_placed(&cfg);

        // DMR is out of scope and must not be imported.
        assert!(
            !cfg.channel.iter().any(|c| c.name == "Example DMR"),
            "the DMR channel must be skipped"
        );

        // The emitted TOML must be loadable again: `[channel.squelch]` is a
        // sub-table, so it has to serialize after the channel's scalar fields.
        let out = config_to_toml(&cfg).expect("serialize");
        let reparsed: Config = toml::from_str(&out).expect("emitted toml reparses");
        assert_eq!(reparsed.site.len(), cfg.site.len());
        assert_eq!(reparsed.channel.len(), cfg.channel.len());
        assert_eq!(reparsed.enabled_sites(), cfg.enabled_sites());
    }

    #[test]
    fn emits_valid_toml() {
        let cfg = playlist_to_config(SAMPLE).expect("parse");
        let out = config_to_toml(&cfg).expect("toml");
        assert!(out.contains("[[site]]"));
        assert!(out.contains("control_frequencies"));
        // Round-trips back through the config parser.
        let reparsed: Config = toml::from_str(&out).expect("reparse");
        assert_eq!(reparsed.site.len(), 1);
    }
}
