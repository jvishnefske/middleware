"""cocotb: the WasmCore interpreter replays tests/vectors/wasm/*.json.

The vector's image (what the loader must build) is written through the
`ld_*` ports; the event script runs with `call_start`; `host_req` is
answered by `ImportUnit`, a Python transcription of
`wasm-hdl-ref/src/abi.rs` (the gateware import unit follows the same
rules), against the recording board of `wasm-hdl-ref/tests/common`.
Trace, results, globals, the final memory's CRC-32 and the executed
instruction count must equal the model's.
"""

import os
import zlib

import cocotb
from cocotb.triggers import ReadOnly, RisingEdge

from mw5a75b.wasm import TRAPS
from tb_common import start, vectors

NAMES = [
    "log", "publish", "subscribe", "now_ms", "set_tick", "gpio_output", "gpio_input",
    "gpio_write", "gpio_read", "now_us", "irq_overflow", "gpio_irq", "timer_start",
    "timer_stop", "spi_open", "spi_transfer", "dma_read", "uart_open", "uart_write",
    "uart_read", "pwm_open", "pwm_set", "pwm_stop", "adc_read", "i2c_open",
    "i2c_transfer", "route_source", "route_sink", "route_stop", "route_stat", "serve",
    "node_id", "route_bridge",
]
UNSUPPORTED, INVALID_ARG, OOB, NO_RES, INVALID_KEY, BUSY = -4, -1, -2, -3, -6, -7
MAX_MESSAGE, MAX_LOG, MIN_TICK = 512, 256, 10
NODE = bytes([0x02, 0x5A, 0x75, 0xB0, 0, 1, 0, 0])
PULLS = {0: "None", 1: "Up", 2: "Down"}
STATS = {0: "Frames", 1: "FramesDropped", 2: "Chunks", 3: "ChunksDropped"}


def u32(v):
    return v & 0xFFFF_FFFF


def i32(v):
    v &= 0xFFFF_FFFF
    return v - (1 << 32) if v & 0x8000_0000 else v


def key_ok(b):
    if not 1 <= len(b) <= 128:
        return False
    seg = 0
    for c in b:
        if c == ord("/"):
            if seg == 0:
                return False
            seg = 0
        elif chr(c).isascii() and (chr(c).isalnum() or chr(c) in "_-."):
            seg += 1
        else:
            return False
    return seg != 0


def payload_key(b):
    if not key_ok(b):
        return None
    if b.split(b"/")[0] == b"sys":
        return None
    return b.decode()


def log_line(b):
    b = b[:MAX_LOG]
    try:
        return b.decode()
    except UnicodeDecodeError as e:
        return b[:e.start].decode()


class Bridges:
    """`BridgeTable` (add with de-duplication, remove, stats)."""

    def __init__(self):
        self.rules = [None] * 16

    def add(self, src, filt, dst):
        for i, r in enumerate(self.rules):
            if r and r[:3] == (src, filt, dst):
                return 16 + i
        for i, r in enumerate(self.rules):
            if r is None:
                self.rules[i] = (src, filt, dst, 0, 0)
                return 16 + i
        return NO_RES

    def get(self, h):
        i = h - 16
        return self.rules[i] if 0 <= i < 16 else None

    def remove(self, h):
        if self.get(h) is None:
            return BUSY
        self.rules[h - 16] = None
        return 0


class Board:
    """tests/common/mod.rs `Rec`."""

    def __init__(self):
        self.trace = []
        self.clock = 0
        self.bridges = Bridges()
        self.led = False


