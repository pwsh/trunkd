#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
#
# update-trunkd.sh - keep a Linux trunkd install current, from published GitHub
# releases or from a git checkout built on this box.
#
#   update-trunkd.sh [--check] [--force] [--stage-only] [--no-rollback] [--config FILE]
#
# One run:
#   1. Decide whether there is anything newer than the installed binary:
#        SOURCE=release  the latest *published* GitHub release (drafts and
#                        pre-releases are never picked up) has a higher version;
#        SOURCE=git      the remote ref's Cargo.toml version is higher
#                        (TRACK=version), or its commit is not the one last
#                        deployed (TRACK=commit).
#   2. Get the new binary:
#        release  download the Linux tarball + SHA256SUMS, verify the checksum;
#        git      fetch into the updater's own clone, cargo build --release
#                 --locked (and cargo test unless RUN_TESTS=0), at low CPU/IO priority.
#   3. Pre-flight: the NEW binary must parse the live config (`trunkd check`).
#   4. Install: keep the current binary as <bin>.prev, swap, restart the service
#      (SIGINT: open calls finish, nothing is lost), then poll the local panel
#      until every enabled tuner that has a site is locked again.
#   5. If that does not happen within LOCK_TIMEOUT seconds, put <bin>.prev back
#      and restart again (unless --no-rollback).
#
# Settings come from /etc/trunkd/update.conf (shell syntax, see
# update.conf.example beside this script); every one has a default.
#
# Exit codes: 0 up to date, or updated and verified
#             1 failed (nothing changed, or rolled back)
#             2 new build staged, not installed (--stage-only)
#             3 another run holds the lock
#            10 --check: an update is available
set -Eeuo pipefail

CONF=/etc/trunkd/update.conf
CHECK_ONLY=0 FORCE=0 STAGE_ONLY=0 NO_ROLLBACK=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    --check) CHECK_ONLY=1 ;;
    --force) FORCE=1 ;;
    --stage-only) STAGE_ONLY=1 ;;
    --no-rollback) NO_ROLLBACK=1 ;;
    --config) CONF="$2"; shift ;;
    -h|--help) sed -n '3,33p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown option: $1" >&2; exit 1 ;;
  esac
  shift
done

# ---------------------------------------------------------------- settings
SOURCE=release                                   # release | git
GITHUB_REPO=pwsh/trunkd                          # owner/name for SOURCE=release
GIT_URL=https://github.com/pwsh/trunkd.git       # for SOURCE=git
GIT_REF=main                                     # branch name, or "latest-tag" (highest v* tag)
TRACK=version                                    # SOURCE=git: version | commit
RUN_TESTS=1                                      # SOURCE=git: cargo test before installing
CARGO=""                                         # path to cargo; default: PATH, then ~/.cargo/bin
BUILD_JOBS=2                                     # cargo -j; keep low beside a live recorder
PIN=""                                           # e.g. "0.2" to stay on 0.2.x; empty = any newer
BIN=/usr/local/bin/trunkd
SERVICE=trunkd
TRUNKD_CONFIG=/etc/trunkd/trunkd.toml
RUN_AS=trunkd                                    # account `trunkd check` runs as
PANEL_URL=""                                     # default: http://127.0.0.1:<[server] bind port>
LOCK_TIMEOUT=120
STATE_DIR=/var/lib/trunkd-update
# shellcheck source=/dev/null
[[ -r "$CONF" ]] && . "$CONF"

log() { printf '%s %s\n' "$(date -u +%FT%TZ)" "$*"; }
die() { log "ERROR: $*"; exit 1; }

mkdir -p "$STATE_DIR"
exec 9>"$STATE_DIR/lock"
flock -n 9 || { log "another update run is in progress"; exit 3; }

for tool in curl tar sha256sum python3 systemctl; do
  command -v "$tool" >/dev/null || die "missing tool: $tool"
done

# ---------------------------------------------------------------- helpers
installed_version() {
  [[ -x "$BIN" ]] || { echo 0.0.0; return; }
  "$BIN" --version 2>/dev/null | awk '{print $2; exit}'
}

# version_gt A B: A is strictly newer than B (sort -V).
version_gt() { [[ "$1" != "$2" && "$(printf '%s\n%s\n' "$1" "$2" | sort -V | tail -1)" == "$1" ]]; }

pin_ok() { [[ -z "$PIN" || "$1" == "$PIN" || "$1" == "$PIN".* ]]; }

# Panel URL and first bearer token, read from the live config by python's tomllib.
panel_info() {
  python3 -I - "$TRUNKD_CONFIG" <<'EOF'
import sys, tomllib
try:
    c = tomllib.load(open(sys.argv[1], "rb"))
except Exception:
    c = {}
s = c.get("server", {})
port = s.get("bind", "127.0.0.1:8080").rsplit(":", 1)[-1]
toks = [t.strip() for t in s.get("tokens", []) if t.strip()]
print(f"http://127.0.0.1:{port}")
print(toks[0] if toks else "")
print("1" if s.get("enabled", False) else "0")
EOF
}

