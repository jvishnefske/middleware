// State machines: the payload's state-machine tables (payload model
// version 2 and later, `machines`). Pure data, a strict shape check and pure
// transitions (every change returns a new machine); what is *allowed* in a
// table (identifier rules, uniqueness, one unguarded row per cell, guard
// overlap, labels of the right pin function, size limits, unreachable
// states and dead events) is not decided here but by protocol-wasm
// `checkMachines` (Rust, the `state-table` crate), reached through a
// `MachineChecker`, the same way the pin table is checked.
//
// A machine:
//
//   { id: "Mode",                 // identifier; names the generated code
//     name: "LED mode",           // free text for the reader
//     initial: "Off",             // a state name
//     states: [{ name, note, entry: ActionRef[] }],
//     events: [{ name, note }],
//     rows:   [{ state, event, guard: Expr | null, next, actions: ActionRef[] }] }
//
// A row fires when its machine is in `state`, `event` is raised and its
// guard (if any) is true. Its `actions` run; then, if `next` differs from
// `state`, the machine moves to `next` and runs `next`'s entry actions
// (a self-transition runs only the row's actions). The initial state's
// entry actions run once in `init`.
//
// `Expr` is a closed, i32-only grammar of bounded depth (`MAX_EXPR_DEPTH`):
// a digital pin read by label (0 or 1), a constant, a machine being in a
// state (`state`, 0 or 1; model version 3), a comparison, and/or, not.
// Because every leaf is a 0/1 pin, a 0/1 state test or a constant, two
// guards can be compared exactly by trying every leaf combination (one
// machine is never in two states), which is how the Rust check decides
// overlap.
//
// `ActionRef` is a closed set too: write / toggle a pin by label, publish
// constant text on a key, log constant text, raise an event of a machine
// (`machine` absent: the table's own; version 3 lets a raise name another
// machine, and a flow's raise always names one). There is no arbitrary
// code, so generated dispatch stays a fixed `switch` with no loops.
//
// Raised events are bounded: a raise while a machine is stepping (from a
// row's or an entry action, from a "when <machine> enters <state>" block,
// or from a flow entered from a state) is queued, at most one per machine (a second is dropped and
// logged), and the queue is drained once after the outermost step; a raise
// during the drain is dropped and logged. So one external raise runs at
// most 1 + (number of machines) steps. The generator implements this and
// the `state-table` crate documents it.
//
// Blocks refer to machines by `<machine id>.<state or event name>` in the
// `STATE` / `EVENT` field of the machine blocks (`MACHINE_STATE_BLOCKS`,
// `MACHINE_EVENT_BLOCKS`); renaming a state or event in the table renames
// those references (`renameMachineRef`).

import type { PinFunc } from "./pins";
import { type WorkspaceJson, everyBlock } from "../blocks/workspace";

/** Most machines in a payload. */
export const MAX_MACHINES = 4;
/** Most states and events per machine (the Rust check's limit). */
export const MAX_STATES = 16;
export const MAX_EVENTS = 16;
/** Most transition rows per machine (the Rust check's limit). */
export const MAX_MACHINE_ROWS = 64;
/** Most actions per row or per state entry. */
export const MAX_ACTIONS = 4;
/** Deepest guard expression (a leaf at the root is depth 1). */
export const MAX_EXPR_DEPTH = 4;
/** Shape bounds (parse): a file over these is refused; between the check
 * limits above and these, it loads and the Rust check reports the size. */
export const PARSE_MAX_NAMED = 64;
export const PARSE_MAX_ROWS = 256;
export const MAX_NAME_TEXT = 200;

export const CMP_OPS = ["eq", "ne", "lt", "le", "gt", "ge"] as const;
export type CmpOp = (typeof CMP_OPS)[number];
export const CMP_TEXT: Readonly<Record<CmpOp, string>> = { eq: "=", ne: "≠", lt: "<", le: "≤", gt: ">", ge: "≥" };

