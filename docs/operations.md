# Operating trunkd

Day-to-day running of a recorder: bringing one up safely, reading its log, what it keeps on disk,
how it recovers from faults, and why its clock matters. Installing it as a service is in
[install-linux.md](install-linux.md) and [windows-service.md](windows-service.md); the panel in
[panel.md](panel.md).

## Start and stop

```sh
trunkd --config /etc/trunkd/trunkd.toml check   # validate + enumerate the SDRs, then exit
trunkd --config /etc/trunkd/trunkd.toml run     # record (`run` is also the default)
```

`check` exits after printing the parsed config, the link and the boards it can see; it opens no
hardware.

**Ctrl+C (or SIGTERM, or a service stop) is the stop.** The first interrupt tears down in order —
the devices stop and finish their open calls, the call queue drains, the uplink drains what it was
handed, and (only if `restore_portapack_on_exit`) a PortaPack goes back to its UI. A second Ctrl+C
exits immediately with code 130; the process is always safe to kill, because the only state that
matters is the spool and that is crash-safe (write temp + rename).

The process exits 0 when an `iqfile` source runs out. A radio that dies is a *fault*: the
supervised device unit restarts it with exponential backoff (1 s → 30 s). Once six failures land
inside a minute it is **not** given up on: it moves to a slower backoff (30 s → 10 min) and is
retried for as long as it takes, shown as `retrying` in the panel's `supervisor` block (with
`next_attempt_ms` and `last_error`) and as the tuner's `fault` in the status Keyup gets. The
panel's restart button, or any newer Keyup desired-state version for that tuner, retries it at once
and starts its budget over. A tuner Keyup has disabled idles and is never retried.

## Bring-up: record without uploading

Set `[uplink] spool_only = true` and no *recording* leaves the box — calls are encoded and written
to the spool, radio events go to the event spool, and the site-observations POST is skipped. This
is how to prove reception before pointing at a live server:

```sh
TRUNKD_UPLINK__SPOOL_ONLY=true trunkd --config /etc/trunkd/trunkd.toml run
```

(or set `spool_only = true` in the file). Listen to a few of the MP3s under `spool/` before
releasing the hold.

**The control plane stays up.** `spool_only` holds *uploads*; it is not an offline switch. A held
recorder still registers with Keyup, still POSTs tuner status every ~15 s and still polls desired
state every ~10 s — which is what lets the hold be released from Keyup or from the local panel
rather than from this file. Only `[uplink] enabled = false` (or an empty `uplink.url`, since every
control route is derived from it) keeps a recorder wholly offline. `trunkd check` says which you
have:

```text
  uplink spool-only (uploads held; control plane connected) → https://keyup.example.org/api/call-upload
```

Releasing the hold resumes uploads and drains the spool oldest-first through the ordinary
reconnect path. Three writers can move it and **the most recent one wins**:

