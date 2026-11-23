#!/usr/bin/env python3
"""Browse DNS-SD (RFC 6763) instances over multicast DNS (RFC 6762).

Lists the middleware nodes announcing themselves on the local link: each
instance of a service type (default `_mw-node._tcp`, the type posix-node
announces), its host, port and addresses. Standard library only.

    python3 scripts/mdns-browse.py                 # 3 s, _mw-node._tcp
    python3 scripts/mdns-browse.py --seconds 5 --type _mw-node._tcp
    python3 scripts/mdns-browse.py --json          # one JSON list, for scripts

As a module (tests/hil/boards.py): `browse(seconds, service_type)` returns
`Service` records. Nothing here needs the network to import.
"""

import argparse
import dataclasses
import json
import socket
import struct
import sys
import time
from typing import Optional

GROUP = "224.0.0.251"
PORT = 5353
PTR, SRV, A, AAAA = 12, 33, 1, 28


def encode_name(name: str) -> bytes:
    out = b""
    for label in name.rstrip(".").split("."):
        raw = label.encode()
        out += bytes([len(raw)]) + raw
    return out + b"\x00"


def query(service: str) -> bytes:
    # One question, PTR, QU bit clear (answers come to the multicast group).
    return query_for([(service, PTR)])


def query_for(questions) -> bytes:
    """A query with one question per (name, type), QU bit clear."""
    header = struct.pack(">HHHHHH", 0, 0, len(questions), 0, 0, 0)
    return header + b"".join(encode_name(n) + struct.pack(">HH", t, 1) for n, t in questions)


def read_name(msg: bytes, off: int):
    """Returns (name, offset after the name in the record stream)."""
    labels, end = [], None
    while True:
        if off >= len(msg):
            raise ValueError("name past end of message")
        n = msg[off]
        if n == 0:
            off += 1
            break
        if n & 0xC0 == 0xC0:  # compression pointer
            if end is None:
                end = off + 2
            off = ((n & 0x3F) << 8) | msg[off + 1]
            continue
        labels.append(msg[off + 1 : off + 1 + n].decode(errors="replace"))
        off += 1 + n
    return ".".join(labels), (end if end is not None else off)


def records(msg: bytes):
    """Yields (name, rtype, rdata_offset, rdlength, message) for every record."""
    qd, an, ns, ar = struct.unpack_from(">HHHH", msg, 4)
    off = 12
    for _ in range(qd):
        _, off = read_name(msg, off)
        off += 4
    for _ in range(an + ns + ar):
        name, off = read_name(msg, off)
        rtype, _, _, rdlen = struct.unpack_from(">HHIH", msg, off)
        off += 10
        yield name, rtype, off, rdlen, msg
        off += rdlen


@dataclasses.dataclass(frozen=True)
class Service:
    """One DNS-SD instance: its label, host name, port and the host's addresses."""

    label: str
    host: str
    port: Optional[int]
    addresses: tuple  # sorted, IPv4 and IPv6 text

    def to_json(self) -> dict:
        return {"label": self.label, "host": self.host, "port": self.port,
                "addresses": list(self.addresses)}


def _host_key(name: str) -> str:
    """A host name as the address records key it: lower case, no trailing dot
    and no `.local` (SRV targets and A records spell it either way)."""
    name = name.rstrip(".").lower()
    return name[: -len(".local")] if name.endswith(".local") else name


