// The States view's decisions, pure (statesTable.ts only renders them and
// forwards events): what a typed name does to the table and to the blocks,
// which options the dropdowns offer, the defaults for a new action, guard
// term or transition, the matrix overview, how issues map onto rows and
// fields, what a fix button does, the simulator's state line, and a pin
// label renamed in the pin table followed into the machines.
//
// Names and references. Typing a state, event or machine name changes it
// on every keystroke, but references (the table's own rows, initial state
// and raise actions, through machines.ts `renameState` / `renameEvent`, and
// the blocks, through the `rename` the page applies) follow only when the
// new name is a valid identifier no other state / event / machine has. So
// typing "Idle" over "Busy" while "Idle" exists never merges Busy's rows
// into Idle's: the references stay on the last name they were given
// (`applied`) until the name is valid and free again.

import {
  type ActionKind,
  type ActionRef,
  type Expr,
  MAX_ACTIONS,
  MAX_EXPR_DEPTH,
  MAX_SIMPLE_TERMS,
  type Machine,
  type MachineFix,
  type MachineIssue,
  type SimpleGuard,
  type TransitionRow,
  addEvent,
  addState,
  cellRows,
  event,
  renameEvent,
  renameState,
  setMeta,
  state,
  transition,
} from "../model/machines";
import { FUNC_LABELS, type PinFunc } from "../model/pins";

/** A pin as the States view needs it: label and function. */
export interface PinRef {
  label: string;
  func: PinFunc;
}

/** Identifiers the generated code and its log lines accept. */
const IDENT_RE = /^[A-Za-z][A-Za-z0-9_]{0,23}$/;
export function isIdent(v: string): boolean {
  return IDENT_RE.test(v);
}

/** True when `v` is a valid identifier none of `names` (other than index
 * `i`) has, case ignored (as the check compares them). */
export function nameFree(names: readonly string[], i: number, v: string): boolean {
  return isIdent(v) && !names.some((n, j) => j !== i && n.toLowerCase() === v.toLowerCase());
}

/** A table rename for blocks: kind, machine id, old and new name. */
export interface Rename {
  kind: "state" | "event" | "machine";
  id: string;
  from: string;
  to: string;
}

function withName(m: Machine, kind: "state" | "event", i: number, name: string): Machine {
  return kind === "state"
    ? { ...m, states: m.states.map((s, j) => (j === i ? { ...s, name } : s)) }
    : { ...m, events: m.events.map((e, j) => (j === i ? { ...e, name } : e)) };
}

/** The machine after typing `v` as the name of state / event `i`, whose
 * references currently name `applied`. References follow (and a rename is
 * returned for the blocks) only when `v` is valid and free. */
export function typeName(
  m: Machine,
  kind: "state" | "event",
  i: number,
  v: string,
  applied: string,
): { machine: Machine; applied: string; rename?: Rename } {
  const names = (kind === "state" ? m.states : m.events).map((x) => x.name);
  if (i < 0 || i >= names.length) return { machine: m, applied };
  if (!nameFree(names, i, v) || v === applied) return { machine: withName(m, kind, i, v), applied };
  // Put the name back to the one references use, then rename it and them.
  const back = withName(m, kind, i, applied);
  const machine = kind === "state" ? renameState(back, i, v) : renameEvent(back, i, v);
  return { machine, applied: v, rename: applied ? { kind, id: m.id, from: applied, to: v } : undefined };
}

/** The machines after typing `v` as machine `i`'s id; blocks follow (a
 * rename is returned) when `v` is valid and no other machine has it. */
export function typeMachineId(ms: readonly Machine[], i: number, v: string, applied: string): { machines: Machine[]; applied: string; rename?: Rename } {
  const m = ms[i];
  if (!m) return { machines: [...ms], applied };
  const machines = ms.map((x, j) => (j === i ? setMeta(x, { id: v }) : x));
  if (!nameFree(ms.map((x) => x.id), i, v) || v === applied) return { machines, applied };
  // Blocks could not name a machine with an empty id: nothing to rename.
  if (!applied) return { machines, applied: v };
  return { machines, applied: v, rename: { kind: "machine", id: applied, from: applied, to: v } };
}

// ---- dropdown options: [value, text]

export type Option = readonly [value: string, text: string];

/** `names` as options, plus `current` marked as missing when it is not
 * one of them (so a broken reference stays visible). */
export function nameOptions(names: readonly string[], current: string, missing: string): Option[] {
  const out: Option[] = names.map((n) => [n, n]);
  if (!names.includes(current)) out.push([current, current ? `${current} (${missing})` : `(choose)`]);
  return out;
}