| source | how | reported as |
| --- | --- | --- |
| config | `[uplink] spool_only` at boot | `"source": "config"` |
| Keyup | the `recorder` block of desired state ([keyup.md](keyup.md#the-recorder-block--settings-for-the-whole-box)) | `"source": "keyup"` |
| panel | `POST /api/uplink` ([panel.md](panel.md#the-upload-hold)) | `"source": "panel"` |

Every status POST carries the current state, so a recorder that is up, healthy and delivering
nothing explains itself:

```json
{"uplink": {"spool_only": true, "source": "panel", "spooled_calls": 214,
            "spool_bytes": 18244096, "url": "https://keyup.example.org/api/call-upload",
            "stream": {"enabled": true, "connected": true, "since": "2026-01-15T14:02:11Z",
                       "reconnects": 0, "last_notify_at": null, "last_error": null}}}
```

## What the log says

Startup, in order — if any of these is missing, that is where to look:

```text
INFO trunkd starting version="0.0.1"
WARN [uplink] spool_only = true: uploads are held and calls are spooled; the keyup control plane stays connected and can release the hold url=https://keyup.example.org/api/call-upload
INFO device streaming device=sdr0 backend=Soapy sample_rate=10000000 center_hz=856000000 sites=1 ring_samples=16777216
INFO hunting for the control channel site=Example County/Site 1 candidates=3
INFO control channel acquired site=Example County/Site 1 frequency_hz=851200000 nac=0x123
```

Then a status line every 10 s, which is the one thing to watch:

```text
INFO status site=Example County/Site 1 control="851.2000 MHz" nac=0x123 tsbk_per_s=26.8
     active_calls=0 pool=0/20 started=12 ended=12 uploaded=12 spooled=0 failed=0
     spool_mib=0.0 disk_free_mib=214093 dropped=0 voice_frames=3480 overflow=0 throughput=10.00 Msps
```

* `control="hunting"` — no candidate has produced five valid TSBKs yet. Acquisition normally takes
  well under a second; minutes of hunting means the antenna, the gains, `center_frequency`, or a
  frequency error ([sdr-hardware.md](sdr-hardware.md#frequency-correction-ppm)).
* `tsbk_per_s` — a healthy P25 Phase 1 control channel carries roughly 25–30. A collapse toward
  zero precedes `control channel lost`.
* `overflow` climbing, with `SDR ring overflow: the DSP thread is not keeping up` — the box cannot
  sustain the sample rate. It should stay at 0. Raise `ring_seconds`, grant real-time priority, or
  move other work off the box.
* `throughput` well under the configured rate — USB or driver trouble, not DSP.
* `pool=20/20` with `traffic pool exhausted` — raise `traffic_pool_size`.
* `event_log_dropped` climbing — raise `[recorder] event_queue_depth`.
* `spool_mib` / `disk_free_mib` — what trunkd is holding, and what the volume has left (`0` means
  free space could not be measured).

Per-call lines land at info (`call ended … talkgroup=… duration_s=… encrypted=…`); fragments
dropped as noise and the encrypted/clear decision are at debug (`log_level = "debug"`).

On Linux, the DSP thread also logs whether it got real-time priority: `DSP thread running at
real-time priority` versus `no permission for real-time scheduling; running at normal priority`.
The second line never appears on Windows, which has no equivalent.

## The spool

```text
spool/YYYY/MM/DD/<system>_<talkgroup>_<start-unix-ms>.mp3
spool/YYYY/MM/DD/<system>_<talkgroup>_<start-unix-ms>.json
```

Audio plus a trunk-recorder-style JSON sidecar — the format Keyup's *file* ingest already reads, so
a spool directory is a valid archive on its own. In normal operation the spool is the fallback for
an unreachable server and is drained oldest-first every 30 s; in `spool_only` mode it is the
destination. Either way it is capped at `spool_cap_bytes` with oldest-first eviction. An encrypted
call spools as a sidecar with no audio file, exactly as it would upload. Keep the spool on one
volume: it relies on same-volume renames being atomic.

**Cap and floor are two different limits.** `spool_cap_bytes` (default 1 GiB) bounds what *trunkd*
keeps: a promise that this recorder will never occupy more than that, no matter how long the server
is unreachable. `spool_min_free_bytes` (default 2 GiB) bounds what the *volume* must keep free for
everyone: system logs, a second recorder, an operator's copy. A disk filled from outside trunkd
would otherwise hit `ENOSPC` with the spool nowhere near its cap, and a recorder that cannot write
loses calls it has already decoded. So whenever the filesystem holding `spool_path` reports less
than the floor available (`statvfs` on Linux, `GetDiskFreeSpaceExW` on Windows), the oldest spooled
calls are evicted until it is back above it. Cap-driven and floor-driven evictions both count in
`uplink.spool_evicted`. Both limits are re-checked on the 30 s drain timer as well as after each
spooled call. Free space is read as what an unprivileged process can actually use; if the reading
fails it is logged once and the floor is skipped for that pass — a bad reading never deletes a call.
`spool_min_free_bytes = 0` disables the floor. The same floor applies to `spool/events/`.

The status line reports both sides of this as `spool_mib=` and `disk_free_mib=`, and the
`uplink.spool_bytes` / `uplink.disk_free_bytes` gauges carry the same numbers.

## Timestamps and the host clock

Call times, event-log `at` stamps and radio-event times all come from the *stream* clock: a
wall-clock instant anchored when a tuner's first block of samples arrives, advanced from there by
sample count ([ARCHITECTURE.md](ARCHITECTURE.md), "Time: the stream clock"). Two corrections keep
it honest against the host's system clock:

* **The drift check compares against the system clock once a second.** A gap over 1.5 s is stepped
  out at once (a *resync*); a smaller steady gap is trimmed out 1 ms a second once its average
  passes 20 ms. Each tuner's stamps end up within about 20 ms of the system clock.
* So **a host clock that is off by 200 ms puts every call and event 200 ms off as well**, and a
  host clock that wanders between syncs makes the trim chase the wandering.

What depends on that accuracy:

| What | Needs | Why |
| --- | --- | --- |
| Call `startMs` / `endMs` | tens of ms | Keyup merges two copies of one call (two tuners, two recorders) by time |
| Radio events and observations | under a second | a radio event's identity includes its time, so two recorders' copies only merge if their clocks agree |
| Recorder link frames | within 30 s between recorders | a frame more than 30 s from the receiver's clock is refused ([recorder-link.md](recorder-link.md)) |

**Keep every recorder on NTP, against the same server, with short polls.** On Linux, chrony or
systemd-timesyncd is enough; on Windows the default Windows Time settings poll far too rarely, see
[windows-service.md](windows-service.md#host-clock-and-ntp). trunkd has no NTP client of its own and
should not have one: its stamps would then be on a different clock from its own log file and
everything else on the box. A slewed NTP correction reaches trunkd gradually through the trim, with
no stamp moving backwards; a stepped one over 1.5 s is a resync.

What remains after the host clock is fixed is the sample clock's own rate error: a tuner's
`clock_trim_ms` keeps growing at that rate (86 ms a day is 1 ppm), and `clock_error_ms` should stay
within ±20 ms.

### Calls stamped N seconds early after a host stall → `clock_resyncs`

A sample counter cannot see samples that never arrived, so anything that costs the host a chunk of
the stream — an out-of-memory stall, a suspended VM, a USB hiccup upstream of the ring — would leave
the clock permanently behind and every event and call stamped that much **early**. The ring's
`overflow` counter stays at 0 when that happens: the samples were lost *before* the ring, so
overflow is not the signal to look at.

The recorder corrects itself. What to check, in order:

```bash
# per tuner, from the panel (or the same fields in Keyup's status POST)
curl -s localhost:8080/api/status | jq '.tuners[] | {tuner_key, clock_resyncs, last_clock_resync_ms, stream_gaps}'
```

* `clock_resyncs` > 0 — the recorder detected the drift and re-anchored. `last_clock_resync_ms` is
  the size of the most recent correction in signed milliseconds (**not** a timestamp): positive
  means the clock had fallen behind and calls before that point were stamped that much early.
* `stream_gaps` > 0 — blocks arrived later than the producer should ever deliver them. An
  independent count of the same event: gaps climbing with no resyncs means samples are being lost
  in doses too small to cross the 1.5 s threshold.
* The log line at the moment of the correction:

  ```text
  WARN stream clock re-anchored: the sample count had fallen out of step with wall time
       (samples lost before the ring), so timestamps up to now were off by this much
       device=sdr0 correction_ms=11600
  ```

* The raw event log carries the seam:

  ```bash
  jq -c 'select(.name=="CLOCK_RESYNC")' events/$(date -u +%F).jsonl
  # {"at":"2026-01-15T20:00:59Z","ch":"control","type":"clock_resync","name":"CLOCK_RESYNC","fields":{"correction_ms":11600}}
  ```

Calls that were already on the air when the correction landed keep the start they were given, so
nothing goes backwards inside a call. Calls *before* the resync stay wrong — the correction is
forward-looking. Drifts under 1.5 s are deliberately left to the trim: inside a second the sample
clock is the more precise of the two.

If `clock_resyncs` keeps climbing, the box is losing samples repeatedly and the clock is a symptom:
look at memory pressure (`journalctl -k | grep -i oom`), at `overflow`, and at `throughput`.

## The raw event log

Everything non-voice the decoder saw, one JSON object per line, for analysis over days rather than
minutes ([NON-VOICE-DATA.md](NON-VOICE-DATA.md) explains why some messages only show up in a
day-long log). On by default (`[recorder] event_log`); stays on the box unless you ship it
([keyup.md](keyup.md#4-shipping-event-log-days)).

```text
events/2026-01-15.jsonl      today, appended to
events/2026-01-14.jsonl.gz   yesterday, gzipped when the day rolled over
```

One file per **UTC** day. The log's day only moves forwards: a line stamped for a day already
closed (tuners on separate stream clocks interleave around midnight) goes into the current day's
file, its own `at` intact, so each day is closed and compressed exactly once. Compression streams
(flat memory, however big the day), and an existing `.gz` is never replaced — a second file for a
day is `YYYY-MM-DD.N.jsonl.gz`. Plain days a previous run left behind are compressed at start-up
and within a minute of nothing having them open. Anything older than `event_log_keep_days` is
deleted, by the date in the file name. On a busy site expect a few million lines a day, a couple of
hundred MB uncompressed and ~20 MB gzipped once repeats are coalesced (below).

```jsonc
{"at":"2026-01-15T09:12:03.481Z","ch":"control","type":"tsbk","opcode":44,"mfid":0,
 "name":"UNIT_REG_RESPONS","hex":"ac00…","fields":{"response":"accepted","source_id":1000001}}
{"at":"…","ch":"851200000","type":"lc","opcode":9,"vendor":0,"name":"SOURCE ID EXTENSION",
 "hex":"09…","fields":{"wacn":781824,"system":2,"source":2000025}}
```

A unit-to-unit call adds two more records — one when the engine opens it, one when it closes — and
the AMBT grant that caused it carries its own decode alongside the payload hex:

```jsonc
{"at":"…","ch":"control","type":"pdu","name":"PDU","fields":{
  "ambt_opcode":4,"ambt_name":"UU_VCH_GRANT","payload":"…",
  "uu_source":1000001,"uu_target":1000097,"uu_band":1,"uu_channel":274,
  "uu_source_wacn":781824,"uu_source_system":1}}
{"at":"…","ch":"control","type":"private_call","name":"PRIVATE CALL START","fields":{
  "start":true,"frequency_hz":851425000,"source":1000001,"target":1000108,"duration_ms":0}}
{"at":"…","ch":"control","type":"private_call","name":"PRIVATE CALL END","fields":{
  "start":false,"frequency_hz":851425000,"source":1000001,"target":1000108,"duration_ms":4200}}
```

The call started at the end record's `at` less its `duration_ms`.

`ch` is `"control"` or the traffic channel's downlink frequency in Hz. `type` is one of `tsbk`,
`lc`, `tdulc`, `hdu`, `pdu_header`, `pdu`, `lsd`, `site_broadcast`, `sndcp_context`,
`private_call`, `clock_resync`. `name` is sdrtrunk's own short label, so a histogram built from
this log and one built from an sdrtrunk decode name the same message the same way. A multi-site
recorder adds a `site` key.

**Repeats.** Most of a day is the site describing itself again and again, byte for byte. A
*constant broadcast* — a TSBK that names no subscriber (`SYS_SVC_BCAST`, `RFSS_STATUS_BCST`,
`NET_STATUS_BCAST`, `ADJ SITE STATUS`, `SEC_CCH_BROADCST`, the `IDEN_UPDATE`s, `SNDCP_DCH_ANN_EX`,
time/date, SCCB, TDMA sync, and the L3Harris `OSP 1`/`OSP 15` beacons and `REGROUP` broadcast) or
a `site_broadcast` record — whose content (everything but `at`) matches one written in full less
than 60 s earlier is written as a back-reference instead:

```jsonc
{"at":"2026-01-15T00:00:00.504Z","id":3,"ch":"control","fields":{},"hex":"bb00…",
 "mfid":0,"name":"NET_STATUS_BCAST","opcode":59,"site":"Site 1","type":"tsbk"}
{"at":"2026-01-15T00:00:01.991Z","rep":3}
```

The full copy carries an `id` straight after `at`; a repeat is exactly `{"at","rep"}` and means the
nearest **preceding** line with that id. Every message still has its own line, its own `at` and its
place in the file, so nothing is lost: drop the `id` and give each repeat the body of its full copy,
and the file is byte for byte what the recorder would have written without coalescing (typically
three quarters of a day's lines become repeats, and the file shrinks by more than half). A full copy
is written again at least once a minute, the table resets with each file (and each run appended to
one), and grants, grant updates, affiliations, registrations, data and link control are never
coalesced. [`harness/eventlog_repeats.py`](../harness/eventlog_repeats.py) expands a log
(`--expand IN OUT`); a `jq` one-liner that needs whole records should run on an expanded copy.

**IMBE voice frames are never written.** That is enforced at the engine — the traffic pass-through
drops them at the source — not by a filter downstream that could later be removed. All-zero
low-speed data is also skipped.

Writes happen on the observations thread, never the DSP thread, over a bounded queue: under
pressure the queue drops oldest and counts it, which the status line shows as `event_log_dropped`.
A slow disk costs events, never a stalled decoder. That queue is `[recorder] event_queue_depth`
(default 16 384 events, ~112 bytes each). The bursts that fill it come from data sessions — PDU
messages at a few hundred a minute — while the observations thread is busy with a log rotation, a
gzip, or one uplink POST. Doubling it costs under 2 MiB a step.

To analyze a day:

```sh
# what opcodes appeared, and how often
zcat events/2026-01-14.jsonl.gz | jq -r 'select(.type=="tsbk") | "\(.mfid):\(.opcode) \(.name)"' \
  | sort | uniq -c | sort -rn

# every registration and affiliation, with the radio
jq -r 'select(.name | test("REG|AFFIL")) | [.at, .name, (.fields|tostring)] | @tsv' \
  events/2026-01-15.jsonl

# did any of the rare commands ever appear?
jq -r 'select(.name | test("STATUS_UPDATE|CALL_ALERT|EMRG|RADIO_MONITR|EXTNDED")) | .at + " " + .name' \
  events/*.jsonl
```

> **Privacy.** The event log records which radios registered, affiliated and talked, and when. It
> is presence data about identifiable people. Keep `event_log_keep_days` as short as your analysis
> needs, or set `event_log = false`.

## Offline replay

The same binary decodes a recorded capture, which is how to test a change without the radio. Point
a `[[device]]` at an IQ file with `backend = "iqfile"` and `pacing = "unthrottled"` — file playback
waits for the DSP thread rather than dropping samples, so an offline run decodes every sample and
finishes as fast as the CPU allows (typically several times real time). How to make a capture, and
the end-to-end test that replays one against a mock server, are in
[capturing-test-iq.md](capturing-test-iq.md).

## Checking the server side

With Keyup, its own pages show recorders, tuners and calls. From a shell (`$TOKEN` is a Keyup token
allowed to read):

```sh
curl -s https://keyup.example.org/api/status | jq .counts
curl -s 'https://keyup.example.org/api/calls?page_size=5' -H "Authorization: Bearer $TOKEN" \
  | jq '.items[] | {started_at, tgid, encrypted, transcript_status}'
```

See Keyup's own documentation for its admin and recorder pages.

## Health monitor

`trunkd monitor` is a separate, tiny process of the same executable: no SDR, no DSP, just HTTP to
the local panel. It restarts the recorder when it stops recording even though the process is still
alive — panel unreachable, no control lock for ten minutes, or a supervised unit that gave up. On
Windows it is installed as a service ([windows-service.md](windows-service.md#keeping-it-alive));
on Linux run it beside the recorder with a restart command:

```sh
trunkd monitor --url http://127.0.0.1:8080 --restart-cmd "systemctl restart trunkd" --log-dir /var/log/trunkd
```

`--log-dir` must be writable by the monitor's user, and that user must be allowed to run the restart
command (root, or a sudo/polkit rule for exactly that command). It needs the panel enabled
(`[server] enabled = true`). With panel tokens set, pass `--token`, set
`TRUNKD_MONITOR_TOKEN`, or give it `--config` so it reads the first `[server] tokens` entry. The
rules and windows are the same on both platforms; see the table in
[windows-service.md](windows-service.md#keeping-it-alive). For most Linux installs systemd's
`Restart=on-failure` plus this monitor is the whole supervision story.

## Auditing dependencies

```sh
cargo install --locked cargo-audit
cargo audit                          # Cargo.lock against the RustSec advisory database
cargo update --dry-run --verbose     # semver-compatible bumps waiting, and newer majors
cargo tree -d --target all           # duplicate crate versions (all targets, Windows included)
```

`cargo audit` reads only `Cargo.lock`, so one run covers the Linux and Windows builds alike. CI
also runs `cargo deny` with the repository's `deny.toml` (licenses, advisories, bans). Known and
accepted: `atomic-polyfill` (RUSTSEC-2023-0089, unmaintained, informational) is in the lockfile
through `postcard`'s default `heapless` feature but compiles only for targets without native
atomics, not for x86_64 or aarch64.
