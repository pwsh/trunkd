# Changelog

All notable changes to trunkd are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/). Until 1.0, minor versions may
change configuration or behaviour; the entry says how.

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