# Wait until every enabled P25 site tuner is locked. A recorder with no site
# tuners (conventional channels only) passes once the panel answers and the
# service has stayed up for 20 s.
wait_locked() {
  local deadline=$((SECONDS + LOCK_TIMEOUT)) url token enabled last=""
  { read -r url; read -r token; read -r enabled; } < <(panel_info)
  [[ -n "$PANEL_URL" ]] && url="$PANEL_URL"
  if [[ "$enabled" != 1 ]]; then
    log "panel disabled in the config; checking only that $SERVICE stays up for 30 s"
    sleep 30
    systemctl is-active --quiet "$SERVICE"
    return
  fi
  while (( SECONDS < deadline )); do
    if ! systemctl is-active --quiet "$SERVICE"; then
      last="service not active"
    else
      local auth=()
      [[ -n "$token" ]] && auth=(-H "Authorization: Bearer $token")
      local body verdict
      if body=$(curl -fsS --max-time 5 "${auth[@]}" "$url/api/status" 2>/dev/null); then
        verdict=$(python3 -I -c '
import json, sys
s = json.loads(sys.stdin.read())
# P25 site tuners only (they carry a traffic pool): a tuner with analog
# channels alone reports a site label but never locks.
want = [t for t in s.get("tuners", [])
        if t.get("enabled") and t.get("site") and int(t.get("pool_size") or 0) > 0]
ok = [t for t in want if t.get("locked")]
up = s.get("uptime_s", 0)
if want and len(ok) == len(want):
    print("OK %d of %d site tuners locked" % (len(ok), len(want)))
elif not want and up >= 20:
    print("OK no site tuners; up %ds" % up)
else:
    print("WAIT %d of %d site tuners locked" % (len(ok), len(want)))
' <<<"$body") || verdict="WAIT unreadable status"
        last="${verdict#* }"
        if [[ "$verdict" == OK* ]]; then log "verified: $last"; return 0; fi
      else
        last="panel not answering at $url"
      fi
    fi
    sleep 3
  done
  log "not verified after ${LOCK_TIMEOUT}s: $last"
  return 1
}

# ---------------------------------------------------------------- 1. decide
current=$(installed_version)
deployed_commit=$(awk '$3 != "-" {print $3}' "$STATE_DIR/deployed" 2>/dev/null || true)
log "installed: trunkd $current${deployed_commit:+ ($deployed_commit)}; source: $SOURCE"

target_version="" target_commit="" asset_url="" sums_url=""
case "$SOURCE" in
  release)
    arch=$(uname -m)
    [[ "$arch" == x86_64 ]] || die "no published Linux build for $arch; use SOURCE=git"
    json=$(curl -fsSL --max-time 30 -H 'Accept: application/vnd.github+json' \
             "https://api.github.com/repos/$GITHUB_REPO/releases/latest") \
      || die "could not query the latest release of $GITHUB_REPO"
    { read -r tag; read -r asset_url; read -r sums_url; } < <(python3 -I -c '
import json, sys
r = json.loads(sys.stdin.read())
tag = r.get("tag_name", "")
assets = {a["name"]: a["browser_download_url"] for a in r.get("assets", [])}
name = "trunkd-%s-x86_64-unknown-linux-gnu.tar.gz" % tag
print(tag); print(assets.get(name, "")); print(assets.get("SHA256SUMS", ""))
' <<<"$json")
    target_version=${tag#v}
    [[ -n "$target_version" ]] || die "latest release has no tag"
    [[ -n "$asset_url" && -n "$sums_url" ]] || die "release $tag has no Linux tarball or SHA256SUMS"
    ;;
  git)
    command -v git >/dev/null || die "missing tool: git"
    src="$STATE_DIR/src"
    if [[ ! -d "$src/.git" ]]; then
      log "cloning $GIT_URL"
      git clone --quiet --no-checkout "$GIT_URL" "$src"
    fi
    git -C "$src" remote set-url origin "$GIT_URL"
    git -C "$src" fetch --quiet --prune --tags --force origin
    if [[ "$GIT_REF" == latest-tag ]]; then
      ref=$(git -C "$src" tag -l 'v[0-9]*' --sort=-v:refname | head -1)
      [[ -n "$ref" ]] || die "no v* tags in $GIT_URL"
      target_commit=$(git -C "$src" rev-parse "$ref^{commit}")
    else
      target_commit=$(git -C "$src" rev-parse "origin/$GIT_REF^{commit}") || die "no branch $GIT_REF"
    fi
    target_version=$(git -C "$src" show "$target_commit:Cargo.toml" |
      sed -n '/^\[workspace.package\]/,/^\[/{s/^version *= *"\([^"]*\)".*/\1/p}' | head -1)
    [[ -n "$target_version" ]] || die "no [workspace.package] version at $target_commit"
    ;;
  *) die "SOURCE must be release or git, not '$SOURCE'" ;;