export type Expr =
  | { kind: "pin"; label: string }
  | { kind: "const"; value: number }
  /** 1 while machine `machine` is in `state`, else 0 (model version 3). */
  | { kind: "state"; machine: string; state: string }
  | { kind: "cmp"; op: CmpOp; a: Expr; b: Expr }
  | { kind: "and"; a: Expr; b: Expr }
  | { kind: "or"; a: Expr; b: Expr }
  | { kind: "not"; a: Expr };

export type ActionRef =
  | { kind: "write"; label: string; level: 0 | 1 }
  | { kind: "toggle"; label: string }
  | { kind: "publish"; key: string; text: string }
  | { kind: "log"; text: string }
  /** `machine` absent: the table's own machine (flows always name it). */
  | { kind: "raise"; event: string; machine?: string };

export const ACTION_KINDS = ["write", "toggle", "publish", "log", "raise"] as const;
export type ActionKind = (typeof ACTION_KINDS)[number];

export interface MachineState {
  name: string;
  note: string;
  /** Run on entering the state (and for the initial state, in init). */
  entry: readonly ActionRef[];
}

export interface MachineEvent {
  name: string;
  note: string;
}

export interface TransitionRow {
  state: string;
  event: string;
  guard: Expr | null;
  next: string;
  actions: readonly ActionRef[];
}

export interface Machine {
  id: string;
  name: string;
  initial: string;
  states: readonly MachineState[];
  events: readonly MachineEvent[];
  rows: readonly TransitionRow[];
}

/** One problem `checkMachines` found (protocol-wasm, `state-table`). */
export interface MachineIssue {
  /** Index into the payload's machines. */
  machine: number;
  /** Which list `row` indexes; "machine" for the machine itself. */
  list: "machine" | "states" | "events" | "rows";
  /** Index into `list`, or null for the machine as a whole. */
  row: number | null;
  field: MachineField;
  severity: "error" | "warning";
  message: string;
  fix?: MachineFix;
}

export type MachineField = "id" | "name" | "initial" | "state" | "event" | "guard" | "next" | "actions" | "entry" | "size";

export type MachineFix =
  | { kind: "add-state"; name: string }
  /** `machine`: the index of the machine to add it to, when it is not
   * the issue's own (a raise of another machine's event). */
  | { kind: "add-event"; name: string; machine?: number }
  | { kind: "add-pin"; label: string; func: PinFunc }
  | { kind: "set-initial"; name: string };

/** protocol-wasm `checkMachines`: the machines and the pin table's rows
 * (label and function are what it reads). */
export type MachineChecker = (machines: readonly Machine[], pins: readonly { label: string; func: PinFunc }[]) => MachineIssue[];

export function hasErrors(issues: readonly MachineIssue[]): boolean {
  return issues.some((i) => i.severity === "error");
}

// ---- shape (parse)

type Obj = Record<string, unknown>;
function isObj(v: unknown): v is Obj {
  return v !== null && typeof v === "object" && !Array.isArray(v);
}
function extraKey(o: Obj, allowed: readonly string[]): string | undefined {
  return Object.keys(o).find((k) => !allowed.includes(k));
}
function text(v: unknown): v is string {
  return typeof v === "string" && v.length <= MAX_NAME_TEXT;
}
function isI32(v: unknown): v is number {
  return typeof v === "number" && Number.isInteger(v) && v >= -2147483648 && v <= 2147483647;
}

