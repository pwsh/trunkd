#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
#
# trunkd-watch.sh - push alerts to ntfy when a recorder box (or the hypervisor it
# runs on) needs attention. Run every couple of minutes by trunkd-watch.timer.
#
#   trunkd-watch.sh [--config FILE] [--test] [--status]
#
# Every check yields a subject ("disk:/", "tuner:ctrl", ...) that is either OK or
# not. A subject going bad is announced once (a "raise"); staying bad is silent;
# it is announced again when it has been OK for CLEAR_AFTER consecutive runs (a
# "clear"). State lives in STATE_DIR, one file per raised subject.
#
# Checks, each switched on by its setting in /etc/trunkd/watch.conf:
#   DISK_PATHS        used space on these mount points (WARN/CRIT percent)
#   SERVICES          these systemd units must be active
#   FAILED_UNITS      these units must not be in the failed state (oneshots)
#   PANEL_URL         trunkd's panel: every enabled tuner with a site locked, no
#                     device unit failed or retrying, no sustained sample
#                     overflow, uploads flowing (no open circuit, spool small)
#   CHECK_CLOCK=1     the system clock is NTP-synchronised
#   PVE_STORAGE=1     Proxmox: every active storage below WARN/CRIT percent
#   PVE_THINPOOL=1    Proxmox: LVM thin pools (data and metadata) below WARN/CRIT
#   PVE_CONTAINERS    Proxmox: these container ids are running
#
# --test sends one notification and exits; --status prints every subject and
# whether it is raised, and sends nothing.
set -Eeuo pipefail

CONF=/etc/trunkd/watch.conf
MODE=run
while [[ $# -gt 0 ]]; do
  case "$1" in
    --config) CONF="$2"; shift ;;
    --test) MODE="test" ;;
    --status) MODE="status" ;;
    -h|--help) sed -n '3,30p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
  shift
done

NTFY_URL=""              # e.g. https://ntfy.example.org/radio (server + topic)
NTFY_TOKEN=""            # optional access token
HOST_LABEL="$(hostname -s)"
DISK_PATHS="/"
DISK_WARN=85
DISK_CRIT=95
SERVICES=""
FAILED_UNITS=""
PANEL_URL=""
PANEL_TOKEN=""
SPOOL_WARN_CALLS=50
CHECK_CLOCK=1
PVE_STORAGE=0
PVE_THINPOOL=0
PVE_CONTAINERS=""
CLEAR_AFTER=2
STATE_DIR=/var/lib/trunkd-watch
# shellcheck source=/dev/null
[[ -r "$CONF" ]] && . "$CONF"

mkdir -p "$STATE_DIR"
exec 9>"$STATE_DIR/.lock"
flock -n 9 || exit 0

notify() { # priority tags title message
  local prio="$1" tags="$2" title="$3" msg="$4"
  if [[ -z "$NTFY_URL" ]]; then
    printf '[%s] %s: %s\n' "$prio" "$title" "$msg"
    return 0
  fi
  local auth=()
  [[ -n "$NTFY_TOKEN" ]] && auth=(-H "Authorization: Bearer $NTFY_TOKEN")
  curl -fsS --max-time 15 -o /dev/null "${auth[@]}" \
    -H "Title: $title" -H "Priority: $prio" -H "Tags: $tags" \
    --data-binary "$msg" "$NTFY_URL" || echo "ntfy delivery failed: $title" >&2
}

if [[ "$MODE" == test ]]; then
  notify 3 "white_check_mark" "$HOST_LABEL: watch test" "trunkd-watch on $HOST_LABEL can reach this topic."
  exit 0
fi

# ------------------------------------------------------------------ observations
# Each check appends "subject<TAB>severity<TAB>message" for every subject it
# looked at; severity is ok, warn or crit.
OBS=()
obs() { OBS+=("$1"$'\t'"$2"$'\t'"$3"); }

pct_sev() { # used% -> severity
  if (( $1 >= DISK_CRIT )); then echo crit; elif (( $1 >= DISK_WARN )); then echo warn; else echo ok; fi
}

for p in $DISK_PATHS; do
  if line=$(df -P "$p" 2>/dev/null | awk 'NR==2'); then
    used=$(awk '{gsub("%","",$5); print $5}' <<<"$line")
    free=$(awk '{printf "%.1f", $4/1048576}' <<<"$line")
    obs "disk:$p" "$(pct_sev "$used")" "$p is ${used}% full (${free} GB free)"
  else
    obs "disk:$p" crit "$p cannot be read"
  fi
done

for u in $SERVICES; do
  if systemctl is-active --quiet "$u"; then obs "service:$u" ok "$u active"
  else obs "service:$u" crit "$u is $(systemctl is-active "$u" 2>/dev/null || true)"; fi
done

for u in $FAILED_UNITS; do
  if systemctl is-failed --quiet "$u"; then
    obs "failed:$u" warn "$u failed: $(journalctl -u "$u" -n 3 -o cat --no-pager 2>/dev/null | tail -1 | cut -c1-200)"
  else obs "failed:$u" ok "$u not failed"; fi
done

if (( CHECK_CLOCK )); then
  if [[ "$(timedatectl show -p NTPSynchronized --value 2>/dev/null)" == yes ]]; then obs clock ok "clock synchronised"
  else obs clock warn "system clock is not NTP-synchronised"; fi
fi

