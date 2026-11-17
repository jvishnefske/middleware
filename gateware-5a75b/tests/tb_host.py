"""A host on the node's links for the cocotb testbenches: sends request
and publication datagrams, collects what the node transmits."""

import random

import cocotb
from cocotb.triggers import ReadOnly, RisingEdge

from mw5a75b.frame import crc16_py
from flashmodel import FlashModel
from tb_common import reset

ZERO = ["button"] + [f"l{n}_{s}" for n in (0, 1) for s in ("rx_valid", "rx_last")]


def ip(s):
    a = [int(x) for x in s.split(".")]
    return a[0] << 24 | a[1] << 16 | a[2] << 8 | a[3]


def frame(body):
    body = bytes(body)
    return body + crc16_py(body).to_bytes(2, "little")


def request(rid, key, payload=b""):
    return frame([1, rid, len(key)] + list(key) + list(payload))


def publish(key, payload):
    return frame([3, 0, len(key)] + list(key) + list(payload))


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
            ready = self.rng.random() < 0.8
            self.s("tx_ready").value = int(ready)
            await ReadOnly()
            if ready and int(self.s("tx_valid").value):
                buf.append(int(self.s("tx_data").value))
                if int(self.s("tx_last").value):
                    d = bytes(buf)
                    assert int(self.s("tx_len").value) == len(d)
                    assert crc16_py(d[:-2]) == int.from_bytes(d[-2:], "little"), d.hex()
                    self.frames.append((int(self.s("tx_ip").value),
                                        int(self.s("tx_port").value), d[:-2]))
                    buf = []
            await RisingEdge(self.dut.sys_clk)

    def pubs(self, key=None):
        out = []
        for a, p, b in self.frames:
            if b[0] == 3:
                k = b[3:3 + b[2]].decode()
                if key is None or k == key:
                    out.append((a, p, k, b[3 + b[2]:]))
        return out


class Host:
    def __init__(self, dut, seed=1):
        self.dut = dut
        self.rng = random.Random(seed)
        self.tx = [TxTap(dut, 0, self.rng), TxTap(dut, 1, self.rng)]
        self.rid = 0
        self.lock = [cocotb.triggers.Lock(), cocotb.triggers.Lock()]

    async def clocks(self, n):
        for _ in range(n):
            await RisingEdge(self.dut.sys_clk)

    async def send(self, n, data, src_ip="10.0.0.1", src_port=5000):
        async with self.lock[n]:
            dut = self.dut
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

    async def request(self, n, key, body=b"", src_ip="10.0.0.1", src_port=5000,
                      timeout=400_000):
        """Sends a request and waits for its response: (code, payload)."""
        if isinstance(key, str):
            key = key.encode()
        self.rid = (self.rid + 1) & 0xFF
        rid = self.rid
        seen = len(self.tx[n].frames)
        await self.send(n, request(rid, key, body), src_ip, src_port)
        for _ in range(timeout):
            for a, p, b in self.tx[n].frames[seen:]:
                if b[0] == 2 and b[1] == rid:
                    assert (a, p) == (ip(src_ip), src_port), "response to the requester"
                    return b[2], b[3:]
            await RisingEdge(self.dut.sys_clk)
        raise AssertionError(f"no response to {key} after {timeout} cycles")

    async def status(self, n=0):
        code, info = await self.request(n, "sys/info")
        assert code == 0
        return info[6]

    async def wait_pub(self, n, key, count=1, timeout=400_000):
        for _ in range(timeout):
            got = self.tx[n].pubs(key)
            if len(got) >= count:
                return got
            await RisingEdge(self.dut.sys_clk)
        raise AssertionError(f"{key}: {len(self.tx[n].pubs(key))} of {count} on link {n}")


async def boot(dut, seed=1):
    """Starts the clock, the flash model and the host, and resets."""
    from cocotb.clock import Clock
    cocotb.start_soon(Clock(dut.sys_clk, 8, unit="ns").start())
    flash = FlashModel(dut)
    host = Host(dut, seed)
    await reset(dut, ZERO)
    return host, flash
