"""A minimal Ethernet/IPv4/UDP endpoint for one link (CL8), in place of
LiteEth's MAC + ARP + IP + ICMP + UDP layers. It sits on LiteEth's RGMII
PHY stream (preamble included, one byte per cycle) and gives the node the
streams of `node.Link`.

Receive (`eth_rx`, 125 MHz): after the start-of-frame delimiter every byte
of a frame addressed to the link's MAC or to broadcast goes into a block-RAM
ring (`rx_depth` bytes) behind a two-byte length; a frame whose CRC-32 is
good and that has at least 64 bytes is committed, any other (or one that
does not fit) is dropped. The system side (50 MHz) reads each record's
first 42 bytes into a shift register and decides:

* IPv4 (no options, not fragmented, to the link's IP) UDP to `port` with
  a payload: the payload streams out on `rx_*` with the sender's IP and
  port; the sender's IP and MAC are remembered;
* ICMP echo request (`with_icmp`): an echo reply with the same identifier,
  sequence and data (the checksum adjusted for the type), sender
  remembered;
* ARP for the link's IP: the sender is remembered in a second entry and a
  request is answered (to the frame's source MAC);
* anything else is dropped. The IP header checksum is not checked (the
  frame's CRC-32 is) and neither is the UDP checksum.

Transmit: a builder (50 MHz) writes whole frames, header then payload,
into a second ring (`tx_depth` bytes); the `eth_tx` side sends the
preamble, the frame, zero padding to 60 bytes, the CRC-32 and a 12-byte
gap. A datagram from the node goes to the MAC remembered for its IP (the
last UDP/ICMP sender, or the last ARP sender); a broadcast or multicast IP
goes to the broadcast MAC. A datagram to any other IP is dropped
(`tx_miss`) and an ARP request for that IP goes out instead (at most one
per `arp_gap` cycles, a power of two), so the reply fills the ARP entry
and later datagrams are sent. Nothing waits for ARP. Frames from a group
source MAC are dropped and never learned; otherwise the remembered
entries are as spoofable as ARP itself. Outgoing IPv4 has TTL 64, DF set, identification 0, UDP
checksum 0 (none).

Pointers cross between the clock domains with a `BusSynchronizer`; the IP
and MAC (system-domain registers that `sys/net/addr` may change) are read
by the system side only, except the MAC filter, which reads a `MultiReg`
copy.
"""

from migen import (Array, Case, Cat, ClockDomainsRenamer, FSM, If, Memory, Module, Mux,
                   NextState, Signal)
from functools import reduce
from operator import xor

from migen.genlib.cdc import MultiReg, PulseSynchronizer
from migen.genlib.misc import WaitTimer

BCAST = (1 << 48) - 1
K_UDP, K_ICMP, K_ARP_REPLY, K_ARP_REQUEST = range(4)


def crc32_step(crc, data):
    """The next reflected CRC-32 (IEEE 802.3, polynomial 0xEDB88320)
    register after one byte, as XOR expressions."""
    c = [{("c", i)} for i in range(32)]
    for k in range(8):
        fb = c[0] ^ {("d", k)}
        c = c[1:] + [set()]
        for i in range(32):
            if (0xEDB88320 >> i) & 1:
                c[i] = c[i] ^ fb
    return Cat(*[reduce(xor, [crc[j] if t == "c" else data[j] for t, j in sorted(bits)])
                 for bits in c])


def crc32_reg(data, reg=0xFFFFFFFF):
    """The same register in Python."""
    for b in data:
        reg ^= b
        for _ in range(8):
            reg = (reg >> 1) ^ (0xEDB88320 if reg & 1 else 0)
    return reg


# The register after a frame and its (good) FCS.
RESIDUE = crc32_reg(b"\0" + (crc32_reg(b"\0") ^ 0xFFFFFFFF).to_bytes(4, "little"))


