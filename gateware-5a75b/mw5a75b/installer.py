"""The image installer (CL6): streams an image the host compiled
(`wasm-hdl-ref/src/image.rs`, `Core::emit_image`) into the WASM core's
RAMs, check for check as `Core::install` does.

The image sits in a 16 KiB image RAM (`img_*` write port: the upload path
or a testbench). `start` resets the core's image (memory zeroed, table 0
emptied, globals zeroed), then reads the image once, byte by byte, never
past `img_len`:

    header   magic "MWI\\x01", code words, functions, table entries,
             globals, page_log2, segments, mem_len, mem_max, 6 exports
    code     4 bytes per word          -> ld_code
    funcs    7 bytes per entry         -> ld_func (the 56-bit word as is)
    table    2 bytes per entry         -> ld_table
    globals  4 bytes each              -> ld_glob
    segments offset, length, bytes     -> linear memory (`mem_*`)

Each count is checked against its table (`TooLarge`), the memory lengths
against the core's (`MemoryTooLarge`), and the page size, the exports and
table entries (a function or none), each segment (inside the initial
memory), reading past the end and trailing bytes (`Malformed`). So a bad
image cannot write outside the core's RAMs or set the core up past its
limits; it is refused with the model's error kind. Code words and
function entries are taken as they are: the core traps what it cannot
run.

`done` pulses with `error` (`ERRORS`; 0 when installed). The outputs
(`code_len`, `nfuncs`, `table_len`, `mem_len`, `mem_max`, `page_log2`,
`exp_*`) configure the core and the runtime; `code_len` stays 0 unless the
whole image installed.
"""

from migen import Array, Cat, Constant, FSM, If, Memory, Module, Mux, NextState, NextValue, Signal

ERRORS = {0: "Ok", 1: "Malformed", 3: "TooLarge", 8: "MemoryTooLarge"}
E_MALFORMED, E_TOO_LARGE, E_MEMORY_TOO_LARGE = 1, 3, 8

MAGIC = int.from_bytes(b"MWI\x01", "little")
CODE_WORDS, MAX_FUNCS, MAX_GLOBALS, MAX_TABLE = 4096, 128, 16, 64
NONE = 0xFFFF
EXPORTS = ["init", "tick", "mw_alloc", "on_message", "on_irq", "on_request"]


