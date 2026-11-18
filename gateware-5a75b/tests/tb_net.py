"""cocotb: the minimal UDP/ARP/ICMP endpoint (`mw5a75b/net.py`) between
Ethernet frames on the PHY streams and datagrams on the node's link
streams."""

import os
import random
import struct
import zlib

import cocotb
from cocotb.clock import Clock
from cocotb.triggers import ReadOnly, RisingEdge

PORT = 7448
OUR_IP = "192.168.1.50"
OUR_MAC = 0x025A75B00000
HOST_IP = "192.168.1.10"
HOST_MAC = 0x0A0B0C0D0E0F
OTHER_IP = "192.168.1.77"
OTHER_MAC = 0x020000000077
BCAST = 0xFFFFFFFFFFFF


def ip(s):
    a = [int(x) for x in s.split(".")]
    return a[0] << 24 | a[1] << 16 | a[2] << 8 | a[3]


def mac_b(m):
    return m.to_bytes(6, "big")


def ip_b(s):
    return ip(s).to_bytes(4, "big")


def csum16(data):
    if len(data) % 2:
        data += b"\0"
    s = sum(struct.unpack(f"!{len(data) // 2}H", data))
    while s >> 16:
        s = (s & 0xFFFF) + (s >> 16)
    return ~s & 0xFFFF


def eth(dst, src, etype, payload, fcs_ok=True):
    f = mac_b(dst) + mac_b(src) + struct.pack("!H", etype) + payload
    f = f.ljust(60, b"\0")
    fcs = zlib.crc32(f)
    if not fcs_ok:
        fcs ^= 1
    return f + fcs.to_bytes(4, "little")


def ipv4(src, dst, proto, payload, frag=0x4000):
    h = struct.pack("!BBHHHBBH4s4s", 0x45, 0, 20 + len(payload), 0x1234, frag, 64, proto, 0,
                    ip_b(src), ip_b(dst))
    h = h[:10] + struct.pack("!H", csum16(h)) + h[12:]
    return h + payload


def udp(sport, dport, payload):
    return struct.pack("!HHHH", sport, dport, 8 + len(payload), 0) + payload


def arp(op, sha, spa, tha, tpa):
    return (struct.pack("!HHBBH", 1, 0x0800, 6, 4, op) + mac_b(sha) + ip_b(spa)
            + mac_b(tha) + ip_b(tpa))


def icmp_echo(ident, seq, data):
    h = struct.pack("!BBHHH", 8, 0, 0, ident, seq) + data
    return h[:2] + struct.pack("!H", csum16(h)) + h[4:]


def host_udp(payload, sport=5000, dport=PORT, src_ip=HOST_IP, src_mac=HOST_MAC,
             dst_ip=OUR_IP, dst_mac=OUR_MAC, **kw):
    return eth(dst_mac, src_mac, 0x0800, ipv4(src_ip, dst_ip, 17, udp(sport, dport, payload)),
               **kw)


