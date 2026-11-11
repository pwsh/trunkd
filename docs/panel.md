# The local control panel

trunkd runs headless, usually on a box that is not the server. `[server] enabled = true` puts a
small embedded web panel on it, so "what is this instance doing right now?" is a page rather than
an SSH session and a log tail. It is **off by default** — a listening port nobody asked for is a
liability, not a feature — and it is **read-mostly by design**: it observes everything, and the
only things it changes are restarting one tuner, holding or releasing the uplink, and the
`[[device]]` tables in `trunkd.toml`.

```toml
[server]
enabled    = true
bind       = "127.0.0.1:8080"   # 0.0.0.0:8080 to expose it; then set tokens
tokens     = []                 # bearer tokens; empty = open reads, loopback-only mutations; "" is ignored
live_audio = false              # opt-in live monitor stream; see below
# public_url = "http://192.0.2.10:8080"   # how Keyup should link to this panel
```

The default port is **8080**, on loopback. From another machine, either reach it through an SSH
tunnel (`ssh -L 8080:127.0.0.1:8080 recorder-host`) or bind it to the LAN *and* set tokens.

`public_url` is the address Keyup puts behind the **Listen live** link on its own status page.
Leave it unset and the recorder derives one from `bind`; a wildcard bind (`0.0.0.0`, `[::]`)
derives `127.0.0.1`, which is honest — the link then only works from the recorder's own browser.
Set it when the panel is bound to the LAN and should be clickable from Keyup. Either way the
recorder reports it, with the stream route and whether a token is needed, as a top-level `monitor`
object on the tuner-status POST:

```json
"monitor": {
  "enabled": true, "url": "http://192.0.2.10:8080",
  "stream_url": "http://192.0.2.10:8080/api/monitor/stream",
  "token_required": false, "streams": 0, "max_streams": 4
}
```

The object is omitted entirely when `[server] enabled` is false. `enabled` inside it is
`live_audio` — a panel with the tap switched off still reports where the panel is.

The panel is served by a `trunkd-http` thread beside the control thread, and is never
load-bearing: if the bind fails it logs an error and ends the thread while the recorder keeps
recording.

## Routes

