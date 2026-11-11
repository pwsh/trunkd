# Changelog

All notable changes to trunkd are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/). Until 1.0, minor versions may
change configuration or behaviour; the entry says how.

## [0.1.0] - unreleased

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
