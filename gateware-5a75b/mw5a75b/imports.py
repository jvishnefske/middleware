"""The import unit and the payload runtime (CL7): the ABI side of the WASM
core, following `wasm-hdl-ref/src/abi.rs` check for check.

`ImportUnit` answers the core's `host_req`. It reads and writes linear
memory through the core's `ext_mem_*` port and implements:

    log           a `sys/log` publication of the line (at most 256 bytes,
                  cut back to whole UTF-8 characters), on every link with
                  a peer
    publish       a publication on every link with a peer
    subscribe     an 8-slot exact-key table (`sub_*` matchers follow
                  incoming keys against it)
    now_ms        the `now_ms` input
    set_tick      the tick schedule (`tick_period`, `tick_due`)
    gpio_*        pin 0 the user LED (output), pin 1 the button (input);
                  other pins Unsupported
    node_id       the 8-byte node id
    route_bridge  validated, then installed in the bridge table
    route_stop    bridge handles; DMA route handles 0..3 are Busy
    route_stat    the bridge rule's counters; 0..3 Busy

and answers `Unsupported` for every other import. Publications leave as
frame bodies on `pub_*` (`[3][0][key_len][key][data]`, with `pub_link`
and `pub_len`), read from guest memory once per link.

`Runtime` sequences the export calls around the core as `wasm-host` does:
`init` (`ev_init`), `tick` when due (`ev_tick`), and deliveries
(`ev_deliver`: `mw_alloc(len)`, the bytes copied from the caller's buffer
port into guest memory, `on_message(slot, ptr, len)`). A trap poisons the
payload until the next load (`ev_reset`).
"""

from migen import (
    Array, Cat, Constant, FSM, If, Memory, Module, Mux, NextState, NextValue, Signal,
)

from .wasm import WasmCore

# ABI import indices (wasm_abi::IMPORTS order).
I_LOG, I_PUBLISH, I_SUBSCRIBE, I_NOW_MS, I_SET_TICK = 0, 1, 2, 3, 4
I_GPIO_OUTPUT, I_GPIO_INPUT, I_GPIO_WRITE, I_GPIO_READ = 5, 6, 7, 8
I_ROUTE_STOP, I_ROUTE_STAT, I_SERVE, I_NODE_ID, I_ROUTE_BRIDGE = 28, 29, 30, 31, 32

E_INVALID_ARG, E_OOB, E_NO_RES, E_UNSUPPORTED, E_INVALID_KEY, E_BUSY = -1, -2, -3, -4, -6, -7
MAX_MESSAGE, MAX_LOG, MIN_TICK = 512, 256, 10
N_SUBS = 8
LOG_KEY = b"sys/log"

# Event results.
R_OK, R_NOTDUE, R_DISABLED, R_NOTICK, R_FALSE, R_POISONED = 0, 1, 2, 3, 4, 5
R_TRAP = 8   # + trap code


def _c(v):
    return Constant(v & 0xFFFF_FFFF, 32)


class KeyCheck(Module):
    """Streams a key: valid (1..=128 bytes of [A-Za-z0-9_.-] segments
    split by '/') and whether its first segment is `sys`."""

    def __init__(self):
        self.start = Signal()
        self.stb = Signal()
        self.byte = Signal(8)
        self.valid = Signal()     # after the last byte: key_ok
        self.sys = Signal()
        self.count = Signal(11)
        seg = Signal(8)
        bad = Signal()
        first = Signal(4)         # bytes of the first segment matched to "sys"
        in_first = Signal()
        b = self.byte
        ok_char = (((b >= ord("0")) & (b <= ord("9"))) | ((b >= ord("A")) & (b <= ord("Z")))
                   | ((b >= ord("a")) & (b <= ord("z"))) | (b == ord("_")) | (b == ord("-"))
                   | (b == ord(".")))
        sys_at = Array(Constant(c, 8) for c in b"sys")
        self.sync += If(self.start,
            seg.eq(0), bad.eq(0), self.count.eq(0), first.eq(0), in_first.eq(1),
        ).Elif(self.stb,
            self.count.eq(self.count + 1),
            If(b == ord("/"),
                If(seg == 0, bad.eq(1)),
                seg.eq(0),
                in_first.eq(0),
            ).Elif(ok_char,
                seg.eq(seg + 1),
            ).Else(bad.eq(1)),
            If(in_first & (b != ord("/")),
                If((self.count < 3) & (b == sys_at[self.count[:2]]) & (first == self.count),
                    first.eq(first + 1),
                ).Else(first.eq(15)),
            ),
        )
        self.comb += [
            self.valid.eq(~bad & (seg != 0) & (self.count != 0) & (self.count <= 128)),
            self.sys.eq(first == 3),
        ]


