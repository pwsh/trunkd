# Talking to Keyup: what trunkd sends and what it accepts

trunkd records calls; it does not store, search, transcribe or publish them. It hands each
finished call to a server over HTTP. This page describes that conversation from the recorder's
side: the per-call upload, the side channels (observations, radio events, event-log days), and
the **control plane** through which a server can manage the recorder at runtime.

**Which server.** Two levels of compatibility:

* **Plain call upload works with any Rdio-Scanner-compatible server.** The per-call POST to
  `/api/call-upload` is the Rdio Scanner upload contract in its trunk-recorder dialect. Point
  `[uplink] url` at any server that accepts it and calls arrive with talkgroup, radios, timing and
  audio.
* **Everything else needs [Keyup](https://github.com/pwsh/keyup).** Site observations, radio
  events, event-log archiving, tuner status, desired state, the push channel, skip lists and
  direct-to-Broadcastify delivery are Keyup routes. Against another server they are 404s, which
  trunkd logs at debug and otherwise ignores: recording is never affected. To keep a recorder
  quiet towards a plain Rdio Scanner server, set `[uplink] observations_interval_s = 0`,
  `stream = false`, `desired_prefetch_timeout_s = 0` and `[recorder] radio_events = false`. The
  tuner-status POST and the desired-state poll still go out every 10–15 s and get a 404 each, which
  is harmless.

Every Keyup route is derived from `[uplink] url` by replacing its `/api/call-upload` path with the
route's own, so one URL configures all of them. A URL without that path has the route appended.
All of them authenticate with `Authorization: Bearer <[uplink] api_key>` (the call upload also
sends it as the `key` form field). In Keyup that key is a recorder token with the `upload` scope;
see Keyup's recorder documentation for minting one.

The local side of the recorder (its own panel and HTTP routes) is in [panel.md](panel.md); the
settings named here are in [configuration.md](configuration.md).

---

## 1. The call upload

One `multipart/form-data` POST per call to `[uplink] url`, normally
`https://keyup.example.org/api/call-upload`. Keyup reads the trunk-recorder dialect whenever
`sources` is present.

### Audio format

Every call is encoded as **MP3, 8 kHz mono, CBR 16 kb/s** by default (`[uplink] audio_format`,
`mp3_bitrate_kbps`). MP3 is the one format every consumer in and around this pipeline takes
(Rdio Scanner servers, Keyup, OpenAI-compatible Whisper servers, Broadcastify Calls and OpenMHz),
and at 16 kb/s it is 8× smaller than the equivalent WAV with no audible loss on IMBE voice. No ID3
frames are written; all metadata travels in the form fields. `audio_format = "wav"` sends 8 kHz
16-bit WAV instead.

If you also feed a call-sharing service from another recorder (sdrtrunk, trunk-recorder), do not
enable a second uploader for the **same system** on that service: two uploaders for one system
collide.

### Fields

`key`, `system` (numeric, ignored by Keyup), `systemLabel` (`[uplink] system_label`), `dateTime`
(call start, unix s), `startMs` / `endMs` (the same call start and its end, unix ms — what Keyup
reconciles two recorders' copies on; `dateTime` stays whole seconds for older servers),
`audio` (the MP3; omitted for encrypted calls), `audioName`, `audioType` (`audio/mpeg`),
`talkgroup`, `talkgroupLabel`, `frequency` (Hz), `source` (first radio), `sources` (JSON
`[{pos, src, emergency, tag}]` — every keying radio with its offset and, on systems whose radios
transmit one, the ASCII talker alias), `frequencies` (JSON), `patches` (JSON member list),
`emergency`, `encrypted`, `talkerAlias`, and extras Keyup keeps in `raw_meta`: `recorder=trunkd`,
`site`, `nac`, `algorithm` (`AES-256`, `UNENCRYPTED`, …), `keyId`, `durationMs`, `speechS`, and —
when the air qualified the talker — `sourceWacn` / `sourceSystem`, plus `sourceQuality` when this
recorder has anything to say about its own copy (below).

**Talkgroup names.** Keyup names talkgroups from its own directory, keyed on the system label
(`systemLabel`). trunkd sends `talkgroupLabel` when it knows one, but Keyup never overwrites a
directory alias with an upload's label. Import your talkgroup list into Keyup (from
RadioReference or an sdrtrunk playlist) and keep `[uplink] system_label` equal to the system name
used there.

**`speechS`** is seconds of audio above **−40 dBFS**, to two decimals: the recorder cuts the call's
8 kHz PCM into 20 ms frames, counts the frames whose RMS clears the floor, and multiplies by 0.02.
It is the cheap half of Keyup's own speech/page/none classification (Keyup also excludes tone
spans first; the recorder does not detect tones, so a page's tones count here as loud audio), sent
so Keyup can cross-check its verdict against what the recorder measured — a server that does not
know the field ignores it. It is always sent (`0.00` for an encrypted, audio-less call) and is
written into the spool sidecar, so a call retried out of the spool carries the number the live
attempt measured rather than reading as silence.

**`sourceWacn` / `sourceSystem`** come from the P25 **Source ID Extension** link-control word
(LCO 9), which a roaming subscriber's radio sends alongside the ordinary group-voice word. LCO 0
carries only a 24-bit radio id, which is ambiguous across systems; LCO 9 qualifies it into a full
`WACN.SYSTEM.ID`. The two fields are **absent for a local radio**, which is the signal that it is
local. See [NON-VOICE-DATA.md](NON-VOICE-DATA.md) for how often this word appears.

### Unit-to-unit (private) calls

A private call has no talkgroup to be filed under, so it goes up with a different pair of fields
and **no `talkgroup` and no `talkgroupLabel` at all**:

| field | value |
| --- | --- |
| `call_kind` | `unit` — absent on a group call, and absence means `group` |
| `target_radio_id` | the radio being called (unit B), decimal |
| `source` / `sources` | the radio doing the talking (unit A), as always |
| `audioName` | `<system>_UNIT_<A>_TO_<B>_<start-unix-ms>.mp3` — the pair stands where the talkgroup id would be, so no private call can collide with a real group |

Every other field is exactly the group contract. Both fields are additive: a server that predates
them ignores them, and a group call's body is unchanged. The spool's JSON sidecar carries the same
two keys, so a private call that waits out an outage is retried as a private call and not as a
group call on talkgroup 0.

Following these is `[recorder] follow_unit_calls` (default on). See also the `private_call` radio
event ([§3](#3-radio-events)) and the `private_call` event-log record
([operations.md](operations.md#the-raw-event-log)), which are emitted whether or not the call
carried audio worth uploading.

### Conventional (analog) channel calls

A call recorded on a `[[channel]]` (analog NBFM, see
[configuration.md](configuration.md#channel--one-conventional-analog-channel)) goes up as an
ordinary **group call**, so a server files, searches and names it exactly like a trunked one. No
server change is needed: the fields that differ are these.

| field | value |
| --- | --- |
| `systemLabel` | the channel's `system` (default `[uplink] system_label`) |
| `site` | the channel's `site` (default `[uplink] site_label`, then the device id) |
| `talkgroup` | the channel's `talkgroup` (default the frequency in kHz, `154250` for 154.250 MHz) |
| `talkgroupLabel` | the channel's `name` |
| `frequency`, `frequencies` | the channel frequency, one segment |
| `sources` | `[]`, and no `source` field: analog carries no radio ids |
| `audioName` | `<system>_<talkgroup>_<start-unix-ms>.mp3`, the same stem as any group call |
| `mode` | `nbfm` |
| `decoder` | `NBFM` (what sdrtrunk's ID3 tag carried for the same calls) |
| `tone` | the configured tone, canonical spelling (`167.9`, `D445N`); absent for carrier squelch |
| `toneDetected` | the tone heard for most of the call, if any — on a carrier-squelch channel too, which listens for every CTCSS and DCS tone. A DCS code is reported as its normal-polarity spelling (`D445I` is heard as `D043N`, the same signal on air). |
| `squelchReason` | why the call ended: `squelch_closed`, `tone_lost`, `max_duration`, `shutdown`, or `orphaned` |
| `snrDb` | channel power above its noise floor when the squelch opened, dB |
| `peakDeviationHz` | largest deviation during the call, Hz |
| `offsetHz` | mean carrier offset from the channel centre, Hz — a channel-only tuner has no automatic PPM correction, so this is where drift shows |

The three measurements carry one decimal and are absent when not measured. `durationMs`,
`speechS`, `recorder=trunkd` and `sourceQuality` are sent as on any call (`voice_frames` is `0`:
there is no vocoder). Keyup keeps the new fields in `raw_meta`; a server that does not know them
ignores them. The spool sidecar carries the same extras under the same names, with
`"audio_type": "analog"` (trunk-recorder's word; P25 calls stay `digital`), so a call retried out of
the spool is retried with them. `[recorder] skip_talkgroups` never applies to these calls.

### `sourceQuality` — which recorder heard this, and how well

Several trunkd instances can hear one P25 site, so Keyup can be offered the same call twice.
`sourceQuality` is the JSON object that lets it keep the *better* copy rather than picking blind:

```json
{"instance": "rec-north-01", "voice_frames": 1234, "bad_frames": 7, "rssi_dbfs": -38.4}
```

| key | meaning |
|---|---|
| `instance` | `[recorder] instance_id` (hostname when unset) — the same id the tuner-status POST reports. Two copies are worth comparing only when this differs. |
| `voice_frames` | IMBE frames this recorder decoded into audio for the call. An encrypted call reports `0`: its ciphertext is discarded, not synthesized. |
| `bad_frames` | How many of those the vocoder *muted* — its running error rate was past repair, or the frame had been repeated to the limit. `bad_frames / voice_frames` is a like-for-like error rate between recorders. |
| `rssi_dbfs` | The capture device's **wideband** level (`device.{id}.rssi_dbfs`) read at call end. Receiver context, not this channel's SNR: it says whether a tuner is deaf or in compression, not that this call was clean. Absent when the device has never measured a level — 0 dBFS is full scale, not "unknown". |

Everything in it is measured, and anything trunkd cannot measure is absent rather than
approximated. There is deliberately **no `gap_ms`**: the assembler appends vocoded frames back to
back and never pads or skips (the voice queue is a blocking send, so no frame is lost between the
DSP thread and the call thread), so there is no discontinuity to report.

The field is additive. It is omitted when there is nothing to report, a server that does not
declare it ignores it, and the spool sidecar carries the same object, so a call that waits out an
outage is retried with exactly the facts it would have carried live.

### Encrypted calls, and calls with no audio

Encryption on P25 covers only the IMBE payload; the HDU, Link Control, Encryption Sync and LSD are
always in the clear, so **every** call is logged with talkgroup, radios, timing, algorithm and key
id. An encrypted call is posted with `encrypted=1` and **no audio**; Keyup stores it with
`audio_state='purged'` (410 on the audio route, still searchable and correlatable). trunkd never
attempts to decrypt anything, and never stores ciphertext.

**A clear call with no audio is never posted.** A server can only answer `400 incomplete call
data: no audio`. Such a call is logged at info with its talkgroup, source, voice-frame count and
end reason, and counted in the status line as `dropped_no_audio`. An *encrypted* call with no
audio is still posted, since its metadata is the whole point. A call that never carried a single
voice frame is dropped earlier still, by `[recorder] min_voice_frames`.

### Response handling

| server answer | what trunkd does |
| --- | --- |
| `Call imported successfully.` / `duplicate call rejected` | done |
| `incomplete call data: …` | dropped, with an error log |
| `401 invalid key` | logged once, spooled, 60 s back-off |
| anything else (5xx, timeouts, server down) | spooled and retried |

The spool is `spool/YYYY/MM/DD/<call>.mp3` + `.json` (a trunk-recorder-style sidecar, so a file
ingest that reads trunk-recorder output can read it too), retried oldest-first every 30 s, capped
at `spool_cap_bytes` and floored at `spool_min_free_bytes` of free space on the volume. See
[operations.md](operations.md#the-spool).

---

## 2. Site observations

Every `[uplink] observations_interval_s` (default 300; `0` disables), trunkd POSTs what the control
channel says the site *is* — WACN, system id, NAC, RFSS/site/LRA, the observed band plan,
secondary control channels, the traffic frequencies actually granted, and the neighbour sites — to
`/api/recorder/observations`. Keyup reconciles that against its configured sites (and, if you use
it, RadioReference). A server without the route answers 404, which is logged at debug; recording
is unaffected either way.

Both the observations and the radio-event bodies carry `recorder: "trunkd"` (the software) and
`instance` (*which* trunkd — the same id the tuner-status POST sends as `instance_id`), because
several recorders may hear one site.

The observations body also carries the window's **non-voice histogram**, which is how the site's
message mix is tracked without storing millions of TSBKs a day:

| key | shape | what it says |
| --- | --- | --- |
| `message_mix` | `{"<mfid>:<opcode>": count}`, plus `{"ambt:<mfid>:<opcode>": count}` | Every TSBK this window, parsed or not. The `ambt:` keys are the multi-block trunking messages (PDU format 0x17), censused from their header's own MFID and opcode — they never reach the TSBK path, so they would otherwise show only as an undifferentiated `pdu_headers` total. |
| `unknown_exemplars` | `[{mfid, opcode, count, exemplar}]` | One raw hex exemplar per opcode the engine does not parse, ≤ 20. This is what would show the day a vendor beacon changes. |
| `link_control_mix` | `{"<vendor>:<lco>": count}` | Traffic-channel link control, same shape. |
| `data_channel` | `{grants, pages, pdu_headers, pdu_blocks_valid}` | SNDCP packet-data activity. |
| `lsd_nonzero` | count | Low-speed-data octet pairs that were not all zero. Many systems send almost none; the counter exists so "this site does not use LSD" stays a measurement rather than an assumption. |

All five reset every window, so each report describes its own interval.

**What gets into the band, neighbour and secondary-control tables.** A TSBK that passes CRC is not
always a TSBK that was sent: on a weak site a few flipped bits can still close the CRC, and one
corrupt `IDEN_UPDATE` could otherwise publish a nonsense band for the rest of the run. Three rules
govern those three tables (identity, the site's own control channel and the traffic list are
unaffected):

* **Plausible or dropped.** A band whose base frequency is outside 100 MHz–1 GHz, whose channel
  spacing is not a standard multiple (2.5 / 5 / 6.25 / 12.5 / 25 kHz, at most 100 kHz) or whose
  bandwidth is not a multiple of 1.25 kHz is discarded on sight, as is an adjacent site or
  secondary control channel whose control frequency resolves outside 100 MHz–1 GHz. Each
  rejection is one `DEBUG` line naming the block's hex, so `RUST_LOG=trunkd_p25=debug` shows what
  a weak site is throwing away.
* **Seen twice before published.** Everything else is a candidate first: it appears in the
  observation only once the byte-identical decode has been heard **twice** — and for an adjacent
  site, twice within **30 minutes**. A real site repeats all three tables every few seconds, so
  this costs seconds of latency; a corrupt block would have to repeat bit for bit to survive it.
  Candidates are held in a bounded map (64 per table, least-recently-seen dropped), so noise
  cannot grow memory.
* **Gone after an hour of silence.** An admitted entry that has not been re-announced for
  **60 minutes** of lock leaves the snapshot and is reclaimed at the next window boundary. A band
  that expires is re-admitted by a fresh pair of sightings the moment it is heard again.

The confirmation counts and timeouts live beside the rules in
`crates/trunkd-p25/src/inventory.rs` (`CONFIRMATIONS`, `ADJACENT_CONFIRM_WINDOW_MS`,
`ENTRY_TTL_MS`, `PENDING_CAP`).

---

## 3. Radio events

Every 10 s or 500 events, whichever comes first, trunkd POSTs what the control channel said about
individual *subscribers* to `/api/recorder/events` (`[recorder] radio_events`, default on):

```jsonc
{ "recorder": "trunkd", "instance": "rec-north-01", "recorder_version": "0.0.1",
  "system": { "label": "Example County", "wacn": 781824, "system_id": 1, "nac": 291 },
  "site":   { "label": "Site 1", "rfss": 1, "site": 1 },
  "events": [ { "at": "2026-01-15T09:12:03.481Z", "kind": "group_affiliation",
                "opcode": 40, "mfid": 0, "radio_id": 1000001,
                "radio_wacn": null, "radio_system": null, "target_radio_id": null,
                "talkgroup": 1001, "result": "accepted", "frequency_hz": null,
                "meta": { "global": true, "announcement_group": 65535 } } ] }
```

Expected reply: `200 {"stored": n}`.

`kind` is one of `unit_registration`, `unit_deregistration`, `unit_registration_command`,
`group_affiliation`, `group_affiliation_query`, `location_registration`, `sndcp_grant`,
`sndcp_page`, `ack`, `deny`, `queued`, `location`, `private_call`. `result` is `accepted` /
`failed` / `denied` / `refused` where the message carries one. Absent fields are explicit `null`s,
not omitted keys: "we did not hear one" is the fact being reported.

`private_call` is emitted once per finished unit-to-unit call: `radio_id` is the talker,
`target_radio_id` the radio called, `frequency_hz` the granted channel, and `meta` carries
`started_at_ms` and `duration_ms`. It is sent whether or not any audio was worth uploading, so a
two-party conversation always leaves a trace.

Volume is typically hundreds of events an hour on a busy site — orders of magnitude below the TSBK
rate, and the best available answer to "which radios are *here*", because a radio that is on the
air but never keys up is invisible to the call archive.

**On failure** — including the `404` a server without the route answers, which is warned about
exactly once and then logged no further — the batch is written to `spool/events/YYYY/MM/DD/*.json`
(the body verbatim, temp-and-renamed) and retried oldest-first every 30 s, one batch per pass so a
backlog cannot starve live events. The directory is capped at `[recorder] event_spool_cap_bytes`
with oldest-first eviction, and is swept for the `[uplink] spool_min_free_bytes` floor on the same
timer.

> **Privacy.** This is *presence* data about identifiable people, denser than what the call
> archive holds, and deserves a shorter retention than the audio. Turn it off with
> `[recorder] radio_events = false` if you have no use for it.

---

## 4. Shipping event-log days

The raw event log ([operations.md](operations.md#the-raw-event-log)) stays on the box by default.
`[recorder] event_log_archive = true` sends each day to Keyup once it is **complete**, which is the
moment the rotation has gzipped it. Never the open day, and never on the DSP thread — this runs on
its own thread beside the observations one.

```
POST {keyup}/api/recorder/event-log        Authorization: Bearer <uplink api_key>
multipart/form-data:
  instance  the recorder's instance id (same value the status POST sends as instance_id)
  date      YYYY-MM-DD
  sha256    lower-hex SHA-256 of the gz exactly as it sits on disk
  bytes     its length
  file      the .jsonl.gz  (filename "<date>.jsonl.gz", Content-Type application/gzip)
```

A day in several files (`<date>.jsonl.gz` plus `<date>.1.jsonl.gz`, …) is **one** upload: the
parts concatenated byte for byte in that order, which is itself a valid multi-member gzip, with
`sha256`/`bytes` of the concatenation. Keyup replaces a stored day when the same date arrives with
a different digest, so uploading a part on its own would replace the day with only its late
lines. A part that appears after the day was archived makes the day pending again and the whole
day is sent once more; every part is marked only when Keyup has accepted the upload. The body is
streamed from the files, never held in memory.

**Expected replies.** Any `2xx` means Keyup has it; `409` means Keyup *already* has it, which is
the same outcome and is treated as success. Anything else is a failure: the file is kept and the
upload retried with a backoff that doubles from 60 s to a one-hour cap.

On success a marker is written beside the day holding Keyup's reply verbatim:

```text
events/2026-01-14.jsonl.gz
events/2026-01-14.jsonl.gz.archived     {"stored":true,"lines":2300000}
```

That marker is the only state this keeps, and it is why the startup scan is the *mechanism*
rather than a special case: the directory is rescanned for finished `.gz` files without a marker,
so days that finished while the recorder was down, or while Keyup was unreachable, are picked up
with no bookkeeping to go stale. Deleting a marker re-uploads that day. The `.gz` itself is never
deleted by the archive — retention stays `event_log_keep_days`' business.

The **uplink hold** applies: a recorder in `spool_only` uploads nothing, archives included, and
resumes when the hold is released. `archive` is also a **runtime** setting: Keyup's desired
`recorder.event_log_archive` turns it on or off without a restart, and `source` says whether the
value in force came from the TOML (`config`) or from Keyup (`keyup`). The status POST and
`/api/status` report:

```json
"event_log": {"archive": true, "source": "keyup", "pending": 2,
              "last_archived": "2026-01-14", "last_error": null,
              "last_written_at": "2026-01-15T05:28:50.230Z"}
```

`pending` is counted even while held, so a recorder sitting on a fortnight of unshipped days says
so rather than going quiet.

`last_written_at` is the timestamp of the newest line the log actually took, and is `null` when the
log is off or nothing has been written since the process started. A `last_written_at` that has
stopped advancing while the tuner is up and locked means events are not reaching the log; one that
stopped at the same second a tuner went idle or was disabled means there are no events to write,
which is a different fault entirely (see `tuners[].enabled`, below).

---

## 5. Tuner status

About every 15 s the recorder POSTs one status document to `/api/recorder/tuners/status`: its
`instance_id`, version, the upload hold (`uplink`), the event-log state, the recorder-wide
`counters` (`calls_started`, `calls_ended`, `calls_discarded_empty`, `uploaded`, `spooled`,
`failed`, `spool_bytes`, `disk_free_bytes`), the `[station]` description, the link state
([recorder-link.md](recorder-link.md)), the local panel's address (`monitor`, see
[panel.md](panel.md#live-audio-monitor)) and one entry per tuner. The tuner entries are built by
the same code as the local panel's `/api/status`, so the two can never disagree.

Each tuner entry carries the *effective* value of every desired-state field ([§6](#6-desired-state--what-keyup-can-push-down)),
the `[[device]]` description fields, and what the recorder measures for itself:

* `tuners[].rssi_dbfs` — wideband received level across the whole capture, measured in the DSP
  thread (one strided pass over each block, smoothed, published once a second). It is
  **ADC-referenced**, not dBm: full scale is a sample riding the converter's rails, so it moves
  with the gain settings, which are reported beside it for exactly that reason. A silent capture
  reads the −120 dBFS floor rather than `-inf`.
* `tuners[].freq_error_hz` — how far off frequency the locked control channel's **carrier**
  actually is, from the carrier meter: the same measurement the auto-PPM loop corrects on.
  Reported only for the tuner that follows control on a site, and absent while that site is still
  hunting. This is the number to reach for when a board will not acquire.
* `tuners[].pll_error_hz` — the demodulator loop's own phase-error reading. It is not a carrier
  measurement (see `crates/trunkd-p25/src/dsp/carrier.rs`) and can read a steady few hundred Hz on
  a healthy, locked tuner. Kept as a diagnostic of the demodulator.
* `tuners[].snr_db` — the locked control channel's signal-to-noise ratio in dB: the control
  channel's own mean power minus the capture's noise floor, both measured by that tuner's
  filterbank on the same samples. **Absent unless that tuner holds a control lock.** This is the
  field that separates "the antenna is deaf" from "the antenna is fine and the band is loud".
* `tuners[].noise_floor_dbfs` — the floor `snr_db` was measured against: the 20th-percentile
  channelizer bin, ADC-referenced like `rssi_dbfs`. Present exactly when `snr_db` is. A rising
  floor at an unchanged `snr_db` is interference or a gain setting, not a fading site.
* `tuners[].tsbk_pass_pct` — the share of control-channel TSBKs that passed CRC over the last 5 s
  stats window, 0–100. Absent unless locked, and absent in a window that carried no TSBK at all.
  It is the only field that tells a control channel whose *bits are arriving damaged* from one
  that is simply quiet.
* `tuners[].sync_losses_per_min` — control-channel framer sync losses over the last window,
  scaled to a minute. Absent unless locked; `0` is the healthy reading and is reported.
* `tuners[].clock_resyncs`, `last_clock_resync_ms`, `stream_gaps`, `clock_error_ms`,
  `clock_trim_ms` — the stream clock's health; see [operations.md](operations.md#timestamps-and-the-host-clock).
* `tuners[].enabled` — whether the tuner is switched on at all. `false` is Keyup's desired
  `enabled: false`: the unit idles with its board open, decodes nothing, records nothing and
  writes no event log, all by request. It is reported plainly because every other field of a
  disabled tuner looks exactly like a broken one.

**All of these are per tuner**, including `tsbk_rate`, `freq_error_hz` and `pll_error_hz`: with
two tuners on one site each publishes its own `device.{id}.*` copy. A tuner serving several sites
publishes the one it is actually following — the locked site, or the first site it was built with
while none is locked.

Every one of these is a measurement, so all obey the same liveness gate as `locked`, including
`tsbk_rate`, which reports `0` rather than its last live figure: a tuner whose stream heartbeat
has gone stale reports none of them. Unmeasured fields are **omitted**, never sent as `null` or
as a zero.

Further per-tuner fields:

* `backend` — `soapy` | `hackrf` | `iqfile`.
* `gain_elements` — `[{"name","min","max","step"}]` from the Soapy open, so a GUI offers the
  board's real controls. Empty for `hackrf` and `iqfile`, and until a stream has come up.
* `auto_ppm_applied` — the correction the closed loop has actually applied, in ppm. Omitted when
  nothing has been corrected. Distinct from `auto_ppm`, which is the switch.
* `ppm_memory` — what this *board* remembers of its own frequency error:
  `{"ppm": <total, config ppm included>, "measured_at": <RFC 3339>, "applied_at_open": <bool>}`,
  or `null` for a board never measured. Always present: `null` is a real answer.
  `applied_at_open` is false when `auto_ppm` is off. Pushing an explicit `ppm` resets the entry
  to that value, so an operator's number wins over stale history.
* `span_hz` — of the sample rate in force, not the TOML's.
* `bands` — the bands in force (omitted when the tuner is unrestricted).
* `fault` — present only while the tuner's unit is failing (most often: it cannot open its
  device) and being retried:
  `{"state": "restarting"|"retrying", "error": <text>, "failures": <n>, "next_attempt_ms":
  <unix ms>, "version": <the desired version the failing build carries>}`. `applied_version`
  stays at the last version that actually opened, so a pushed change that cannot open stays
  **pending** on Keyup — also across a recorder restart — with this saying why.
* `capabilities` — `{"runtime_gain", "runtime_ppm", "restart_on": [...]}`: which pushed fields
  apply live and which restart the tuner. `restart_on` is backend-specific: an Airspy is never
  offered `lna_db`, and a HackRF is never offered `rx_port`.
* `overrides` — the TOML field names whose value in force came from Keyup's desired state rather
  than the file, plus `file_role` when `role` is one of them. A field Keyup supplied that
  *matches* the file is not listed; `site_id` always is, because the file has nowhere to write
  one. Sent always, empty array included, so a reader can tell "this tuner runs exactly what the
  file says" from "this recorder is too old to tell you".

### Conventional channels

A tuner that records conventional NBFM [`[[channel]]`s](configuration.md#channel--one-conventional-analog-channel)
adds a `channels` array to its entry, one object per channel in config order (at most 64; `name`
and `system` capped at 64 characters):

```json
"channels": [
  { "index": 0, "name": "Public Works", "system": "Example County",
    "frequency_hz": 154000000, "tone": "167.9", "talkgroup": 154000,
    "state": "open", "open": true, "snr_db": 21.5, "power_dbfs": -42.0,
    "noise_floor_dbfs": -63.5, "offset_hz": -120.0, "calls": 4, "calls_too_short": 1,
    "tone_mismatch": 0, "false_opens": 2, "last_call_end_ms": 1788499700000 }
]
```

`tone` is the canonical CTCSS frequency or DCS code, `null` for carrier squelch. `state` is
`closed` | `opening` | `open` | `hang`, or `idle` while the tuner is not streaming or is disabled;
the live measurements (`snr_db`, `power_dbfs`, `noise_floor_dbfs`) obey the tuner's liveness gate
and are omitted while `idle`. `offset_hz` (the last call's carrier offset) and `last_call_end_ms`
(unix ms) are omitted before the first call. The counters run from the tuner's last start. Field
meanings are spelled out in [panel.md](panel.md#conventional-channels).

The field is **additive and omitted entirely** on a tuner with no channels, so a P25-only recorder
posts exactly the body it did before channels existed; Keyup ignores fields it does not know.

### Station self-description

`[station]` and the per-`[[device]]` description fields ([configuration.md](configuration.md#station-self-description))
travel *up* on this POST and never come back down: the desired-state endpoint cannot set any of
it, because only the person standing next to the antenna knows what is on the mast. A blank
`receiver` is reported as the backend's own name (`HackRF One`, `SoapySDR`, `IQ file`).

---

## 6. Desired state — what Keyup can push down

Every ~10 s the recorder GETs `/api/recorder/desired-state?instance=<id>` and applies what changed.
A field that is **absent is inherited** — a partial desired-state never wipes a TOML value — and a
field this recorder has never heard of is ignored, so the two can be upgraded in either order.
**The TOML is never written back**: desired state is a runtime override over the file.

```json
{"version": 12,
 "recorder": {"version": 3,
              "spool_only": false,
              "event_log_archive": true,
              "link": {"enabled": true, "bind": "0.0.0.0:47123",
                       "peers": ["192.0.2.11:47123"], "key": "a-long-random-shared-secret",
                       "hello_interval_s": 5, "accept_remote_grants": true,
                       "publish_grants": true, "defer_to_claims": false}},
 "tuners": {"voice0": {"version": 4,
                       "config": {"bands": ["856000000-866000000"], "enabled": true}}}}
```

**Fetched once before the tuners are built.** At process start, before the first tuner opens, the
recorder makes one synchronous GET of the same endpoint and seeds its registry with what comes
back, so the very first build already carries Keyup's config and the version it came at is echoed
on the first status POST. Without it, every start would bring the tuners up on the TOML and then
restart any tuner whose stored config differed a few hundred ms later. The wait is capped by
`[uplink] desired_prefetch_timeout_s` (default 3 s; `0` turns the prefetch off), and a server that
is down, slow, or predates the route just leaves the recorder on its TOML with one
`desired-state prefetch failed; starting from config` line. The prefetch is skipped entirely when
`[uplink] enabled = false` or `url` is empty.

### The push channel — `[uplink] stream`

The ~10 s poll is the contract and it never changes. On top of it the recorder holds one WebSocket
open to `/api/recorder/stream` (with `http` → `ws` and `https` → `wss`, the same Bearer token), and
Keyup uses it to say *fetch now*. The recorder's first frame is its hello:

```json
{"type": "hello", "instance": "rec-01", "recorder_version": "0.0.1", "desired_seen": 12}
```

`desired_seen` is the top-level `version` of the newest desired-state document this recorder has
fetched (`null` before the first one). Keyup answers with its own `hello`
(`{"type":"hello","server_time":…,"heartbeat_s":15,"desired_version":13,"recorder_block_version":4}`)
and thereafter sends `{"type":"desired_state","version":…}` whenever something changes, plus a
`heartbeat` on the stated cadence. **The channel carries no configuration.** A version number is
the entire payload, and the recorder responds by making the same authenticated desired-state GET
it would have made anyway, through the same parse, validation and apply path — so a duplicate or
out-of-order frame costs one extra GET and nothing else.

Losing the channel therefore costs latency, not correctness: `stream = false` (or a server without
the route) means a change lands within ten seconds instead of within a tenth of one. The reconnect
backoff doubles from 1 s to `stream_reconnect_max_s` and resets after a connection that lasted a
minute; two missed heartbeats are treated as a dead socket. Close code **4401** (the API key was
refused) backs off for a full minute, and **4408** (the hello did not arrive inside Keyup's
ten-second deadline) is logged; each is warned about once, not once per attempt. The channel is
skipped entirely when `[uplink] enabled = false` or `url` is empty. Its health rides on every
status POST and on `/api/status`, inside `uplink`:

```json
{"uplink": {"spool_only": false, "source": "config", "spooled_calls": 0,
            "spool_bytes": 0, "url": "https://keyup.example.org/api/call-upload",
            "stream": {"enabled": true, "connected": true,
                       "since": "2026-01-15T14:02:11Z", "reconnects": 2,
                       "last_notify_at": "2026-01-15T14:31:40Z", "last_error": null}}}
```

`since` is when the current connection came up (`null` while it is down), `reconnects` counts
connections after the first, `last_notify_at` is the last time Keyup asked for a fetch, and
`last_error` says why the last connection ended. The panel shows the same thing as a
`push: connected` line beside the upload hold.

### Per-tuner fields

**Every field a tuner's `config` may carry**, and what applying it costs:

| field | type | applies | notes |
| --- | --- | --- | --- |
| `role` | `"auto"`/`"control"`/`"voice"`/`"data"` | restart | Unrecognised names are ignored. |
| `site` | string | restart | Advisory site-label focus. |
| `site_id` | string or number | restart | Keyup's numeric id, echoed back distinctly. |
| `center_frequency` | Hz | restart | |
| `bands` | array of labels | restart | Four safety nets; see below. |
| `enabled` | bool | restart | `false` idles the tuner and drops it from routing. |
| `spectrum` | bool | **live** | A DSP-thread atomic; never restarts. |
| `gain` | dB | **live** on `soapy` | Overall/distributed gain. |
| `gains` | `{element: dB}` | **live** on `soapy` | By the driver's own element names. |
| `agc` | bool | **live** on `soapy` | Hardware AGC. |
| `ppm` | ppm | **live** on `soapy` | Static frequency correction. |
| `auto_ppm` | bool | restart | Whether the closed-loop correction runs. |
| `auto_ppm_limit` | ppm | restart | Ceiling on the automatic correction. |
| `bandwidth_hz` | Hz | restart | `soapy` analog filter bandwidth. |
| `rx_port` | string | restart | `soapy` antenna/port name. |
| `settings` | `{key: value}` | restart | `soapy` `writeSetting`; values may be strings, numbers or bools and are all sent as strings. |
| `lna_db` | dB | restart | HackRF LNA. |
| `vga_db` | dB | restart | HackRF VGA. |
| `amp` | bool | restart | HackRF +14 dB front-end amp. |
| `data_follow` | bool | restart | Applies to **every** site this tuner serves. |
| `remote_grants` | bool | restart | Whether this tuner takes grants a **peer** recorder heard over the `[link]`. Absent keeps the role default (`voice`/`data` yes, `auto`/`control` no). |
| `control_frequencies` | array of Hz | restart | Replaces the candidate list of the tuner's **primary** site — the first `[[site]]` in the file that this tuner serves. |
| `sample_rate` | samples/sec | restart | Must satisfy the channelizer rule ([sdr-hardware.md](sdr-hardware.md#the-sample-rate-rule)). |

"Live" means the running stream applies it between reads — no gap in the recording, no restart
budget spent. On `soapy` that is gain, AGC and ppm, because the reader thread owns the device
handle. The `hackrf` backend drives a `hackrf_transfer` child it can only reconfigure by
respawning, so its gains take the ordinary restart path. **Nothing restarts when nothing
changed**: a newer version carrying an identical config only advances the applied version.

**Rejections.** A pushed field that cannot work is dropped from the push — the rest of the entry
still applies — and comes back on the next status POST beside `applied_version`:

```json
"applied_version": 6,
"rejected": {"ppm": "must be within +/-200 ppm, got 9999"}
```

The version is **still acknowledged**: a recorder that refused a field but did not move the
version would leave Keyup resending it forever. Omitted entirely when nothing was rejected.

| field | rule |
| --- | --- |
| `ppm` | within ±200 ppm. |
| `gain` | within the device's gain range — the sum of the element ranges the Soapy open reported. Unchecked when the elements are unknown; the driver then refuses it at open. |
| `gains` | every key must name a gain element this device actually has, and every value must be inside that element's range. |
| `lna_db` | 0–40 in 8 dB steps (`hackrf`). |
| `vga_db` | 0–62 in 2 dB steps (`hackrf`). |
| `auto_ppm_limit` | > 0 and ≤ 50 ppm. |
| `bandwidth_hz` | > 0. |
| `control_frequencies` | non-empty, each a multiple of 12 500 Hz and inside the tuner's usable span. |
| `sample_rate` | `sample_rate / 12500` must be an even whole number ≥ 4. |

A malformed *shape* — `gains` that is not an object, a `control_frequencies` entry that is not a
number — drops the whole tuner entry with one warning rather than being applied in part.

**`enabled` and `bands`.** `enabled = false` builds that tuner in an idle state *and* takes it out
of the cross-tuner routing table, so its grants fall to whatever sibling tuner can serve them;
re-enabling puts it back. `bands` follows the TOML field exactly (same labels, same Hz ranges) and
overrides it for the life of the process. An **empty array is an instruction** — "no restriction"
— and is how a TOML band list is cleared remotely; omitting the key is what means "leave it
alone". The cross-tuner routing table is republished from the pushed list on the same rebuild.

**A bad bands push cannot mute a recorder.** Four nets, in order:

1. **Parse** — at most 16 entries, each at most 64 characters, and every entry must resolve.
   Anything else drops that tuner's whole desired entry with one warning; no band list is ever
   *partly* applied.
2. **Apply** — a merged config whose bands do not overlap the tuner's usable capture at all
   (`["vhf"]` on an 800 MHz capture, or a `center_frequency` push that moves the capture off the
   bands it inherits) is refused with a warning: nothing is staged, nothing restarts, and the
   applied version does not move, so Keyup keeps showing the change as *pending*.
3. **Two-phase apply** — anything that gets past those still only takes effect through a rebuild,
   and a unit whose stream never opens never confirms, so the applied version never advances.
4. **Control is not band-gated** — whatever the bands say, the tuner still acquires control,
   still reports status, and still polls this endpoint, so a narrowing push is always reversible.

A legal but over-narrow partition (nobody covering the frequencies the site actually grants) is
not refused; it shows up as `coordinator.unrouted_grants.<band>` climbing while calls do not
arrive.

**Which values are Keyup's.** Because every field is reported *effective*, a tuner the file
leaves on `role = "auto"` that Keyup has pushed to `control` reads exactly like one the file names
`control`. The `overrides` list on each tuner entry ([§5](#5-tuner-status)) says which values came
from Keyup. The local panel marks each affected row with a dim `keyup` tag (hover it for the
file's value). Clearing a field on Keyup's recorder page drops the override, and the tuner is back
on the file's value from that push on.

### The `recorder` block — settings for the whole box

The optional top-level **`recorder`** block carries the recorder-wide settings, versioned on its
own counter so Keyup can change one without touching any tuner's version. It is applied when
`version` is newer than the last recorder-level version this instance applied and the block
manages at least one field. **Every field is a tri-state**: absent or `null` means "Keyup is not
managing this" and leaves whatever the config or the panel last set alone.

| field | type | applies | notes |
| --- | --- | --- | --- |
| `spool_only` | bool \| null | **live** | The upload hold; the same flag `[uplink] spool_only` seeds and the panel toggles. |
| `event_log_archive` | bool \| null | **live** | Whether finished event-log days are shipped ([§4](#4-shipping-event-log-days)). Turning it on ships the backlog already on disk, oldest day first. |
| `link` | object \| null | **link rebuild** | The whole `[link]` section; see [recorder-link.md](recorder-link.md#managing-it-from-keyup). Rebuilds the link's socket and threads in place. **No tuner is restarted and no call is interrupted.** |
| `skip_talkgroups` | object \| null | **live** | Talkgroups to skip entirely; see below. `null` here is a *value* — "drop the list you are holding" — so only an absent key means "not managed". Applied on its own `revision`. |
| `broadcastify` | object \| null | **live** | Direct-to-Broadcastify delivery; see below. `null` here is a *value* — "the feature is off, drop whatever you are holding". Applied on its own `revision`. |

**Rejections.** A recorder-level field that cannot work is dropped — the rest of the block still
applies, and the version is still acknowledged — and comes back in a recorder-level `rejected` map
keyed by field (`link.*` for the ones inside the link object).

**What was applied** comes back on the next status POST, omitted entirely until Keyup has pushed
one:

```json
"recorder_applied": {
  "version": 3,
  "spool_only": false,
  "event_log_archive": true,
  "link": {"enabled": true, "bind": "0.0.0.0:47123", "peers": ["192.0.2.11:47123"],
           "hello_interval_s": 5, "accept_remote_grants": true, "publish_grants": true,
           "defer_to_claims": false,
           "key_set": true, "key_fingerprint": "9f2a1c04"},
  "rejected": {"link.key": "must be at least 16 characters"}
}
```

`rejected` is omitted when nothing was refused. A panel toggle of the hold made after an apply
does **not** move this echo: it reports what Keyup asked for and got, which is exactly the
divergence Keyup needs to be able to see.

What the box-wide settings cost is reported the way the per-tuner ones are:

```json
"capabilities": {"recorder": {"restart_on": []}}
```

The list is empty and is meant to stay that way: nothing Keyup manages at the recorder level
restarts a tuner or the process.

#### `recorder.skip_talkgroups` — talkgroups not to record at all

```json
{"recorder": {"version": 3,
              "skip_talkgroups": {"tgids": [1501, 1502], "revision": "a1b2c3d4e5f6"}}}
```

A talkgroup whose Record, Transcribe and every export switch the operator has turned off is
traffic nobody wants, and the cheapest place to honour that is before a tuner is spent on it. A
grant whose talkgroup is on this list is **refused at the grant**: no channelizer slot is
subscribed, no call is opened, no traffic-channel pool slot is held, and nothing is published for
a sibling tuner or a link peer to record — every peer holds the same list from Keyup.

| field | type | notes |
| --- | --- | --- |
| `tgids` | array of 1..=65535 | The talkgroups. At most 4096. One bad entry refuses the **whole** list (`rejected["skip_talkgroups"]`) rather than applying half of it. |
| `revision` | string | Keyup's opaque revision. A different one is applied at once, with no restart; the same one is a redelivery and changes nothing. |

`null` drops Keyup's list; `[recorder] skip_talkgroups` in the TOML survives it, because the
file's half is the operator's own and is unioned with Keyup's rather than replaced by it. What the
recorder does **not** stop doing: the grant is decoded, logged to the event log, and counted, and
the radio events around it still go up. A call already on the air when its talkgroup joins the
list runs to its own end, and a call on a **patch** supergroup is skipped only when every live
member talkgroup is skipped too. Refusals are counted as `calls.skipped_by_policy`, once per call.
The list in force is on `/api/status` under `recorder.skip_talkgroups` (`count`, `revision`,
`local`) and on the panel's recorder card.

#### `recorder.broadcastify` — uploading each call to Broadcastify Calls

Off unless Keyup sends this block. You supply your own Broadcastify Calls credentials in Keyup;
trunkd never ships with any. When the block is present and `enabled`, the uplink worker offers
every eligible finished call to Broadcastify Calls **before** it pushes the same call to Keyup, and
reports what happened on that push. It runs on the uplink thread, never on a DSP thread, and is
bounded by its timeouts (~12 s worst case, ~1 s typical). `[uplink] direct_broadcastify = false`
refuses the whole block: Keyup's setting is the primary switch, the recorder's is a veto.

| field | type | notes |
| --- | --- | --- |
| `enabled` | bool | Whether to upload at all. |
| `endpoint_url` | string | The Broadcastify Calls endpoint. Must be `http(s)`; the block is refused otherwise. |
| `api_key` | string | Required. **Never logged and never echoed** — the panel and the logs carry only `api_key_set` and the first six characters, and every stored response string has the key redacted out of it. |
| `system_id` | string | Sent verbatim as `systemId`. |
| `encoding` | string | Sent as `enc`; `mp3` (the only format this recorder produces for it). |
| `talkgroups` | array \| null | `null` = every group call on the system; a list is an allow-list. |
| `skew_limit_s` | float | A call whose end is more than `skew_limit_s − 1` seconds ago is `late` and nothing goes on the wire. |
| `min_speech_s` | float, optional | The least `speechS` a call must carry to be uploaded. **Absent (or negative) means 0.25**; `0` disables the gate. A call below it is `skipped` with `no speech (0.04 s)` — decided locally, *before* the claim. |
| `claim` / `claim_url` | bool / string | Ask Keyup which instance owns the call first (see below). A path is resolved against `[uplink] url`. |
| `revision` | string | Keyup's opaque revision. A different one is applied at once, with no restart. |

**Claim.** With `claim: true` the recorder POSTs `{"instance_id", "tgid", "ts", "duration_s"}` to
`claim_url` with the same Bearer upload token (timeout 1.5 s). `granted: false` ends it — outcome
`claim_denied`, the holder recorded, no Broadcastify traffic. A Keyup that cannot be reached is
treated as a grant (Broadcastify deduplicates anyway) and the fact is noted in the outcome's
response text.

**Register and PUT.** A multipart POST to `endpoint_url` (`apiKey`, `systemId`, `callDuration`,
`ts`, `tg`, `src`, `freq` in MHz, `enc`; timeout 4 s) answers with one line, `<code> <message>`:
`0 <url>` means PUT the MP3 there as `audio/mpeg` (timeout 6 s) → `uploaded`, with the remote id
taken from the last path segment; `1 SKIPPED---ALREADY-RECEIVED-THIS-CALL` → `duplicate`; anything
else → `failed` with the line kept verbatim (first line, 500 characters, key redacted).

**Outcome.** The verdict is written onto the call, so it goes out on the Keyup push as
`broadcastifyOutcome` (`uploaded` | `duplicate` | `failed` | `late` | `claim_denied` | `skipped`)
plus `broadcastifyCode`, `broadcastifyResponse`, `broadcastifyRemoteId` (uploaded only) and
`broadcastifyHolder` (claim_denied only) — all of them absent when the feature decided nothing. It
is also written into the spool sidecar, so a call that waits out a Keyup outage is **never
registered with Broadcastify twice**. Encrypted calls (no audio), unit-to-unit calls (no
talkgroup), talkgroups outside the list, calls with less than `min_speech_s` of speech, and a held
uplink are `skipped` with a short reason. The speech gate's own refusals are also counted as
`skipped_no_speech` in the panel's `broadcastify` block.

Broadcastify's terms of service apply to whatever you upload; read them before enabling this.

---

## See also

* [configuration.md](configuration.md) — the `[uplink]` and `[recorder]` keys named here.
* [recorder-link.md](recorder-link.md) — two recorders on one site, and the `link` block.
* [operations.md](operations.md) — the spool, the upload hold, the event log, reading the logs.