def parse(msgs, service: str) -> list[Service]:
    """Instances of `service` from the responses, with host, port and addresses."""
    service = service.rstrip(".").lower()
    instances = {}   # instance name -> {"host", "port"}
    addrs = {}       # host name -> [addresses]
    for msg in msgs:
        try:
            for name, rtype, off, rdlen, m in records(msg):
                name = _host_key(name) if rtype in (A, AAAA) else name
                if rtype == PTR and name.lower() == service:
                    target, _ = read_name(m, off)
                    instances.setdefault(target, {"host": None, "port": None})
                elif rtype == SRV and name.lower().endswith("." + service):
                    _, _, port = struct.unpack_from(">HHH", m, off)
                    host, _ = read_name(m, off + 6)
                    entry = instances.setdefault(name, {"host": None, "port": None})
                    entry["host"], entry["port"] = host, port
                elif rtype == A and rdlen == 4:
                    addrs.setdefault(name, []).append(socket.inet_ntop(socket.AF_INET, m[off : off + 4]))
                elif rtype == AAAA and rdlen == 16:
                    addrs.setdefault(name, []).append(socket.inet_ntop(socket.AF_INET6, m[off : off + 16]))
        except (ValueError, struct.error):
            continue  # a malformed datagram on the link is not ours to report
    rows = []
    for inst, info in instances.items():
        if not info["host"]:
            continue
        label = inst[: -len(service) - 1] if inst.lower().endswith("." + service) else inst
        rows.append(Service(label, info["host"], info["port"],
                            tuple(sorted(set(addrs.get(_host_key(info["host"]), []))))))
    return sorted(rows, key=lambda r: (r.label, r.host))


def _query_messages(service: str, seconds: float, q: bytes | None = None) -> list[bytes]:
    q = query(service) if q is None else q
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM, socket.IPPROTO_UDP)
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    if hasattr(socket, "SO_REUSEPORT"):
        sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEPORT, 1)
    sock.bind(("", PORT))
    sock.setsockopt(socket.IPPROTO_IP, socket.IP_ADD_MEMBERSHIP,
                    socket.inet_aton(GROUP) + socket.inet_aton("0.0.0.0"))
    sock.setsockopt(socket.IPPROTO_IP, socket.IP_MULTICAST_TTL, 255)
    sock.setsockopt(socket.IPPROTO_IP, socket.IP_MULTICAST_LOOP, 1)
    msgs = []
    end = time.time() + seconds
    next_send = 0.0
    sock.settimeout(0.2)
    while time.time() < end:
        if time.time() >= next_send:  # ask again at 0, 1 s, 2 s ...
            sock.sendto(q, (GROUP, PORT))
            next_send = time.time() + 1.0
        try:
            data, _ = sock.recvfrom(9000)
            msgs.append(data)
        except socket.timeout:
            pass
    sock.close()
    return msgs


def browse(seconds: float = 3.0, service_type: str = "_mw-node._tcp") -> list[Service]:
    """The instances of `service_type` that answer within `seconds`.

    Raises OSError when the multicast socket cannot be set up (no network).
    """
    service = service_type.rstrip(".")
    if not service.endswith(".local"):
        service += ".local"
    msgs = _query_messages(service, seconds)
    # Some announcers answer with the SRV but not the address record: ask for
    # the address of each such host (A and AAAA), once more.
    rows = parse(msgs, service)
    hosts = sorted({r.host for r in rows if r.host and not r.addresses})
    if hosts:
        names = [h if h.lower().endswith(".local") else h + ".local" for h in hosts]
        q = query_for([(n, A) for n in names] + [(n, AAAA) for n in names])
        msgs += _query_messages(service, max(1.0, seconds / 2), q)
        rows = parse(msgs, service)
    return rows


def main(argv):
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--seconds", type=float, default=3.0)
    ap.add_argument("--type", default="_mw-node._tcp", help="DNS-SD service type")
    ap.add_argument("--json", action="store_true", help="print a JSON list instead of the table")
    a = ap.parse_args(argv)
    try:
        rows = browse(a.seconds, a.type)
    except OSError as e:
        print(f"mdns: {e}", file=sys.stderr)
        return 1
    if a.json:
        print(json.dumps([r.to_json() for r in rows]))
        return 0
    print(f"DNS-SD {a.type}: {len(rows)} instance(s) in {a.seconds:g} s")
    for r in rows:
        where = ", ".join(r.addresses) if r.addresses else "no address record in the answers"
        print(f"  {r.label:<24} {r.host}:{r.port}  {where}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
