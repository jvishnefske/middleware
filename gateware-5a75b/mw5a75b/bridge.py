"""The bridge engine (CL5): the ABI 6 rule table in gateware
(`middleware::wasm_abi::bridge::BridgeTable` semantics, checked against
`tests/vectors/bridge.json`).

Sixteen rules, each `valid, src link, dst link, prefix flag, key length`
in registers and its key bytes in one byte lane of a 128 x (16 x 8) key
memory, so one read gives byte `i` of every rule's key. A `KeyMatcher`
follows a key as it streams in (one byte per cycle, from the frame
decoder's key strobes) and keeps, per rule, whether every byte so far
matched (`alive`) and whether the byte just past the rule's key was `/`
(`slash`). At the end of the key:

    exact  rule:  alive and key_len == len
    prefix rule:  alive and (key_len == len or key_len > len and slash)

which is `Filter::matches`: `demo/**` (stored as `demo`, prefix) takes
`demo`, `demo/x`, `demo/x/y` but not `demox`.

There is one matcher per link for incoming publications, plus one for
installing: the new rule's key runs through it too, so an identical rule
(same src, dst, kind, length, bytes) is found and its handle returned,
as `BridgeTable::add` de-duplicates; otherwise the lowest free slot gets
the rule (its key bytes are written into that lane while they stream) and
its counters restart at zero.

Handles are `BRIDGE_BASE + slot` (16..31). Counters: per rule, forwarded
and dropped (saturating 32 bit), counted by `acct_*` after a forward
(`BridgeTable::account`). They live in a 32 x 32 RAM (forwarded at
`slot`, dropped at `16 + slot`) served by one sequencer: accounting
requests wait in an 8-entry FIFO (`acct_ready` low when full: the
forwarder waits rather than lose a count) and each matched rule costs a
read and a write; a stat is a read; installing a rule zeroes both of its
counters, after any accounting already queued (which belongs to the
rule the slot held before).
"""

from migen import Array, Cat, FSM, If, Memory, Module, Mux, NextState, NextValue, Signal
from migen.genlib.fifo import SyncFIFO

N_RULES = 16
MAX_KEY = 128
BRIDGE_BASE = 16
ERR_NO_RESOURCES = -3
ERR_BUSY = -7
SLASH = ord("/")


class KeyMatcher(Module):
    """Follows one streamed key against every rule's lane."""

    def __init__(self, rd, lens):
        self.start = Signal()
        self.stb = Signal()
        self.idx = Signal(8)
        self.byte = Signal(8)
        self.alive = Signal(N_RULES)
        self.slash = Signal(N_RULES)

        # # #

        self.comb += rd.adr.eq(self.idx[:7])
        stb1 = Signal()
        idx1 = Signal(8)
        byte1 = Signal(8)
        self.sync += stb1.eq(self.stb), idx1.eq(self.idx), byte1.eq(self.byte)
        for r in range(N_RULES):
            lane = rd.dat_r[8 * r:8 * r + 8]
            self.sync += If(self.start,
                self.alive[r].eq(1),
                self.slash[r].eq(0),
            ).Elif(stb1,
                If(idx1 < lens[r],
                    If(byte1 != lane, self.alive[r].eq(0)),
                ).Elif(idx1 == lens[r],
                    self.slash[r].eq(byte1 == SLASH),
                ),
            )