/** The problem with an expression, or null. `depth` is the node's depth. */
export function exprProblem(v: unknown, at: string, depth = 1): string | null {
  if (depth > MAX_EXPR_DEPTH) return `${at} is nested deeper than ${MAX_EXPR_DEPTH} levels`;
  if (!isObj(v)) return `${at} must be an object`;
  const keys: Record<string, readonly string[]> = {
    pin: ["kind", "label"],
    const: ["kind", "value"],
    state: ["kind", "machine", "state"],
    cmp: ["kind", "op", "a", "b"],
    and: ["kind", "a", "b"],
    or: ["kind", "a", "b"],
    not: ["kind", "a"],
  };
  const allowed = typeof v.kind === "string" ? keys[v.kind] : undefined;
  if (!allowed) return `${at}.kind must be one of ${Object.keys(keys).join(", ")}`;
  const extra = extraKey(v, allowed);
  if (extra !== undefined) return `${at} has an unknown field "${extra}"`;
  switch (v.kind) {
    case "pin":
      return text(v.label) ? null : `${at}.label must be text`;
    case "const":
      return isI32(v.value) ? null : `${at}.value must be a whole 32-bit number`;
    case "state":
      return text(v.machine) && text(v.state) ? null : `${at} needs text machine and state`;
    case "cmp":
      if (!(CMP_OPS as readonly unknown[]).includes(v.op)) return `${at}.op must be one of ${CMP_OPS.join(", ")}`;
      return exprProblem(v.a, `${at}.a`, depth + 1) ?? exprProblem(v.b, `${at}.b`, depth + 1);
    case "and":
    case "or":
      return exprProblem(v.a, `${at}.a`, depth + 1) ?? exprProblem(v.b, `${at}.b`, depth + 1);
    default:
      return exprProblem(v.a, `${at}.a`, depth + 1);
  }
}

export function actionProblem(v: unknown, at: string): string | null {
  if (!isObj(v)) return `${at} must be an object`;
  const keys: Record<string, readonly string[]> = {
    write: ["kind", "label", "level"],
    toggle: ["kind", "label"],
    publish: ["kind", "key", "text"],
    log: ["kind", "text"],
    raise: ["kind", "event", "machine"],
  };
  const allowed = typeof v.kind === "string" ? keys[v.kind] : undefined;
  if (!allowed) return `${at}.kind must be one of ${ACTION_KINDS.join(", ")}`;
  const extra = extraKey(v, allowed);
  if (extra !== undefined) return `${at} has an unknown field "${extra}"`;
  for (const k of allowed) {
    if (k === "kind") continue;
    // A raise's machine is optional (absent: the table's own machine).
    if (k === "machine" && v.machine === undefined) continue;
    if (k === "level") {
      if (v.level !== 0 && v.level !== 1) return `${at}.level must be 0 or 1`;
    } else if (k === "text") {
      // Message text may be as long as a message (512 bytes, checked in Rust).
      if (typeof v.text !== "string" || v.text.length > 512) return `${at}.text must be text (at most 512 characters)`;
    } else if (!text(v[k])) return `${at}.${k} must be text`;
  }
  return null;
}

function actionsProblem(v: unknown, at: string): string | null {
  if (!Array.isArray(v) || v.length > MAX_ACTIONS) return `${at} must be a list of at most ${MAX_ACTIONS} actions`;
  for (const [i, a] of v.entries()) {
    const p = actionProblem(a, `${at}[${i}]`);
    if (p) return p;
  }
  return null;
}

function machineProblem(v: unknown, at: string): string | null {
  if (!isObj(v)) return `${at} must be an object`;
  const extra = extraKey(v, ["id", "name", "initial", "states", "events", "rows"]);
  if (extra !== undefined) return `${at} has an unknown field "${extra}"`;
  for (const k of ["id", "name", "initial"] as const) if (!text(v[k])) return `${at}.${k} must be text`;
  if (!Array.isArray(v.states) || v.states.length > PARSE_MAX_NAMED) return `${at}.states must be a list of at most ${PARSE_MAX_NAMED} states`;
  for (const [i, s] of v.states.entries()) {
    const sat = `${at}.states[${i}]`;
    if (!isObj(s)) return `${sat} must be an object`;
    const e = extraKey(s, ["name", "note", "entry"]);
    if (e !== undefined) return `${sat} has an unknown field "${e}"`;
    if (!text(s.name) || !text(s.note)) return `${sat} needs text name and note`;
    const p = actionsProblem(s.entry, `${sat}.entry`);
    if (p) return p;
  }
  if (!Array.isArray(v.events) || v.events.length > PARSE_MAX_NAMED) return `${at}.events must be a list of at most ${PARSE_MAX_NAMED} events`;
  for (const [i, s] of v.events.entries()) {
    const eat = `${at}.events[${i}]`;
    if (!isObj(s)) return `${eat} must be an object`;
    const e = extraKey(s, ["name", "note"]);
    if (e !== undefined) return `${eat} has an unknown field "${e}"`;
    if (!text(s.name) || !text(s.note)) return `${eat} needs text name and note`;
  }
  if (!Array.isArray(v.rows) || v.rows.length > PARSE_MAX_ROWS) return `${at}.rows must be a list of at most ${PARSE_MAX_ROWS} rows`;
  for (const [i, r] of v.rows.entries()) {
    const rat = `${at}.rows[${i}]`;
    if (!isObj(r)) return `${rat} must be an object`;
    const e = extraKey(r, ["state", "event", "guard", "next", "actions"]);
    if (e !== undefined) return `${rat} has an unknown field "${e}"`;
    for (const k of ["state", "event", "next"] as const) if (!text(r[k])) return `${rat}.${k} must be text`;
    if (r.guard !== null) {
      const p = exprProblem(r.guard, `${rat}.guard`);
      if (p) return p;
    }
    const p = actionsProblem(r.actions, `${rat}.actions`);
    if (p) return p;
  }
  return null;
}

