"""cocotb: the two-link node."""

import random

import cocotb
from cocotb.triggers import ReadOnly, RisingEdge

from mw5a75b.frame import crc16_py
from mw5a75b.node import _node_id
from tb_bridge import add, wait_done
from tb_common import start


def ip(s):
    a = [int(x) for x in s.split(".")]
    return a[0] << 24 | a[1] << 16 | a[2] << 8 | a[3]


def frame(body):
    return bytes(body) + crc16_py(body).to_bytes(2, "little")


def request(rid, key, payload=b""):
    return frame([1, rid, len(key)] + list(key) + list(payload))


def publish(key, payload):
    return frame([3, 0, len(key)] + list(key) + list(payload))


def info_payload(status=0):
    node = _node_id(0x025A75B00000).to_bytes(8, "little")
    return (bytes([6, 0]) + (16384).to_bytes(4, "little") + bytes([status, 4]) + b"ecp5"
            + bytes([5]) + b"0.1.0" + node + (2).to_bytes(4, "little"))


class TxTap:
    def __init__(self, dut, n, rng):
        self.dut, self.n, self.rng = dut, n, rng
        self.frames = []
        cocotb.start_soon(self.run())

    def s(self, name):
        return getattr(self.dut, f"l{self.n}_{name}")

    async def run(self):
        buf = []
        while True:
            ready = self.rng.random() < 0.7
            self.s("tx_ready").value = int(ready)
            await ReadOnly()
            if ready and int(self.s("tx_valid").value):
                buf.append(int(self.s("tx_data").value))
                if int(self.s("tx_last").value):
                    self.frames.append((int(self.s("tx_ip").value), int(self.s("tx_port").value),
                                        int(self.s("tx_len").value), bytes(buf)))
                    buf = []
            await RisingEdge(self.dut.sys_clk)


async def send(dut, n, data, src_ip, src_port):
    s = lambda name: getattr(dut, f"l{n}_{name}")
    s("rx_ip").value = ip(src_ip)
    s("rx_port").value = src_port
    i = 0
    while i < len(data):
        s("rx_valid").value = 1
        s("rx_data").value = data[i]
        s("rx_last").value = int(i == len(data) - 1)
        await ReadOnly()
        took = int(s("rx_ready").value)
        await RisingEdge(dut.sys_clk)
        if took:
            i += 1
    s("rx_valid").value = 0
    s("rx_last").value = 0


async def settle(dut, n=200):
    for _ in range(n):
        await RisingEdge(dut.sys_clk)


async def stat(dut, h, dropped):
    dut.handle.value = h
    dut.stat_dropped.value = dropped
    dut.stat_stb.value = 1
    await RisingEdge(dut.sys_clk)
    dut.stat_stb.value = 0
    return await wait_done(dut)


@cocotb.test()
async def cl8_two_links(dut):
    zero = ["add_start", "add_stb", "add_end", "rm_stb", "stat_stb", "status"] + \
        [f"l{n}_{s}" for n in (0, 1) for s in ("rx_valid", "rx_last")]
    await start(dut, zero)
    rng = random.Random(3)
    tx = [TxTap(dut, 0, rng), TxTap(dut, 1, rng)]

    # sys/info and sys/trust/info answered to the sender, NotFound otherwise.
    await send(dut, 0, request(7, b"sys/info"), "10.0.0.1", 5000)
    await settle(dut)
    await send(dut, 0, request(8, b"sys/trust/info"), "10.0.0.1", 5000)
    await settle(dut)
    await send(dut, 0, request(9, b"sys/nope"), "10.0.0.1", 5001)
    await settle(dut)
    await send(dut, 0, request(10, b"sys/info")[:-1] + b"\x00", "10.0.0.1", 5000)  # bad CRC
    await settle(dut)
    want = [
        (ip("10.0.0.1"), 5000, frame([2, 7, 0] + list(info_payload()))),
        (ip("10.0.0.1"), 5000, frame([2, 8, 0, 0, 0, 0, 0, 0])),
        (ip("10.0.0.1"), 5001, frame([2, 9, 1])),
    ]
    got = [(a, p, d) for a, p, l, d in tx[0].frames]
    assert got == want, f"{got}\n{want}"
    assert all(l == len(d) for _, _, l, d in tx[0].frames)
    assert tx[1].frames == []

    # A rule demo/** link 0 -> 1. No peer on link 1 yet: dropped.
    h = await add(dut, 0, "demo/**", 1)
    assert h == 16
    await send(dut, 0, publish(b"demo/x", b"\x01\x02"), "10.0.0.1", 5000)
    await settle(dut)
    assert tx[1].frames == []
    # Link 1 learns its peer from any valid frame.
    await send(dut, 1, request(1, b"sys/info"), "10.0.1.2", 6000)
    await settle(dut)
    assert len(tx[1].frames) == 1 and tx[1].frames[0][:2] == (ip("10.0.1.2"), 6000)
    pub = publish(b"demo/x", bytes(range(200)))
    await send(dut, 0, pub, "10.0.0.1", 5000)
    await settle(dut, 600)
    await send(dut, 0, publish(b"other/y", b"z"), "10.0.0.1", 5000)
    await send(dut, 0, publish(b"demox", b"z"), "10.0.0.1", 5000)
    await send(dut, 1, publish(b"demo/x", b"wrong way"), "10.0.1.2", 6000)
    await settle(dut, 400)
    fwd = tx[1].frames[1:]
    assert [(a, p, l, d) for a, p, l, d in fwd] == [(ip("10.0.1.2"), 6000, len(pub), pub)], fwd
    assert len(tx[0].frames) == 3, "nothing forwarded back to link 0"
    assert await stat(dut, 16, 0) == 1
    assert await stat(dut, 16, 1) == 1