class Utf8Cut(Module):
    """Streams bytes: `upto` = the length of the longest prefix that is
    whole UTF-8 characters (`str::from_utf8`'s `valid_up_to`)."""

    def __init__(self):
        self.start = Signal()
        self.stb = Signal()
        self.byte = Signal(8)
        self.upto = Signal(9)
        self.ok = Signal()        # every byte so far is whole characters
        need = Signal(2)
        lo = Signal(8)
        hi = Signal(8)
        stop = Signal()
        i = Signal(9)
        b = self.byte
        self.comb += self.ok.eq(~stop & (need == 0))
        self.sync += If(self.start,
            need.eq(0), stop.eq(0), i.eq(0), self.upto.eq(0),
        ).Elif(self.stb & ~stop,
            i.eq(i + 1),
            If(need == 0,
                If(b < 0x80, self.upto.eq(i + 1)
                ).Elif((b >= 0xC2) & (b <= 0xDF), need.eq(1), lo.eq(0x80), hi.eq(0xBF)
                ).Elif(b == 0xE0, need.eq(2), lo.eq(0xA0), hi.eq(0xBF)
                ).Elif(b == 0xED, need.eq(2), lo.eq(0x80), hi.eq(0x9F)
                ).Elif((b >= 0xE1) & (b <= 0xEF), need.eq(2), lo.eq(0x80), hi.eq(0xBF)
                ).Elif(b == 0xF0, need.eq(3), lo.eq(0x90), hi.eq(0xBF)
                ).Elif((b >= 0xF1) & (b <= 0xF3), need.eq(3), lo.eq(0x80), hi.eq(0xBF)
                ).Elif(b == 0xF4, need.eq(3), lo.eq(0x80), hi.eq(0x8F)
                ).Else(stop.eq(1)),
            ).Else(
                If((b >= lo) & (b <= hi),
                    need.eq(need - 1), lo.eq(0x80), hi.eq(0xBF),
                    If(need == 1, self.upto.eq(i + 1)),
                ).Else(stop.eq(1)),
            ),
        )


class SubMatcher(Module):
    """Follows a streamed key against the subscription lanes (exact)."""

    def __init__(self, rd, lens, count):
        self.start = Signal(name="sub_start")
        self.stb = Signal(name="sub_stb")
        self.idx = Signal(8, name="sub_idx")
        self.byte = Signal(8, name="sub_byte")
        self.key_len = Signal(8, name="sub_key_len")
        self.hit = Signal(N_SUBS, name="sub_hit")
        alive = Signal(N_SUBS)
        self.comb += rd.adr.eq(self.idx[:7])
        stb1, idx1, byte1 = Signal(), Signal(8), Signal(8)
        self.sync += stb1.eq(self.stb), idx1.eq(self.idx), byte1.eq(self.byte)
        for s in range(N_SUBS):
            self.sync += If(self.start, alive[s].eq(1)).Elif(
                stb1 & (idx1 < lens[s]) & (byte1 != rd.dat_r[8 * s:8 * s + 8]),
                alive[s].eq(0))
            self.comb += self.hit[s].eq(alive[s] & (s < count) & (self.key_len == lens[s]))
        self.slot = Signal(3, name="sub_slot")
        for s in reversed(range(N_SUBS)):
            self.comb += If(self.hit[s], self.slot.eq(s))


