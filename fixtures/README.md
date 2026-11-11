# Capturing your own test IQ

The full guide is [docs/capturing-test-iq.md](../docs/capturing-test-iq.md); this file
covers what this directory is for.

IQ captures are large (a minute of 20 Msps is 2.4 GB) and are never committed: `*.iq` is
gitignored here and at the repository root. This directory is where trunkd's tests and
your own offline replays look for them.

## What uses a capture

* **The capture-replay tests** (`crates/trunkd-p25/tests/site_fixture.rs`,
  `crates/trunkd/tests/fixture_pipeline.rs`) replay one specific wideband recording and
  check the decode against what is known to be in it (control channel, band plan, the
  calls it carries). They are oracle tests: without that exact file they are reported as
  **ignored**, never as a pass, and the ignore message names the file they need. A
  capture of a different system cannot satisfy them, because the expected values are
  the contents of that recording.
* **Offline replay** with the `iqfile` backend (see `trunkd.example.toml`), which runs the
  whole recorder against a file instead of a radio. This works with any capture of any
  P25 Phase 1 system, and is the useful thing to do with your own.

Search order for both: `$TRUNKD_FIXTURES`, then this directory. A git worktree has no
captures of its own; point `TRUNKD_FIXTURES` at the directory that holds them, then
rebuild (the build scripts decide which tests are ignored). `cargo t` (the workspace
alias for `cargo test`) builds the DSP crates optimized, so the replays run in a normal
test pass.

## Format

The `iqfile` backend picks the format from the extension: `.iq`/`.c8` are interleaved
**signed 8-bit** I/Q (what `hackrf_transfer -r` writes), `.cf32` is 32-bit float, and
`.wav` is two-channel float; [docs/capturing-test-iq.md](../docs/capturing-test-iq.md)
has the full table and how to convert `rtl_sdr` output. Name the file after what is in
it, for example `example_20M_int8.iq`: the centre frequency, the sample rate and the
format are what you will need to know later, and nothing in the file records them.

## Capturing with a HackRF

Pick a centre frequency that puts every channel you care about inside the capture, with
the control channel well away from 0 Hz offset (the DC/LO-leakage spike sits exactly at
the centre). A 20 Msps capture spans ±10 MHz, enough for a typical 800 MHz site;
`sample_rate / 12500` must be an even integer (trunkd's channel bins are 12.5 kHz).

```sh
# 60 s at 20 Msps, centred on 855.0 MHz; amp on, LNA 40 dB, VGA 40 dB.
hackrf_transfer -r example_20M_int8.iq \
  -f 855000000 -s 20000000 -a 1 -l 40 -g 40 -n 1200000000
```

* `-n` is the number of complex samples: seconds × sample rate.
* Gain: aim for peaks well short of ±127 with no clipping; start from the values in
  `trunkd.example.toml` and adjust. A capture 10+ dB too quiet decodes the control
  channel and drops voice.
* Write to a fast local disk. 20 Msps is 40 MB/s; a slow disk or a USB hub shared with
  the radio drops samples, and `hackrf_transfer` reports it.
* A PortaPack must be switched into HackRF mode first (from its UI, or by letting trunkd
  do it via `[[device]] portapack_port`, then stopping trunkd).
* A narrow capture of the control channel alone (2.4 Msps, tuned a few hundred kHz to
  one side of it to keep the carrier off the DC spike) is enough for signalling-only
  replays and is a tenth of the size.

## Replaying it

```toml
[[device]]
id = "replay0"
backend = "iqfile"
path = "fixtures/example_20M_int8.iq"
sample_rate = 20000000
center_frequency = 855000000
pacing = "realtime"   # or "unthrottled" to run as fast as the CPU allows
```

Point a `[[site]]` at `device = "replay0"` with your system's control frequencies, set
`[uplink] enabled = false` (or `spool_only = true`) so nothing is uploaded, and run
`trunkd --config <file>`.

## Keep captures private

A capture of a live system contains everything that was on the air while it ran: unit
ids, talker aliases, locations and clear voice. Do not publish one without listening to
it and checking what it carries.