class PtrSync(Module):
    """`migen.genlib.cdc.BusSynchronizer` with the output and the input
    buffer reset to 0, so a reset of both domains forgets the old value
    (a ring pointer must not survive a reset)."""

    def __init__(self, width, idomain, odomain, timeout=128):
        self.i = Signal(width)
        self.o = Signal(width)
        sync_i = getattr(self.sync, idomain)
        sync_o = getattr(self.sync, odomain)
        starter = Signal(reset=1)
        sync_i += starter.eq(0)
        self.submodules.ping = PulseSynchronizer(idomain, odomain)
        ping_o = Signal()
        sync_o += ping_o.eq(self.ping.o)
        self.submodules.pong = PulseSynchronizer(odomain, idomain)
        self.submodules.timeout = ClockDomainsRenamer(idomain)(WaitTimer(timeout))
        self.comb += [
            self.timeout.wait.eq(~self.ping.i),
            self.ping.i.eq(starter | self.pong.o | self.timeout.done),
            self.pong.i.eq(ping_o),
        ]
        ibuffer = Signal(width)
        obuffer = Signal(width)
        sync_i += If(self.pong.o, ibuffer.eq(self.i))
        ibuffer.attr.add("no_retiming")
        self.specials += MultiReg(ibuffer, obuffer, odomain)
        sync_o += If(ping_o, self.o.eq(obuffer))


class _ReadPort:
    """A read port whose data (not address) is registered in `domain`: as
    Migen's synchronous port, but yosys maps it to block RAM even when the
    write port is clocked by another domain (with a registered address it
    falls back to LUT RAM)."""

    def __init__(self, module, mem, domain):
        p = mem.get_port(async_read=True)
        module.specials += p
        self.adr = p.adr
        self.dat_r = Signal(8)
        sync = getattr(module.sync, domain)
        sync += self.dat_r.eq(p.dat_r)


def _be(sig, nbytes):
    """Byte i (wire order, big-endian) of a multi-byte value."""
    return [sig[8 * (nbytes - 1 - i):8 * (nbytes - i)] for i in range(nbytes)]


