# Automatic updates on Linux

`update-trunkd.sh` keeps a system install (the layout from
[docs/install-linux.md](../../docs/install-linux.md): `/usr/local/bin/trunkd`,
`/etc/trunkd/trunkd.toml`, `trunkd.service`) current. A systemd timer runs it once a day.

It has two sources:

| `SOURCE=` | Where the new binary comes from | Triggers an update when | Needs on the box |
| --- | --- | --- | --- |
| `release` (default) | The Linux x86-64 tarball of the latest **published** GitHub release of `GITHUB_REPO`. Drafts and pre-releases are never picked up. The tarball is checked against the release's `SHA256SUMS`. | the release's version is higher than `trunkd --version` | `curl`, `python3` |
| `git` | A clone of `GIT_URL` kept in `/var/lib/trunkd-update/src`, built with `cargo build --release --locked`, then `cargo test` unless `RUN_TESTS=0` | `TRACK=version`: the `[workspace.package]` version in `Cargo.toml` on `GIT_REF` goes up. `TRACK=commit`: `GIT_REF` has a commit that was not the last one deployed. `GIT_REF=latest-tag` follows the highest `v*` tag. | `git`, a Rust toolchain (`rustup`), a C compiler |

`release` is the one to use on a recorder: nothing is compiled on the box, so the DSP
thread never competes with a build. Use `git` on an architecture with no published build
(only x86-64 is published), or to follow a fork or a private mirror.

Every run, whatever the source:

1. **Pre-flight.** The new binary runs `trunkd --config /etc/trunkd/trunkd.toml check` as the
   `trunkd` user. A build that rejects the live config is never installed.
2. **Install.** The running binary is kept as `/usr/local/bin/trunkd.prev`, the new one is
   moved into place, and `trunkd.service` restarts. The stop is the usual SIGINT, so open
   calls finish and the spool is kept.
3. **Verify.** The local panel's `/api/status` is polled until every enabled P25 site tuner
   (one with a traffic pool; analog-channel-only tuners have nothing to lock) is locked again (up to `LOCK_TIMEOUT`, 120 s). The panel's address and bearer token
   come from `[server]` in the config. A recorder with no site tuners, only conventional
   channels, passes once it has stayed up for 20 s. With the panel disabled, it passes if the
   service stays up for 30 s.
4. **Roll back.** If verification fails, `trunkd.prev` goes back in and the service restarts
   again. The run exits 1 either way, so the timer's unit shows as failed.

## Install

As root, from a checkout (in a release tarball the same files are under `update/`):

```bash
install -m 0755 scripts/linux/update-trunkd.sh /usr/local/sbin/update-trunkd.sh
install -m 0644 scripts/linux/trunkd-update.service scripts/linux/trunkd-update.timer /etc/systemd/system/
install -m 0644 scripts/linux/update.conf.example /etc/trunkd/update.conf   # optional: defaults are SOURCE=release
systemctl daemon-reload
update-trunkd.sh --check          # 0 = up to date, 10 = an update is available
systemctl enable --now trunkd-update.timer
```

The timer fires daily at 04:30 local time, plus up to 30 minutes of random delay. If the box
was off at that time, it runs at boot.

## Running it by hand

```bash
systemctl start trunkd-update            # one run now, logged to the journal
journalctl -u trunkd-update -n 50        # what the last runs did
update-trunkd.sh --check                 # look, change nothing
update-trunkd.sh --stage-only --force    # fetch/build + pre-flight, install nothing (exit 2)
update-trunkd.sh --force                 # reinstall even when already current
update-trunkd.sh --no-rollback           # keep a build that fails verification
```

| exit | meaning |
| --- | --- |
| 0 | up to date, or updated and verified |
| 1 | failed: nothing changed, or rolled back |
| 2 | staged, not installed (`--stage-only`) |
| 3 | another run holds the lock |
| 10 | `--check`: an update is available |

## Settings

`/etc/trunkd/update.conf` is sourced as shell. [`update.conf.example`](update.conf.example)
lists every setting with its default. The ones you are most likely to change:

* `SOURCE`, `GITHUB_REPO`: release source.
* `GIT_URL`, `GIT_REF`, `TRACK`, `RUN_TESTS`, `BUILD_JOBS`: git source. A private mirror that
  needs credentials goes in `GIT_URL`. Keep the file root-only (`chmod 600`) if it holds a
  token.
* `PIN=0.2`: take 0.2.x updates only. A 0.3.0 waits until you change the pin.

State lives in `/var/lib/trunkd-update`:

| path | what |
| --- | --- |
| `deployed` | `<source> <version> <commit or -> <sha256> <time>` of the last verified install |
| `stage/` | the last downloaded or built binary and its pre-flight log |
| `src/` | the git source's clone and its cargo `target/` |
| `lock` | held for the length of a run |

## In a container

* Nothing about the updater changes in an LXC container. It only needs the service and the
  local panel, both of which are inside.
* A `SOURCE=git` build shares the container's CPUs with the recorder. The unit runs it at
  `Nice=19`, idle I/O, `CPUWeight=20` and `-j2`. In an unprivileged container the recorder
  itself runs without real-time priority, so prefer `SOURCE=release` there.

# Alerts to ntfy

`trunkd-watch.sh` and its two-minute timer push a notification to an
[ntfy](https://ntfy.sh) topic when something on the box needs attention. It sends one when a
problem appears and one when it has stayed fixed for two runs; nothing in between. It covers what
the upstream archive cannot see from outside:

| Setting | Watches |
| --- | --- |
| `DISK_PATHS` | used space per mount point (`DISK_WARN` 85 %, `DISK_CRIT` 95 %) |
| `SERVICES` | units that must be active (e.g. `trunkd`, an SDR vendor service) |
| `FAILED_UNITS` | oneshots that must not be failed (e.g. `trunkd-update.service`) |
| `PANEL_URL` | trunkd's panel: every enabled P25 site tuner locked, no device unit failed or retrying, no sample overflow two runs running, uploads flowing (no open circuit, spool under `SPOOL_WARN_CALLS`) |
| `CHECK_CLOCK` | the clock is NTP-synchronised |
| `PVE_STORAGE`, `PVE_THINPOOL`, `PVE_CONTAINERS` | on a Proxmox host: storage use, LVM thin-pool data/metadata, containers running |

Install on the recorder box, and on its hypervisor if you want that watched too:

```bash
install -m 0755 scripts/linux/trunkd-watch.sh /usr/local/sbin/trunkd-watch.sh
install -m 0644 scripts/linux/trunkd-watch.service scripts/linux/trunkd-watch.timer /etc/systemd/system/
install -m 0600 scripts/linux/watch.conf.example /etc/trunkd/watch.conf     # set NTFY_URL and the checks
trunkd-watch.sh --status        # every subject and whether it is raised; sends nothing
trunkd-watch.sh --test          # one test notification
systemctl daemon-reload && systemctl enable --now trunkd-watch.timer
```
