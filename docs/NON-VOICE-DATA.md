# Non-voice data on a P25 Phase 1 L3Harris site

What the P25 engine already decodes off the air and the recorder throws away, how
much of it there is, what it would tell an analyst, and what is worth keeping.

Scope: **one site of a multi-site L3Harris P25 Phase 1 LSM (simulcast) system**,
measured off the air from its control channel and traffic channels. Radio,
talkgroup, key and system identifiers below are replaced with example values
(WACN `0xBEE00`, local system `1`); the message rates are as measured. Everything
below is measured, not assumed; §7 says plainly what the measurements cannot show.

> **Status: implemented.** §5's "retain now" (1 and 2) and the retain-later
> items that only needed the parsers (3, and 5's decoder) are in the recorder;
> §6's Transport A and Transport B are both live. The measurements and the
> reasoning below are unchanged — they are what the implementation was built
> from — but the "dropped" column of §3 now describes what the engine did
> *before* this work, not what it does. [keyup.md](keyup.md) documents the wire
> formats, [configuration.md](configuration.md) the config keys, and
> [operations.md](operations.md#the-raw-event-log) how to read the event log.
>
> | §5 item | state |
> | --- | --- |
> | 1. Registration & affiliation family | **implemented** — 11 opcodes parsed, POSTed as `radio_events` |
> | 2. Fully qualified source SUID (LC 9) | **implemented** — `sourceWacn` / `sourceSystem` on the call upload |
> | 3. SNDCP data activity | **implemented** — opcodes 20/21 as events; PDU counters in the observation window |
> | 4. Generic rare-command capture | **implemented as the raw event log** rather than as `radio_events` rows: §7 is right that their rate is unknown, and a log answers that question before a schema has to |
> | 5. Harris talker GPS (LC 42/43) | **parser written, unverified** — `decode_harris_gps`, exercised only by synthetic words, because no capture has ever shown the blocks |
> | 6. PDU payload bytes | **not done** — still only counts and validity |
> | 7–11. Discard | **as recommended** — beacons kept as one exemplar per window, the firehose as a histogram, LSD counted not stored |

---

## 1. What the recorder forwards today

`trunkd run` (crates/trunkd/src/recorder.rs → `pipeline/`) sends Keyup exactly two
things:

| Path | Content |
| --- | --- |
| `POST /api/call-upload` (per call) | audio + `FinishedCall`: system/site, start/end, frequency, talkgroup, source + per-transmission `sources`, `frequencies`, patches, emergency, encrypted, algorithm, key id, talker alias, NAC |
| `POST /api/recorder/observations` (every `observations_interval_s`, 300 s live) | `InventorySnapshot`: NAC/WACN/system/RFSS/site/LRA, live + secondary control channels, band plan, granted traffic frequencies with grant/call counts, adjacent sites, configured control candidates, TSBK rate |

Everything else the engine decodes is dropped, and it is dropped in exactly one
place. `SiteConfig::raw_control` is **already `true`** in production
(`pipeline/device.rs:119`), so every control-channel `P25Event` already crosses
the bounded queue as an `ObsMsg` into the observations thread
(`pipeline/device.rs:496`). There, `SiteInventory::observe`
(`crates/trunkd-p25/src/inventory.rs:310`) matches on six event kinds —
`NidDecoded`, `Tsbk` (counted; only `IdenUpdate` read), `TsbkFailed`,
`IdenUpdate`, `Grant`, `CallStart`, `SiteBroadcast` — and ends with `_ => {}`.
**That `_ => {}` was the entire loss.** Traffic-channel events were dropped one
layer lower: `CallAssembler` (`crates/trunkd-p25/src/call.rs:201`) ended with
`LinkControl::Other { .. } => {}`.

**Both are now closed.** The observations thread runs a second fold beside
`SiteInventory::observe` (`crates/trunkd/src/pipeline/nonvoice.rs`) that turns
the registration/affiliation/data family into batched `radio_events`, counts
every TSBK into the window histogram, and writes the raw event log;
`CallAssembler` parses LC 9 and latches the talker's WACN/system onto the call.
Traffic-channel signaling reaches the fold through a new
`SiteConfig::raw_traffic` pass-through that mirrors `raw_control` and **drops
IMBE frames at the source**, so voice cannot reach any log.

There is no `raw_control` config knob to turn on: the pass-through is already on.
To see the stream offline, run the example the histograms below came from:

```sh
cargo build --release -p trunkd-p25 --examples
target/release/examples/site_decode <capture.iq> --rate 20000000 --format int8 \
    --center <CENTER_HZ> --control <CONTROL_HZ> [--control ...] --raw-control --quiet
```

A live tap would be a `[uplink] raw_control_log = "path.jsonl"` (or a second fold
next to `SiteInventory::observe`) — no DSP change, no new thread, no risk to the
audio path.

---

## 2. Method

| Capture | Length | Command |
| --- | --- | --- |
| 2.4 Msps control-channel capture | 10 s control only | `p25_decode … --rate 2400000 --offset <HZ>` |
| 20 Msps wideband capture ("run 1") | 90 s wideband | `site_decode … --raw-control --quiet` + `channelizer_decode` over the 8 granted traffic frequencies |
| 20 Msps wideband capture ("run 2") | 60 s wideband | same, over its 9 granted traffic frequencies |
| sdrtrunk's decoded-message log of the 10 s capture | sdrtrunk reference decode | payload semantics for opcodes trunkd only counts |
| a live recorder log | 46 min live, read-only | daytime/overnight sanity check |

The captures are local (see `fixtures/README.md` for capturing your own); they
are not shipped with the repository.

`harness/nonvoice_histogram.py` (added with this document) turns those JSONL
dumps into the tables below: it names every opcode the way sdrtrunk's `Opcode` /
`LinkControlOpcode` enums do, marks each as *parsed* or *DROPPED* by the current
engine, and hand-decodes two things the engine leaves raw — standard LC opcode 9
(Source ID Extension) and the L3Harris talker-GPS block pair.

No hardware was touched: the live recorder kept running throughout, and no
fresh capture was taken.

**Total control-channel sample: 150 s wideband (3 956 TSBKs) + 10 s narrowband
(227 TSBKs), plus 32 followed calls across 17 traffic frequencies.**

---

## 3. The histogram

### 3.1 TSBKs, both wideband runs (150 s, 3 956 TSBKs, 26.4/s — the live log's
`tsbk_per_s` is 25.0–26.8, so this is representative)

| n (150 s) | /hour | mfid | op | sdrtrunk name | engine |
| ---: | ---: | ---: | ---: | --- | --- |
| 695 | 16 680 | 0 | 2 | GRP_VCH_GRNT_UPD | parsed (drives following) |
| 488 | 11 712 | 164 | 15 | HARRIS OSP UNKNOWN 15 | **dropped** (`HarrisOsp`) |
| 486 | 11 664 | 164 | 1 | HARRIS OSP UNKNOWN 1 | **dropped** (`HarrisOsp`) |
| 484 | 11 616 | 0 | 58 | RFSS_STATUS_BCST | parsed → observations |
| 482 | 11 568 | 0 | 59 | NET_STATUS_BCAST | parsed → observations |
| 244 | 5 856 | 0 | 22 | SNDCP_DCH_ANN_EX | **dropped** (`Other`) |
| 244 | 5 856 | 0 | 60 | ADJ SITE STATUS | parsed → observations |
| 243 | 5 832 | 0 | 56 | SYS_SVC_BCAST | **dropped** (`Other`) |
| 242 | 5 808 | 0 | 61 | IDEN_UPDATE | parsed → band plan |
| 241 | 5 784 | 0 | 57 | SEC_CCH_BROADCST | parsed → observations |
| 60 | 1 440 | 0 | 0 | GRP_VCH_GRANT | parsed |
| 19 | 456 | 0 | 20 | SNDCP_DCH_GRANT | **dropped** |
| 12 | 288 | 0 | 44 | UNIT_REG_RESPONS | **dropped** |
| 7 | 168 | 0 | 40 | GRP_AFFIL_RESP | **dropped** |
| 3 | 72 | 0 | 42 | GRP_AFFIL_QUERY | **dropped** |
| 3 | 72 | 0 | 47 | DE_REGIST_ACK | **dropped** |
| 2 | 48 | 0 | 21 | SNDCP_DCH_PAG_RQ | **dropped** |
| 1 | 24 | 0 | 32 | ACK_RESPONSE_FNE | **dropped** |

The "engine" column is as of the measurement. Since then opcodes **20, 21, 22,
32, 33, 39, 40, 42, 43, 44, 45 and 47** are parsed, and all but 22 are emitted
as `radio_events` — opcode 22 is the static `CHAN:0-0 DAC:65535` advertisement
§4 found to carry nothing, so it is counted and not stored. **56** and the two
Harris beacons are counted in the window histogram with one raw exemplar each,
which is what §5.7–8 asked for rather than a row apiece.

The 10 s narrowband fixture agrees (same opcode set: 1× op 44, 1× op 40, 16× op
22, 16× op 56, 31× Harris 1, 34× Harris 15). sdrtrunk's decode of that fixture
gives the payloads trunkd only counts:

```
UNIT_REG_RESPONS REGISTRATION ACCEPTED FOR RADIO:1000001
GRP_AFFIL_RESP TO:1000001 GROUP ADDRESS:1001 (GLOBAL) AFFILIATION:ACCEPTED
SNDCP_DCH_ANN_EX CHAN:0-0 REQUESTED-ACCESS DAC:65535 NSAPI:0 HALF DUPLEX
SYS_SVC_BCAST AVAILABLE [GROUP_VOICE, INDIVIDUAL_VOICE, INDIVIDUAL_DATA,
    UNIT_REGISTRATION, GROUP_AFFILIATION, USER_STATUS, USER_MESSAGE, USER_STATUS_QUERY]
HARRIS **UNRECOGNIZED OSP OPCODE:15 MSG:8FA4…  (12 bytes, ×37, byte-identical)
HARRIS **UNRECOGNIZED OSP OPCODE:1  MSG:81A4…  (12 bytes, ×30, byte-identical)
```

**25 % of every TSBK on this control channel is one of two L3Harris vendor
messages that never change a single byte.** They are beacons, not events.

### 3.2 Link Control, traffic channels (150 s, 32 calls)

| n | mfid | lco | name | engine |
| ---: | ---: | ---: | --- | --- |
| 79 / 115 | 0 | 0 | GROUP VOICE CHANNEL USER (LDU1 / TDULC) | parsed → talkgroup/source/emergency |
| 132 | 0 | 15 | CALL TERMINATION (TDULC) | dropped (redundant with call end) |
| 73 | 0 | 9 | **SOURCE ID EXTENSION** (LDU1 40, TDULC 33) | **dropped** (`Other`) |
| 51 | 164 | 10 | L3HARRIS RETURN TO CONTROL CHANNEL (TDULC) | dropped |
| 31 | 164 | 50 | L3HARRIS TALKER ALIAS BLOCK 1 | parsed → talker alias |
| 14 | 164 | 51 | …BLOCK 2 | parsed |
| 11 | 164 | 52 | …BLOCK 3 | parsed |
| 6 | 164 | 53 | …BLOCK 4 | parsed |
| 21 | 0 | 2 | GROUP VOICE CHANNEL UPDATE | dropped (duplicate of control-channel grants) |
| 1 | 0 | 16 | GROUP AFFILIATION QUERY (FNE → one radio) | **dropped** |
| 0 | 164 | 42/43 | L3HARRIS TALKER GPS BLOCK 1/2 | **not present in either fixture** |

Since the measurement, LC **9** is parsed (`LinkControl::SourceIdExtension`) and
latched onto the call; every LC is counted into `link_control_mix` by
`(vendor, lco)`; and LC 42/43 have a decoder (`decode_harris_gps`) that no
capture has yet been able to verify.

The Source ID Extension words decode cleanly (sdrtrunk `LCSourceIDExtension`
layout: WACN bits 16–35, system 36–47, id 48–71) to **11 distinct fully qualified
roaming subscribers in 150 s**:

```
WACN:BEE00 SYSTEM:2 ID:2000001   WACN:BEE00 SYSTEM:2 ID:2000002
WACN:BEE00 SYSTEM:2 ID:2000003   WACN:BEE00 SYSTEM:2 ID:2000004
WACN:BEE00 SYSTEM:3 ID:3000001   WACN:BEE00 SYSTEM:2 ID:2000005  … (+5)
```

The two foreign systems (here 2 and 3) are exactly the neighbours the
adjacent-site broadcasts announce. These are out-of-system radios working
on our site — and Keyup's `radios` table already has `wacn` / `home_system_id`
columns for precisely this, which today are always `0` because trunkd never sends
them.

### 3.3 HDU / encryption / low-speed data / PDU

* **HDU**: 24 valid headers, 10 CRC failures; talkgroup + ALGID + key id.
  Three distinct key ids seen, all AES-256, plus clear calls.
  Already reaching Keyup through the call record (`algorithm`, `key_id`).
* **LDU2 encryption sync**: 195 words, same three key ids. Per-call, already
  covered; the *superframe-level* MI stream is not, and is worthless without keys.
* **Low-speed data**: 398 octet pairs, **`0000` in 393 of them**; the rest are
  single-octet FEC failures. This system does not use LSD. Discard.
  (Counted, not stored, as `lsd_nonzero`. The recorder's own replay of run 2
  finds **2** correctable non-zero pairs in 60 s — near-zero, as measured, but
  not exactly zero, which is why the counter exists rather than an assumption.)
* **PDU**: none at all on the control channel in 150 s. On the traffic band,
  run 2 caught real packet data on one traffic frequency:
  `format 22 (PACKET DATA), confirmed, LLID 1000002, 2 blocks, valid` plus two
  `format 3 (RESPONSE)` headers for the same LLID. That is a subscriber's SNDCP
  session — the thing opcodes 20/21/22 are about. The engine validates the blocks
  but **never surfaces their payload bytes**, so whether they carry LRRP/GPS or
  plain IP cannot be answered from the current API.

### 3.4 Volume, if stored raw

| Class | events/hour | rows/day | JSONB bytes/day (≈120 B/row) |
| --- | ---: | ---: | ---: |
| Every TSBK | 95 000 | 2.28 M | ~270 MB |
| The two Harris beacons | 23 400 | 561 k | ~67 MB |
| SNDCP announcement (op 22) | 5 856 | 141 k | ~17 MB |
| Registration/affiliation family (40/42/43/44/45/47) | ~600 | 15 k | **~1.8 MB** |
| SNDCP grants + pages (20/21) | ~500 | 12 k | ~1.4 MB |
| Fully qualified SUIDs (LC 9), deduped per call | ~770 | 18 k | rides the call upload, 0 new rows |

The interesting classes are three orders of magnitude cheaper than the firehose.

---

## 4. What each class is worth

**Registration / affiliation (OSP 44, 47, 40, 42, 43, 45).** The single best
answer to "which radios are *here*". Today `radio_affiliations` is populated only
from calls (`ingest/service.py:touch_affiliation`, +1 per call), so a radio that
is on the air but never keys up is invisible to the archive. Unit registration
responses name the radio; affiliation responses name radio **and** talkgroup, and
are emitted when a user turns a knob — before any call. In the 10 s narrowband
fixture, one radio registered *and* affiliated to a talkgroup without appearing
in any call. Volume trivial (~600/h). Sensitivity: real — this is presence and
shift-pattern tracking of individual, identifiable radios, including ones whose
users never transmit. It is the same class of data Keyup already stores per call,
but denser.

**Fully qualified source SUID (LC 9).** Turns "radio 2000001" into
"BEE00.2.2000001 — a system-2 radio roaming onto this site". Prevents the
archive from silently merging a foreign radio into the local numbering space, and
makes mutual-aid activity visible. Volume zero-new: fold it into the call record.

**SNDCP data activity (OSP 20/21/22 + PDU headers).** Tells you *that* data is
flowing, for which LLID, on which channel, and how much. `site_frequencies`
already has a `role` column, and a data channel is a role the site never
otherwise reveals. The steady 1.6/s opcode-22 announcement is a static
advertisement (`CHAN:0-0 DAC:65535`) and carries nothing; the grants (op 20, 456/h)
and the actual PDUs do.

**Talker GPS (Harris LC 42/43).** Not present in these fixtures at all, so it
cannot be quantified here. If it ever appears it is per-second unit *location* —
the highest-value and highest-sensitivity item on this list. Worth building the
parser only once a capture shows the blocks.

**Emergency / status / message / call alert / extended function / radio unit
monitor (OSP 24, 26, 28, 29, 31, 36, 39, 32, 33).** Zero to one instance in 150 s.
Individually these are the most *interesting* messages on a P25 system (an
emergency alarm, a "call me" alert, a covert radio monitor command), which argues
for catching them generically rather than building a table per opcode.

**Site health (OSP 56 SYS_SVC_BCAST, and the service-class bits already parsed).**
A static capability bitmap. It belongs in the observation snapshot as *fields*,
not as 5 832 events per hour.

**The two Harris beacons (mfid 164 op 1 and op 15).** 23 400/hour, byte-identical
every time, semantics unknown to trunkd *and* to sdrtrunk. Nothing an analyst can
use. One exemplar per observation window is enough to notice the day they change.

**Low-speed data, TDULC call termination, LC group voice update on traffic.**
Empty, redundant, redundant. Discard.

**What we structurally cannot see:** every ISP (inbound, subscriber→FNE) message
— emergency alarm *requests*, registration *requests*, affiliation *requests*,
status *updates from the field* — rides the uplink pair (−45 MHz on band 1) which
the recorder never tunes. We only ever see the FNE's downlink responses. Any
design that says "capture emergency alarms" must mean "capture the emergency bit
in service options and the FNE's acknowledge/deny responses".

---

## 5. Ranked recommendation

### Retain now

1. **Registration & affiliation family** — OSP 44 (unit reg response), 47
   (de-registration ack), 40 (affiliation response), 42 (affiliation query), 43
   (location registration response), 45 (unit registration command).
   *Engine*: 6 new `TsbkMessage` variants + a `P25Event::RadioEvent`; the field
   layouts are in the spec and mirrored in sdrtrunk's `osp/` classes — roughly the
   size of the site-broadcast work already merged.
   *Recorder*: a second fold beside `SiteInventory::observe` in the observations
   thread; the events already arrive there.
   *Keyup*: one `radio_events` table + fold into `radios`/`radio_affiliations`.
2. **Fully qualified source SUID (LC 9)** — parse in `LinkControl`, latch onto the
   open call in `CallAssembler`, add `source_wacn` / `source_system` to
   `FinishedCall` and to the call-upload form. **No new endpoint, no new table** —
   `radios.wacn` and `radios.home_system_id` already exist and are waiting.
   Cheapest high-value change on the list; do it first.

### Retain later

3. **SNDCP data activity** — OSP 20/21 as `radio_events` rows (kind
   `data_grant` / `data_page`), and the granted frequency as a
   `site_frequencies` row with `role='data'`. Add the PDU header (format, SAP,
   LLID, block count) as a counter in the observation window.
4. **Generic rare-command capture** — OSP 24, 26, 28, 29, 31, 32, 33, 36, 39, 46,
   49, 50, 53, 54, 55 into the same `radio_events` table with `kind='other'` and
   the decoded fields in `meta`. Cheap once (1) exists; near-zero volume.
5. **Harris talker GPS (LC 42/43)** — parser is ~30 lines (layout in
   `harness/nonvoice_histogram.py:decode_harris_gps`, verified against sdrtrunk's
   `L3HarrisGPS`), but build it only when a capture actually shows the blocks.
   Treat as sensitive: it is live officer location.
6. **PDU payload bytes** — would need the engine to surface data-block contents
   (it currently reports only counts and validity). Only worth it if someone wants
   to know whether the observed data-channel packets are LRRP.

### Discard

7. The two L3Harris beacons (op 1, op 15) — keep one exemplar per observation
   window in the observation JSON, drop the other ~23 400/hour.
8. OSP 56 SYS_SVC_BCAST — fold the service bitmap into the observation snapshot
   as a field; drop the events.
9. Low-speed data — all zeros on this system.
10. TDULC CALL TERMINATION (LC 15), L3Harris return-to-control-channel (LC 10),
    LC 2 group voice update on traffic channels — redundant with what the call
    record and the control channel already say.
11. The raw TSBK firehose — never store 2.3 M rows/day. Store the *histogram*
    (opcode → count) per observation window instead: 20-odd numbers every 5
    minutes, which is enough to notice a new opcode appearing.

---

## 6. Proposed schema and transport

**Transport A — extend the existing observations POST** (no new endpoint, and
`recorder_observations.payload` is already stored verbatim as JSONB):

```jsonc
{ // … existing document …
  "message_mix": { "0/2": 695, "164/15": 488, "0/44": 12, … },   // per window
  "unknown_vendor": [ { "mfid": 164, "opcode": 15, "count": 488,
                        "exemplar": "8fa4…" }   // the 12 raw bytes, hex ],
  "services": { "available": ["group_voice","individual_data","unit_registration", …] },
  "data": { "grants": 19, "pdus": 3, "channels": [855012500] }
}
```

**Transport B — new batch endpoint for per-radio events**, same Bearer token with
the `upload` scope the call push and observations already use (Keyup's
`require_recorder_token` is reusable verbatim):

```
POST /api/recorder/events
{ "recorder": "trunkd", "system": {"label": "Example System", "wacn": 781824, "system_id": 1},
  "site": {"label": "Example Site", "rfss": 1, "site": 1},
  "events": [ { "at": "2026-08-31T09:12:03.481Z", "kind": "affiliation",
                "radio_id": 1000001, "radio_wacn": 781824, "radio_system": 2,
                "talkgroup": 1001, "result": "accepted", "meta": {"opcode": 40} } ] }
```

Batched every ~10 s or 500 events, drop-oldest on a bounded queue like the rest
of the pipeline, spooled on failure exactly as calls are.

**Keyup schema — one table, kind + JSONB** (a table per opcode family would be
six tables for ~15 k rows/day):

```sql
CREATE TYPE radio_event_kind AS ENUM (
  'registration','deregistration','registration_command','affiliation',
  'affiliation_query','location_registration','status','message','call_alert',
  'extended_function','radio_monitor','ack','deny','queued','data_grant',
  'data_page','gps','other');

CREATE TABLE radio_events (
  id              bigserial PRIMARY KEY,
  system_id       bigint REFERENCES systems(id) ON DELETE CASCADE,
  site_id         bigint REFERENCES sites(id)   ON DELETE CASCADE,
  at              timestamptz NOT NULL,
  kind            radio_event_kind NOT NULL,
  radio_id        bigint,            -- the subscriber the message is about
  radio_wacn      bigint  NOT NULL DEFAULT 0,   -- 0 = local, as in radios
  radio_home_system integer NOT NULL DEFAULT 0,
  target_radio_id bigint,            -- unit-to-unit / query target
  talkgroup       integer,
  result          text,              -- accepted | denied | refused | failed
  opcode          smallint NOT NULL,
  mfid            smallint NOT NULL DEFAULT 0,
  meta            jsonb,
  recorder        text NOT NULL DEFAULT 'trunkd',
  created_at      timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX ON radio_events (site_id, at DESC);
CREATE INDEX ON radio_events (radio_id, at DESC);
CREATE INDEX ON radio_events (kind, at DESC);
```

Ingest folds each row into `radios` (`first_seen_at`/`last_seen_at`, and `wacn` /
`home_system_id` when the SUID extension gave them) and, for affiliation kinds,
into `radio_affiliations` — which then stops meaning "was heard talking" and
starts meaning "was present". That change of meaning should be explicit: add a
`source` column to `radio_affiliations` (`call` | `affiliation`) rather than
mixing the two silently.

Retention: `radio_events` is presence data about identifiable people. It deserves
its own `retention_policies` row — 90 days is a defensible default, and shorter
than the audio.

---

## 7. What these measurements cannot tell you

* **150 s is a small window for rare messages.** Emergency-related responses,
  status updates, message updates, call alerts, radio unit monitor, extended
  function, authentication, roaming address and time/date announcements appeared
  **zero times**. Their true rate could be anything from "never" to "several an
  hour at shift change". A day-long `raw_control` log is needed before any of them
  is designed for; that is why §5 puts them behind one generic table.
* **The fixtures are evening/afternoon captures** (the narrowband one at 16:44
  local, the wideband ones ~19:45). The live log used for cross-checking covers
  03:50–04:36 local — overnight, 135 calls in 46 min. TSBK rate is essentially
  identical in both (26.4 vs 25.0–26.8/s) because the control channel is dominated
  by beacons, but the *registration* rate almost certainly is not: shift change
  should produce a burst that neither sample contains.
* **No talker GPS was observed**, so its rate and even its presence on this system
  are unknown.
* **PDU payloads are not observable through the current engine API** — one packet
  on one traffic frequency proves data exists, not what it carries.
* **Nothing inbound is observable at all** (§4, last paragraph).
* **The unparsed opcodes' payloads were read from sdrtrunk's oracle log, not from
  trunkd**, because `P25Event::Tsbk` carries only the parsed message and never the
  raw 96 bits. Any implementation of §5 should either add the parsers or expose
  the raw block — the latter is one field on the event and would make future
  audits like this one self-service.