esac

wanted=0
if (( FORCE )); then
  wanted=1
elif ! pin_ok "$target_version"; then
  log "available $target_version is outside PIN=$PIN; staying on $current"
elif [[ "$SOURCE" == git && "$TRACK" == commit ]]; then
  [[ "$target_commit" != "$deployed_commit" ]] && wanted=1
else
  version_gt "$target_version" "$current" && wanted=1
fi
log "available: $target_version${target_commit:+ (${target_commit:0:12})}"

if (( ! wanted )); then log "up to date"; exit 0; fi
if (( CHECK_ONLY )); then log "update available"; exit 10; fi

# ---------------------------------------------------------------- 2. get the binary
stage="$STATE_DIR/stage"
rm -rf "$stage"; mkdir -p "$stage"
case "$SOURCE" in
  release)
    name="trunkd-v${target_version}-x86_64-unknown-linux-gnu"
    log "downloading $name.tar.gz"
    curl -fsSL --max-time 300 -o "$stage/$name.tar.gz" "$asset_url" || die "download failed"
    curl -fsSL --max-time 30 -o "$stage/SHA256SUMS" "$sums_url" || die "SHA256SUMS download failed"
    ( cd "$stage" && grep " $name.tar.gz\$" SHA256SUMS | sha256sum -c --quiet - ) \
      || die "checksum mismatch for $name.tar.gz"
    tar -xzf "$stage/$name.tar.gz" -C "$stage"
    new="$stage/$name/trunkd"
    ;;
  git)
    if [[ -z "$CARGO" ]]; then
      CARGO=$(command -v cargo || true)
      [[ -z "$CARGO" && -x "${HOME:-/root}/.cargo/bin/cargo" ]] && CARGO="${HOME:-/root}/.cargo/bin/cargo"
    fi
    [[ -x "$CARGO" ]] || die "cargo not found; set CARGO= in $CONF"
    PATH="$(dirname "$CARGO"):$PATH"
    export PATH
    # The updater owns this clone: a forced checkout is safe here.
    git -C "$src" -c advice.detachedHead=false checkout --quiet --force --detach "$target_commit"
    git -C "$src" clean -fdq -e target
    low=(nice -n 19)
    command -v ionice >/dev/null && low+=(ionice -c 3)
    log "building ${target_commit:0:12} with -j$BUILD_JOBS"
    ( cd "$src" && "${low[@]}" "$CARGO" build --release --locked -j "$BUILD_JOBS" -p trunkd ) \
      || die "build failed"
    if [[ "$RUN_TESTS" == 1 ]]; then
      log "testing"
      ( cd "$src" && "${low[@]}" "$CARGO" test --release --locked -j "$BUILD_JOBS" --workspace ) \
        || die "tests failed; the running recorder was not touched"
    fi
    cp "$src/target/release/trunkd" "$stage/trunkd"
    new="$stage/trunkd"
    ;;
esac
chmod 0755 "$new"
got=$("$new" --version | awk '{print $2; exit}')
[[ "$got" == "$target_version" ]] || die "new binary reports $got, expected $target_version"

# ---------------------------------------------------------------- 3. pre-flight
log "pre-flight: the new binary parses $TRUNKD_CONFIG"
chmod 0755 "$STATE_DIR" "$stage" "$(dirname "$new")"
if ! runuser -u "$RUN_AS" -- "$new" --config "$TRUNKD_CONFIG" check >"$stage/check.log" 2>&1; then
  tail -20 "$stage/check.log"
  die "the new build rejects the current config; nothing installed"
fi

if (( STAGE_ONLY )); then
  log "staged $new ($target_version); not installed (--stage-only)"
  exit 2
fi

# ---------------------------------------------------------------- 4. install + verify
sha=$(sha256sum "$new" | awk '{print $1}')
[[ -x "$BIN" ]] && cp -p "$BIN" "$BIN.prev"
install -m 0755 "$new" "$BIN.new" && mv -f "$BIN.new" "$BIN"
log "installed $target_version; restarting $SERVICE"
systemctl restart "$SERVICE"
if wait_locked; then
  printf '%s %s %s %s %s\n' "$SOURCE" "$target_version" "${target_commit:--}" "$sha" \
    "$(date -u +%FT%TZ)" >"$STATE_DIR/deployed"
  log "updated $current -> $target_version"
  exit 0
fi

# ---------------------------------------------------------------- 5. rollback
if (( NO_ROLLBACK )) || [[ ! -x "$BIN.prev" ]]; then
  die "new build did not verify; left in place (--no-rollback or no previous binary)"
fi
log "rolling back to $current"
install -m 0755 "$BIN.prev" "$BIN.new" && mv -f "$BIN.new" "$BIN"
systemctl restart "$SERVICE"
if wait_locked; then die "rolled back to $current; tuners locked again"; fi
die "rolled back to $current, and tuners are STILL NOT LOCKED - check the recorder"
