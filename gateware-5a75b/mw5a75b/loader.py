"""The loader (CL6): compiles the uploaded module into the WASM core's RAMs,
check for check and word for word as `wasm-hdl-ref/src/load.rs` does.

The module sits in a 16 KiB image RAM (`img_*` write port: the upload path
or a testbench). `start` resets the core's image (memory zeroed, table 0
emptied), then one pass over the module:

* a `Reader` fetches bytes and LEB128 values (u32, s32, s64) under a
  bound (`limit`: the module, a section, a function body), so reading
  past the end of any of them is `Malformed`, as the model's nested
  readers make it;
* the sections in their binary order (type, import, function, table,
  memory, global, export, element, data count, code, data; custom
  sections skipped) fill the type table, the function table (`ld_func`),
  table 0 (`ld_table`), the globals (`ld_glob`), the exports and linear
  memory (data segments), with the model's limits;
* each body is compiled into `ld_code` words: a control stack (64
  entries, LUT RAM) and an operand-type stack (256 entries) give every
  branch its target, keep and drop; forward branches are chained through
  their own target fields and patched at `end` by reading the code back
  (`dbg_code_*`).

`done` pulses with `error` (`ERRORS`; 0 when loaded). On success the
outputs (`code_len`, `nfuncs`, `table_len`, `mem_len`, `mem_max`,
`page_log2`, `exp_*`) configure the core and the runtime.
"""

from migen import (
    Array, Case, Cat, Constant, FSM, If, Memory, Module, Mux, NextState, NextValue, Signal,
)

from .wasm import import_shapes

ERRORS = {0: "Ok", 1: "Malformed", 2: "Unsupported", 3: "TooLarge", 4: "UnknownImport",
          5: "BadSignature", 6: "BadExport", 7: "MissingMemory", 8: "MemoryTooLarge"}
E_MALFORMED, E_UNSUPPORTED, E_TOO_LARGE, E_UNKNOWN_IMPORT = 1, 2, 3, 4
E_BAD_SIGNATURE, E_BAD_EXPORT, E_MISSING_MEMORY, E_MEMORY_TOO_LARGE = 5, 6, 7, 8

IMPORT_NAMES = [
    "log", "publish", "subscribe", "now_ms", "set_tick", "gpio_output", "gpio_input",
    "gpio_write", "gpio_read", "now_us", "irq_overflow", "gpio_irq", "timer_start",
    "timer_stop", "spi_open", "spi_transfer", "dma_read", "uart_open", "uart_write",
    "uart_read", "pwm_open", "pwm_set", "pwm_stop", "adc_read", "i2c_open",
    "i2c_transfer", "route_source", "route_sink", "route_stop", "route_stat", "serve",
    "node_id", "route_bridge",
]
# Exports the runtime calls, with (params, results).
EXPORT_NAMES = [("init", 0, 0), ("tick", 1, 0), ("mw_alloc", 1, 1), ("on_message", 3, 0),
                ("on_irq", 4, 0), ("on_request", 6, 1), ("memory", None, None)]

CODE_WORDS, MAX_FUNCS, MAX_TYPES, MAX_PARAMS = 4096, 128, 32, 8
MAX_GLOBALS, MAX_TABLE, MAX_LOCALS, MAX_CTRL, MAX_OPERANDS = 16, 64, 256, 64, 256
NONE = 0xFFFF
LOCAL = 0xFF
I32, I64 = 0x7F, 0x7E
K_FUNC, K_BLOCK, K_LOOP, K_IF = 0, 1, 2, 3

# ISA (wasm-hdl-ref/src/isa.rs).
UNREACHABLE, CONST, CONST32, DROP, SELECT = 0x00, 0x01, 0x02, 0x03, 0x04
LGET, LSET, LTEE, GGET, GSET = 0x05, 0x06, 0x07, 0x08, 0x09
LOAD, STORE, MSIZE, MGROW, MCOPY, MFILL = 0x0A, 0x0B, 0x0C, 0x0D, 0x0E, 0x0F
UN, BIN, BR, BRIF, BRUNLESS, BRTABLE = 0x10, 0x11, 0x12, 0x13, 0x14, 0x15
CALL, CALLI, HOST, RET = 0x16, 0x17, 0x18, 0x19

R_BYTE, R_U32, R_S32, R_S64 = 0, 1, 2, 3


def word(op, a):
    """An instruction word: the argument zero-extended to 24 bits, then op."""
    if isinstance(a, int):
        a = Constant(a, 24)
    w = len(a)
    arg = a[:24] if w >= 24 else Cat(a, Constant(0, 24 - w))
    return Cat(arg, Constant(op, 8))


class Reader(Module):
    """Bytes and LEB128 values from the image RAM, bounded by `limit`."""

    def __init__(self, rd):
        self.go = Signal()
        self.cmd = Signal(2)
        self.pos = Signal(15)
        self.limit = Signal(15)
        self.set_pos = Signal()
        self.pos_in = Signal(15)
        self.done = Signal()
        self.err = Signal()
        self.val = Signal(64)
        self.byte = Signal(8)
        # Another reader of the RAM while this one is idle.
        self.ext_sel = Signal()
        self.ext_adr = Signal(14)

        acc = Signal(64)
        n = Signal(4)
        cmd = Signal(2)
        b = rd.dat_r
        chunk = b[:7]
        self.comb += rd.adr.eq(Mux(self.ext_sel, self.ext_adr, self.pos[:14]))
        fsm = FSM(reset_state="IDLE")
        self.submodules += fsm
        maxb = Signal(4)
        self.comb += maxb.eq(Mux(cmd == R_S64, 10, 5))
        ext = Signal(64)       # acc with this byte's 7 bits in place
        self.comb += Case(n, {i: ext.eq(acc | (Cat(chunk, Constant(0, 57)) << (7 * i))[:64])
                              for i in range(10)})
        signed = Signal(64)    # ext sign-extended past the last chunk
        self.comb += Case(n, {i: signed.eq(Mux(b[6], ext | ((1 << 64) - (1 << 7 * (i + 1))), ext))
                                 if 7 * (i + 1) < 64 else signed.eq(ext)
                              for i in range(10)})
        fits_u32 = Signal()
        fits_i32 = Signal()
        self.comb += [
            fits_u32.eq(ext[32:] == 0),
            fits_i32.eq((signed[31:] == 0) | (signed[31:] == (1 << 33) - 1)),
        ]
        self.sync += If(self.set_pos, self.pos.eq(self.pos_in))
        fsm.act("IDLE",
            If(self.go,
                NextValue(cmd, self.cmd), NextValue(acc, 0), NextValue(n, 0),
                NextState("FETCH"),
            ),
        )
        fsm.act("FETCH",
            If(self.pos >= self.limit,
                self.done.eq(1), self.err.eq(1), NextState("IDLE"),
            ).Else(NextState("DATA")),
        )
        fsm.act("DATA",
            NextValue(self.pos, self.pos + 1),
            NextValue(self.byte, b),
            If(cmd == R_BYTE,
                NextValue(self.val, b),
                NextState("OK"),
            ).Elif(b[7],
                If(n + 1 == maxb,
                    NextState("BAD"),
                ).Else(
                    NextValue(acc, ext), NextValue(n, n + 1), NextState("FETCH"),
                ),
            ).Else(
                If(cmd == R_U32,
                    If(fits_u32, NextValue(self.val, ext), NextState("OK")).Else(NextState("BAD")),
                ).Elif(cmd == R_S32,
                    If(fits_i32, NextValue(self.val, signed), NextState("OK")).Else(NextState("BAD")),
                ).Else(
                    NextValue(self.val, signed), NextState("OK"),
                ),
            ),
        )
        fsm.act("OK", self.done.eq(1), NextState("IDLE"))
        fsm.act("BAD", self.done.eq(1), self.err.eq(1), NextState("IDLE"))