class Bench:
    def __init__(self, dut, seed=1, link=True):
        self.dut = dut
        self.link = link
        self.rng = random.Random(seed)
        self.sent = []              # frames off the PHY TX, FCS checked and stripped
        self.got = []               # (ip, port, payload) on the link
        self.ready_p = 0.8

    async def start(self):
        d = self.dut
        cocotb.start_soon(Clock(d.sys_clk, 20, unit="ns").start())
        cocotb.start_soon(Clock(d.eth_rx_clk, 8, unit="ns").start())
        cocotb.start_soon(Clock(d.eth_tx_clk, 8, unit="ns").start())
        zero = ["phy_rx_valid", "phy_rx_last"]
        if self.link:
            d.ip.value = ip(OUR_IP)
            d.mac.value = OUR_MAC
            zero += ["tx_valid", "tx_last", "rx_ready"]
        else:
            zero += ["button"]
        for s in zero:
            getattr(d, s).value = 0
        for r in ("sys_rst", "eth_rx_rst", "eth_tx_rst"):
            getattr(d, r).value = 1
        for _ in range(3):
            await RisingEdge(d.sys_clk)
        for r in ("sys_rst", "eth_rx_rst", "eth_tx_rst"):
            getattr(d, r).value = 0
        cocotb.start_soon(self._phy_tx())
        self.miss = 0
        if self.link:
            cocotb.start_soon(self._link_rx())
            cocotb.start_soon(self._tx_miss())
        cocotb.start_soon(self._rx_full())
        for _ in range(20):
            await RisingEdge(d.sys_clk)

    async def _phy_tx(self):
        d = self.dut
        buf = []
        idle = 99
        while True:
            await RisingEdge(d.eth_tx_clk)
            await ReadOnly()
            if int(d.phy_tx_valid.value):
                if not buf:
                    assert idle >= 12, f"inter-frame gap {idle}"
                buf.append(int(d.phy_tx_data.value))
                if int(d.phy_tx_last.value):
                    f = bytes(buf)
                    buf = []
                    idle = 0
                    assert f[:8] == b"\x55" * 7 + b"\xd5", f[:8].hex()
                    f = f[8:]
                    assert len(f) >= 64, len(f)
                    assert zlib.crc32(f[:-4]).to_bytes(4, "little") == f[-4:], "FCS"
                    self.sent.append(f[:-4])
            else:
                assert not buf, "valid dropped inside a frame"
                idle += 1

    async def _tx_miss(self):
        while True:
            await RisingEdge(self.dut.sys_clk)
            await ReadOnly()
            self.miss += int(self.dut.tx_miss.value)

    async def _rx_full(self):
        self.full = 0
        while True:
            await RisingEdge(self.dut.eth_rx_clk)
            await ReadOnly()
            self.full += int(self.dut.rx_full.value)

    async def _link_rx(self):
        d = self.dut
        buf = []
        while True:
            ready = self.rng.random() < self.ready_p
            d.rx_ready.value = int(ready)
            await ReadOnly()
            if ready and int(d.rx_valid.value):
                buf.append(int(d.rx_data.value))
                if int(d.rx_last.value):
                    self.got.append((int(d.rx_ip.value), int(d.rx_port.value), bytes(buf)))
                    buf = []
            await RisingEdge(d.sys_clk)

    async def phy_send(self, frame, gap=12, preamble=7):
        d = self.dut
        data = b"\x55" * preamble + b"\xd5" + frame
        for i, b in enumerate(data):
            await RisingEdge(d.eth_rx_clk)
            d.phy_rx_valid.value = 1
            d.phy_rx_data.value = b
            d.phy_rx_last.value = int(i == len(data) - 1)
        await RisingEdge(d.eth_rx_clk)
        d.phy_rx_valid.value = 0
        d.phy_rx_last.value = 0
        for _ in range(gap - 1):
            await RisingEdge(d.eth_rx_clk)

    async def link_send(self, dst_ip, dst_port, payload):
        d = self.dut
        d.tx_ip.value = ip(dst_ip)
        d.tx_port.value = dst_port
        d.tx_len.value = len(payload)
        i = 0
        while i < len(payload):
            d.tx_valid.value = 1
            d.tx_data.value = payload[i]
            d.tx_last.value = int(i == len(payload) - 1)
            await ReadOnly()
            took = int(d.tx_ready.value)
            await RisingEdge(d.sys_clk)
            if took:
                i += 1
        d.tx_valid.value = 0
        d.tx_last.value = 0

    async def wait(self, cond, cycles=20000, what="condition"):
        for _ in range(cycles):
            if cond():
                return
            await RisingEdge(self.dut.sys_clk)
        raise AssertionError(f"timed out waiting for {what() if callable(what) else what}")

    async def idle(self, cycles=2000):
        for _ in range(cycles):
            await RisingEdge(self.dut.sys_clk)