/** Checks the `machines` list's shape; returns the problem, or null. */
export function machinesProblem(v: unknown): string | null {
  if (!Array.isArray(v) || v.length > MAX_MACHINES) return `machines must be a list of at most ${MAX_MACHINES} state machines`;
  for (const [i, m] of v.entries()) {
    const p = machineProblem(m, `machines[${i}]`);
    if (p) return p;
  }
  return null;
}

/** A deep copy (shape already checked), so edits never alias the input. */
export function copyMachines(ms: readonly Machine[]): Machine[] {
  return structuredClone(ms) as Machine[];
}

// ---- constructors and transitions

export function state(name: string, entry: readonly ActionRef[] = [], note = ""): MachineState {
  return { name, note, entry };
}
export function event(name: string, note = ""): MachineEvent {
  return { name, note };
}
export function transition(st: string, ev: string, next: string, actions: readonly ActionRef[] = [], guard: Expr | null = null): TransitionRow {
  return { state: st, event: ev, guard, next, actions };
}

/** `base`, `base2`, `base3`, ...: the first name not in `taken` (case
 * ignored, as the check compares them). */
export function freeName(taken: readonly string[], base: string): string {
  const t = new Set(taken.map((n) => n.toLowerCase()));
  if (!t.has(base.toLowerCase())) return base;
  for (let i = 2; i < 100; i++) if (!t.has(`${base}${i}`.toLowerCase())) return `${base}${i}`;
  return `${base}_${taken.length}`;
}

/** A new two-state machine with an id no other machine has. */
export function newMachine(existing: readonly Machine[]): Machine {
  const id = freeName(
    existing.map((m) => m.id),
    "Machine",
  );
  return {
    id,
    name: "",
    initial: "Idle",
    states: [state("Idle"), state("Busy")],
    events: [event("GO"), event("DONE")],
    rows: [transition("Idle", "GO", "Busy"), transition("Busy", "DONE", "Idle")],
  };
}

export function addMachine(ms: readonly Machine[], m: Machine): Machine[] {
  return ms.length >= MAX_MACHINES ? [...ms] : [...ms, m];
}
export function setMachine(ms: readonly Machine[], i: number, m: Machine): Machine[] {
  return ms.map((x, j) => (j === i ? m : x));
}
export function removeMachine(ms: readonly Machine[], i: number): Machine[] {
  return ms.filter((_, j) => j !== i);
}

export function setMeta(m: Machine, patch: Partial<Pick<Machine, "id" | "name" | "initial">>): Machine {
  return { ...m, ...patch };
}

export function addState(m: Machine, s: MachineState = state(freeName(m.states.map((x) => x.name), "State"))): Machine {
  return { ...m, states: [...m.states, s] };
}

/** Renames state `i` and every reference to it (rows, initial). */
export function renameState(m: Machine, i: number, name: string): Machine {
  const old = m.states[i]?.name;
  if (old === undefined) return m;
  const ren = (n: string) => (n === old ? name : n);
  return {
    ...m,
    initial: ren(m.initial),
    states: m.states.map((s, j) => (j === i ? { ...s, name } : s)),
    rows: m.rows.map((r) => ({ ...r, state: ren(r.state), next: ren(r.next) })),
  };
}

