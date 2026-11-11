# trunkd

trunkd is a P25 Phase 1 trunked-radio recorder written in Rust. It takes a
wideband capture from one or more SDRs, follows a site's control channel,
records every voice call it is granted, decodes the IMBE voice, and uploads
each finished call as an MP3 to a server. It also records analog NBFM
conventional channels from the same capture. It runs unattended as a systemd
unit on Linux or as a Windows service, as one binary with no runtime to
install.

It covers the recording side of [sdrtrunk](https://github.com/DSheirer/sdrtrunk):
the P25 receive chain follows sdrtrunk, and the voice decoder is a
sample-exact port of [JMBE](https://github.com/DSheirer/jmbe). Storing,
searching, transcribing and publishing calls is the server's job, not
trunkd's.

**Full guide: <https://pwsh.github.io/radio-docs/>**

## What it does

- P25 Phase 1 control and traffic channels, C4FM and LSM (CQPSK simulcast).
- IMBE voice. Encrypted calls are recorded as metadata only and never decoded.
- Analog NBFM conventional channels, cut from the same capture as the P25
  sites (or from a tuner of their own), with CTCSS or DCS tone squelch or plain
  carrier squelch. Each transmission uploads as a group call under the
  channel's system and talkgroup.
- Several sites and several tuners per recorder. A polyphase channelizer cuts
  each capture into 12.5 kHz channels, so one tuner covers every channel of a
  site that fits in its bandwidth.
- An offline spool: calls queue on disk while the server is unreachable.
- A local web panel for status, tuner restarts, live audio and device setup.
- An authenticated UDP link that lets two recorders share a site.

Not built yet: P25 Phase 2 (TDMA), AMBE, AM channels, DMR, NXDN.

## Hardware

| Radio | Backend | Notes |
| --- | --- | --- |
| Airspy R2 / Mini | `soapy` | 10 MHz (R2) covers most single-band 800 MHz sites with one tuner. |
| SDRplay RSP | `soapy` | Needs SDRplay's API service. |
| RTL-SDR | `soapy` | 2.4 MHz each, so a site usually needs several. |
| HackRF One (and PortaPack) | `hackrf` | 20 MHz, driven through `hackrf_transfer`. |

SoapySDR is loaded at run time, so building trunkd needs none of the SDR
libraries. Choosing gains, sample rates and a host is covered in
[docs/sdr-hardware.md](docs/sdr-hardware.md).

## Where calls go

- **Plain call upload works with any Rdio-Scanner-compatible server.** Each
  call is one `POST /api/call-upload` in the Rdio Scanner (trunk-recorder)
  format.
- **Everything else needs [Keyup](https://github.com/pwsh/keyup).** Keyup is
  the companion archive server. Site observations, radio events, tuner status,
  remote configuration, skip lists and Broadcastify Calls delivery all go
  through Keyup's routes. Other servers answer those routes with a 404, which
  trunkd ignores. Recording is not affected.
- **No server at all.** With `[uplink] spool_only = true`, calls stay on disk.

[docs/keyup.md](docs/keyup.md) lists everything trunkd sends and accepts.

## Quickstart (Linux, from source)

```sh
sudo apt install build-essential git curl        # a C compiler builds the vendored LAME
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
git clone https://github.com/pwsh/trunkd.git && cd trunkd
cargo build --release                            # rust-toolchain.toml picks the toolchain
cp trunkd.example.toml trunkd.toml && chmod 600 trunkd.toml
$EDITOR trunkd.toml                              # your SDR, site control channels, [uplink] url + token
./target/release/trunkd --config trunkd.toml check   # validates the config and lists the SDRs it sees
./target/release/trunkd --config trunkd.toml run
```

Release builds for Linux x86-64 and Windows x64 are attached to each
[GitHub release](https://github.com/pwsh/trunkd/releases). To run trunkd as a
service, install the SDR runtime and set up real-time priority, follow
[docs/install-linux.md](docs/install-linux.md) or
[docs/install-windows.md](docs/install-windows.md).

## Documentation

The guide at <https://pwsh.github.io/radio-docs/> walks through installing
trunkd and Keyup, connecting the two, and configuring a system from
RadioReference or an sdrtrunk playlist. The reference pages live in this
repository:

| Page | Covers |
| --- | --- |
| [install-linux.md](docs/install-linux.md) | Build, install, systemd unit ([`scripts/systemd/`](scripts/systemd/)), real-time priority |
| [install-windows.md](docs/install-windows.md), [windows-service.md](docs/windows-service.md) | Windows build, SoapySDR runtime, the Windows service, updates |
| [configuration.md](docs/configuration.md) | Every `trunkd.toml` table; [`trunkd.example.toml`](trunkd.example.toml) is a commented example |
| [sdr-hardware.md](docs/sdr-hardware.md) | Radios, gains, frequency correction, host sizing |
| [keyup.md](docs/keyup.md) | The call upload, the side channels, and the Keyup control plane |
| [panel.md](docs/panel.md) | The local control panel and its HTTP routes |
| [recorder-link.md](docs/recorder-link.md) | Sharing a site between two recorders |
| [operations.md](docs/operations.md) | Logs, the spool, clocks, the raw event log, the health monitor |
| [capturing-test-iq.md](docs/capturing-test-iq.md) | Recording an IQ capture to replay and test against |
| [ARCHITECTURE.md](docs/ARCHITECTURE.md) | How the crates fit together |
| [P25P1-SPEC.md](docs/P25P1-SPEC.md), [SPEC-GAPS.md](docs/SPEC-GAPS.md), [NON-VOICE-DATA.md](docs/NON-VOICE-DATA.md) | Protocol notes |

`trunkd import-sdrtrunk <playlist.xml>` prints a starting `trunkd.toml` from
an sdrtrunk playlist.

## Building and testing

```sh
cargo build --release
cargo test --workspace
```

The tests that replay a recorded off-air IQ capture, and the IMBE tests that
compare against a clear-voice JMBE vector, need data that is not in this
repository. Without it they are reported as **ignored**, with the reason, not
as passed. [docs/capturing-test-iq.md](docs/capturing-test-iq.md) and
[`crates/trunkd-mbe/tests/data/README.md`](crates/trunkd-mbe/tests/data/README.md)
explain how to supply them.

CI ([`.github/workflows/ci.yml`](.github/workflows/ci.yml)) runs `cargo fmt`,
`cargo clippy -D warnings`, the workspace tests and `cargo deny` on Linux, and
a release build plus the library tests on Windows. Run the same checks before
opening a pull request: see [CONTRIBUTING.md](CONTRIBUTING.md).

## License

Copyright © 2026 Eric Holzhueter.

trunkd is licensed under the GNU General Public License, version 3 or later
([LICENSE](LICENSE)). It contains code ported from JMBE, sdrtrunk and
JTransforms; their notices, and the note on the expired IMBE patents, are in
[NOTICE.md](NOTICE.md). Security reports: [SECURITY.md](SECURITY.md).

You are responsible for knowing whether receiving, recording or publishing
radio traffic is legal where you are.

<sub>Source revision: `c72f95e54978`</sub>