if [[ -n "$PANEL_URL" ]]; then
  auth=()
  [[ -n "$PANEL_TOKEN" ]] && auth=(-H "Authorization: Bearer $PANEL_TOKEN")
  if body=$(curl -fsS --max-time 10 "${auth[@]}" "$PANEL_URL/api/status" 2>/dev/null); then
    obs panel ok "panel answering"
    while IFS=$'\t' read -r s sev m; do obs "$s" "$sev" "$m"; done < <(
      SPOOL_WARN_CALLS="$SPOOL_WARN_CALLS" STATE_DIR="$STATE_DIR" python3 -I -c '
import json, os, sys
s = json.loads(sys.stdin.read())
out = []
def o(subject, sev, msg): out.append(f"{subject}\t{sev}\t{msg}")
units = {u.get("name"): u for u in s.get("supervisor", []) if isinstance(u, dict) and u.get("name")}
for t in s.get("tuners", []):
    key = t.get("tuner_key")
    if not t.get("enabled"):
        continue
    # Only tuners serving a P25 site lock (they carry a traffic pool); a tuner
    # with analog channels alone reports a site label but has nothing to lock.
    site = t.get("site") if int(t.get("pool_size") or 0) > 0 else None
    if site:
        if t.get("locked"):
            o(f"tuner:{key}", "ok", f"{key} locked on {site}")
        else:
            o(f"tuner:{key}", "crit", f"{key} is not locked on {site}")
    # Overflow: raise only when it was also non-zero last run (a one-off blip is noise).
    path = os.path.join(os.environ["STATE_DIR"], f".overflow-{key}")
    ovf = float(t.get("overflow_per_s") or 0)
    prev = float(open(path).read() or 0) if os.path.exists(path) else 0.0
    open(path, "w").write(str(ovf))
    o(f"overflow:{key}", "warn" if ovf > 0 and prev > 0 else "ok",
      f"{key} is dropping samples ({ovf:.0f}/s); the box may be overloaded" if ovf > 0 and prev > 0 else f"{key} no overflow")
for name, u in units.items():
    state = str(u.get("state", "")).lower()
    err = u.get("last_error") or ""
    if state in ("failed", "retrying"):
        rc = u.get("restart_count", 0)
        o(f"unit:{name}", "crit", f"{name} is {state} (restarts {rc}) {err}".strip())
    else:
        o(f"unit:{name}", "ok", f"{name} {state}")
up = s.get("uplink", {}) or {}
spooled = int(up.get("spooled_calls") or 0)
if up.get("circuit_open"):
    o("uplink", "crit", f"uploads to keyup are failing (circuit open, {spooled} calls spooled)")
elif spooled > int(os.environ["SPOOL_WARN_CALLS"]):
    o("uplink", "warn", f"{spooled} calls waiting in the spool")
else:
    o("uplink", "ok", "uploads flowing")
print("\n".join(out))
' <<<"$body")
  else
    obs panel crit "trunkd panel at $PANEL_URL is not answering"
  fi
fi

if (( PVE_STORAGE )) && command -v pvesm >/dev/null; then
  while read -r name _type status total used _avail pct; do
    [[ "$status" == active && "$total" =~ ^[0-9]+$ && "$total" -gt 0 ]] || continue
    p=${pct%%.*}; p=${p%\%}
    obs "storage:$name" "$(pct_sev "$p")" "storage $name is ${pct} used ($(( (total-used)/1048576 )) GB free)"
  done < <(pvesm status 2>/dev/null | awk 'NR>1')
fi

if (( PVE_THINPOOL )) && command -v lvs >/dev/null; then
  while read -r lv vg data meta; do
    d=${data%%.*}; m=${meta%%.*}
    worst=$(( d > m ? d : m ))
    obs "thinpool:$vg/$lv" "$(pct_sev "$worst")" "thin pool $vg/$lv data ${data}% metadata ${meta}%"
  done < <(lvs --noheadings -o lv_name,vg_name,data_percent,metadata_percent -S 'lv_attr=~^t' 2>/dev/null)
fi

if [[ -n "$PVE_CONTAINERS" ]] && command -v pct >/dev/null; then
  for id in $PVE_CONTAINERS; do
    st=$(pct status "$id" 2>/dev/null | awk '{print $2}')
    if [[ "$st" == running ]]; then obs "ct:$id" ok "container $id running"
    else obs "ct:$id" crit "container $id is ${st:-missing}"; fi
  done
fi

# ------------------------------------------------------------------ transitions
if [[ "$MODE" == status ]]; then
  for o in "${OBS[@]}"; do
    IFS=$'\t' read -r s sev m <<<"$o"
    printf '%-28s %-5s %s%s\n' "$s" "$sev" "$m" "$([[ -e "$STATE_DIR/$(tr '/:' '__' <<<"$s")" ]] && echo '  [raised]')"
  done
  exit 0
fi

for o in "${OBS[@]}"; do
  IFS=$'\t' read -r s sev m <<<"$o"
  f="$STATE_DIR/$(tr '/:' '__' <<<"$s")"
  if [[ "$sev" != ok ]]; then
    rm -f "$f.ok"
    if [[ ! -e "$f" ]] || [[ "$(cat "$f")" != "$sev" ]]; then
      echo "$sev" >"$f"
      if [[ "$sev" == crit ]]; then notify 5 "rotating_light" "$HOST_LABEL: $s" "$m"
      else notify 4 "warning" "$HOST_LABEL: $s" "$m"; fi
    fi
  elif [[ -e "$f" ]]; then
    n=$(( $(cat "$f.ok" 2>/dev/null || echo 0) + 1 ))
    if (( n >= CLEAR_AFTER )); then
      rm -f "$f" "$f.ok"
      notify 3 "white_check_mark" "$HOST_LABEL: $s recovered" "$m"
    else
      echo "$n" >"$f.ok"
    fi
  fi
done
