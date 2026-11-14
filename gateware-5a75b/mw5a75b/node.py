"""The middleware node: two links, each a UDP port 7448, around one bridge
table (CL4, CL5, CL8).

Per link:

* **RX**: datagrams (from LiteEth's UDP port in the SoC, from the
  testbench here) go into a `FrameDecoder`. The unicast sender of every
  valid frame becomes the link's *peer*: responses go back to the
  request's sender; publications (forwarded or the device's own) go to
  the peer. The peer is not authenticated (the board is unclaimed and
  takes unsigned uploads anyway); a configured peer is `sys/net/addr`'s
  job (CL8, open).
* **Dispatch**, once the decoder is done:
  * a request is classified by its key (`KeyClassifier`, constant keys
    matched while the key streams in) and answered by the `Responder`
    (`sys/info`, `sys/trust/info`; any other key is `NotFound`);
  * a publication goes through the bridge table; when a rule sends it to
    the other link, the forwarder streams the frame body out of the
    decoder's buffer to that link's TX (a fresh CRC is appended, so the
    bytes on the wire are the same), then reports the outcome
    (`acct_*`): sent, or dropped when that link has no peer yet;
  * responses and invalid frames are dropped.
* **TX**: an arbiter between the link's responder and the other link's
  forwarder, one datagram at a time, through a `CrcAppender`.

Interfaces are plain valid/ready streams with the UDP addressing beside
them, so the node can be simulated without LiteEth; `soc.py` connects
them to LiteEth UDP ports.
"""

from migen import Array, Cat, Constant, FSM, If, Module, Mux, NextState, NextValue, Signal

from .bridge import BridgeTable
from .frame import KIND_PUBLISH, KIND_REQUEST, CrcAppender, FrameDecoder

PORT = 7448
ABI_VERSION = 6
CAPACITY = 16 * 1024
CHIP = b"ecp5"
FIRMWARE = b"0.1.0"
CAP_BRIDGE = 2
NOT_FOUND = 1

KEYS = [b"sys/info", b"sys/trust/info"]
KEY_INFO, KEY_TRUST = 0, 1


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
    """Matches a streamed key against constant keys."""

    def __init__(self, keys):
        self.start = Signal()
        self.stb = Signal()
        self.idx = Signal(8)
        self.byte = Signal(8)
        self.key_len = Signal(8)
        self.hit = Signal(len(keys))
        alive = Signal(len(keys))
        for k, key in enumerate(keys):
            rom = Array(Constant(b, 8) for b in key)
            self.sync += If(self.start, alive[k].eq(1)).Elif(
                self.stb & ((self.idx >= len(key)) | (self.byte != rom[self.idx[:max(1, (len(key) - 1).bit_length())]])),
                alive[k].eq(0))
            self.comb += self.hit[k].eq(alive[k] & (self.key_len == len(key)))


def info_body(node_id):
    """`DeviceInfo` with status at offset 6 and the node id as Signals."""
    body = []
    body += list(ABI_VERSION.to_bytes(2, "little"))
    body += list(CAPACITY.to_bytes(4, "little"))
    body += ["status", len(CHIP)] + list(CHIP)
    body += [len(FIRMWARE)] + list(FIRMWARE)
    body += [("node", i) for i in range(8)]
    body += list(CAP_BRIDGE.to_bytes(4, "little"))
    return body


class Responder(Module):
    """Builds `[2][id][status][payload]` for a classified request."""

    def __init__(self, node_id, status):
        self.start = Signal()
        self.req_id = Signal(8)
        self.key = Signal(2)           # KEY_* + 1, 0 = unknown
        self.busy = Signal()
        self.valid = Signal()
        self.data = Signal(8)
        self.last = Signal()
        self.ready = Signal()
        self.length = Signal(16)       # body length

        info = info_body(node_id)
        trust = [0, 0, 0, 0, 0]
        node_bytes = [node_id[8 * i:8 * i + 8] for i in range(8)]

        def table(body):
            out = []
            for b in body:
                if b == "status":
                    out.append(status)
                elif isinstance(b, tuple):
                    out.append(node_bytes[b[1]])
                else:
                    out.append(Constant(b, 8))
            return out

        info_t = Array(table(info))
        trust_t = Array(table(trust))
        pos = Signal(8)
        n = Signal(8)
        key = Signal(2)
        rid = Signal(8)
        self.comb += [
            n.eq(Mux(key == KEY_INFO + 1, len(info), Mux(key == KEY_TRUST + 1, len(trust), 0))),
            self.length.eq(3 + n),
        ]
        byte = Signal(8)
        self.comb += If(pos == 0, byte.eq(2)
                     ).Elif(pos == 1, byte.eq(rid)
                     ).Elif(pos == 2, byte.eq(Mux(key == 0, NOT_FOUND, 0))
                     ).Elif(key == KEY_INFO + 1, byte.eq(info_t[pos - 3])
                     ).Else(byte.eq(trust_t[(pos - 3)[:3]]))
        self.comb += [
            self.valid.eq(self.busy),
            self.data.eq(byte),
            self.last.eq(pos == 2 + n),
        ]
        self.sync += [
            If(self.start,
                self.busy.eq(1), pos.eq(0), key.eq(self.key), rid.eq(self.req_id),
            ).Elif(self.busy & self.ready,
                pos.eq(pos + 1),
                If(self.last, self.busy.eq(0)),
            ),
        ]


