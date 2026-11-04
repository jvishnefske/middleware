// AssemblyScript for the payload's state-machine tables (model/machines.ts).
// Pure: machines in, source lines out; the Blocks generator (generator.ts)
// owns the module and passes a `MachineHost` for the shared parts (string
// constants, imports, helpers, pin labels, issues).
//
// For machine `k` (its index in the payload's list) the code is:
//
// * `const SM{k}_<state>: i32` (the state's index) and `const
//   EV{k}_<event>: i32`; `let sm{k}: i32` (the current state, starting at
//   the initial one) and `let sm{k}Pending: i32 = -1` (its one queued
//   event); one shared `let smBusy: i32` (0 idle, 1 stepping, 2 draining);
// * per event that has rows, `sm{k}_on_<event>()`: one `switch (sm{k})`
//   over the states with rows for it. In a case, the guarded rows become
//   `if (<guard> != 0) ... else if ...` in table order and the unguarded
//   row is the final `else` (no unguarded row: no `else`, nothing
//   happens when every guard is false). A row runs its actions; if `next`
//   differs from `state` it sets `sm{k}`, logs `machineLogMove(...)` and
//   calls `sm{k}_enter_<next>()`. A self-transition runs only the actions;
//   no log, no entry;
// * `sm{k}_enter_<state>()`, only for a state whose entry actions or
//   "when <state> is entered" blocks are non-empty: the entry actions,
//   then those blocks' bodies in workspace order;
// * `sm{k}_step(ev)`: a `switch (ev)` calling the event's function;
// * `sm{k}_raise(ev)`, the bounded queue: while draining (`smBusy == 2`)
//   the event is dropped and logged; while stepping (`smBusy == 1`) it
//   takes the machine's empty slot, or is dropped and logged when the slot
//   is full; otherwise `smBusy = 1`, one step, then `smDrain()`;
// * `smDrain()` (shared): `smBusy = 2`, each machine's slot run once in
//   machine order, `smBusy = 0`. So an external raise runs at most
//   1 + (number of machines) steps;
// * in `init` (after the pin table, subscriptions, pin interrupts and the
//   tick, before the on-init stacks): `smBusy = 1`, then per machine in
//   order the `machineLogInit(...)` line and its initial state's entry,
//   then one `smDrain()`.
//
// No loops, no allocation, every value an i32. Guards compile to i32
// expressions (`mw_gpio_read(PIN_<label>)`, constants, `<i32>(a op b)`,
// `<i32>(a != 0 && b != 0)`, `<i32>(a == 0)`).
//
// The Rust check (`checkMachines`) normally refuses a bad table before
// the build, but this module never trusts it: a machine over the limits,
// with a bad identifier, a duplicate name or no states is reported and
// left out; a row naming an unknown state, event or next state, an action
// naming an unknown pin or event, a second unguarded row in a cell and a
// guard deeper than `MAX_EXPR_DEPTH` are reported and that piece is left
// out (a bad guard reads as 0). The output is always valid source.

import {
  type ActionRef,
  type Expr,
  MAX_ACTIONS,
  MAX_EVENTS,
  MAX_EXPR_DEPTH,
  MAX_MACHINE_ROWS,
  MAX_MACHINES,
  MAX_STATES,
  type Machine,
  type TransitionRow,
  machineLogInit,
  machineLogMove,
} from "../model/machines";
import type { PinFunc } from "../model/pins";

/** What machine code needs from the module being generated. */
export interface MachineHost {
  /** Identifier of a constant byte string (`${id}_LEN` its length). */
  str(text: string): string;
  /** Imports host function `mw_<name>`. */
  use(name: string): void;
  /** Emits a run-time helper (pinWrite, pinToggle). */
  helper(name: string): void;
  /** The `PIN_<label>` constant for a pin-table row of one of `funcs`, or
   * the problem. */
  pin(label: string, funcs: readonly PinFunc[], what: string): { ident: string } | { problem: string };
  error(message: string): void;
  warn(message: string): void;
  /** Checks a publish key; the problem, or null. */
  keyProblem(key: string): string | null;
  /** Free text made safe for a `//` comment (generator.ts `commentText`). */
  comment(text: string): string;
}

