"""cocotb: Runtime (WasmCore + ImportUnit + BridgeTable) replays the wasm
vectors. Board-visible effects are compared with the model's trace
(publications and log lines leave as frames on pub_*, bridge commands on
the table's ports); results, globals, the final memory CRC and the
executed count must match too."""

import os
import zlib

import cocotb
from cocotb.triggers import ReadOnly, RisingEdge

from mw5a75b.imports import R_DISABLED, R_FALSE, R_NOTDUE, R_NOTICK, R_OK, R_POISONED, R_TRAP
from mw5a75b.wasm import TRAPS
from tb_common import reset, start, vectors
from tb_wasm import NODE, memory_shape, u32, i32

VISIBLE = ("publish ", "log ", "bridge ", "route_stop ", "route_stat ")


class Board:
    def __init__(self, dut):
        self.dut = dut
        self.trace = []
        self.buf = b""
        cocotb.start_soon(self.pubs())
        cocotb.start_soon(self.bridge_cmds())
        cocotb.start_soon(self.dlv_port())
        if os.environ.get("WASM_DEBUG"):
            cocotb.start_soon(self.host_log())

    async def host_log(self):
        dut = self.dut
        while True:
            await ReadOnly()
            if int(dut.host_ack.value):
                dut._log.info("host %d -> %d", int(dut.host_idx.value),
                              i32(int(dut.host_result.value)))
            await RisingEdge(dut.sys_clk)

    async def pubs(self):
        dut = self.dut
        cur = []
        while True:
            dut.pub_ready.value = 1
            await ReadOnly()
            if int(dut.pub_valid.value):
                cur.append(int(dut.pub_data.value))
                if int(dut.pub_last.value):
                    body = bytes(cur)
                    cur = []
                    assert body[0] == 3 and body[1] == 0
                    k = body[2]
                    key, data = body[3:3 + k].decode(), body[3 + k:]
                    assert int(dut.pub_len.value) == len(body)
                    if key == "sys/log":
                        self.trace.append(f"log {data.decode()}")
                    else:
                        self.trace.append(f"publish {key} {data.hex()}")
            await RisingEdge(dut.sys_clk)

    async def bridge_cmds(self):
        dut = self.dut
        key = []
        while True:
            await ReadOnly()
            if int(dut.add_start.value):
                key = []
                src, dst, prefix = (int(dut.add_src.value), int(dut.add_dst.value),
                                    int(dut.add_prefix.value))
            if int(dut.add_stb.value):
                key.append(int(dut.add_byte.value))
            if int(dut.add_end.value):
                text = bytes(key).decode() + ("/**" if prefix else "")
                self.trace.append(f"bridge {src} {text} {dst}")
            if int(dut.rm_stb.value):
                self.trace.append(f"route_stop {int(dut.handle.value)}")
            if int(dut.stat_stb.value):
                names = {0: "Frames", 1: "FramesDropped"}
                self.trace.append(
                    f"route_stat {int(dut.handle.value)} {names[int(dut.stat_dropped.value)]}")
            await RisingEdge(dut.sys_clk)

    async def dlv_port(self):
        dut = self.dut
        while True:
            await RisingEdge(dut.sys_clk)
            a = int(dut.dlv_adr.value)
            dut.dlv_dat.value = self.buf[a] if a < len(self.buf) else 0


async def mem_write(dut, a, data):
    dut.ld_mem_sel.value = 1
    for i, v in enumerate(data):
        dut.ld_mem_adr.value = a + i
        dut.ld_mem_dat.value = v
        dut.ld_mem_we.value = 1
        await RisingEdge(dut.sys_clk)
    dut.ld_mem_we.value = 0
    dut.ld_mem_sel.value = 0


async def event(dut, name, **kw):
    for k, v in kw.items():
        getattr(dut, k).value = v
    getattr(dut, name).value = 1
    await RisingEdge(dut.sys_clk)
    getattr(dut, name).value = 0
    while True:
        await ReadOnly()
        if int(dut.ev_done.value):
            r = int(dut.ev_result.value)
            await RisingEdge(dut.sys_clk)
            return r
        await RisingEdge(dut.sys_clk)


def name_of(r, ok):
    if r >= R_TRAP:
        return f"trap {TRAPS[r - R_TRAP]}"
    if r == R_POISONED:
        return "trap Poisoned"
    return ok.get(r, f"?{r}")


async def sub_slot(dut, key):
    """Streams `key` into subscription matcher 0, as the frame decoder
    does for an incoming publication; returns the slot or None."""
    dut.sub_start.value = 1
    await RisingEdge(dut.sys_clk)
    dut.sub_start.value = 0
    for i, b in enumerate(key):
        dut.sub_stb.value = 1
        dut.sub_idx.value = i
        dut.sub_byte.value = b
        await RisingEdge(dut.sys_clk)
    dut.sub_stb.value = 0
    dut.sub_key_len.value = len(key)
    for _ in range(3):
        await RisingEdge(dut.sys_clk)
    await ReadOnly()
    hit = int(dut.sub_hit.value)
    slot = int(dut.sub_slot.value)
    await RisingEdge(dut.sys_clk)
    return slot if hit else None