/** Labels of the pins with one of `funcs`, plus `current` marked when it
 * is not one of them. */
export function labelOptions(pins: readonly PinRef[], funcs: readonly PinFunc[], current: string, missing: string): Option[] {
  const labels = pins.filter((p) => p.label && funcs.includes(p.func)).map((p) => p.label);
  return nameOptions([...new Set(labels)], current, missing);
}

/** Pin functions an action can write or toggle, and a guard can read. */
export const OUTPUT_FUNCS: readonly PinFunc[] = ["gpio-out"];
export const INPUT_FUNCS: readonly PinFunc[] = ["gpio-in"];

function firstLabel(pins: readonly PinRef[], funcs: readonly PinFunc[], fallback: string): string {
  return pins.find((p) => p.label && funcs.includes(p.func))?.label ?? fallback;
}

export const ACTION_KIND_TEXT: Readonly<Record<ActionKind, string>> = {
  write: "write pin",
  toggle: "toggle pin",
  publish: "publish",
  log: "log",
  raise: "raise event",
};

/** A new action of `kind`, keeping what `prev` had that still applies (a
 * pin label, a text). */
export function actionOfKind(kind: ActionKind, m: Machine, pins: readonly PinRef[], prev?: ActionRef): ActionRef {
  const label = prev && (prev.kind === "write" || prev.kind === "toggle") ? prev.label : firstLabel(pins, OUTPUT_FUNCS, "LED");
  const txt = prev && (prev.kind === "publish" || prev.kind === "log") ? prev.text : "";
  switch (kind) {
    case "write":
      return { kind, label, level: prev?.kind === "write" ? prev.level : 1 };
    case "toggle":
      return { kind, label };
    case "publish":
      return { kind, key: prev?.kind === "publish" ? prev.key : `app/${m.id.toLowerCase() || "state"}`, text: txt };
    case "log":
      return { kind, text: txt };
    case "raise":
      return { kind, event: prev?.kind === "raise" ? prev.event : (m.events[0]?.name ?? "") };
  }
}

export function addAction(as: readonly ActionRef[], m: Machine, pins: readonly PinRef[]): ActionRef[] {
  return as.length >= MAX_ACTIONS ? [...as] : [...as, actionOfKind(firstLabel(pins, OUTPUT_FUNCS, "") ? "toggle" : "log", m, pins)];
}
export function setAction(as: readonly ActionRef[], k: number, a: ActionRef): ActionRef[] {
  return as.map((x, j) => (j === k ? a : x));
}
export function removeAction(as: readonly ActionRef[], k: number): ActionRef[] {
  return as.filter((_, j) => j !== k);
}

// ---- guard terms

export type Term = SimpleGuard["terms"][number];

export function addTerm(g: SimpleGuard, pins: readonly PinRef[]): SimpleGuard {
  if (g.terms.length >= MAX_SIMPLE_TERMS) return g;
  return { ...g, terms: [...g.terms, { label: firstLabel(pins, INPUT_FUNCS, "BUTTON"), op: "eq", value: 1 }] };
}
export function setTerm(g: SimpleGuard, k: number, patch: Partial<Term>): SimpleGuard {
  return { ...g, terms: g.terms.map((t, j) => (j === k ? { ...t, ...patch } : t)) };
}
export function removeTerm(g: SimpleGuard, k: number): SimpleGuard {
  return { ...g, terms: g.terms.filter((_, j) => j !== k) };
}
/** A typed term value: a whole 32-bit number, else null (keep the old). */
export function termValue(text: string): number | null {
  const n = Number(text.trim());
  return text.trim() !== "" && Number.isInteger(n) && n >= -2147483648 && n <= 2147483647 ? n : null;
}

// ---- transitions and the matrix

/** A new row: the first (state, event) cell with no row, as a
 * self-transition (which, like an empty cell, changes nothing until it is
 * edited); null when there are no states or no events. */
export function newTransition(m: Machine): TransitionRow | null {
  const s0 = m.states[0];
  const e0 = m.events[0];
  if (!s0 || !e0) return null;
  for (const s of m.states) for (const e of m.events) if (cellRows(m, s.name, e.name).length === 0) return transition(s.name, e.name, s.name);
  return transition(s0.name, e0.name, s0.name);
}

export interface MatrixCell {
  state: string;
  event: string;
  /** The rows of the cell: row index, next state and whether guarded. */
  rows: { i: number; next: string; guarded: boolean }[];
}

/** The state x event overview: one line per state, one cell per event. */
export function matrix(m: Machine): MatrixCell[][] {
  return m.states.map((s) =>
    m.events.map((e) => ({
      state: s.name,
      event: e.name,
      rows: cellRows(m, s.name, e.name).map(({ i, row }) => ({ i, next: row.next, guarded: row.guard !== null })),
    })),
  );
}

