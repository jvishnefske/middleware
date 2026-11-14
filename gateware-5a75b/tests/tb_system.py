"""cocotb: the loader builds every vector's image from the module bytes
alone (compared with the model's image), then the vector's script runs on
it; every refused module fails with the model's error kind."""

import os
import zlib

import cocotb
from cocotb.triggers import ReadOnly, RisingEdge

from mw5a75b.imports import R_DISABLED, R_FALSE, R_NOTDUE, R_NOTICK, R_OK
from mw5a75b.loader import ERRORS
from tb_common import reset, start, vectors
from tb_runtime import VISIBLE, Board, event, name_of, sub_slot
from tb_wasm import NODE, i32

ZERO = ["ev_reset", "ev_init", "ev_tick", "ev_deliver", "sub_start", "sub_stb", "now_ms",
        "button", "glob_rd_adr", "tb_mem_sel", "img_we", "start", "dbg_code_adr",
        "dbg_func_adr", "dbg_table_adr"]


async def clk(dut):
    await RisingEdge(dut.sys_clk)


async def load(dut, wasm):
    for i, v in enumerate(wasm):
        dut.img_adr.value, dut.img_dat.value, dut.img_we.value = i, v, 1
        await clk(dut)
    dut.img_we.value = 0
    dut.img_len.value = len(wasm)
    dut.start.value = 1
    await clk(dut)
    dut.start.value = 0
    while True:
        await ReadOnly()
        if int(dut.done.value):
            e = ERRORS[int(dut.error.value)]
            await clk(dut)
            return e
        await clk(dut)


async def rd(dut, sig_adr, sig_dat, a):
    getattr(dut, sig_adr).value = a
    await clk(dut)
    await ReadOnly()
    v = int(getattr(dut, sig_dat).value)
    await clk(dut)
    return v


async def check_image(dut, name, im):
    assert int(dut.code_len.value) == len(im["code"]), f"{name}: code_len"
    got_code = [await rd(dut, "dbg_code_adr", "dbg_code_dat", a) for a in range(len(im["code"]))]
    for a, w in enumerate(im["code"]):
        assert got_code[a] == w, (f"{name}: code[{a}] {got_code[a]:#010x} != {w:#010x}\n"
                                  f"got  {[hex(x) for x in got_code[max(0,a-6):a+6]]}\n"
                                  f"want {[hex(x) for x in im['code'][max(0,a-6):a+6]]}")
    assert int(dut.nfuncs.value) == len(im["funcs"]), f"{name}: nfuncs"
    for a, (imp, entry, params, results, locals_) in enumerate(im["funcs"]):
        want = imp | entry << 8 | params << 24 | results << 32 | locals_ << 40
        got = await rd(dut, "dbg_func_adr", "dbg_func_dat", a)
        assert got == want, f"{name}: func[{a}] {got:#x} != {want:#x}"
    assert int(dut.table_len.value) == len(im["table"]), f"{name}: table_len"
    for a, t in enumerate(im["table"]):
        got = await rd(dut, "dbg_table_adr", "dbg_table_dat", a)
        assert got == t, f"{name}: table[{a}]"
    for a, g in enumerate(im["globals"]):
        dut.glob_rd_adr.value = a
        await ReadOnly()
        assert i32(int(dut.glob_rd_dat.value)) == g, f"{name}: global[{a}]"
        await clk(dut)
    for k, sig in (("init", "exp_init"), ("tick", "exp_tick"), ("mw_alloc", "exp_mw_alloc"),
                   ("on_message", "exp_on_message"), ("on_request", "exp_on_request")):
        f = im["exports"][k]
        got = int(getattr(dut, sig).value)
        assert got == (0 if f is None else 256 | f), f"{name}: export {k} {got}"
    assert int(dut.mem_len.value) == im["memory_len"], f"{name}: mem_len"
    mem = await read_memory(dut, im["memory_len"])
    assert zlib.crc32(mem) == im["memory_crc32"], f"{name}: initial memory"


async def read_memory(dut, n):
    out = []
    dut.tb_mem_sel.value = 1
    for a in range(n):
        dut.tb_mem_adr.value = a
        await clk(dut)
        await ReadOnly()
        out.append(int(dut.mem_rd_dat.value))
        await clk(dut)
    dut.tb_mem_sel.value = 0
    return bytes(out)


async def run_script(dut, name, vec, board):
    board.trace = []
    dut.now_ms.value = 0
    dut.ev_reset.value = 1
    await clk(dut)
    dut.ev_reset.value = 0
    await clk(dut)
    dut.fuel_per_call.value = vec["fuel_per_call"]
    results = [name_of(await event(dut, "ev_init"), {R_OK: "ok"})]
    tick_names = {R_OK: "ran", R_NOTDUE: "notdue", R_DISABLED: "disabled", R_NOTICK: "notick"}
    for st in vec["steps"][1:]:
        if st["op"] == "tick":
            dut.now_ms.value = st["now"]
            results.append(name_of(await event(dut, "ev_tick"), tick_names))
        else:
            data = bytes.fromhex(st["data"])
            slot = await sub_slot(dut, st["key"].encode())
            if int(dut.poisoned.value):
                results.append("trap Poisoned")
            elif slot is None or len(data) > 512:
                results.append("false")
            else:
                board.buf = data
                r = await event(dut, "ev_deliver", dlv_slot=slot, dlv_len=len(data), dlv_base=0)
                results.append(name_of(r, {R_OK: "true", R_FALSE: "false"}))
    exp = vec["expect"]
    assert results == exp["results"], f"{name}: results {results} != {exp['results']}"
    want = [t for t in exp["trace"] if t.startswith(VISIBLE)]
    assert board.trace == want, f"{name}: trace\n{board.trace}\n{want}"
    mem = await read_memory(dut, int(dut.mem_len.value))
    assert zlib.crc32(mem) == exp["memory_crc32"], f"{name}: memory"


@cocotb.test()
async def cl6_loader_builds_every_image_and_it_runs(dut):
    await start(dut, ZERO)
    dut.peer_ok.value = 1
    dut.node_id.value = int.from_bytes(NODE, "little")
    board = Board(dut)
    only = os.environ.get("WASM_VECTOR")
    for name in vectors("wasm/index.json"):
        if only and name != only:
            continue
        vec = vectors(f"wasm/{name}.json")
        err = await load(dut, bytes.fromhex(vec["module"]))
        assert err == "Ok", f"{name}: {err}"
        await check_image(dut, name, vec["image"])
        await run_script(dut, name, vec, board)
        dut._log.info("vector %s ok", name)


@cocotb.test()
async def cl6_refused_modules_fail_like_the_model(dut):
    await start(dut, ZERO)
    for case in vectors("loaderr.json")["cases"]:
        err = await load(dut, bytes.fromhex(case["module"]))
        assert err == case["error"], f"{case['name']}: {err} != {case['error']}"