def parse_ipv4_udp(f):
    """Checks an outgoing IPv4/UDP frame; returns (dst mac, dst ip, dport, payload)."""
    dst, src, et = f[0:6], f[6:12], struct.unpack("!H", f[12:14])[0]
    assert src == mac_b(OUR_MAC) and et == 0x0800, f.hex()
    h = f[14:34]
    assert h[0] == 0x45 and csum16(h) == 0, "IPv4 header checksum " + h.hex()
    tl, frag, ttl, proto = struct.unpack("!H", h[2:4])[0], h[6:8], h[8], h[9]
    assert frag == b"\x40\x00" and ttl == 64 and proto == 17
    assert h[12:16] == ip_b(OUR_IP)
    sport, dport, ulen, uck = struct.unpack("!HHHH", f[34:42])
    assert sport == PORT and uck == 0 and ulen == tl - 20
    payload = f[42:42 + ulen - 8]
    assert len(payload) == ulen - 8
    if 14 + tl < 60:
        assert f[14 + tl:] == b"\0" * (60 - 14 - tl), "padding"
    else:
        assert len(f) == 14 + tl
    return int.from_bytes(dst, "big"), int.from_bytes(h[16:20], "big"), dport, payload


@cocotb.test()
async def arp_request_is_answered(dut):
    b = Bench(dut)
    await b.start()
    await b.phy_send(eth(BCAST, HOST_MAC, 0x0806, arp(1, HOST_MAC, HOST_IP, 0, OUR_IP)))
    await b.wait(lambda: len(b.sent) == 1, what="ARP reply")
    f = b.sent[0]
    assert f[:14] == mac_b(HOST_MAC) + mac_b(OUR_MAC) + b"\x08\x06"
    assert f[14:42] == arp(2, OUR_MAC, OUR_IP, HOST_MAC, HOST_IP), f[14:42].hex()
    assert f[42:] == b"\0" * 18
    # Not for us: another IP, a bad FCS, a reply (learned, not answered).
    await b.phy_send(eth(BCAST, HOST_MAC, 0x0806, arp(1, HOST_MAC, HOST_IP, 0, OTHER_IP)))
    await b.phy_send(eth(BCAST, HOST_MAC, 0x0806, arp(1, HOST_MAC, HOST_IP, 0, OUR_IP),
                         fcs_ok=False))
    await b.phy_send(eth(OUR_MAC, OTHER_MAC, 0x0806, arp(2, OTHER_MAC, OTHER_IP, OUR_MAC,
                                                         OUR_IP)))
    await b.idle()
    assert len(b.sent) == 1


@cocotb.test()
async def udp_to_the_port_reaches_the_link(dut):
    b = Bench(dut, seed=2)
    await b.start()
    p = bytes(range(1, 30))
    await b.phy_send(host_udp(p, sport=5555))
    await b.wait(lambda: len(b.got) == 1, what="datagram")
    assert b.got[0] == (ip(HOST_IP), 5555, p)
    # Dropped: another port, another IP, another MAC, a bad FCS, a fragment, empty.
    await b.phy_send(host_udp(p, dport=7449))
    await b.phy_send(host_udp(p, dst_ip=OTHER_IP))
    await b.phy_send(host_udp(p, dst_mac=OTHER_MAC))
    await b.phy_send(host_udp(p, fcs_ok=False))
    await b.phy_send(eth(OUR_MAC, HOST_MAC, 0x0800,
                         ipv4(HOST_IP, OUR_IP, 17, udp(5000, PORT, p), frag=0x2000)))
    await b.phy_send(host_udp(b""))
    # Accepted: to the broadcast MAC (with our IP), long, back to back.
    await b.phy_send(host_udp(b"bc", dst_mac=BCAST))
    big = bytes(b.rng.randrange(256) for _ in range(1024))
    await b.phy_send(host_udp(big, sport=6000, src_ip=OTHER_IP, src_mac=OTHER_MAC))
    await b.wait(lambda: len(b.got) == 3, cycles=40000, what="big datagram")
    small = [bytes([i]) * (1 + i * 7) for i in range(12)]
    for i, s in enumerate(small):
        await b.phy_send(host_udp(s, sport=7000 + i), gap=12)
    await b.wait(lambda: len(b.got) == 3 + len(small), cycles=40000, what="burst")
    await b.idle()
    assert b.got[1] == (ip(HOST_IP), 5000, b"bc")
    assert b.got[2] == (ip(OTHER_IP), 6000, big)
    assert b.got[3:] == [(ip(HOST_IP), 7000 + i, s) for i, s in enumerate(small)]
    assert b.sent == []