class ImportUnit(Module):
    def __init__(self, core, bridge=None, n_match=2):
        aw = len(core.ext_mem_adr)
        self.now_ms = Signal(32)
        self.node_id = Signal(64)
        self.has_tick = Signal()
        self.has_messages = Signal()
        self.reset_tables = Signal()      # on load
        self.tick_on = Signal()
        self.tick_period = Signal(32)
        self.tick_due = Signal(32)
        self.tick_set = Signal()          # schedule written by set_tick
        self.led = Signal()
        self.led_enabled = Signal()
        self.button = Signal()
        self.peer_ok = Signal(2)          # links with a peer
        # Publications out.
        self.pub_valid = Signal()
        self.pub_data = Signal(8)
        self.pub_last = Signal()
        self.pub_ready = Signal()
        self.pub_link = Signal()
        self.pub_len = Signal(16)
        self.pub_req = Signal()
        # Bridge commands out (when no BridgeTable is given).
        self.br_add_start = Signal()
        self.br_add_src = Signal(3)
        self.br_add_dst = Signal(3)
        self.br_add_prefix = Signal()
        self.br_add_len = Signal(8)
        self.br_add_stb = Signal()
        self.br_add_byte = Signal(8)
        self.br_add_end = Signal()
        self.br_rm_stb = Signal()
        self.br_stat_stb = Signal()
        self.br_stat_dropped = Signal()
        self.br_handle = Signal(8)
        self.br_done = Signal()
        self.br_result = Signal((32, True))
        if bridge is not None:
            self.comb += [
                bridge.add_start.eq(self.br_add_start), bridge.add_src.eq(self.br_add_src),
                bridge.add_dst.eq(self.br_add_dst), bridge.add_prefix.eq(self.br_add_prefix),
                bridge.add_len.eq(self.br_add_len), bridge.add_stb.eq(self.br_add_stb),
                bridge.add_byte.eq(self.br_add_byte), bridge.add_end.eq(self.br_add_end),
                bridge.rm_stb.eq(self.br_rm_stb), bridge.stat_stb.eq(self.br_stat_stb),
                bridge.stat_dropped.eq(self.br_stat_dropped), bridge.handle.eq(self.br_handle),
                self.br_done.eq(bridge.cmd_done), self.br_result.eq(bridge.cmd_result),
            ]

        # # #

        # Subscription table: 8 lanes of 128 key bytes.
        subkeys = Memory(8 * N_SUBS, 128)
        self.specials += subkeys
        sub_wr = subkeys.get_port(write_capable=True, we_granularity=8)
        self.specials += sub_wr
        sub_lens = Array(Signal(8, name=f"sub_len{s}") for s in range(N_SUBS))
        sub_count = Signal(4, name="sub_count")
        self.sub_count = sub_count

        def matcher():
            rd = subkeys.get_port(has_re=False)
            self.specials += rd
            m = SubMatcher(rd, sub_lens, sub_count)
            self.submodules += m
            return m
        self.sub_match = [matcher() for _ in range(n_match)]
        own = matcher()

        kc = KeyCheck()
        utf = Utf8Cut()
        self.submodules += kc, utf

        args = core.host_args
        a0, a1, a2, a3 = args[0], args[1], args[2], args[3]
        idx = Signal(8)
        result = Signal(32)
        mem_len = core.mem_len

        # Guest ranges: p + n <= mem_len with u32 p and n.
        def in_range(p, n):
            return (Cat(p, Constant(0, 1)) + Cat(n, Constant(0, 1))) <= mem_len

        # Streaming reads of guest memory: `rd_at` .. `rd_at + rd_n`.
        rd_at = Signal(33)
        rd_i = Signal(11)
        rd_n = Signal(11)
        # Memory requests; `Runtime` muxes them onto the core's port.
        self.mem_adr = Signal(aw)
        self.mem_dat_w = Signal(8)
        self.mem_we = Signal()
        self.comb += self.mem_adr.eq(rd_at[:aw])
        key_at = Signal(32)
        key_n = Signal(11)
        dat_at = Signal(32)
        dat_n = Signal(11)
        body_n = Signal(11)       # route_bridge: key without "/**"
        prefix = Signal()
        tail = Signal(24)         # the last three key bytes
        link = Signal()
        is_log = Signal()
        log_rom = Array(Constant(c, 8) for c in LOG_KEY)
        hdr_i = Signal(2)
        node_i = Signal(4)

        fsm = FSM(reset_state="IDLE")
        self.submodules += fsm

        def answer(v):
            return [NextValue(result, _c(v) if isinstance(v, int) else v), NextState("ACK")]

        fsm.act("IDLE",
            If(core.host_req,
                NextValue(idx, core.host_idx),
                NextState("DISPATCH"),
            ),
        )
        fsm.act("DISPATCH",
            If(idx == I_LOG,
                If(in_range(a0, a1),
                    NextValue(is_log, 1),
                    NextValue(dat_at, a0),
                    NextValue(dat_n, Mux(a1 > MAX_LOG, MAX_LOG, a1)),
                    NextValue(rd_at, a0), NextValue(rd_i, 0),
                    NextValue(rd_n, Mux(a1 > MAX_LOG, MAX_LOG, a1)),
                    utf.start.eq(1),
                    NextState("UTF_RD"),
                ).Else(*answer(0)),
            ).Elif(idx == I_PUBLISH,
                If(~in_range(a0, a1) | ~in_range(a2, a3), *answer(E_OOB)
                ).Elif(a3 > MAX_MESSAGE, *answer(E_INVALID_ARG)
                ).Else(
                    NextValue(is_log, 0),
                    NextValue(key_at, a0), NextValue(key_n, a1),
                    NextValue(dat_at, a2), NextValue(dat_n, a3),
                    NextValue(rd_at, a0), NextValue(rd_i, 0), NextValue(rd_n, a1),
                    kc.start.eq(1),
                    NextState("KEY_RD"),
                ),
            ).Elif(idx == I_SUBSCRIBE,
                If(~in_range(a0, a1), *answer(E_OOB)).Else(
                    NextValue(key_at, a0), NextValue(key_n, a1),
                    NextValue(rd_at, a0), NextValue(rd_i, 0), NextValue(rd_n, a1),
                    kc.start.eq(1), own.start.eq(1),
                    NextState("KEY_RD"),
                ),
            ).Elif(idx == I_NOW_MS, *answer(self.now_ms)
            ).Elif(idx == I_SET_TICK,
                If(a0 == 0,
                    self.tick_set.eq(1),
                    NextValue(self.tick_on, 0),
                    *answer(0),
                ).Elif(a0[31] | (a0 < MIN_TICK), *answer(E_INVALID_ARG)
                ).Elif(~self.has_tick, *answer(E_UNSUPPORTED)
                ).Else(
                    self.tick_set.eq(1),
                    NextValue(self.tick_on, 1),
                    NextValue(self.tick_period, a0),
                    NextValue(self.tick_due, self.now_ms + a0),
                    *answer(0),
                ),
            ).Elif((idx >= I_GPIO_OUTPUT) & (idx <= I_GPIO_READ),
                If(a0 > 255, *answer(E_INVALID_ARG)
                ).Elif((idx == I_GPIO_INPUT) & (a1 > 2), *answer(E_INVALID_ARG)
                ).Elif((idx == I_GPIO_WRITE) & (a1 > 1), *answer(E_INVALID_ARG)
                ).Elif(idx == I_GPIO_READ,
                    *answer(Mux(a0 == 1, self.button, Mux(a0 == 0, self.led, 0))),
                ).Elif(a0 > 1, *answer(E_UNSUPPORTED)
                ).Else(
                    If((idx == I_GPIO_OUTPUT) & (a0 == 0), NextValue(self.led_enabled, 1)),
                    If((idx == I_GPIO_WRITE) & (a0 == 0), NextValue(self.led, a1[0])),
                    *answer(0),
                ),
            ).Elif(idx == I_NODE_ID,
                If(~in_range(a0, 8), *answer(E_OOB)).Else(
                    NextValue(rd_at, a0), NextValue(node_i, 0),
                    NextState("NODE_WR"),
                ),
            ).Elif(idx == I_ROUTE_BRIDGE,
                If((a0[3:] != 0) | (a3[3:] != 0) | (a0 == a3), *answer(E_INVALID_ARG)
                ).Elif(~in_range(a1, a2), *answer(E_OOB)
                ).Else(
                    NextValue(key_at, a1), NextValue(key_n, a2),
                    NextValue(prefix, 0),
                    NextValue(tail, 0),
                    NextValue(rd_at, a1 + a2 - Mux(a2 >= 3, 3, a2)),
                    NextValue(rd_i, 0), NextValue(rd_n, Mux(a2 >= 3, 3, a2)),
                    NextState("TAIL_RD"),
                ),
            ).Elif((idx == I_ROUTE_STOP) | (idx == I_ROUTE_STAT),
                If(~((a0 < 4) | ((a0 >= 16) & (a0 < 32))), *answer(E_INVALID_ARG)
                ).Elif((idx == I_ROUTE_STAT) & (a1 > 3), *answer(E_INVALID_ARG)
                ).Elif(a0 < 4, *answer(E_BUSY)
                ).Else(
                    NextValue(self.br_handle, a0),
                    NextState("BR_STOP_STAT"),
                ),
            ).Else(*answer(E_UNSUPPORTED)),
        )

        # --- keys: validate (and match for subscribe) while reading ---------
        fsm.act("KEY_RD",
            If(rd_i == rd_n, NextState("KEY_END")).Else(NextState("KEY_LATCH")),
        )
        fsm.act("KEY_LATCH",
            kc.stb.eq(1), kc.byte.eq(core.ext_mem_dat_r),
            own.stb.eq(1), own.idx.eq(rd_i), own.byte.eq(core.ext_mem_dat_r),
            sub_wr.adr.eq(rd_i[:7]),
            sub_wr.dat_w.eq(Cat(*[core.ext_mem_dat_r] * N_SUBS)),
            sub_wr.we.eq(Mux((idx == I_SUBSCRIBE) & (sub_count < N_SUBS) & (rd_i < 128),
                             1 << sub_count[:3], 0)),
            NextValue(rd_at, rd_at + 1),
            NextValue(rd_i, rd_i + 1),
            NextState("KEY_RD"),
        )
        # Matchers see the last byte two cycles later.
        own_wait = Signal(2)
        fsm.act("KEY_END",
            own.key_len.eq(key_n),
            NextValue(own_wait, own_wait + 1),
            If(own_wait == 2,
                NextValue(own_wait, 0),
                If(~kc.valid | kc.sys, *answer(E_INVALID_KEY)
                ).Elif(idx == I_PUBLISH,
                    NextValue(link, 0),
                    NextState("PUB_NEXT"),
                ).Else(  # subscribe
                    If(~self.has_messages, *answer(E_UNSUPPORTED)
                    ).Elif(own.hit != 0, *answer(own.slot)
                    ).Elif(sub_count >= N_SUBS, *answer(E_NO_RES)
                    ).Else(
                        NextValue(sub_lens[sub_count[:3]], key_n),
                        NextValue(sub_count, sub_count + 1),
                        *answer(sub_count),
                    ),
                ),
            ),
        )
        self.sync += If(self.reset_tables, sub_count.eq(0))

        # --- log: find the UTF-8 cut, then publish on sys/log ---------------
        fsm.act("UTF_RD",
            If(rd_i == rd_n, NextState("UTF_END")).Else(NextState("UTF_LATCH")),
        )
        fsm.act("UTF_LATCH",
            utf.stb.eq(1), utf.byte.eq(core.ext_mem_dat_r),
            NextValue(rd_at, rd_at + 1), NextValue(rd_i, rd_i + 1),
            NextState("UTF_RD"),
        )
        fsm.act("UTF_END",
            NextValue(dat_n, utf.upto),
            NextValue(key_n, len(LOG_KEY)),
            NextValue(link, 0),
            NextState("PUB_NEXT"),
        )

        # --- publications: one frame body per link with a peer --------------
        fsm.act("PUB_NEXT",
            If((self.peer_ok >> link)[0],
                NextValue(hdr_i, 0),
                NextState("PUB_HDR"),
            ).Elif(link == 0,
                NextValue(link, 1),
            ).Else(*answer(0)),
        )
        self.comb += [
            self.pub_link.eq(link),
            self.pub_len.eq(3 + key_n + dat_n),
        ]
        fsm.act("PUB_HDR",
            self.pub_req.eq(1),
            self.pub_valid.eq(1),
            self.pub_data.eq(Mux(hdr_i == 0, 3, Mux(hdr_i == 1, 0, key_n))),
            self.pub_last.eq((hdr_i == 2) & (key_n == 0) & (dat_n == 0)),
            If(self.pub_ready,
                NextValue(hdr_i, hdr_i + 1),
                If(hdr_i == 2,
                    NextValue(rd_i, 0),
                    NextValue(rd_at, key_at),
                    If(is_log, NextState("PUB_LOGKEY")).Else(NextState("PUB_KEY_RD")),
                ),
            ),
        )
        fsm.act("PUB_LOGKEY",
            self.pub_req.eq(1),
            self.pub_valid.eq(1),
            self.pub_data.eq(log_rom[rd_i[:3]]),
            self.pub_last.eq((rd_i == len(LOG_KEY) - 1) & (dat_n == 0)),
            If(self.pub_ready,
                NextValue(rd_i, rd_i + 1),
                If(rd_i == len(LOG_KEY) - 1,
                    NextValue(rd_i, 0), NextValue(rd_at, dat_at),
                    NextState("PUB_DAT_RD"),
                ),
            ),
        )
        fsm.act("PUB_KEY_RD",
            self.pub_req.eq(1),
            If(rd_i == key_n,
                NextValue(rd_i, 0), NextValue(rd_at, dat_at),
                NextState("PUB_DAT_RD"),
            ).Else(NextState("PUB_KEY")),
        )
        fsm.act("PUB_KEY",
            self.pub_req.eq(1),
            self.pub_valid.eq(1),
            self.pub_data.eq(core.ext_mem_dat_r),
            self.pub_last.eq((rd_i == key_n - 1) & (dat_n == 0)),
            If(self.pub_ready,
                NextValue(rd_i, rd_i + 1), NextValue(rd_at, rd_at + 1),
                NextState("PUB_KEY_RD"),
            ),
        )
        fsm.act("PUB_DAT_RD",
            self.pub_req.eq(1),
            If(rd_i == dat_n, NextState("PUB_LINK_DONE")).Else(NextState("PUB_DAT")),
        )
        fsm.act("PUB_DAT",
            self.pub_req.eq(1),
            self.pub_valid.eq(1),
            self.pub_data.eq(core.ext_mem_dat_r),
            self.pub_last.eq(rd_i == dat_n - 1),
            If(self.pub_ready,
                NextValue(rd_i, rd_i + 1), NextValue(rd_at, rd_at + 1),
                NextState("PUB_DAT_RD"),
            ),
        )
        fsm.act("PUB_LINK_DONE",
            If(link == 0, NextValue(link, 1), NextState("PUB_NEXT")).Else(*answer(0)),
        )

        # --- node_id: write the 8 bytes --------------------------------------
        fsm.act("NODE_WR",
            If(node_i == 8, *answer(0)).Else(
                self.mem_dat_w.eq((self.node_id >> Cat(Constant(0, 3), node_i[:3]))[:8]),
                self.mem_we.eq(1),
                NextValue(rd_at, rd_at + 1),
                NextValue(node_i, node_i + 1),
            ),
        )

        # --- route_bridge: "/**" suffix, then validate, then install --------
        fsm.act("TAIL_RD",
            If(rd_i == rd_n, NextState("TAIL_END")).Else(NextState("TAIL_LATCH")),
        )
        fsm.act("TAIL_LATCH",
            NextValue(tail, Cat(tail[8:24], core.ext_mem_dat_r)),
            NextValue(rd_at, rd_at + 1), NextValue(rd_i, rd_i + 1),
            NextState("TAIL_RD"),
        )
        is_prefix = Signal()
        self.comb += is_prefix.eq((key_n >= 3) & (tail == (ord("/") | ord("*") << 8 | ord("*") << 16)))
        fsm.act("TAIL_END",
            NextValue(prefix, is_prefix),
            NextValue(body_n, Mux(is_prefix, key_n - 3, key_n)),
            NextValue(rd_at, key_at), NextValue(rd_i, 0),
            NextValue(rd_n, Mux(is_prefix, key_n - 3, key_n)),
            kc.start.eq(1),
            NextState("BKEY_RD"),
        )
        fsm.act("BKEY_RD",
            If(rd_i == rd_n, NextState("BKEY_END")).Else(NextState("BKEY_LATCH")),
        )
        fsm.act("BKEY_LATCH",
            kc.stb.eq(1), kc.byte.eq(core.ext_mem_dat_r),
            NextValue(rd_at, rd_at + 1), NextValue(rd_i, rd_i + 1),
            NextState("BKEY_RD"),
        )
        fsm.act("BKEY_END",
            # `sys` exactly or `sys/...` (any first segment `sys`).
            If(~kc.valid | kc.sys, *answer(E_INVALID_KEY)
            ).Elif((a0 > 1) | (a3 > 1), *answer(E_UNSUPPORTED)
            ).Else(
                self.br_add_start.eq(1),
                self.br_add_src.eq(a0[:3]), self.br_add_dst.eq(a3[:3]),
                self.br_add_prefix.eq(prefix), self.br_add_len.eq(body_n),
                NextValue(rd_at, key_at), NextValue(rd_i, 0),
                NextState("BADD_RD"),
            ),
        )
        fsm.act("BADD_RD",
            If(rd_i == body_n,
                self.br_add_end.eq(1),
                NextState("BR_WAIT"),
            ).Else(NextState("BADD_LATCH")),
        )
        fsm.act("BADD_LATCH",
            self.br_add_stb.eq(1), self.br_add_byte.eq(core.ext_mem_dat_r),
            NextValue(rd_at, rd_at + 1), NextValue(rd_i, rd_i + 1),
            NextState("BADD_RD"),
        )
        fsm.act("BR_STOP_STAT",
            If(idx == I_ROUTE_STOP, self.br_rm_stb.eq(1)).Else(
                self.br_stat_stb.eq(1),
                self.br_stat_dropped.eq((a1 == 1) | (a1 == 3)),
            ),
            NextState("BR_WAIT"),
        )
        fsm.act("BR_WAIT",
            If(self.br_done, *answer(self.br_result)),
        )

        fsm.act("ACK",
            core.host_ack.eq(1),
            core.host_result.eq(result),
            NextState("IDLE"),
        )