class BridgeTable(Module):
    def __init__(self, n_links=2):
        # Install: add_start (with src, dst, prefix, len), the key bytes on
        # add_stb/add_byte, then add_end; cmd_done pulses with cmd_result.
        self.add_start = Signal()
        self.add_src = Signal(3)
        self.add_dst = Signal(3)
        self.add_prefix = Signal()
        self.add_len = Signal(8)
        self.add_stb = Signal()
        self.add_byte = Signal(8)
        self.add_end = Signal()
        # Remove and stat by handle.
        self.rm_stb = Signal()
        self.clear = Signal()            # remove every rule (payload stopped)
        self.stat_stb = Signal()
        self.stat_dropped = Signal()     # 0: forwarded, 1: dropped
        self.handle = Signal(8)
        self.cmd_done = Signal()
        self.cmd_result = Signal((32, True))
        # Accounting after a forward.
        self.acct_stb = Signal()
        self.acct_rules = Signal(N_RULES)
        self.acct_sent = Signal(8)
        self.acct_ready = Signal()       # acct_stb is taken only when high
        # Match ports, one per link.
        self.m_start = [Signal(name=f"m{l}_start") for l in range(n_links)]
        self.m_stb = [Signal(name=f"m{l}_stb") for l in range(n_links)]
        self.m_idx = [Signal(8, name=f"m{l}_idx") for l in range(n_links)]
        self.m_byte = [Signal(8, name=f"m{l}_byte") for l in range(n_links)]
        self.m_end = [Signal(name=f"m{l}_end") for l in range(n_links)]
        self.m_len = [Signal(8, name=f"m{l}_len") for l in range(n_links)]
        self.m_valid = [Signal(name=f"m{l}_valid") for l in range(n_links)]
        self.m_links = [Signal(8, name=f"m{l}_links") for l in range(n_links)]
        self.m_rules = [Signal(N_RULES, name=f"m{l}_rules") for l in range(n_links)]

        # # #

        valid = Signal(N_RULES)
        src = Array(Signal(3, name=f"src{r}") for r in range(N_RULES))
        dst = Array(Signal(3, name=f"dst{r}") for r in range(N_RULES))
        prefix = Signal(N_RULES)
        lens = Array(Signal(8, name=f"len{r}") for r in range(N_RULES))
        self.valid = valid

        keys = Memory(8 * N_RULES, MAX_KEY)
        self.specials += keys
        wr = keys.get_port(write_capable=True, we_granularity=8)
        self.specials += wr

        def matcher():
            rd = keys.get_port(has_re=False)
            self.specials += rd
            m = KeyMatcher(rd, lens)
            self.submodules += m
            return m

        # Key end bookkeeping: a match is evaluated two cycles after the
        # end strobe, once the last byte has passed the memory and the
        # matcher's registers.
        def delayed(sig):
            d1, d2 = Signal(), Signal()
            self.sync += d1.eq(sig), d2.eq(d1)
            return d2

        # --- incoming publications ---------------------------------------
        for l in range(n_links):
            m = matcher()
            k = Signal(8, name=f"m{l}_klen")
            self.comb += [
                m.start.eq(self.m_start[l]),
                m.stb.eq(self.m_stb[l]),
                m.idx.eq(self.m_idx[l]),
                m.byte.eq(self.m_byte[l]),
            ]
            self.sync += If(self.m_end[l], k.eq(self.m_len[l]))
            hit = Signal(N_RULES, name=f"m{l}_hit")
            for r in range(N_RULES):
                self.comb += hit[r].eq(
                    valid[r] & (src[r] == l) & m.alive[r]
                    & Mux(prefix[r],
                          (k == lens[r]) | ((k > lens[r]) & m.slash[r]),
                          k == lens[r]))
            links = Signal(8, name=f"m{l}_hitlinks")
            for j in range(8):
                self.comb += links[j].eq(
                    _or_reduce([hit[r] & (dst[r] == j) for r in range(N_RULES)]))
            ev = delayed(self.m_end[l])
            self.sync += [
                self.m_valid[l].eq(ev),
                If(ev,
                    self.m_rules[l].eq(hit),
                    self.m_links[l].eq(links),
                ),
            ]

        # --- installing ---------------------------------------------------
        am = matcher()
        a_src = Signal(3)
        a_dst = Signal(3)
        a_prefix = Signal()
        a_len = Signal(8)
        a_idx = Signal(8)
        free = Signal(5)           # lowest free slot, or N_RULES
        self.comb += [
            am.start.eq(self.add_start),
            am.stb.eq(self.add_stb),
            am.idx.eq(a_idx),
            am.byte.eq(self.add_byte),
        ]
        free_now = Signal(5)
        self.comb += free_now.eq(N_RULES)
        for r in reversed(range(N_RULES)):
            self.comb += If(~valid[r], free_now.eq(r))
        self.sync += [
            If(self.add_start,
                a_src.eq(self.add_src),
                a_dst.eq(self.add_dst),
                a_prefix.eq(self.add_prefix),
                a_len.eq(self.add_len),
                a_idx.eq(0),
                free.eq(free_now),
            ).Elif(self.add_stb, a_idx.eq(a_idx + 1)),
        ]
        self.comb += [
            wr.adr.eq(a_idx[:7]),
            wr.dat_w.eq(Cat(*[self.add_byte] * N_RULES)),
            wr.we.eq(Mux(self.add_stb & (free < N_RULES), 1 << free[:4], 0)),
        ]
        same = Signal(N_RULES)
        for r in range(N_RULES):
            self.comb += same[r].eq(
                valid[r] & (src[r] == a_src) & (dst[r] == a_dst)
                & (prefix[r] == a_prefix) & (lens[r] == a_len) & am.alive[r])
        dup = Signal(5)
        self.comb += dup.eq(N_RULES)
        for r in reversed(range(N_RULES)):
            self.comb += If(same[r], dup.eq(r))
        add_eval = delayed(self.add_end)

        # --- commands and counters -----------------------------------------
        slot = Signal(5)
        in_range = Signal()
        self.comb += [
            slot.eq(self.handle - BRIDGE_BASE),
            in_range.eq((self.handle >= BRIDGE_BASE)
                        & (self.handle < BRIDGE_BASE + N_RULES)),
        ]
        sl = slot[:4]
        self.sync += [
            self.cmd_done.eq(0),
            If(add_eval,
                self.cmd_done.eq(1),
                If(dup < N_RULES,
                    self.cmd_result.eq(BRIDGE_BASE + dup),
                ).Elif(free < N_RULES,
                    self.cmd_result.eq(BRIDGE_BASE + free),
                    _set_bit(valid, free[:4], 1),
                    Array(src)[free[:4]].eq(a_src),
                    Array(dst)[free[:4]].eq(a_dst),
                    _set_bit(prefix, free[:4], a_prefix),
                    Array(lens)[free[:4]].eq(a_len),
                ).Else(
                    self.cmd_result.eq(ERR_NO_RESOURCES),
                ),
            ),
            If(self.rm_stb,
                self.cmd_done.eq(1),
                If(in_range & (valid >> sl)[0],
                    _set_bit(valid, sl, 0),
                    self.cmd_result.eq(0),
                ).Else(self.cmd_result.eq(ERR_BUSY)),
            ),
            If(self.clear, valid.eq(0)),
        ]

        # --- counters ------------------------------------------------------
        counters = Memory(32, 2 * N_RULES)
        self.specials += counters
        cp = counters.get_port(write_capable=True)
        self.specials += cp
        fifo = SyncFIFO(N_RULES + 8, 8)
        self.submodules += fifo
        self.comb += [
            fifo.din.eq(Cat(self.acct_rules, self.acct_sent)),
            fifo.we.eq(self.acct_stb & fifo.writable),
            self.acct_ready.eq(fifo.writable),
        ]
        # Slots whose counters must be zeroed (installed since), one bit
        # each so back-to-back installs are all served.
        zero_pend = Signal(N_RULES)
        zero_slot = Signal(4)
        stat_req = Signal()
        stat_adr = Signal(5)
        self.sync += [
            If(self.stat_stb,
                If(in_range & (valid >> sl)[0],
                    stat_req.eq(1),
                    stat_adr.eq(Cat(sl, self.stat_dropped)),
                ).Else(
                    self.cmd_done.eq(1),
                    self.cmd_result.eq(ERR_BUSY),
                ),
            ),
        ]
        cur_rules = Signal(N_RULES)
        cur_sent = Signal(8)
        r_i = Signal(5)
        c_adr = Signal(5)
        dsts = Array(dst)
        fsm = FSM(reset_state="IDLE")
        self.submodules += fsm
        self.comb += cp.adr.eq(c_adr)
        zero_low = Signal(4)
        for r in reversed(range(N_RULES)):
            self.comb += If(zero_pend[r], zero_low.eq(r))
        # Accounting queued before an install is counted first (on the old
        # rule), then the new rule's counters are zeroed.
        fsm.act("IDLE",
            If(fifo.readable,
                # Accounting queued before a stat is counted first, as
                # BridgeTable::account runs before a later route_stat.
                fifo.re.eq(1),
                NextValue(cur_rules, fifo.dout[:N_RULES]),
                NextValue(cur_sent, fifo.dout[N_RULES:]),
                NextValue(r_i, 0),
                NextState("ACCT_SCAN"),
            ).Elif(zero_pend != 0,
                NextValue(zero_slot, zero_low),
                NextValue(c_adr, zero_low),
                NextState("ZERO_FWD"),
            ).Elif(stat_req,
                NextValue(c_adr, stat_adr),
                NextState("STAT_READ"),
            ),
        )
        fsm.act("ZERO_FWD",
            cp.dat_w.eq(0), cp.we.eq(1),
            NextValue(c_adr, Cat(zero_slot, 1)),
            NextState("ZERO_DRP"),
        )
        fsm.act("ZERO_DRP",
            cp.dat_w.eq(0), cp.we.eq(1),
            NextState("IDLE"),
        )
        # Clear the served bit before a new install may set another (the
        # later statement wins on the same bit, which cannot be the one
        # just zeroed: that slot is valid now).
        self.sync += If(fsm.ongoing("ZERO_DRP"),
            zero_pend.eq(zero_pend & ~(1 << zero_slot)))
        self.sync += If(add_eval & (dup == N_RULES) & (free < N_RULES)
                        & ~fsm.ongoing("ZERO_DRP"),
            zero_pend.eq(zero_pend | (1 << free[:4])))
        self.sync += If(add_eval & (dup == N_RULES) & (free < N_RULES)
                        & fsm.ongoing("ZERO_DRP"),
            zero_pend.eq((zero_pend & ~(1 << zero_slot)) | (1 << free[:4])))
        fsm.act("STAT_READ",
            NextState("STAT_DONE"),
        )
        fsm.act("STAT_DONE",
            NextState("IDLE"),
        )
        self.sync += If(fsm.ongoing("STAT_DONE"),
            stat_req.eq(0),
            self.cmd_done.eq(1),
            self.cmd_result.eq(_sat31(cp.dat_r)),
        )
        # Accounting: for each matched rule, count forwarded or dropped.
        # The lowest matched rule still to count, one per three cycles.
        todo = Signal(N_RULES)
        self.comb += todo.eq(cur_rules & valid)
        low = Signal(4)
        for r in reversed(range(N_RULES)):
            self.comb += If(todo[r], low.eq(r))
        sent_ok = Signal()
        self.comb += sent_ok.eq((cur_sent >> dsts[low])[0])
        fsm.act("ACCT_SCAN",
            If(todo == 0,
                NextState("IDLE"),
            ).Else(
                NextValue(c_adr, Cat(low, ~sent_ok)),
                NextValue(r_i, low),
                _set_bit_next(cur_rules, low),
                NextState("ACCT_READ"),
            ),
        )
        fsm.act("ACCT_READ",
            NextState("ACCT_WRITE"),
        )
        fsm.act("ACCT_WRITE",
            cp.dat_w.eq(Mux(cp.dat_r == 0xFFFF_FFFF, cp.dat_r, cp.dat_r + 1)), cp.we.eq(1),
            NextState("ACCT_SCAN"),
        )


def _or_reduce(xs):
    out = xs[0]
    for x in xs[1:]:
        out = out | x
    return out


def _set_bit(sig, idx, value):
    """`sig[idx] = value` for a run-time index."""
    return sig.eq(Mux(value, sig | (1 << idx), sig & ~(1 << idx)))


def _set_bit_next(sig, idx):
    """Clears bit `idx` of `sig` at the next FSM transition."""
    return NextValue(sig, sig & ~(1 << idx))


def _sat31(v):
    """A u32 count as an i32 result, saturating at i32::MAX."""
    return Mux(v[31], 0x7FFF_FFFF, v)