| route | method | what it returns |
| --- | --- | --- |
| `/` | GET | The whole panel: one self-contained HTML page, no external assets (the box may have no route out). Header (instance, version, uptime, station), a card per tuner, the site table, the **Conventional channels** table (only when `[[channel]]`s are configured), the counters strip, an **Activity** section (what is on air now, with elapsed time ticking, and the last 60 finished calls with their outcome — each row with a *listen* button when the live monitor is on), supervised-unit health, and a config dump on demand. Refreshes every 2 s. |
| `/api/status` | GET | Everything the panel draws, as JSON: `instance_id`, `recorder_version`, `uptime_s`, `station`, `counters`, `tuners[]`, `supervisor[]`, `sites[]`, plus `recorder` — the recorder-wide settings in force (`spool_only`, `event_log_archive`, `link`, `broadcastify`) each with the source that last set it (`config`, `keyup` or, for the hold, `panel`). The `tuners[]` objects are built by the *same* code as the Keyup tuner-status POST (plus a `pending` flag), so the panel can never show a different truth than Keyup does. The `link` settings are **display only** here: Keyup is the editor for them. A top-level `broadcastify` object carries direct-to-Broadcastify delivery: `{enabled, allowed, revision, api_key_set, api_key_prefix, uploaded, duplicate, failed, late, claim_denied, skipped, last_response, last_at}` — counters since boot, and never the key itself. A top-level `calls` object carries the Activity view: `{"active": [...], "recent": [...]}`, each row `{id, site, kind, talkgroup, alias, source, target, frequency_hz, encrypted, started_at_ms, ended_at_ms, active, voice_frames, reason, outcome}`. `outcome` is this recorder's own disposition of the finished call (`delivered`, `no voice`, `dropped`, `no audio`). A top-level `channels` array carries the conventional channels (see [below](#conventional-channels)); it is always present and empty on a recorder with none. |
| `/api/sites/{index}/detail` | GET | One site's full profile, as the panel's expandable Sites row draws it. The body is the **observation document** that site's observations POST carries (`system`, `site`, `services`, `control`, `secondary_control`, `configured_control`, `bands[]`, `traffic[]`, `adjacent_sites[]`, `message_mix`, `unknown_exemplars`, `link_control_mix`, `data_channel`, `lsd_nonzero`) — one renderer, so the panel and the archive cannot disagree — plus `site_label`, `system_label`, `locked` and `control_locked_hz`, `recorder_sites[]` (every site this recorder decodes, so a neighbour that is one of them can be marked), `message_kinds` (`{known, unknown, noise, top_unknown[]}`) and `unknowns[]` (prose lines: neighbours this recorder does not decode, secondary control channels with no resolvable frequency, services never heard, recurring unnamed message kinds, bands referenced but never heard). 404 for an index that is not a configured site; 503 in the first seconds of a run. |
| `/api/config` | GET | The loaded config as JSON with every secret replaced by `"***"`: `uplink.api_key`, each `server.tokens` entry, `[link] key`, and any `[device.settings]` entry whose name contains `key`, `token`, `secret` or `password`. An unset secret stays `""`. Direct-to-Broadcastify credentials pushed by Keyup never appear in any panel route. |
| `/about` | GET | The About / diagnostics page (below). Served without a token for the same reason `/` is — it carries no data, it fetches `/api/about`. |
| `/api/about` | GET | Everything on that page, as JSON. |
| `/api/tuners/{tuner_key}/restart` | POST | Asks that one tuner to restart. `{"restarting": true}`, or 404 for an unknown key. |
| `/api/uplink` | GET, POST | Reads or sets the upload hold. POST `{"spool_only": true|false}`; either verb answers with the same `uplink` object `/api/status` carries. |
| `/api/monitor/stream` | GET | Live decoded voice as a chunked `audio/mpeg` stream — `?talkgroup=N` for one talkgroup, no parameter for everything this recorder hears, mixed. 404 naming the setting when `live_audio` is off, 503 past four concurrent streams. |
| `/api/monitor/status` | GET | `{enabled, streams, max_streams}` (plus `bitrate_kbps` and `dropped_frames` when it is on). |
| `/api/devices/discover` | GET | What is plugged in, what is configured, and which is which. |
| `/api/config/devices` | GET | The `[[device]]` and `[[site]]` tables **as they are on disk**, re-read per request. |
| `/api/config/devices/{id}` | PUT, DELETE | Create/edit or remove one `[[device]]`. |
| `/api/config/reload` | POST | Apply the file on disk: same process, pipeline rebuilt. |
| anything else | — | 404 JSON; a known route with the wrong method is a 405 naming the method it takes. |

Examples:

```sh
curl -s localhost:8080/api/status | jq '.tuners[] | {tuner_key, locked, snr_db, freq_error_hz}'
curl -s -X POST localhost:8080/api/tuners/sdr0/restart
curl -s -X POST localhost:8080/api/uplink -H 'Content-Type: application/json' -d '{"spool_only": false}'
```

```powershell
Invoke-RestMethod http://127.0.0.1:8080/api/status | ConvertTo-Json -Depth 4
Invoke-RestMethod -Method Post http://127.0.0.1:8080/api/uplink `
  -ContentType application/json -Body '{"spool_only": false}'