class Runtime(Module):
    """WasmCore + ImportUnit + the event sequencing of `wasm-host`."""

    def __init__(self, bridge=None, mem_bytes=16 * 1024, n_match=2):
        self.submodules.core = core = WasmCore(mem_bytes)
        self.submodules.imports = imp = ImportUnit(core, bridge, n_match)
        aw = len(core.ext_mem_adr)
        # Exports (function index, valid bit) from the loader.
        self.exp_init = Signal(9)
        self.exp_tick = Signal(9)
        self.exp_alloc = Signal(9)
        self.exp_on_message = Signal(9)
        self.comb += [
            imp.has_tick.eq(self.exp_tick[8]),
            imp.has_messages.eq(self.exp_alloc[8] & self.exp_on_message[8]),
        ]
        # Events.
        self.ev_reset = Signal()
        self.ev_init = Signal()
        self.ev_tick = Signal()
        self.ev_deliver = Signal()
        self.dlv_slot = Signal(3)
        self.dlv_len = Signal(10)
        self.dlv_adr = Signal(10)       # source buffer read port
        self.dlv_dat = Signal(8)
        self.dlv_base = Signal(10)      # offset of the data in that buffer
        self.ev_done = Signal()
        self.ev_result = Signal(8)
        self.poisoned = Signal()
        self.running = Signal()
        # Loader / debug access to linear memory while no event runs.
        self.ld_mem_sel = Signal()
        self.ld_mem_adr = Signal(aw)
        self.ld_mem_dat = Signal(8)
        self.ld_mem_we = Signal()
        self.mem_rd_dat = Signal(8)

        # # #

        ptr = Signal(32)
        i = Signal(10)

        def call(fidx, nargs, *args):
            stmts = [core.call_start.eq(1), core.call_func.eq(fidx[:8]), core.call_nargs.eq(nargs)]
            for k, a in enumerate(args):
                stmts.append(core.call_args[k].eq(a))
            return stmts

        def finish(code):
            return [NextValue(self.ev_result, code), NextState("DONE")]

        def trapped():
            return [NextValue(self.poisoned, 1),
                    NextValue(self.ev_result, R_TRAP + core.trap), NextState("DONE")]

        tick_due_now = Signal()
        self.comb += tick_due_now.eq(~(self.imports.now_ms - imp.tick_due)[31])
        nxt = Signal(32)
        self.comb += nxt.eq(imp.tick_due + imp.tick_period)

        fsm = FSM(reset_state="IDLE")
        self.submodules += fsm
        self.comb += self.running.eq(~fsm.ongoing("IDLE") & ~fsm.ongoing("DONE"))
        fsm.act("IDLE",
            If(self.ev_reset,
                NextValue(self.poisoned, 0),
                imp.reset_tables.eq(1),
                NextValue(imp.tick_on, 0),
            ).Elif(self.ev_init,
                If(~self.exp_init[8], *finish(R_OK)).Else(
                    *call(self.exp_init, 0), NextState("INIT")),
            ).Elif(self.ev_tick,
                If(self.poisoned, *finish(R_POISONED)
                ).Elif(~self.exp_tick[8], *finish(R_NOTICK)
                ).Elif(~imp.tick_on, *finish(R_DISABLED)
                ).Elif(~tick_due_now, *finish(R_NOTDUE)
                ).Else(
                    # Schedule::advance: one period on, or now + period when
                    # more than a period behind.
                    NextValue(imp.tick_due, Mux(~(self.imports.now_ms - nxt)[31],
                                                self.imports.now_ms + imp.tick_period, nxt)),
                    *call(self.exp_tick, 1, self.imports.now_ms),
                    NextState("TICK"),
                ),
            ).Elif(self.ev_deliver,
                If(self.poisoned, *finish(R_POISONED)
                ).Elif(~imp.has_messages, *finish(R_FALSE)
                ).Else(
                    *call(self.exp_alloc, 1, self.dlv_len),
                    NextState("ALLOC"),
                ),
            ),
        )
        fsm.act("INIT",
            If(core.call_done,
                If(core.trap != 0, *trapped()).Else(*finish(R_OK)),
            ),
        )
        fsm.act("TICK",
            If(core.call_done,
                If(core.trap != 0, *trapped()).Else(*finish(R_OK)),
            ),
        )
        alloc_ok = Signal()
        self.comb += alloc_ok.eq((Cat(core.call_result, Constant(0, 1))
                                  + self.dlv_len) <= core.mem_len)
        fsm.act("ALLOC",
            If(core.call_done,
                If(core.trap != 0, *trapped()
                ).Elif((core.call_result == 0) | ~alloc_ok, *finish(R_FALSE)
                ).Else(
                    NextValue(ptr, core.call_result),
                    NextValue(i, 0),
                    NextState("COPY_RD"),
                ),
            ),
        )
        # Copy dlv_len bytes from the caller's buffer into guest memory.
        self.comb += self.dlv_adr.eq(self.dlv_base + i)
        fsm.act("COPY_RD",
            If(i == self.dlv_len,
                *call(self.exp_on_message, 3, self.dlv_slot, ptr, self.dlv_len),
                NextState("ON_MESSAGE"),
            ).Else(NextState("COPY_WR")),
        )
        fsm.act("COPY_WR",
            NextValue(i, i + 1),
            NextState("COPY_RD"),
        )
        # The one mux onto the core's memory port: the loader while idle,
        # the delivery copy, otherwise the import unit.
        self.comb += [
            If(self.ld_mem_sel & fsm.ongoing("IDLE"),
                core.ext_mem_adr.eq(self.ld_mem_adr),
                core.ext_mem_dat_w.eq(self.ld_mem_dat),
                core.ext_mem_we.eq(self.ld_mem_we),
            ).Elif(fsm.ongoing("COPY_WR"),
                core.ext_mem_adr.eq((ptr + i)[:aw]),
                core.ext_mem_dat_w.eq(self.dlv_dat),
                core.ext_mem_we.eq(1),
            ).Else(
                core.ext_mem_adr.eq(imp.mem_adr),
                core.ext_mem_dat_w.eq(imp.mem_dat_w),
                core.ext_mem_we.eq(imp.mem_we),
            ),
            self.mem_rd_dat.eq(core.ext_mem_dat_r),
        ]
        fsm.act("ON_MESSAGE",
            If(core.call_done,
                If(core.trap != 0, *trapped()).Else(*finish(R_OK)),
            ),
        )
        fsm.act("DONE",
            self.ev_done.eq(1),
            NextState("IDLE"),
        )
