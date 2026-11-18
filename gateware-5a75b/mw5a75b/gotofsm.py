"""A Migen `FSM` whose next state is encoded from per-target strobes.

Migen's FSM lowers each `NextState` to an assignment of the binary next
state, so every transition is another 8-bit mux in one long chain (yosys
does not re-encode the FSM: its register has an initial value). Here a
transition to state `t` raises a one-bit `goto[t]`, and

    next_state = encode(goto) if any goto else state

where bit `b` of `encode(goto)` is the OR of the strobes of the states
with bit `b` set. The actions stay in Migen's `Case` on the binary state.

`NextState` keeps Migen's meaning (the last one executed wins): it
clears the strobes of the state's other targets. `NextValue` and
`ongoing` are Migen's; the entering/leaving signals are not supported.
"""

from functools import reduce
from operator import or_

from migen import Case, If, Mux, Signal
from migen.fhdl.visit import NodeVisitor
from migen.genlib.fsm import FSM, NextState, _LowerNext


class _Targets(NodeVisitor):
    """The states named by the NextState statements in some actions."""

    def __init__(self, aliases):
        self.aliases = aliases
        self.found = []

    def visit_unknown(self, node):
        if isinstance(node, NextState):
            t = self.aliases.get(node.state, node.state)
            if t not in self.found:
                self.found.append(t)


class _LowerGoto(_LowerNext):
    def __init__(self, goto, aliases):
        super().__init__(None, None, aliases)
        self.goto = goto
        self.local = []          # the targets of the state being lowered

    def visit_unknown(self, node):
        if isinstance(node, NextState):
            t = self.aliases.get(node.state, node.state)
            return [self.goto[t].eq(1)] + [self.goto[u].eq(0) for u in self.local if u != t]
        return super().visit_unknown(node)


class GotoFSM(FSM):
    def do_finalize(self):
        if (self.before_entering_signals or self.before_leaving_signals
                or self.after_entering_signals or self.after_leaving_signals):
            raise NotImplementedError("GotoFSM: entering/leaving signals")
        states = list(self.actions.keys())
        self.encoding = {s: i for i, s in enumerate(states)}
        self.decoding = {i: s for s, i in self.encoding.items()}
        n = len(states)
        self.state = Signal(max=n, reset=self.encoding[self.reset_state])
        self.state._enumeration = self.decoding
        self.next_state = Signal(max=n)
        goto = {s: Signal(name=f"goto{i}") for s, i in self.encoding.items()}
        ls = _LowerGoto(goto, self.state_aliases)
        cases = {}
        for s, actions in self.actions.items():
            if not actions:
                continue
            targets = _Targets(self.state_aliases)
            targets.visit(actions)
            ls.local = targets.found
            cases[self.encoding[s]] = ls.visit(actions)
        self.comb += Case(self.state, cases).makedefault(self.encoding[self.reset_state])
        any_goto = Signal()
        self.comb += any_goto.eq(reduce(or_, goto.values()))
        enc = Signal(len(self.state))
        for b in range(len(enc)):
            strobes = [goto[s] for s, i in self.encoding.items() if (i >> b) & 1]
            if strobes:
                self.comb += enc[b].eq(reduce(or_, strobes))
        self.comb += self.next_state.eq(Mux(any_goto, enc, self.state))
        self.sync += self.state.eq(self.next_state)
        for register, next_value_ce, next_value in ls.registers:
            self.sync += If(next_value_ce, register.eq(next_value))