/** A machine that passed the structural checks and gets code. */
export interface PlannedMachine {
  /** Index in the payload's machine list. */
  k: number;
  m: Machine;
  states: ReadonlyMap<string, number>;
  events: ReadonlyMap<string, number>;
}

/** Largest message text (wasm_abi::MAX_MESSAGE). */
const MAX_TEXT_BYTES = 512;

const IDENT = /^[A-Za-z][A-Za-z0-9_]*$/;

const CMP_AS: Readonly<Record<string, string>> = { eq: "==", ne: "!=", lt: "<", le: "<=", gt: ">", ge: ">=" };

/** How issues name a machine: `machine Mode`, or by position when its id
 * is unusable. */
function who(m: Machine, k: number): string {
  return IDENT.test(m.id) ? `machine ${m.id}` : `machine ${k + 1}`;
}

/** Machines that get code, in order; every machine left out has an error
 * explaining why. */
export function planMachines(ms: readonly Machine[], host: MachineHost): PlannedMachine[] {
  const out: PlannedMachine[] = [];
  const ids = new Set<string>();
  ms.forEach((m, k) => {
    const at = who(m, k);
    const fail = (msg: string) => host.error(`${at}: ${msg}`);
    if (k >= MAX_MACHINES) return fail(`a payload has at most ${MAX_MACHINES} state machines`);
    if (!IDENT.test(m.id)) return fail(`the id "${m.id}" must start with a letter and hold only letters, digits and _`);
    if (ids.has(m.id)) return fail("another state machine has this id");
    ids.add(m.id);
    if (m.states.length === 0) return fail("has no states");
    if (m.states.length > MAX_STATES) return fail(`has more than ${MAX_STATES} states`);
    if (m.events.length > MAX_EVENTS) return fail(`has more than ${MAX_EVENTS} events`);
    if (m.rows.length > MAX_MACHINE_ROWS) return fail(`has more than ${MAX_MACHINE_ROWS} rows`);
    const states = new Map<string, number>();
    for (const s of m.states) {
      if (!IDENT.test(s.name)) return fail(`state "${s.name}" must start with a letter and hold only letters, digits and _`);
      if (states.has(s.name)) return fail(`two states are named ${s.name}`);
      states.set(s.name, states.size);
    }
    const events = new Map<string, number>();
    for (const e of m.events) {
      if (!IDENT.test(e.name)) return fail(`event "${e.name}" must start with a letter and hold only letters, digits and _`);
      if (events.has(e.name)) return fail(`two events are named ${e.name}`);
      events.set(e.name, events.size);
    }
    if (!states.has(m.initial)) host.error(`${at}: the initial state "${m.initial}" is not one of its states; the first state is used`);
    out.push({ k, m, states, events });
  });
  return out;
}

/** The planned machine with this id. */
export function plannedById(plan: readonly PlannedMachine[], id: string): PlannedMachine | undefined {
  return plan.find((p) => p.m.id === id);
}

export function stateConst(k: number, st: string): string {
  return `SM${k}_${st}`;
}
export function eventConst(k: number, ev: string): string {
  return `EV${k}_${ev}`;
}
export function enterFn(k: number, st: string): string {
  return `sm${k}_enter_${st}`;
}
export function raiseCall(k: number, ev: string): string {
  return `sm${k}_raise(${eventConst(k, ev)});`;
}
export function inStateExpr(k: number, st: string): string {
  return `<i32>(sm${k} == ${stateConst(k, st)})`;
}

/** The machine code: constants, globals, functions and init lines. */
export interface MachineCode {
  consts: string[];
  globals: string[];
  fns: string[];
  init: string[];
}

class Emitter {
  /** Events raised by table actions, as `k.event`. */
  readonly raised = new Set<string>();

  constructor(
    private readonly host: MachineHost,
    private readonly p: PlannedMachine,
    /** Body lines of "when <state> is entered" blocks, by state. */
    private readonly hats: ReadonlyMap<string, readonly string[]>,
  ) {}

  private get at(): string {
    return who(this.p.m, this.p.k);
  }