class Installer(Module):
    def __init__(self, core, mem_bytes=16 * 1024, image_bytes=16 * 1024):
        aw = (mem_bytes - 1).bit_length()
        # Image RAM.
        self.img_adr = Signal(14)
        self.img_dat = Signal(8)
        self.img_we = Signal()
        self.img_len = Signal(15)
        # Image RAM read port for others while the installer is idle
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
        self.exp = Array(Signal(9, name=f"exp_{n}") for n in EXPORTS)
        self.exp_init, self.exp_tick, self.exp_alloc, self.exp_on_message = (
            self.exp[0], self.exp[1], self.exp[2], self.exp[3])
        self.exp_on_irq, self.exp_on_request = self.exp[4], self.exp[5]
        # Code reads (testbenches).
        self.dbg_code_adr = Signal(12)

        # # #

        img = Memory(8, image_bytes)
        self.specials += img
        iw = img.get_port(write_capable=True)
        ir = img.get_port(has_re=False)
        self.specials += iw, ir
        self.comb += iw.adr.eq(self.img_adr), iw.dat_w.eq(self.img_dat), iw.we.eq(self.img_we)

        pos = Signal(15)
        self.comb += [
            ir.adr.eq(Mux(self.img_rd_sel & ~self.busy, self.img_rd_adr, pos[:14])),
            self.img_rd_dat.eq(ir.dat_r),
            core.dbg_code_adr.eq(self.dbg_code_adr),
            core.code_len.eq(self.code_len), core.nfuncs.eq(self.nfuncs),
            core.table_len.eq(self.table_len), core.mem_max.eq(self.mem_max),
            core.page_log2.eq(self.page_log2),
        ]

        # Bytes enter the shift register at the top: after n bytes the
        # little-endian value is its top 8n bits.
        sr = Signal(56)
        have = Signal(3)
        v1, v2, v4 = sr[48:56], sr[40:56], sr[24:56]

        cnt = Signal(15)           # CLEAR, then the item counter
        code_n = Signal(13)
        nglobals = Signal(5)
        nsegs = Signal(16)
        k = Signal(3)              # export
        off = Signal(aw + 2)
        seg_n = Signal(aw + 2)
        j = Signal(aw + 2)

        fsm = FSM(reset_state="IDLE")
        self.submodules += fsm
        self.comb += self.busy.eq(~fsm.ongoing("IDLE"))

        def fail(code):
            return [NextValue(self.error, code), NextState("DONE")]

        def field(name, n, then):
            """State `name` reads n bytes (into `sr`), then runs `then`."""
            fsm.act(name,
                If(have == n, NextValue(have, 0), *then
                ).Elif(pos >= self.img_len, *fail(E_MALFORMED)
                ).Else(NextState(name + "_T")),
            )
            # `pos` was the address in `name`: its byte is out now.
            fsm.act(name + "_T",
                NextValue(sr, Cat(sr[8:], ir.dat_r)),
                NextValue(pos, pos + 1), NextValue(have, have + 1),
                NextState(name),
            )

        def items(name, count, nxt, n, then):
            """`count` items of n bytes each; `then(cont)` writes item
            `cnt` and continues with `cont`."""
            fsm.act(name,
                If(cnt == count, NextValue(cnt, 0), NextState(nxt)
                ).Else(NextState(name + "_R")),
            )
            field(name + "_R", n, then([NextValue(cnt, cnt + 1), NextState(name)]))

        fsm.act("IDLE",
            If(self.start,
                NextValue(self.error, 0), NextValue(cnt, 0), NextValue(self.code_len, 0),
                NextState("CLEAR"),
            ),
        )
        # Reset the image: memory zeroed, table entries empty, globals 0.
        fsm.act("CLEAR",
            self.mem_sel.eq(1),
            self.mem_adr.eq(cnt[:aw]), self.mem_dat.eq(0), self.mem_we.eq(1),
            If(cnt < MAX_TABLE,
                core.ld_table_adr.eq(cnt[:6]), core.ld_table_dat.eq(NONE), core.ld_table_we.eq(1),
            ),
            If(cnt < MAX_GLOBALS,
                core.ld_glob_adr.eq(cnt[:4]), core.ld_glob_dat.eq(0), core.ld_glob_we.eq(1),
            ),
            NextValue(cnt, cnt + 1),
            If(cnt == mem_bytes - 1,
                NextValue(self.nfuncs, 0), NextValue(self.table_len, 0),
                NextValue(self.mem_len, 0), NextValue(self.mem_max, 0),
                NextValue(self.page_log2, 16),
                *[NextValue(e, 0) for e in self.exp],
                NextValue(pos, 0), NextValue(have, 0), NextValue(k, 0),
                NextState("MAGIC"),
            ),
        )
        field("MAGIC", 4, [If(v4 != MAGIC, *fail(E_MALFORMED)).Else(NextState("CODE_N"))])
        field("CODE_N", 2, [
            If(v2 > CODE_WORDS, *fail(E_TOO_LARGE)).Else(
                NextValue(code_n, v2), NextState("NFUNCS")),
        ])
        field("NFUNCS", 1, [
            If(v1 > MAX_FUNCS, *fail(E_TOO_LARGE)).Else(
                NextValue(self.nfuncs, v1), NextState("NTABLE")),
        ])
        field("NTABLE", 1, [
            If(v1 > MAX_TABLE, *fail(E_TOO_LARGE)).Else(
                NextValue(self.table_len, v1), NextState("NGLOBALS")),
        ])
        field("NGLOBALS", 1, [
            If(v1 > MAX_GLOBALS, *fail(E_TOO_LARGE)).Else(
                NextValue(nglobals, v1), NextState("PAGE")),
        ])
        field("PAGE", 1, [
            If((v1 != 0) & (v1 != 16), *fail(E_MALFORMED)).Else(
                NextValue(self.page_log2, v1), NextState("NSEGS")),
        ])
        field("NSEGS", 2, [NextValue(nsegs, v2), NextState("MEMLEN")])
        field("MEMLEN", 2, [
            If(v2 > mem_bytes, *fail(E_MEMORY_TOO_LARGE)).Else(
                NextValue(self.mem_len, v2), NextState("MEMMAX")),
        ])
        field("MEMMAX", 2, [
            If(v2 > mem_bytes, *fail(E_MEMORY_TOO_LARGE)
            ).Elif(v2 < self.mem_len, *fail(E_MALFORMED)
            ).Else(NextValue(self.mem_max, v2), NextState("EXPORT")),
        ])
        # A function index or none: an export or a table entry.
        func_ok = Signal()
        self.comb += func_ok.eq((v2 == NONE) | (v2 < self.nfuncs))
        field("EXPORT", 2, [
            If(~func_ok, *fail(E_MALFORMED)).Else(
                *[If(k == e, NextValue(self.exp[e], Mux(v2 == NONE, 0, Cat(v2[:8], Constant(1, 1)))))
                  for e in range(len(EXPORTS))],
                NextValue(k, k + 1),
                If(k == len(EXPORTS) - 1, NextValue(cnt, 0), NextState("CODE")
                ).Else(NextState("EXPORT")),
            ),
        ])
        items("CODE", code_n, "FUNC", 4, lambda cont: [
            core.ld_code_adr.eq(cnt[:12]), core.ld_code_dat.eq(v4), core.ld_code_we.eq(1),
            *cont,
        ])
        items("FUNC", self.nfuncs, "TABLE", 7, lambda cont: [
            core.ld_func_adr.eq(cnt[:7]), core.ld_func_dat.eq(sr), core.ld_func_we.eq(1),
            *cont,
        ])
        items("TABLE", self.table_len, "GLOBAL", 2, lambda cont: [
            If(~func_ok, *fail(E_MALFORMED)).Else(
                core.ld_table_adr.eq(cnt[:6]), core.ld_table_dat.eq(v2),
                core.ld_table_we.eq(1), *cont),
        ])
        items("GLOBAL", nglobals, "SEG", 4, lambda cont: [
            core.ld_glob_adr.eq(cnt[:4]), core.ld_glob_dat.eq(v4), core.ld_glob_we.eq(1),
            *cont,
        ])
        # Segments: cnt counts them, j their bytes.
        fsm.act("SEG",
            If(cnt == nsegs, NextState("END")).Else(NextState("SEG_OFF")),
        )
        field("SEG_OFF", 2, [NextValue(off, v2), NextState("SEG_LEN")])
        field("SEG_LEN", 2, [
            If(Cat(off, Constant(0, 1)) + v2 > self.mem_len, *fail(E_MALFORMED)).Else(
                NextValue(seg_n, v2), NextValue(j, 0), NextState("SEG_B")),
        ])
        fsm.act("SEG_B",
            If(j == seg_n, NextValue(cnt, cnt + 1), NextState("SEG")).Else(NextState("SEG_BYTE")),
        )
        field("SEG_BYTE", 1, [
            self.mem_sel.eq(1),
            self.mem_adr.eq((off + j)[:aw]), self.mem_dat.eq(v1), self.mem_we.eq(1),
            NextValue(j, j + 1), NextState("SEG_B"),
        ])
        fsm.act("END",
            If(pos != self.img_len, *fail(E_MALFORMED)).Else(
                NextValue(self.code_len, code_n), NextState("DONE")),
        )
        fsm.act("DONE", self.done.eq(1), NextState("IDLE"))
