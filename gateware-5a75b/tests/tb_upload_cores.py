"""cocotb: the CRC-32 and SHA-256 cores reproduce the model's values."""

import cocotb
from cocotb.triggers import ReadOnly, RisingEdge

from mw5a75b.upload import crc32_py
from tb_common import start, vectors


async def clk(dut):
    await RisingEdge(dut.sys_clk)


@cocotb.test()
async def cl6_cores(dut):
    await start(dut, ["crc_init", "crc_stb", "start", "valid", "finish"])
    for chk in vectors("upload.json")["checks"]:
        data = bytes.fromhex(chk["data"])
        assert crc32_py(data) == chk["crc32"]
        dut.crc_init.value = 1
        dut.start.value = 1
        await clk(dut)
        dut.crc_init.value = 0
        dut.start.value = 0
        for b in data:
            dut.crc_stb.value, dut.crc_din.value = 1, b
            await clk(dut)
        dut.crc_stb.value = 0
        await ReadOnly()
        assert int(dut.crc_value.value) == chk["crc32"], len(data)
        await clk(dut)
        i = 0
        while i < len(data):
            dut.valid.value, dut.data.value = 1, data[i]
            await ReadOnly()
            took = int(dut.ready.value)
            await clk(dut)
            if took:
                i += 1
        dut.valid.value = 0
        dut.finish.value = 1
        await clk(dut)
        dut.finish.value = 0
        while True:
            await ReadOnly()
            if int(dut.done.value):
                got = int(dut.digest.value).to_bytes(32, "big").hex()
                assert got == chk["sha256"], f"{len(data)}: {got}"
                break
            await clk(dut)
        await clk(dut)