  /** Whether `sm{k}_enter_<st>` exists. */
  hasEnter(st: string): boolean {
    const s = this.p.m.states.find((x) => x.name === st);
    return (s?.entry.length ?? 0) > 0 || (this.hats.get(st)?.length ?? 0) > 0;
  }

  expr(e: Expr, depth: number, where: string): string {
    if (depth > MAX_EXPR_DEPTH) {
      this.host.error(`${this.at} ${where}: the guard is nested deeper than ${MAX_EXPR_DEPTH} levels`);
      return "0";
    }
    switch (e.kind) {
      case "pin": {
        const p = this.host.pin(e.label, ["gpio-in", "gpio-out"], "a GPIO input or output");
        if ("problem" in p) {
          this.host.error(`${this.at} ${where}: ${p.problem}`);
          return "0";
        }
        this.host.use("gpio_read");
        return `mw_gpio_read(${p.ident})`;
      }
      case "const":
        if (!Number.isInteger(e.value) || e.value < -2147483648 || e.value > 2147483647) {
          this.host.error(`${this.at} ${where}: ${e.value} is not a whole 32-bit number`);
          return "0";
        }
        return e.value < 0 ? `(${e.value})` : String(e.value);
      case "cmp": {
        const op = CMP_AS[e.op];
        if (!op) {
          this.host.error(`${this.at} ${where}: unknown comparison "${String(e.op)}"`);
          return "0";
        }
        return `<i32>(${this.expr(e.a, depth + 1, where)} ${op} ${this.expr(e.b, depth + 1, where)})`;
      }
      case "and":
      case "or":
        return `<i32>(${this.expr(e.a, depth + 1, where)} != 0 ${e.kind === "and" ? "&&" : "||"} ${this.expr(e.b, depth + 1, where)} != 0)`;
      case "not":
        return `<i32>(${this.expr(e.a, depth + 1, where)} == 0)`;
      default:
        this.host.error(`${this.at} ${where}: unknown guard`);
        return "0";
    }
  }

  /** Pointer and length arguments for constant text. */
  private textArgs(text: string): string {
    if (text === "") return "0, 0";
    const id = this.host.str(text);
    return `${id}, ${id}_LEN`;
  }

  actions(as: readonly ActionRef[], where: string, pad: string, out: string[]): void {
    if (as.length > MAX_ACTIONS) this.host.error(`${this.at} ${where}: at most ${MAX_ACTIONS} actions`);
    for (const a of as.slice(0, MAX_ACTIONS)) {
      switch (a.kind) {
        case "write":
        case "toggle": {
          const p = this.host.pin(a.label, ["gpio-out"], "a GPIO output");
          if ("problem" in p) {
            this.host.error(`${this.at} ${where}: ${p.problem}`);
            break;
          }
          this.host.helper(a.kind === "write" ? "pinWrite" : "pinToggle");
          out.push(a.kind === "write" ? `${pad}pinWrite(${p.ident}, ${a.level ? 1 : 0});` : `${pad}pinToggle(${p.ident});`);
          break;
        }
        case "publish": {
          const kp = this.host.keyProblem(a.key);
          if (kp) {
            this.host.error(`${this.at} ${where}: ${kp}`);
            break;
          }
          if (new TextEncoder().encode(a.text).length > MAX_TEXT_BYTES) {
            this.host.error(`${this.at} ${where}: the text is longer than ${MAX_TEXT_BYTES} bytes`);
            break;
          }
          this.host.use("publish");
          const k = this.host.str(a.key);
          out.push(`${pad}mw_publish(${k}, ${k}_LEN, ${this.textArgs(a.text)});`);
          break;
        }
        case "log":
          if (new TextEncoder().encode(a.text).length > MAX_TEXT_BYTES) {
            this.host.error(`${this.at} ${where}: the text is longer than ${MAX_TEXT_BYTES} bytes`);
            break;
          }
          this.host.use("log");
          out.push(`${pad}mw_log(${this.textArgs(a.text)});`);
          break;
        case "raise":
          if (!this.p.events.has(a.event)) {
            this.host.error(`${this.at} ${where}: raises "${a.event}", which is not one of its events`);
            break;
          }
          this.raised.add(`${this.p.k}.${a.event}`);
          out.push(`${pad}${raiseCall(this.p.k, a.event)}`);
          break;
        default:
          this.host.error(`${this.at} ${where}: unknown action`);
      }
    }
  }

