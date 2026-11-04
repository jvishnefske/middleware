// The Flows view's decisions, pure (flowsView.ts only renders them and
// forwards events): canvas geometry (node boxes, port positions, wire
// curves, the canvas size, zoom steps), what a node says about itself,
// new entries / actions / conditions keeping what still applies, the
// outline's menus (which nodes a port may lead to) and reordering a chain,
// the undo history, how issues map onto nodes and wires, the tab text,
// and what a fix does.

import { type Edge, type EntryOn, type EntryProps, type Flow, type FlowFix, type FlowIssue, type FlowNode, IN_PORT, MAX_SEQUENCE, connect, entryText, hasInput, outputs, wireFrom, wouldCycle } from "../model/flows";
import { type ActionKind, type ActionRef, type Expr, type Machine, actionText, addEvent, event, exprText, machineRef, setMachine } from "../model/machines";
import { type PinRef, INPUT_FUNCS, OUTPUT_FUNCS } from "./statesModel";

// ---- geometry (canvas units; the SVG scales them by the zoom)

export const NODE_W = 176;
export const NODE_H = 66;
export const PORT_R = 7;
/** Space kept around the drawing so a node can be dragged past the edge. */
export const CANVAS_PAD = 160;
export const ZOOMS = [0.5, 0.75, 1, 1.25, 1.5] as const;

export interface Point {
  x: number;
  y: number;
}

/** Where a port sits: the input at the top middle, outputs spread along
 * the bottom edge in run order. */
export function portPos(n: FlowNode, port: string): Point {
  if (port === IN_PORT) return { x: n.x + NODE_W / 2, y: n.y };
  const outs = outputs(n);
  const i = Math.max(0, outs.indexOf(port));
  return { x: n.x + (NODE_W * (i + 1)) / (outs.length + 1), y: n.y + NODE_H };
}

/** A wire's curve: down out of the port, into the input from above (a
 * target level with or above the port gets a wider loop). */
export function wirePath(a: Point, b: Point): string {
  const dy = b.y > a.y + 40 ? Math.max(30, (b.y - a.y) / 2) : 70 + (a.y - b.y) / 2;
  return `M ${a.x} ${a.y} C ${a.x} ${a.y + dy} ${b.x} ${b.y - dy} ${b.x} ${b.y}`;
}

/** The drawing's size: every node plus the padding. */
export function canvasSize(f: Flow): { w: number; h: number } {
  let w = 0;
  let h = 0;
  for (const n of f.nodes) {
    w = Math.max(w, n.x + NODE_W);
    h = Math.max(h, n.y + NODE_H);
  }
  return { w: Math.max(480, w + CANVAS_PAD), h: Math.max(320, h + CANVAS_PAD) };
}

/** The next zoom step in direction `dir` (-1, +1), or 1 for 0. */
export function zoomStep(z: number, dir: -1 | 0 | 1): number {
  if (dir === 0) return 1;
  const i = ZOOMS.findIndex((x) => x >= z - 1e-9);
  const at = i < 0 ? ZOOMS.length - 1 : i;
  return ZOOMS[Math.max(0, Math.min(ZOOMS.length - 1, at + dir))] ?? 1;
}

/** A free spot for a new node: below the lowest node, in the left column. */
export function newNodeSpot(f: Flow): Point {
  const bottom = f.nodes.reduce((y, n) => Math.max(y, n.y + NODE_H), 0);
  return { x: 40, y: f.nodes.length === 0 ? 40 : bottom + 64 };
}

// ---- what a node says

export const KIND_TEXT: Readonly<Record<FlowNode["kind"], string>> = {
  entry: "Entry",
  action: "Action",
  branch: "Branch",
  repeat: "Repeat",
  sequence: "Sequence",
};

export const ENTRY_TEXT: Readonly<Record<EntryOn, string>> = {
  init: "on start",
  every: "every N ms",
  message: "on message",
  "pin-edge": "on pin edge",
  entered: "when a state is entered",
};

/** One line saying what the node does. */
export function nodeTitle(n: FlowNode): string {
  switch (n.kind) {
    case "entry":
      return entryText(n.props);
    case "action":
      return actionText(n.props);
    case "branch":
      return `if ${exprText(n.props.cond)}`;
    case "repeat":
      return `repeat ${n.props.times} times`;
    case "sequence":
      return `${n.props.steps} steps in order`;
  }
}

