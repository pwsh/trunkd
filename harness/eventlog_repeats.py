#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Read (and reproduce) the event log's repeat records.

Since 2026-09-23 trunkd writes a constant control-channel broadcast that it has
already written in full within the last minute as a one-line back-reference
instead of the whole record (`crates/trunkd/src/pipeline/eventlog.rs`,
"Repeats"; `docs/operations.md`, "The raw event log"):

    {"at":"2026-09-21T00:00:00.504Z","id":3,"ch":"control",…,"type":"tsbk"}
    {"at":"2026-09-21T00:00:01.991Z","rep":3}

The full record carries an extra `"id"`, always written straight after `at`;
each repeat is exactly `{"at", "rep"}`, where `rep` names the nearest
*preceding* line that carried that id. Every repeat
keeps its own line and its own `at`, so nothing is lost: `expand()` turns a
coalesced log back into the byte-identical lines the recorder used to write,
in the same order. That is what keeps every offline analysis unchanged — the
scanners in this directory read the log through `expand()`, and on a log from
before the change (no ids, no repeats) it passes every line through untouched.

`coalesce()` is a reference implementation of the recorder's writer, used to
check the round trip on a real day and to measure what coalescing saves.

    python3 harness/eventlog_repeats.py --check events/2026-09-21.jsonl.gz
    python3 harness/eventlog_repeats.py --coalesce IN.jsonl.gz OUT.jsonl.gz
    python3 harness/eventlog_repeats.py --expand IN.jsonl.gz OUT.jsonl.gz

Stdlib only.
"""

from __future__ import annotations

import argparse
import datetime as dt
import gzip
import json
import re
import sys

# The recorder's window: a repeat may only refer to a full copy written no
# more than this far (by `at`) from it, so a reader starting anywhere in a day
# meets a full copy of every constant broadcast at least once a minute.
WINDOW_MS = 60_000
# Most distinct broadcasts the recorder remembers at once (`REPEAT_TABLE_CAP`):
# past it the stale ones are forgotten, and all of them if that is not enough.
TABLE_CAP = 4096

# What the recorder coalesces. Mirrors `nonvoice::is_constant_broadcast`.
# Standard (MFID 0/1) TSBK opcodes that announce the site itself and carry no
# subscriber: SNDCP data-channel announcement, SCCB explicit, TDMA sync, the
# three band-plan updates, time/date, system services, secondary control,
# RFSS status, network status, adjacent site (and its unconfirmed form).
STANDARD_BROADCASTS = frozenset({22, 41, 48, 51, 52, 53, 56, 57, 58, 59, 60, 61, 62})
# L3Harris (MFID 0xA4): the two beacons and the regroup broadcast.
HARRIS_BROADCASTS = frozenset({1, 15, 48})
MFID_HARRIS = 0xA4

_FULL = re.compile(r'^(\{"at":"(?:[^"\\]|\\.)*"),"id":(\d+)(?=[,}])')
_REP = re.compile(r'^\{"at":("(?:[^"\\]|\\.)*"),"rep":(\d+)\}\s*$')
_AT = re.compile(r'^\{"at":"(?:[^"\\]|\\.)*"')


def _open(path, mode):
    p = str(path)
    return gzip.open(p, mode) if p.endswith(".gz") else open(p, mode)


def expand(lines):
    """Yield `lines` with every repeat replaced by the record it repeats.

    A full record's `"id"` is removed, so the output is exactly what the
    recorder wrote before repeats existed. Lines that are neither pass through
    unchanged (and a repeat whose id was never defined — a file cut in the
    middle — is passed through as it is, for the caller to skip as an
    unknown shape)."""
    full = {}                       # id -> the line after its {"at":"…"
    for line in lines:
        if line.startswith('{"at":"'):
            if ',"rep":' in line:
                m = _REP.match(line)
                if m:
                    rest = full.get(int(m.group(2)))
                    if rest is not None:
                        yield '{"at":' + m.group(1) + rest
                        continue
            elif ',"id":' in line:
                m = _FULL.match(line)
                if m:
                    rest = line[m.end():]
                    full[int(m.group(2))] = rest
                    line = m.group(1) + rest
        yield line


def repeatable(d):
    """Whether the recorder may coalesce this record's exact repeats."""
    ty = d.get("type")
    if ty == "site_broadcast":
        return True
    if ty != "tsbk":
        return False
    mfid, op = d.get("mfid"), d.get("opcode")
    if mfid in (0, 1):
        return op in STANDARD_BROADCASTS
    return mfid == MFID_HARRIS and op in HARRIS_BROADCASTS


