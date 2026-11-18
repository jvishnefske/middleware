"""The WebAssembly interpreter (CL7): executes the internal code that the
model compiles on the host (`wasm-hdl-ref/src/isa.rs`), one instruction
per several cycles, with the semantics of `wasm-hdl-ref/src/exec.rs`.

RAMs (synchronous read):

    code      CODE_WORDS x 32   the compiled code
    funcs     MAX_FUNCS x 56    import(8) entry(16) params(8) results(8) locals(16)
    table     MAX_TABLE x 16    table 0 (function indices, 0xFFFF empty)
    stack     STACK_SLOTS x 32  value stack: [params][locals][operands] per frame
    frames    CALL_DEPTH x 32   return pc(16) caller fp(16)
    memory    MEM x 8           linear memory, byte wide

and 16 globals in registers.

A call (`call_start` with the function index and up to six arguments)
pushes the arguments, enters the function with the top return address
0xFFFF and runs until that frame returns (`call_done`, `call_result`,
`call_has_result`) or a trap (`call_done` with `trap` != 0, codes in
`TRAPS`). Each instruction costs one unit of fuel and counts in `executed`.

A `HOST` instruction (or a `call_indirect` that lands on an import) pops
the import's arguments and raises `host_req` with `host_idx` (the ABI
import index) and `host_args`; the import unit may then use linear memory
through the `ext_mem_*` port and answers `host_ack` with `host_result`,
which is pushed when the import returns a value.

The installer (or a testbench) fills the RAMs through the `ld_*` ports while
the core is idle, sets `code_len`, `nfuncs`, `table_len`, `mem_max` and
`page_log2`, and loads the initial memory length with `ld_mem_len`
(`mem_len` itself follows `memory.grow`).
"""

from migen import (
    Array, Case, Cat, ClockSignal, Constant, If, Instance, Memory, Module, Mux, NextState,
    NextValue, Replicate, Signal,
)

from .gotofsm import GotoFSM
from .narrow import ge, sum_gt

# Build the linear memory from ECP5 DP16KD primitives instead of a Migen
# Memory (soc.py sets this; simulation keeps the Memory). Yosys merges the
# two exclusive write ports of the Memory into one and then copies the RAM
# for the second read port: 16 block RAMs instead of 8.
ECP5_PRIMITIVES = False

CODE_WORDS = 4096
MAX_FUNCS = 128
MAX_TABLE = 64
MAX_GLOBALS = 16
STACK_SLOTS = 1024
CALL_DEPTH = 32
NONE = 0xFFFF
LOCAL = 0xFF

# ISA opcodes (wasm-hdl-ref/src/isa.rs).
UNREACHABLE, CONST, CONST32, DROP, SELECT = 0x00, 0x01, 0x02, 0x03, 0x04
LGET, LSET, LTEE, GGET, GSET = 0x05, 0x06, 0x07, 0x08, 0x09
LOAD, STORE, MSIZE, MGROW, MCOPY, MFILL = 0x0A, 0x0B, 0x0C, 0x0D, 0x0E, 0x0F
UN, BIN, BR, BRIF, BRUNLESS, BRTABLE = 0x10, 0x11, 0x12, 0x13, 0x14, 0x15
CALL, CALLI, HOST, RET = 0x16, 0x17, 0x18, 0x19

TRAPS = {
    1: "Unreachable", 2: "OutOfFuel", 3: "StackOverflow", 4: "MemoryOutOfBounds",
    5: "DivideByZero", 6: "IntegerOverflow", 7: "TableOutOfBounds",
    8: "IndirectNull", 9: "IndirectType", 10: "BadCode",
}
T_UNREACHABLE, T_FUEL, T_STACK, T_MEM, T_DIV0, T_OVF, T_TABLE, T_NULL, T_TYPE, T_BAD = range(1, 11)


def import_shapes():
    """(params, returns) of every ABI import, in `wasm_abi::IMPORTS`
    order (src/wasm_abi.rs)."""
    return [
        (2, 0), (4, 1), (2, 1), (0, 1), (1, 1), (1, 1), (2, 1), (2, 1), (1, 1),  # ABI 1
        (0, 1), (0, 1), (2, 1), (3, 1), (1, 1), (7, 1), (4, 1), (3, 1), (4, 1),  # ABI 2
        (3, 1), (3, 1),
        (2, 1), (3, 1), (1, 1), (1, 1), (2, 1), (6, 1),                          # ABI 3
        (5, 1), (5, 1), (1, 1), (2, 1),                                          # ABI 4
        (2, 1), (1, 1),                                                          # ABI 5
        (4, 1),                                                                  # ABI 6
    ]


def sext(v, bits):
    return Cat(v[:bits], *([v[bits - 1]] * (32 - bits)))


