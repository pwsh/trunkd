# Changelog

All notable changes to trunkd are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/). Until 1.0, minor versions may
change configuration or behaviour; the entry says how.

## [Unreleased]

## [0.3.1] - 2026-11-24

- Documentation: AM channels are recorded (the example config, the start-up checks and the
  architecture status still said AM was skipped).

## [0.3.0] - 2026-11-24

- `scripts/linux/trunkd-watch.sh` with a two-minute timer: ntfy alerts, sent once when a
  problem appears and once when it clears, for disk space, services, failed oneshots, clock
  sync, and through the panel P25 tuner lock, device units, sample overflow and upload
  backlog. It also covers Proxmox storage, thin pools and containers.
- `update-trunkd.sh` verifies lock on P25 site tuners only; a tuner that carries analog
  channels alone has nothing to lock, so an update on such a box no longer rolls back.
- `scripts/linux/update-trunkd.sh` with a systemd service and daily timer: unattended
  updates on Linux, from the latest published GitHub release (checksum-verified) or a git ref
  built locally. Triggered by a version bump or by every new commit. Each run pre-flights the
  live config with the new binary, verifies that the site tuners lock after the restart, and
  rolls back if they do not. See `scripts/linux/README.md`.
- docs/install-linux.md: reaching the panel on a headless box or container.
- Conventional AM channels (`[[channel]] mode = "am"`, e.g. airport towers and CTAFs in
  the 118–137 MHz airband) are recorded instead of skipped: envelope detector with a
  carrier-tracking AGC, same squelch and call segmentation as NBFM, no tone gate. Uploads
  carry `mode=am`, `decoder=AM` and no `peakDeviationHz`. `trunkd import-sdrtrunk` now gives
  AM channels a tuner that hears them, like NBFM.
- `[server] auth` chooses which panel requests need a token: `changes` keeps reads open
  and requires the token for buttons only, `none` opens buttons to the network, `loopback`
  and `token` name the two existing rules. The default, `auto`, is the previous behaviour.

## [0.2.2] - 2026-11-11

- Direct Broadcastify Calls upload no longer sends conventional `[[channel]]`
  calls: Broadcastify Calls accepts trunked-system calls only, and in the
  default every-group-call mode the analog calls went up too. They are now
  always `skipped` (`conventional channel (not a trunked call)`), whatever the
  talkgroup list says, counted as `broadcastify.skipped_conventional` (panel:
  `skipped_conventional`) and logged once per channel. P25 uploads are
  unchanged.
- Release archives include the systemd unit and its README (Linux) and an SDR
  runtime note (Windows). Public documentation edited for outside readers.

## [0.2.1] - 2026-11-11

- Tests: temp-directory paths in test configs are TOML literal strings, so the
  Windows test run (and the release build) no longer fails on backslashes.

## [0.2.0] - 2026-11-11

- Analog NBFM conventional channels (`[[channel]]`): cut from a tuner's
  capture by the same channelizer as the P25 sites, so a tuner can carry a
  site and channels or channels alone. CTCSS (50 tones) and DCS (104 codes,
  either polarity) tone squelch or carrier squelch, a dB power squelch with
  attack/hang timing, kerchunk filtering and long-call splitting. Calls upload
  as group calls with `mode`, `decoder`, `tone`, `toneDetected`,
  `squelchReason` and signal measurements; `trunkd import-sdrtrunk` carries a
  playlist's NBFM channels across. AM channels are parsed and skipped with a
  warning.
- Replaying several IQ files in step no longer lets the first reader run ahead
  before its peers join.

## [0.1.0] - 2026-11-11

First public release.

- P25 Phase 1 recorder: C4FM and LSM (CQPSK simulcast) demodulation, control
  channel following, voice channel recording, and IMBE decoding through a
  sample-exact Rust port of JMBE.
- One wideband SDR per site (HackRF through `hackrf_transfer`; Airspy, RTL-SDR
  and others through SoapySDR) with several sites or tuners per recorder.
- Calls upload as MP3 to Keyup, or to any server that accepts the
  Rdio-Scanner multipart upload, with an offline spool. Optional direct
  Broadcastify Calls upload, off by default and switched from Keyup.
- Local control panel, health monitor, and an authenticated UDP link between
  recorders.
- Linux and Windows builds. Licensed GPL-3.0-or-later; see `NOTICE.md`.
