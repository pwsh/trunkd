# Installing trunkd on Linux

This guide takes a Linux box from nothing to a trunkd recorder running under systemd. It assumes a
Debian or Ubuntu system; other distributions need the equivalent packages. Windows is covered in
[install-windows.md](install-windows.md).

What you need:

* A supported SDR and an antenna for your site's band ([sdr-hardware.md](sdr-hardware.md)).
* The control-channel frequencies of the P25 site you want to record (from RadioReference, or an
  sdrtrunk playlist).
* Somewhere to send calls: a [Keyup](https://github.com/pwsh/keyup) server and a recorder token for
  it, or any Rdio-Scanner-compatible server ([keyup.md](keyup.md)). You can also start with no
  server at all and only spool calls to disk.

## 1. Get the binary

**From a release.** Tagged releases on <https://github.com/pwsh/trunkd/releases> carry a Linux
x86-64 tarball: the binary, `trunkd.example.toml`, and the systemd unit with its notes under
`systemd/` (use `systemd/trunkd.service` wherever this page says `scripts/systemd/trunkd.service`).
Unpack it, check the binary runs (`./trunkd --version`), and skip to step 2.

**From source.**

```bash
sudo apt install build-essential git curl
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh    # or your distribution's rustup
git clone https://github.com/pwsh/trunkd.git
cd trunkd
cargo build --release          # rust-toolchain.toml selects the toolchain
./target/release/trunkd --version
```

The only C code in the build is LAME (the MP3 encoder), compiled from vendored source through the
`cc` crate, so a C compiler is the only native build dependency. The SDR libraries are **not**
needed to build: SoapySDR is loaded at run time, and HackRFs are driven through the
`hackrf_transfer` tool. The default features (`soapy`, `hackrf`) cost nothing at build time.

To run the test suite: `cargo test --workspace`. Tests that need large off-air IQ captures report
themselves as ignored unless you provide one ([capturing-test-iq.md](capturing-test-iq.md)).

## 2. Install the SDR runtime

Install the runtime for your radio and its udev rules, following
[sdr-hardware.md](sdr-hardware.md#linux-runtime) (SoapySDR + modules for Airspy, SDRplay and
RTL-SDR; the `hackrf` package for a HackRF). Check that the radio is visible to an ordinary user:

```bash
SoapySDRUtil --find          # Airspy / SDRplay / RTL-SDR
hackrf_info                  # HackRF
```

## 3. Install the binary, a user and a config

```bash
sudo install -m 0755 target/release/trunkd /usr/local/bin/trunkd
sudo useradd --system --home-dir /var/lib/trunkd --create-home --shell /usr/sbin/nologin trunkd
sudo usermod -aG plugdev,dialout trunkd       # USB radios; dialout for a PortaPack console
sudo install -d -m 0750 -o trunkd -g trunkd /etc/trunkd
sudo install -m 0640 -o trunkd -g trunkd trunkd.example.toml /etc/trunkd/trunkd.toml
```

Edit `/etc/trunkd/trunkd.toml` ([configuration.md](configuration.md) walks through it). At minimum:

* `[uplink] url`, `api_key`, `system_label`, `site_label` — or leave `url` empty for now;
* one `[[device]]` for your radio;
* one `[[site]]` with your control frequencies;
* `[recorder] instance_id`, a stable name for this box;
* absolute paths for the data directories, so they do not depend on the service's working
  directory:

  ```toml
  [uplink]
  spool_path = "/var/lib/trunkd/spool"

  [recorder]
  event_log_dir   = "/var/lib/trunkd/events"
  ppm_memory_path = "/var/lib/trunkd/state/ppm-memory.json"
  ```

The `trunkd` user owns the directory because the panel's device editor
([panel.md](panel.md#setting-up-devices-from-the-panel)) rewrites this file in place, with `.tmp` and
`.bak` files beside it. If you would rather edit the file only as root, make both `root:trunkd`
(`0750`/`0640`) and use the panel read-only.

To keep the upload token out of the file, leave `api_key` empty and set
`TRUNKD_UPLINK__API_KEY` in a root-only environment file the unit loads (`EnvironmentFile=`).

Validate:

```bash
sudo -u trunkd trunkd --config /etc/trunkd/trunkd.toml check
```

`check` prints the parsed config and the SDRs the `trunkd` user can see, and opens nothing.

## 4. First run in the foreground

Prove reception before uploading anything. Run once with the upload hold on:

```bash
sudo -u trunkd env TRUNKD_UPLINK__SPOOL_ONLY=true \
  trunkd --config /etc/trunkd/trunkd.toml run
```

Look for `control channel acquired` within a few seconds, then the 10 s status line
([operations.md](operations.md#what-the-log-says)). After a few calls, stop it with Ctrl+C and
listen to the MP3s under `/var/lib/trunkd/spool/`. If it hunts forever, see
[sdr-hardware.md](sdr-hardware.md#frequency-correction-ppm).

## 5. Run it under systemd

The repository ships a system unit, [`scripts/systemd/trunkd.service`](../scripts/systemd/trunkd.service):
it runs `/usr/local/bin/trunkd --config /etc/trunkd/trunkd.toml` as the `trunkd` user with the
`plugdev` and `dialout` groups, grants the DSP thread `CAP_SYS_NICE` through
`AmbientCapabilities=`, and restarts on failure. Install it and start it:

```bash
sudo install -m 0644 scripts/systemd/trunkd.service /etc/systemd/system/trunkd.service
sudo systemctl daemon-reload
sudo systemctl enable --now trunkd
journalctl -u trunkd -f
```

[`scripts/systemd/README.md`](../scripts/systemd/README.md) describes the unit's settings, and
`trunkd-user.service` beside it, a variant for running trunkd as your own user from a checkout
(`systemctl --user`). Edit a copy (`systemctl edit trunkd`) rather than the installed file if you
need to change paths, add `EnvironmentFile=`, or set `Nice=`.

A stop (`systemctl stop trunkd`) is the same clean shutdown as Ctrl+C: the unit sends SIGINT
(`KillSignal=SIGINT`), so open calls finish, the queues drain and nothing is lost. trunkd does not
catch SIGTERM, so a plain `kill` ends it at once; send `kill -INT` instead. The unit's `Restart=on-failure` catches a process that dies; the
optional health monitor catches one that is alive but no longer recording
([operations.md](operations.md#health-monitor)).

When the recorder looks right, release the upload hold — it was only an environment override for
the foreground run, so the service already uploads unless the file sets `spool_only = true`, in
which case release it from Keyup or from the panel ([panel.md](panel.md#the-upload-hold)) rather
than editing the file.

## Real-time priority

The DSP thread asks for `SCHED_FIFO` at the *lowest* real-time priority when it starts, so that a
compiler or backup job on the same box cannot starve it into dropping samples. This needs
`CAP_SYS_NICE`; without it the recorder logs `no permission for real-time scheduling; running at
normal priority` and carries on. Nothing about recording depends on it, and Linux's real-time
throttling still bounds a runaway. The point is to outrank a compiler, not the kernel.

* **Under the shipped system unit** it is already granted (`AmbientCapabilities=CAP_SYS_NICE`).
* **Running a binary from a checkout**, give the file the capability instead:

  ```bash
  scripts/grant-rt.sh          # setcap cap_sys_nice+ep on target/release/trunkd
  ```

  File capabilities do not survive a rebuild, so this runs after every build that will feed a live
  recorder. [`scripts/trunkd-setcap.sudoers.template`](../scripts/trunkd-setcap.sudoers.template)
  is a sudoers rule that makes the re-grant passwordless: replace its user and path placeholders
  with yours, then install it with
  `sudo install -m 0440 <your copy> /etc/sudoers.d/trunkd-setcap` (check it first with
  `visudo -cf <your copy>`).

Confirm from the startup log: `DSP thread running at real-time priority`.

## Keep the clock right

Every timestamp trunkd writes is anchored to the system clock, and two recorders on one site must
agree to within 30 s for the recorder link to work at all. Make sure the box runs an NTP client
(`timedatectl` should say `System clock synchronized: yes`); chrony against a nearby server is
better than the distribution default. Why it matters:
[operations.md](operations.md#timestamps-and-the-host-clock).

## Upgrading

```bash
cd trunkd && git pull && cargo build --release
sudo install -m 0755 target/release/trunkd /usr/local/bin/trunkd
sudo systemctl restart trunkd
```

New config keys come with defaults, so an existing file keeps working; read `CHANGELOG.md` for
anything that changed meaning.
`/about` on the panel shows which build is running ([panel.md](panel.md#about--diagnostics-about-apiabout)).

## Where to go next

* [configuration.md](configuration.md) — every table in the config file.
* [sdr-hardware.md](sdr-hardware.md) — radios, gains, frequency correction, host sizing.
* [panel.md](panel.md) — the local web panel.
* [keyup.md](keyup.md) — what is sent upstream, and what Keyup can change remotely.
* [recorder-link.md](recorder-link.md) — a second recorder on the same site.
* [operations.md](operations.md) — logs, spool, event log, clock, health monitor.
* [capturing-test-iq.md](capturing-test-iq.md) — recording IQ for offline replay and tests.
* [ARCHITECTURE.md](ARCHITECTURE.md) — how the engine works inside; [P25P1-SPEC.md](P25P1-SPEC.md)
  and [SPEC-GAPS.md](SPEC-GAPS.md) for the protocol notes it was built from, and
  [NON-VOICE-DATA.md](NON-VOICE-DATA.md) for the non-voice traffic it records.
