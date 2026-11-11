// SPDX-License-Identifier: GPL-3.0-or-later
//! Import an sdrtrunk playlist XML into an equivalent `trunkd.toml`.
//!
//! Parses the elements named in ARCHITECTURE (`<channel>` with
//! `sourceConfigTunerMultipleFrequency` / `sourceConfigTuner` sources and
//! `decodeConfigP25Phase1` / `decodeConfigNBFM` / `decodeConfigAM` decoders) and
//! produces devices/sites/channels. Alias data and other elements are ignored.
//!
//! sdrtrunk playlists don't record concrete tuner hardware, so a single
//! placeholder Soapy device is synthesized and referenced by everything.

use serde::Deserialize;
use trunkd_core::{Error, Result};

use crate::config::{Backend, Channel, ChannelMode, Config, Device, PacingName, Site, Squelch};

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

    /// Squelch settings, present only when the playlist carried all four
    /// attributes.
    fn squelch(&self) -> Option<Squelch> {
        Some(Squelch {
            noise_open: self.squelch_noise_open?,
            noise_close: self.squelch_noise_close?,
            hysteresis_open: self.squelch_hysteresis_open?,
            hysteresis_close: self.squelch_hysteresis_close?,
        })
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
                    device: PLACEHOLDER_DEVICE.to_owned(),
                    frequency: freq,
                    mode: if is_am {
                        ChannelMode::Am
                    } else {
                        ChannelMode::Nbfm
                    },
                    enabled,
                    bandwidth_hz: decode.bandwidth_hz(),
                    talkgroup: decode.talkgroup,
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
/// control channels.
///
/// An sdrtrunk playlist says nothing at all about the SDR: it is a list of
/// frequencies. But trunkd will not start a site whose control channels fall
/// outside its tuner's capture, and a single 2.4 MHz placeholder could not hold
/// even one 700 MHz site — let alone the 700 and 800 MHz sites a real playlist
/// mixes. So the importer derives what it *can* derive: one 20 MHz tuner per
/// site, centered between that site's lowest and highest control channel. The
/// operator still edits `backend`/`driver`/gain, but the config that comes out
/// of `trunkd import` describes a receiver that could actually hear the sites
/// it lists.
fn place_devices(cfg: &mut Config) {
    if cfg.site.is_empty() {
        // Analog-only playlist: keep the single placeholder, centered on the
        // channels it will demodulate.
        let freqs: Vec<u64> = cfg.channel.iter().map(|c| c.frequency).collect();
        cfg.device[0].center_frequency = Some(span_center(&freqs).unwrap_or(DEFAULT_CENTER_HZ));
        return;
    }

    let template = cfg.device[0].clone();
    let mut devices = Vec::with_capacity(cfg.site.len());
    for (i, site) in cfg.site.iter_mut().enumerate() {
        let id = format!("sdr{i}");
        devices.push(Device {
            id: id.clone(),
            center_frequency: span_center(&site.control_frequencies).or(Some(DEFAULT_CENTER_HZ)),
            ..template.clone()
        });
        site.device = id;
    }
    // Analog channels stay on the first tuner; a playlist gives no way to know
    // which capture they belong in, and the operator is editing these anyway.
    cfg.device = devices;
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
        // channels so the imported config actually validates.
        assert_eq!(cfg.device.len(), cfg.site.len());
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
        let sq = disabled.squelch.expect("squelch parsed");
        assert_eq!(sq.noise_open, 0.1);
        assert_eq!(sq.noise_close, 0.19);
        assert_eq!(sq.hysteresis_open, 4);
        assert_eq!(sq.hysteresis_close, 6);

        // A channel with no bandwidth attribute defaults to narrowband.
        let fire = cfg
            .channel
            .iter()
            .find(|c| c.name == "Fire Dispatch")
            .expect("fire channel");
        assert_eq!(fire.bandwidth_hz, 12_500);
        assert!(fire.squelch.is_none());
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
        }

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