export function setStateNote(m: Machine, i: number, note: string): Machine {
  return { ...m, states: m.states.map((s, j) => (j === i ? { ...s, note } : s)) };
}
export function setEntry(m: Machine, i: number, entry: readonly ActionRef[]): Machine {
  return { ...m, states: m.states.map((s, j) => (j === i ? { ...s, entry: entry.slice(0, MAX_ACTIONS) } : s)) };
}

/** Removes state `i` and the rows from or to it; the initial state moves
 * to the first remaining state if it was this one. */
export function removeState(m: Machine, i: number): Machine {
  const old = m.states[i]?.name;
  if (old === undefined) return m;
  const states = m.states.filter((_, j) => j !== i);
  return {
    ...m,
    initial: m.initial === old ? (states[0]?.name ?? "") : m.initial,
    states,
    rows: m.rows.filter((r) => r.state !== old && r.next !== old),
  };
}

export function addEvent(m: Machine, e: MachineEvent = event(freeName(m.events.map((x) => x.name), "EVENT"))): Machine {
  return { ...m, events: [...m.events, e] };
}

/** Whether `a` raises machine `id`'s event `ev` (a raise without a
 * machine names the table's own, `own`). */
export function raisesEvent(a: ActionRef, own: string, id: string, ev: string): boolean {
  return a.kind === "raise" && a.event === ev && (a.machine ?? own) === id;
}

function renameRaise(actions: readonly ActionRef[], own: string, old: string, name: string): ActionRef[] {
  return actions.map((a) => (raisesEvent(a, own, own, old) ? { ...a, event: name } : a));
}

/** Renames event `i` and every reference to it (rows, raise actions). */
export function renameEvent(m: Machine, i: number, name: string): Machine {
  const old = m.events[i]?.name;
  if (old === undefined) return m;
  return {
    ...m,
    events: m.events.map((e, j) => (j === i ? { ...e, name } : e)),
    states: m.states.map((s) => ({ ...s, entry: renameRaise(s.entry, m.id, old, name) })),
    rows: m.rows.map((r) => ({ ...r, event: r.event === old ? name : r.event, actions: renameRaise(r.actions, m.id, old, name) })),
  };
}

export function setEventNote(m: Machine, i: number, note: string): Machine {
  return { ...m, events: m.events.map((e, j) => (j === i ? { ...e, note } : e)) };
}

/** Removes event `i`, its rows and the actions raising it. */
export function removeEvent(m: Machine, i: number): Machine {
  const old = m.events[i]?.name;
  if (old === undefined) return m;
  const keep = (as: readonly ActionRef[]) => as.filter((a) => !raisesEvent(a, m.id, m.id, old));
  return {
    ...m,
    events: m.events.filter((_, j) => j !== i),
    states: m.states.map((s) => ({ ...s, entry: keep(s.entry) })),
    rows: m.rows.filter((r) => r.event !== old).map((r) => ({ ...r, actions: keep(r.actions) })),
  };
}

export function addRow(m: Machine, r: TransitionRow): Machine {
  return { ...m, rows: [...m.rows, r] };
}
export function setRow(m: Machine, i: number, patch: Partial<TransitionRow>): Machine {
  return { ...m, rows: m.rows.map((r, j) => (j === i ? { ...r, ...patch, actions: (patch.actions ?? r.actions).slice(0, MAX_ACTIONS) } : r)) };
}
export function removeRow(m: Machine, i: number): Machine {
  return { ...m, rows: m.rows.filter((_, j) => j !== i) };
}

/** Rows for the (state, event) cell, with their indices. */
export function cellRows(m: Machine, st: string, ev: string): { i: number; row: TransitionRow }[] {
  return m.rows.flatMap((row, i) => (row.state === st && row.event === ev ? [{ i, row }] : []));
}

// ---- guards: the structured editor's form

/** The form the guard editor edits: up to three "LABEL op value" terms
 * joined by all-of (and) or any-of (or). Any `Expr` the editor did not
 * write is shown as text (`exprText`) and can be replaced. */