class Divider(Module):
    """Unsigned restoring division, one quotient bit per cycle."""

    def __init__(self):
        self.start = Signal()
        self.n = Signal(32)
        self.d = Signal(32)
        self.busy = Signal()
        self.q = Signal(32)
        self.r = Signal(32)
        count = Signal(6)
        rem = Signal(33)
        trial = Signal(33)
        d = Signal(32)
        self.comb += trial.eq(Cat(self.q[31], rem[:32]) - d)
        self.sync += If(self.start,
            self.busy.eq(1),
            self.q.eq(self.n),
            d.eq(self.d),
            rem.eq(0),
            count.eq(32),
        ).Elif(self.busy,
            If(trial[32],
                rem.eq(Cat(self.q[31], rem[:32])),
                self.q.eq(Cat(0, self.q[:31])),
            ).Else(
                rem.eq(trial),
                self.q.eq(Cat(1, self.q[:31])),
            ),
            count.eq(count - 1),
            If(count == 1, self.busy.eq(0)),
        )
        self.comb += self.r.eq(rem[:32])


class _Port:
    """The signals of one Migen memory port (adr, dat_w, we, dat_r)."""

    def __init__(self, aw, dw):
        self.adr = Signal(aw)
        self.dat_w = Signal(dw)
        self.we = Signal()
        self.dat_r = Signal(dw)


def ecp5_tdp_bytes(module, depth):
    """A `depth` x 8 true dual-port RAM of DP16KD blocks in x1 mode (block i
    holds bit i of every byte, so no address decoding): synchronous read,
    write-first on both ports, as a Migen Memory port. Returns (a, b)."""
    assert depth == 16 * 1024, "x1 DP16KD blocks are 16384 deep"
    a, b = _Port(14, 8), _Port(14, 8)
    for i in range(8):
        pins = {}
        for n, p in (("A", a), ("B", b)):
            pins.update({f"i_CLK{n}": ClockSignal("sys"), f"i_CE{n}": 1, f"i_OCE{n}": 1,
                         f"i_RST{n}": 0, f"i_WE{n}": p.we, f"i_DI{n}0": p.dat_w[i],
                         f"o_DO{n}0": p.dat_r[i]})
            pins.update({f"i_CS{n}{k}": 0 for k in range(3)})
            pins.update({f"i_AD{n}{k}": p.adr[k] for k in range(14)})
        module.specials += Instance(
            "DP16KD",
            p_DATA_WIDTH_A=1, p_DATA_WIDTH_B=1, p_REGMODE_A="NOREG", p_REGMODE_B="NOREG",
            p_RESETMODE="SYNC", p_CSDECODE_A="0b000", p_CSDECODE_B="0b000",
            p_WRITEMODE_A="WRITETHROUGH", p_WRITEMODE_B="WRITETHROUGH", p_GSR="DISABLED",
            **pins)
    return a, b


