# Harness: offline event-log tools

Two small, stdlib-only Python tools for working with what trunkd writes, plus
the test that keeps one of them honest. None of them talks to a running
recorder, an SDR, or Keyup.

## `eventlog_repeats.py` — read and reproduce the event log's repeat records

The raw non-voice event log (`[recorder] event_log_dir`, one
`YYYY-MM-DD.jsonl[.gz]` per day) writes a constant control-channel broadcast
that already appeared in full within the last minute as a one-line
back-reference (`{"at": …, "rep": N}`) instead of the whole record.
`expand()` turns such a log back into the byte-identical lines a recorder
without coalescing would have written; `coalesce()` is a reference
implementation of the recorder's writer. Any script of your own that reads the
log should read it through `expand()`.

```
python3 harness/eventlog_repeats.py --check    events/YYYY-MM-DD.jsonl.gz
python3 harness/eventlog_repeats.py --coalesce IN.jsonl.gz OUT.jsonl.gz
python3 harness/eventlog_repeats.py --expand   IN.jsonl.gz OUT.jsonl.gz
```

## `nonvoice_histogram.py` — histogram decoded non-voice traffic

Counts the control- and traffic-channel messages the P25 engine decodes, by
message class, naming each opcode the way sdrtrunk does. It also decodes
standard LC opcode 9 (Source ID Extension) and the L3Harris talker-GPS LC pair.
Input is the JSONL printed by the `site_decode --raw-control` and
`channelizer_decode` examples of `trunkd-p25`. Background:
[`docs/NON-VOICE-DATA.md`](../docs/NON-VOICE-DATA.md).

```
python3 harness/nonvoice_histogram.py CONTROL.jsonl [TRAFFIC.jsonl ...] [--seconds 90]
```

## `test_eventlog_repeats.py` — round-trip test

```
python3 harness/test_eventlog_repeats.py [--log events/YYYY-MM-DD.jsonl.gz] [--lines 300000]
```

The self-contained part (what may become a repeat, the one-minute window, the
byte-exact `expand(coalesce(x)) == x` round trip) runs anywhere. If an event
log is available (default: the newest file in `events/`), the test also
coalesces a slice of it and checks the round trip on real data. Exit status 0
means everything matched.
