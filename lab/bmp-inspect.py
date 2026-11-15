#!/usr/bin/env python3
"""Read a raw BMPv3 stream and report its message structure.

This reader is deliberately independent of bgpviews. It exists so the lab can
prove that the file written by capture.sh is a well-formed BMPv3 stream and
that it carries the message types the lab promises, without needing a live
router or the bgpviews binary.

It walks the 6-byte BMP common header (version u8, length u32 big-endian
including the header, type u8), prints one line per message, and prints
per-type counts. Where the payload is cheap to decode it also prints the
Initiation identities, the per-peer header, and the NLRI of an encapsulated
BGP UPDATE. Payload decoding is best effort: a decode failure is reported as
a note and does not invalidate an otherwise well-framed stream.

Exit status is 0 when the stream walks cleanly to EOF and every type named by
--require is present, 1 otherwise.
"""

from __future__ import annotations

import argparse
import ipaddress
import struct
import sys

TYPE_NAMES = {
    0: "Route Monitoring",
    1: "Statistics Report",
    2: "Peer Down",
    3: "Peer Up",
    4: "Initiation",
    5: "Termination",
    6: "Route Mirroring",
}

INITIATION_TLVS = {
    0: "string",
    1: "sysDescr",
    2: "sysName",
    3: "sysLocation",
    4: "sysContact",
}

# Per-peer header flag bits for peer types 0, 1 and 2 (RFC 7854 section 4.2).
FLAG_V = 0x80  # peer address is IPv6
FLAG_L = 0x40  # post-policy Adj-RIB-In
FLAG_A = 0x20  # legacy 2-byte AS_PATH
FLAG_O = 0x10  # Adj-RIB-Out

BGP_TYPE_NAMES = {
    1: "OPEN",
    2: "UPDATE",
    3: "NOTIFICATION",
    4: "KEEPALIVE",
    5: "ROUTE-REFRESH",
}


def fmt_prefix(addr: bytes, plen: int, addr_len: int) -> str:
    nbytes = (plen + 7) // 8
    raw = addr[:nbytes] + b"\x00" * (addr_len - nbytes)
    if addr_len == 4:
        return f"{ipaddress.IPv4Address(raw)}/{plen}"
    return f"{ipaddress.IPv6Address(raw).compressed}/{plen}"


def parse_nlri(data: bytes, addr_len: int) -> list[str]:
    out = []
    i = 0
    while i < len(data):
        plen = data[i]
        i += 1
        nbytes = (plen + 7) // 8
        if i + nbytes > len(data) or plen > addr_len * 8:
            raise ValueError("truncated or overlong NLRI")
        out.append(fmt_prefix(data[i : i + nbytes], plen, addr_len))
        i += nbytes
    return out


def decode_bgp_update(msg: bytes, as_width: int) -> dict[str, list[str]]:
    """Best-effort decode of the NLRI and AS_PATH in a BGP UPDATE."""
    if len(msg) < 19 or msg[18] != 2:
        raise ValueError("not a BGP UPDATE")

    body = msg[19:]
    if len(body) < 4:
        raise ValueError("short UPDATE body")

    withdrawn_len = struct.unpack_from("!H", body, 0)[0]
    if 2 + withdrawn_len + 2 > len(body):
        raise ValueError("bad withdrawn-routes length")
    withdrawn_raw = body[2 : 2 + withdrawn_len]
    attr_pos = 2 + withdrawn_len
    attr_len = struct.unpack_from("!H", body, attr_pos)[0]
    attrs = body[attr_pos + 2 : attr_pos + 2 + attr_len]
    nlri = body[attr_pos + 2 + attr_len :]

    result = {
        "announced_v4": [],
        "announced_v6": [],
        "withdrawn_v4": [],
        "withdrawn_v6": [],
        "as_path": [],
    }
    if withdrawn_raw:
        result["withdrawn_v4"] = parse_nlri(withdrawn_raw, 4)

    i = 0
    while i + 3 <= len(attrs):
        flags = attrs[i]
        atype = attrs[i + 1]
        if flags & 0x10:  # extended length
            if i + 4 > len(attrs):
                raise ValueError("short extended attribute")
            alen = struct.unpack_from("!H", attrs, i + 2)[0]
            hdr = 4
        else:
            alen = attrs[i + 2]
            hdr = 3
        val = attrs[i + hdr : i + hdr + alen]
        if len(val) != alen:
            raise ValueError("truncated attribute")

        if atype == 2 and alen >= 2:
            # AS_PATH: (segment type, count, AS numbers). The AS width comes
            # from the BMP per-peer header, not from this attribute's flags.
            j = 0
            segments = []
            while j + 2 <= len(val):
                count = val[j + 1]
                j += 2
                asns = []
                for _ in range(count):
                    if j + as_width > len(val):
                        raise ValueError("truncated AS_PATH")
                    asns.append(int.from_bytes(val[j : j + as_width], "big"))
                    j += as_width
                segments.append(" ".join(str(a) for a in asns))
            result["as_path"] = segments
        elif atype == 14 and alen >= 5:
            afi = struct.unpack_from("!H", val, 0)[0]
            nexthop_len = val[3]
            if 5 + nexthop_len > len(val):
                raise ValueError("truncated MP_REACH")
            if afi == 2:
                result["announced_v6"] = parse_nlri(val[5 + nexthop_len :], 16)
        elif atype == 15 and alen >= 3:
            afi = struct.unpack_from("!H", val, 0)[0]
            if afi == 2:
                result["withdrawn_v6"] = parse_nlri(val[3:], 16)

        i += hdr + alen

    if nlri:
        result["announced_v4"] = parse_nlri(nlri, 4)
    return result