/** The node's accessible name: id, kind and what it does. */
export function nodeLabel(n: FlowNode): string {
  return `${n.id}, ${KIND_TEXT[n.kind].toLowerCase()}: ${nodeTitle(n)}`;
}

// ---- new properties, keeping what still applies

function firstLabel(pins: readonly PinRef[], funcs: readonly string[], fallback: string): string {
  return pins.find((p) => p.label && funcs.includes(p.func))?.label ?? fallback;
}

/** The `<machine>.<state>` references the machines offer. */
export function stateRefs(ms: readonly Machine[]): string[] {
  return ms.flatMap((m) => m.states.filter((s) => s.name).map((s) => machineRef(m.id, s.name)));
}

export function entryOf(on: EntryOn, prev: EntryProps, pins: readonly PinRef[], ms: readonly Machine[]): EntryProps {
  switch (on) {
    case "init":
      return { on };
    case "every":
      return { on, ms: prev.on === "every" ? prev.ms : 500 };
    case "message":
      return { on, key: prev.on === "message" ? prev.key : "app/in" };
    case "pin-edge":
      return { on, label: prev.on === "pin-edge" ? prev.label : firstLabel(pins, INPUT_FUNCS, "BUTTON"), edge: prev.on === "pin-edge" ? prev.edge : ("fall" satisfies Edge) };
    case "entered":
      return { on, state: prev.on === "entered" ? prev.state : (stateRefs(ms)[0] ?? "") };
  }
}

/** A flow action of `kind`; a raise names a machine (the first). */
export function flowActionOf(kind: ActionKind, prev: ActionRef, pins: readonly PinRef[], ms: readonly Machine[]): ActionRef {
  const label = prev.kind === "write" || prev.kind === "toggle" ? prev.label : firstLabel(pins, OUTPUT_FUNCS, "LED");
  const txt = prev.kind === "publish" || prev.kind === "log" ? prev.text : "";
  switch (kind) {
    case "write":
      return { kind, label, level: prev.kind === "write" ? prev.level : 1 };
    case "toggle":
      return { kind, label };
    case "publish":
      return { kind, key: prev.kind === "publish" ? prev.key : "app/out", text: txt };
    case "log":
      return { kind, text: txt };
    case "raise": {
      const m = (prev.kind === "raise" && ms.find((x) => x.id === prev.machine)) || ms[0];
      return { kind, machine: m?.id ?? "", event: prev.kind === "raise" && prev.machine === m?.id ? prev.event : (m?.events[0]?.name ?? "") };
    }
  }
}

/** The branch editor's forms: a pin compared with a number, a machine
 * being in a state, or (read-only) anything else. */
export type CondForm = { form: "pin"; label: string; op: "eq" | "ne"; value: number } | { form: "state"; ref: string } | { form: "other"; text: string };

export function condForm(e: Expr): CondForm {
  if (e.kind === "cmp" && (e.op === "eq" || e.op === "ne") && e.a.kind === "pin" && e.b.kind === "const") return { form: "pin", label: e.a.label, op: e.op, value: e.b.value };
  if (e.kind === "state") return { form: "state", ref: machineRef(e.machine, e.state) };
  return { form: "other", text: exprText(e) };
}

export function condOf(f: CondForm): Expr {
  switch (f.form) {
    case "pin":
      return { kind: "cmp", op: f.op, a: { kind: "pin", label: f.label }, b: { kind: "const", value: f.value } };
    case "state": {
      const dot = f.ref.indexOf(".");
      return { kind: "state", machine: dot < 0 ? f.ref : f.ref.slice(0, dot), state: dot < 0 ? "" : f.ref.slice(dot + 1) };
    }
    case "other":
      return { kind: "const", value: 0 };
  }
}

/** A new condition of form `form`, keeping what applies. */
export function condOfForm(form: "pin" | "state", prev: CondForm, pins: readonly PinRef[], ms: readonly Machine[]): Expr {
  if (form === "pin") return condOf(prev.form === "pin" ? prev : { form, label: firstLabel(pins, INPUT_FUNCS, "BUTTON"), op: "eq", value: 0 });
  return condOf(prev.form === "state" ? prev : { form, ref: stateRefs(ms)[0] ?? "" });
}

/** Sequence step counts the editor offers. */
export const SEQUENCE_STEPS: readonly number[] = Array.from({ length: MAX_SEQUENCE - 1 }, (_, i) => i + 2);