class WasmCore(Module):
    def __init__(self, mem_bytes=16 * 1024):
        aw = (mem_bytes - 1).bit_length()
        # Control.
        self.call_start = Signal()
        self.call_func = Signal(8)
        self.call_nargs = Signal(3)
        self.call_args = Array(Signal(32, name=f"call_arg{i}") for i in range(6))
        self.fuel_per_call = Signal(32, reset=1_000_000)
        self.busy = Signal()
        self.call_done = Signal()
        self.call_result = Signal(32)
        self.call_has_result = Signal()
        self.trap = Signal(4)
        self.executed = Signal(48)
        # Host imports.
        self.host_req = Signal()
        self.host_idx = Signal(8)
        # host_args (the import's arguments, first parameter at 0) are the
        # operand registers; see below.
        self.host_ack = Signal()
        self.host_result = Signal(32)
        # Linear memory for the import unit (and tests).
        self.ext_mem_adr = Signal(aw)
        self.ext_mem_dat_w = Signal(8)
        self.ext_mem_we = Signal()
        self.ext_mem_dat_r = Signal(8)
        # Loading.
        self.ld_code_adr = Signal(12)
        self.ld_code_dat = Signal(32)
        self.ld_code_we = Signal()
        self.ld_func_adr = Signal(7)
        self.ld_func_dat = Signal(56)
        self.ld_func_we = Signal()
        self.ld_table_adr = Signal(6)
        self.ld_table_dat = Signal(16)
        self.ld_table_we = Signal()
        self.ld_glob_adr = Signal(4)
        self.ld_glob_dat = Signal(32)
        self.ld_glob_we = Signal()
        self.glob_rd_adr = Signal(4)
        self.glob_rd_dat = Signal(32)
        # Reads of code, funcs and table while idle (tests read the image
        # back). Synchronous.
        self.dbg_code_adr = Signal(12)
        self.dbg_code_dat = Signal(32)
        self.dbg_func_adr = Signal(7)
        self.dbg_func_dat = Signal(56)
        self.dbg_table_adr = Signal(6)
        self.dbg_table_dat = Signal(16)
        self.code_len = Signal(13)
        self.nfuncs = Signal(8)
        self.table_len = Signal(7)
        self.mem_len = Signal(aw + 2)        # current length (memory.grow)
        self.ld_mem_len = Signal(aw + 2)
        self.ld_mem_len_we = Signal()
        self.mem_max = Signal(aw + 2)
        self.page_log2 = Signal(5)

        # # #

        # RAMs.
        code = Memory(32, CODE_WORDS)
        funcs = Memory(56, MAX_FUNCS)
        table = Memory(16, MAX_TABLE)
        stack = Memory(32, STACK_SLOTS)
        frames = Memory(32, CALL_DEPTH)
        self.specials += code, funcs, table, stack, frames
        code_rd = code.get_port(has_re=False)
        code_wr = code.get_port(write_capable=True)
        func_rd = funcs.get_port(has_re=False)
        func_wr = funcs.get_port(write_capable=True)
        table_rd = table.get_port(has_re=False)
        table_wr = table.get_port(write_capable=True)
        st_rd = stack.get_port(has_re=False)
        st_wr = stack.get_port(write_capable=True)
        fr_rd = frames.get_port(has_re=False)
        fr_wr = frames.get_port(write_capable=True)
        self.specials += (code_rd, code_wr, func_rd, func_wr, table_rd, table_wr,
                          st_rd, st_wr, fr_rd, fr_wr)
        if ECP5_PRIMITIVES:
            mem_a, mem_b = ecp5_tdp_bytes(self, mem_bytes)
        else:
            mem = Memory(8, mem_bytes)
            mem_a = mem.get_port(write_capable=True)
            mem_b = mem.get_port(write_capable=True)
            self.specials += mem, mem_a, mem_b
        glob = Memory(32, MAX_GLOBALS)
        glob_dbg = Memory(32, MAX_GLOBALS)
        self.specials += glob, glob_dbg
        g_rd = glob.get_port(async_read=True)
        g_wr = glob.get_port(write_capable=True)
        gd_rd = glob_dbg.get_port(async_read=True)
        gd_wr = glob_dbg.get_port(write_capable=True)
        self.specials += g_rd, g_wr, gd_rd, gd_wr
        g_we = Signal()
        g_adr = Signal(4)
        g_dat = Signal(32)
        self.comb += [
            g_wr.adr.eq(Mux(self.ld_glob_we, self.ld_glob_adr, g_adr)),
            g_wr.dat_w.eq(Mux(self.ld_glob_we, self.ld_glob_dat, g_dat)),
            g_wr.we.eq(self.ld_glob_we | g_we),
            gd_wr.adr.eq(g_wr.adr), gd_wr.dat_w.eq(g_wr.dat_w), gd_wr.we.eq(g_wr.we),
            gd_rd.adr.eq(self.glob_rd_adr),
        ]

        self.comb += [
            code_wr.adr.eq(self.ld_code_adr), code_wr.dat_w.eq(self.ld_code_dat),
            code_wr.we.eq(self.ld_code_we),
            func_wr.adr.eq(self.ld_func_adr), func_wr.dat_w.eq(self.ld_func_dat),
            func_wr.we.eq(self.ld_func_we),
            table_wr.adr.eq(self.ld_table_adr), table_wr.dat_w.eq(self.ld_table_dat),
            table_wr.we.eq(self.ld_table_we),
            # The external port writes only while the core is idle or stalled
            # in a host call, so the two ports never write together. Saying so
            # (`& ~mem_a.we`) lets yosys drop the write-port priority, which a
            # DP16KD cannot express (a Memory then falls back to 131072
            # flip-flops), and keeps the primitives' same-address write
            # collision unreachable.
            mem_b.adr.eq(self.ext_mem_adr), mem_b.dat_w.eq(self.ext_mem_dat_w),
            mem_b.we.eq(self.ext_mem_we & ~mem_a.we), self.ext_mem_dat_r.eq(mem_b.dat_r),
            self.glob_rd_dat.eq(gd_rd.dat_r),
        ]

        shapes = import_shapes()
        imp_params = Array(p for p, _ in shapes)
        imp_returns = Array(r for _, r in shapes)

        # 64 KiB pages (page_log2 16) or 1-byte pages (0).
        big = Signal()
        self.comb += big.eq(self.page_log2 != 0)
        pages64 = self.mem_len[16:] if len(self.mem_len) > 16 else Constant(0, 1)

        # Machine state.
        pc = Signal(16)
        sp = Signal(11)
        fp = Signal(11)
        depth = Signal(6)
        fuel = Signal(32)
        op = Signal(8)
        a = Signal(24)
        imm = Signal(32)          # the extra word (literal or offset)
        v = Signal(32)            # value to push
        opnd = Array(Signal(32, name=f"host_arg{i}") for i in range(8))
        self.host_args = opnd
        npop = Signal(4)          # operands still to pop
        pidx = Signal(4)          # where the next popped one goes
        pidx_m1 = Signal(3)
        self.comb += pidx_m1.eq(pidx - 1)
        # Loop counter (locals, bytes) and copy/fill addresses: the range
        # checks use the full operands, so past them every count and
        # address is below mem_len (aw + 2 bits).
        cnt = Signal(aw + 2)
        # Effective address: the low bits of x + imm, and whether either
        # operand reaches past mem_len's width (then it is out of range).
        ea = Signal(aw + 3)
        ea_far = Signal()
        src_a = Signal(aw + 2)
        dst_a = Signal(aw + 2)
        backward = Signal()
        acc = Signal(32)
        size = Signal(4)
        fn = Signal(56)
        f_import = Signal(8)
        f_entry = Signal(16)
        f_params = Signal(8)
        f_results = Signal(8)
        f_locals = Signal(16)
        self.comb += [
            f_import.eq(fn[0:8]), f_entry.eq(fn[8:24]), f_params.eq(fn[24:32]),
            f_results.eq(fn[32:40]), f_locals.eq(fn[40:56]),
        ]
        br_target = Signal(16)
        br_keep = Signal()
        br_drop = Signal(7)
        ret_pc = Signal(16)
        is_host_call = Signal()
        div = Divider()
        self.submodules += div
        prod = Signal(64)
        # Shapes of the current HOST import.
        h_params = Signal(4)
        h_returns = Signal()
        self.comb += h_params.eq(imp_params[self.host_idx]), h_returns.eq(imp_returns[self.host_idx])

        x = opnd[0]
        y = opnd[1]
        z = opnd[2]

        self.comb += g_rd.adr.eq(a[:4])

        # The memory port of the core: one access per cycle.
        self.comb += mem_a.adr.eq(ea[:aw])

        def trap(code):
            return [NextValue(self.trap, code), NextState("DONE")]

        def unbranch(word_arg):
            return [
                NextValue(br_target, word_arg[0:16]),
                NextValue(br_keep, word_arg[16]),
                NextValue(br_drop, word_arg[17:24]),
            ]

        def pop_then(n, first_idx):
            """Pop n operands into opnd[first_idx-1] .. opnd[first_idx-n]
            (top first, so opnd[0] is the deepest), then EXEC."""
            return [NextValue(npop, n), NextValue(pidx, first_idx), NextState("POP")]

        fsm = GotoFSM(reset_state="IDLE")
        self.submodules += fsm
        self.comb += self.busy.eq(~fsm.ongoing("IDLE"))
        self.comb += [
            self.dbg_code_dat.eq(code_rd.dat_r),
            self.dbg_func_dat.eq(func_rd.dat_r),
            self.dbg_table_dat.eq(table_rd.dat_r),
        ]

        # The call's function and arguments are latched at call_start.
        c_func = Signal(8)
        c_nargs = Signal(3)
        c_args = Array(Signal(32, name=f"c_arg{i}") for i in range(6))
        self.sync += If(self.call_start & fsm.ongoing("IDLE"),
            c_func.eq(self.call_func), c_nargs.eq(self.call_nargs),
            *[c_args[i].eq(self.call_args[i]) for i in range(6)],
        )
        fsm.act("IDLE",
            code_rd.adr.eq(self.dbg_code_adr),
            func_rd.adr.eq(self.dbg_func_adr),
            table_rd.adr.eq(self.dbg_table_adr),
            If(self.call_start,
                NextValue(sp, 0),
                NextValue(fp, 0),
                NextValue(depth, 0),
                NextValue(fuel, self.fuel_per_call),
                NextValue(self.trap, 0),
                NextValue(cnt, 0),
                NextValue(is_host_call, 0),
                NextState("ARGS"),
            ),
        )
        # Push the call's arguments, then enter the function.
        fsm.act("ARGS",
            If(cnt < c_nargs,
                st_wr.adr.eq(sp), st_wr.dat_w.eq(c_args[cnt[:3]]), st_wr.we.eq(1),
                NextValue(sp, sp + 1),
                NextValue(cnt, cnt + 1),
            ).Else(
                func_rd.adr.eq(c_func),
                NextValue(ret_pc, NONE),
                NextState("ENTER_READ"),
            ),
        )
        # funcs[...] was addressed in the previous cycle.
        fsm.act("ENTER_READ",
            NextValue(fn, func_rd.dat_r),
            NextState("ENTER"),
        )
        fsm.act("ENTER",
            If(f_import != LOCAL,
                NextState("ENTER_HOST"),
            ).Elif(depth >= CALL_DEPTH,
                *trap(T_STACK),
            ).Elif(sp < f_params,
                *trap(T_BAD),
            ).Else(
                fr_wr.adr.eq(depth), fr_wr.dat_w.eq(Cat(ret_pc, fp[:11], 0)), fr_wr.we.eq(1),
                NextValue(depth, depth + 1),
                NextValue(fp, sp - f_params),
                NextValue(cnt, f_locals),
                NextState("LOCALS"),
            ),
        )
        # An indirect call that lands on an import runs it as a HOST op.
        fsm.act("ENTER_HOST",
            NextValue(self.host_idx, f_import),
            NextValue(a, f_import),
            NextState("HOST_POP"),
        )
        fsm.act("LOCALS",
            If(cnt == 0,
                NextValue(pc, f_entry),
                NextState("FETCH"),
            ).Elif(sp >= STACK_SLOTS,
                *trap(T_STACK),
            ).Else(
                st_wr.adr.eq(sp), st_wr.dat_w.eq(0), st_wr.we.eq(1),
                NextValue(sp, sp + 1),
                NextValue(cnt, cnt - 1),
            ),
        )
        fsm.act("FETCH",
            code_rd.adr.eq(pc),
            If(pc >= self.code_len,
                *trap(T_BAD),
            ).Elif(fuel == 0,
                *trap(T_FUEL),
            ).Else(
                NextValue(fuel, fuel - 1),
                NextValue(self.executed, self.executed + 1),
                NextState("DECODE"),
            ),
        )
        w = code_rd.dat_r
        fsm.act("DECODE",
            NextValue(op, w[24:32]),
            NextValue(a, w[0:24]),
            NextValue(pc, pc + 1),
            NextState("DISPATCH"),
        )
        # Ops with an extra word read it here.
        fsm.act("DISPATCH",
            code_rd.adr.eq(pc),
            If((op == CONST32) | (op == LOAD) | (op == STORE),
                NextValue(pc, pc + 1),
                NextState("IMM"),
            ).Else(
                NextState("PREP"),
            ),
        )
        fsm.act("IMM",
            NextValue(imm, code_rd.dat_r),
            NextState("PREP"),
        )
        # How many operands each op pops (into opnd[0..n-1], deepest first).
        fsm.act("PREP",
            If(op == UNREACHABLE, *trap(T_UNREACHABLE)
            ).Elif(op == CONST, NextValue(v, sext(a, 24)), NextState("PUSH")
            ).Elif(op == CONST32, NextValue(v, imm), NextState("PUSH")
            ).Elif(op == DROP,
                If(sum_gt(fp, a, sp), *trap(T_BAD)).Else(NextValue(sp, sp - a), NextState("FETCH"))
            ).Elif(op == SELECT, *pop_then(3, 3)
            ).Elif(op == LGET,
                st_rd.adr.eq(fp + a), NextState("LGET2")
            ).Elif((op == LSET) | (op == LTEE) | (op == GSET), *pop_then(1, 1)
            ).Elif(op == GGET, NextValue(v, g_rd.dat_r), NextState("PUSH")
            ).Elif(op == LOAD, *pop_then(1, 1)
            ).Elif(op == STORE, If(a == 3, *pop_then(3, 3)).Else(*pop_then(2, 2))
            ).Elif(op == MSIZE, NextValue(v, Mux(big, pages64, self.mem_len)), NextState("PUSH")
            ).Elif(op == MGROW, *pop_then(1, 1)
            ).Elif((op == MCOPY) | (op == MFILL), *pop_then(3, 3)
            ).Elif(op == UN, *pop_then(1, 1)
            ).Elif(op == BIN, *pop_then(2, 2)
            ).Elif(op == BR, *unbranch(a), NextState("BRANCH")
            ).Elif((op == BRIF) | (op == BRUNLESS) | (op == BRTABLE), *pop_then(1, 1)
            ).Elif(op == CALL,
                func_rd.adr.eq(a[:7]), NextValue(ret_pc, pc), NextState("ENTER_READ")
            ).Elif(op == CALLI, *pop_then(1, 1)
            ).Elif(op == HOST, NextValue(self.host_idx, a[:8]), NextState("HOST_POP")
            ).Elif(op == RET,
                If(a[0], *pop_then(1, 1)).Else(NextState("EXEC"))
            ).Else(*trap(T_BAD)),
        )
        # POP: read stack[sp-1] (registered next cycle in POP2).
        fsm.act("POP",
            If(npop == 0,
                NextState("EXEC"),
            ).Elif(sp <= fp,
                *trap(T_BAD),
            ).Else(
                st_rd.adr.eq(sp - 1),
                NextValue(sp, sp - 1),
                NextState("POP2"),
            ),
        )
        fsm.act("POP2",
            NextValue(opnd[pidx_m1], st_rd.dat_r),
            NextValue(pidx, pidx - 1),
            NextValue(npop, npop - 1),
            NextState("POP"),
        )
        fsm.act("LGET2",
            NextValue(v, st_rd.dat_r),
            NextState("PUSH"),
        )
        fsm.act("PUSH",
            If(sp >= STACK_SLOTS,
                *trap(T_STACK),
            ).Else(
                st_wr.adr.eq(sp), st_wr.dat_w.eq(v), st_wr.we.eq(1),
                NextValue(sp, sp + 1),
                NextState("FETCH"),
            ),
        )

        # --- EXEC: operands are in opnd[] --------------------------------
        un_r = Signal(32)
        clz = Signal(6)
        ctz = Signal(6)
        pop = Signal(6)
        self.comb += clz.eq(32), ctz.eq(32)
        for i in range(32):
            self.comb += If(x[i], clz.eq(31 - i))
        for i in reversed(range(32)):
            self.comb += If(x[i], ctz.eq(i))
        self.comb += pop.eq(sum(x[i] for i in range(32)))
        self.comb += [
            If(a[:3] == 0, un_r.eq(x == 0)
            ).Elif(a[:3] == 1, un_r.eq(clz)
            ).Elif(a[:3] == 2, un_r.eq(ctz)
            ).Elif(a[:3] == 3, un_r.eq(pop)
            ).Elif(a[:3] == 4, un_r.eq(sext(x, 8))
            ).Else(un_r.eq(sext(x, 16))),
        ]
        # One 64-bit right shifter serves every shift and rotate:
        #   shr_u {0, x} >> sh      shr_s {sign, x} >> sh   rotr {x, x} >> sh
        #   shl   {x, 0} >> 32-sh   rotl  {x, x} >> 32-sh
        sh = Signal(5)
        self.comb += sh.eq(y[:5])
        bop = Signal(5)
        self.comb += bop.eq(a[:5])
        sh_in = Signal(64)
        sh_amt = Signal(6)
        sh_out = Signal(64)
        self.comb += [
            If(bop == 20, sh_in.eq(Cat(Constant(0, 32), x))
            ).Elif(bop == 21, sh_in.eq(Cat(x, Replicate(x[31], 32)))
            ).Elif(bop == 22, sh_in.eq(Cat(x, Constant(0, 32)))
            ).Else(sh_in.eq(Cat(x, x))),
            sh_amt.eq(Mux((bop == 20) | (bop == 23), 32 - sh, sh)),
            sh_out.eq(sh_in >> sh_amt),
        ]
        # Comparisons from one subtraction.
        diff = Signal(33)
        self.comb += diff.eq(Cat(x, Constant(0, 1)) - Cat(y, Constant(0, 1)))
        eq = Signal()
        ltu = Signal()
        lts = Signal()
        self.comb += [
            eq.eq(x == y),
            ltu.eq(diff[32]),
            lts.eq(Mux(x[31] != y[31], x[31], diff[32])),
        ]
        # Each flag is its own 1-bit signal: in Verilog `~flag` assigned to
        # 32 bits would invert after widening.
        flags = [Signal(name=f"cmp{i}") for i in range(10)]
        self.comb += [
            flags[0].eq(eq), flags[1].eq(~eq),
            flags[2].eq(lts), flags[3].eq(ltu),
            flags[4].eq(~lts & ~eq), flags[5].eq(~ltu & ~eq),
            flags[6].eq(lts | eq), flags[7].eq(ltu | eq),
            flags[8].eq(~lts), flags[9].eq(~ltu),
        ]
        bin_r = Signal(32)
        cases = {
            **{i: bin_r.eq(flags[i]) for i in range(10)},
            10: bin_r.eq(x + y), 11: bin_r.eq(diff[:32]),
            17: bin_r.eq(x & y), 18: bin_r.eq(x | y), 19: bin_r.eq(x ^ y),
            20: bin_r.eq(sh_out[:32]), 21: bin_r.eq(sh_out[:32]), 22: bin_r.eq(sh_out[:32]),
            23: bin_r.eq(sh_out[:32]), 24: bin_r.eq(sh_out[:32]),
        }
        self.comb += Case(a[:5], {**cases, "default": bin_r.eq(0)})

        neg_x = Signal(32)
        neg_y = Signal(32)
        self.comb += neg_x.eq(-x), neg_y.eq(-y)
        signed_div = Signal()
        q_neg = Signal()
        r_neg = Signal()

        fsm.act("EXEC",
            If(op == SELECT, NextValue(v, Mux(z != 0, x, y)), NextState("PUSH")
            ).Elif(op == LSET,
                st_wr.adr.eq(fp + a), st_wr.dat_w.eq(x), st_wr.we.eq(1), NextState("FETCH")
            ).Elif(op == LTEE,
                st_wr.adr.eq(fp + a), st_wr.dat_w.eq(x), st_wr.we.eq(1),
                NextValue(v, x), NextState("PUSH")
            ).Elif(op == GSET, g_adr.eq(a[:4]), g_dat.eq(x), g_we.eq(1), NextState("FETCH")
            ).Elif(op == LOAD,
                NextValue(ea, x[:aw + 2] + imm[:aw + 2]),
                NextValue(ea_far, (x[aw + 2:] != 0) | (imm[aw + 2:] != 0)),
                NextValue(size, Mux(a == 0, 4, Mux((a == 1) | (a == 2), 1, 2))),
                NextState("LD_CHECK")
            ).Elif(op == STORE,
                NextValue(ea, x[:aw + 2] + imm[:aw + 2]),
                NextValue(ea_far, (x[aw + 2:] != 0) | (imm[aw + 2:] != 0)),
                NextValue(size, Mux(a == 0, 4, Mux(a == 1, 1, Mux(a == 2, 2, 8)))),
                NextState("ST_CHECK")
            ).Elif(op == MGROW, NextState("GROW")
            ).Elif(op == MCOPY,
                NextValue(dst_a, x), NextValue(src_a, y), NextValue(cnt, z),
                NextState("COPY_CHECK")
            ).Elif(op == MFILL,
                NextValue(dst_a, x), NextValue(acc, y), NextValue(cnt, z),
                NextState("FILL_CHECK")
            ).Elif(op == UN, NextValue(v, un_r), NextState("PUSH")
            ).Elif(op == BIN,
                If(a[:5] == 12, NextState("MUL")
                ).Elif((a[:5] >= 13) & (a[:5] <= 16), NextState("DIV_START")
                ).Else(NextValue(v, bin_r), NextState("PUSH"))
            ).Elif(op == BRIF,
                If(x != 0, *unbranch(a), NextState("BRANCH")).Else(NextState("FETCH"))
            ).Elif(op == BRUNLESS,
                If(x == 0, NextValue(pc, a[:16])), NextState("FETCH")
            ).Elif(op == BRTABLE,
                code_rd.adr.eq(pc + Mux(~ge(x, a), x, a)), NextState("BRTABLE2")
            ).Elif(op == CALLI,
                If(ge(x, self.table_len), *trap(T_TABLE)
                ).Else(table_rd.adr.eq(x[:6]), NextState("CALLI2"))
            ).Elif(op == RET, NextState("RET")
            ).Else(*trap(T_BAD)),
        )

        # Loads and stores, one byte per cycle (little-endian).
        fsm.act("LD_CHECK",
            If(ea_far | (ea + size > self.mem_len), *trap(T_MEM)).Else(
                NextValue(cnt, 0), NextValue(acc, 0), NextState("LD_BYTE")),
        )
        fsm.act("LD_BYTE",
            # mem_a.adr = ea (comb); the byte arrives next cycle.
            NextState("LD_LATCH"),
        )
        ea_inc = Signal(aw + 2)            # ea + 1 once ea is in range
        self.comb += ea_inc.eq(ea[:aw + 2] + 1)
        fsm.act("LD_LATCH",
            Case(cnt[:2], {i: NextValue(acc, Cat(*[mem_a.dat_r if j == i else acc[8 * j:8 * j + 8]
                                                   for j in range(4)]))
                           for i in range(4)}),
            NextValue(ea, ea_inc),
            NextValue(cnt, cnt + 1),
            If(cnt + 1 == size, NextState("LD_DONE")).Else(NextState("LD_BYTE")),
        )
        fsm.act("LD_DONE",
            If(a == 1, NextValue(v, sext(acc, 8))
            ).Elif(a == 3, NextValue(v, sext(acc, 16))
            ).Else(NextValue(v, acc)),
            NextState("PUSH"),
        )
        st_byte = Signal(8)
        self.comb += Case(cnt[:3], {i: st_byte.eq((y if i < 4 else z)[8 * (i % 4):8 * (i % 4) + 8])
                                    for i in range(8)})
        fsm.act("ST_CHECK",
            If(ea_far | (ea + size > self.mem_len), *trap(T_MEM)).Else(
                NextValue(cnt, 0), NextState("ST_BYTE")),
        )
        fsm.act("ST_BYTE",
            mem_a.dat_w.eq(st_byte), mem_a.we.eq(1),
            NextValue(ea, ea_inc),
            NextValue(cnt, cnt + 1),
            If(cnt + 1 == size, NextState("FETCH")),
        )

        # memory.grow: pages of 2**page_log2 bytes.
        # The bytes added: x pages of 64 KiB (x == 0 or out of range) or x
        # bytes; `grow_far` when they cannot fit in mem_max's width.
        grow_add = Signal(len(self.mem_len))
        grow_far = Signal()
        self.comb += [
            grow_add.eq(Mux(big, 0, x)),
            grow_far.eq(Mux(big, x != 0, x[len(self.mem_len):] != 0)),
        ]
        fsm.act("GROW",
            If(~grow_far & (Cat(self.mem_len, Constant(0, 1)) + grow_add <= self.mem_max),
                NextValue(v, Mux(big, pages64, self.mem_len)),
                NextValue(self.mem_len, self.mem_len + grow_add),
            ).Else(
                NextValue(v, 0xFFFF_FFFF),
            ),
            NextState("PUSH"),
        )

        # memory.copy (memmove) and memory.fill. The checks read the operands
        # (x dst, y src or value, z count), unchanged since EXEC.
        def in_mem(p, n):
            return ~sum_gt(p, n, self.mem_len)

        fsm.act("COPY_CHECK",
            If(~in_mem(y, z) | ~in_mem(x, z), *trap(T_MEM)
            ).Elif(z == 0, NextState("FETCH")
            ).Else(
                NextValue(backward, dst_a > src_a),
                If(dst_a > src_a,
                    NextValue(src_a, src_a + cnt - 1),
                    NextValue(dst_a, dst_a + cnt - 1),
                ),
                NextState("COPY_RD"),
            ),
        )
        fsm.act("COPY_RD",
            NextValue(ea, src_a),
            NextState("COPY_RD2"),
        )
        fsm.act("COPY_RD2",
            NextState("COPY_WR"),
        )
        fsm.act("COPY_WR",
            NextValue(acc, mem_a.dat_r),
            NextValue(ea, dst_a),
            NextState("COPY_WR2"),
        )
        fsm.act("COPY_WR2",
            mem_a.dat_w.eq(acc[:8]), mem_a.we.eq(1),
            NextValue(cnt, cnt - 1),
            # One adder each: +1, or -1 (all ones) going backward.
            NextValue(src_a, src_a + Cat(1, Replicate(backward, aw + 1))),
            NextValue(dst_a, dst_a + Cat(1, Replicate(backward, aw + 1))),
            If(cnt == 1, NextState("FETCH")).Else(NextState("COPY_RD")),
        )
        fsm.act("FILL_CHECK",
            If(~in_mem(x, z), *trap(T_MEM)
            ).Elif(z == 0, NextState("FETCH")
            ).Else(NextValue(ea, dst_a), NextState("FILL")),
        )
        fsm.act("FILL",
            mem_a.dat_w.eq(acc[:8]), mem_a.we.eq(1),
            NextValue(ea, ea_inc),
            NextValue(cnt, cnt - 1),
            If(cnt == 1, NextState("FETCH")),
        )

        # Multiply through a registered product (DSP blocks).
        fsm.act("MUL",
            NextValue(prod, x * y),
            NextState("MUL2"),
        )
        fsm.act("MUL2",
            NextValue(v, prod[:32]),
            NextState("PUSH"),
        )
        # Division: 13 div_s, 14 div_u, 15 rem_s, 16 rem_u.
        fsm.act("DIV_START",
            If(y == 0, *trap(T_DIV0)
            ).Elif((a[:5] == 13) & (x == 0x8000_0000) & (y == 0xFFFF_FFFF), *trap(T_OVF)
            ).Else(
                div.start.eq(1),
                If((a[:5] == 13) | (a[:5] == 15),
                    div.n.eq(Mux(x[31], neg_x, x)),
                    div.d.eq(Mux(y[31], neg_y, y)),
                    NextValue(signed_div, 1),
                    NextValue(q_neg, x[31] ^ y[31]),
                    NextValue(r_neg, x[31]),
                ).Else(
                    div.n.eq(x), div.d.eq(y),
                    NextValue(signed_div, 0),
                    NextValue(q_neg, 0), NextValue(r_neg, 0),
                ),
                NextState("DIV_WAIT"),
            ),
        )
        fsm.act("DIV_WAIT",
            If(~div.busy,
                If((a[:5] == 13) | (a[:5] == 14),
                    NextValue(v, Mux(q_neg, -div.q, div.q)),
                ).Else(
                    NextValue(v, Mux(r_neg, -div.r, div.r)),
                ),
                NextState("PUSH"),
            ),
        )

        # Branches: keep the top `keep` slots, drop `drop` more, jump.
        fsm.act("BRANCH",
            If(br_keep,
                st_rd.adr.eq(sp - 1),
                NextState("BRANCH_KEEP"),
            ).Else(
                If(sp < fp + br_drop, *trap(T_BAD)).Else(
                    NextValue(sp, sp - br_drop),
                    NextValue(pc, br_target),
                    NextState("FETCH"),
                ),
            ),
        )
        fsm.act("BRANCH_KEEP",
            If(sp < fp + br_drop + 1, *trap(T_BAD)).Else(
                st_wr.adr.eq(sp - 1 - br_drop), st_wr.dat_w.eq(st_rd.dat_r), st_wr.we.eq(1),
                NextValue(sp, sp - br_drop),
                NextValue(pc, br_target),
                NextState("FETCH"),
            ),
        )
        fsm.act("BRTABLE2",
            If(code_rd.dat_r[24:32] != BR, *trap(T_BAD)).Else(
                *unbranch(code_rd.dat_r[0:24]),
                NextState("BRANCH"),
            ),
        )

        # call_indirect: table entry, then the function and its type.
        fsm.act("CALLI2",
            If(table_rd.dat_r == NONE, *trap(T_NULL)).Else(
                func_rd.adr.eq(table_rd.dat_r[:7]),
                NextState("CALLI3"),
            ),
        )
        fsm.act("CALLI3",
            NextValue(fn, func_rd.dat_r),
            NextState("CALLI4"),
        )
        fsm.act("CALLI4",
            If((f_params != a[8:16]) | (f_results != a[0:8]), *trap(T_TYPE)).Else(
                NextValue(ret_pc, pc),
                NextState("ENTER"),
            ),
        )

        # HOST: pop the import's parameters into host_args, ask, push.
        fsm.act("HOST_POP",
            NextValue(npop, h_params),
            NextValue(pidx, h_params),
            NextState("HOST_POP1"),
        )
        fsm.act("HOST_POP1",
            If(npop == 0,
                NextState("HOST_REQ"),
            ).Elif(sp <= fp,
                *trap(T_BAD),
            ).Else(
                st_rd.adr.eq(sp - 1),
                NextValue(sp, sp - 1),
                NextState("HOST_POP2"),
            ),
        )
        fsm.act("HOST_POP2",
            NextValue(opnd[pidx_m1], st_rd.dat_r),
            NextValue(pidx, pidx - 1),
            NextValue(npop, npop - 1),
            NextState("HOST_POP1"),
        )
        fsm.act("HOST_REQ",
            self.host_req.eq(1),
            If(self.host_ack,
                NextValue(v, self.host_result),
                If(h_returns, NextState("PUSH")).Else(NextState("FETCH")),
            ),
        )

        # RET: keep opnd[0] if a, cut back to fp, pop the frame.
        fsm.act("RET",
            fr_rd.adr.eq(depth - 1),
            NextValue(sp, fp),
            NextValue(depth, depth - 1),
            NextState("RET2"),
        )
        fsm.act("RET2",
            If(a[0],
                st_wr.adr.eq(sp), st_wr.dat_w.eq(x), st_wr.we.eq(1),
                NextValue(sp, sp + 1),
            ),
            If(fr_rd.dat_r[0:16] == NONE,
                NextValue(self.call_result, x),
                NextValue(self.call_has_result, a[0]),
                NextState("DONE"),
            ).Else(
                NextValue(pc, fr_rd.dat_r[0:16]),
                NextValue(fp, fr_rd.dat_r[16:27]),
                NextState("FETCH"),
            ),
        )
        fsm.act("DONE",
            self.call_done.eq(1),
            NextState("IDLE"),
        )

        self.sync += If(self.ld_mem_len_we, self.mem_len.eq(self.ld_mem_len))
