"""The middleware node: two links, each a UDP port 7448, around one bridge
table and one payload (CL4, CL5, CL6, CL8).

Per link:

* **RX**: datagrams (from LiteEth's UDP port in the SoC, from the
  testbench here) go into a `FrameDecoder`. The link's *peer* is the
  configured one (`sys/net/addr`), or else the unicast sender of the
  latest valid frame: responses go back to the request's sender;
  publications (forwarded, the payload's, the system's) go to the peer.
* **Dispatch**, once the decoder is done:
  * a request is classified by its key (`KeyClassifier`, constant keys
    matched while the key streams in) and handed to `System` (one
    request at a time for both links), which reads its body from the
    decoder's buffer and answers;
  * a publication goes through the bridge table; when a rule sends it to
    the other link, the forwarder streams the frame body out of the
    decoder's buffer to that link's TX (a fresh CRC is appended, so the
    bytes on the wire are the same), then reports the outcome
    (`acct_*`): sent, or dropped when that link has no peer yet. Then,
    if the payload subscribed to the key (`ImportUnit.sub_match`, not a
    `sys/` key, data at most 512 bytes), the data is delivered to it
    (`on_message`) from the same buffer. Bridging never consumes;
  * responses and invalid frames are dropped.
* **TX**: an arbiter between the system's frames (responses to this
  link's requester, system publications), the other link's forwarder and
  the payload's publications, one datagram at a time, through a
  `CrcAppender`.

Interfaces are plain valid/ready streams with the UDP addressing beside
them, so the node can be simulated without LiteEth; `soc.py` connects
them to LiteEth UDP ports and the flash pads, and takes each link's IP
and MAC address from `System.net`.
"""

from migen import Array, Cat, Constant, FSM, If, Module, Mux, NextState, NextValue, Signal

from .bridge import BridgeTable
from .frame import KIND_PUBLISH, KIND_REQUEST, CrcAppender, FrameDecoder
from .imports import KeyCheck, MAX_MESSAGE
from .system import KEYS, System

PORT = 7448


class Link:
    """The streams of one link."""

    def __init__(self, n):
        p = f"l{n}_"
        self.rx_valid = Signal(name=p + "rx_valid")
        self.rx_data = Signal(8, name=p + "rx_data")
        self.rx_last = Signal(name=p + "rx_last")
        self.rx_ready = Signal(name=p + "rx_ready")
        self.rx_ip = Signal(32, name=p + "rx_ip")
        self.rx_port = Signal(16, name=p + "rx_port")
        self.tx_valid = Signal(name=p + "tx_valid")
        self.tx_data = Signal(8, name=p + "tx_data")
        self.tx_last = Signal(name=p + "tx_last")
        self.tx_ready = Signal(name=p + "tx_ready")
        self.tx_ip = Signal(32, name=p + "tx_ip")
        self.tx_port = Signal(16, name=p + "tx_port")
        self.tx_len = Signal(16, name=p + "tx_len")   # UDP payload bytes

    def signals(self):
        return [v for v in vars(self).values() if isinstance(v, Signal)]


class KeyClassifier(Module):
    """Matches a streamed key against constant keys: `idx_out` is the
    1-based index of the matching key, 0 for none."""

    def __init__(self, keys):
        self.start = Signal()
        self.stb = Signal()
        self.idx = Signal(8)
        self.byte = Signal(8)
        self.key_len = Signal(8)
        self.hit = Signal(len(keys))
        self.idx_out = Signal(max=len(keys) + 1)
        alive = Signal(len(keys))
        for k, key in enumerate(keys):
            rom = Array(Constant(b, 8) for b in key)
            self.sync += If(self.start, alive[k].eq(1)).Elif(
                self.stb & ((self.idx >= len(key))
                            | (self.byte != rom[self.idx[:max(1, (len(key) - 1).bit_length())]])),
                alive[k].eq(0))
            self.comb += self.hit[k].eq(alive[k] & (self.key_len == len(key)))
        for k in reversed(range(len(keys))):
            self.comb += If(self.hit[k], self.idx_out.eq(k + 1))


def _node_id(mac):
    """`SenderId::from_mac`: the MAC's six bytes, then two zeros, as the
    little-endian 64-bit value whose byte i is wire byte i."""
    b = mac.to_bytes(6, "big") + b"\0\0"
    return int.from_bytes(b, "little")


def ip_of(s):
    a = [int(x) for x in s.split(".")]
    return a[0] << 24 | a[1] << 16 | a[2] << 8 | a[3]