// ---- the outline's menus and reordering

/** Nodes output `port` of `from` may lead to: not itself, not the entry,
 * not one that would close a loop. */
export function connectTargets(f: Flow, from: string): FlowNode[] {
  return f.nodes.filter((n) => n.id !== from && hasInput(n) && !wouldCycle(f, from, n.id));
}

/** Moves node `id` one place earlier in its chain: if it follows `p`
 * through `p`'s `next`, it now runs before `p` (it takes `p`'s place
 * under `p`'s parent and `p` follows it). Unchanged when that does not
 * apply (no parent through `next`, or the parent is the entry, or the node
 * has no `next`). */
export function moveEarlier(f: Flow, id: string): Flow {
  const into = f.wires.find((w) => w.to.node === id);
  if (!into || into.from.port !== "next") return f;
  const p = f.nodes.find((n) => n.id === into.from.node);
  const x = f.nodes.find((n) => n.id === id);
  if (!p || !x || p.kind === "entry" || !outputs(x).includes("next")) return f;
  const above = f.wires.find((w) => w.to.node === p.id);
  if (!above) return f;
  const after = wireFrom(f, id, "next")?.w.to.node;
  // Unhook p -> x, x -> after and parent -> p, then rewire parent -> x -> p -> after.
  const wires = f.wires.filter((w) => w !== into && w !== above && !(w.from.node === id && w.from.port === "next"));
  let g: Flow = { ...f, wires };
  g = connect(g, above.from.node, above.from.port, id);
  g = connect(g, id, "next", p.id);
  if (after) g = connect(g, p.id, "next", after);
  return g;
}

/** Moves node `id` one place later: its `next` node moves earlier. */
export function moveLater(f: Flow, id: string): Flow {
  const after = wireFrom(f, id, "next")?.w.to.node;
  return after ? moveEarlier(f, after) : f;
}

// ---- undo

/** The flows lists before each change, newest last (at most `UNDO_DEPTH`). */
export const UNDO_DEPTH = 50;
export function record(past: readonly (readonly Flow[])[], before: readonly Flow[]): (readonly Flow[])[] {
  return [...past.slice(-(UNDO_DEPTH - 1)), before];
}

// ---- issues

export function issuesForNode(issues: readonly FlowIssue[], flow: number, node: number): FlowIssue[] {
  return issues.filter((i) => i.flow === flow && i.node === node);
}
export function issuesForWire(issues: readonly FlowIssue[], flow: number, wire: number): FlowIssue[] {
  return issues.filter((i) => i.flow === flow && i.wire === wire);
}
export function severityOf(issues: readonly FlowIssue[]): "error" | "warning" | null {
  return issues.some((i) => i.severity === "error") ? "error" : issues.length > 0 ? "warning" : null;
}

/** "Flows", or "Flows (2 to fix)" counting errors only. */
export function flowsTabText(issues: readonly FlowIssue[]): string {
  const n = issues.filter((i) => i.severity === "error").length;
  return n > 0 ? `Flows (${n} to fix)` : "Flows";
}

export function flowFixText(f: FlowFix, ms: readonly Machine[]): string {
  switch (f.kind) {
    case "add-pin":
      return `Add pin ${f.label}`;
    case "add-event":
      return `Add event ${f.name}${f.machine !== undefined && ms[f.machine] ? ` to ${ms[f.machine]?.id}` : ""}`;
    default:
      return f.kind === "add-state" ? `Add state ${f.name}` : `Make ${f.name} initial`;
  }
}

/** What a fix changes: the machines (an event added) or the pin table. */
export function applyFlowFix(ms: readonly Machine[], f: FlowFix): { machines: Machine[] } | { pin: PinRef } | null {
  if (f.kind === "add-pin") return { pin: { label: f.label, func: f.func } };
  if (f.kind === "add-event" && f.machine !== undefined) {
    const m = ms[f.machine];
    return m ? { machines: setMachine(ms, f.machine, addEvent(m, event(f.name))) } : null;
  }
  return null;
}

/** The live line for the simulator panel: the node each flow last ran. */
export function flowTrailText(trail: ReadonlyMap<string, readonly string[]>): string {
  const parts = [...trail].map(([id, nodes]) => `${id} at ${nodes[nodes.length - 1] ?? "-"}`);
  return parts.length > 0 ? `Flows: ${parts.join(", ")}` : "";
}