class UdpCore(Module):
    """FSMs carry the control only: the data registers are assigned in plain
    `sync` statements with enables (Migen's `NextValue` builds a
    state-decoded multiplexer in front of every target, a LUT per bit)."""

    def __init__(self, ip, mac, port=7448, with_icmp=True, rx_depth=2048, tx_depth=2048,
                 arp_gap=1 << 22):
        # PHY side: `phy_rx_*` in eth_rx, `phy_tx_*` in eth_tx (the PHY is
        # always ready: 1000BASE-T RGMII).
        self.phy_rx_valid = Signal()
        self.phy_rx_data = Signal(8)
        self.phy_rx_last = Signal()
        self.phy_tx_valid = Signal()
        self.phy_tx_data = Signal(8)
        self.phy_tx_last = Signal()
        # Node side (sys), as `node.Link`.
        self.rx_valid = Signal()
        self.rx_data = Signal(8)
        self.rx_last = Signal()
        self.rx_ready = Signal()
        self.rx_ip = Signal(32)
        self.rx_port = Signal(16)
        self.tx_valid = Signal()
        self.tx_data = Signal(8)
        self.tx_last = Signal()
        self.tx_ready = Signal()
        self.tx_ip = Signal(32)
        self.tx_port = Signal(16)
        self.tx_len = Signal(16)
        # eth_rx: a frame dropped because the receive ring was full (a pulse).
        self.rx_full = Signal()
        # sys: a datagram from the node dropped because no MAC is known for
        # its (unicast) IP (a pulse); an ARP request for it follows.
        self.tx_miss = Signal()

        ra = (rx_depth - 1).bit_length()
        ta = (tx_depth - 1).bit_length()
        assert 1 << ra == rx_depth and 1 << ta == tx_depth

        # ---------------------------------------------------------------
        # Receive ring: written in eth_rx, read on the system side. A record
        # is [length lo][length hi][frame with FCS].
        rxmem = Memory(8, rx_depth)
        rxw = rxmem.get_port(write_capable=True, async_read=True, clock_domain="eth_rx")
        self.specials += rxmem, rxw
        rxr = _ReadPort(self, rxmem, "sys")

        rd = Signal(ra)                     # sys: start of the oldest record
        rd_e = PtrSync(ra, "sys", "eth_rx")
        cm_s = PtrSync(ra, "eth_rx", "sys")
        self.submodules += rd_e, cm_s

        # Pipelined for 125 MHz: the PHY stream is registered first; ring
        # space is decided from registered comparisons one cycle ahead (and
        # conservatively); the destination MAC is matched against a shifting
        # copy, a byte a cycle; the FCS check is registered before the
        # commit; the RAM's write port is driven from registers.
        mac_e = Signal(48)
        self.specials += MultiReg(mac, mac_e, "eth_rx")
        start = Signal(ra)                  # eth_rx: end of the committed records
        start1 = Signal(ra, reset=1)        # start + 1
        wp = Signal(ra)
        cnt = Signal(ra)
        crc = Signal(32)
        to_us = Signal()
        to_all = Signal()
        macsh = Signal(48)                  # the MAC, shifted a byte per byte received
        first6 = Signal(6)                  # one bit per destination byte still to come
        self.comb += rd_e.i.eq(rd), cm_s.i.eq(start)
        rv, d, rl = Signal(), Signal(8), Signal()
        self.sync.eth_rx += [
            rv.eq(self.phy_rx_valid), d.eq(self.phy_rx_data), rl.eq(self.phy_rx_last),
        ]
        # rd - 1, rd - 2, rd - 3 (rd only moves forward, so stale is safe).
        rdm = [Signal(ra, name=f"rx_rdm{i}") for i in (1, 2, 3)]
        self.sync.eth_rx += [r.eq(rd_e.o - i) for i, r in zip((1, 2, 3), rdm)]
        room = Signal()                     # a record may start at `start`
        full = Signal()                     # no byte may be written at `wp`
        sfd = Signal()
        wr = Signal()
        crc_ok = Signal()
        good = Signal()
        self.sync.eth_rx += [
            # From this cycle's wp: covers the next write whether or not wp
            # advances now. Right after the SFD the room check covers it.
            full.eq(~sfd & ((wp == rdm[0]) | (wp == rdm[1]))),
            crc_ok.eq(crc == RESIDUE),
            good.eq((to_us | to_all) & (cnt[6:] != 0)),       # >= 64 bytes
        ]
        wfsm = ClockDomainsRenamer("eth_rx")(FSM(reset_state="PRE"))
        self.submodules += wfsm
        wfsm.act("PRE",
            If(rv & (d == 0xD5) & ~rl,
                If(room, sfd.eq(1), NextState("DATA")
                ).Else(self.rx_full.eq(1), NextState("DROP")),
            ),
        )
        wfsm.act("DATA",
            If(rv,
                If(full,
                    self.rx_full.eq(1),
                    If(rl, NextState("PRE")).Else(NextState("DROP")),
                ).Else(
                    wr.eq(1),
                    If(rl, NextState("CHECK")),
                ),
            ),
        )
        # CHECK: the CRC register holds the frame and its FCS; `crc_ok` and
        # `good` are registered from it during this cycle.
        wfsm.act("CHECK", NextState("CHECK2"))
        wfsm.act("CHECK2", If(crc_ok & good, NextState("LEN0")).Else(NextState("PRE")))
        wfsm.act("DROP", If(rv & rl, NextState("PRE")))
        wfsm.act("LEN0", NextState("LEN1"))
        wfsm.act("LEN1", NextState("PRE"))
        len0 = wfsm.ongoing("LEN0")
        len1 = wfsm.ongoing("LEN1")
        # Not in the cycle after a commit (`start` changes then), so a frame
        # never starts from a stale space check.
        self.sync.eth_rx += room.eq(
            ~len1 & (start != rdm[0]) & (start != rdm[1]) & (start != rdm[2]))
        # The write port, one cycle behind (the commit pointer takes many
        # cycles to reach the reader).
        w_we, w_adr, w_dat = Signal(), Signal(ra), Signal(8)
        self.comb += rxw.we.eq(w_we), rxw.adr.eq(w_adr), rxw.dat_w.eq(w_dat)
        self.sync.eth_rx += [
            w_we.eq(wr | len0 | len1),
            w_adr.eq(Mux(len0, start, Mux(len1, start1, wp))),
            w_dat.eq(Mux(len0, cnt[:8], Mux(len1, cnt[8:], d))),
            If(sfd,
                wp.eq(start + 2), cnt.eq(0), crc.eq(0xFFFFFFFF), to_us.eq(1), to_all.eq(1),
                macsh.eq(mac_e), first6.eq(0b111111),
            ).Elif(wr,
                wp.eq(wp + 1), cnt.eq(cnt + 1), crc.eq(crc32_step(crc, d)),
                macsh.eq(macsh << 8), first6.eq(first6 >> 1),
                If(first6[0],
                    If(d != macsh[40:], to_us.eq(0)),
                    If(d != 0xFF, to_all.eq(0)),
                ),
            ),
            If(len1, start.eq(wp), start1.eq(wp + 1)),
        ]

        # ---------------------------------------------------------------
        # System side: the record's first 44 bytes into `hdr`, then a
        # decision. `off` is the record offset presented to the RAM; with
        # `adv` the next one is, so `rxr.dat_r` is always byte `off`.
        off = Signal(ra)
        adv = Signal()
        self.comb += rxr.adr.eq(rd + Mux(adv, off + 1, off))
        hdr = Signal(44 * 8)
        rb = lambda i: hdr[8 * i:8 * (i + 1)]          # record byte
        fb = lambda i: rb(i + 2)                        # frame byte

        def be(i, n):
            return Cat(*reversed([fb(i + k) for k in range(n)]))

        n_rec = Cat(rb(0), rb(1))[:ra]
        et = be(12, 2)
        tl = be(16, 2)
        proto = fb(23)
        sip = be(26, 4)
        dip = be(30, 4)
        sport = be(34, 2)
        ulen = be(38, 2)
        smac = be(6, 6)
        sha = be(22, 6)
        # A group (multicast, broadcast) source MAC is never a neighbour: such
        # frames are dropped, so they are neither learned nor answered.
        src_ok = ~smac[40]
        ip_ok = Signal()
        udp_ok = Signal()
        icmp_ok = Signal()
        arp_ok = Signal()
        self.comb += [
            ip_ok.eq(src_ok & (et == 0x0800) & (fb(14) == 0x45) & (be(20, 2)[:14] == 0) & (dip == ip)
                     & (tl + 18 <= n_rec)),
            udp_ok.eq(ip_ok & (proto == 17) & (be(36, 2) == port) & (ulen + 20 == tl)
                      & (ulen > 8)),
            arp_ok.eq(src_ok & ~sha[40] & (et == 0x0806) & (be(14, 2) == 1) & (be(16, 2) == 0x0800)
                      & (fb(18) == 6) & (fb(19) == 4) & (be(38, 4) == ip)),
        ]
        if with_icmp:
            self.comb += icmp_ok.eq(ip_ok & (proto == 1) & (fb(34) == 8) & (fb(35) == 0)
                                    & (tl >= 28))
        arp_request = be(20, 2) == 1

        # The two remembered neighbours.
        n_ip = Signal(32)
        n_mac = Signal(48)
        a_ip = Signal(32)
        a_mac = Signal(48)

        end_off = Signal(ra)
        reply_req = Signal()
        reply_icmp = Signal()
        reply_done = Signal()
        rx_adv = Signal()
        self.comb += [
            self.rx_data.eq(rxr.dat_r),
            self.rx_ip.eq(sip),
            self.rx_port.eq(sport),
        ]
        pfsm = FSM(reset_state="IDLE")
        self.submodules += pfsm
        pfsm.act("IDLE", If(rd != cm_s.o, NextState("HDR")))
        pfsm.act("HDR",
            adv.eq(1),
            If(off == 43, NextState("DECIDE")),
        )
        pfsm.act("DECIDE",
            If(udp_ok, NextState("STREAM")
            ).Elif(icmp_ok | (arp_ok & arp_request), NextState("REPLY")
            ).Else(NextState("RELEASE")),
        )
        pfsm.act("STREAM",
            self.rx_valid.eq(1),
            self.rx_last.eq(off == end_off),
            If(self.rx_ready,
                adv.eq(1),
                If(off == end_off, NextState("RELEASE")),
            ),
        )
        pfsm.act("REPLY",
            reply_req.eq(1),
            adv.eq(rx_adv),
            If(reply_done, NextState("RELEASE")),
        )
        pfsm.act("RELEASE", NextState("IDLE"))
        decide = pfsm.ongoing("DECIDE")
        release = pfsm.ongoing("RELEASE")
        self.sync += [
            If(pfsm.ongoing("HDR"), hdr.eq(Cat(hdr[8:], rxr.dat_r))),
            If(release, off.eq(0)
            ).Elif(decide & icmp_ok, off.eq(40)        # the ICMP identifier
            ).Elif(adv, off.eq(off + 1)),
            If(release, rd.eq(rd + n_rec + 2)),
            If(decide,
                end_off.eq(ulen + 35),                 # the last payload byte
                reply_icmp.eq(icmp_ok),
            ),
            If(decide & (udp_ok | icmp_ok), n_ip.eq(sip), n_mac.eq(smac)),
            If(decide & arp_ok, a_ip.eq(be(28, 4)), a_mac.eq(sha)),
        ]

        # ---------------------------------------------------------------
        # Transmit ring: written on the system side, sent in eth_tx. A
        # record is [length lo][length hi][frame without padding and FCS].
        txmem = Memory(8, tx_depth)
        txw = txmem.get_port(write_capable=True, async_read=True, clock_domain="sys")
        self.specials += txmem, txw
        txr = _ReadPort(self, txmem, "eth_tx")
        tc = Signal(ta)                     # sys: end of the committed frames
        trd = Signal(ta)                    # eth_tx: start of the frame being sent
        tc_e = PtrSync(ta, "sys", "eth_tx")
        trd_s = PtrSync(ta, "eth_tx", "sys")
        self.submodules += tc_e, trd_s
        self.comb += tc_e.i.eq(tc), trd_s.i.eq(trd)

        kind = Signal(2)
        d_mac = Signal(48)
        d_ip = Signal(32)
        d_port = Signal(16)
        t_tl = Signal(16)
        t_len = Signal(16)                  # frame bytes before padding and FCS
        t_ulen = Signal(16)
        icmp_ck = Signal(16)
        woff = Signal(ta)
        arp_pend = Signal()
        arp_ip = Signal(32)
        # ARP requests: one allowed per period of a free-running counter.
        gap_t = Signal(max(1, (arp_gap - 1).bit_length()))
        arp_may = Signal(reset=1)

        is_arp = kind[1]
        is_icmp = kind == K_ICMP
        is_req = kind == K_ARP_REQUEST
        # The IPv4 header checksum.
        csum_s = Signal(19)
        csum_1 = Signal(17)
        csum = Signal(16)
        self.comb += [
            csum_s.eq(Mux(is_icmp, 0xC501, 0xC511) + t_tl + ip[16:] + ip[:16]
                      + d_ip[16:] + d_ip[:16]),
            csum_1.eq(csum_s[:16] + csum_s[16:]),
            csum.eq(~(csum_1[:16] + csum_1[16])),
        ]
        # Echo reply: the type word goes from 0x0800 to 0, so the checksum
        # grows by 0x0800 (one's complement).
        ck_t = Signal(17)
        self.comb += ck_t.eq(be(36, 2) + 0x0800)

        hb = Signal(8)
        tha = Mux(is_req, 0, d_mac)
        arp_b = ([0x00, 0x01, 0x08, 0x00, 0x06, 0x04, 0x00, Mux(is_req, 1, 2)]
                 + _be(mac, 6) + _be(ip, 4) + _be(tha, 6) + _be(d_ip, 4))
        ip_b = ([0x45, 0x00] + _be(t_tl, 2) + [0x00, 0x00, 0x40, 0x00, 0x40,
                                                 Mux(is_icmp, 1, 17)]
                + _be(csum, 2) + _be(ip, 4) + _be(d_ip, 4)
                + [Mux(is_icmp, 0, port >> 8), Mux(is_icmp, 0, port & 0xFF)]
                + [Mux(is_icmp, b1, b2) for b1, b2 in zip(_be(icmp_ck, 2), _be(d_port, 2))]
                + _be(t_ulen, 2) + [0x00, 0x00])
        hdr_b = (_be(d_mac, 6) + _be(mac, 6) + [0x08, Mux(is_arp, 0x06, 0x00)]
                 + [Mux(is_arp, a, b) for a, b in zip(arp_b, ip_b)])
        assert len(hdr_b) == 42
        cases = {0: hb.eq(t_len[:8]), 1: hb.eq(t_len[8:])}
        for o, e in enumerate(hdr_b):
            cases[o + 2] = hb.eq(e)
        self.comb += Case(woff[:6], cases)          # (the header is 44 bytes)

        free = Signal(ta)
        self.comb += free.eq(trd_s.o - tc - 1)
        h_last = Mux(is_icmp, 39, 43)
        ti = self.tx_ip
        hit_n = ti == n_ip
        hit_a = ti == a_ip
        group = (ti == 0xFFFFFFFF) | (ti[28:] == 0xE)
        go = Signal()
        wadv = Signal()
        bfsm = FSM(reset_state="IDLE")
        self.submodules += bfsm
        # A datagram to a unicast IP with no known MAC is dropped (never sent
        # to the broadcast MAC); an ARP request for the IP goes out instead.
        miss = ~reply_req & ~arp_pend & self.tx_valid & ~hit_n & ~hit_a & ~group
        bfsm.act("IDLE",
            If(miss,
                self.tx_miss.eq(1),
                NextState("DRAIN"),
            ).Elif(reply_req | arp_pend | self.tx_valid, go.eq(1), NextState("SPACE")),
        )
        bfsm.act("DRAIN",
            self.tx_ready.eq(1),
            If(self.tx_valid & self.tx_last, NextState("IDLE")),
        )
        bfsm.act("SPACE",
            If(t_len[:ta + 1] + 2 <= free, NextState("HDR")),
        )
        bfsm.act("HDR",
            wadv.eq(1),
            If(woff == h_last,
                If(kind == K_UDP, NextState("PAY_UDP")
                ).Elif(is_icmp, NextState("PAY_ICMP")
                ).Else(NextState("COMMIT")),
            ),
        )
        bfsm.act("PAY_UDP",
            self.tx_ready.eq(1),
            wadv.eq(self.tx_valid),
            If(self.tx_valid & self.tx_last, NextState("COMMIT")),
        )
        bfsm.act("PAY_ICMP",
            wadv.eq(1), rx_adv.eq(1),
            If(woff == t_len + 1, NextState("COMMIT")),
        )
        bfsm.act("COMMIT",
            reply_done.eq(~is_req & (kind != K_UDP)),
            NextState("IDLE"),
        )
        commit = bfsm.ongoing("COMMIT")
        self.comb += [
            txw.adr.eq(tc + woff),
            txw.we.eq(wadv),
            txw.dat_w.eq(Mux(bfsm.ongoing("HDR"), hb,
                             Mux(bfsm.ongoing("PAY_UDP"), self.tx_data, rxr.dat_r))),
        ]
        # Which neighbour: the parser has just remembered the requester of a
        # reply (ICMP: n, ARP: a); a datagram uses the entry for its IP.
        use_n = Mux(reply_req, reply_icmp, ~arp_pend & hit_n)
        use_a = Mux(reply_req, ~reply_icmp, ~arp_pend & ~hit_n & hit_a)
        self.sync += [
            gap_t.eq(gap_t + 1),
            If(gap_t == 0, arp_may.eq(1)),
            If(go,
                kind.eq(Mux(reply_req, Mux(reply_icmp, K_ICMP, K_ARP_REPLY),
                            Mux(arp_pend, K_ARP_REQUEST, K_UDP))),
                d_mac.eq(Mux(use_n, n_mac, Mux(use_a, a_mac, BCAST))),
                d_ip.eq(Mux(reply_req, Mux(reply_icmp, n_ip, a_ip), Mux(arp_pend, arp_ip, ti))),
                t_len.eq(Mux(reply_req, Mux(reply_icmp, tl + 14, 42),
                             Mux(arp_pend, 42, self.tx_len + 42))),
                t_tl.eq(Mux(reply_req, tl, self.tx_len + 28)),
                t_ulen.eq(self.tx_len + 8),
                d_port.eq(self.tx_port),
                icmp_ck.eq(ck_t[:16] + ck_t[16]),
            ),
            If(miss & arp_may,
                arp_pend.eq(1), arp_ip.eq(ti), arp_may.eq(0),
            ),
            If(commit,
                tc.eq(tc + t_len + 2),
                If(is_req, arp_pend.eq(0)),
            ),
            If(bfsm.ongoing("IDLE"), woff.eq(0)).Elif(wadv, woff.eq(woff + 1)),
        ]

        # ---------------------------------------------------------------
        # eth_tx: preamble, frame, padding, FCS, gap. Pipelined for 125 MHz:
        # the block RAM's output (slow) goes straight into a register `q`, so
        # a byte arrives two cycles after its address; the address runs two
        # ahead (the PHY is always ready, so the stream never stalls). The
        # CRC follows a registered copy of the outgoing byte (stage 1), and
        # the FCS bytes are put in at the PHY register.
        p = Signal(ta)                      # the address presented when not advancing
        p1 = Signal(ta, reset=1)            # p + 1 (no adder in front of the RAM)
        tadv = Signal()
        q = Signal(8)
        self.comb += txr.adr.eq(Mux(tadv, p1, p))
        self.sync.eth_tx += q.eq(txr.dat_r)
        flen = Signal(16)
        scnt = Signal(16)
        k = Signal(4)
        tcrc = Signal(32)
        ov, od, ol, oc = Signal(), Signal(8), Signal(), Signal()
        sfsm = ClockDomainsRenamer("eth_tx")(FSM(reset_state="IDLE"))
        self.submodules += sfsm
        # IDLE presents the length's low byte, W its high byte; L0 and L1
        # take them from `q`; L0 moves on to the first frame byte, which
        # PRE presents; the last PRE cycle presents the second.
        sfsm.act("IDLE", If(p != tc_e.o, NextState("W")))
        sfsm.act("W", tadv.eq(1), NextState("L0"))
        sfsm.act("L0", tadv.eq(1), NextState("L1"))
        sfsm.act("L1", NextState("PRE"))
        sfsm.act("PRE",
            ov.eq(1), od.eq(Mux(k == 7, 0xD5, 0x55)),
            If(k == 7, tadv.eq(1), NextState("DATA")),
        )
        # `last` is registered: it steers the RAM address.
        rem = Signal(16)                    # frame bytes after the current one
        last = Signal()
        sfsm.act("DATA",
            ov.eq(1), od.eq(q), oc.eq(1),
            tadv.eq(~last),                 # p ends at the record's end
            If(last,
                If(scnt < 59, NextState("PAD")).Else(NextState("FCS")),
            ),
        )
        sfsm.act("PAD",
            ov.eq(1), od.eq(0), oc.eq(1),
            If(scnt == 59, NextState("FCS")),
        )
        sfsm.act("FCS",
            ov.eq(1),                       # (the byte is put in at the output)
            ol.eq(k == 3),
            If(k == 3, NextState("GAP")),
        )
        # `k` runs on from the FCS (4) to 15: a 12-cycle gap.
        sfsm.act("GAP", If(k == 15, NextState("IDLE")))
        pre = sfsm.ongoing("PRE")
        fcs_st = sfsm.ongoing("FCS")
        fcs = ~tcrc
        s1, s1v, s1l, s1c, s1f, s1k = (Signal(8), Signal(), Signal(), Signal(), Signal(),
                                       Signal(2))
        hdr_st = sfsm.ongoing("IDLE") | sfsm.ongoing("W") | sfsm.ongoing("L0") | sfsm.ongoing("L1")
        self.sync.eth_tx += [
            # Stage 1, then the PHY register, which puts in the FCS bytes:
            # by then the CRC (following stage 1) holds the last byte.
            s1.eq(od), s1v.eq(ov), s1l.eq(ol), s1c.eq(oc), s1f.eq(fcs_st), s1k.eq(k[:2]),
            self.phy_tx_valid.eq(s1v), self.phy_tx_last.eq(s1l),
            self.phy_tx_data.eq(Mux(s1f, Array(fcs[8 * i:8 * (i + 1)] for i in range(4))[s1k],
                                    s1)),
            If(pre, tcrc.eq(0xFFFFFFFF)).Elif(s1c, tcrc.eq(crc32_step(tcrc, s1))),
            If(tadv, p.eq(p1), p1.eq(p1 + 1)),
            If(sfsm.ongoing("L0"), flen[:8].eq(q)),
            If(sfsm.ongoing("L1"), flen[8:].eq(q)),
            If(pre, scnt.eq(0)).Elif(oc, scnt.eq(scnt + 1)),
            If(pre, rem.eq(flen - 1), last.eq(flen == 1)
            ).Elif(sfsm.ongoing("DATA"), rem.eq(rem - 1), last.eq(rem == 1)),
            If(hdr_st | oc, k.eq(0)).Else(k.eq(k + 1)),
            If(sfsm.ongoing("GAP") & (k == 15), trd.eq(p)),
        ]