class Node(Module):
    def __init__(self, macs=(0x025A75B00000, 0x025A75B00001),
                 ips=("192.168.1.50", "192.168.2.50"), with_sha256=True, flash_div=2,
                 ms_cycles=50_000):
        self.links = [Link(0), Link(1)]
        self.submodules.bridge = BridgeTable(2)
        b = self.bridge
        node_id = Signal(64, reset=_node_id(macs[0]))
        self.node_id = node_id
        self.submodules.sys = sysu = System(b, node_id, [ip_of(i) for i in ips], macs,
                                            with_sha256=with_sha256, flash_div=flash_div,
                                            ms_cycles=ms_cycles)
        imp = sysu.runtime.imports
        self.status = sysu.status
        self.led = Signal()                # user LED on (the payload's pin 0)
        self.led_claimed = sysu.led_claimed
        self.button = sysu.button
        self.comb += self.led.eq(sysu.led_claimed & sysu.led)

        decoders = [FrameDecoder() for _ in range(2)]
        self.submodules += decoders
        learned = [(Signal(32, name=f"learn{n}_ip"), Signal(16, name=f"learn{n}_port"))
                   for n in range(2)]
        peers = [(Signal(32, name=f"peer{n}_ip"), Signal(16, name=f"peer{n}_port"))
                 for n in range(2)]
        self.peers = peers
        for n in range(2):
            cfg = sysu.net[n]
            self.comb += If(cfg.peer_ip != 0,
                peers[n][0].eq(cfg.peer_ip), peers[n][1].eq(cfg.peer_port),
            ).Else(
                peers[n][0].eq(learned[n][0]), peers[n][1].eq(learned[n][1]),
            )
        self.comb += sysu.peer_ok.eq(Cat(*[p[0] != 0 for p in peers]))

        fwd = [dict(req=Signal(name=f"fwd{n}_req"), valid=Signal(name=f"fwd{n}_valid"),
                    data=Signal(8, name=f"fwd{n}_data"), last=Signal(name=f"fwd{n}_last"),
                    ready=Signal(name=f"fwd{n}_ready"), length=Signal(16, name=f"fwd{n}_len"))
               for n in range(2)]
        acct_reqs = []
        req_want = []
        req_fields = []
        resp_dst = []

        for n, (link, dec) in enumerate(zip(self.links, decoders)):
            rx_ip = Signal(32, name=f"rx{n}_ip")
            rx_port = Signal(16, name=f"rx{n}_port")
            self.comb += [
                dec.sink_valid.eq(link.rx_valid),
                dec.sink_data.eq(link.rx_data),
                dec.sink_last.eq(link.rx_last),
                link.rx_ready.eq(dec.sink_ready),
            ]
            self.sync += If(dec.start, rx_ip.eq(link.rx_ip), rx_port.eq(link.rx_port))
            # Bridge matching, key classification, subscriptions and the
            # `sys` check follow the key.
            cls = KeyClassifier(KEYS)
            kc = KeyCheck()
            self.submodules += cls, kc
            sm = imp.sub_match[n]
            self.comb += [
                b.m_start[n].eq(dec.start), b.m_stb[n].eq(dec.key_stb),
                b.m_idx[n].eq(dec.key_idx), b.m_byte[n].eq(dec.key_byte),
                b.m_end[n].eq(dec.done), b.m_len[n].eq(dec.third),
                cls.start.eq(dec.start), cls.stb.eq(dec.key_stb),
                cls.idx.eq(dec.key_idx), cls.byte.eq(dec.key_byte),
                cls.key_len.eq(dec.third),
                sm.start.eq(dec.start), sm.stb.eq(dec.key_stb),
                sm.idx.eq(dec.key_idx), sm.byte.eq(dec.key_byte), sm.key_len.eq(dec.third),
                kc.start.eq(dec.start), kc.stb.eq(dec.key_stb), kc.byte.eq(dec.key_byte),
            ]
            rd_ip = Signal(32, name=f"resp{n}_ip")
            rd_port = Signal(16, name=f"resp{n}_port")
            resp_dst.append((rd_ip, rd_port))

            f = fwd[n]
            rd_pos = Signal(11, name=f"fwd{n}_pos")
            blen = Signal(11, name=f"fwd{n}_blen")
            acct_stb = Signal(name=f"acct{n}_stb")
            acct_rules = Signal(16, name=f"acct{n}_rules")
            acct_sent = Signal(8, name=f"acct{n}_sent")
            acct_reqs.append((acct_stb, acct_rules, acct_sent))
            other = 1 - n
            want = Signal(name=f"req{n}_want")
            req_want.append(want)
            plen = Signal(11, name=f"rx{n}_plen")     # payload (body after the key)
            pbase = Signal(11, name=f"rx{n}_pbase")
            self.comb += pbase.eq(3 + dec.third), plen.eq(dec.body_len - pbase)
            req_fields.append(dict(id=dec.id, key=cls.idx_out, len=plen, base=pbase))
            dlv_want = Signal(name=f"dlv{n}_want")
            dlv_ok = Signal(name=f"dlv{n}_ok")
            self.comb += [
                sysu.dlv_req[n].eq(dlv_want),
                sysu.dlv_slot[n].eq(sm.slot),
                sysu.dlv_len[n].eq(plen),
                sysu.dlv_base[n].eq(pbase),
            ]
            # Subscribed (not a sys/ key), data within MAX_MESSAGE.
            self.sync += If(b.m_valid[n], dlv_ok.eq((sm.hit != 0) & ~kc.sys
                                                    & (plen <= MAX_MESSAGE)))

            fsm = FSM(reset_state="WAIT")
            self.submodules += fsm
            unicast = Signal(name=f"rx{n}_unicast")
            self.comb += unicast.eq((rx_ip != 0) & (rx_ip != 0xFFFF_FFFF)
                                    & (rx_ip[28:32] != 0xE) & (rx_port != 0))
            fsm.act("WAIT",
                If(dec.done,
                    If(dec.ok & unicast,
                        NextValue(learned[n][0], rx_ip), NextValue(learned[n][1], rx_port),
                    ),
                    If(dec.ok & (dec.kind == KIND_REQUEST),
                        NextState("REQUEST"),
                    ).Elif(dec.ok & (dec.kind == KIND_PUBLISH),
                        NextState("MATCH"),
                    ).Else(
                        NextState("RELEASE"),
                    ),
                ),
            )
            taken = Signal(name=f"req{n}_taken")
            self.comb += taken.eq(sysu.req_ready & (sysu.req_link == n))
            fsm.act("REQUEST",
                want.eq(1),
                If(taken,
                    NextValue(rd_ip, rx_ip), NextValue(rd_port, rx_port),
                    NextState("SYSTEM"),
                ),
            )
            owned = Signal(name=f"sys{n}_owned")
            self.comb += owned.eq(fsm.ongoing("SYSTEM"))
            fsm.act("SYSTEM",
                If(sysu.req_release & (sysu.cur_link == n), NextState("RELEASE")),
            )
            fsm.act("MATCH",
                If(b.m_valid[n],
                    NextValue(acct_rules, b.m_rules[n]),
                    If((b.m_links[n] >> other)[0],
                        If(peers[other][0] != 0,
                            NextValue(blen, dec.body_len),
                            NextValue(rd_pos, 0),
                            NextState("FWD_REQ"),
                        ).Else(
                            NextValue(acct_sent, 0),
                            NextState("ACCT"),
                        ),
                    ).Else(
                        NextState("DELIVER"),
                    ),
                ),
            )
            # Stream buffer bytes 0..blen-1; the read port has one cycle of
            # latency, so the address runs one byte ahead of `data`.
            fwd_adr = Signal(10, name=f"fwd{n}_adr")
            fsm.act("FWD_REQ",
                f["req"].eq(1),
                fwd_adr.eq(0),
                If(f["ready"] | f["valid"], NextState("FWD")),
                NextValue(rd_pos, 0),
            )
            fsm.act("FWD",
                f["req"].eq(1),
                f["valid"].eq(1),
                f["data"].eq(dec.rd_dat),
                f["last"].eq(rd_pos == blen - 1),
                fwd_adr.eq(Mux(f["ready"], rd_pos + 1, rd_pos)),
                If(f["ready"],
                    NextValue(rd_pos, rd_pos + 1),
                    If(rd_pos == blen - 1,
                        NextValue(acct_sent, 1 << other),
                        NextState("ACCT"),
                    ),
                ),
            )
            self.comb += f["length"].eq(blen)
            fsm.act("ACCT",
                If(b.acct_ready & ~_other_acct(acct_reqs, n),
                    acct_stb.eq(1),
                    NextState("DELIVER"),
                ),
            )
            fsm.act("DELIVER",
                If(~dlv_ok, NextState("RELEASE")).Else(
                    dlv_want.eq(1),
                    If(sysu.dlv_done[n], NextState("RELEASE")),
                ),
            )
            delivering = Signal(name=f"dlv{n}_active")
            self.comb += delivering.eq(fsm.ongoing("DELIVER") & (sysu.dlv_link == n))
            # The buffer's read port: the system's, the runtime's or the
            # forwarder's.
            self.comb += If(owned, dec.rd_adr.eq(sysu.buf_adr)
                         ).Elif(delivering, dec.rd_adr.eq(sysu.dlv_adr)
                         ).Else(dec.rd_adr.eq(fwd_adr))
            fsm.act("RELEASE",
                dec.consumed.eq(1),
                NextState("WAIT"),
            )

        # The system takes one request at a time: link 0 first.
        sel = Signal()
        self.comb += sel.eq(~req_want[0])
        self.comb += [
            sysu.req_valid.eq(req_want[0] | req_want[1]),
            sysu.req_link.eq(sel),
            sysu.req_id.eq(Mux(sel, req_fields[1]["id"], req_fields[0]["id"])),
            sysu.req_key.eq(Mux(sel, req_fields[1]["key"], req_fields[0]["key"])),
            sysu.req_len.eq(Mux(sel, req_fields[1]["len"], req_fields[0]["len"])),
            sysu.req_base.eq(Mux(sel, req_fields[1]["base"], req_fields[0]["base"])),
            sysu.buf_dat.eq(Mux(sysu.cur_link, decoders[1].rd_dat, decoders[0].rd_dat)),
            sysu.dlv_dat.eq(Mux(sysu.dlv_link, decoders[1].rd_dat, decoders[0].rd_dat)),
        ]

        # One accounting port on the table: link 0 first.
        (s0, r0, t0), (s1, r1, t1) = acct_reqs
        self.comb += [
            b.acct_stb.eq(s0 | s1),
            b.acct_rules.eq(Mux(s0, r0, r1)),
            b.acct_sent.eq(Mux(s0, t0, t1)),
        ]

        # TX per link: the system, the other link's forwarder, the payload.
        G_IDLE, G_SYS, G_FWD, G_PUB = 0, 1, 2, 3
        sys_ready = []
        pub_ready = []
        for n, link in enumerate(self.links):
            f = fwd[1 - n]
            crc = CrcAppender()
            self.submodules += crc
            grant = Signal(2, name=f"tx{n}_grant")
            in_valid = Signal(name=f"tx{n}_in_valid")
            in_data = Signal(8, name=f"tx{n}_in_data")
            in_last = Signal(name=f"tx{n}_in_last")
            sr = Signal(name=f"tx{n}_sys_ready")
            pr = Signal(name=f"tx{n}_pub_ready")
            sys_ready.append(sr)
            pub_ready.append(pr)
            sys_here = sysu.tx_req & (sysu.tx_link == n)
            pub_here = imp.pub_req & (imp.pub_link == n)
            # Destination and length are latched with the grant: a source
            # may move on while the CRC appender sends the last two bytes.
            t_ip = Signal(32, name=f"tx{n}_ip_r")
            t_port = Signal(16, name=f"tx{n}_port_r")
            t_len = Signal(16, name=f"tx{n}_len_r")
            self.comb += [
                link.tx_ip.eq(t_ip), link.tx_port.eq(t_port), link.tx_len.eq(t_len),
                If(grant == G_SYS,
                    in_valid.eq(sysu.tx_valid), in_data.eq(sysu.tx_data),
                    in_last.eq(sysu.tx_last),
                    sr.eq(crc.sink_ready),
                ).Elif(grant == G_FWD,
                    in_valid.eq(f["valid"]), in_data.eq(f["data"]), in_last.eq(f["last"]),
                    f["ready"].eq(crc.sink_ready),
                ).Elif(grant == G_PUB,
                    in_valid.eq(imp.pub_valid), in_data.eq(imp.pub_data),
                    in_last.eq(imp.pub_last),
                    pr.eq(crc.sink_ready),
                ),
                crc.sink_valid.eq(in_valid),
                crc.sink_data.eq(in_data),
                crc.sink_last.eq(in_last),
                link.tx_valid.eq(crc.source_valid & (grant != G_IDLE)),
                link.tx_data.eq(crc.source_data),
                link.tx_last.eq(crc.source_last),
                crc.source_ready.eq(link.tx_ready & (grant != G_IDLE)),
            ]
            to_peer = [t_ip.eq(peers[n][0]), t_port.eq(peers[n][1])]
            self.sync += [
                If(grant == G_IDLE,
                    If(sys_here,
                        grant.eq(G_SYS), t_len.eq(sysu.tx_len + 2),
                        If(sysu.tx_resp, t_ip.eq(resp_dst[n][0]), t_port.eq(resp_dst[n][1])
                        ).Else(*to_peer),
                    ).Elif(f["req"],
                        grant.eq(G_FWD), t_len.eq(f["length"] + 2), *to_peer,
                    ).Elif(pub_here,
                        grant.eq(G_PUB), t_len.eq(imp.pub_len + 2), *to_peer,
                    ),
                ).Elif(link.tx_valid & link.tx_ready & link.tx_last,
                    grant.eq(G_IDLE),
                ),
            ]
        self.comb += [
            sysu.tx_ready.eq(Mux(sysu.tx_link, sys_ready[1], sys_ready[0])),
            imp.pub_ready.eq(Mux(imp.pub_link, pub_ready[1], pub_ready[0])),
        ]


def _other_acct(reqs, n):
    """Link 1 waits while link 0 accounts in the same cycle."""
    return reqs[0][0] if n == 1 else Constant(0, 1)