def parse_open_my_as(msg: bytes) -> int:
    if len(msg) < 21 or msg[18] != 1:
        raise ValueError("not a BGP OPEN")
    return struct.unpack_from("!H", msg, 20)[0]


def decode_initiation(body: bytes) -> list[str]:
    out = []
    i = 0
    while i + 4 <= len(body):
        tlv_type, tlv_len = struct.unpack_from("!HH", body, i)
        val = body[i + 4 : i + 4 + tlv_len]
        if len(val) != tlv_len:
            break
        label = INITIATION_TLVS.get(tlv_type, f"tlv{tlv_type}")
        out.append(f"{label}={val.decode('utf-8', 'replace')}")
        i += 4 + tlv_len
    return out


def parse_per_peer(header: bytes) -> dict:
    if len(header) < 42:
        raise ValueError("short per-peer header")
    peer_type = header[0]
    flags = header[1]
    peer_as = struct.unpack_from("!I", header, 26)[0]
    if peer_type == 3:
        peer_ip = "0.0.0.0"
        as_width = 4
    else:
        raw = header[10:26]
        if flags & FLAG_V:
            peer_ip = ipaddress.IPv6Address(raw).compressed
        else:
            peer_ip = str(ipaddress.IPv4Address(raw[12:]))
        as_width = 2 if flags & FLAG_A else 4
    return {
        "type": peer_type,
        "flags": flags,
        "peer_ip": peer_ip,
        "peer_as": peer_as,
        "as_width": as_width,
        "post_policy": bool(flags & FLAG_L),
        "adj_out": bool(flags & FLAG_O),
    }


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("path", help="raw BMPv3 byte stream")
    ap.add_argument(
        "--require",
        default="",
        help="comma-separated BMP message types that must be present, e.g. 0,3,4",
    )
    args = ap.parse_args()

    with open(args.path, "rb") as fh:
        data = fh.read()

    print(f"file={args.path} bytes={len(data)}")
    if not data:
        print("FAIL: empty file")
        return 1

    print(f"first_byte=0x{data[0]:02x}")
    if data[0] != 3:
        print("FAIL: first byte is not 0x03 (BMP version 3)")
        return 1

    counts: dict[int, int] = {}
    off = 0
    while off < len(data):
        if len(data) - off < 6:
            print(f"FAIL: {len(data) - off} trailing byte(s) at offset {off}, short common header")
            return 1

        version, length, mtype = struct.unpack_from("!BIB", data, off)
        if version != 3:
            print(f"FAIL: offset {off} has BMP version {version}, expected 3")
            return 1
        if length < 6:
            print(f"FAIL: offset {off} declares length {length} < 6")
            return 1
        if off + length > len(data):
            print(
                f"FAIL: offset {off} declares length {length}, "
                f"only {len(data) - off} bytes remain"
            )
            return 1

        body = data[off + 6 : off + length]
        detail = ""
        try:
            if mtype == 4:
                parts = decode_initiation(body)
                if parts:
                    detail = " " + " ".join(parts)
            elif mtype in (0, 2, 3):
                peer = parse_per_peer(body)
                detail = (
                    f" peer={peer['peer_ip']} peer_as={peer['peer_as']}"
                    f" post_policy={peer['post_policy']} adj_out={peer['adj_out']}"
                )
                if mtype == 3:
                    local_as = parse_open_my_as(body[42 + 16 + 4 :])
                    detail += f" local_as={local_as}"
                elif mtype == 0:
                    bgp = body[42:]
                    if len(bgp) >= 19:
                        bgp_type = BGP_TYPE_NAMES.get(bgp[18], str(bgp[18]))
                        detail += f" bgp={bgp_type}"
                        if bgp[18] == 2:
                            update = decode_bgp_update(bgp, peer["as_width"])
                            for key in (
                                "announced_v4",
                                "announced_v6",
                                "withdrawn_v4",
                                "withdrawn_v6",
                            ):
                                if update[key]:
                                    detail += f" {key}={','.join(update[key])}"
                            if update["as_path"]:
                                detail += f" as_path={','.join(update['as_path'])}"
        except Exception as exc:  # noqa: BLE001 - decode is best effort
            detail = f" (decode note: {exc})"

        name = TYPE_NAMES.get(mtype, "Unknown")
        print(f"{off:6d}  version={version}  type={mtype} ({name})  length={length}{detail}")
        counts[mtype] = counts.get(mtype, 0) + 1
        off += length

    print(f"walked={sum(counts.values())} messages end_offset={off} eof={len(data)}")
    print("counts=" + ",".join(f"{t}:{counts[t]}" for t in sorted(counts)))

    required = [int(x) for x in args.require.split(",") if x.strip()]
    missing = [t for t in required if t not in counts]
    if missing:
        print(
            "MISSING required types: "
            + ",".join(f"{t}({TYPE_NAMES.get(t, '?')})" for t in missing)
        )
        return 1
    if required:
        print("required types present: " + ",".join(str(t) for t in required))
    print("OK: BMPv3 stream walks cleanly to EOF")
    return 0


if __name__ == "__main__":
    sys.exit(main())