class ImportUnit:
    def __init__(self, dut, board, exports):
        self.dut = dut
        self.b = board
        self.exports = exports
        self.subs = []
        self.serves = []
        self.schedule = None  # (period, due)

    async def mem_read(self, a, n):
        out = []
        for i in range(n):
            self.dut.ext_mem_adr.value = a + i
            await RisingEdge(self.dut.sys_clk)
            await ReadOnly()
            out.append(int(self.dut.ext_mem_dat_r.value))
            await RisingEdge(self.dut.sys_clk)
        return bytes(out)

    async def mem_write(self, a, data):
        for i, v in enumerate(data):
            self.dut.ext_mem_adr.value = a + i
            self.dut.ext_mem_dat_w.value = v
            self.dut.ext_mem_we.value = 1
            await RisingEdge(self.dut.sys_clk)
        self.dut.ext_mem_we.value = 0

    def mem_len(self):
        return int(self.dut.mem_len.value)

    def rng(self, p, n):
        p, n = u32(p), u32(n)
        if p + n > self.mem_len():
            return None
        return p, n

    async def guest(self, p, n):
        r = self.rng(p, n)
        if r is None:
            return None
        return await self.mem_read(*r)

    async def call(self, name, a):
        b = self.b
        if name == "log":
            data = await self.guest(a[0], a[1])
            if data is not None:
                b.trace.append(f"log {log_line(data)}")
            return 0
        if name == "publish":
            k = await self.guest(a[0], a[1])
            if k is None:
                return OOB
            d = await self.guest(a[2], a[3])
            if d is None:
                return OOB
            if len(d) > MAX_MESSAGE:
                return INVALID_ARG
            key = payload_key(k)
            if key is None:
                return INVALID_KEY
            b.trace.append(f"publish {key} {d.hex()}")
            return 0
        if name in ("subscribe", "serve"):
            k = await self.guest(a[0], a[1])
            if k is None:
                return OOB
            key = payload_key(k)
            if key is None:
                return INVALID_KEY
            need = "on_message" if name == "subscribe" else "on_request"
            if self.exports["mw_alloc"] is None or self.exports[need] is None:
                return UNSUPPORTED
            table = self.subs if name == "subscribe" else self.serves
            if key in table:
                return table.index(key)
            if len(table) >= 8:
                return NO_RES
            if name == "serve":
                b.trace.append(f"serve {key}")
            table.append(key)
            return len(table) - 1
        if name == "now_ms":
            return i32(b.clock)
        if name == "set_tick":
            p = i32(a[0])
            if p == 0:
                self.schedule = None
            elif p < MIN_TICK:
                return INVALID_ARG
            elif self.exports["tick"] is None:
                return UNSUPPORTED
            else:
                self.schedule = (p, u32(b.clock + p))
            return 0
        if name in ("gpio_output", "gpio_input", "gpio_write", "gpio_read"):
            pin = i32(a[0])
            if not 0 <= pin <= 255:
                return INVALID_ARG
            if name == "gpio_output":
                b.trace.append(f"gpio_output {pin}")
                return 0 if pin < 2 else UNSUPPORTED
            if name == "gpio_input":
                pull = i32(a[1])
                if pull not in PULLS:
                    return INVALID_ARG
                b.trace.append(f"gpio_input {pin} {PULLS[pull]}")
                return 0 if pin < 2 else UNSUPPORTED
            if name == "gpio_write":
                lvl = i32(a[1])
                if lvl not in (0, 1):
                    return INVALID_ARG
                b.trace.append(f"gpio_write {pin} {'true' if lvl else 'false'}")
                b.led = bool(lvl)
                return 0 if pin < 2 else UNSUPPORTED
            b.trace.append(f"gpio_read {pin}")
            return int(b.led)
        if name == "node_id":
            r = self.rng(a[0], 8)
            if r is None:
                return OOB
            b.trace.append("node_id")
            await self.mem_write(r[0], NODE)
            return 0
        if name == "route_bridge":
            src, dst = i32(a[0]), i32(a[3])
            if not (0 <= src < 8 and 0 <= dst < 8) or src == dst:
                return INVALID_ARG
            k = await self.guest(a[1], a[2])
            if k is None:
                return OOB
            prefix = k.endswith(b"/**")
            body = k[:-3] if prefix else k
            if not key_ok(body) or body == b"sys" or body.startswith(b"sys/"):
                return INVALID_KEY
            text = body.decode() + ("/**" if prefix else "")
            b.trace.append(f"bridge {src} {text} {dst}")
            if src > 1 or dst > 1:
                return UNSUPPORTED
            return b.bridges.add(src, text, dst)
        if name in ("route_stop", "route_stat"):
            h = i32(a[0])
            if not (0 <= h < 4 or 16 <= h < 32):
                return INVALID_ARG
            if name == "route_stop":
                b.trace.append(f"route_stop {h}")
                return b.bridges.remove(h)
            st = i32(a[1])
            if st not in STATS:
                return INVALID_ARG
            b.trace.append(f"route_stat {h} {STATS[st]}")
            r = b.bridges.get(h)
            if r is None:
                return BUSY
            return r[3] if st in (0, 2) else r[4]
        return UNSUPPORTED


