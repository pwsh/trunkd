# Configuring trunkd

`trunkd run --config <toml>` is the whole recorder: it opens the SDRs, decodes the site, and
uploads every call. One process, one config file. This page explains that file. The fully
commented reference is [`trunkd.example.toml`](../trunkd.example.toml) at the repository root:
copy it, then change what this page tells you to.

Related pages: SDR-specific settings and gains in [sdr-hardware.md](sdr-hardware.md); the local
web panel in [panel.md](panel.md); what is sent upstream in [keyup.md](keyup.md); a second
recorder on the same site in [recorder-link.md](recorder-link.md).

## Where the file lives

* Linux: `/etc/trunkd/trunkd.toml` for the system service ([install-linux.md](install-linux.md)).
* Windows: `C:\trunkd\config\trunkd.toml` for the service ([install-windows.md](install-windows.md)).

The file holds your upload token, so keep it readable only by the account that runs trunkd
(`chmod 0600` on Linux). The panel's device editor preserves the file's mode when it rewrites it.

**Windows paths** in the TOML need forward slashes (`"C:/trunkd/spool"`) or doubled backslashes:
`"C:\trunkd"` is a TOML escape error.

**Environment overrides.** Any value can be overridden from the environment with a `TRUNKD_`
prefix and `__` for nesting: `TRUNKD_UPLINK__API_KEY=…`, `TRUNKD_UPLINK__SPOOL_ONLY=true`,
`TRUNKD_SERVER__BIND=0.0.0.0:8080`. This is a convenient way to keep the token out of the file
(for example in a systemd `EnvironmentFile=`). Relative paths in the config resolve against the
working directory; the Windows service sets that to the config file's directory.

**Starting from an sdrtrunk playlist.** `trunkd import-sdrtrunk <playlist.xml>` prints a
`trunkd.toml` equivalent of an sdrtrunk playlist's P25 systems to stdout, as a starting point.

**Validating.** `trunkd --config trunkd.toml check` parses and validates the file, prints the
result and the SDRs it can see, and exits without opening any of them.

## A minimal config

One tuner covering a whole 800 MHz site, uploading to Keyup:

```toml
[uplink]
url          = "https://keyup.example.org/api/call-upload"
api_key      = "…"                 # a Keyup recorder token with the `upload` scope
system_label = "Example County"    # what the server files calls under
site_label   = "Site 1"

[recorder]
instance_id = "rec-01"             # how this box is named upstream; hostname when unset

[[device]]
id               = "sdr0"
backend          = "soapy"
driver           = "airspy"
sample_rate      = 10000000
center_frequency = 856000000       # covers 851–861 MHz
[device.gains]
LNA = 12
MIX = 10
VGA = 12

[[site]]
system  = "Example County"
site    = "Site 1"
devices = ["sdr0"]
control_frequencies = [851200000, 852400000, 853600000]
```

Everything else has a default. The sections below describe each table.

**The span check.** Every enabled site's control frequencies must fall inside
`center_frequency ± sample_rate/2`, less a 1 % guard band at each edge. A frequency outside it is
a startup error rather than a recorder that hunts forever in silence.

## `[uplink]` — where calls go