export interface SimpleGuard {
  join: "and" | "or";
  terms: readonly { label: string; op: CmpOp; value: number }[];
}
export const MAX_SIMPLE_TERMS = 3;

export function guardFromSimple(g: SimpleGuard): Expr | null {
  const terms = g.terms.slice(0, MAX_SIMPLE_TERMS).map(
    (t): Expr => ({ kind: "cmp", op: t.op, a: { kind: "pin", label: t.label }, b: { kind: "const", value: t.value } }),
  );
  const [first, ...rest] = terms;
  if (!first) return null;
  // Left-nested: ((t0 op t1) op t2), depth <= 4.
  return rest.reduce((acc, t): Expr => ({ kind: g.join, a: acc, b: t }), first);
}

/** The simple form of `e`, or null when `e` is not one the editor writes. */
export function simpleGuard(e: Expr | null): SimpleGuard | null {
  if (e === null) return { join: "and", terms: [] };
  const term = (x: Expr) => (x.kind === "cmp" && x.a.kind === "pin" && x.b.kind === "const" ? { label: x.a.label, op: x.op, value: x.b.value } : null);
  const t = term(e);
  if (t) return { join: "and", terms: [t] };
  if (e.kind !== "and" && e.kind !== "or") return null;
  const join = e.kind;
  const out: { label: string; op: CmpOp; value: number }[] = [];
  let cur: Expr = e;
  for (let k = 0; k < MAX_SIMPLE_TERMS; k++) {
    if (cur.kind === join) {
      const right = term(cur.b);
      if (!right) return null;
      out.unshift(right);
      cur = cur.a;
    } else {
      const left = term(cur);
      if (!left) return null;
      out.unshift(left);
      return out.length <= MAX_SIMPLE_TERMS ? { join, terms: out } : null;
    }
  }
  return null;
}

/** Plain text of an expression: `BUTTON = 0 and DOOR = 1`. Bounded by
 * the shape's depth limit. */
export function exprText(e: Expr): string {
  switch (e.kind) {
    case "pin":
      return e.label;
    case "const":
      return String(e.value);
    case "state":
      return `${e.machine} is ${e.state}`;
    case "cmp":
      return `${exprText(e.a)} ${CMP_TEXT[e.op]} ${exprText(e.b)}`;
    case "and":
    case "or": {
      const side = (x: Expr) => (x.kind === "and" || x.kind === "or") && x.kind !== e.kind ? `(${exprText(x)})` : exprText(x);
      return `${side(e.a)} ${e.kind} ${side(e.b)}`;
    }
    case "not":
      return `not (${exprText(e.a)})`;
  }
}

export function actionText(a: ActionRef): string {
  switch (a.kind) {
    case "write":
      return `write ${a.label} ${a.level ? "high" : "low"}`;
    case "toggle":
      return `toggle ${a.label}`;
    case "publish":
      return `publish ${a.key} "${a.text}"`;
    case "log":
      return `log "${a.text}"`;
    case "raise":
      return a.machine === undefined ? `raise ${a.event}` : `raise ${a.machine}.${a.event}`;
  }
}

/** Pin labels a machine names, with the function each needs (an output for
 * write/toggle; a digital read, satisfied by an input, for guards). */
export function machinePinNeeds(m: Machine): { label: string; func: PinFunc }[] {
  const out: { label: string; func: PinFunc }[] = [];
  const add = (label: string, func: PinFunc) => {
    if (!out.some((n) => n.label === label)) out.push({ label, func });
  };
  const acts = (as: readonly ActionRef[]) => {
    for (const a of as) if (a.kind === "write" || a.kind === "toggle") add(a.label, "gpio-out");
  };
  const expr = (e: Expr, depth: number): void => {
    if (depth > MAX_EXPR_DEPTH) return;
    if (e.kind === "pin") add(e.label, "gpio-in");
    else if (e.kind === "cmp" || e.kind === "and" || e.kind === "or") {
      expr(e.a, depth + 1);
      expr(e.b, depth + 1);
    } else if (e.kind === "not") expr(e.a, depth + 1);
  };
  for (const s of m.states) acts(s.entry);
  for (const r of m.rows) {
    acts(r.actions);
    if (r.guard) expr(r.guard, 1);
  }
  return out;
}

