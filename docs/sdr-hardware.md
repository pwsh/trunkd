# SDR hardware

trunkd decodes a whole P25 site from one wideband capture: a polyphase channelizer cuts the
capture into 12.5 kHz bins, one bin follows the control channel and the others follow whichever
traffic channels are granted. So the radio's job is to cover the site's frequencies, and the
host's job is to keep up with the sample rate. This page covers which radios work, how to install
their runtimes on Linux and Windows, how to set gains and frequency correction, and how to size the
host.

Two backends drive real hardware:

* **`soapy`** (the default) reaches every radio SoapySDR has a driver module for: Airspy, SDRplay,
  RTL-SDR and more.
* **`hackrf`** drives a HackRF One (optionally inside a PortaPack) through the stock
  `hackrf_transfer` tool.

A third, **`iqfile`**, plays back a recorded capture; see [capturing-test-iq.md](capturing-test-iq.md).

## Choosing a radio

| Radio | Use | Not | Notes |
| --- | --- | --- | --- |
| Airspy R2 | 10e6, or 2.5e6 | — | 10 MHz covers most single-band 800 MHz sites in one tuner. |
| Airspy Mini | 6e6, or 3e6 | — | |
| SDRplay RSPdx | 2e6–10e6; 8e6 with `bandwidth_hz = 8000000` | — | Proprietary API; see below. |
| RTL-SDR | 2.4e6 | 3.2e6 (drops samples on USB 2), 2.56e6 (not divisible by 12500) | Needs a measured `ppm`. |
| HackRF One | 20e6 | — | 8-bit; widest single capture. |

An RTL-SDR's 2.4 MHz covers roughly a quarter of a 9 MHz P25 site, so a site normally needs
**several dongles**, each a `[[device]]` with its own `center_frequency` and a `bands` list
partitioning the site's downlink between them ([configuration.md](configuration.md#bands--what-a-tuner-is-responsible-for)).