  /** A row's actions and, for a change of state, the move. */
  private rowBody(r: TransitionRow, i: number, pad: string, out: string[]): void {
    const { k, m } = this.p;
    this.actions(r.actions, `row ${i + 1}`, pad, out);
    if (r.next === r.state) return;
    out.push(`${pad}sm${k} = ${stateConst(k, r.next)};`);
    this.host.use("log");
    out.push(`${pad}mw_log(${this.textArgs(machineLogMove(m.id, r.state, r.next, r.event))});`);
    if (this.hasEnter(r.next)) out.push(`${pad}${enterFn(k, r.next)}();`);
  }

  /** Rows that get code, by event then state, table order kept; bad rows
   * are reported and left out. */
  private cells(): Map<string, Map<string, { i: number; r: TransitionRow }[]>> {
    const { m, states, events } = this.p;
    const byEvent = new Map<string, Map<string, { i: number; r: TransitionRow }[]>>();
    m.rows.forEach((r, i) => {
      const bad = (msg: string) => this.host.error(`${this.at} row ${i + 1}: ${msg}`);
      if (!states.has(r.state)) return bad(`"${r.state}" is not one of its states`);
      if (!events.has(r.event)) return bad(`"${r.event}" is not one of its events`);
      if (!states.has(r.next)) return bad(`the next state "${r.next}" is not one of its states`);
      let byState = byEvent.get(r.event);
      if (!byState) byEvent.set(r.event, (byState = new Map()));
      const cell = byState.get(r.state) ?? [];
      if (r.guard === null && cell.some((c) => c.r.guard === null)) {
        return bad(`${r.state} already has a row without a guard for ${r.event}; only the first is used`);
      }
      byState.set(r.state, [...cell, { i, r }]);
    });
    return byEvent;
  }

  emit(code: MachineCode): void {
    const { k, m, states, events } = this.p;
    const name = m.name === "" ? "" : ` ${this.host.comment(m.name)}`;
    code.consts.push(`// State machine ${k}: ${m.id}${name}`);
    for (const [s, i] of states) code.consts.push(`const ${stateConst(k, s)}: i32 = ${i};`);
    for (const [e, i] of events) code.consts.push(`const ${eventConst(k, e)}: i32 = ${i};`);
    const initial = states.has(m.initial) ? m.initial : (m.states[0]?.name ?? "");
    code.globals.push(
      `// ${m.id}: the current state and the one queued event (-1: none).`,
      `let sm${k}: i32 = ${stateConst(k, initial)};`,
      `let sm${k}Pending: i32 = -1;`,
    );

    const fns: string[] = [];
    for (const s of m.states) {
      if (!this.hasEnter(s.name)) continue;
      const body: string[] = [];
      this.actions(s.entry, `state ${s.name} entry`, "  ", body);
      body.push(...(this.hats.get(s.name) ?? []));
      fns.push([`// Entering ${m.id}.${s.name}.`, `function ${enterFn(k, s.name)}(): void {`, ...body, "}"].join("\n"));
    }

    const cells = this.cells();
    const handled: string[] = [];
    for (const e of m.events) {
      const byState = cells.get(e.name);
      if (!byState || byState.size === 0) continue;
      handled.push(e.name);
      const lines = [`function sm${k}_on_${e.name}(): void {`, `  switch (sm${k}) {`];
      for (const s of m.states) {
        const rows = byState.get(s.name);
        if (!rows) continue;
        lines.push(`    case ${stateConst(k, s.name)}:`);
        const guarded = rows.filter((c) => c.r.guard !== null);
        const plain = rows.find((c) => c.r.guard === null);
        if (guarded.length === 0 && plain) {
          this.rowBody(plain.r, plain.i, "      ", lines);
        } else {
          guarded.forEach((c, j) => {
            const g = this.expr(c.r.guard as Expr, 1, `row ${c.i + 1}`);
            lines.push(`      ${j === 0 ? "if" : "} else if"} (${g} != 0) {`);
            this.rowBody(c.r, c.i, "        ", lines);
          });
          if (plain) {
            lines.push("      } else {");
            this.rowBody(plain.r, plain.i, "        ", lines);
          }
          lines.push("      }");
        }
        lines.push("      break;");
      }
      lines.push("  }", "}");
      fns.push(lines.join("\n"));
    }

    const step = [`function sm${k}_step(ev: i32): void {`];
    if (handled.length > 0) {
      step.push("  switch (ev) {");
      for (const e of handled) step.push(`    case ${eventConst(k, e)}:`, `      sm${k}_on_${e}();`, "      break;");
      step.push("  }");
    }
    step.push("}");
    fns.push(step.join("\n"));

    this.host.use("log");
    const drop = [`// Logs a raise of ${m.id} that the bounded queue drops.`, `function sm${k}_drop(ev: i32): void {`];
    if (m.events.length > 0) {
      drop.push("  switch (ev) {");
      for (const e of m.events) drop.push(`    case ${eventConst(k, e.name)}:`, `      mw_log(${this.textArgs(dropLog(m.id, e.name))});`, "      break;");
      drop.push("  }");
    }
    drop.push("}");
    fns.push(drop.join("\n"));

    fns.push(
      [
        `// Raises an event of ${m.id}: steps now when idle, else queues it (one slot).`,
        `function sm${k}_raise(ev: i32): void {`,
        "  if (smBusy == 2) {",
        `    sm${k}_drop(ev);`,
        "    return;",
        "  }",
        "  if (smBusy == 1) {",
        `    if (sm${k}Pending < 0) {`,
        `      sm${k}Pending = ev;`,
        "    } else {",
        `      sm${k}_drop(ev);`,
        "    }",
        "    return;",
        "  }",
        "  smBusy = 1;",
        `  sm${k}_step(ev);`,
        "  smDrain();",
        "}",
      ].join("\n"),
    );
    code.fns.push(...fns);

    code.init.push(`  mw_log(${this.textArgs(machineLogInit(m.id, initial))});`);
    if (this.hasEnter(initial)) code.init.push(`  ${enterFn(k, initial)}();`);
  }
}