class Node(Module):
    def __init__(self, macs=(0x025A75B00000, 0x025A75B00001)):
        self.links = [Link(0), Link(1)]
        self.status = Signal(8)        # payload status for sys/info
        self.submodules.bridge = BridgeTable(2)
        b = self.bridge
        node_id = Signal(64, reset=_node_id(macs[0]))
        self.node_id = node_id

        decoders = [FrameDecoder() for _ in range(2)]
        self.submodules += decoders
        peers = [(Signal(32, name=f"peer{n}_ip"), Signal(16, name=f"peer{n}_port"))
                 for n in range(2)]
        self.peers = peers

        # Forwarders: link n streams its buffer to link 1-n.
        fwd = [dict(req=Signal(name=f"fwd{n}_req"), valid=Signal(name=f"fwd{n}_valid"),
                    data=Signal(8, name=f"fwd{n}_data"), last=Signal(name=f"fwd{n}_last"),
                    ready=Signal(name=f"fwd{n}_ready"), length=Signal(16, name=f"fwd{n}_len"),
                    done=Signal(name=f"fwd{n}_done"))
               for n in range(2)]
        responders = []
        acct_reqs = []

        for n, (link, dec) in enumerate(zip(self.links, decoders)):
            # RX into the decoder; the sender of the frame in flight.
            rx_ip = Signal(32, name=f"rx{n}_ip")
            rx_port = Signal(16, name=f"rx{n}_port")
            self.comb += [
                dec.sink_valid.eq(link.rx_valid),
                dec.sink_data.eq(link.rx_data),
                dec.sink_last.eq(link.rx_last),
                link.rx_ready.eq(dec.sink_ready),
            ]
            self.sync += If(dec.start, rx_ip.eq(link.rx_ip), rx_port.eq(link.rx_port))
            # Bridge matching and key classification follow the key.
            cls = KeyClassifier(KEYS)
            self.submodules += cls
            self.comb += [
                b.m_start[n].eq(dec.start), b.m_stb[n].eq(dec.key_stb),
                b.m_idx[n].eq(dec.key_idx), b.m_byte[n].eq(dec.key_byte),
                b.m_end[n].eq(dec.done), b.m_len[n].eq(dec.third),
                cls.start.eq(dec.start), cls.stb.eq(dec.key_stb),
                cls.idx.eq(dec.key_idx), cls.byte.eq(dec.key_byte),
                cls.key_len.eq(dec.third),
            ]
            resp = Responder(node_id, self.status)
            self.submodules += resp
            responders.append(resp)
            resp_ip = Signal(32, name=f"resp{n}_ip")
            resp_port = Signal(16, name=f"resp{n}_port")
            self.resp_dst = getattr(self, "resp_dst", []) + [(resp_ip, resp_port)]

            f = fwd[n]
            rd_pos = Signal(11, name=f"fwd{n}_pos")
            blen = Signal(11, name=f"fwd{n}_blen")
            acct_stb = Signal(name=f"acct{n}_stb")
            acct_rules = Signal(16, name=f"acct{n}_rules")
            acct_sent = Signal(8, name=f"acct{n}_sent")
            acct_reqs.append((acct_stb, acct_rules, acct_sent))
            other = 1 - n

            fsm = FSM(reset_state="WAIT")
            self.submodules += fsm
            # A peer must be a unicast sender: not 0.0.0.0, not the
            # broadcast address, not multicast (224/4), and port != 0.
            unicast = Signal(name=f"rx{n}_unicast")
            self.comb += unicast.eq((rx_ip != 0) & (rx_ip != 0xFFFF_FFFF)
                                    & (rx_ip[28:32] != 0xE) & (rx_port != 0))
            fsm.act("WAIT",
                If(dec.done,
                    If(dec.ok & unicast,
                        NextValue(peers[n][0], rx_ip), NextValue(peers[n][1], rx_port),
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
            fsm.act("REQUEST",
                If(~resp.busy,
                    resp.start.eq(1),
                    resp.req_id.eq(dec.id),
                    resp.key.eq(Mux(cls.hit[KEY_INFO], KEY_INFO + 1,
                                    Mux(cls.hit[KEY_TRUST], KEY_TRUST + 1, 0))),
                    NextValue(resp_ip, rx_ip), NextValue(resp_port, rx_port),
                    NextState("RELEASE"),
                ),
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
                        NextState("RELEASE"),
                    ),
                ),
            )
            # Stream buffer bytes 0..blen-1; the read port has one cycle of
            # latency, so the address runs one byte ahead of `data`.
            fsm.act("FWD_REQ",
                f["req"].eq(1),
                dec.rd_adr.eq(0),
                If(f["ready"] | f["valid"], NextState("FWD")),
                NextValue(rd_pos, 0),
            )
            fsm.act("FWD",
                f["req"].eq(1),
                f["valid"].eq(1),
                f["data"].eq(dec.rd_dat),
                f["last"].eq(rd_pos == blen - 1),
                dec.rd_adr.eq(Mux(f["ready"], rd_pos + 1, rd_pos)),
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
                    NextState("RELEASE"),
                ),
            )
            fsm.act("RELEASE",
                dec.consumed.eq(1),
                NextState("WAIT"),
            )

        # One accounting port on the table: link 0 first.
        (s0, r0, t0), (s1, r1, t1) = acct_reqs
        self.comb += [
            b.acct_stb.eq(s0 | s1),
            b.acct_rules.eq(Mux(s0, r0, r1)),
            b.acct_sent.eq(Mux(s0, t0, t1)),
        ]

        # TX per link: responder of this link or forwarder of the other.
        for n, link in enumerate(self.links):
            resp = responders[n]
            f = fwd[1 - n]
            crc = CrcAppender()
            self.submodules += crc
            grant = Signal(2, name=f"tx{n}_grant")   # 0 idle, 1 resp, 2 fwd
            in_valid = Signal(name=f"tx{n}_in_valid")
            in_data = Signal(8, name=f"tx{n}_in_data")
            in_last = Signal(name=f"tx{n}_in_last")
            self.comb += [
                If(grant == 1,
                    in_valid.eq(resp.valid), in_data.eq(resp.data), in_last.eq(resp.last),
                    resp.ready.eq(crc.sink_ready),
                    link.tx_ip.eq(self.resp_dst[n][0]), link.tx_port.eq(self.resp_dst[n][1]),
                    link.tx_len.eq(resp.length + 2),
                ).Elif(grant == 2,
                    in_valid.eq(f["valid"]), in_data.eq(f["data"]), in_last.eq(f["last"]),
                    f["ready"].eq(crc.sink_ready),
                    link.tx_ip.eq(peers[n][0]), link.tx_port.eq(peers[n][1]),
                    link.tx_len.eq(f["length"] + 2),
                ),
                crc.sink_valid.eq(in_valid),
                crc.sink_data.eq(in_data),
                crc.sink_last.eq(in_last),
                link.tx_valid.eq(crc.source_valid & (grant != 0)),
                link.tx_data.eq(crc.source_data),
                link.tx_last.eq(crc.source_last),
                crc.source_ready.eq(link.tx_ready & (grant != 0)),
            ]
            self.sync += [
                If(grant == 0,
                    If(resp.busy, grant.eq(1)).Elif(f["req"], grant.eq(2)),
                ).Elif(link.tx_valid & link.tx_ready & link.tx_last,
                    grant.eq(0),
                ),
            ]


def _other_acct(reqs, n):
    """Link 1 waits while link 0 accounts in the same cycle."""
    return reqs[0][0] if n == 1 else Constant(0, 1)


def _node_id(mac):
    """`SenderId::from_mac`: the MAC's six bytes, then two zeros, as the
    little-endian 64-bit value whose byte i is wire byte i."""
    b = mac.to_bytes(6, "big") + b"\0\0"
    return int.from_bytes(b, "little")
