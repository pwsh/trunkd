# Running trunkd under systemd

Two units, pick one:

| Unit | Runs as | Config | State (spool, events, `state/`) | Real-time priority |
|---|---|---|---|---|
| `trunkd.service` | system account `trunkd` | `/etc/trunkd/trunkd.toml` | `/var/lib/trunkd` | granted by the unit (`AmbientCapabilities=CAP_SYS_NICE`) |
| `trunkd-user.service` | you | `~/.config/trunkd/trunkd.toml` | `~/.local/state/trunkd` | `scripts/grant-rt.sh` after each build |

Both run `trunkd --config <file>` in the foreground, log to the journal, stop with
`SIGINT` (trunkd's clean-shutdown signal) and restart on failure. Relative paths in the
config (`[uplink] spool_path`, `[recorder] event_log_dir`, the ppm memory) resolve against
the unit's working directory, which is the state directory above.

## System service

```sh
# 1. The binary
cargo build --release
sudo install -m 0755 target/release/trunkd /usr/local/bin/trunkd

# 2. An account with access to the SDR (plugdev) and the PortaPack console (dialout)
sudo useradd --system --home-dir /var/lib/trunkd --shell /usr/sbin/nologin \
     --groups plugdev,dialout trunkd

# 3. The config. The directory is writable by trunkd because the local panel can
#    edit the [[device]]/[[site]] tables in place; the file carries the Keyup
#    upload token, so keep it out of reach of other users.
sudo install -d -o trunkd -g trunkd -m 0750 /etc/trunkd
sudo install -o trunkd -g trunkd -m 0640 trunkd.example.toml /etc/trunkd/trunkd.toml
sudoedit /etc/trunkd/trunkd.toml
sudo -u trunkd /usr/local/bin/trunkd check --config /etc/trunkd/trunkd.toml

# 4. The unit
sudo install -m 0644 scripts/systemd/trunkd.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now trunkd
journalctl -u trunkd -f
```

Look for `DSP thread running at real-time priority` and `control channel acquired` in the
log. Rebuilding is `cargo build --release`, the `install` line from step 1 and
`sudo systemctl restart trunkd`; no `setcap` is needed because the unit grants the
capability itself.

If your distribution has no `plugdev` group, create it or edit `SupplementaryGroups=` to
whatever group its SDR udev rules use (see `scripts/udev/README.md`). The unit is
hardened (`ProtectSystem=strict`, `ProtectHome=true`, no new privileges); if you keep the
config or the spool somewhere other than `/etc/trunkd` and `/var/lib/trunkd`, add those
paths to `ReadWritePaths=` with `sudo systemctl edit trunkd`.

## User service

For a box you are logged into anyway, or for running a build straight out of a checkout.
The install commands are at the top of `trunkd-user.service`. Two things differ from the
system service:

* A user unit cannot grant capabilities. Run `scripts/grant-rt.sh` after every build (the
  file capability is lost whenever the binary is rewritten); to make that passwordless,
  install the sudoers rule from `scripts/trunkd-setcap.sudoers.template`. Without it the
  recorder runs at normal priority and says so once in the log.
* User services stop when you log out unless lingering is on:
  `sudo loginctl enable-linger "$USER"`.

Manage it with `systemctl --user {status,restart,stop} trunkd` and
`journalctl --user -u trunkd -f`.

## SoapySDR installed somewhere unusual

trunkd loads `libSoapySDR` at run time and finds it the way the dynamic linker does
(`libSoapySDR.so.0.8`, then `libSoapySDR.so`). If you built SoapySDR into a private prefix
such as `~/.local`, name the library and its driver modules explicitly rather than relying
on `LD_LIBRARY_PATH`, which the kernel ignores for a binary carrying a file capability:

```ini
# sudo systemctl edit trunkd   (or systemctl --user edit trunkd)
[Service]
Environment=TRUNKD_SOAPYSDR_LIB=/opt/soapy/lib/libSoapySDR.so.0.8
Environment=SOAPY_SDR_PLUGIN_PATH=/opt/soapy/lib/SoapySDR/modules0.8
```

`trunkd check` prints which library it loaded and which radios it enumerated.