/** The log line for a dropped raise (not a `parseMachineLog` line). */
export function dropLog(id: string, ev: string): string {
  return `sm ${id}: dropped ${ev}`;
}

/** The code for the planned machines. `hats` holds the bodies of "when
 * <state> is entered" blocks by `k.state`; `blockRaised` the `k.event`
 * pairs that raise blocks name (for the never-raised warning). */
export function emitMachines(
  plan: readonly PlannedMachine[],
  hats: ReadonlyMap<string, readonly string[]>,
  blockRaised: ReadonlySet<string>,
  host: MachineHost,
): MachineCode {
  const code: MachineCode = { consts: [], globals: [], fns: [], init: [] };
  if (plan.length === 0) return code;
  const raised = new Set(blockRaised);
  for (const p of plan) {
    const mine = new Map<string, readonly string[]>();
    for (const s of p.m.states) {
      const h = hats.get(`${p.k}.${s.name}`);
      if (h) mine.set(s.name, h);
    }
    const em = new Emitter(host, p, mine);
    em.emit(code);
    for (const r of em.raised) raised.add(r);
  }
  for (const p of plan) {
    for (const e of p.m.events) {
      if (!raised.has(`${p.k}.${e.name}`)) host.warn(`${who(p.m, p.k)}: event ${e.name} is never raised (no raise block or action names it)`);
    }
  }
  code.globals.unshift("// State machines: 0 idle, 1 stepping, 2 draining the queued raises.", "let smBusy: i32 = 0;");
  const drain = ["// Runs each machine's queued event once, in machine order; raises meanwhile are dropped.", "function smDrain(): void {", "  smBusy = 2;"];
  for (const p of plan) {
    drain.push(`  if (sm${p.k}Pending >= 0) {`, `    const ev = sm${p.k}Pending;`, `    sm${p.k}Pending = -1;`, `    sm${p.k}_step(ev);`, "  }");
  }
  drain.push("  smBusy = 0;", "}");
  code.fns.push(drain.join("\n"));
  code.init.unshift("  // State machines: initial states and their entry, then the queued raises.", "  smBusy = 1;");
  code.init.push("  smDrain();");
  return code;
}