def at_ms(at):
    try:
        return int(dt.datetime.fromisoformat(at.replace("Z", "+00:00"))
                   .timestamp() * 1000)
    except (ValueError, AttributeError):
        return None


def coalesce(lines, window_ms=WINDOW_MS):
    """The recorder's writer, over already-written lines of one file."""
    seen = {}                       # content -> [id, at_ms of the full copy]
    next_id = 0
    for line in lines:
        a = _AT.match(line)
        if not a:
            yield line
            continue
        try:
            d = json.loads(line)
        except ValueError:
            yield line
            continue
        if not repeatable(d):
            yield line
            continue
        t = at_ms(d.get("at"))
        content = line[a.end():]
        hit = seen.get(content)
        if hit is not None and t is not None and abs(t - hit[1]) <= window_ms:
            yield line[:a.end()] + ',"rep":%d}\n' % hit[0]
            continue
        if hit is None:
            if len(seen) >= TABLE_CAP:
                seen = {k: v for k, v in seen.items()
                        if abs((t or 0) - v[1]) <= window_ms}
                if len(seen) >= TABLE_CAP:
                    seen = {}
            ident = next_id
            next_id += 1
        else:
            ident = hit[0]
        seen[content] = [ident, t if t is not None else 0]
        yield line[:a.end()] + ',"id":%d' % ident + content


class _Sizes:
    """A write-only sink that only counts, for gzip sizes without the bytes."""

    def __init__(self):
        self.n = 0

    def write(self, b):
        self.n += len(b)
        return len(b)

    def flush(self):
        pass


def _check(path):
    """Coalesce a day, expand it again and compare with the day line by line,
    streaming (a day is half a gigabyte), counting sizes on the way."""
    sizes = {"raw": 0, "new": 0, "lines": 0, "reps": 0}
    gz_orig, gz_new = _Sizes(), _Sizes()
    zo = gzip.GzipFile(fileobj=gz_orig, mode="wb", compresslevel=6, mtime=0)
    zn = gzip.GzipFile(fileobj=gz_new, mode="wb", compresslevel=6, mtime=0)

    def counted(src, key, z):
        for ln in src:
            b = ln.encode()
            sizes[key] += len(b)
            z.write(b)
            if key == "new" and ln.startswith('{"at":"') and ',"rep":' in ln:
                sizes["reps"] += 1
            yield ln

    same = True
    with _open(path, "rt") as a, _open(path, "rt") as b:
        back = expand(counted(coalesce(counted(a, "raw", zo)), "new", zn))
        for x, y in zip(back, b):
            sizes["lines"] += 1
            if x != y:
                if same:
                    print(f"  first difference at line {sizes['lines']}:\n"
                          f"    {y!r}\n    {x!r}")
                same = False
        same = same and next(back, None) is None and next(b, None) is None
    zo.close()
    zn.close()
    raw, comp, reps = sizes["raw"], sizes["new"], sizes["reps"]
    print(f"{path}: {sizes['lines']:,} lines, {reps:,} written as repeats "
          f"({reps / max(1, sizes['lines']) * 100:.1f} %)")
    print(f"  uncompressed {raw:,} -> {comp:,} bytes "
          f"({comp / max(1, raw) * 100:.1f} %)")
    print(f"  gzip -6      {gz_orig.n:,} -> {gz_new.n:,} bytes "
          f"({gz_new.n / max(1, gz_orig.n) * 100:.1f} %)")
    print(f"  expand(coalesce(day)) == day, byte for byte: {same}")
    return same


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    g = ap.add_mutually_exclusive_group(required=True)
    g.add_argument("--check", metavar="LOG",
                   help="coalesce a day in memory, report the saving and "
                        "verify it expands back byte for byte")
    g.add_argument("--coalesce", nargs=2, metavar=("IN", "OUT"))
    g.add_argument("--expand", nargs=2, metavar=("IN", "OUT"))
    args = ap.parse_args()
    if args.check:
        return 0 if _check(args.check) else 1
    src, dst = args.coalesce or args.expand
    fn = coalesce if args.coalesce else expand
    with _open(src, "rt") as fi, _open(dst, "wt") as fo:
        for ln in fn(fi):
            fo.write(ln)
    return 0


if __name__ == "__main__":
    sys.exit(main())
