# trunkd — Architecture

trunkd is a P25 trunked-radio recorder engine written in Rust. It owns everything between the SDR hardware and a
finished call: tuner management, channelization, P25 Phase 1 and analog NBFM decoding, voice synthesis, call
assembly, and delivery to [Keyup](https://github.com/pwsh/keyup) over the Rdio-Scanner upload contract that
Keyup already implements (`POST /api/call-upload`, its ARCHITECTURE.md §1.6/§1.7). It replaces
[sdrtrunk](https://github.com/DSheirer/sdrtrunk) for the recording role; sdrtrunk (and
[JMBE](https://github.com/DSheirer/jmbe) for the vocoder) is the *reference
implementation* — every demodulator, EDAC decoder, and trunking behavior here is ported from or validated against it.

Primary goals, in order: **stability** (run for months unattended), **minimal resource usage** (a Raspberry Pi 5 must
comfortably record one P25 system; no GC, no unbounded queues), **responsiveness** (calls uploaded within ~1 s of call
end), and **operability** (a control/health API, self-healing supervision, one small static binary per OS).

Targets: Linux x86_64 (systemd service), Windows 10/11 x64 (Windows service), Linux aarch64 / Raspberry Pi.

Everything downstream of "a call exists" — archive layout, search, transcription, playback, users/roles, alerts,
retention, statistics — is Keyup's job and is deliberately **not** implemented here.

> **Status (checked against the code 2026-11-11).** This document began as the design. Built today: P25 Phase 1
> (C4FM and LSM) control and traffic decode, IMBE voice, analog NBFM conventional channels (`[[channel]]`, with
> CTCSS/DCS tone squelch or carrier squelch), call assembly, the Keyup and Broadcastify uplinks, the recorder link,
> the local panel. **Not built:** P25 Phase 2 / TDMA voice (`[[site]] phase2` is a stub), AMBE, AM channels
> (`mode = "am"` is parsed and skipped with a warning), DMR and NXDN, the `/api/v1` control API of §5.
> Sections below carry a *Status* note where the code differs from the design.

---

## 1. Functional regrouping (sdrtrunk capability → where it lives now)

sdrtrunk mixes radio engine, archive, and desktop UI in one JVM. trunkd regroups that surface into three planes:

| sdrtrunk functionality | Where it goes | Notes |
|---|---|---|
| Tuner discovery/config, USB drivers, PPM, channelizer | **trunkd: radio platform** | SoapySDR HAL instead of in-app register drivers |
| Channel DSP, P25 P1/P2 demod/decode, trunk tracking | **trunkd: DSP + protocol** | ported from reference impl |
| IMBE/AMBE voice decode (JMBE) | **trunkd: `trunkd-mbe` crate** | IMBE patents expired; GPL-3.0 like JMBE (see `NOTICE.md`) |
| Audio segments, call assembly, dedup-relevant metadata | **trunkd: call production** | richer than sdrtrunk: per-transmission sources, emergency/encrypted flags |
| Streaming to Broadcastify/Icecast/OpenMHz/RdioScanner | **trunkd: uplink** (Keyup, plus direct Broadcastify Calls upload when Keyup pushes credentials) | other services can be re-added as uplink sinks later if ever wanted |
| Audio recording files, event-log CSVs | **Keyup** (trunkd keeps only an offline spool) | YYYY/MM/DD/channel layout, conversations, formats are Keyup features |
| Playlist (channels/site config) | **trunkd config** (TOML + API) | trunkd imports sdrtrunk `default.xml` channels once |
| Aliases, talkgroup directory, RadioReference import | **Keyup directory** | trunkd carries no alias database; it reports raw identifiers |
| Now Playing, spectrum/waterfall, map, icons, Swing/JavaFX UI | **Keyup Admin “Recorder” page** via trunkd control API | spectrum snapshots served by trunkd as data, rendered by the web UI |
| Local audio playback through sound card | dropped; the panel serves an **opt-in monitor stream** instead | listening to *calls* stays in Keyup's web player — `[server] live_audio` exists to answer "is what this box decodes intelligible?" |
| JVM heap tuning, diagnostic reports | **trunkd supervision** | health model designed-in, not bolted on |

Non-goals: DMR/NXDN/LTR/MPT1327/Passport (P25 P1+P2 and analog NBFM/AM only; of these, P25 Phase 1 and NBFM are
built), transmitting, alias actions
(beep/clip/script), desktop GUI.

## 2. Process model and dataflow

One OS process, three thread domains:

```
                    ┌─────────────────────────── control plane (plain threads) ──────────────────────┐
                    │ panel HTTP · call thread · uploader+spool · control loop · observations · link │
                    └────────────▲────────────────────────────────────────────────▲──────────────────┘
                                 │ status events / commands (bounded channels)     │ finished calls
   SDR thread (per device)       │                DSP thread (per device)          │
 ┌───────────────────────┐  IQ ring buffer   ┌──────────────────────────────────┐  │
 │ SoapySDR read_stream  ├──────(rtrb)──────▶│ channelizer → per-channel DDC    │  │
 │ overflow counting     │                   │ → demod → framer → EDAC → msgs   ├──┘
 └───────────────────────┘                   │ → trunking mgr → voice → calls   │
                                             └──────────────────────────────────┘
```

- **SDR threads**: one dedicated OS thread per device does nothing but `read_stream` into a lock-free SPSC ring
  buffer (`rtrb`). If the ring is full, samples are **dropped and counted** — never buffered without bound. The
  overflow counter is a health signal.
- **DSP threads**: one dedicated OS thread per device drains the ring, runs the channelizer and every per-channel
  pipeline synchronously (cache-friendly, no per-buffer allocation: all scratch buffers preallocated and reused).
  Emits protocol messages, decode events, and finished calls into bounded channels toward the control plane.
- **Control plane**: everything non-realtime, on ordinary OS threads — there is no async runtime. The panel is a
  small `tiny_http` server on its own bounded transport, Keyup HTTP goes through one `ureq` agent per loop, and the
  call thread (vocoding, call assembly), uplink worker, observations/event-log thread, cross-tuner coordinator and
  recorder link each own a thread. Slow consumers can never stall DSP — every channel crossing a domain boundary is
  bounded with an explicit drop-oldest or drop-and-count policy.
- **Sites share a device's channelizer**: one polyphase channelizer per device, fed once per block, with every
  site engine on that device taking its channels from it (fb5ab7d; four sites on one HackRF went from 1.05x to
  0.28x real time of DSP work).
- **Conventional channels ride the same bank**: each enabled `[[channel]]` is one more subscription on its
  device's channelizer, drained by an NBFM engine after the site engines on the same DSP thread (no new threads). A
  device with channels and no site builds the bank from its own rate and centre. Call events cross to the call
  thread on the same bounded queue as P25 calls, whose depth grows by 8 per enabled channel (unchanged without
  channels).
- **One control-channel stream per site**: every tuner serving a site runs its own engine on the control channel
  (an auto front hunts and locks it as well as the control front), so with two tuners each TSBK reaches the
  observations thread twice, stamped by two stream clocks. That thread folds one tuner's stream — the first to
  deliver, until it has been quiet for 3 s — into the event log, the observation and the radio events, and counts
  the other copies as `site.N.control_duplicates`. Until 2026-10-24 both were folded, which doubled all three
  (52.8 TSBK/s reported for a channel carrying 26.4).

### Time: the stream clock

Every timestamp trunkd writes — event-log `at`, call start/end, observation windows — comes from the **stream
clock**, not from `SystemTime::now()` at the moment of the event. Each device anchors a `StreamEpoch` to the wall
clock when its DSP thread drains the first block, and from then on maps stream time (consumed samples × 1000 ÷ sample
rate) to wall time as `epoch + at_ms`. One epoch per device: a two-tuner run has one timeline per tuner, converted at
the source, so calls from different tuners share a wall clock without sharing a stream clock. Inside a second the
sample counter is the *better* of the two clocks — it carries no NTP step and no scheduler jitter.

What a sample counter cannot see is **samples that never arrived**. On 2026-09-08 an out-of-memory stall on the
laptop recorder cost ~12 s of samples (trunkd survived; the ring's overflow counter never moved, because the samples
were lost before the ring). The sample count stayed 12 s behind wall time, so for the next 7.7 h every event and
every call was stamped 11.6 s early.

The stream clock is therefore **self-correcting** (`crates/trunkd/src/pipeline/streamclock.rs`). Once a second, on
the DSP thread's existing off-hot-path cadence, each device compares the wall estimate for its newest sample against
the wall clock:

- The steady-state difference is *not* error. The DSP thread always reads the newest sample some milliseconds after
  it arrived (whatever the ring holds), so the monitor spends its first 10 s measuring that offset — ignoring the
  first 2 s while the ring fills — and treats the mean as its **baseline**.
- If `(now − stream estimate) − baseline` exceeds **1 500 ms** (`STREAM_CLOCK_RESYNC_MS`) for **3 consecutive**
  checks, the epoch is re-anchored so the newest sample maps to `now − baseline`. Anything smaller is never
  re-anchored.
- A sample clock that is merely a few ppm off never trips that quickly, and used to be left to creep: on
  2026-10-16..22 the Windows recorder's two Airspys ran 3.3 and 4.3 ppm slow of the host, so each drifted 290-370
  ms a day toward the threshold, their stamps up to 1.5 s early and up to 0.65 s apart from each other. A **slow
  trim** now takes that out as it accrues: the error under the threshold is averaged over ~30 checks, and once the
  average passes 20 ms the epoch is moved 1 ms per check toward the wall clock until it is back under 5 ms.
  Forward steps land on the next block; backward ones join the slew below, so no stamp moves backwards. Inside a
  second the sample clock still rules; over minutes the wall clock does. `device.{id}.clock_error_ms` is the
  averaged error and `device.{id}.clock_trim_ms` the total moved (its slope is the rate error: 86 ms a day is
  1 ppm), both in the tuner objects as `clock_error_ms` / `clock_trim_ms`.
- The reference for both corrections is the host's system clock, so stamps are only as accurate as it is: keep it
  on NTP with short polls. Recommended Windows Time settings, and what they change, are in
  [windows-service.md](windows-service.md#host-clock-and-ntp).
- A resync logs a warning with the signed correction, bumps `device.{id}.clock_resyncs`, records the correction in
  `device.{id}.last_clock_resync_ms`, and writes a `{"name":"CLOCK_RESYNC","fields":{"correction_ms":…}}` record to
  the raw event log on the control channel. Corrections are spaced by at least 5 s.
- The correction lands on a **block boundary**, and calls already on the air keep the start they were given: nothing
  goes backwards inside a call. The cost, stated plainly, is that a call spanning a resync may be up to one
  correction long or short.
- Samples the ring drops are counted into stream time, drift is measured at the newest *produced* sample (not the
  newest consumed one, which lags under a backlog), and no stamp is ever written earlier than one before it on the
  same stream (`StampFloor`, a3ca959).

Independently of the clock, the DSP thread counts how long it *waited* for each block (measured from the end of the
previous block's processing, so it is the producer's gap and not our own work). A wait over `max(2 × block duration,
50 ms)` bumps `device.{id}.stream_gaps` — a cheap, direct signal that samples stopped arriving, whether or not the
drift ever crossed the resync threshold. All three counters appear per tuner in the panel's and the keyup status
POST's tuner objects.

These rules are the direct answer to sdrtrunk's failure modes (unbounded `Dispatcher` queues, unbounded cached thread
pool, synchronous listener fan-out): **no unbounded queue exists anywhere in trunkd, and nothing on a DSP thread ever
blocks on the control plane.** The price is that a drop-oldest queue can evict a call-lifecycle message: evictions
from the DSP → call-thread queue are counted (`calls.queue_dropped`), and a call whose end was evicted is closed by
the call thread after 60 s of silence (`calls.orphaned`, 1a43715). A full uplink queue spools the call instead of
dropping it (e84b8df).

## 3. Workspace layout

Cargo workspace, one binary:

- **`trunkd`** (bin + lib) — config load, startup, supervisor wiring, the pipeline threads (`src/pipeline/`), the
  local panel, and the CLI: `run`, `check`, `import-sdrtrunk`, `monitor`, and `service install/uninstall` on
  Windows.
- **`trunkd-core`** — shared types (frequencies, timestamps, identifiers, call/event structs), bounded-channel
  helpers, error taxonomy, metrics registry.
- **`trunkd-sdr`** — `TunerDevice` trait: enumerate/open/tune/gain/ppm + sample stream + error events. Backends as
  features, all on by default because none of them needs a native library at *build* time:
  `soapy` (primary; covers RTL-SDR, Airspy R2/Mini/HF+, SDRplay, remote SDRs via SoapyRemote — the SoapySDR C ABI is
  bound by hand and `dlopen`ed at run time, so only a machine that configures a Soapy device needs `libSoapySDR`
  installed), `hackrf` (a HackRF One through the stock `hackrf_transfer` CLI), `iqfile`
  (WAV/CS16/CF32 playback at real-time or unthrottled rate — the test source, format-compatible with sdrtrunk
  baseband recordings). Native `rtlsdr`/`airspy` backends can be added later behind the same trait if Soapy modules
  misbehave. Frequency planning: one device serves many channels; a center-frequency planner keeps all tuned channels
  inside the usable bandwidth (avoiding the DC spike), mirroring sdrtrunk's `CenterFrequencyCalculator`.
- **DSP** (planned as a `trunkd-dsp` crate; it lives in `trunkd-p25::dsp`) — f32 primitives: FIR/half-band
  decimators, RRC, mixers/NCO, carrier recovery, FFT wrapper. Two front ends: the per-channel DDC (mixer + half-band cascade
  → Kaiser tail, 50 or 25 ksps; O(N) in followed channels — ~3.8× realtime per channel at 20 Msps after the bit-exact
  block-path rework, spec §3.7) and the **Fred-Harris M2 polyphase channelizer** (`dsp::channelizer::PolyphaseChannelizerM2`, port of
  sdrtrunk `ComplexPolyphaseChannelizerM2`: `M = fs/12.5 kHz` bins at a native 25 ksps, one-/two-bin synthesis;
  O(1) in channel count — measured 11 channels of 60 s @ 20 Msps in 21.7 s vs 19.5 s for one, ≈ 0.36 % realtime per
  extra channel; spec §3.6). The channelizer is the recorder's path for control and traffic channels, one per
  device shared by every site on it. No explicit SIMD or `multiversion` yet: the filter loop is autovectorized
  scalar. **Closed-loop frequency correction is a requirement, not an optimization:**
  the P25 LSM decision-directed PLL false-locks π/2 away once the residual carrier offset exceeds ~400 Hz (spec §6.2);
  the engine emits `FrequencyError` (§14) and the recorder must retune/PPM-correct from it, as sdrtrunk does.
- **`trunkd-p25`** — Phase 1 C4FM + LSM demodulators (Phase 2 HDQPSK/TDMA: not built), sync detectors, message
  framers, EDAC
  (BCH(63,16,23), Golay 23/24, Hamming 10/15, RS 24,12 / 24,16 / 63,35 over GF(64), 1/2 & 3/4 trellis), TSBK/PDU/MAC
  parsing, and the trunking manager (control-channel following, grant → recorder-channel allocation from a bounded
  pool, patch tracking, continue/teardown logic — port of `P25TrafficChannelManager` behavior).
- **`trunkd-analog`** — analog NBFM for conventional `[[channel]]`s. Each enabled channel subscribes one centre on
  its device's channelizer bank (the same bank the P25 sites draw from, so a channel costs a 25 ksps stream, not a
  tuner) and runs an `NbfmChannel` engine on that device's DSP thread: a channel filter (off-grid channels take the
  bank's two-bin path, and the filter rejects 7.5 kHz-spaced neighbours), a power squelch against a tracked noise
  floor with a discriminator-noise veto, a polar discriminator, and two paths off it — sub-audio decimated to
  2.5 ksps into a CTCSS detector (the 50-tone table) and a DCS detector (Golay(23,12), all 104 codes, either
  polarity), and audio through a 300 Hz high-pass, de-emphasis and a 3.2 kHz low-pass resampled to 8 kHz i16 in 20 ms
  frames, the same PCM the IMBE path produces. A segmenter turns squelch and tone into calls: with a `tone`, audio
  is held from carrier-up until the tone confirms (`tone_timeout_ms`), so a call starts at the carrier, and a
  carrier with the wrong tone or none is discarded and counted as a tone mismatch; without one, the carrier alone
  opens it. Kerchunks under `min_call_ms` are dropped, transmissions over `max_call_s` are split. The engine emits
  `Started → Pcm* → Ended`; the call thread builds the call as for P25 and it uploads as a **group call** with the
  channel's system, site, talkgroup and name, plus the analog extras (`mode`, `decoder`, `tone`, `toneDetected`,
  `squelchReason`, `snrDb`, `peakDeviationHz`, `offsetHz`; [keyup.md](keyup.md#conventional-analog-channel-calls)).
  Nothing is built, subscribed or iterated when the config has no channels. *Status:* NBFM only; AM is not built.
- **`trunkd-mbe`** — IMBE (144-bit) and AMBE 3600×2450 (72-bit) frame decode → 8 kHz PCM; port of JMBE with its
  spec-algorithm structure preserved (auditable against the spec). GPL-3.0-or-later like JMBE; the IMBE patents have
  expired (`NOTICE.md`), so release binaries include it. *Status:* IMBE only; there is no AMBE decoder and no
  `ambe` feature yet.
- **`trunkd-call`** — call assembly: PCM accumulation per call, per-transmission source list (radio, offset,
  emergency), encryption flag handling (encrypted calls produce metadata-only calls by default), duration limits,
  WAV/MP3 encode for upload (MP3 via LAME binding to match Keyup's current corpus; WAV allowed by the contract).
- **`trunkd-uplink`** — delivery: Rdio-Scanner multipart POST to Keyup with trunk-recorder-style extras
  (`sources`, `frequencies`, `patches`, `talkerAlias`, emergency/encrypted), retry with backoff, and a bounded
  **offline spool**: when Keyup is unreachable, calls are written to `spool/YYYY/MM/DD/` as audio +
  trunk-recorder-style JSON sidecar (which Keyup's file ingest already understands), then drained on reconnect.
  Spool has a size cap and a free-space floor with oldest-first eviction. While Keyup fails, a circuit breaker
  sends live calls straight to the spool; only Keyup's documented permanent answers delete a call, and an entry
  that keeps failing is moved to `spool/quarantine/` (e84b8df).
  The worker also carries **direct-to-Broadcastify delivery**: when Keyup pushes a
  `recorder.broadcastify` block on the desired-state poll (and `[uplink] direct_broadcastify` has
  not vetoed it), each eligible finished call is registered with Broadcastify Calls and PUT there
  *before* the same call is POSTed to Keyup, so the outcome (`uploaded` | `duplicate` | `failed` |
  `late` | `claim_denied` | `skipped`) rides along on that push as `broadcastifyOutcome` and
  friends. It happens on the uplink thread, never on a DSP thread, and is bounded by its own
  timeouts (claim 1.5 s, register 4 s, PUT 6 s). The verdict is decided once and written into the
  spool sidecar, so a call retried out of the spool is never registered a second time. Optionally
  gated by a **claim** to Keyup, so several recorders hearing one site do not all upload it.
  Ahead of all of that is a local **speech gate**: call assembly measures `speech_s` — seconds of
  20 ms frames whose RMS clears −40 dBFS — while the PCM is still in hand, it rides the Keyup push
  as `speechS` and the sidecar as `speechS`, and a call carrying less than the block's
  `min_speech_s` (default 0.25 s, the floor Keyup classifies with) is `skipped` with `no speech
  (0.04 s)` before any claim or register. A mic click costs one float compare instead of a register
  and a PUT, and never reaches the feed. See [keyup.md](keyup.md#recorderbroadcastify--uploading-each-call-to-broadcastify-calls) for the wire
  contract.
- **`trunkd-link`** — the recorder link: HMAC-authenticated UDP between trunkd instances for hellos, control-lock
  state, grant hand-off (acked, retried) and call claims, with a per-sender replay window (a3ca959).
- **`trunkd-api`** — *not built as a crate.* The HTTP surface is the local panel in `trunkd` (see §5).
- **`trunkd-supervisor`** — health model (see §6).

## 4. P25 recording behavior

- A **site** = one control channel (with alternates) + a traffic-channel recorder pool, all served by whichever
  configured device covers the frequencies. Control channel is decoded continuously; decode rate and sync state are
  health signals. Control-channel loss triggers hunt across the configured alternate frequency list.
- Grants allocate a recorder from a bounded pool (like sdrtrunk's traffic-channel pool; exhaustion increments a
  counter and the call is still logged as metadata-only). Continues update the active call; teardown/timeout closes
  it. Patch grants record the supergroup with the full member list.
- **Skipped talkgroups.** A talkgroup the operator has turned Record, Transcribe and every export
  off for is refused *at the grant*, before a tuner is spent on it: no channelizer slot, no call,
  no pool slot held (the one the trunking tracker allocated is handed straight back), and no
  `GrantOut` for a sibling tuner or a link peer — every peer holds the same list, so routing it
  would only move the refusal one hop. The list is `[recorder] skip_talkgroups` in the TOML
  **unioned** with Keyup's `recorder.skip_talkgroups` block on the desired-state poll, which is
  revision-gated exactly as `recorder.broadcastify` is (a `revision` that differs applies at once
  with no restart; `null` drops Keyup's half and leaves the file's alone) and shared with the DSP
  threads as one `trunkd_core::SkipList`, so a change lands on the very next grant. What does not
  change: the grant is decoded, written to the event log and counted, and the radio events around
  it are archived as always — the archive still knows the radio keyed up, there is simply no audio
  for it. A call already on the air when its talkgroup joins the list runs to its own end, and a
  call on a patch supergroup is skipped only when every live member talkgroup is skipped too.
  Counted as `calls.skipped_by_policy` (once per call, not per grant refresh) and reported on
  `/api/status` as `counters.calls_skipped_by_policy` and `recorder.skip_talkgroups`.
- **Phase 2 (TDMA) grants are refused, not followed** (interim, until TDMA voice exists). A grant
  whose band the site announced with a TDMA channel type (IDEN_UPDATE_TDMA, opcode 51) is
  recognised by the trunking tracker before it is tracked or allocated: no pool slot, no
  channelizer slot, no call, no `GrantOut`. The `Grant` itself is still reported, so the event log
  and the site's traffic list keep it. Counted once per call per (carrier, timeslot) as
  `calls.tdma_refused` (`counters.calls_tdma_refused` on the status POST and `/api/status`), per
  site as `site.N.calls_tdma_refused` (`sites[].calls_tdma_refused`, the status line's
  `tdma_refused`, the panel's "phase 2 refused" column), and per traffic frequency in the
  observation document (`traffic[].tdma_refused`, total `tdma_calls_refused`); the first refusal
  of each talkgroup is logged at INFO.
- Every call carries: system/site labels, TG + patch members, per-transmission radio IDs with offsets, frequency,
  timeslot (P2), true start/stop timestamps (ms), emergency and encrypted flags — i.e., the fields Keyup reserved
  but sdrtrunk's file path could never provide.
- Phase 2: scramble parameters learned from the control channel are applied to traffic channels at grant time.

## 5. Control API (consumed by Keyup Admin's Recorder page)

> *Status:* the `/api/v1` API below is the design and does not exist. Keyup drives trunkd the other way round: the
> recorder polls Keyup's desired state (plus a push channel that pulls a poll forward) and POSTs tuner status,
> observations, radio events and spectrum to it. Locally, `[server] enabled` serves the panel's routes —
> `/api/status`, `/api/about`, `/api/config` (secrets redacted), `/api/config/devices[/{id}]`,
> `/api/config/reload`, `/api/devices/discover`, `/api/uplink`, `/api/sites/{i}/detail`,
> `/api/tuners/{key}/restart`, `/api/monitor/*` — with one flat token list (no scopes), no OpenAPI, no
> Prometheus `/metrics`, no WebSocket. [panel.md](panel.md) is the reference.

Design:

- `GET /status` — process health, uptime, version, per-component state (the §6 tree), upload/spool depth.
- `GET /devices` · `POST /devices/{id}/restart` — tuner inventory, live stats (sample rate, overflow count, PPM),
  manual restart.
- `GET /sites`, `GET /channels` — control-channel state (sync, decode rate, NAC), active calls, recorder pool usage.
- `POST /sites/{id}/enable|disable`, `POST /channels/{id}/enable|disable`.
- `GET /calls/active` — in-progress calls (for a live board).
- `GET /devices/{id}/spectrum` — windowed FFT magnitude snapshot (data only; UI renders it).
- `GET /config` · `PUT /config` (validate + atomic rewrite + component restart), `POST /config/import-sdrtrunk`
  (playlist `default.xml` → sites/channels).
- `GET /metrics` — Prometheus.
- `WS /events` — status changes, call start/end, health transitions (the Keyup Recorder page subscribes).

## 6. Supervision and self-healing

An in-process supervision tree, not an afterthought:

- Every component (device reader, DSP pipeline, site decoder, uplink) is a supervised unit with a state
  (`Running/Degraded/Failed/Restarting`), heartbeat, and restart policy (exponential backoff, cap, escalation).
- **Watchdog signals**: SDR read stalls / overflow bursts; control-channel sync loss and message-rate collapse;
  DSP thread heartbeat; uploader error streaks; spool growth. Tuner death → automatic close/reopen/re-enumerate with
  backoff (the fix for sdrtrunk's GUI-only tuner restart, upstream issue #1890).
- Escalation: component restart → device restart → process exit with distinct exit code, letting systemd
  (`Restart=on-failure`, `WatchdogSec` via sd-notify) or the Windows service recovery settings restart the process.
  *Status:* a device unit that spends its restart budget logs an error and keeps retrying every 30 s to 10 min
  (the panel's restart button or a new Keyup version retries at once, ff6c182); nothing exits the process and there
  is no sd-notify watchdog.
  The process is always safe to kill: no state matters except the spool, which is crash-safe (write temp + rename).
- Keyup's `recorder_gaps` detection remains the independent outer safety net.

## 7. Configuration

One TOML file (`trunkd.toml`), env-var overrides (`TRUNKD_*`), reload via the panel's `POST /api/config/reload`
or a Keyup desired-state change (no SIGHUP handler). Sections:
`[server]` (bind, tokens), `[uplink]` (Keyup URL, API key, spool path/cap), `[[device]]` (driver args, sample
rate, gain, ppm), `[[site]]` (system/site labels, control + alternate frequencies, traffic pool size, phase 2
options), `[[channel]]` (conventional analog channels: NBFM; `mode = "am"` is skipped), `[health]` (thresholds). A `--check` mode validates config
and device availability and exits.

### 7.1 Live audio monitor (`[server] live_audio`)

Off by default, and a decision separate from `[server] enabled`. When it is on, the call thread publishes each
20 ms vocoder frame into a fan-out hub as well as into the call being assembled, and the panel serves
`GET /api/monitor/stream[?talkgroup=N]` as a chunked `audio/mpeg` body a browser's `<audio>` element can play.
`GET /api/monitor/status` reports `{enabled, streams, max_streams}`; `/api/status` carries the same block plus
`monitor_streams`.

What it exists for is the one question the archive cannot answer: *is what this box is decoding, right now,
intelligible?* A misaligned tuner or a vocoder fed marginal frames produces calls that upload perfectly and sound
like gravel. It is not a scanner and not a second listening surface — finished calls are listened to in Keyup.

Rules it is built to:

- **Free when off.** The hub is not constructed at all, so the call thread holds `None`. With it on and nobody
  listening, publishing is one relaxed atomic load.
- **Never blocks the call thread.** Delivery is a `try_send` into a bounded 2 s queue per listener and drops on a
  full one; drops are counted (`monitor.dropped_frames`), never waited on.
- **Never blocks the accept loop.** Every panel request runs on a thread of its own (at most 16 in flight, with
  read/write time limits, 2424b15); the stream holds its thread only while it plays.
- **Bounded.** Four concurrent streams, then 503. Each ticks on a 20 ms monotonic deadline, mixes simultaneous
  calls by saturating sum, emits a frame of silence when the site is idle (so the browser's decoder never sees
  the stream stall), and encodes incrementally through one long-lived LAME encoder at 32 kbps mono 8 kHz.
- **Same auth as any read**, with one widening: because an `<audio>` element cannot send a header, this route
  also accepts `?token=`.
- Encrypted calls are silent here for the same reason they upload without audio: the voice is discarded before
  it is vocoded.

## 8. Testing and validation

- **Golden-vector tests**: EDAC decoders and framers tested against vectors generated from the reference Java
  implementations (run once, committed as fixtures).
- **IQ replay tests**: baseband IQ recordings of a live P25 system (captured locally; not shipped in the
  repository) replayed through `iqfile`; assertions on decoded TSBK counts, grants, and call
  metadata vs sdrtrunk's decode of the same file. Decode parity with sdrtrunk is the acceptance bar for `trunkd-p25`.
- **Voice parity**: `.mbe` JSON call sequences from sdrtrunk decoded by `trunkd-mbe` and compared (within tolerance)
  to JMBE output WAVs.
- **Uplink contract tests** against a local Keyup instance (its `/api/call-upload` responses are the spec).
- **Synthetic analog tests** (no capture needed): `trunkd-analog`'s `testsig` feature modulates NBFM with CTCSS/DCS
  at a chosen SNR and offset for the detector, squelch and segmenter tests, and
  `crates/trunkd/tests/conventional_pipeline.rs` writes a 2 Msps CF32 capture with four keyed carriers (CTCSS, DCS,
  carrier squelch, and a wrong tone that must not record), replays it through `run_once` against a mock Keyup and
  checks every upload's fields and MP3 length. It runs in every `cargo t` in a few seconds.
- End-to-end soak: `iqfile` in a loop for hours under valgrind/heaptrack once per release; RSS must be flat.

## 9. Build order

1. **A0** — workspace, `trunkd-core`, config, logging (tracing + rotation), skeleton supervisor, `--check`.
2. **A1** — `trunkd-sdr`: `iqfile` source + `soapy` backend; ring-buffer ingest; device supervision.
3. **A2** — `trunkd-dsp`: DDC (mixer + half-band cascade), FIR/RRC, squelch; bench harness.
4. **A3** — `trunkd-p25` Phase 1: C4FM demod → framer → EDAC → TSBK → trunking manager (decode parity vs sdrtrunk on
   recorded control-channel IQ).
5. **A4** — `trunkd-mbe` IMBE + `trunkd-call` + `trunkd-uplink` (first live call lands in Keyup).
6. **A5** — `trunkd-api` + full supervision + systemd/Windows service packaging.
7. **A6** — P25 Phase 2 (HDQPSK/TDMA), Phase 1 LSM, AMBE, analog NBFM/AM, polyphase channelizer optimization.
   *Status:* Phase 1 LSM, analog NBFM and the polyphase channelizer are built; Phase 2, AMBE and AM are not.
8. **A7** — Keyup Admin “Recorder” page (in the Keyup repo) consuming the control API.
