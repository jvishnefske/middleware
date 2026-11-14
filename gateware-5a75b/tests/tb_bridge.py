"""cocotb: BridgeTable replays tests/vectors/bridge.json."""

import cocotb
from cocotb.triggers import RisingEdge, ReadOnly

from tb_common import start, vectors

CMD_INPUTS = ["add_start", "add_src", "add_dst", "add_prefix", "add_len",
              "add_stb", "add_byte", "add_end", "rm_stb", "stat_stb",
              "stat_dropped", "handle", "acct_stb", "acct_rules", "acct_sent"]


def sig(dut, name):
    return getattr(dut, name)


async def clk(dut):
    await RisingEdge(dut.sys_clk)


async def wait_done(dut):
    for _ in range(10):
        await ReadOnly()
        if int(dut.cmd_done.value):
            r = dut.cmd_result.value.to_signed()
            await clk(dut)
            return r
        await clk(dut)
    raise AssertionError("no cmd_done")


async def add(dut, src, key, dst):
    prefix = key.endswith("/**")
    body = key[:-3] if prefix else key
    dut.add_src.value = src
    dut.add_dst.value = dst
    dut.add_prefix.value = int(prefix)
    dut.add_len.value = len(body)
    dut.add_start.value = 1
    await clk(dut)
    dut.add_start.value = 0
    for b in body.encode():
        dut.add_stb.value = 1
        dut.add_byte.value = b
        await clk(dut)
    dut.add_stb.value = 0
    dut.add_end.value = 1
    await clk(dut)
    dut.add_end.value = 0
    return await wait_done(dut)


async def route(dut, src, key):
    s = lambda n: getattr(dut, f"m{src}_{n}")
    s("start").value = 1
    await clk(dut)
    s("start").value = 0
    for i, b in enumerate(key.encode()):
        s("stb").value = 1
        s("idx").value = i
        s("byte").value = b
        if i == len(key) - 1:
            s("end").value = 1
            s("len").value = len(key)
        await clk(dut)
        s("end").value = 0
    s("stb").value = 0
    for _ in range(10):
        await ReadOnly()
        if int(s("valid").value):
            r = int(s("links").value), int(s("rules").value)
            await clk(dut)
            return r
        await clk(dut)
    raise AssertionError("no match result")


@cocotb.test()
async def cl5_bridge_vectors(dut):
    zero = CMD_INPUTS + [f"m{l}_{n}" for l in (0, 1)
                         for n in ("start", "stb", "idx", "byte", "end", "len")]
    await start(dut, zero)
    for op in vectors("bridge.json")["ops"]:
        kind = op["op"]
        if kind == "add":
            got = await add(dut, op["src"], op["key"], op["dst"])
            want = op.get("handle", op.get("error"))
            assert got == want, f"{op}: got {got}"
        elif kind == "route":
            links, rules = await route(dut, op["src"], op["key"])
            assert (links, rules) == (op["links"], op["rules"]), f"{op}: {links} {rules}"
            dut.acct_rules.value = rules
            dut.acct_sent.value = op["sent"]
            dut.acct_stb.value = 1
            await clk(dut)
            dut.acct_stb.value = 0
        elif kind == "stat":
            got = []
            for which in (0, 1):
                dut.handle.value = op["handle"]
                dut.stat_dropped.value = which
                dut.stat_stb.value = 1
                await clk(dut)
                dut.stat_stb.value = 0
                got.append(await wait_done(dut))
            if "error" in op:
                assert got == [op["error"]] * 2, f"{op}: {got}"
            else:
                assert got == [op["forwarded"], op["dropped"]], f"{op}: {got}"
        elif kind == "remove":
            dut.handle.value = op["handle"]
            dut.rm_stb.value = 1
            await clk(dut)
            dut.rm_stb.value = 0
            got = await wait_done(dut)
            assert got == op["result"], f"{op}: {got}"
        else:
            raise AssertionError(kind)