// ---- blocks referring to machines

/** Blocks with an `EVENT` field naming `<machine id>.<event>`. */
export const MACHINE_EVENT_BLOCKS: readonly string[] = ["mw_sm_raise"];
/** Blocks with a `STATE` field naming `<machine id>.<state>`. */
export const MACHINE_STATE_BLOCKS: readonly string[] = ["mw_sm_in_state", "mw_sm_on_enter"];

export function machineRef(id: string, name: string): string {
  return `${id}.${name}`;
}
export function parseMachineRef(v: string): { id: string; name: string } | null {
  const dot = v.indexOf(".");
  return dot <= 0 || dot === v.length - 1 ? null : { id: v.slice(0, dot), name: v.slice(dot + 1) };
}

function refField(type: string): "EVENT" | "STATE" | null {
  return MACHINE_EVENT_BLOCKS.includes(type) ? "EVENT" : MACHINE_STATE_BLOCKS.includes(type) ? "STATE" : null;
}

/** Machine references the workspace's blocks make, sorted, by field. */
export function machineRefsUsed(ws: WorkspaceJson): { events: string[]; states: string[] } {
  const events = new Set<string>();
  const states = new Set<string>();
  for (const b of everyBlock(ws)) {
    const f = refField(b.type);
    const v = f ? b.fields?.[f] : undefined;
    if (typeof v !== "string" || v === "") continue;
    (f === "EVENT" ? events : states).add(v);
  }
  return { events: [...events].sort(), states: [...states].sort() };
}

/** The workspace with references to `<id>.<from>` (a state or an event)
 * renamed to `<id>.<to>`, or, for kind "machine", every reference to
 * machine `from` moved to machine `to`. A deep copy. */
export function renameMachineRef(ws: WorkspaceJson, kind: "state" | "event" | "machine", id: string, from: string, to: string): WorkspaceJson {
  const copy = structuredClone(ws);
  for (const b of everyBlock(copy)) {
    const f = refField(b.type);
    const v = f ? b.fields?.[f] : undefined;
    if (!f || typeof v !== "string" || !b.fields) continue;
    const ref = parseMachineRef(v);
    if (!ref) continue;
    if (kind === "machine") {
      if (ref.id === from) b.fields[f] = machineRef(to, ref.name);
    } else if ((kind === "event") === (f === "EVENT") && ref.id === id && ref.name === from) {
      b.fields[f] = machineRef(id, to);
    }
  }
  return copy;
}

// ---- the simulator's view

/** The log line the generated code writes in init for each machine
 * (`sm Mode: Off`) and on each change of state (`sm Mode: Off -> On on
 * PRESS`); self-transitions are not logged. */
export function machineLogInit(id: string, st: string): string {
  return `sm ${id}: ${st}`;
}
export function machineLogMove(id: string, from: string, to: string, ev: string): string {
  return `sm ${id}: ${from} -> ${to} on ${ev}`;
}

const LOG_RE = /^sm ([A-Za-z][A-Za-z0-9_]*): (?:([A-Za-z][A-Za-z0-9_]*) -> )?([A-Za-z][A-Za-z0-9_]*)(?: on ([A-Za-z][A-Za-z0-9_]*))?$/;

/** What a machine log line says: the machine and the state it is now in
 * (with the transition, for a move); null for any other line. */
export function parseMachineLog(line: string): { id: string; state: string; from?: string; event?: string } | null {
  const m = LOG_RE.exec(line);
  if (!m) return null;
  const [, id, from, to, ev] = m;
  if (!id || !to || (from === undefined) !== (ev === undefined)) return null;
  return from === undefined ? { id, state: to } : { id, state: to, from, event: ev };
}

/** The current state of each machine after these log lines, by id. */
export function machineStates(lines: readonly string[]): Map<string, string> {
  const out = new Map<string, string>();
  for (const l of lines) {
    const p = parseMachineLog(l);
    if (p) out.set(p.id, p.state);
  }
  return out;
}