class NameMatch(Module):
    """Streams a name against constant names: `hit` one-hot at the end."""

    def __init__(self, names):
        self.start = Signal()
        self.stb = Signal()
        self.idx = Signal(16)
        self.byte = Signal(8)
        self.length = Signal(32)
        self.hit = Signal(len(names))
        alive = Signal(len(names))
        for k, nm in enumerate(names):
            rom = Array(Constant(c, 8) for c in nm.encode())
            bits = max(1, (len(nm) - 1).bit_length())
            self.sync += If(self.start, alive[k].eq(1)).Elif(
                self.stb & ((self.idx >= len(nm)) | (self.byte != rom[self.idx[:bits]])),
                alive[k].eq(0))
            self.comb += self.hit[k].eq(alive[k] & (self.length == len(nm)))


class Utf8Valid(Module):
    """Whether the streamed bytes are whole UTF-8 (`str::from_utf8`)."""

    def __init__(self):
        from .imports import Utf8Cut
        self.submodules.cut = cut = Utf8Cut()
        self.start, self.stb, self.byte = cut.start, cut.stb, cut.byte
        self.length = Signal(32)
        self.valid = Signal()
        self.comb += self.valid.eq(cut.ok)


class Loader(Module):
    def __init__(self, core, mem_bytes=16 * 1024, image_bytes=16 * 1024):
        aw = (mem_bytes - 1).bit_length()
        # Image RAM.
        self.img_adr = Signal(14)
        self.img_dat = Signal(8)
        self.img_we = Signal()
        self.img_len = Signal(15)
        # Image RAM read port for others while the loader is idle
        # (synchronous: data a cycle after the address).
        self.img_rd_sel = Signal()
        self.img_rd_adr = Signal(14)
        self.img_rd_dat = Signal(8)
        # Control.
        self.start = Signal()
        self.busy = Signal()
        self.done = Signal()
        self.error = Signal(4)
        # Linear memory (through the runtime's ld_mem port).
        self.mem_sel = Signal()
        self.mem_adr = Signal(aw)
        self.mem_dat = Signal(8)
        self.mem_we = Signal()
        # Results.
        self.code_len = Signal(13)
        self.nfuncs = Signal(8)
        self.table_len = Signal(7)
        self.mem_len = Signal(aw + 2)
        self.mem_max = Signal(aw + 2)
        self.page_log2 = Signal(5)
        self.exp = Array(Signal(9, name=f"exp_{n}") for n, _, _ in EXPORT_NAMES[:6])
        self.exp_init, self.exp_tick, self.exp_alloc, self.exp_on_message = (
            self.exp[0], self.exp[1], self.exp[2], self.exp[3])
        self.exp_on_irq, self.exp_on_request = self.exp[4], self.exp[5]

        # # #

        img = Memory(8, image_bytes)
        self.specials += img
        iw = img.get_port(write_capable=True)
        ir = img.get_port(has_re=False)
        self.specials += iw, ir
        self.comb += iw.adr.eq(self.img_adr), iw.dat_w.eq(self.img_dat), iw.we.eq(self.img_we)
        self.submodules.rd = rd = Reader(ir)
        self.comb += [
            rd.ext_sel.eq(self.img_rd_sel & ~self.busy), rd.ext_adr.eq(self.img_rd_adr),
            self.img_rd_dat.eq(ir.dat_r),
        ]
        val = rd.val
        v32 = val[:32]

        shapes = import_shapes()
        imp_params = Array(Constant(p, 4) for p, _ in shapes)
        imp_returns = Array(Constant(r, 1) for _, r in shapes)

        # Module state.
        types_p = Array(Signal(4, name=f"ty_p{i}") for i in range(MAX_TYPES))
        types_r = Array(Signal(1, name=f"ty_r{i}") for i in range(MAX_TYPES))
        ntypes = Signal(6)
        nimports = Signal(8)
        nglobals = Signal(5)
        has_table = Signal()
        has_memory = Signal()
        memory_exported = Signal()
        bodies = Signal(8)
        last = Signal(4)
        # Function shadow: params(4) results(1) import(8).
        fsh = Memory(13, MAX_FUNCS)
        self.specials += fsh
        fsh_w = fsh.get_port(write_capable=True)
        fsh_r = fsh.get_port(async_read=True)
        self.specials += fsh_w, fsh_r
        f_params = fsh_r.dat_r[0:4]
        f_results = fsh_r.dat_r[4]
        f_import = fsh_r.dat_r[5:13]

        # Counters and scratch.
        sid = Signal(8)
        sec_end = Signal(15)
        n = Signal(32)
        i = Signal(32)
        j = Signal(32)
        cnt = Signal(32)
        tmp = Signal(32)
        tmp2 = Signal(32)
        tp = Signal(4)
        tr = Signal(1)
        imp_idx = Signal(6)
        mod_ok = Signal()
        flags = Signal(32)
        mn = Signal(32)
        mx = Signal(32)
        log2 = Signal(5)
        name_len = Signal(32)
        name_phase = Signal(2)
        kind = Signal(8)
        body_end = Signal(15)
        locals_ = Signal(10)   # saturating at 512
        fi = Signal(8)

        mods = NameMatch(["mw"])
        imps = NameMatch(IMPORT_NAMES)
        exps = NameMatch([nm for nm, _, _ in EXPORT_NAMES])
        utf = Utf8Valid()
        self.submodules += mods, imps, exps, utf

        # Compiler state.
        ctrl = Memory(72, MAX_CTRL)
        self.specials += ctrl
        c_w = ctrl.get_port(write_capable=True)
        c_top = ctrl.get_port(async_read=True)
        c_lab = ctrl.get_port(async_read=True)
        c_first = ctrl.get_port(async_read=True)
        self.specials += c_w, c_top, c_lab, c_first
        nctrl = Signal(7)
        self.comb += c_top.adr.eq(nctrl - 1), c_first.adr.eq(0)

        def fields(w):
            return dict(kind=w[0:2], results=w[2], vals=w[3:12], slots=w[12:22],
                        start=w[22:38], fixups=w[38:54], unless=w[54:70],
                        dead=w[70], outer_dead=w[71])

        top = fields(c_top.dat_r)
        lab = fields(c_lab.dat_r)
        first = fields(c_first.dat_r)

        def pack(kind, results, vals, slots, start, fixups, unless, dead, outer_dead):
            return Cat(kind[:2] if not isinstance(kind, int) else Constant(kind, 2),
                       results, vals[:9], slots[:10], start[:16], fixups[:16], unless[:16],
                       dead, outer_dead)

        tys = Signal(MAX_OPERANDS)   # 1 = I64
        nvals = Signal(9)
        slots = Signal(10)
        clen = Signal(13)
        nlocals = Signal(10)
        dead = Signal()
        self.comb += dead.eq(top["dead"])
        opc = Signal(8)
        top_t = Signal()             # type of the top operand (1: I64)
        self.comb += top_t.eq((tys >> (nvals - 1))[0])
        wd = Signal(32)              # word to emit
        wd2 = Signal(32)
        resume = Signal(5)           # where EMIT2 / POPS continue

        # Branch arguments for `label(depth)`.
        depth = Signal(32)
        lab_i = Signal(7)
        self.comb += lab_i.eq(nctrl - 1 - depth[:7])
        self.comb += c_lab.adr.eq(lab_i)
        lab_keep = Signal()
        lab_drop = Signal(11)
        lab_ok = Signal()            # depth in range, drop non-negative
        lab_target = Signal(16)
        self.comb += [
            lab_keep.eq(Mux(lab["kind"] == K_LOOP, 0, lab["results"])),
            lab_drop.eq(slots - lab["slots"] - lab_keep),
            lab_ok.eq((depth < nctrl) & (slots >= lab["slots"] + lab_keep)),
            lab_target.eq(Mux(lab["kind"] == K_LOOP, lab["start"], lab["fixups"])),
        ]
        br_arg = Signal(24)
        self.comb += br_arg.eq(Cat(lab_target, lab_keep, lab_drop[:7]))

        # Code reads: the loader's while busy, otherwise `dbg_code_adr`.
        code_rd_adr = Signal(12)
        self.dbg_code_adr = Signal(12)
        self.comb += core.dbg_code_adr.eq(Mux(self.busy, code_rd_adr, self.dbg_code_adr))

        # Outputs to the core.
        self.comb += [
            core.code_len.eq(self.code_len), core.nfuncs.eq(self.nfuncs),
            core.table_len.eq(self.table_len), core.mem_max.eq(self.mem_max),
            core.page_log2.eq(self.page_log2),
        ]

        fsm = FSM(reset_state="IDLE")
        self.submodules += fsm
        self.comb += self.busy.eq(~fsm.ongoing("IDLE"))

        def fail(code):
            return [NextValue(self.error, code), NextState("DONE")]

        def read(name, cmd, then, err=E_MALFORMED):
            """State `name` reads; `then` (statements, val valid) runs on
            success; a reader error fails with `err`."""
            fsm.act(name, rd.go.eq(1), rd.cmd.eq(cmd), NextState(name + "_W"))
            fsm.act(name + "_W",
                If(rd.done,
                    If(rd.err, *fail(err)).Else(*then),
                ),
            )

        def emit(w, then):
            """Writes code[clen] = w, clen++; TooLarge past 4096 words."""
            return [
                If(clen >= CODE_WORDS, *fail(E_TOO_LARGE)).Else(
                    core.ld_code_adr.eq(clen[:12]), core.ld_code_dat.eq(w),
                    core.ld_code_we.eq(1),
                    NextValue(clen, clen + 1),
                    *then,
                ),
            ]

        def push(t, then):
            return [
                If(nvals >= MAX_OPERANDS, *fail(E_TOO_LARGE)).Else(
                    NextValue(tys, Mux(t, tys | (1 << nvals[:8]), tys & ~(1 << nvals[:8]))),
                    NextValue(nvals, nvals + 1),
                    NextValue(slots, slots + Mux(t, 2, 1)),
                    *then,
                ),
            ]

        # ---------------------------------------------------------------
        fsm.act("IDLE",
            If(self.start,
                NextValue(self.error, 0),
                NextValue(cnt, 0),
                NextState("CLEAR"),
            ),
        )
        # Reset the image: memory zeroed, table entries empty.
        fsm.act("CLEAR",
            self.mem_sel.eq(1),
            If(cnt < mem_bytes,
                self.mem_adr.eq(cnt[:aw]), self.mem_dat.eq(0), self.mem_we.eq(1),
            ),
            If(cnt < MAX_TABLE,
                core.ld_table_adr.eq(cnt[:6]), core.ld_table_dat.eq(NONE), core.ld_table_we.eq(1),
            ),
            If(cnt < MAX_GLOBALS,
                core.ld_glob_adr.eq(cnt[:4]), core.ld_glob_dat.eq(0), core.ld_glob_we.eq(1),
            ),
            NextValue(cnt, cnt + 1),
            If(cnt == max(mem_bytes, MAX_TABLE) - 1,
                NextValue(ntypes, 0), NextValue(self.nfuncs, 0), NextValue(nimports, 0),
                NextValue(nglobals, 0), NextValue(self.table_len, 0),
                NextValue(has_table, 0), NextValue(has_memory, 0),
                NextValue(memory_exported, 0), NextValue(bodies, 0), NextValue(last, 0),
                NextValue(self.mem_len, 0), NextValue(self.mem_max, 0),
                NextValue(self.page_log2, 16), NextValue(clen, 0),
                *[NextValue(e, 0) for e in self.exp],
                rd.set_pos.eq(1), rd.pos_in.eq(0),
                NextValue(rd.limit, self.img_len),
                NextValue(i, 0),
                NextState("HDR"),
            ),
        )
        magic = Array(Constant(c, 8) for c in b"\0asm\x01\0\0\0")
        read("HDR", R_BYTE, [
            If(rd.byte != magic[i[:3]], *fail(E_MALFORMED)
            ).Elif(i == 7, NextState("SEC")
            ).Else(NextValue(i, i + 1), NextState("HDR")),
        ])

        # --- sections ----------------------------------------------------
        fsm.act("SEC",
            If(rd.pos == self.img_len, NextState("END")).Else(NextState("SEC_ID")),
        )
        read("SEC_ID", R_BYTE, [NextValue(sid, rd.byte), NextState("SEC_LEN")])
        order = Signal(4)
        last_order = Signal(4)
        self.comb += [
            order.eq(Mux(sid == 12, 9, sid[:4])),
            last_order.eq(Mux(last == 12, 9, last)),
        ]
        read("SEC_LEN", R_U32, [
            NextValue(sec_end, rd.pos + v32[:15]),
            If(rd.pos + v32 > self.img_len, *fail(E_MALFORMED)
            ).Elif((sid != 0) & (sid <= 15) & (order <= last_order) & ~((sid == 12) & (last == 9)),
                *fail(E_MALFORMED)
            ).Else(
                NextValue(rd.limit, rd.pos + v32[:15]),
                If(sid != 0, NextValue(last, sid[:4])),
                NextState("SEC_DISPATCH"),
            ),
        ])
        fsm.act("SEC_DISPATCH",
            If((sid == 0) | (sid == 12),
                rd.set_pos.eq(1), rd.pos_in.eq(sec_end), NextState("SEC_END"),
            ).Elif(sid == 1, NextState("TY_N")
            ).Elif(sid == 2, NextState("IM_N")
            ).Elif(sid == 3, NextState("FN_N")
            ).Elif(sid == 4, NextState("TB_N")
            ).Elif(sid == 5, NextState("ME_N")
            ).Elif(sid == 6, NextState("GL_N")
            ).Elif(sid == 7, NextState("EX_N")
            ).Elif(sid == 8, *fail(E_UNSUPPORTED)
            ).Elif(sid == 9, NextState("EL_N")
            ).Elif(sid == 10, NextState("CO_N")
            ).Elif(sid == 11, NextState("DA_N")
            ).Else(*fail(E_MALFORMED)),
        )
        fsm.act("SEC_END",
            If(rd.pos != sec_end, *fail(E_MALFORMED)).Else(
                NextValue(rd.limit, self.img_len), NextState("SEC")),
        )
        fsm.act("END",
            If(bodies != self.nfuncs - nimports, *fail(E_MALFORMED)
            ).Elif(~memory_exported, *fail(E_MISSING_MEMORY)
            ).Else(
                NextValue(self.code_len, clen),
                NextValue(self.error, 0),
                NextState("DONE"),
            ),
        )
        fsm.act("DONE", self.done.eq(1), NextState("IDLE"))

        def vt(then):
            """A value type byte in rd.byte: I32 continues, else Unsupported."""
            return [If(rd.byte != I32, *fail(E_UNSUPPORTED)).Else(*then)]

        # Type section.
        read("TY_N", R_U32, [
            If(v32 > MAX_TYPES, *fail(E_TOO_LARGE)).Else(
                NextValue(n, v32), NextValue(i, 0), NextState("TY_LOOP")),
        ])
        fsm.act("TY_LOOP",
            If(i == n, NextValue(ntypes, n[:6]), NextState("SEC_END")).Else(NextState("TY_FORM")),
        )
        read("TY_FORM", R_BYTE, [If(rd.byte != 0x60, *fail(E_MALFORMED)).Else(NextState("TY_NP"))])
        read("TY_NP", R_U32, [
            If(v32 > MAX_PARAMS, *fail(E_TOO_LARGE)).Else(
                NextValue(tp, v32[:4]), NextValue(j, 0), NextState("TY_P")),
        ])
        fsm.act("TY_P", If(j == tp, NextState("TY_NR")).Else(NextState("TY_PT")))
        read("TY_PT", R_BYTE, vt([NextValue(j, j + 1), NextState("TY_P")]))
        read("TY_NR", R_U32, [
            If(v32 > 1, *fail(E_UNSUPPORTED)).Else(
                NextValue(tr, v32[0]), NextValue(j, 0), NextState("TY_R")),
        ])
        fsm.act("TY_R", If(j == tr, NextState("TY_STORE")).Else(NextState("TY_RT")))
        read("TY_RT", R_BYTE, vt([NextValue(j, j + 1), NextState("TY_R")]))
        fsm.act("TY_STORE",
            NextValue(types_p[i[:5]], tp), NextValue(types_r[i[:5]], tr),
            NextValue(i, i + 1), NextState("TY_LOOP"),
        )

        # A name: u32 length then bytes, UTF-8 checked, streamed to `nm`.
        def name(prefix, matcher, then):
            read(prefix, R_U32, [
                NextValue(name_len, v32), NextValue(j, 0),
                matcher.start.eq(1), utf.start.eq(1),
                NextState(prefix + "_B"),
            ])
            fsm.act(prefix + "_B",
                If(j == name_len,
                    matcher.length.eq(name_len), utf.length.eq(name_len),
                    If(~utf.valid, *fail(E_MALFORMED)).Else(*then),
                ).Else(NextState(prefix + "_R")),
            )
            read(prefix + "_R", R_BYTE, [
                matcher.stb.eq(1), matcher.idx.eq(j[:16]), matcher.byte.eq(rd.byte),
                utf.stb.eq(1), utf.byte.eq(rd.byte),
                NextValue(j, j + 1),
                NextState(prefix + "_B"),
            ])

        def ty_of(then):
            """v32 is a type index: Malformed past ntypes; sets tp/tr."""
            return [If(v32 >= ntypes, *fail(E_MALFORMED)).Else(
                NextValue(tp, types_p[v32[:5]]), NextValue(tr, types_r[v32[:5]]), *then)]

        def push_func(imp, entry, then):
            return [
                If(self.nfuncs >= MAX_FUNCS, *fail(E_TOO_LARGE)).Else(
                    core.ld_func_adr.eq(self.nfuncs[:7]),
                    core.ld_func_dat.eq(Cat(imp, Constant(NONE, 16), tp, Constant(0, 4),
                                            tr, Constant(0, 7), Constant(0, 16))),
                    core.ld_func_we.eq(1),
                    fsh_w.adr.eq(self.nfuncs[:7]),
                    fsh_w.dat_w.eq(Cat(tp, tr, imp)), fsh_w.we.eq(1),
                    NextValue(self.nfuncs, self.nfuncs + 1),
                    *then,
                ),
            ]

        # Import section.
        read("IM_N", R_U32, [NextValue(n, v32), NextValue(i, 0), NextState("IM_LOOP")])
        fsm.act("IM_LOOP", If(i == n, NextState("SEC_END")).Else(NextState("IM_MOD")))
        mod_hit = Signal()
        self.comb += mod_hit.eq(mods.hit[0])
        name("IM_MOD", mods, [NextValue(mod_ok, mod_hit), NextState("IM_NAME")])
        imp_hit_idx = Signal(6)
        imp_any = Signal()
        self.comb += [imp_any.eq(imps.hit != 0), imp_hit_idx.eq(0)]
        for k in reversed(range(len(IMPORT_NAMES))):
            self.comb += If(imps.hit[k], imp_hit_idx.eq(k))
        imp_found = Signal()
        name("IM_NAME", imps, [
            NextValue(imp_idx, imp_hit_idx), NextValue(imp_found, imp_any),
            NextState("IM_KIND"),
        ])
        read("IM_KIND", R_BYTE, [If(rd.byte != 0, *fail(E_UNSUPPORTED)).Else(NextState("IM_TY"))])
        read("IM_TY", R_U32, ty_of([NextState("IM_CHECK")]))
        fsm.act("IM_CHECK",
            If(~mod_ok | ~imp_found, *fail(E_UNKNOWN_IMPORT)
            ).Elif((tp != imp_params[imp_idx]) | (tr != imp_returns[imp_idx]),
                *fail(E_BAD_SIGNATURE)
            ).Else(*push_func(Cat(imp_idx, Constant(0, 2)), NONE, [
                NextValue(nimports, nimports + 1),
                NextValue(i, i + 1), NextState("IM_LOOP"),
            ])),
        )

        # Function section.
        read("FN_N", R_U32, [NextValue(n, v32), NextValue(i, 0), NextState("FN_LOOP")])
        fsm.act("FN_LOOP", If(i == n, NextState("SEC_END")).Else(NextState("FN_TY")))
        read("FN_TY", R_U32, ty_of([NextState("FN_PUSH")]))
        fsm.act("FN_PUSH", *push_func(Constant(LOCAL, 8), NONE, [
            NextValue(i, i + 1), NextState("FN_LOOP")]))

        # Table section.
        read("TB_N", R_U32, [
            If(v32 > 1, *fail(E_UNSUPPORTED)
            ).Elif(v32 == 0, NextState("SEC_END")
            ).Else(NextState("TB_TYPE")),
        ])
        read("TB_TYPE", R_BYTE, [If(rd.byte != 0x70, *fail(E_UNSUPPORTED)).Else(NextState("TB_FLAGS"))])
        read("TB_FLAGS", R_BYTE, [NextValue(flags, rd.byte), NextState("TB_MIN")])
        read("TB_MIN", R_U32, [NextValue(mn, v32),
                               If(flags[0], NextState("TB_MAX")).Else(NextState("TB_SET"))])
        read("TB_MAX", R_U32, [NextState("TB_SET")])
        fsm.act("TB_SET",
            If(mn > MAX_TABLE, *fail(E_TOO_LARGE)).Else(
                NextValue(self.table_len, mn[:7]), NextValue(has_table, 1),
                NextState("SEC_END")),
        )

        # Memory section.
        read("ME_N", R_U32, [
            If(v32 > 1, *fail(E_UNSUPPORTED)
            ).Elif(v32 == 0, NextState("SEC_END")
            ).Else(NextState("ME_FLAGS")),
        ])
        read("ME_FLAGS", R_U32, [
            If((v32 & ~0b1001) != 0, *fail(E_UNSUPPORTED)).Else(
                NextValue(flags, v32), NextState("ME_MIN")),
        ])
        has_max = Signal()
        read("ME_MIN", R_U32, [NextValue(mn, v32), NextValue(has_max, 0),
                               If(flags[0], NextState("ME_MAX")).Else(NextState("ME_LOG"))])
        read("ME_MAX", R_U32, [NextValue(mx, v32), NextValue(has_max, 1), NextState("ME_LOG")])
        fsm.act("ME_LOG",
            If(flags[3], NextState("ME_LOGR")).Else(NextValue(log2, 16), NextState("ME_SET")),
        )
        read("ME_LOGR", R_U32, [
            If((v32 != 0) & (v32 != 16), *fail(E_UNSUPPORTED)).Else(
                NextValue(log2, v32[:5]), NextState("ME_SET")),
        ])
        min_bytes = Signal(48)
        max_bytes = Signal(48)
        self.comb += [
            min_bytes.eq(Mux(log2 == 16, Cat(Constant(0, 16), mn), mn)),
            max_bytes.eq(Mux(log2 == 16, Cat(Constant(0, 16), mx), mx)),
        ]
        fsm.act("ME_SET",
            If(min_bytes > mem_bytes, *fail(E_MEMORY_TOO_LARGE)).Else(
                NextValue(self.page_log2, log2),
                NextValue(self.mem_len, min_bytes),
                NextValue(self.mem_max, Mux(has_max & (max_bytes < mem_bytes), max_bytes,
                                            mem_bytes)),
                NextValue(has_memory, 1),
                NextState("SEC_END")),
        )

        # A constant i32 expression: i32.const v end, into tmp.
        def const_i32(prefix, then):
            read(prefix, R_BYTE, [If(rd.byte != 0x41, *fail(E_UNSUPPORTED)).Else(
                NextState(prefix + "_V"))])
            read(prefix + "_V", R_S32, [NextValue(tmp, v32), NextState(prefix + "_E")])
            read(prefix + "_E", R_BYTE, [If(rd.byte != 0x0B, *fail(E_UNSUPPORTED)).Else(*then)])

        # Global section.
        read("GL_N", R_U32, [
            If(v32 > MAX_GLOBALS, *fail(E_TOO_LARGE)).Else(
                NextValue(n, v32), NextValue(i, 0), NextState("GL_LOOP")),
        ])
        fsm.act("GL_LOOP",
            If(i == n, NextValue(nglobals, n[:5]), NextState("SEC_END")).Else(NextState("GL_T")))
        read("GL_T", R_BYTE, vt([NextState("GL_MUT")]))
        read("GL_MUT", R_BYTE, [NextState("GL_C")])
        const_i32("GL_C", [NextState("GL_SET")])
        fsm.act("GL_SET",
            core.ld_glob_adr.eq(i[:4]), core.ld_glob_dat.eq(tmp), core.ld_glob_we.eq(1),
            NextValue(i, i + 1), NextState("GL_LOOP"),
        )

        # Export section.
        read("EX_N", R_U32, [NextValue(n, v32), NextValue(i, 0), NextState("EX_LOOP")])
        fsm.act("EX_LOOP", If(i == n, NextState("SEC_END")).Else(NextState("EX_NAME")))
        ex_hit = Signal(len(EXPORT_NAMES))
        name("EX_NAME", exps, [NextValue(ex_hit, exps.hit), NextState("EX_KIND")])
        read("EX_KIND", R_BYTE, [NextValue(kind, rd.byte), NextState("EX_IDX")])
        read("EX_IDX", R_U32, [NextValue(tmp, v32), NextState("EX_CHECK")])
        self.comb += If(fsm.ongoing("EX_CHECK"), fsh_r.adr.eq(tmp[:7]))
        shape_bad = Signal()
        shape_terms = []
        for k, (_, p, r) in enumerate(EXPORT_NAMES[:6]):
            shape_terms.append(ex_hit[k] & ((f_params != p) | (f_results != r)))
        self.comb += shape_bad.eq(_or(shape_terms))
        fsm.act("EX_CHECK",
            If(kind == 0,
                If(tmp >= self.nfuncs, *fail(E_MALFORMED)
                ).Elif(shape_bad, *fail(E_BAD_EXPORT)
                ).Else(
                    *[If(ex_hit[k], NextValue(self.exp[k], Cat(tmp[:8], Constant(1, 1))))
                      for k in range(6)],
                    NextValue(i, i + 1), NextState("EX_LOOP"),
                ),
            ).Elif(kind == 1, *fail(E_UNSUPPORTED)
            ).Elif(kind == 2,
                If(ex_hit[6] & has_memory, NextValue(memory_exported, 1)),
                NextValue(i, i + 1), NextState("EX_LOOP"),
            ).Elif(kind == 3,
                NextValue(i, i + 1), NextState("EX_LOOP"),
            ).Else(*fail(E_MALFORMED)),
        )

        # Element section.
        read("EL_N", R_U32, [NextValue(n, v32), NextValue(i, 0), NextState("EL_LOOP")])
        fsm.act("EL_LOOP", If(i == n, NextState("SEC_END")).Else(NextState("EL_FLAGS")))
        read("EL_FLAGS", R_U32, [If((v32 != 0) | ~has_table, *fail(E_UNSUPPORTED)).Else(
            NextState("EL_OFF"))])
        const_i32("EL_OFF", [NextState("EL_K")])
        read("EL_K", R_U32, [
            If(Cat(tmp, Constant(0, 1)) + v32 > self.table_len, *fail(E_MALFORMED)).Else(
                NextValue(cnt, v32), NextValue(j, 0), NextState("EL_F")),
        ])
        fsm.act("EL_F", If(j == cnt, NextValue(i, i + 1), NextState("EL_LOOP")).Else(
            NextState("EL_FR")))
        read("EL_FR", R_U32, [
            If(v32 >= self.nfuncs, *fail(E_MALFORMED)).Else(
                core.ld_table_adr.eq((tmp + j)[:6]), core.ld_table_dat.eq(v32[:16]),
                core.ld_table_we.eq(1),
                NextValue(j, j + 1), NextState("EL_F")),
        ])

        # Data section.
        read("DA_N", R_U32, [NextValue(n, v32), NextValue(i, 0), NextState("DA_LOOP")])
        fsm.act("DA_LOOP", If(i == n, NextState("SEC_END")).Else(NextState("DA_FLAGS")))
        read("DA_FLAGS", R_U32, [If(v32 != 0, *fail(E_UNSUPPORTED)).Else(NextState("DA_OFF"))])
        const_i32("DA_OFF", [NextState("DA_LEN")])
        read("DA_LEN", R_U32, [
            If(rd.pos + v32 > rd.limit, *fail(E_MALFORMED)
            ).Elif(Cat(tmp, Constant(0, 1)) + v32 > self.mem_len, *fail(E_MALFORMED)
            ).Else(NextValue(cnt, v32), NextValue(j, 0), NextState("DA_B")),
        ])
        fsm.act("DA_B", If(j == cnt, NextValue(i, i + 1), NextState("DA_LOOP")).Else(
            NextState("DA_R")))
        read("DA_R", R_BYTE, [
            self.mem_sel.eq(1),
            self.mem_adr.eq((tmp + j)[:aw]), self.mem_dat.eq(rd.byte), self.mem_we.eq(1),
            NextValue(j, j + 1), NextState("DA_B"),
        ])

        # --- code section --------------------------------------------------
        read("CO_N", R_U32, [
            If(v32 != self.nfuncs - nimports, *fail(E_MALFORMED)).Else(
                NextValue(n, v32), NextValue(i, 0), NextState("CO_LOOP")),
        ])
        fsm.act("CO_LOOP", If(i == n, NextState("SEC_END")).Else(NextState("CO_SIZE")))
        read("CO_SIZE", R_U32, [
            If(rd.pos + v32 > rd.limit, *fail(E_MALFORMED)).Else(
                NextValue(body_end, rd.pos + v32[:15]),
                NextValue(rd.limit, rd.pos + v32[:15]),
                NextValue(fi, nimports + i[:8]),
                NextValue(locals_, 0),
                NextState("CO_DECLS")),
        ])
        read("CO_DECLS", R_U32, [NextValue(cnt, v32), NextValue(j, 0), NextState("CO_D")])
        fsm.act("CO_D", If(j == cnt, NextState("CO_LCHK")).Else(NextState("CO_DC")))
        read("CO_DC", R_U32, [NextValue(tmp2, v32), NextState("CO_DT")])
        read("CO_DT", R_BYTE, vt([
            NextValue(locals_, Mux(locals_ + tmp2 > 511, 511, locals_ + tmp2)),
            NextValue(j, j + 1), NextState("CO_D"),
        ]))
        self.comb += If(fsm.ongoing("CO_LCHK") | fsm.ongoing("CO_ENTRY"), fsh_r.adr.eq(fi[:7]))
        fsm.act("CO_LCHK",
            If(f_params + locals_ > MAX_LOCALS, *fail(E_TOO_LARGE)).Else(NextState("CO_ENTRY")),
        )
        fsm.act("CO_ENTRY",
            core.ld_func_adr.eq(fi[:7]),
            core.ld_func_dat.eq(Cat(Constant(LOCAL, 8), clen[:12], Constant(0, 4), f_params,
                                    Constant(0, 4), f_results, Constant(0, 7),
                                    locals_, Constant(0, 6))),
            core.ld_func_we.eq(1),
            NextValue(nlocals, f_params + locals_),
            NextValue(nvals, 0), NextValue(slots, 0), NextValue(nctrl, 0),
            NextValue(tr, f_results),
            NextState("C_OPEN_FUNC"),
        )

        def open_ctrl(k, results, then):
            return [
                If(nctrl >= MAX_CTRL, *fail(E_TOO_LARGE)).Else(
                    c_w.adr.eq(nctrl[:6]),
                    c_w.dat_w.eq(pack(Constant(k, 2) if isinstance(k, int) else k, results,
                                      nvals, slots, Cat(clen, Constant(0, 3)),
                                      Constant(NONE, 16), Constant(NONE, 16),
                                      (nctrl != 0) & dead, (nctrl != 0) & dead)),
                    c_w.we.eq(1),
                    NextValue(nctrl, nctrl + 1),
                    *then,
                ),
            ]

        fsm.act("C_OPEN_FUNC", *open_ctrl(K_FUNC, tr, [NextState("C_OP")]))
        fsm.act("C_OP", If(nctrl == 0, NextState("C_BODY_END")).Else(NextState("C_OPC")))
        fsm.act("C_BODY_END",
            If(rd.pos != body_end, *fail(E_MALFORMED)).Else(
                NextValue(rd.limit, sec_end),
                NextValue(bodies, bodies + 1),
                NextValue(i, i + 1),
                NextState("CO_LOOP")),
        )
        read("C_OPC", R_BYTE, [NextValue(opc, rd.byte), NextState("C_DISPATCH")])

        # Helpers on the top entry.
        def write_top(**kw):
            f = dict(top)
            f.update(kw)
            return [c_w.adr.eq((nctrl - 1)[:6]),
                    c_w.dat_w.eq(pack(f["kind"], f["results"], f["vals"], f["slots"],
                                      f["start"], f["fixups"], f["unless"], f["dead"],
                                      f["outer_dead"])),
                    c_w.we.eq(1)]

        def set_dead(then):
            return [NextValue(nvals, top["vals"]), NextValue(slots, top["slots"]),
                    *write_top(dead=Constant(1, 1)), *then]

        # pop_i32 n times (resume to `nxt`); pops into nothing.
        pops_n = Signal(4)
        pops_next = Signal(6)
        POP_TARGETS = []

        def pops(k, nxt):
            if nxt not in POP_TARGETS:
                POP_TARGETS.append(nxt)
            return [NextValue(pops_n, k), NextValue(pops_next, POP_TARGETS.index(nxt)),
                    NextState("C_POPS")]

        floor = top["vals"]
        fsm.act("C_POPS",
            If(pops_n == 0,
                NextState("C_POPS_DONE"),
            ).Elif(nvals <= floor, *fail(E_MALFORMED)
            ).Elif(top_t, *fail(E_UNSUPPORTED)
            ).Else(
                NextValue(nvals, nvals - 1), NextValue(slots, slots - 1),
                NextValue(pops_n, pops_n - 1),
            ),
        )
        # A general pop (drop, store value): the type into popped_t.
        popped_t = Signal()

        def pop_any(then):
            return [If(nvals <= floor, *fail(E_MALFORMED)).Else(
                NextValue(popped_t, top_t),
                NextValue(nvals, nvals - 1), NextValue(slots, slots - Mux(top_t, 2, 1)),
                *then)]

        # Per-op continuation after POPS.
        imm = Signal(32)
        sub = Signal(8)
        un_code = Signal(8)
        bin_code = Signal(8)

        def emit_then_push(w, t=0):
            return emit(w, push(Constant(t, 1), [NextState("C_OP")]))

        d = opc
        is_unop = (d == 0x45) | ((d >= 0x67) & (d <= 0x69)) | (d == 0xC0) | (d == 0xC1)
        is_binop = ((d >= 0x46) & (d <= 0x4F)) | ((d >= 0x6A) & (d <= 0x78))
        is_load = (d == 0x28) | ((d >= 0x2C) & (d <= 0x2F))
        is_store = (d == 0x36) | (d == 0x37) | (d == 0x3A) | (d == 0x3B)
        self.comb += [
            un_code.eq(Mux(d == 0x45, 0, Mux(d == 0x67, 1, Mux(d == 0x68, 2, Mux(d == 0x69, 3,
                                                                Mux(d == 0xC0, 4, 5)))))),
            bin_code.eq(Mux(d <= 0x4F, d - 0x46, d - 0x6A + 10)),
        ]
        load_kind = Signal(3)
        store_kind = Signal(3)
        self.comb += [
            load_kind.eq(Mux(d == 0x28, 0, Mux(d == 0x2C, 1, Mux(d == 0x2D, 2, Mux(d == 0x2E, 3, 4))))),
            store_kind.eq(Mux(d == 0x36, 0, Mux(d == 0x37, 3, Mux(d == 0x3A, 1, 2)))),
        ]

        fsm.act("C_DISPATCH",
            If(d == 0x00,
                If(dead, NextState("C_OP")).Else(*emit(word(UNREACHABLE, 0), set_dead([NextState("C_OP")])))
            ).Elif(d == 0x01, NextState("C_OP")
            ).Elif((d == 0x02) | (d == 0x03) | (d == 0x04), NextState("C_BT")
            ).Elif(d == 0x05, NextState("C_ELSE")
            ).Elif(d == 0x0B, NextState("C_END")
            ).Elif((d == 0x0C) | (d == 0x0D) | (d == 0x0E), NextState("C_BR_IMM")
            ).Elif(d == 0x0F,
                If(dead, NextState("C_OP")).Else(
                    *emit(word(RET, Cat(first["results"], Constant(0, 23))),
                          set_dead([NextState("C_OP")])))
            ).Elif((d == 0x10) | (d == 0x11), NextState("C_CALL_IMM")
            ).Elif(d == 0x1A,
                If(dead, NextState("C_OP")).Else(*pop_any([NextState("C_DROP")]))
            ).Elif(d == 0x1B,
                If(dead, NextState("C_OP")).Else(*pops(3, "C_SELECT"))
            ).Elif(d == 0x1C, NextState("C_TSEL_N")
            ).Elif((d >= 0x20) & (d <= 0x24), NextState("C_VAR_IMM")
            ).Elif(is_load | is_store, NextState("C_MEMARG")
            ).Elif((d == 0x3F) | (d == 0x40), NextState("C_MEMIDX")
            ).Elif(d == 0x41, NextState("C_I32")
            ).Elif(d == 0x42, NextState("C_I64")
            ).Elif(is_unop,
                If(dead, NextState("C_OP")).Else(*pops(1, "C_UN"))
            ).Elif(is_binop,
                If(dead, NextState("C_OP")).Else(*pops(2, "C_BIN"))
            ).Elif(d == 0xFC, NextState("C_FC")
            ).Else(*fail(E_UNSUPPORTED)),
        )
        fsm.act("C_UN", *emit_then_push(word(UN, un_code)))
        fsm.act("C_BIN", *emit_then_push(word(BIN, bin_code)))
        fsm.act("C_SELECT", *emit_then_push(word(SELECT, 0)))
        fsm.act("C_DROP", *emit(word(DROP, Mux(popped_t, 2, 1)), [NextState("C_OP")]))

        # Typed select: vec of types, each must be i32.
        read("C_TSEL_N", R_U32, [NextValue(cnt, v32), NextValue(j, 0), NextState("C_TSEL")])
        fsm.act("C_TSEL", If(j == cnt,
            If(dead, NextState("C_OP")).Else(*pops(3, "C_SELECT"))).Else(NextState("C_TSEL_T")))
        read("C_TSEL_T", R_BYTE, [If(rd.byte != I32, *fail(E_UNSUPPORTED)).Else(
            NextValue(j, j + 1), NextState("C_TSEL"))])

        # block / loop / if.
        read("C_BT", R_BYTE, [
            If(rd.byte == 0x40, NextValue(tr, 0), NextState("C_BT_OPEN")
            ).Elif(rd.byte == I32, NextValue(tr, 1), NextState("C_BT_OPEN")
            ).Else(*fail(E_UNSUPPORTED)),
        ])
        if_dead = Signal()
        fsm.act("C_BT_OPEN",
            If(d == 0x04,
                If(dead, NextValue(if_dead, 1), NextState("C_IF_OPEN")).Else(
                    NextValue(if_dead, 0), *pops(1, "C_IF_OPEN")),
            ).Else(*open_ctrl(Mux(d == 0x02, K_BLOCK, K_LOOP), tr, [NextState("C_OP")])),
        )
        fsm.act("C_IF_OPEN", *open_ctrl(K_IF, tr, [
            If(if_dead, NextState("C_OP")).Else(NextState("C_IF_UNLESS"))]))
        fsm.act("C_IF_UNLESS", *emit(word(BRUNLESS, NONE), [
            *write_top(unless=Cat(clen, Constant(0, 3))),
            NextState("C_OP")]))

        # else.
        fsm.act("C_ELSE",
            If(top["kind"] != K_IF, *fail(E_MALFORMED)
            ).Elif(top["outer_dead"], NextState("C_OP")
            ).Elif(~top["dead"],
                NextValue(depth, 0), NextValue(resume, 1), NextState("C_LABEL"),
            ).Else(NextState("C_ELSE2")),
        )
        # resume 1: emit BR with the label of depth 0, then ELSE2.
        fsm.act("C_ELSE2",
            If(top["unless"] != NONE,
                code_rd_adr.eq(top["unless"][:12]), NextState("C_ELSE3"),
            ).Else(NextState("C_ELSE4")),
        )
        fsm.act("C_ELSE3",
            core.ld_code_adr.eq(top["unless"][:12]),
            core.ld_code_dat.eq(Cat(clen, Constant(0, 11), core.dbg_code_dat[24:32])),
            core.ld_code_we.eq(1),
            NextState("C_ELSE4"),
        )
        fsm.act("C_ELSE4",
            *write_top(unless=Constant(NONE, 16), dead=Constant(0, 1)),
            NextValue(nvals, top["vals"]), NextValue(slots, top["slots"]),
            NextState("C_OP"),
        )

        # label(depth): updates the label's fixup chain, sets br_word.
        br_word_arg = Signal(24)
        br_op = Signal(8)
        fsm.act("C_LABEL",
            If(~lab_ok, *fail(E_MALFORMED)
            ).Elif(lab_drop > 127, *fail(E_TOO_LARGE)
            ).Else(
                NextValue(br_word_arg, br_arg),
                If(lab["kind"] != K_LOOP,
                    c_w.adr.eq(lab_i[:6]),
                    c_w.dat_w.eq(pack(lab["kind"], lab["results"], lab["vals"], lab["slots"],
                                      lab["start"], Cat(clen, Constant(0, 3)), lab["unless"],
                                      lab["dead"], lab["outer_dead"])),
                    c_w.we.eq(1),
                ),
                NextState("C_LABEL_EMIT"),
            ),
        )
        fsm.act("C_LABEL_EMIT", *emit(Cat(br_word_arg, br_op), [
            Case(resume, {
                1: NextState("C_ELSE2"),
                2: NextState("C_BR_DEAD"),      # br
                3: NextState("C_OP"),           # br_if
                4: NextState("C_BRT_NEXT"),     # br_table entry
            })]))
        self.comb += br_op.eq(Mux(resume == 3, BRIF, BR))

        # end.
        e_kind = Signal(2)
        e_res = Signal()
        e_vals = Signal(9)
        e_slots = Signal(10)
        e_fix = Signal(16)
        e_unless = Signal(16)
        patch_at = Signal(16)
        patch_steps = Signal(14)
        fsm.act("C_END",
            NextValue(nctrl, nctrl - 1),
            If(top["outer_dead"], NextState("C_OP")).Else(
                NextValue(e_kind, top["kind"]), NextValue(e_res, top["results"]),
                NextValue(e_vals, top["vals"]), NextValue(e_slots, top["slots"]),
                NextValue(e_fix, top["fixups"]), NextValue(e_unless, top["unless"]),
                NextValue(patch_at, top["fixups"]), NextValue(patch_steps, 0),
                NextState("C_PATCH"),
            ),
        )
        fsm.act("C_PATCH",
            If(patch_at == NONE, NextState("C_END_UNLESS")
            ).Elif((patch_at >= CODE_WORDS) | (patch_steps > clen), *fail(E_MALFORMED)
            ).Else(code_rd_adr.eq(patch_at[:12]), NextState("C_PATCH2")),
        )
        fsm.act("C_PATCH2",
            core.ld_code_adr.eq(patch_at[:12]),
            core.ld_code_dat.eq(Cat(clen, Constant(0, 3), core.dbg_code_dat[16:32])),
            core.ld_code_we.eq(1),
            NextValue(patch_at, core.dbg_code_dat[0:16]),
            NextValue(patch_steps, patch_steps + 1),
            NextState("C_PATCH"),
        )
        fsm.act("C_END_UNLESS",
            If(e_unless != NONE,
                code_rd_adr.eq(e_unless[:12]), NextState("C_END_UNLESS2"),
            ).Else(NextState("C_END_RES")),
        )
        fsm.act("C_END_UNLESS2",
            core.ld_code_adr.eq(e_unless[:12]),
            core.ld_code_dat.eq(Cat(clen, Constant(0, 11), core.dbg_code_dat[24:32])),
            core.ld_code_we.eq(1),
            NextState("C_END_RES"),
        )
        fsm.act("C_END_RES",
            If(e_res,
                If(e_vals >= MAX_OPERANDS, *fail(E_TOO_LARGE)).Else(
                    NextValue(tys, tys & ~(1 << e_vals[:8])),
                    NextValue(nvals, e_vals + 1), NextValue(slots, e_slots + 1),
                    If(e_kind == K_FUNC, NextState("C_END_RET")).Else(NextState("C_OP")),
                ),
            ).Else(
                NextValue(nvals, e_vals), NextValue(slots, e_slots),
                If(e_kind == K_FUNC, NextState("C_END_RET")).Else(NextState("C_OP")),
            ),
        )
        fsm.act("C_END_RET", *emit(word(RET, Cat(e_res, Constant(0, 23))), [NextState("C_OP")]))

        # br / br_if / br_table.
        read("C_BR_IMM", R_U32, [
            NextValue(imm, v32),
            If(d == 0x0C,
                If(dead, NextState("C_OP")).Else(
                    NextValue(depth, v32), NextValue(resume, 2), NextState("C_LABEL")),
            ).Elif(d == 0x0D,
                If(dead, NextState("C_OP")).Else(*pops(1, "C_BRIF")),
            ).Else(
                If(v32 > 0xFFFFFF, *fail(E_TOO_LARGE)).Else(
                    NextValue(cnt, v32), NextValue(j, 0),
                    If(dead, NextState("C_BRT_D")).Else(*pops(1, "C_BRT_HDR"))),
            ),
        ])
        fsm.act("C_BRIF", NextValue(depth, imm), NextValue(resume, 3), NextState("C_LABEL"))
        fsm.act("C_BR_DEAD", *set_dead([NextState("C_OP")]))
        fsm.act("C_BRT_HDR", *emit(word(BRTABLE, cnt[:24]), [NextState("C_BRT_D")]))
        fsm.act("C_BRT_D",
            If(j == cnt + 1,
                If(dead, NextState("C_OP")).Else(*set_dead([NextState("C_OP")])),
            ).Else(NextState("C_BRT_R")),
        )
        read("C_BRT_R", R_U32, [
            NextValue(j, j + 1),
            If(dead, NextState("C_BRT_D")).Else(
                NextValue(depth, v32), NextValue(resume, 4), NextState("C_LABEL")),
        ])
        fsm.act("C_BRT_NEXT", NextState("C_BRT_D"))

        # call / call_indirect.
        read("C_CALL_IMM", R_U32, [
            NextValue(imm, v32),
            If(d == 0x10,
                If(v32 >= self.nfuncs, *fail(E_MALFORMED)).Else(NextState("C_CALL")),
            ).Else(NextState("C_CI_TBL")),
        ])
        self.comb += If(fsm.ongoing("C_CALL") | fsm.ongoing("C_CALL_EMIT"), fsh_r.adr.eq(imm[:7]))
        call_p = Signal(4)
        call_r = Signal()
        call_w = Signal(32)
        fsm.act("C_CALL",
            NextValue(call_p, f_params), NextValue(call_r, f_results),
            NextValue(call_w, Mux(f_import == LOCAL, word(CALL, imm[:24]),
                                  word(HOST, Cat(f_import, Constant(0, 16))))),
            If(dead, NextState("C_OP")).Else(
                NextValue(pops_n, f_params), NextValue(resume, 5), NextState("C_POPS")),
        )
        fsm.act("C_CALL_EMIT", *emit(call_w, [
            If(call_r, *push(Constant(0, 1), [NextState("C_OP")])).Else(NextState("C_OP"))]))
        read("C_CI_TBL", R_U32, [
            If((v32 != 0) | ~has_table, *fail(E_MALFORMED)
            ).Elif(imm >= ntypes, *fail(E_MALFORMED)
            ).Else(
                NextValue(call_p, types_p[imm[:5]]), NextValue(call_r, types_r[imm[:5]]),
                If(dead, NextState("C_OP")).Else(*pops(1, "C_CI_POPS"))),
        ])
        fsm.act("C_CI_POPS", NextValue(pops_n, call_p), NextValue(resume, 6),
                NextState("C_POPS"))
        fsm.act("C_CI_EMIT", NextValue(call_w, word(CALLI, Cat(call_r, Constant(0, 7), call_p,
                                                                 Constant(0, 12)))),
                NextState("C_CALL_EMIT"))

        # Locals and globals.
        read("C_VAR_IMM", R_U32, [
            NextValue(imm, v32),
            If((d <= 0x22) & (v32 >= nlocals), *fail(E_MALFORMED)
            ).Elif((d >= 0x23) & (v32 >= nglobals), *fail(E_MALFORMED)
            ).Elif(dead, NextState("C_OP")
            ).Elif((d == 0x20) | (d == 0x23), NextState("C_VAR_EMIT")
            ).Else(*pops(1, "C_VAR_EMIT")),
        ])
        var_op = Signal(8)
        self.comb += var_op.eq(Mux(d == 0x20, LGET, Mux(d == 0x21, LSET, Mux(d == 0x22, LTEE,
                                                                             Mux(d == 0x23, GGET, GSET)))))
        fsm.act("C_VAR_EMIT", *emit(Cat(imm[:24], var_op), [
            If((d == 0x21) | (d == 0x24), NextState("C_OP")).Else(
                *push(Constant(0, 1), [NextState("C_OP")]))]))

        # Loads and stores: memarg (align, offset).
        read("C_MEMARG", R_U32, [NextState("C_MEMOFF")])
        read("C_MEMOFF", R_U32, [
            NextValue(imm, v32),
            If(~has_memory, *fail(E_MALFORMED)
            ).Elif(dead, NextState("C_OP")
            ).Elif(is_load, *pops(1, "C_LOAD")
            ).Else(*pop_any([NextState("C_STORE_T")])),
        ])
        fsm.act("C_LOAD", *emit(word(LOAD, load_kind), [NextState("C_LOAD_OFF")]))
        fsm.act("C_LOAD_OFF", *emit(imm, push(Constant(0, 1), [NextState("C_OP")])))
        fsm.act("C_STORE_T",
            If(popped_t != (d == 0x37), *fail(E_UNSUPPORTED)).Else(*pops(1, "C_STORE")))
        fsm.act("C_STORE", *emit(word(STORE, store_kind), [NextState("C_STORE_OFF")]))
        fsm.act("C_STORE_OFF", *emit(imm, [NextState("C_OP")]))

        # memory.size / memory.grow.
        read("C_MEMIDX", R_U32, [
            If(v32 != 0, *fail(E_MALFORMED)
            ).Elif(~has_memory, *fail(E_MALFORMED)
            ).Elif(dead, NextState("C_OP")
            ).Elif(d == 0x40, *pops(1, "C_MGROW")
            ).Else(NextState("C_MSIZE")),
        ])
        fsm.act("C_MGROW", *emit_then_push(word(MGROW, 0)))
        fsm.act("C_MSIZE", *emit_then_push(word(MSIZE, 0)))

        # Constants.
        def konst(v, then_state):
            fits = (v[23:32] == 0) | (v[23:32] == 0x1FF)
            return [If(fits, *emit(word(CONST, v[:24]), [NextState(then_state)])).Else(
                NextValue(wd2, v), *emit(word(CONST32, 0), [NextState(then_state + "_W")]))]

        read("C_I32", R_S32, [NextValue(wd, v32),
                              If(dead, NextState("C_OP")).Else(NextState("C_I32_K"))])
        fsm.act("C_I32_K", *konst(wd, "C_I32_P"))
        fsm.act("C_I32_P_W", *emit(wd2, [NextState("C_I32_P")]))
        fsm.act("C_I32_P", *push(Constant(0, 1), [NextState("C_OP")]))
        hi = Signal(32)
        read("C_I64", R_S64, [NextValue(wd, val[:32]), NextValue(hi, val[32:]),
                              If(dead, NextState("C_OP")).Else(NextState("C_I64_LO"))])
        fsm.act("C_I64_LO", *konst(wd, "C_I64_HI"))
        fsm.act("C_I64_HI_W", *emit(wd2, [NextState("C_I64_HI")]))
        fsm.act("C_I64_HI", *konst(hi, "C_I64_P"))
        fsm.act("C_I64_P_W", *emit(wd2, [NextState("C_I64_P")]))
        fsm.act("C_I64_P", *push(Constant(1, 1), [NextState("C_OP")]))

        # 0xFC: memory.copy (10), memory.fill (11).
        read("C_FC", R_U32, [
            NextValue(sub, v32[:8]),
            If((v32 == 10) | (v32 == 11), NextState("C_FC_M1")).Else(*fail(E_UNSUPPORTED)),
        ])
        read("C_FC_M1", R_U32, [
            If(v32 != 0, *fail(E_MALFORMED)
            ).Elif(sub == 10, NextState("C_FC_M2")
            ).Else(NextState("C_FC_GO")),
        ])
        read("C_FC_M2", R_U32, [If(v32 != 0, *fail(E_MALFORMED)).Else(NextState("C_FC_GO"))])
        fsm.act("C_FC_GO",
            If(~has_memory, *fail(E_MALFORMED)
            ).Elif(dead, NextState("C_OP")
            ).Else(*pops(3, "C_FC_EMIT")),
        )
        fsm.act("C_FC_EMIT", *emit(Mux(sub == 10, MCOPY << 24, MFILL << 24), [NextState("C_OP")]))

        # Where POPS continues.
        fsm.act("C_POPS_DONE",
            If(resume == 5, NextValue(resume, 0), NextState("C_CALL_EMIT")
            ).Elif(resume == 6, NextValue(resume, 0), NextState("C_CI_EMIT")
            ).Else(Case(pops_next, {k: NextState(s) for k, s in enumerate(POP_TARGETS)})),
        )


def _or(xs):
    out = xs[0]
    for x in xs[1:]:
        out = out | x
    return out