```

## Security

**Auth.** Which rule is in force is simply whether `server.tokens` is set:

| `server.tokens` | `GET /` | `GET /api/*` | `POST /api/*` |
| --- | --- | --- | --- |
| empty | served | open | loopback peers only → otherwise **403** |
| non-empty | served | `Authorization: Bearer <token>` → otherwise **401** | same bearer requirement → otherwise **401** |

The page itself is always served, because an operator who cannot even load the shell has no way to
discover that a token is what they are missing; the page has a token box (kept in that browser's
`localStorage`) that supplies the header on every API call. A read-only panel exposed on the LAN
with no token is a defensible choice; an unauthenticated restart button on the LAN is not, which is
why the loopback rule exists. An unknown peer address counts as not-loopback.

An **empty or whitespace-only token is not a token**: `tokens = [""]` is dropped at load with a
warning (the panel stays in its no-token mode), and a blank `Authorization: Bearer ` or `?token=`
never matches anything. Configured tokens are trimmed, as presented ones are.

**Origin check on mutations.** The loopback rule stops the LAN, not a web page open in a browser
*on the recorder box*: any page can `fetch()` a POST to `127.0.0.1:<port>` without a preflight, and
a DNS-rebinding page can do it under a name of its own. So every `POST`/`PUT`/`DELETE` under
`/api/` that carries an `Origin` header must come from the panel's own page, or it is refused with
**403**. Accepted origins:

* `http://localhost:<port>`, `http://127.0.0.1:<port>`, `http://[::1]:<port>` — the panel's bound
  port, not any port on localhost;
* `http://<bind host>:<port>` — and for a wildcard bind (`0.0.0.0`, `[::]`) any IP literal on that
  port, never a DNS name;
* this machine's own hostname (and `<hostname>.local`) on the panel's port;
* exactly the origin of `public_url` (scheme, host and port). **Reaching the panel by a DNS name,
  or through a reverse proxy, and using its buttons requires `public_url` to name that address.**

A request with no `Origin` at all — `curl`, scripts, the health monitor — is not a browser being
steered and is left to the token/loopback rule. Reads are not origin-checked.

**Host check on every request.** A rebinding page does not need a mutation: once its own name
resolves to `127.0.0.1` it can *read* `/api/status` and `/api/config` as if same-origin. Its `Host`
header still carries its own name, so every request must name the panel by one of the names above,
or it is refused with **421 Misdirected Request**. The port is not compared (a proxy or
port-forward may rewrite it). Reaching the panel by any other DNS name needs `public_url` set to
it.

**JSON bodies say they are JSON.** `POST /api/uplink` and `PUT /api/config/devices/{id}` take only
`Content-Type: application/json` and answer **415** otherwise — `text/plain` and form encodings are
what a cross-site page can send without a preflight. With curl add
`-H 'Content-Type: application/json'` (a bare `-d` sends a form type); with PowerShell
`-ContentType application/json`.

**Frames and connections.** Every answer carries `X-Frame-Options: DENY` and
`Content-Security-Policy: frame-ancestors 'none'`. Each connection carries one request and is
closed after the answer; at most 16 are read or answered at once (a 17th gets a 503), a request
must arrive within 10 s and an answer be written within 15 s, and each connection has its own
thread, so a client that stops reading costs its own connection and nobody else's.
`/api/monitor/stream` needs HTTP/1.1 and ends when one chunk cannot be written within 10 s.

## Conventional channels

A recorder with enabled NBFM [`[[channel]]`s](configuration.md#channel--one-conventional-analog-channel)
gets one more table, under Sites: one row per channel with its name and system, frequency, tone
(`carrier` for carrier squelch), the tuner recording it, the squelch state, SNR, calls kept (plus
how many openings were too short to keep), when the last call ended, tone mismatches and false
opens. With no channels configured the section stays hidden and the page is exactly what it was.
The figures come from the `channel.{i}.*` gauges the tuner's DSP thread publishes once a second.

`/api/status` carries the same rows as a top-level `channels` array, in config order:

```json
"channels": [
  { "index": 0, "name": "Public Works", "system": "Example County", "device": "sdr0",
    "frequency_hz": 154000000, "tone": "167.9", "talkgroup": 154000,
    "state": "open", "open": true, "snr_db": 21.5, "power_dbfs": -42.0,
    "noise_floor_dbfs": -63.5, "offset_hz": -120.0, "calls": 4, "calls_too_short": 1,
    "tone_mismatch": 0, "false_opens": 2, "last_call_end_ms": 1788499700000 }
]
```

* `state` — `closed`, `opening` (carrier up, audio held while the tone or the minimum length is
  confirmed), `open`, `hang` (carrier dropped, waiting out the hang time), or `idle` when the
  tuner is not streaming or is disabled. `open` is true for `open` and `hang`.
* `snr_db`, `power_dbfs`, `noise_floor_dbfs` — live measurements, omitted while the channel is
  `idle`. `offset_hz` is the carrier offset measured on the last call (omitted before the first);
  a steady value beyond ±500 Hz says the tuner's `ppm` wants setting.
* `calls`, `calls_too_short`, `tone_mismatch`, `false_opens` — since the tuner last started.
  `tone_mismatch` counts carrier openings dropped because the configured tone never came (another
  user of the frequency); `false_opens` counts openings vetoed as noise. A climbing
  `false_opens` usually means `open_db` is too low for the site.
* `last_call_end_ms` — unix ms, omitted before the first call.

Each tuner entry in `tuners[]` carries its own channels under the same field (without `device`),
which is how they reach Keyup ([keyup.md](keyup.md#5-tuner-status)).

## Restarting a tuner

The button bumps that tuner's desired-state *generation* and nothing else; the supervisor rebuilds
the tuner with the config it already had. So:

* it is a **clean rebuild**, not a fault — no restart budget is spent and no backoff applies;
* **no parameter changes** — the applied version does not move and Keyup's pending badge is
  untouched. A change Keyup has staged but not yet applied stays staged, and the rebuild picks it
  up exactly as a Keyup-driven apply would have;
* only that one tuner restarts. The process, the call thread and the uplink are untouched.

## The upload hold

The Uplink section shows whether uploads are held, who set that (`config` | `keyup` | `panel`),
how much is spooled, and where a released hold would send it. The button writes the same shared
flag `[uplink] spool_only` seeds and Keyup's `recorder` block pushes — **most recent writer wins**,
in either direction, and the next status POST tells Keyup who moved it. Releasing asks for
confirmation first, because it starts uploading whatever the spool has been collecting; holding
does not. The panel is the control that still works when Keyup is the unreachable thing. See
[operations.md](operations.md#bring-up-record-without-uploading).

**Runtime state is Keyup's to write.** Keyup's desired state (gain, ppm, bands, centre frequency)
is applied by restarting one tuner and the panel does not compete with it — two writers of the same
state is how a recorder ends up running something nobody can point at. What the panel *does* write
is the `[[device]]` tables in the TOML, which describe what hardware exists rather than what it is
currently doing.

## Live audio monitor

`[server] live_audio = true` (plus a restart) adds a **Live monitor** card: pick *all traffic* or
one talkgroup, press listen, and an `<audio>` element plays `GET /api/monitor/stream` while the tab
is open.

It exists for one question a finished call cannot answer in time: **is what this box is decoding
actually intelligible?** A tuner half a kilohertz off, a moved antenna, or a vocoder chewing on
marginal frames all produce calls that upload perfectly and sound like gravel. It is deliberately
*not* a scanner — no squelch, no history, no recording — and listening to calls stays in the
server's player.

What it costs, and what it refuses to cost:

- **Nothing when off.** The fan-out hub is not constructed. It is a separate switch from `enabled`
  because a status page and a live tap on voice traffic are different decisions.
- **One atomic load when on and idle.**
- **Never slows call assembly.** Each listener has a bounded ~2 s queue; a browser that stalls has
  frames *dropped* into `monitor.dropped_frames`, never waited on.
- **Never occupies the HTTP thread.** The request is moved to a thread of its own.
- **Bounded at four concurrent streams**, then 503.

Behaviour worth knowing before you use it to judge audio:

- Several calls at once (in *all traffic* mode) are **mixed** — summed with saturation, not
  averaged.
- Idle time is real silence frames, not a stalled connection, which keeps the browser's decoder
  alive so the *next* call is not the one that gets lost.
- Expect a few seconds of latency: it is a buffered MP3 stream (32 kbps mono, 8 kHz).
- **Encrypted calls are silent**, for the same reason they upload with no audio.
- Pressing *stop* clears the player's source, which closes the connection and frees the slot;
  simply pausing does not.

**Auth for the stream.** A browser `<audio>` element cannot be given an `Authorization` header, so
this one route also accepts the token as `?token=<token>`. Query strings show up in proxy logs in a
way headers do not, which is one more reason to leave `bind` on loopback and reach the panel
through an SSH tunnel.

## Setting up devices from the panel

The panel's **Devices** section lists the boards actually attached, writes `trunkd.toml` for you,
and applies the result without the process going away.

**Discover** (`GET /api/devices/discover`) asks every compiled-in backend to enumerate. That is a
*listing*, not an open — SoapySDR's `enumerateStrArgs` asks each driver module what it can see and
never touches a device — so it is safe while the recorder is recording on those same boards. For
each board you get the driver (`airspy`, `sdrplay`, `rtlsdr`, `hackrf`), the serial when the
backend reports one, the driver's label, which configured `[[device]]` it is (if any), whether
that device is running, and a **suggested** starting config for the driver (sample rate, gain
elements and settings). The same body lists the configured devices with `present` and the sites
each one serves. Matching a configured device to a board goes by serial first
(case-insensitively), then by an exact `soapy` args string, and only then by driver, and only when
that is unambiguous: with two identical boards and no serials in the file, both are reported
absent rather than guessed at. A board another program holds open is not listed
([sdr-hardware.md](sdr-hardware.md#only-free-radios-enumerate)).

**Saving** (`PUT`/`DELETE` on `/api/config/devices/{id}`) writes `trunkd.toml`:

* The file is edited **in place**, not regenerated. Every comment, every blank line, your key order
  and your commented-out sites survive; only the `[[device]]` table being edited and any
  `[[site]] devices` list changing membership are touched. `[uplink] api_key`, `[link] key` and
  `[server] tokens` are never re-serialized — the panel never even holds the real ones. A
  `[device.settings]` entry with a secret-shaped name is listed as `"***"`; sending `"***"` back
  keeps the value on disk, and `"***"` for a setting with no value on disk is a 400.
* The previous version is kept as **`trunkd.toml.bak`** (one generation). The new file is written
  to `trunkd.toml.tmp` and renamed into place.
* The file's **permissions are kept**: on Unix the temp file is created with the current file's
  mode (so a `0600` config stays `0600`, and so does its `.bak`), or `0600` when there was no file.
  On Windows the file inherits its directory's ACL.
* Nothing is written until the edited document has been **loaded and validated** by exactly the
  code path startup uses. A sample rate the channelizer cannot drive, or a centre frequency that
  puts a site's control channel outside the capture, comes back as a 422 with the message startup
  would have printed — and the file on disk is untouched.
* A `PUT` **merges**: fields you do not send keep whatever the file says, and a field sent as `null`
  is removed. `sites`, when present, is the complete membership list for that device.
* A `DELETE` is **refused with 409** while any `[[site]]` or `[[channel]]` still names the device,
  and says which.
* A site still using the scalar `device = "x"` is migrated to `devices = ["x"]` the first time its
  membership changes.

A save does **not** change what is recording. It sets `restart_pending`, which `/api/status` and
the discover body both report and the page shows as a banner.

**Apply** (`POST /api/config/reload`) is what makes it real. The file is loaded and validated
first — a broken file is a 422 and the recorder keeps running. Then the run ends through the
ordinary teardown (devices finish their open calls, the call thread drains, the uplink drains) and
the **same process** builds the whole pipeline again from the new file. Same PID, same log file,
same panel port. Expect a **few seconds with no reception** while the tuners are rebuilt, and a
short window in which the panel port is not listening; `trunkd monitor` tolerates that.

Why a reload rather than adding the device live: every per-device array in the running recorder is
sized at boot. **Changes Keyup can push — gain, ppm, bands, centre frequency — remain live and need
none of this**; only the shape of the device table is a reload.

If the recorder was configured from the environment rather than a file (`TRUNKD_*` with no
`--config`), there is nothing to write: the discover body reports `"writable": false` and every
write route answers **503**.

| route | method | body | answers |
| --- | --- | --- | --- |
| `/api/devices/discover` | GET | — | `config_path`, `writable`, `restart_pending`, `hardware[]` (`key`, `backend`, `driver`, `serial`, `label`, `args`, `configured_id`, `running`, `suggested`), `configured[]` (`id`, `present`, `running`, `sites[]`) |
| `/api/config/devices` | GET | — | `{"devices": [...], "sites": [...]}` from the file on disk |
| `/api/config/devices/{id}` | PUT | device fields by their TOML names, optional `sites[]` | `{"ok": true, "restart_required": true, "id": ...}` |
| `/api/config/devices/{id}` | DELETE | — | `{"ok": true, "restart_required": true}` |
| `/api/config/reload` | POST | — | `{"ok": true}`, then the pipeline rebuilds |

Errors are `{"error": "<message>"}` with **400** (bad JSON or a field that is not a `[[device]]`
field), **404** (no such device, or a site name the file does not have), **409** (delete refused;
`referenced_by` lists the sites), **422** (the result would not load — the message is the
validator's, verbatim) and **503** (no config file to write). Every one of them leaves the file
exactly as it was.

## About / diagnostics (`/about`, `/api/about`)

Three questions come up every time a recorder misbehaves: *which build is this*, *which SoapySDR
did it actually load*, and *what is it really configured with*. `/about` answers all three from
the running process. Include its "copy as JSON" output in bug reports.

`GET /api/about` returns a stable JSON document. `schema_version` is bumped when a field changes
meaning or disappears; **adding** a field does not bump it.

```jsonc
{
  "schema_version": 1,

  // Stamped in at compile time by crates/trunkd/build.rs.
  "build": {
    "version": "0.0.1",                  // crate version
    "git_sha": "3f2c9a1b7d40",           // short sha, or "unknown" with no git
    "git_describe": "3f2c9a1-dirty",
    "git_dirty": true,                   // true | false | "unknown" (no git to ask: tarball build)
    "built_at": "2026-01-15T19:12:28Z",  // RFC 3339 UTC; honours SOURCE_DATE_EPOCH
    "built_on": "build-host",            // machine that ran the build
    "rustc": "rustc 1.98.0 (…)",
    "target": "x86_64-unknown-linux-gnu",
    "host_triple": "x86_64-unknown-linux-gnu",
    "profile": "release", "opt_level": "3",
    "features": ["default", "hackrf", "soapy"],
    "debug_assertions": false
  },

  // The workspace Cargo.lock, parsed at build time and baked in.
  "dependencies": {
    "count": 214, "workspace_count": 9,
    "packages": [
      { "name": "serde", "version": "1.0.219", "source": "crates.io", "workspace": false },
      { "name": "trunkd-core", "version": "0.0.1", "source": "path", "workspace": true }
      // a git dependency reads "git <url> @ <locked commit>"
    ]
  },

  // Native libraries, asked their versions on the first request and cached.
  // Nothing here opens a device: version getters and a module listing only.
  "libraries": {
    "soapysdr": {
      "available": true,
      "library": "libSoapySDR.so.0.8",             // the candidate dlopen accepted
      "resolved_path": "/usr/lib/.../libSoapySDR.so.0.8.1",  // Linux: from /proc/self/maps
      "api_version": "0.8.1", "lib_version": "0.8.1", "abi_version": "0.8",
      "root_path": "/usr",
      "module_count": 4,
      "modules": [ { "path": ".../libairspySupport.so", "version": "0.2.0" } ]
    },
    "libhackrf":   { "available": false, "reason": "libhackrf could not be loaded; ..." },
    "mp3_encoder": { "available": true, "name": "libmp3lame", "version": "3.100",
                     "linkage": "static" },
    "rust_std":    { "os": "linux", "arch": "x86_64", "family": "unix",
                     "target": "x86_64-unknown-linux-gnu", "pointer_width": 64 }
  },

  // Recomputed per request.
  "process": {
    "hostname": "rec-01",
    "os": "Debian GNU/Linux 12 (bookworm)", "os_version": "12", "kernel": "6.1.0-18-amd64",
    "arch": "x86_64", "cpus": 4, "pid": 1462,
    "started_at": "2026-01-15T04:10:02Z", "uptime_s": 226946,
    "executable": "/usr/local/bin/trunkd", "working_dir": "/etc/trunkd",
    "launched_by": "systemd",              // "systemd" | "windows-service" | "foreground"
    "directories": [
      { "label": "spool", "path": "spool", "exists": true,
        "free_bytes": 812345678, "total_bytes": 500107862016 }
      // labels: spool, event_log, ppm_memory, log — whichever are configured
    ]
  },

  // The effective config, with secrets removed. See below.
  "config": {
    "path": "/etc/trunkd/trunkd.toml",     // null for an environment-only run
    "redacted": true,
    "keys_from_file": ["uplink.url", "device.id", "..."],   // dotted paths the file set
    "env_overrides": ["TRUNKD_UPLINK__API_KEY"],            // names only, never values
    "json": { "...": "the whole Config, redacted" },
    "toml": "# the same document rendered as TOML text\n"
  }
}
```

**Redaction.** A field whose *name* contains `key`, `token`, `secret` or `password`
(case-insensitively) has its **value** replaced with `"<redacted: set, N chars>"` or
`"<redacted: unset>"` — the name is always kept, and the length is reported because "is the key
set, and is it the short one or the long one" is most of what you are trying to find out. It
recurses through nested tables, arrays (`server.tokens` keeps its element count) and free-form
string maps, so a `[device.settings]` entry called `api_token` is covered too. Matching is on names
and **never** on values, which is what keeps `uplink.url = "https://keyup.example.org/..."`
readable: `keyup` contains "key", but `url` does not. A name that looks like an address (`url`,
`uri`, `endpoint`, `host`) is never treated as a credential. A test asserts that none of the secret
values appear in either the JSON or the TOML rendering while every other value does.

Both `/api/about` and `/api/config` answer to the same auth rule as every other `/api/` route.