@cocotb.test()
async def node_datagrams_go_to_the_learned_mac(dut):
    b = Bench(dut, seed=3)
    await b.start()
    await b.phy_send(host_udp(b"hello", sport=5001))
    await b.wait(lambda: len(b.got) == 1)
    for n in (1, 17, 18, 100, 1026):
        p = bytes(b.rng.randrange(256) for _ in range(n))
        await b.link_send(HOST_IP, 5001, p)
        await b.wait(lambda: len(b.sent) == 1, cycles=40000, what=f"{n}-byte datagram")
        assert parse_ipv4_udp(b.sent.pop()) == (HOST_MAC, ip(HOST_IP), 5001, p)
    # Back to back, the ring holding several.
    ps = [bytes([i]) * (10 + 50 * i) for i in range(6)]
    for p in ps:
        await b.link_send(HOST_IP, 5001, p)
    await b.wait(lambda: len(b.sent) == len(ps), cycles=40000)
    assert [parse_ipv4_udp(f)[3] for f in b.sent] == ps


@cocotb.test()
async def unknown_ip_is_dropped_until_arp_learns_it(dut):
    b = Bench(dut, seed=4)
    await b.start()
    # No MAC for the IP: the datagram is dropped, an ARP request goes out.
    await b.link_send(OTHER_IP, 9000, b"first")
    await b.wait(lambda: len(b.sent) == 1, what="ARP request")
    await b.idle(200)
    assert len(b.sent) == 1 and b.miss == 1
    f = b.sent[0]
    assert f[:14] == mac_b(BCAST) + mac_b(OUR_MAC) + b"\x08\x06"
    assert f[14:42] == arp(1, OUR_MAC, OUR_IP, 0, OTHER_IP), f[14:42].hex()
    # Within the gap: dropped again, no second request.
    await b.link_send(OTHER_IP, 9000, b"second")
    await b.idle(200)
    assert len(b.sent) == 1 and b.miss == 2
    # The reply fills the ARP entry: unicast from now on.
    await b.phy_send(eth(OUR_MAC, OTHER_MAC, 0x0806,
                         arp(2, OTHER_MAC, OTHER_IP, OUR_MAC, OUR_IP)))
    await b.idle(200)
    await b.link_send(OTHER_IP, 9000, b"third")
    await b.wait(lambda: len(b.sent) == 2)
    assert parse_ipv4_udp(b.sent[1]) == (OTHER_MAC, ip(OTHER_IP), 9000, b"third")
    # A host's datagram fills the other entry; both are kept.
    await b.phy_send(host_udp(b"x"))
    await b.wait(lambda: len(b.got) == 1)
    await b.link_send(HOST_IP, 5000, b"to host")
    await b.link_send(OTHER_IP, 9000, b"to other")
    await b.wait(lambda: len(b.sent) == 4)
    assert parse_ipv4_udp(b.sent[2])[:2] == (HOST_MAC, ip(HOST_IP))
    assert parse_ipv4_udp(b.sent[3])[:2] == (OTHER_MAC, ip(OTHER_IP))
    # Broadcast and multicast IPs: broadcast MAC, no ARP.
    await b.idle(2100)       # past the ARP gap
    await b.link_send("255.255.255.255", 9000, b"all")
    await b.link_send("239.1.2.3", 9000, b"group")
    await b.wait(lambda: len(b.sent) == 6)
    await b.idle(200)
    assert len(b.sent) == 6 and b.miss == 2
    assert parse_ipv4_udp(b.sent[4])[:2] == (BCAST, 0xFFFFFFFF)
    assert parse_ipv4_udp(b.sent[5])[:2] == (BCAST, ip("239.1.2.3"))