async def run_vector(dut, name, board):
    vec = vectors(f"wasm/{name}.json")
    im = vec["image"]
    board.trace = []
    board.buf = b""
    await event_reset(dut)
    # Load the image (what the installer builds).
    for i, w in enumerate(im["code"]):
        dut.ld_code_adr.value, dut.ld_code_dat.value, dut.ld_code_we.value = i, w, 1
        await RisingEdge(dut.sys_clk)
    dut.ld_code_we.value = 0
    for i, (imp, entry, params, results, locals_) in enumerate(im["funcs"]):
        dut.ld_func_adr.value = i
        dut.ld_func_dat.value = imp | entry << 8 | params << 24 | results << 32 | locals_ << 40
        dut.ld_func_we.value = 1
        await RisingEdge(dut.sys_clk)
    dut.ld_func_we.value = 0
    for i, t in enumerate(im["table"]):
        dut.ld_table_adr.value, dut.ld_table_dat.value, dut.ld_table_we.value = i, t, 1
        await RisingEdge(dut.sys_clk)
    dut.ld_table_we.value = 0
    for i in range(16):
        g = im["globals"][i] if i < len(im["globals"]) else 0
        dut.ld_glob_adr.value, dut.ld_glob_dat.value, dut.ld_glob_we.value = i, u32(g), 1
        await RisingEdge(dut.sys_clk)
    dut.ld_glob_we.value = 0
    mem = bytearray(vec["memory_bytes"])
    for a, v in im["memory_nonzero"]:
        mem[a] = v
    await mem_write(dut, 0, mem)
    dut.code_len.value = len(im["code"])
    dut.nfuncs.value = len(im["funcs"])
    dut.table_len.value = len(im["table"])
    dut.ld_mem_len.value = im["memory_len"]
    dut.ld_mem_len_we.value = 1
    await RisingEdge(dut.sys_clk)
    dut.ld_mem_len_we.value = 0
    log2, mx = memory_shape(bytes.fromhex(vec["module"]), vec["memory_bytes"])
    dut.page_log2.value, dut.mem_max.value = log2, mx
    dut.fuel_per_call.value = vec["fuel_per_call"]
    for k, sig in (("init", "exp_init"), ("tick", "exp_tick"), ("mw_alloc", "exp_alloc"),
                   ("on_message", "exp_on_message")):
        f = im["exports"][k]
        getattr(dut, sig).value = 0 if f is None else (256 | f)

    results = [name_of(await event(dut, "ev_init"), {R_OK: "ok"})]
    tick_names = {R_OK: "ran", R_NOTDUE: "notdue", R_DISABLED: "disabled", R_NOTICK: "notick"}
    for st in vec["steps"][1:]:
        if st["op"] == "tick":
            dut.now_ms.value = st["now"]
            results.append(name_of(await event(dut, "ev_tick"), tick_names))
        elif st["op"] == "msg":
            data = bytes.fromhex(st["data"])
            slot = await sub_slot(dut, st["key"].encode())
            if os.environ.get("WASM_DEBUG"):
                dut._log.info("sub %s -> %s count=%s", st["key"], slot, int(dut.sub_count.value))
            if int(dut.poisoned.value):
                results.append("trap Poisoned")
            elif slot is None or len(data) > 512:
                results.append("false")
            else:
                board.buf = data
                r = await event(dut, "ev_deliver", dlv_slot=slot, dlv_len=len(data), dlv_base=0)
                results.append(name_of(r, {R_OK: "true", R_FALSE: "false"}))
        else:
            raise AssertionError(st["op"])
    exp = vec["expect"]
    assert results == exp["results"], f"{name}: results {results} != {exp['results']}"
    want = [t for t in exp["trace"] if t.startswith(VISIBLE)]
    assert board.trace == want, f"{name}: trace\n{board.trace}\n{want}"
    out = []
    dut.ld_mem_sel.value = 1
    for a in range(int(dut.mem_len.value)):
        dut.ld_mem_adr.value = a
        await RisingEdge(dut.sys_clk)
        await ReadOnly()
        out.append(int(dut.mem_rd_dat.value))
        await RisingEdge(dut.sys_clk)
    dut.ld_mem_sel.value = 0
    assert zlib.crc32(bytes(out)) == exp["memory_crc32"], f"{name}: memory"
    assert int(dut.executed.value) == exp["executed"], name


async def event_reset(dut):
    dut.ev_reset.value = 1
    await RisingEdge(dut.sys_clk)
    dut.ev_reset.value = 0
    await RisingEdge(dut.sys_clk)


ZERO = ["ev_reset", "ev_init", "ev_tick", "ev_deliver", "ld_code_we", "ld_func_we",
        "ld_table_we", "ld_glob_we", "ld_mem_len_we", "sub_start", "sub_stb", "now_ms",
        "button", "glob_rd_adr", "ld_mem_sel", "ld_mem_we"]


@cocotb.test()
async def cl7_runtime_vectors(dut):
    only = os.environ.get("WASM_VECTOR")
    first = True
    board = None
    for name in vectors("wasm/index.json"):
        if only and name != only:
            continue
        if first:
            await start(dut, ZERO)
            first = False
        else:
            await reset(dut, ZERO)
        dut.executed.value = 0
        dut.peer_ok.value = 1
        dut.node_id.value = int.from_bytes(NODE, "little")
        if board is None:
            board = Board(dut)
        await run_vector(dut, name, board)
        dut._log.info("vector %s ok", name)
