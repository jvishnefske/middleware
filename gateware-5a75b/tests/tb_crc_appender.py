"""cocotb: CrcAppender turns each valid vector's body into its datagram."""

import random

import cocotb
from cocotb.triggers import RisingEdge, ReadOnly

from tb_common import start, vectors


@cocotb.test()
async def cl4_bodies_get_their_crc(dut):
    await start(dut, ["sink_valid", "sink_data", "sink_last", "source_ready"])
    rng = random.Random(7)
    for f in vectors("frame.json")["frames"]:
        if not f["ok"]:
            continue
        dgram = bytes.fromhex(f["datagram"])
        body = dgram[:-2]
        out = []
        i = 0
        done = False
        while not done:
            dut.sink_valid.value = int(i < len(body))
            dut.sink_data.value = body[i] if i < len(body) else 0
            dut.sink_last.value = int(i == len(body) - 1)
            ready = rng.random() < 0.8
            dut.source_ready.value = int(ready)
            await ReadOnly()
            took = int(dut.sink_valid.value) and int(dut.sink_ready.value)
            if int(dut.source_valid.value) and ready:
                out.append(int(dut.source_data.value))
                done = bool(int(dut.source_last.value))
            await RisingEdge(dut.sys_clk)
            if took:
                i += 1
        assert bytes(out) == dgram, f["name"]