**Antennas.** Use an antenna resonant in the site's band (for 800 MHz systems, an 800/900 MHz
collinear or a quarter-wave ground plane cut for ~855 MHz) rather than a wideband discone where
you can; a bandpass filter in front of the radio helps a lot near strong pager or cellular
transmitters. Keep the coax short and low-loss. Record what you used in the `[[device]]`
description fields ([configuration.md](configuration.md#station-self-description)).

### The sample-rate rule

The channelizer divides the capture into 12.5 kHz bins, so `sample_rate / 12500` must be an
**even integer of at least 4**, and the hardware must support the rate **exactly**. Soapy drivers
routinely coerce an unsupported rate to a nearby supported one and report success — which would
silently move every recorded channel — so trunkd reads the rate back after setting it and refuses
to open if it moved by more than 1 Hz, naming the device's supported ranges in the error. The rates
in the table above all pass.

Every enabled site's control frequencies must also fall inside `center_frequency ± sample_rate/2`
less a 1 % guard band at each edge; a frequency outside it is a startup error.

## SoapySDR (Airspy, SDRplay, RTL-SDR)

trunkd binds the SoapySDR C ABI by hand and `dlopen`s it on first use, so **nothing is needed to
build** — a trunkd binary from any machine works. What a *running* recorder needs is the SoapySDR
runtime plus the module for the specific radio.

Configuration lives in `[[device]]`: `driver` (`airspy`, `sdrplay`, `rtlsdr`, ...) with an optional
`serial`, or a verbatim `soapy = "..."` args string; then `bandwidth_hz`, `rx_port`, `agc`,
`[device.gains]` and `[device.settings]`. [`trunkd.example.toml`](../trunkd.example.toml) carries a
worked block for each of the three radios.

trunkd binds the **SoapySDR 0.8** C ABI. 0.7 changed the shape of `setupStream`, so a 0.7 library
would load and then corrupt memory; loading checks `SoapySDR_getAPIVersion` and refuses anything
older, naming the version it found. Every current distribution ships 0.8.

### Linux runtime

Debian/Ubuntu:

```bash
sudo apt install soapysdr-tools soapysdr-module-airspy soapysdr-module-rtlsdr
# udev rules so the recorder need not run as root:
sudo apt install libairspy-dev rtl-sdr        # both ship /lib/udev/rules.d entries
sudo udevadm control --reload-rules && sudo udevadm trigger
```

Then add the account that runs trunkd to the group the rules grant access to (`plugdev` on
Debian-family systems). See also [`scripts/udev/README.md`](../scripts/udev/README.md).

**SDRplay is the awkward one.** There is no packaged module: install the proprietary `sdrplay_api`
from <https://www.sdrplay.com/downloads/> (a shell-script installer that also registers the
`sdrplay_apiService` daemon — the driver talks to that daemon, so check `systemctl status sdrplay`
before blaming trunkd), then build SoapySDRPlay3 from source:

```bash
git clone https://github.com/pothosware/SoapySDRPlay3
cmake -S SoapySDRPlay3 -B SoapySDRPlay3/build && cmake --build SoapySDRPlay3/build
sudo cmake --install SoapySDRPlay3/build
```

**A SoapySDR somewhere unusual** (built from source into `~/.local` or `/opt`, for example): point
trunkd at the library directly with `TRUNKD_SOAPYSDR_LIB=/opt/soapy/lib/libSoapySDR.so.0.8`, and
if the modules are not found either, set `SOAPY_SDR_PLUGIN_PATH` to their directory. Nothing about
this affects the build.

### Windows runtime

There are two ways to get a SoapySDR runtime on Windows:

* **The self-contained bundle (recommended for a service).** `scripts\windows\build-redist.ps1`
  builds SoapySDR, libairspy and the Airspy and SDRplay modules from pinned sources into one folder
  that sits beside `trunkd.exe`. trunkd finds `SoapySDR.dll` beside itself and points
  `SOAPY_SDR_PLUGIN_PATH` at its `SoapySDR\modules0.8\` folder (an operator's own value is never
  overridden). See [install-windows.md](install-windows.md#2-the-soapysdr-runtime).
* **PothosSDR.** `winget install Pothosware.PothosSDR` or the installer from
  <https://downloads.myriadrf.org/builds/PothosSDR/>. It ships `SoapySDR.dll` plus the airspy,
  rtlsdr and sdrplay modules. Its `bin` directory — `C:\Program Files\PothosSDR\bin` for the winget
  install, `C:\PothosSDR\bin` for the manual one — must be on `PATH`; neither route adds it for you.
  Verify with `where SoapySDR.dll`. The PothosSDR bundle dates from 2021: its SDRplay module targets
  the old API 3.07 and does not work with current SDRplay API releases (use the build script, or
  build SoapySDRPlay3 0.5.2 yourself against the installed API), and its libusb is the version the
  Airspy note below requires.

Then:

1. **SDRplay** additionally needs the SDRplay API installer from sdrplay.com, same as on Linux.
2. **Zadig** (<https://zadig.akeo.ie/>): bind the **WinUSB** driver to the Airspy and to the
   RTL-SDR's "Bulk-In, Interface (Interface 0)". Do **not** do this for an SDRplay — it uses its
   own vendor driver and Zadig will break it. A box that already runs sdrtrunk or SDR# on the
   Airspy has this done (Device Manager shows the Airspy's driver service as `WINUSB`).
3. If `trunkd check` reports the library but enumerates nothing, the modules were not found: set
   `SOAPY_SDR_PLUGIN_PATH` to the `lib\SoapySDR\modules0.8` directory under the install root.
4. `SoapyVOLKConverters: no VOLK config file found` on every start is PothosSDR's harmless nag;
   `volk_profile` silences it.

**Airspy on Windows: keep libusb at 1.0.24.** libairspy 1.0.10's `airspy_close()` calls
`libusb_close()` before freeing its transfers, and newer libusb releases touch the device from
inside `libusb_free_transfer()`. The process then dies with an access violation (`0xC0000005`) the
first time a stream is torn down *cleanly* — as a service that shows up as `WIN32_EXIT_CODE 1067`
and a log that stops part-way through teardown. airspy-tools' own `airspy_rx.exe` faults the same
way with the same pair, so it is not something trunkd can order its way out of. Measured with that
tool (five runs each): 1.0.30 faulted 5/5, 1.0.25 3/5, 1.0.24 0/5. `build-redist.ps1` pins 1.0.24
for this reason; PothosSDR ships 1.0.24 as well. Swapping only `libusb-1.0.dll` is not enough:
`airspy.dll` links against the matching import library, so libairspy must be rebuilt with it.

### Only free radios enumerate

libairspy has to open each board to read its serial, and the SDRplay service hides a device another
process holds, so a tuner that sdrtrunk, SDR# or any other program has open is simply absent from
`trunkd check`'s list (and the panel's Discover) rather than reported busy. Stop the other program
first, or pick a different board.

### Verifying

```bash
trunkd --config /etc/trunkd/trunkd.toml check
# SoapySDR: libSoapySDR.so.0.8 (SoapySDR 0.8.1)
#
# SDR devices available on this host:
#   [soapy:airspy] Airspy R2 (driver=airspy,label=Airspy R2 [...],serial=...)
```

An `unavailable` line means the library itself is missing (the message names what to install); a
loaded library with an empty device list means the *module* for your radio is missing, or
udev/Zadig has not given you access to it.

Then start the recorder and read the one `opened SoapySDR device` line at `info`. It carries the
driver and hardware keys, the sample rate and bandwidth the device actually settled on, the
antenna/port names, and **every gain element with its range** — which is where the names for
`[device.gains]` come from. The panel's Discover also suggests a starting config per driver.

## HackRF One

```toml
[[device]]
id = "hackrf0"
backend = "hackrf"
sample_rate = 20000000
center_frequency = 856000000      # covers 846–866 MHz
lna_db = 32                       # 0..40 in 8 dB steps
vga_db = 30                       # 0..62 in 2 dB steps
amp = true                        # front-end +14 dB
# serial = "0000000000000000deadbeef00000001"
```

trunkd runs `hackrf_transfer -r -` and reads its stdout, so the HackRF host tools must be on
`PATH`: `hackrf` on Debian/Ubuntu (which also installs the udev rule), or `hackrf_transfer.exe`,
`hackrf_info.exe` and `hackrf.dll` from the official HackRF release zip on Windows, with the board
bound to WinUSB by Zadig. 20 Msps of 8-bit I/Q is 40 MB/s over USB 2.0: give the HackRF its own
USB controller if you can.

`hackrf_transfer` takes its frequency and gains only on its command line, so every gain change is a
restart of that child (a short gap), and the closed-loop frequency correction is switched off on
this backend (`backend has no runtime retune; auto_ppm off` in the log). Set `ppm` by hand if the
board needs it, or use an external 10 MHz reference.

**PortaPack.** A HackRF inside a PortaPack running Mayhem firmware boots into the PortaPack UI. Set
`portapack_port` to its USB console (`/dev/ttyACM0` on Linux, the `COMn` port Device Manager shows
on Windows) and trunkd switches the board into plain HackRF mode before the tuner opens.
`restore_portapack_on_exit = true` puts it back into the UI on a clean stop. Without
`portapack_port`, switch the board to HackRF mode from its screen before starting.

## Gain guidance

Start from the values below, then watch `rssi_dbfs`, `snr_db` and `tsbk_pass_pct` on the panel
([keyup.md](keyup.md#5-tuner-status) explains each). More gain is not better once the noise floor
rises with it.

- **Airspy** — three elements, `LNA`, `MIX`, `VGA`, each an index **0–15** (not dB). 12/10/12 is a
  reasonable 700/800 MHz starting point; back `LNA` off first if a strong pager or cellular site
  nearby makes the noise floor jump when you raise it.
- **SDRplay** — `IFGR` is IF ***attenuation*** in dB, **20–59**, so **lower means more gain**; 40 is
  a sane start. `RFGR` is the LNA *state index* (0 = least attenuation) and its legal range depends
  on the band. `agc = true` hands IF gain to the driver instead. `rfnotch_ctrl` is the broadcast-FM
  notch — leave it off for 700/800 MHz work.
- **RTL-SDR** — one `TUNER` element, **0–49.6 dB**; `gain = 40.0` is equivalent and simpler. More
  importantly, **set `ppm`** (below).
- **HackRF** — `lna_db` 0–40 in 8 dB steps, `vga_db` 0–62 in 2 dB steps, `amp` for the +14 dB
  front end. Aim for I/Q peaks well short of full scale with no clipping.

## Frequency correction (ppm)

The LSM/C4FM decoder does not acquire a control channel past a few hundred Hz of residual carrier
offset, and at 855 MHz every ppm of oscillator error is about 855 Hz. So a board that *streams*,
whose waterfall shows the carrier, and that still hunts forever is almost always off frequency.

- **Airspy** — the R2's TCXO is nominally 0.5 ppm, but individual boards can be ~1 ppm out, which
  is enough that the control channel never acquires. sdrtrunk hides this with automatic PPM
  correction, so a board that works in sdrtrunk and only hunts in trunkd is the classic symptom.
- **RTL-SDR** — these dongles run tens of ppm off; 50 ppm at 855 MHz is 43 kHz, more than three
  12.5 kHz bins, so the control channel is not where trunkd looks. Measure once with
  `rtl_test -p` or kalibrate and write it down.

**`auto_ppm` (on by default) corrects this for you** on backends that can retune at runtime
(`soapy`). The hunt measures each control candidate's carrier offset and re-tunes the board until
it is centred, up to `auto_ppm_limit` (default 5 ppm). The applied total shows as
`auto_ppm_applied` on the panel and in the status POST, `auto_ppm` in metrics, and in the
`auto frequency correction` log line. Set `auto_ppm = false` to pin a board to exactly what you
configured. On the panel, `carrier off` (`freq_error_hz`) is the residual to fold in by hand — not
`pll err`, which is the demodulator loop's phase error and reads a couple of hundred Hz on a
perfectly healthy tuner.

If a board hunts while a sibling on the same antenna locks and you would rather set `ppm` by hand,
try `ppm = 1.0` and `-1.0` for 30 s each: the one that locks gives the sign.

**You should not have to copy the measured value.** What the loop measures is written down per
board and applied at the *next* open, so the correction is paid for once rather than once per
restart. Three things compose, in this order:

1. **`[[device]] ppm`** — yours, applied at open.
2. **The remembered correction** — added on top, so the board comes up on its known offset and
   locks in the first seconds instead of hunting its way back. It is stored as a *total* alongside
   the `ppm` it was measured against, so editing `ppm` later moves the result by exactly what you
   typed. A remembered value past `auto_ppm_limit` is clamped to it, with a warning.
3. **The hunt** — if the board still does not lock, the hunt-and-correct path runs as before,
   starting from the remembered total, so its first measurement *refines* the correction. Every
   correction rewrites the memory.

The file is `[recorder] ppm_memory_path` (default `state/ppm-memory.json`, beside the spool), keyed
by backend and serial (`soapy:airspy:<serial>`, `hackrf:<serial>`) so the memory follows the board
across a device rename or a different USB port; a board that reports no serial falls back to its
device id. It is written atomically on every correction and again on a clean stop. `trunkd check`
prints the table — board, ppm, sample count and age — and deleting the file makes every board
re-measure. A corrupt or unreadable file costs one warning and is ignored. With `auto_ppm = false`
nothing is applied: `ppm` is yours alone, and the memory is only reported so you can see the number
worth typing.

## Tuners for conventional channels

A tuner can carry analog `[[channel]]`s alongside a P25 site, or channels alone
([configuration.md](configuration.md#channel--one-conventional-analog-channel)). The channels come
out of the same channelizer, so the sample-rate rule above applies unchanged. A few things differ
on a **channel-only tuner** (no `[[site]]` on it):

* **No automatic frequency correction.** `auto_ppm` is driven by the P25 control-channel hunt, and
  a channel-only tuner has nothing to hunt, so it never measures or corrects its own offset. A
  correction remembered from an earlier run of the same board with a site is still applied at
  open; otherwise set `ppm` by hand. Each call reports its measured carrier offset (`offsetHz` on
  the upload, `channel.N.offset_hz` in metrics), which is the number to watch: one ppm at 155 MHz is
  155 Hz, so a dongle 30 ppm out puts a VHF carrier about 4.7 kHz off — past the ±4 kHz passband
  of a 12.5 kHz channel's filter, with its audio badly distorted or lost. Aim for an offset under a
  few hundred Hz.
  Several channels on one tuner should all show about the same offset in ppm; one that differs is
  a transmitter that is off frequency, not the tuner.
* **Keep channels off the centre.** The capture centre (DC) bin is refused at start, and on
  zero-IF radios (RTL-SDR, SDRplay) the centre also carries the local-oscillator spur. Put
  `center_frequency` at least 25 kHz from every channel.
* **The analog filter, not the sample rate, sets the usable span.** `sample_rate` decides what the
  channelizer can address (`center_frequency ± (sample_rate/2 − 12.5 kHz)`, which `trunkd check`
  enforces); the radio's own IF filter (`bandwidth_hz` on Soapy devices, chosen by the driver when
  unset) decides what actually arrives unattenuated. On radios whose filter steps are narrower than
  the sample rate — an SDRplay at 2 Msps with a 1.536 MHz filter, for example — a channel between
  the filter edge and the capture edge passes the check and then records weakly or not at all,
  with energy folded in from beyond the edge. Keep every channel inside the filter with some
  margin; the 1 % edge warning at start is a floor, not a guarantee.
* **Choosing a centre.** Start at the midpoint of the lowest and highest channel, check that the
  spread plus a margin at each edge fits inside the analog filter, then nudge the centre (by
  50–100 kHz, say) until no channel is within 25 kHz of it and none sits near the filter edge.
  Avoid centring on a strong local transmitter you do not record. `trunkd check` validates the
  result; if the channels do not fit in one capture, split them over two tuners.
* **Gain.** Conventional channels are often stronger and closer than a P25 site. The squelch is
  relative to the tracked noise floor, so it does not care about absolute level, but an overloaded
  front end raises that floor for every channel: reduce gain until the waterfall's floor stops
  falling with it.

The per-channel cost is small next to the filterbank: on a 3 GHz desktop CPU each busy channel
takes about 0.35 % of one core, so eleven channels on a 10 Msps tuner add roughly 4 % of a core to
the filterbank's ~14 %. `cargo test --release -p trunkd-analog --test bench -- --ignored
--nocapture` measures it on your own hardware.

## Sizing the host

* **CPU.** The channelizer's cost scales with the sample rate, not with how busy the site is. As a
  rough guide, one 20 Msps HackRF capture of a busy site uses about half of one core of a recent
  x86-64 laptop CPU; a 10 Msps Airspy about half that. Leave headroom for whatever else the box
  runs, and watch the `overflow` column of the status line ([operations.md](operations.md#what-the-log-says)):
  it should stay at 0.
* **Memory.** The SDR ring dominates: `ring_seconds` of samples rounded up to a power of two (about
  256 MiB per 20 Msps tuner at the default 1 s; less at lower rates). The process itself is a few
  tens of MB.
* **Disk.** The spool is capped (`spool_cap_bytes`, 1 GiB by default) and only fills while the
  server is unreachable. The raw event log is the steady user: on a busy site expect a couple of
  hundred MB a day uncompressed for the current day and ~20 MB per gzipped past day, kept
  `event_log_keep_days` (14) days.
* **USB.** One wideband SDR per USB 2.0 controller where possible. A capture that delivers less
  than its configured `throughput` is a USB problem, not a DSP one.
* **Clock.** Keep the host on NTP; see [operations.md](operations.md#timestamps-and-the-host-clock).

On Linux, granting real-time priority to the DSP thread ([install-linux.md](install-linux.md#real-time-priority))
lets a recorder share its CPU with other work without overflowing. Windows has no equivalent, so
use a deeper ring (`ring_seconds = 2.0`) there.