/** What a matrix cell shows, and its accessible label. */
export function cellText(c: MatrixCell): { text: string; label: string } {
  if (c.rows.length === 0) return { text: "—", label: `${c.state} on ${c.event}: no transition. Add one` };
  const parts = c.rows.map((r) => `${r.next}${r.guarded ? " (if)" : ""}`);
  const said = c.rows.map((r) => `row ${r.i + 1} to ${r.next}${r.guarded ? ", guarded" : ""}`);
  return { text: parts.join(", "), label: `${c.state} on ${c.event}: ${said.join("; ")}. Go to row ${(c.rows[0]?.i ?? 0) + 1}` };
}

// ---- issues

export type IssueList = MachineIssue["list"];

/** The issues of machine `mi` on `list` row `row` (null: the list as a
 * whole, or the machine itself for "machine"). */
export function issuesAt(issues: readonly MachineIssue[], mi: number, list: IssueList, row: number | null): MachineIssue[] {
  return issues.filter((x) => x.machine === mi && x.list === list && x.row === row);
}

/** The worst severity among `issues` on one of `fields`, or null. */
export function fieldSeverity(issues: readonly MachineIssue[], fields: readonly MachineIssue["field"][]): "error" | "warning" | null {
  const on = issues.filter((x) => fields.includes(x.field));
  return on.some((x) => x.severity === "error") ? "error" : on.length > 0 ? "warning" : null;
}

/** Errors (warnings do not block compiling). */
export function errorCount(issues: readonly MachineIssue[], mi?: number): number {
  return issues.filter((x) => x.severity === "error" && (mi === undefined || x.machine === mi)).length;
}

/** The States tab's text: "States", or "States (2 to fix)". */
export function statesTabText(issues: readonly MachineIssue[]): string {
  const n = errorCount(issues);
  return n ? `States (${n} to fix)` : "States";
}

export function fixText(f: MachineFix): string {
  switch (f.kind) {
    case "add-state":
      return `Add state ${f.name}`;
    case "add-event":
      return `Add event ${f.name}`;
    case "set-initial":
      return `Start in ${f.name}`;
    case "add-pin":
      return `Add pin ${f.label} (${FUNC_LABELS[f.func]})`;
  }
}

/** A fix applied to machine `m`, or the pin to add to the pin table. */
export function applyMachineFix(m: Machine, f: MachineFix): { machine: Machine } | { pin: PinRef } {
  switch (f.kind) {
    case "add-state":
      return { machine: addState(m, state(f.name)) };
    case "add-event":
      return { machine: addEvent(m, event(f.name)) };
    case "set-initial":
      return { machine: setMeta(m, { initial: f.name }) };
    case "add-pin":
      return { pin: { label: f.label, func: f.func } };
  }
}

// ---- simulator

/** The simulator's line for a parsed machine log line:
 * "State: Mode = On (from Off on PRESS)". */
export function simStateText(p: { id: string; state: string; from?: string; event?: string }): string {
  return `State: ${p.id} = ${p.state}${p.from !== undefined ? ` (from ${p.from} on ${p.event})` : ""}`;
}

// ---- pin labels

export function renameInExpr(e: Expr, from: string, to: string, depth: number): Expr {
  if (depth > MAX_EXPR_DEPTH) return e;
  switch (e.kind) {
    case "pin":
      return e.label === from ? { ...e, label: to } : e;
    case "const":
    case "state":
      return e;
    case "not":
      return { ...e, a: renameInExpr(e.a, from, to, depth + 1) };
    default:
      return { ...e, a: renameInExpr(e.a, from, to, depth + 1), b: renameInExpr(e.b, from, to, depth + 1) };
  }
}
export function renameInActions(as: readonly ActionRef[], from: string, to: string): ActionRef[] {
  return as.map((a) => ((a.kind === "write" || a.kind === "toggle") && a.label === from ? { ...a, label: to } : a));
}

/** The machines with pin label `from` renamed to `to` in guards and
 * write / toggle actions (the pin table renamed a label). */
export function renamePinLabel(ms: readonly Machine[], from: string, to: string): Machine[] {
  return ms.map((m) => ({
    ...m,
    states: m.states.map((s) => ({ ...s, entry: renameInActions(s.entry, from, to) })),
    rows: m.rows.map((r) => ({ ...r, guard: r.guard ? renameInExpr(r.guard, from, to, 1) : null, actions: renameInActions(r.actions, from, to) })),
  }));
}
