# Capturing IQ for offline replay and tests

A recorded capture of your site lets you test a change, a gain setting or a new build without the
radio and without touching a live recorder. The same binary that records live decodes a file: the
`iqfile` backend plays it back as if it were a tuner.

> **Captures contain real voice.** An off-air capture of a public-safety or utility system holds
> clear calls of identifiable people. Keep captures private, do not commit them (`*.iq`, `*.c8`,
> `*.cf32` are ignored by git in this repository), and delete them when you no longer need them.

## What to capture

* **A wideband capture of the whole site** — the control channel and every traffic channel in one
  file — is what exercises the full recorder: control-channel hunt, grants, voice-call assembly,
  MP3 encoding and upload. Capture at the same sample rate and centre frequency your recorder uses,
  so the file replays with your real `[[device]]` settings.
* **A narrow capture of the control channel alone** (2.4 Msps is plenty) is enough for
  control-channel decode work and is a fraction of the size. Tune the LO a few hundred kHz away
  from the channel so it does not sit on the zero-IF DC spike.
* **Long enough to hold a clear call.** On a busy site a minute or two usually is; on a mostly
  encrypted system it may take longer. A capture with only encrypted calls still tests everything
  except audio.

Size is sample rate × bytes per sample: interleaved signed 8-bit I/Q at 20 Msps is 40 MB/s, so a
minute is 2.4 GB. Write to a local SSD: a slow disk drops samples during capture just as it would
during recording.

## File formats

The format is chosen from the file extension:

| extension | format | typical source |
| --- | --- | --- |
| `.iq`, `.c8`, anything else | interleaved **signed** 8-bit I/Q | `hackrf_transfer` |
| `.cf32`, `.fc32` | interleaved 32-bit little-endian float I/Q | `airspy_rx -t 0`, GNU Radio, SoapySDR tools |
| `.wav` | two-channel float WAV (I, Q) | SDR# and similar recorders |

There is no header for the raw formats, so the sample rate and centre frequency have to be
recorded separately — put them in the file name (for example
`site1_c856000k_20M_int8.iq`) and in a note beside it: date, antenna, gains, and anything unusual.

`rtl_sdr` writes **unsigned** 8-bit samples, which are not the same as signed. Convert first, for
example to `.cf32`:

```python
import numpy as np
u8 = np.fromfile("capture.u8", dtype=np.uint8)          # interleaved I, Q
iq = (u8.astype(np.float32) - 127.5) / 127.5
iq.tofile("capture.cf32")
```

## Capture commands

Stop trunkd (or any other program) on the radio first: a board can only be opened once.

**HackRF**, 60 s at 20 Msps, amp on, LNA 32 dB, VGA 30 dB:

```bash
hackrf_transfer -r site1_c856000k_20M_int8.iq \
  -f 856000000 -s 20000000 -a 1 -l 32 -g 30 -n 1200000000
```

A HackRF inside a PortaPack must be in HackRF mode first (from its screen, or let a trunkd run with
`portapack_port` switch it).

**Airspy**, 60 s at 10 Msps as float I/Q (`-f` is in MHz; `-t 0` is FLOAT32_IQ; `-l`/`-m`/`-v`
are the LNA, mixer and VGA indexes):

```bash
airspy_rx -r site1_c856000k_10M.cf32 -f 856.0 -a 10000000 -t 0 -l 12 -m 10 -v 12 -n 600000000
```

Use the same gains you record with. Check a capture before relying on it: a quick spectrum in any
SDR tool should show the control channel well above the noise floor, and the samples should not
sit at full scale (clipping).

## Replaying a capture

Point a `[[device]]` at the file:

```toml
[[device]]
id = "replay0"
backend = "iqfile"
path = "/data/captures/site1_c856000k_20M_int8.iq"
sample_rate = 20000000
center_frequency = 856000000
pacing = "unthrottled"   # or "realtime" to imitate hardware timing

[[site]]
system = "Example County"
site = "Site 1"
devices = ["replay0"]
control_frequencies = [851200000, 852400000, 853600000]

[uplink]
spool_only = true        # keep a replay from uploading anything
```

`pacing = "unthrottled"` makes playback wait for the DSP thread rather than dropping samples, so an
offline run decodes every sample and finishes as fast as the CPU allows. `realtime` (the default)
paces the file at its sample rate, which is what to use when you want the timing behaviour of live
hardware. The process exits 0 when the file runs out. Keep a replay off the live server: set
`spool_only = true`, or leave `[uplink] url` empty.

For lower-level work the `trunkd-p25` examples decode a file without the recorder around them:

```bash
cargo run --release -p trunkd-p25 --example site_decode -- \
  site1_c856000k_20M_int8.iq --rate 20000000 --format int8 \
  --center 856000000 --control 851200000 --control 852400000 --quiet
```

`channelizer_decode` and `p25_decode` in the same directory decode known channels directly; each
file's header comment lists its options.

## Using a capture in the test suite

Some tests replay a real capture end to end — `crates/trunkd/tests/fixture_pipeline.rs` runs the
whole recorder against one and a mock server, and checks calls, voice frames, overflow and uploads.
Such captures are far too large (and too personal) to ship, so these tests look for their file at
build time and are reported as **ignored**, naming the file they need, when it is absent — never as
a silent pass.

To run them against your own capture:

1. Put the capture in a directory of its own and point `TRUNKD_FIXTURES` at it (or put it in the
   repository's `fixtures/` directory; capture files there are ignored by git).
2. Give it the name the test expects (the `FIXTURE` constant at the top of the test file), or
   adjust the test's expectations to your capture; its call counts are for the capture it was
   written against.
3. Rebuild — the presence check runs in the build script — and run the tests optimised:

   ```bash
   export TRUNKD_FIXTURES=/data/captures
   cargo test -p trunkd --release --test fixture_pipeline -- --nocapture
   ```

[`fixtures/README.md`](../fixtures/README.md) describes the fixture directory and how to record a
capture for it.
