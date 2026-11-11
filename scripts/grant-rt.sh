#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Grant the trunkd binary CAP_SYS_NICE so its DSP thread can take SCHED_FIFO
# (see crates/trunkd/src/pipeline/priority.rs). File capabilities do not
# survive a rebuild, so run this after every `cargo build` that will feed the
# live recorder. With the sudoers rule from scripts/trunkd-setcap.sudoers.template
# installed (fill in your user and this binary's path; the template says how) it
# needs no password.
#
# Only for running trunkd out of a source checkout. Installed under systemd, the
# units in scripts/systemd/ grant CAP_SYS_NICE through AmbientCapabilities, so
# neither this script nor the sudoers rule is needed.
#
# The binary is safe to setcap: it links only system libc/libm/libgcc (libhackrf
# is static), so secure-exec's env stripping changes nothing at runtime.
set -euo pipefail
BIN="$(cd "$(dirname "$0")/.." && pwd)/target/release/trunkd"
[ -x "$BIN" ] || { echo "no binary at $BIN — build first" >&2; exit 1; }
sudo -n /usr/sbin/setcap cap_sys_nice+ep "$BIN" 2>/dev/null \
  || sudo /usr/sbin/setcap cap_sys_nice+ep "$BIN"
/usr/sbin/getcap "$BIN"