class Runner:
    def __init__(self, dut, vec):
        self.dut = dut
        self.vec = vec
        self.exports = vec["image"]["exports"]
        self.board = Board()
        self.unit = ImportUnit(dut, self.board, self.exports)
        self.poisoned = False

    async def load(self):
        dut, im = self.dut, self.vec["image"]
        for i, w in enumerate(im["code"]):
            dut.ld_code_adr.value = i
            dut.ld_code_dat.value = w
            dut.ld_code_we.value = 1
            await RisingEdge(dut.sys_clk)
        dut.ld_code_we.value = 0
        for i, (imp, entry, params, results, locals_) in enumerate(im["funcs"]):
            dut.ld_func_adr.value = i
            dut.ld_func_dat.value = (imp | entry << 8 | params << 24 | results << 32
                                     | locals_ << 40)
            dut.ld_func_we.value = 1
            await RisingEdge(dut.sys_clk)
        dut.ld_func_we.value = 0
        for i, t in enumerate(im["table"]):
            dut.ld_table_adr.value = i
            dut.ld_table_dat.value = t
            dut.ld_table_we.value = 1
            await RisingEdge(dut.sys_clk)
        dut.ld_table_we.value = 0
        for i in range(16):
            g = im["globals"][i] if i < len(im["globals"]) else 0
            dut.ld_glob_adr.value = i
            dut.ld_glob_dat.value = u32(g)
            dut.ld_glob_we.value = 1
            await RisingEdge(dut.sys_clk)
        dut.ld_glob_we.value = 0
        mem = bytearray(self.vec["memory_bytes"])
        for a, v in im["memory_nonzero"]:
            mem[a] = v
        await self.unit.mem_write(0, mem)
        assert zlib.crc32(bytes(mem[:im["memory_len"]])) == im["memory_crc32"]
        dut.code_len.value = len(im["code"])
        dut.nfuncs.value = len(im["funcs"])
        dut.table_len.value = len(im["table"])
        dut.ld_mem_len.value = im["memory_len"]
        dut.ld_mem_len_we.value = 1
        await RisingEdge(dut.sys_clk)
        dut.ld_mem_len_we.value = 0
        # Page size and maximum from the module's memory section.
        log2, mx = memory_shape(bytes.fromhex(self.vec["module"]), self.vec["memory_bytes"])
        dut.page_log2.value = log2
        dut.mem_max.value = mx
        dut.fuel_per_call.value = self.vec["fuel_per_call"]

    async def invoke(self, func, args):
        """Runs one export call; returns ("ok", result) or ("trap", name)."""
        if self.poisoned:
            return ("trap", "Poisoned")
        dut = self.dut
        dut.call_func.value = func
        dut.call_nargs.value = len(args)
        for i, v in enumerate(args):
            getattr(dut, f"call_arg{i}").value = u32(v)
        dut.call_start.value = 1
        await RisingEdge(dut.sys_clk)
        dut.call_start.value = 0
        while True:
            await ReadOnly()
            if int(dut.call_done.value):
                trap = int(dut.trap.value)
                res = i32(int(dut.call_result.value))
                await RisingEdge(dut.sys_clk)
                if trap:
                    self.poisoned = True
                    return ("trap", TRAPS[trap])
                return ("ok", res)
            if int(dut.host_req.value):
                idx = int(dut.host_idx.value)
                args = [int(getattr(dut, f"host_arg{i}").value) for i in range(8)]
                await RisingEdge(dut.sys_clk)
                r = await self.unit.call(NAMES[idx], args)
                if os.environ.get("WASM_DEBUG"):
                    dut._log.info("host %s %s -> %s mem_len=%s", NAMES[idx], args, r,
                                  int(dut.mem_len.value))
                dut.host_result.value = u32(r)
                dut.host_ack.value = 1
                await RisingEdge(dut.sys_clk)
                dut.host_ack.value = 0
                continue
            await RisingEdge(dut.sys_clk)

    async def init(self):
        f = self.exports["init"]
        if f is None:
            return "ok"
        kind, r = await self.invoke(f, [])
        return "ok" if kind == "ok" else f"trap {r}"

    async def tick(self, now):
        self.board.clock = now
        if self.poisoned:
            return "trap Poisoned"
        f = self.exports["tick"]
        if f is None:
            return "notick"
        s = self.unit.schedule
        if s is None:
            return "disabled"
        period, due = s
        if i32(now - due) < 0:
            return "notdue"
        nxt = u32(due + period)
        self.unit.schedule = (period, u32(now + period) if i32(now - nxt) >= 0 else nxt)
        kind, r = await self.invoke(f, [i32(now)])
        return "ran" if kind == "ok" else f"trap {r}"

    async def deliver(self, key, data):
        if self.poisoned:
            return "trap Poisoned"
        if key not in self.unit.subs:
            return "false"
        slot = self.unit.subs.index(key)
        if len(data) > MAX_MESSAGE:
            return "false"
        alloc, onm = self.exports["mw_alloc"], self.exports["on_message"]
        if alloc is None or onm is None:
            return "false"
        kind, ptr = await self.invoke(alloc, [len(data)])
        if os.environ.get("WASM_DEBUG"):
            self.dut._log.info("mw_alloc(%d) -> %s %s", len(data), kind, ptr)
        if kind != "ok":
            return f"trap {ptr}"
        if ptr == 0 or self.unit.rng(ptr, len(data)) is None:
            return "false"
        await self.unit.mem_write(u32(ptr), data)
        kind, r = await self.invoke(onm, [slot, ptr, len(data)])
        return "true" if kind == "ok" else f"trap {r}"

    async def final_memory(self):
        return await self.unit.mem_read(0, int(self.dut.mem_len.value))

    async def globals_(self):
        out = []
        for i in range(len(self.vec["image"]["globals"])):
            self.dut.glob_rd_adr.value = i
            await ReadOnly()
            out.append(i32(int(self.dut.glob_rd_dat.value)))
            await RisingEdge(self.dut.sys_clk)
        return out