@cocotb.test()
async def group_source_macs_are_not_learned(dut):
    b = Bench(dut, seed=8)
    await b.start()
    grp = 0x01005E000001
    # UDP, ARP request (group source, or group sender address), ping: all
    # dropped, nothing learned or answered.
    await b.phy_send(host_udp(b"g", src_ip=OTHER_IP, src_mac=grp))
    await b.phy_send(host_udp(b"g", src_ip=OTHER_IP, src_mac=BCAST))
    await b.phy_send(eth(BCAST, grp, 0x0806, arp(1, grp, OTHER_IP, 0, OUR_IP)))
    await b.phy_send(eth(BCAST, OTHER_MAC, 0x0806, arp(1, grp, OTHER_IP, 0, OUR_IP)))
    await b.phy_send(eth(OUR_MAC, grp, 0x0800,
                         ipv4(OTHER_IP, OUR_IP, 1, icmp_echo(1, 1, b"ab"))))
    await b.idle(1000)
    assert b.got == [] and b.sent == []
    await b.link_send(OTHER_IP, 9000, b"x")
    await b.wait(lambda: len(b.sent) == 1)
    await b.idle(200)
    assert b.miss == 1 and b.sent[0][12:14] == b"\x08\x06", "an ARP request only"


@cocotb.test()
async def ping_is_answered(dut):
    b = Bench(dut, seed=5)
    await b.start()
    if os.environ.get("NET_ICMP") == "0":       # built without ICMP: no reply
        await b.phy_send(eth(OUR_MAC, HOST_MAC, 0x0800,
                             ipv4(HOST_IP, OUR_IP, 1, icmp_echo(1, 1, b"ab"))))
        await b.idle()
        assert b.sent == []
        return
    for n, data in enumerate([b"", bytes(range(56)), bytes(b.rng.randrange(256)
                                                            for _ in range(1000))]):
        req = icmp_echo(0x4242, n, data)
        await b.phy_send(eth(OUR_MAC, HOST_MAC, 0x0800, ipv4(HOST_IP, OUR_IP, 1, req)))
        await b.wait(lambda: len(b.sent) == 1, cycles=40000, what="echo reply")
        f = b.sent.pop()
        assert f[:14] == mac_b(HOST_MAC) + mac_b(OUR_MAC) + b"\x08\x00"
        h = f[14:34]
        assert csum16(h) == 0 and h[9] == 1 and h[12:16] == ip_b(OUR_IP) \
            and h[16:20] == ip_b(HOST_IP)
        tl = struct.unpack("!H", h[2:4])[0]
        rep = f[34:14 + tl]
        assert tl == 20 + len(req)
        assert rep[0] == 0 and rep[1] == 0 and csum16(rep) == 0, rep[:8].hex()
        assert rep[4:] == req[4:]
    # Not an echo request: an echo reply, ICMP to another IP.
    await b.phy_send(eth(OUR_MAC, HOST_MAC, 0x0800,
                         ipv4(HOST_IP, OUR_IP, 1, b"\0" + icmp_echo(1, 1, b"ab")[1:])))
    await b.phy_send(eth(OUR_MAC, HOST_MAC, 0x0800,
                         ipv4(HOST_IP, OTHER_IP, 1, icmp_echo(1, 1, b"ab"))))
    await b.idle()
    assert b.sent == []


@cocotb.test()
async def traffic_both_ways_at_once(dut):
    b = Bench(dut, seed=6)
    await b.start()
    b.ready_p = 0.5
    await b.phy_send(host_udp(b"first"))
    await b.wait(lambda: len(b.got) == 1)
    b.got.clear()

    async def node_side():
        for i in range(20):
            await b.link_send(HOST_IP, 5000, bytes([i]) * (5 + 37 * i))

    t = cocotb.start_soon(node_side())
    rx = []
    for i in range(20):
        p = bytes([0x80 | i]) * (3 + 11 * i)
        rx.append(p)
        await b.phy_send(host_udp(p))
        await b.phy_send(eth(BCAST, HOST_MAC, 0x0806, arp(1, HOST_MAC, HOST_IP, 0, OUR_IP)))
        await b.idle(300 + b.rng.randrange(400))
    await t
    await b.wait(lambda: len(b.got) == 20 and len(b.sent) == 40, cycles=100000,
                 what=lambda: f"{len(b.got)} datagrams, {len(b.sent)} frames, {b.full} full: "
                 + " ".join(f[12:14].hex() + ":" + str(len(f)) + ":" + f[:6].hex() for f in b.sent))
    assert [g[2] for g in b.got] == rx
    assert b.full == 0
    udps = [f for f in b.sent if f[12:14] == b"\x08\x00"]
    arps = [f for f in b.sent if f[12:14] == b"\x08\x06"]
    assert len(arps) == 20
    assert [parse_ipv4_udp(f)[3] for f in udps] == [bytes([i]) * (5 + 37 * i)
                                                    for i in range(20)]


