#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Histogram the *non-voice* traffic the P25 engine decodes, by message class.

Input is the JSONL the two decode examples already print:

* ``site_decode --raw-control`` — every control-channel ``P25Event`` as a
  ``{"type":"control","event":"<rust Debug>"}`` line, plus the call events.
* ``channelizer_decode`` (without ``--quiet``) — every traffic-channel event as
  typed JSON, including the raw Link Control / TDULC words in hex.

It names each opcode the way sdrtrunk's ``Opcode`` / ``LinkControlOpcode``
enums do, and decodes two things the engine currently leaves as ``Other``:

* standard LC **opcode 9** (Source ID Extension) — the fully qualified
  ``WACN.SYSTEM.ID`` of a roaming radio;
* L3Harris LC **opcodes 42/43** (Talker GPS block 1/2), per sdrtrunk's
  ``L3HarrisGPS`` field layout, when both blocks are present.

Usage:
    python3 harness/nonvoice_histogram.py CONTROL.jsonl [TRAFFIC.jsonl ...] \\
        [--seconds 90]

This is an analysis helper for docs/NON-VOICE-DATA.md, not a test.
"""

from __future__ import annotations

import argparse
import collections
import json
import re
import sys

# --- sdrtrunk name tables ------------------------------------------------

OSP = {
    0: "GRP_VCH_GRANT (group voice channel grant)",
    2: "GRP_VCH_GRNT_UPD (group voice grant update)",
    3: "GRP_VCH_GRNT_UPX (group voice grant update explicit)",
    4: "UU_VCH_GRANT (unit-to-unit grant)",
    5: "UU_ANS_REQ (unit-to-unit answer request)",
    6: "UU_VCH_GRANT_UPD (unit-to-unit grant update)",
    8: "TEL_INT_VCH_GRNT (telephone interconnect grant)",
    9: "TEL_INT_VCH_GRNU (telephone interconnect grant update)",
    10: "TEL_INT_ANS_RQST (telephone answer request)",
    16: "IND_DCH_GRANT (individual data channel grant, obsolete)",
    17: "GRP_DCH_GRANT (group data channel grant, obsolete)",
    18: "GRP_DCH_ANNOUNCE (group data announcement, obsolete)",
    19: "GRP_DCH_ANNC_EXP (group data announcement explicit, obsolete)",
    20: "SNDCP_DCH_GRANT (SNDCP data channel grant)",
    21: "SNDCP_DCH_PAG_RQ (SNDCP data page request)",
    22: "SNDCP_DCH_ANN_EX (SNDCP data channel announcement explicit)",
    24: "STATUS_UPDATE",
    26: "STATUS_QUERY",
    28: "MESSAGE_UPDATE",
    29: "RADIO_MONITR_CMD (radio unit monitor command)",
    30: "RADIO UNIT MONITOR ENHANCED COMMAND",
    31: "CALL_ALERT",
    32: "ACK_RESPONSE_FNE (acknowledge response)",
    33: "QUEUED_RESPONSE",
    36: "EXTNDED_FUNC_CMD (extended function command)",
    39: "DENY_RESPONSE",
    40: "GRP_AFFIL_RESP (group affiliation response)",
    41: "SCCB_CCH_BCST_EX (secondary control channel broadcast explicit)",
    42: "GRP_AFFIL_QUERY (group affiliation query)",
    43: "LOCN_REG_RESPONS (location registration response)",
    44: "UNIT_REG_RESPONS (unit registration response)",
    45: "UNIT_REG_COMMAND (unit registration command)",
    46: "AUTH_COMMAND (authentication command)",
    47: "DE_REGIST_ACK (unit de-registration acknowledge)",
    48: "TDMA_SYNC_BCST (TDMA synchronization broadcast)",
    49: "AUTH_DEMAND",
    50: "AUTH_FNE_RESP",
    51: "IDEN_UPDATE_TDMA",
    52: "IDEN_UPDATE_VUHF",
    53: "TIME_DATE_ANNOUN (time and date announcement)",
    54: "ROAM_ADDR_CMD (roaming address command)",
    55: "ROAM_ADDR_UPDATE (roaming address update)",
    56: "SYS_SVC_BCAST (system service broadcast)",
    57: "SEC_CCH_BROADCST (secondary control channel broadcast)",
    58: "RFSS_STATUS_BCST",
    59: "NET_STATUS_BCAST",
    60: "ADJ SITE STATUS (adjacent site status)",
    61: "IDEN_UPDATE",
    62: "ADJ_STS_BCST_UNC (adjacent status, uncoordinated band plan)",
}

HARRIS_OSP = {48: "HARRIS REGROUP (group regroup w/ explicit encryption)"}

LC = {
    0: "GROUP VOICE CHANNEL USER",
    2: "GROUP VOICE CHANNEL UPDATE",
    3: "UNIT-TO-UNIT VOICE CHANNEL USER",
    4: "GROUP VOICE CHANNEL UPDATE EXPLICIT",
    5: "UNIT-TO-UNIT ANSWER REQUEST",
    6: "TELEPHONE INTERCONNECT VOICE CHANNEL USER",
    7: "TELEPHONE INTERCONNECT ANSWER REQUEST",
    9: "SOURCE ID EXTENSION (fully qualified WACN.SYS.ID)",
    10: "UNIT-TO-UNIT VOICE CHANNEL USER EXTENDED",
    15: "CALL TERMINATION",
    16: "GROUP AFFILIATION QUERY",
    17: "UNIT REGISTRATION COMMAND",
    18: "UNIT AUTHENTICATION COMMAND",
    19: "STATUS QUERY",
    20: "STATUS UPDATE",
    21: "MESSAGE UPDATE",
    22: "CALL ALERT",
    23: "EXTENDED FUNCTION COMMAND",
    24: "CHANNEL IDENTIFIER UPDATE",
    25: "CHANNEL IDENTIFIER UPDATE EXPLICIT",
    26: "STATUS UPDATE-SOURCE ID REQUIRED",
    27: "MESSAGE UPDATE-SOURCE ID REQUIRED",
    28: "EXTENDED FUNCTION COMMAND-SOURCE ID REQUIRED",
    32: "SYSTEM SERVICE BROADCAST",
    33: "SECONDARY CONTROL CHANNEL BROADCAST",
    34: "ADJACENT SITE STATUS",
    35: "RFSS STATUS BROADCAST",
    36: "NET STATUS BROADCAST",
    37: "ENCRYPTION PARAMETERS BROADCAST",
    38: "SECONDARY CONTROL CHANNEL BROADCAST EXPLICIT",
    39: "ADJACENT SITE STATUS EXPLICIT",
    40: "RFSS STATUS BROADCAST EXPLICIT",
    41: "NETWORK STATUS BROADCAST EXPLICIT",
    42: "CONVENTIONAL FALLBACK INDICATION",
}

HARRIS_LC = {
    10: "L3HARRIS RETURN TO CONTROL CHANNEL",
    42: "L3HARRIS TALKER GPS BLOCK 1/2",
    43: "L3HARRIS TALKER GPS BLOCK 2/2",
    50: "L3HARRIS TALKER ALIAS BLOCK 1",
    51: "L3HARRIS TALKER ALIAS BLOCK 2",
    52: "L3HARRIS TALKER ALIAS BLOCK 3",
    53: "L3HARRIS TALKER ALIAS BLOCK 4",
}

# Which TSBK opcodes the trunkd engine turns into a typed message today
# (message.rs `parse_tsbk`); everything else lands in `TsbkMessage::Other`.
PARSED_OSP = {0, 2, 3, 4, 6, 8, 9, 51, 52, 57, 58, 59, 60, 61}
PARSED_LC = {0, 3, 6}

TSBK_RE = re.compile(r"opcode: (\d+), vendor: (\d+), message: ([A-Za-z]+)")


def bits(data: bytes, start: int, width: int) -> int:
    """MSB-first bit field [start, start+width) of a byte string."""
    v = 0
    for i in range(start, start + width):
        v = (v << 1) | ((data[i >> 3] >> (7 - (i & 7))) & 1)
    return v


def name_osp(opcode: int, vendor: int) -> str:
    if vendor == 164:
        return HARRIS_OSP.get(opcode, f"HARRIS OSP UNKNOWN OPCODE {opcode}")
    if vendor not in (0, 1):
        return f"VENDOR {vendor} OPCODE {opcode}"
    return OSP.get(opcode, f"OSP RESERVED/UNKNOWN {opcode}")


def decode_source_id_extension(word: bytes) -> str:
    """Standard LC opcode 9 — sdrtrunk LCSourceIDExtension."""
    return f"WACN:{bits(word, 16, 20)} SYSTEM:{bits(word, 36, 12)} ID:{bits(word, 48, 24)}"


def decode_harris_gps(block1: bytes, block2: bytes) -> str:
    """L3Harris talker GPS, blocks 1+2 (sdrtrunk L3HarrisGPS field layout).

    Each block contributes its octets 2..8 (bits 16..71); block 1 lands at
    bit 0 of the assembled message and block 2 at bit 56.
    """
    msg = bytearray(14)  # 112 bits

    def load(dst_bit: int, src: bytes) -> None:
        for i in range(56):
            bit = (src[(16 + i) >> 3] >> (7 - ((16 + i) & 7))) & 1
            if bit:
                msg[(dst_bit + i) >> 3] |= 1 << (7 - ((dst_bit + i) & 7))

    load(0, block1)
    load(56, block2)
    m = bytes(msg)

    def coord(deg_at, min_at, frac_at, hemi_at, frac_width):
        deg = bits(m, deg_at, 8)
        minutes = bits(m, min_at, 7)
        frac = bits(m, frac_at, frac_width)
        val = deg + (minutes + frac / 10000.0) / 60.0
        return -val if bits(m, hemi_at, 1) else val

    lat = coord(24, 17, 0, 16, 16)
    lon = coord(56, 49, 32, 48, 16)
    secs = bits(m, 64, 16) * (2 if bits(m, 80, 1) else 1)
    heading = bits(m, 95, 9)
    return f"lat {lat:.6f} lon {lon:.6f} heading {heading} t+{secs}s"


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("files", nargs="+")
    ap.add_argument("--seconds", type=float, default=0.0, help="capture length, for rates")
    args = ap.parse_args()

    tsbk = collections.Counter()
    ctl_types = collections.Counter()
    lc = collections.Counter()
    tdulc = collections.Counter()
    lsd = collections.Counter()
    hdu = collections.Counter()
    enc = collections.Counter()
    pdu = collections.Counter()
    suids = collections.Counter()
    gps_blocks: list[bytes] = []

    for path in args.files:
        for line in open(path):
            line = line.strip()
            if not line.startswith("{"):
                continue
            try:
                e = json.loads(line)
            except json.JSONDecodeError:
                continue
            t = e.get("type")
            if t == "control":  # site_decode raw pass-through, Rust Debug text
                ev = e["event"]
                kind = ev.split(" ", 1)[0]
                ctl_types[kind] += 1
                if kind == "Tsbk":
                    m = TSBK_RE.search(ev)
                    if m:
                        op, ven, variant = int(m[1]), int(m[2]), m[3]
                        tsbk[(ven, op, variant)] += 1
                continue
            if t == "tsbk":
                tsbk[(e["vendor"], e["opcode"], e["msg"])] += 1
            elif t in ("link_control", "tdulc"):
                word = bytes.fromhex(e["lc"])
                std_flag = (word[0] >> 6) & 1
                lco = word[0] & 0x3F
                vendor = 0 if std_flag else word[1]
                target = lc if t == "link_control" else tdulc
                target[(vendor, lco)] += 1
                if vendor in (0, 1) and lco == 9:
                    suids[decode_source_id_extension(word)] += 1
                if vendor == 164 and lco in (42, 43):
                    gps_blocks.append(word)
            elif t == "lsd":
                lsd[(e["data"], tuple(e["valid"]))] += 1
            elif t == "hdu":
                hdu[(e.get("valid"), e.get("talkgroup"), e.get("algorithm"), e.get("key_id"))] += 1
            elif t == "encryption_sync":
                enc[(e["algorithm"], e["key_id"])] += 1
            elif t in ("pdu_header", "pdu", "sndcp_context"):
                pdu[(t, e.get("format"), e.get("llid"), e.get("blocks"))] += 1

    def rate(n: int) -> str:
        return f"{n * 60.0 / args.seconds:8.1f}/min" if args.seconds else " " * 12

    print("== control-channel event kinds ==")
    for k, v in ctl_types.most_common():
        print(f"{v:7d} {rate(v)}  {k}")

    print("\n== TSBK opcodes (vendor/opcode, sdrtrunk name, engine variant) ==")
    for (ven, op, variant), v in sorted(tsbk.items(), key=lambda kv: -kv[1]):
        parsed = "parsed " if (ven in (0, 1) and op in PARSED_OSP) or (ven == 164 and op == 48) else "DROPPED"
        print(f"{v:7d} {rate(v)}  {parsed} mfid={ven:<4d} op={op:<3d} {name_osp(op, ven)}   [{variant}]")

    for title, table in (("LDU1 Link Control", lc), ("TDULC Link Control", tdulc)):
        if not table:
            continue
        print(f"\n== {title} ==")
        for (ven, lco), v in sorted(table.items(), key=lambda kv: -kv[1]):
            if ven == 164:
                nm = HARRIS_LC.get(lco, f"HARRIS LC UNKNOWN {lco}")
                parsed = "DROPPED"
            else:
                nm = LC.get(lco, f"LC RESERVED/UNKNOWN {lco}")
                parsed = "parsed " if lco in PARSED_LC else "DROPPED"
            print(f"{v:7d} {rate(v)}  {parsed} mfid={ven:<4d} lco={lco:<3d} {nm}")

    if suids:
        print("\n== fully qualified source SUIDs (LC opcode 9) ==")
        for k, v in suids.most_common():
            print(f"{v:7d}  {k}")

    if gps_blocks:
        print("\n== L3Harris talker GPS ==")
        b1 = [w for w in gps_blocks if (w[0] & 0x3F) == 42]
        b2 = [w for w in gps_blocks if (w[0] & 0x3F) == 43]
        print(f"  block1={len(b1)} block2={len(b2)}")
        for a, b in zip(b1, b2):
            print("  ", decode_harris_gps(a, b))

    if hdu:
        print("\n== HDU (valid, talkgroup, algorithm, key id) ==")
        for k, v in hdu.most_common():
            print(f"{v:7d}  {k}")
    if enc:
        print("\n== LDU2 encryption sync (algorithm, key id) ==")
        for k, v in enc.most_common():
            print(f"{v:7d}  {k}")
    if lsd:
        print("\n== low-speed data (bytes, per-octet FEC validity) ==")
        for k, v in lsd.most_common(10):
            print(f"{v:7d}  {k}")
    print("\n== PDU ==")
    if not pdu:
        print("  (none decoded)")
    for k, v in pdu.most_common():
        print(f"{v:7d}  {k}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