def leb(b, at):
    v = shift = 0
    while True:
        x = b[at]
        at += 1
        v |= (x & 0x7F) << shift
        shift += 7
        if not x & 0x80:
            return v, at


def memory_shape(wasm, cap):
    """(page_log2, max bytes) from the memory section, as the loader
    computes them."""
    at = 8
    while at < len(wasm):
        sid = wasm[at]
        size, body = leb(wasm, at + 1)
        if sid == 5:
            _, p = leb(wasm, body)
            flags, p = leb(wasm, p)
            _, p = leb(wasm, p)
            mx = None
            if flags & 1:
                mx, p = leb(wasm, p)
            log2 = 16
            if flags & 8:
                log2, p = leb(wasm, p)
            return log2, cap if mx is None else min(mx << log2, cap)
        at = body + size
    return 16, cap


async def run_vector(dut, name):
    vec = vectors(f"wasm/{name}.json")
    r = Runner(dut, vec)
    await r.load()
    results = [await r.init()]
    for st in vec["steps"][1:]:
        if st["op"] == "tick":
            results.append(await r.tick(st["now"]))
        elif st["op"] == "msg":
            results.append(await r.deliver(st["key"], bytes.fromhex(st["data"])))
        else:
            raise AssertionError(f"{name}: step {st['op']} not modelled here")
    exp = vec["expect"]
    assert results == exp["results"], f"{name}: results {results} != {exp['results']}"
    assert r.board.trace == exp["trace"], f"{name}: trace\n{r.board.trace}\n{exp['trace']}"
    assert await r.globals_() == exp["globals"], f"{name}: globals"
    mem = await r.final_memory()
    assert zlib.crc32(mem) == exp["memory_crc32"], f"{name}: memory"
    assert int(dut.executed.value) == exp["executed"], \
        f"{name}: executed {int(dut.executed.value)} != {exp['executed']}"


ZERO = ["call_start", "call_func", "call_nargs", "host_ack", "host_result",
        "ext_mem_adr", "ext_mem_dat_w", "ext_mem_we", "ld_code_we", "ld_func_we",
        "ld_table_we", "ld_glob_we", "glob_rd_adr", "ld_mem_len_we"]


@cocotb.test()
async def cl7_wasm_vectors(dut):
    names = vectors("wasm/index.json")
    only = os.environ.get("WASM_VECTOR")
    for name in names:
        if only and name != only:
            continue
        await start(dut, ZERO)
        dut.executed.value = 0
        await run_vector(dut, name)
        dut._log.info("vector %s ok", name)