@cocotb.test()
async def a_full_ring_drops_whole_frames(dut):
    b = Bench(dut, seed=7)
    await b.start()
    b.ready_p = 0.0
    ps = [bytes([i]) * 300 for i in range(12)]
    for i, p in enumerate(ps):
        await b.phy_send(host_udp(p, sport=8000 + i))
    await b.idle(200)
    assert b.full > 0
    b.ready_p = 0.8
    await b.wait(lambda: len(b.got) >= 1)
    await b.idle(5000)
    n = len(b.got)
    assert 0 < n < len(ps)
    it = iter(enumerate(ps))
    for a, port, data in b.got:         # an in-order subsequence, each intact
        assert a == ip(HOST_IP)
        assert any(port == 8000 + i and data == p for i, p in it)
    await b.phy_send(host_udp(b"after", sport=9999))
    await b.wait(lambda: len(b.got) == n + 1)
    assert b.got[-1] == (ip(HOST_IP), 9999, b"after")



@cocotb.test()
async def a_frame_right_after_a_commit_never_overwrites_unread_ones(dut):
    """The ring filled to two bytes short of the unread record (the node
    does not read), then one more frame whose SFD follows the previous
    frame as closely as the PHY stream allows, for every gap: it does not
    fit, so it is dropped (or missed), never written over the unread
    records."""
    b = Bench(dut, seed=9)
    await b.start()
    d = dut
    # Three records of 2 + 46 + 634 + 4... bytes: 3 * (48 + 634) = 2046.
    fill = [bytes([0x10 + j]) * 634 for j in range(3)]
    for gap in range(1, 10):
        b.ready_p = 0.0
        for r in ("sys_rst", "eth_rx_rst", "eth_tx_rst"):
            getattr(d, r).value = 1
        for _ in range(5):
            await RisingEdge(d.sys_clk)
        for r in ("sys_rst", "eth_rx_rst", "eth_tx_rst"):
            getattr(d, r).value = 0
        await b.idle(300)
        b.got.clear()
        b.full = 0
        for j, p in enumerate(fill):
            await b.phy_send(host_udp(p, sport=8000 + j), gap=4 if j < 2 else gap)
        await b.phy_send(host_udp(b"late" * 20, sport=8999), preamble=0)
        await b.idle(300)
        b.ready_p = 0.8
        await b.wait(lambda: len(b.got) == 3, cycles=20000, what=f"gap {gap}")
        await b.idle(500)
        assert b.got == [(ip(HOST_IP), 8000 + j, p) for j, p in enumerate(fill)], f"gap {gap}"


@cocotb.test()
async def back_to_back_frames_never_overwrite_unread_ones(dut):
    """Frames with gaps and preambles shorter than any PHY makes, into a
    nearly full ring that nobody reads: whatever is kept arrives intact."""
    b = Bench(dut, seed=9)
    await b.start()
    b.ready_p = 0.0
    sent = []
    i = 0
    for gap in range(1, 10):
        for pre in (0, 1, 3):
            p = bytes([i]) * (40 + 37 * (i % 7))
            sent.append((9000 + i, p))
            await b.phy_send(host_udp(p, sport=9000 + i), gap=gap, preamble=pre)
            i += 1
    await b.idle(500)
    assert b.full > 0, "the ring filled"
    b.ready_p = 0.8
    await b.idle(30000)
    assert b.got, "something was kept"
    it = iter(sent)
    for a, port, data in b.got:         # an in-order subsequence, each intact
        assert a == ip(HOST_IP)
        assert any(port == sp and data == p for sp, p in it), (port, len(data))