| key | default | meaning |
| --- | --- | --- |
| `url` | `""` | The call-upload URL. Every Keyup route is derived from it ([keyup.md](keyup.md)). Empty means nothing is sent anywhere. |
| `api_key` | `""` | The upload token. |
| `enabled` | `true` | `false` drops calls after assembly and keeps the recorder wholly offline (no control plane either). |
| `system_label` | `""` | Sent as `systemLabel`. Must match the system name your server files talkgroups under. |
| `site_label` | `""` | Recorded on every call. |
| `system_id` | `1` | Numeric `system` field. Keyup accepts and ignores it. |
| `audio_format` | `"mp3"` | `"mp3"` or `"wav"`. |
| `mp3_bitrate_kbps` | `16` | MP3 constant bitrate. |
| `spool_only` | `false` | Boot value of the **upload hold**: spool calls, do not upload them. The control plane stays connected. See [operations.md](operations.md#bring-up-record-without-uploading). |
| `spool_path` | `"spool"` | Offline spool directory. |
| `spool_cap_bytes` | `1073741824` | 1 GiB; oldest-first eviction. |
| `spool_min_free_bytes` | `2147483648` | 2 GiB the spool's volume must keep free. `0` disables the floor. |
| `observations_interval_s` | `300` | Seconds between site-observation POSTs. `0` disables them. |
| `desired_prefetch_timeout_s` | `3` | How long to wait for the server's desired state before building the tuners. `0` disables the prefetch. |
| `stream` | `true` | Hold a WebSocket push channel open to Keyup so desired-state changes land at once. |
| `stream_reconnect_max_s` | `30` | Ceiling on that channel's reconnect backoff (it doubles from 1 s). |
| `direct_broadcastify` | `true` | May this box upload to Broadcastify Calls itself when Keyup pushes credentials? A veto, not a trigger: nothing happens until Keyup sends a `recorder.broadcastify` block. |

Pointing `url` at a plain Rdio Scanner server rather than Keyup works for call upload; see
[keyup.md](keyup.md) for which settings to turn off.

## `[[device]]` — one SDR

Each `[[device]]` is one tuner. The common keys:

| key | meaning |
| --- | --- |
| `id` | Your name for the tuner. Used in logs, the panel, metrics and `[[site]] devices`. |
| `backend` | `soapy` (default; Airspy, SDRplay, RTL-SDR and anything else SoapySDR drives), `hackrf`, or `iqfile` (playback of a recorded capture). |
| `sample_rate` | Samples per second. Must satisfy [the sample-rate rule](sdr-hardware.md#the-sample-rate-rule). |
| `center_frequency` | Hz. |
| `serial` | Pick a board by serial when several of the same kind are attached. |
| `role` | `auto` (default: control and voice in one tuner), `control`, `voice` or `data`. |
| `bands` | What part of the capture this tuner records traffic in; see below. |
| `ppm`, `auto_ppm`, `auto_ppm_limit` | Frequency correction; see [sdr-hardware.md](sdr-hardware.md#frequency-correction-ppm). `auto_ppm` defaults on, `auto_ppm_limit` to 5 ppm. |
| `ring_seconds` | SDR → DSP ring depth; see below. Default `1.0`. |
| `remote_grants` | Whether this tuner takes grants a **peer** recorder heard ([recorder-link.md](recorder-link.md)). |

Backend-specific keys (`driver`, `soapy`, `gain`, `[device.gains]`, `agc`, `bandwidth_hz`,
`rx_port`, `[device.settings]` for SoapySDR; `lna_db`, `vga_db`, `amp`, `portapack_port`,
`restore_portapack_on_exit` for HackRF; `path`, `pacing` for `iqfile`) are covered in
[sdr-hardware.md](sdr-hardware.md) and [capturing-test-iq.md](capturing-test-iq.md).

Devices can also be added and edited from the local panel, which writes this file for you
([panel.md](panel.md#setting-up-devices-from-the-panel)).

### `bands` — what a tuner is responsible for

A tuner's antenna and bandpass may only cover part of what its capture spans, and may cover
several disjoint pieces of it. `bands` says so:

```toml
[[device]]
id = "voice0"
bands = ["800", "851000000-856000000"]   # labels: vhf uhf 700 800 900, or explicit "lo-hi" Hz
```

Empty (the default) means "the whole usable capture". Each entry must resolve *and* overlap the
tuner's usable span, or startup fails — a band that overlaps nothing would silently unroute every
grant in it. Bands gate **traffic grants only**: control acquisition is bounded by the capture span
alone, so bands can never stop a tuner from finding its control channel. On a `control`-role tuner
they do nothing at all (a control front publishes every grant) and are warned about at load. On a
single-tuner site the one `auto` front records everything in its capture, so bands there do
nothing unless the `[link]` is on (they then decide which of a peer's grants this box will take);
they earn their keep on a multi-tuner site, where they are the partition the coordinator routes
by. Keyup can override `bands` at runtime ([keyup.md](keyup.md#per-tuner-fields)).

### Keeping the DSP thread fed

The SDR never pauses, so the only slack between it and a descheduled DSP thread is the ring. Two
knobs:

* **`[[device]] ring_seconds`** (default `1.0`, minimum `0.1`) — how much the ring holds, rounded
  up to a power of two of samples (1.68 s, about 256 MiB, at 20 Msps for the default), which is why
  it is a knob and not simply a large constant. A compiler or other heavy job running beside the
  recorder can stall the DSP thread for a couple of seconds; raise this on a box that shares its
  CPU with other work, and watch the `overflow` column of the status line to know whether you need
  to. On Windows, where there is no real-time priority, `2.0` is a sensible value.
* **Real-time scheduling (Linux)** — the DSP thread asks for `SCHED_FIFO` at the *lowest*
  real-time priority when it starts. This needs `CAP_SYS_NICE`; without it the call fails with
  `EPERM`, the recorder logs one line at info and carries on at normal priority. Nothing about
  recording depends on it. How to grant it is in [install-linux.md](install-linux.md#real-time-priority).

## `[[site]]` — one P25 site

| key | default | meaning |
| --- | --- | --- |
| `system` | — | System label (display, and how the link matches sites). |
| `site` | — | Site label. |
| `devices` | `[]` | The tuners serving this site. (`device = "x"`, a single scalar, is the older spelling; the panel migrates it to `devices` the first time membership changes.) |
| `enabled` | `true` | A disabled site stays in the file but is not decoded. |
| `control_frequencies` | `[]` | Control-channel candidates, in Hz. All are decoded at once; the first to yield five valid TSBKs under a consistent NAC wins, so order is preference only. |
| `traffic_pool_size` | `20` | Simultaneous traffic channels this site may follow. Raise it if the log says `traffic pool exhausted`. |
| `data_follow` | `false` | Follow SNDCP data-channel grants as well as voice. |

Where to get the control frequencies: RadioReference's page for your system lists every site's
control channels, and an sdrtrunk playlist carries them too (`trunkd import-sdrtrunk`). Keyup can
also replace a tuner's primary-site candidates at runtime.

## `[recorder]` — what stays on the box

Local options. Nothing here is sent anywhere except where it says so; what it costs is disk.

| key | default | meaning |
| --- | --- | --- |
| `instance_id` | hostname | Stable identifier for this recorder, sent upstream as `instance_id` and in `sourceQuality`. Give each recorder its own. At most 128 characters. |
| `event_log` | `true` | Write the raw non-voice event log at all ([operations.md](operations.md#the-raw-event-log)). |
| `event_log_dir` | `"events"` | Directory for it. Empty also disables it. |
| `event_log_keep_days` | `14` | Days kept; older files are deleted at rotation. `0` keeps forever. |
| `event_log_traffic` | `true` | Log traffic-channel signaling (link control, HDU, PDU, LSD) as well as the control channel. Voice is never logged either way. |
| `event_log_archive` | `false` | Upload each finished day's `.jsonl.gz` to Keyup once it is complete. Boot value only: Keyup can turn it on or off at runtime ([keyup.md](keyup.md#4-shipping-event-log-days)). |
| `radio_events` | `true` | Build and POST per-radio events ([keyup.md](keyup.md#3-radio-events)). |
| `event_spool_cap_bytes` | `67108864` | Cap on the undelivered radio-event spool. The `[uplink] spool_min_free_bytes` floor applies to it too. |
| `event_queue_depth` | `16384` | Depth of the DSP → observations/event-log queue, in events (`256`..=`1048576`). Drop-oldest: the status line's `event_log_dropped` counts what it sheds. Raise it if that counter climbs. ~112 bytes an event, so the default costs under 2 MiB. |
| `min_voice_frames` | `1` | Fewest voice frames a finished call must carry to be encoded and uploaded. A grant whose traffic channel never delivered anything ends as a properly identified call with no voice at all; encoding one produces an MP3 that is a LAME header and no audio, which servers reject. Those are counted as `calls.discarded_empty` (`counters.calls_discarded_empty` on the status POST, `discarded_empty` on the status line) and not sent. **Only the audio is dropped** — the call's observation and radio events are emitted as always. `0` uploads everything. |
| `follow_unit_calls` | `true` | Follow unit-to-unit (private) voice grants. On systems that carry private-call setup as multi-block **AMBT** messages (opcodes 0:4 and 0:6), these are decoded and followed like group calls: such a grant competes for a traffic channel exactly as an unpatched group call does, and the call is uploaded with `call_kind=unit` + `target_radio_id` instead of a talkgroup ([keyup.md](keyup.md#unit-to-unit-private-calls)). Counted as `calls.unit_started` / `calls.unit_ended`. Off leaves the grants decoded, counted and logged but unfollowed. Recorder-wide on purpose: one tuner recording half a conversation is not a useful setting. |
| `skip_talkgroups` | `[]` | Talkgroups this recorder never records: a grant for one of them claims **no tuner**, opens no call, uploads nothing and is not published to a sibling tuner or a link peer either. This is the operator's own half of the list; Keyup can push the other half ([keyup.md](keyup.md#recorderskip_talkgroups--talkgroups-not-to-record-at-all)) and the two are **unioned**. The grant is still decoded, logged and counted, and the radio events around it are archived as always: only the recording does not happen. Refusals are counted as `calls.skipped_by_policy`, once per call. A call already on the air when a talkgroup joins the list is allowed to finish; a **patched** call is skipped only when every member talkgroup is skipped too. |
| `location_bounds` | `[24.0, -125.0, 50.0, -66.0]` | Sanity box a decoded LRRP location must land in, as `[lat_min, lon_min, lat_max, lon_max]`. An **empty list** means no box; anything else must be four numbers making a real rectangle or the recorder refuses to start. The default is a generous continental-US rectangle — set your own if you are outside it. A refused fix is logged at DEBUG and counted in `lrrp.rejected.<reason>`. |
| `ppm_memory_path` | `<spool_path>/../state/ppm-memory.json` | Where each board's measured frequency error is remembered ([sdr-hardware.md](sdr-hardware.md#frequency-correction-ppm)). |

## `[server]` — the local panel and logging

| key | default | meaning |
| --- | --- | --- |
| `enabled` | `false` | Serve the local web panel ([panel.md](panel.md)). |
| `bind` | `"127.0.0.1:8080"` | Panel address. Loopback by default; `0.0.0.0:8080` exposes it, and then you should set `tokens`. |
| `tokens` | `[]` | Bearer tokens for the panel API. |
| `live_audio` | `false` | Opt-in live audio monitor stream. |
| `public_url` | derived from `bind` | How Keyup should link to this panel, e.g. `http://192.0.2.10:8080`. |
| `log_level` | `"info"` | `error`, `warn`, `info`, `debug` or `trace`. `RUST_LOG` wins. |
| `log_dir` | unset | Directory for daily-rolling log files. Console logging is always on. The Windows service defaults it to `logs` beside the config. |

## `[link]` — sharing a site with another recorder

Off by default. See [recorder-link.md](recorder-link.md). The link's UDP port defaults to
`0.0.0.0:47123`.

## `[health]`

| key | default | meaning |
| --- | --- | --- |
| `overflow_warn_per_sec` | `10000` | Overflow samples per second above which a device is flagged degraded. |
| `control_sync_timeout_secs` | `30` | Seconds without control-channel sync before the site hunts its alternate control frequencies. |

## Station self-description

`[station]` and the per-`[[device]]` description fields are where you write down the standing
facts about a recorder: where it is and what is on the mast. When several recorders hear the same
site, the server has to pick whose audio to keep; that decision is partly live link quality and
partly standing fact — a good antenna high on a hilltop beats a desk whip in a basement, whatever
either reports this minute.

**It is display and telemetry only.** Nothing in either block changes what trunkd tunes, decodes,
or records. It travels *up* on the tuner-status POST and never comes back down.

| block | keys |
| --- | --- |
| `[station]` | `name`, `location`, `latitude`, `longitude`, `elevation_m`, `contact`, `notes` |
| `[[device]]` | `receiver`, `antenna`, `cable`, `amplifier`, `height_m`, `notes` |

```toml
[station]
name      = "Example rooftop"
location  = "Example County, roof of building 4"
latitude  = 40.0
longitude = -95.0
contact   = "N0CALL"

[[device]]
id       = "sdr0"
receiver = "Airspy R2"
antenna  = "800 MHz collinear"
cable    = "10 m LMR-400"
height_m = 12.0
```

Text is capped at 256 characters (`notes`: 1024) when the config is read. `latitude` must be within
-90..=90 and `longitude` within -180..=180 — out of range is a **startup error, never a clamp**.
`amplifier` is prose about the external chain, and is *not* the same knob as `amp`, which is the
HackRF's own +14 dB front-end amplifier. Publishing a station's exact location is your choice;
`location` can be as vague as you like.

## What can be changed at runtime

Three writers can change a running recorder, and each owns different things:

* **The TOML file** is what hardware exists. It is read at start and on a panel *reload*; nothing
  else writes it except the panel's device editor.
* **Keyup's desired state** is the runtime override for tuner settings (gain, ppm, bands, centre
  frequency, role, enabled, …) and recorder-wide settings (upload hold, event-log archive, link,
  skip list, Broadcastify). It is never written back to the file. See
  [keyup.md](keyup.md#6-desired-state--what-keyup-can-push-down).
* **The local panel** restarts a tuner, holds or releases uploads, and edits `[[device]]` tables.
  See [panel.md](panel.md).
