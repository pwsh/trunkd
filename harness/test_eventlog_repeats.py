#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Check that the event log's repeat records change no offline analysis.

Two parts:

* a self-contained test of `eventlog_repeats` (runs anywhere): what may and
  may not become a repeat, the window, and the byte-exact round trip;
* the analyses themselves, on a real day: a slice of an `events/*.jsonl.gz`
  (default: the newest; `--lines 0` for the whole day) is written out twice —
  as recorded, and coalesced the way the recorder now writes it — and the
  per-file scan of every analysis script that reads the log is run over both.
  The scan result is everything each report is built from, so equal scans
  mean equal reports. Those scripts are site-specific and live outside the
  public tree (a directory named by $TRUNKD_SCANNER_DIR); when none is present
  this part is skipped.

    python3 harness/test_eventlog_repeats.py [--log events/YYYY-MM-DD.jsonl.gz] [--lines 300000]

Exit status 0 when everything matches. Stdlib only.
"""

from __future__ import annotations

import argparse
import gzip
import json
import os
import sys
import tempfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

import eventlog_repeats as er            # noqa: E402  (path set above)

RFSS = ('{"at":"%s","ch":"control","fields":{},"hex":"ba000131230101103e700000",'
        '"mfid":0,"name":"RFSS_STATUS_BCST","opcode":58,"type":"tsbk"}\n')
GRANT_UPD = ('{"at":"%s","ch":"control","fields":{},"hex":"820010910bb910910bb90000",'
             '"mfid":0,"name":"GRP_VCH_GRNT_UPD","opcode":2,"type":"tsbk"}\n')
BCAST = ('{"at":"%s","ch":"control","kind":"rfss_status",'
         '"name":"RFSS_STATUS_BCST","type":"site_broadcast"}\n')


def unit():
    t = lambda s: "2026-09-21T00:%s" % s       # noqa: E731
    day = [
        RFSS % t("00:00Z"), BCAST % t("00:00Z"), GRANT_UPD % t("00:00.1Z"),
        RFSS % t("00:01Z"), BCAST % t("00:01Z"), GRANT_UPD % t("00:01.1Z"),
        RFSS % t("01:00Z"), RFSS % t("01:00.001Z"), RFSS % t("01:01Z"),
    ]
    out = list(er.coalesce(day))
    want = [
        '{"at":"%s","id":0,"ch":' % t("00:00Z"),
        '{"at":"%s","id":1,"ch":' % t("00:00Z"),
        GRANT_UPD % t("00:00.1Z"),                   # never a repeat
        '{"at":"%s","rep":0}\n' % t("00:01Z"),
        '{"at":"%s","rep":1}\n' % t("00:01Z"),
        GRANT_UPD % t("00:01.1Z"),
        '{"at":"%s","rep":0}\n' % t("01:00Z"),        # exactly 60 s: inside
        '{"at":"%s","id":0,"ch":' % t("01:00.001Z"),  # past it: full, same id
        '{"at":"%s","rep":0}\n' % t("01:01Z"),
    ]
    ok = True
    for i, (got, w) in enumerate(zip(out, want)):
        if not got.startswith(w):
            print(f"FAIL unit line {i}: {got!r} does not start {w!r}")
            ok = False
    if list(er.expand(out)) != day:
        print("FAIL unit: expand(coalesce(day)) != day")
        ok = False
    # A log written before repeats existed passes through untouched.
    if list(er.expand(day)) != day:
        print("FAIL unit: expand changed a log without repeats")
        ok = False
    print("unit:", "ok" if ok else "FAILED")
    return ok


def _plain(x):
    """JSON-comparable form of a scan result (sets become sorted lists)."""
    if isinstance(x, dict):
        return {str(k): _plain(v) for k, v in x.items()}
    if isinstance(x, (list, tuple)):
        return [_plain(v) for v in x]
    if isinstance(x, (set, frozenset)):
        return sorted(_plain(v) for v in x)
    return x


# Optional, site-specific analysis scripts (not shipped publicly): a directory
# named by $TRUNKD_SCANNER_DIR, else a local-only `private/harness/` beside the
# checkout. Every module there that exposes a per-file scan is compared.
SCANNER_DIR = Path(os.environ.get("TRUNKD_SCANNER_DIR")
                   or HERE.parent / "private" / "harness")


def _bind(m):
    """A one-file scan from module `m`, or None if it exposes none.

    `scan_file(path, [verbose])` is used as is; otherwise `scan_events(paths,
    ...)` whose first parameter takes a list of files is called with just this
    file and None for its other required arguments. `verbose` is turned off."""
    import inspect
    for fn_name in ("scan_file", "scan_events"):
        fn = getattr(m, fn_name, None)
        if not callable(fn):
            continue
        params = list(inspect.signature(fn).parameters.values())
        kw = {"verbose": False} if any(q.name == "verbose" for q in params) else {}
        if fn_name == "scan_file":
            return lambda p, fn=fn, kw=kw: fn(p, **kw)
        if params and params[0].name == "paths":
            rest = [None for q in params[1:]
                    if q.default is inspect.Parameter.empty and q.name != "verbose"]
            return lambda p, fn=fn, kw=kw, rest=rest: fn([p], *rest, **kw)
    return None


def scanners():
    """The per-file scans of whichever analysis scripts are present."""
    import importlib
    if not SCANNER_DIR.is_dir():
        return {}
    if str(SCANNER_DIR) not in sys.path:
        sys.path.insert(1, str(SCANNER_DIR))
    out = {}
    for f in sorted(SCANNER_DIR.glob("*.py")):
        scan = _bind(importlib.import_module(f.stem))
        if scan is not None:
            out[f.stem] = scan
    return out


def real(log: Path, limit: int):
    with tempfile.TemporaryDirectory(prefix="eventlog-repeats-") as tmp:
        a = Path(tmp) / "recorded" / log.name
        b = Path(tmp) / "coalesced" / log.name
        a.parent.mkdir()
        b.parent.mkdir()
        n = 0
        with gzip.open(log, "rt") as src, gzip.open(a, "wt") as fa:
            for ln in src:
                if limit and n >= limit:
                    break
                fa.write(ln)
                n += 1
        with gzip.open(a, "rt") as fa, gzip.open(b, "wt") as fb:
            for ln in er.coalesce(fa):
                fb.write(ln)
        size = lambda p: sum(len(x.encode()) for x in gzip.open(p, "rt"))  # noqa: E731
        print(f"{log}: first {n:,} lines, {size(a):,} -> {size(b):,} bytes "
              f"uncompressed, {a.stat().st_size:,} -> {b.stat().st_size:,} gzipped")
        with gzip.open(a, "rt") as fa, gzip.open(b, "rt") as fb:
            ok = list(er.expand(fb)) == list(fa)
        print(f"  expand(coalesce(day)) == day: {ok}")
        scans = scanners()
        if not scans:
            print("  no analysis scripts present; skipping the scan comparison")
        for name, scan in scans.items():
            x = json.dumps(_plain(scan(a)), sort_keys=True)
            y = json.dumps(_plain(scan(b)), sort_keys=True)
            same = x == y
            ok &= same
            print(f"  {name:18s} scan identical: {same} ({len(x):,} bytes of result)")
        return ok


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--log", type=Path,
                    help="a day of event log (default: newest events/*.jsonl.gz)")
    ap.add_argument("--lines", type=int, default=300_000,
                    help="lines of it to use; 0 for all (default 300000)")
    args = ap.parse_args()
    ok = unit()
    log = args.log
    if log is None:
        days = sorted((HERE.parent / "events").glob("????-??-??.jsonl.gz"))
        log = days[-1] if days else None
    if log is None:
        print("no event log to check the analyses against (events/ is empty)")
    else:
        ok &= real(log, args.lines)
    print("OK" if ok else "FAILED")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
