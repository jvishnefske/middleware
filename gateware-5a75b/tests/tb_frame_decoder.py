"""cocotb: FrameDecoder replays tests/vectors/frame.json."""

import cocotb
from cocotb.triggers import RisingEdge, ReadOnly

from mw5a75b.frame import ERROR_NAMES
from tb_common import start, vectors

async def read_buf(dut, start_at, n):
    out = []
    for a in range(start_at, start_at + n):
        dut.rd_adr.value = a
        await RisingEdge(dut.sys_clk)
        await RisingEdge(dut.sys_clk)
        await ReadOnly()
        out.append(int(dut.rd_dat.value))
        await RisingEdge(dut.sys_clk)
    return bytes(out)


class KeyTap:
    """Collects the key strobes."""

    def __init__(self, dut):
        self.dut = dut
        self.bytes = []
        cocotb.start_soon(self.run())

    async def run(self):
        while True:
            await RisingEdge(self.dut.sys_clk)
            await ReadOnly()
            if int(self.dut.key_stb.value) and int(self.dut.sink_valid.value) \
                    and int(self.dut.sink_ready.value):
                self.bytes.append((int(self.dut.key_idx.value), int(self.dut.key_byte.value)))


async def check_all(dut, gaps):
    await start(dut, ["sink_valid", "sink_data", "sink_last", "consumed", "rd_adr"])
    tap = KeyTap(dut)
    v = vectors("frame.json")
    for f in v["frames"]:
        data = bytes.fromhex(f["datagram"])
        tap.bytes.clear()
        # Drive one byte per cycle while ready.
        for i, b in enumerate(data):
            dut.sink_valid.value = 1
            dut.sink_data.value = b
            dut.sink_last.value = int(i == len(data) - 1)
            await RisingEdge(dut.sys_clk)
            if gaps and i % 3 == 1 and i != len(data) - 1:
                dut.sink_valid.value = 0
                await RisingEdge(dut.sys_clk)
        dut.sink_valid.value = 0
        dut.sink_last.value = 0
        await ReadOnly()
        assert int(dut.done.value) == 1, f["name"]
        name = f["name"]
        ok = bool(int(dut.ok.value))
        err = ERROR_NAMES[int(dut.error.value)]
        assert ok == f["ok"], f"{name}: ok={ok} err={err}"
        if not f["ok"]:
            want = f["error"].split("(")[0]
            assert err == want, f"{name}: {err} != {want}"
        else:
            assert int(dut.kind.value) == f["kind"], name
            assert int(dut.id.value) == f["id"], name
            third = int(dut.third.value)
            blen = int(dut.body_len.value)
            await RisingEdge(dut.sys_clk)
            if f["kind"] == 2:
                assert third == f["status"], name
                payload = await read_buf(dut, 3, blen - 3)
            else:
                key = f["key"].encode()
                assert third == len(key), name
                got_key = await read_buf(dut, 3, third)
                assert got_key == key, name
                assert bytes(b for _, b in tap.bytes) == key, f"{name}: strobes"
                assert [i for i, _ in tap.bytes] == list(range(len(key))), name
                payload = await read_buf(dut, 3 + third, blen - 3 - third)
            assert payload.hex() == f["payload"], name
        # While busy the decoder refuses input; consumed frees it.
        await RisingEdge(dut.sys_clk)
        await ReadOnly()
        assert int(dut.sink_ready.value) == 0
        await RisingEdge(dut.sys_clk)
        dut.consumed.value = 1
        await RisingEdge(dut.sys_clk)
        dut.consumed.value = 0
        await RisingEdge(dut.sys_clk)


@cocotb.test()
async def cl4_frame_vectors_back_to_back(dut):
    await check_all(dut, gaps=False)


@cocotb.test()
async def cl4_frame_vectors_with_idle_cycles(dut):
    await check_all(dut, gaps=True)
