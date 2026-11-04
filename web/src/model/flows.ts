// Flows: small control-flow graphs drawn on the Flows canvas, the
// payload's second kind of logic source (payload model version 3:
// `logic = [blocks, ...flows]`). Pure data, a strict shape check and pure
// transitions (every edit returns a new flow); what is *allowed* in a flow
// (one entry, no cycles, each port wired at most once, every node
// reachable, labels / machines / events / states that exist with the right
// pin function, size limits) is decided by protocol-wasm `checkFlows`
// (Rust, `state-table` crate `flow` module), reached through a
// `FlowChecker`, like the pin table and the machines.
//
// A flow:
//
//   { kind: "flow", id: "Press", name: "BUTTON raises PRESS",
//     nodes: [{ id: "n1", kind: "entry", x, y, props: { on: "pin-edge", label: "BUTTON", edge: "fall" } },
//             { id: "n2", kind: "action", x, y, props: { kind: "raise", machine: "Mode", event: "PRESS" } }],
//     wires: [{ from: { node: "n1", port: "next" }, to: { node: "n2", port: "in" } }] }
//
// Node kinds are a closed set reusing the machines' types (machines.ts):
//
// * entry: `init` | `every` (ms) | `message` (key) | `pin-edge` (label,
//   edge) | `entered` (`<machine>.<state>`, run after the state's entry
//   actions, in the same enter function "when <state> is entered" blocks
//   use);
// * action: exactly an `ActionRef`; a raise names its machine and goes
//   through the same bounded queue as every other raise;
// * branch: an `Expr` (with the `state` leaf); `then`, `else`, then `next`;
// * repeat: `times` (1..MAX_REPEAT); `body` that many times, then `next`;
// * sequence: `steps` (2..MAX_SEQUENCE); ports `1`, `2`, ... in order.
//
// Every output port starts a chain that runs to its end; a repeat's body
// loops by itself, so a flow has no back wires. Every node but the entry
// has one input, `in`, wired at most once, so a valid flow is a tree under
// its entry. Lowering (blocks/flowGen.ts) walks that tree.
//
// While the payload runs, each node outside a repeat body logs `flow <id>:
// <node>` when it runs (`flowLog`); `flowTrail` turns those lines into the
// nodes of each flow's latest run, which the canvas highlights.

import { type ActionRef, type Expr, MAX_EXPR_DEPTH, actionProblem, exprProblem, raisesEvent } from "./machines";
import type { PinFunc } from "./pins";

/** Most flows in a payload (logic sources after the Blocks workspace). */
export const MAX_FLOWS = 8;
/** Most nodes and wires per flow (the Rust check's limits). */
export const MAX_FLOW_NODES = 64;
export const MAX_FLOW_WIRES = 128;
/** Shape bounds (parse): a file over these is refused; between the check
 * limits above and these, it loads and the Rust check reports the size. */
export const PARSE_MAX_NODES = 128;
export const PARSE_MAX_WIRES = 256;
/** Largest repeat count (the generator's MAX_LOOP). */
export const MAX_REPEAT = 10_000;
/** Most steps of a sequence. */
export const MAX_SEQUENCE = 4;
/** Canvas coordinates are whole numbers in 0..MAX_COORD. */
export const MAX_COORD = 4000;
const MAX_NAME_TEXT = 200;

export const EDGES = ["rise", "fall", "both"] as const;
export type Edge = (typeof EDGES)[number];
/** The `mw_gpio_irq` edge code: 1 rising, 2 falling, 3 both. */
export const EDGE_CODE: Readonly<Record<Edge, number>> = { rise: 1, fall: 2, both: 3 };

export type EntryProps =
  | { on: "init" }
  | { on: "every"; ms: number }
  | { on: "message"; key: string }
  | { on: "pin-edge"; label: string; edge: Edge }
  | { on: "entered"; state: string };
export const ENTRY_ONS = ["init", "every", "message", "pin-edge", "entered"] as const;
export type EntryOn = (typeof ENTRY_ONS)[number];

interface At {
  id: string;
  x: number;
  y: number;
}
export type FlowNode =
  | (At & { kind: "entry"; props: EntryProps })
  | (At & { kind: "action"; props: ActionRef })
  | (At & { kind: "branch"; props: { cond: Expr } })
  | (At & { kind: "repeat"; props: { times: number } })
  | (At & { kind: "sequence"; props: { steps: number } });
export const NODE_KINDS = ["entry", "action", "branch", "repeat", "sequence"] as const;
export type NodeKind = (typeof NODE_KINDS)[number];

export interface PortRef {
  node: string;
  port: string;
}
export interface Wire {
  from: PortRef;
  to: PortRef;
}

export interface Flow {
  kind: "flow";
  id: string;
  name: string;
  nodes: readonly FlowNode[];
  wires: readonly Wire[];
}

/** The input port's name. */
export const IN_PORT = "in";

/** A node's output ports, in the order they run. */
export function outputs(n: FlowNode): readonly string[] {
  switch (n.kind) {
    case "entry":
    case "action":
      return ["next"];
    case "branch":
      return ["then", "else", "next"];
    case "repeat":
      return ["body", "next"];
    case "sequence":
      return ["1", "2", "3", "4"].slice(0, Math.min(MAX_SEQUENCE, Math.max(2, Math.trunc(n.props.steps))));
  }
}

/** Whether the node has an input (every node but the entry). */
export function hasInput(n: FlowNode): boolean {
  return n.kind !== "entry";
}

/** How a port reads in the outline and on the canvas. */
export function portText(port: string): string {
  return /^\d$/.test(port) ? `step ${port}` : port;
}

// ---- issues

/** One problem `checkFlows` found (protocol-wasm, `state-table`). */
export interface FlowIssue {
  /** Index into the payload's flows. */
  flow: number;
  /** The node (index) or wire (index) it is about; both null for the flow. */
  node: number | null;
  wire: number | null;
  severity: "error" | "warning";
  message: string;
  fix?: FlowFix;
}

export type FlowFix =
  | { kind: "add-pin"; label: string; func: PinFunc }
  /** Add event `name` to machine `machine` (an index). */
  | { kind: "add-event"; name: string; machine?: number }
  | { kind: "add-state" | "set-initial"; name: string };

/** protocol-wasm `checkFlows`. */
export type FlowChecker = (
  flows: readonly Flow[],
  machines: readonly import("./machines").Machine[],
  pins: readonly { label: string; func: PinFunc }[],
) => FlowIssue[];

export function hasFlowErrors(issues: readonly FlowIssue[]): boolean {
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
function text(v: unknown, max = MAX_NAME_TEXT): v is string {
  return typeof v === "string" && v.length <= max;
}
function whole(v: unknown, lo: number, hi: number): v is number {
  return typeof v === "number" && Number.isInteger(v) && v >= lo && v <= hi;
}

function entryProblem(v: unknown, at: string): string | null {
  if (!isObj(v)) return `${at} must be an object`;
  const keys: Record<string, readonly string[]> = {
    init: ["on"],
    every: ["on", "ms"],
    message: ["on", "key"],
    "pin-edge": ["on", "label", "edge"],
    entered: ["on", "state"],
  };
  const allowed = typeof v.on === "string" ? keys[v.on] : undefined;
  if (!allowed) return `${at}.on must be one of ${ENTRY_ONS.join(", ")}`;
  const extra = extraKey(v, allowed);
  if (extra !== undefined) return `${at} has an unknown field "${extra}"`;
  switch (v.on) {
    case "every":
      return whole(v.ms, 0, 0x7fffffff) ? null : `${at}.ms must be a whole number of milliseconds`;
    case "message":
      return text(v.key) ? null : `${at}.key must be text`;
    case "pin-edge":
      if (!text(v.label)) return `${at}.label must be text`;
      return (EDGES as readonly unknown[]).includes(v.edge) ? null : `${at}.edge must be one of ${EDGES.join(", ")}`;
    case "entered":
      return text(v.state) ? null : `${at}.state must be text (machine.state)`;
    default:
      return null;
  }
}

function nodeProblem(v: unknown, at: string): string | null {
  if (!isObj(v)) return `${at} must be an object`;
  const extra = extraKey(v, ["id", "kind", "x", "y", "props"]);
  if (extra !== undefined) return `${at} has an unknown field "${extra}"`;
  if (!text(v.id, 64)) return `${at}.id must be text`;
  if (!whole(v.x, 0, MAX_COORD) || !whole(v.y, 0, MAX_COORD)) return `${at}.x and .y must be whole numbers from 0 to ${MAX_COORD}`;
  const p = v.props;
  const pat = `${at}.props`;
  switch (v.kind) {
    case "entry":
      return entryProblem(p, pat);
    case "action":
      return actionProblem(p, pat);
    case "branch": {
      if (!isObj(p)) return `${pat} must be an object`;
      const e = extraKey(p, ["cond"]);
      if (e !== undefined) return `${pat} has an unknown field "${e}"`;
      return exprProblem(p.cond, `${pat}.cond`);
    }
    case "repeat": {
      if (!isObj(p)) return `${pat} must be an object`;
      const e = extraKey(p, ["times"]);
      if (e !== undefined) return `${pat} has an unknown field "${e}"`;
      return whole(p.times, 0, 0x7fffffff) ? null : `${pat}.times must be a whole number`;
    }
    case "sequence": {
      if (!isObj(p)) return `${pat} must be an object`;
      const e = extraKey(p, ["steps"]);
      if (e !== undefined) return `${pat} has an unknown field "${e}"`;
      return whole(p.steps, 0, 64) ? null : `${pat}.steps must be a whole number`;
    }
    default:
      return `${at}.kind must be one of ${NODE_KINDS.join(", ")}`;
  }
}

function portProblem(v: unknown, at: string): string | null {
  if (!isObj(v)) return `${at} must be an object`;
  const extra = extraKey(v, ["node", "port"]);
  if (extra !== undefined) return `${at} has an unknown field "${extra}"`;
  return text(v.node, 64) && text(v.port, 16) ? null : `${at} needs text node and port`;
}

/** The problem with one flow logic source, or null. */
export function flowProblem(v: unknown, at: string): string | null {
  if (!isObj(v)) return `${at} must be an object`;
  const extra = extraKey(v, ["kind", "id", "name", "nodes", "wires"]);
  if (extra !== undefined) return `${at} has an unknown field "${extra}"`;
  if (v.kind !== "flow") return `${at}.kind must be "flow"`;
  if (!text(v.id) || !text(v.name)) return `${at} needs text id and name`;
  if (!Array.isArray(v.nodes) || v.nodes.length > PARSE_MAX_NODES) return `${at}.nodes must be a list of at most ${PARSE_MAX_NODES} nodes`;
  for (const [i, n] of v.nodes.entries()) {
    const p = nodeProblem(n, `${at}.nodes[${i}]`);
    if (p) return p;
  }
  if (!Array.isArray(v.wires) || v.wires.length > PARSE_MAX_WIRES) return `${at}.wires must be a list of at most ${PARSE_MAX_WIRES} wires`;
  for (const [i, w] of v.wires.entries()) {
    const wat = `${at}.wires[${i}]`;
    if (!isObj(w)) return `${wat} must be an object`;
    const e = extraKey(w, ["from", "to"]);
    if (e !== undefined) return `${wat} has an unknown field "${e}"`;
    const p = portProblem(w.from, `${wat}.from`) ?? portProblem(w.to, `${wat}.to`);
    if (p) return p;
  }
  return null;
}

/** A deep copy (shape already checked), so edits never alias the input. */
export function copyFlows(fs: readonly Flow[]): Flow[] {
  return structuredClone(fs) as Flow[];
}

// ---- constructors and transitions (pure)

/** `base`, `base2`, ...: the first not in `taken` (case ignored). */
function free(taken: readonly string[], base: string): string {
  const t = new Set(taken.map((n) => n.toLowerCase()));
  if (!t.has(base.toLowerCase())) return base;
  for (let i = 2; i < 1000; i++) if (!t.has(`${base}${i}`.toLowerCase())) return `${base}${i}`;
  return `${base}_${taken.length}`;
}

/** A new flow with an id no other flow has, starting on init. */
export function newFlow(existing: readonly Flow[]): Flow {
  const id = free(
    existing.map((f) => f.id),
    "Flow",
  );
  return { kind: "flow", id, name: "", nodes: [{ id: "n1", kind: "entry", x: 40, y: 40, props: { on: "init" } }], wires: [] };
}

/** The default properties of a new node of `kind`. */
export function defaultNode(kind: NodeKind, id: string, x: number, y: number): FlowNode {
  const at = { id, x: clampCoord(x), y: clampCoord(y) };
  switch (kind) {
    case "entry":
      return { ...at, kind, props: { on: "init" } };
    case "action":
      return { ...at, kind, props: { kind: "log", text: "hello" } };
    case "branch":
      return { ...at, kind, props: { cond: { kind: "cmp", op: "eq", a: { kind: "pin", label: "BUTTON" }, b: { kind: "const", value: 0 } } } };
    case "repeat":
      return { ...at, kind, props: { times: 3 } };
    case "sequence":
      return { ...at, kind, props: { steps: 2 } };
  }
}

export function clampCoord(v: number): number {
  return Math.max(0, Math.min(MAX_COORD, Math.round(v)));
}

/** A node id the flow does not use yet: n1, n2, ... */
export function freeNodeId(f: Flow): string {
  const taken = new Set(f.nodes.map((n) => n.id));
  for (let i = 1; i <= PARSE_MAX_NODES + 1; i++) if (!taken.has(`n${i}`)) return `n${i}`;
  return `n${f.nodes.length + 1}`;
}

/** The flow with `node` added (unchanged at the node limit). */
export function addNode(f: Flow, node: FlowNode): Flow {
  return f.nodes.length >= MAX_FLOW_NODES ? f : { ...f, nodes: [...f.nodes, node] };
}

export function moveNode(f: Flow, id: string, x: number, y: number): Flow {
  return { ...f, nodes: f.nodes.map((n) => (n.id === id ? { ...n, x: clampCoord(x), y: clampCoord(y) } : n)) };
}

/** The node with new properties (and, for a kind change, new defaults). */
export function setNode(f: Flow, id: string, node: FlowNode): Flow {
  const nodes = f.nodes.map((n) => (n.id === id ? { ...node, id, x: n.x, y: n.y } : n));
  // Wires from ports the node no longer has are dropped.
  const changed = nodes.find((n) => n.id === id);
  const ports = changed ? outputs(changed) : [];
  const wires = f.wires.filter((w) => w.from.node !== id || ports.includes(w.from.port));
  const intoEntry = changed?.kind === "entry";
  return { ...f, nodes, wires: intoEntry ? wires.filter((w) => w.to.node !== id) : wires };
}

/** Removes the node and every wire to or from it. */
export function removeNode(f: Flow, id: string): Flow {
  return { ...f, nodes: f.nodes.filter((n) => n.id !== id), wires: f.wires.filter((w) => w.from.node !== id && w.to.node !== id) };
}

/** Wires output `port` of `from` to node `to`'s input. A port and an
 * input take one wire each, so an existing wire from that port or into
 * `to` is replaced; a wire to itself or into an entry is refused (the flow
 * is returned unchanged). */
export function connect(f: Flow, from: string, port: string, to: string): Flow {
  const src = f.nodes.find((n) => n.id === from);
  const dst = f.nodes.find((n) => n.id === to);
  if (!src || !dst || from === to || !hasInput(dst) || !outputs(src).includes(port)) return f;
  if (wouldCycle(f, from, to)) return f;
  const kept = f.wires.filter((w) => !(w.from.node === from && w.from.port === port) && w.to.node !== to);
  if (kept.length >= MAX_FLOW_WIRES) return f;
  return { ...f, wires: [...kept, { from: { node: from, port }, to: { node: to, port: IN_PORT } }] };
}

/** Whether a wire from `from` to `to` would close a loop: `from` is
 * reachable from `to`. Iterative, at most one pass per node. */
export function wouldCycle(f: Flow, from: string, to: string): boolean {
  const seen = new Set<string>([to]);
  for (let pass = 0; pass <= f.nodes.length; pass++) {
    const before = seen.size;
    for (const w of f.wires) if (seen.has(w.from.node)) seen.add(w.to.node);
    if (seen.has(from)) return true;
    if (seen.size === before) return false;
  }
  return seen.has(from);
}

export function disconnect(f: Flow, wire: number): Flow {
  return { ...f, wires: f.wires.filter((_, i) => i !== wire) };
}

/** The wire leaving `port` of `node`, with its index. */
export function wireFrom(f: Flow, node: string, port: string): { i: number; w: Wire } | null {
  const i = f.wires.findIndex((w) => w.from.node === node && w.from.port === port);
  const w = f.wires[i];
  return w ? { i, w } : null;
}

export function setFlow(fs: readonly Flow[], i: number, f: Flow): Flow[] {
  return fs.map((x, j) => (j === i ? f : x));
}

// ---- the outline (keyboard and screen-reader view)

export interface OutlineRow {
  node: FlowNode;
  /** 0 for the entry and for nodes nothing leads to. */
  depth: number;
  /** The port of the parent this node hangs from (null at the top). */
  via: { node: string; port: string } | null;
  /** False for a node nothing leads to from the entry. */
  attached: boolean;
}

/** The flow as an indented list in run order: the entry, then each port's
 * chain under its node (pre-order), then nodes nothing leads to. Each
 * node appears once. Iterative (an explicit stack), bounded by the node
 * count. */
export function outline(f: Flow): OutlineRow[] {
  const byId = new Map(f.nodes.map((n) => [n.id, n] as const));
  const rows: OutlineRow[] = [];
  const seen = new Set<string>();
  const walk = (start: FlowNode, attached: boolean) => {
    const stack: { node: FlowNode; depth: number; via: OutlineRow["via"] }[] = [{ node: start, depth: 0, via: null }];
    for (let t = stack.pop(); t; t = stack.pop()) {
      if (seen.has(t.node.id)) continue;
      seen.add(t.node.id);
      rows.push({ node: t.node, depth: t.depth, via: t.via, attached });
      const kids: typeof stack = [];
      for (const port of outputs(t.node)) {
        const w = wireFrom(f, t.node.id, port);
        const child = w ? byId.get(w.w.to.node) : undefined;
        if (child && !seen.has(child.id)) kids.push({ node: child, depth: t.depth + 1, via: { node: t.node.id, port } });
      }
      for (let i = kids.length - 1; i >= 0; i--) stack.push(kids[i] as (typeof stack)[number]);
    }
  };
  const entry = f.nodes.find((n) => n.kind === "entry");
  if (entry) walk(entry, true);
  // Then chains nothing leads to from the entry: from their first node
  // (no wire into it), and last any loop of nodes feeding each other.
  const fed = (n: FlowNode) => f.wires.some((w) => w.to.node === n.id && byId.has(w.from.node) && w.from.node !== n.id);
  for (const n of f.nodes) if (!seen.has(n.id) && !fed(n)) walk(n, false);
  for (const n of f.nodes) if (!seen.has(n.id)) walk(n, false);
  return rows;
}

// ---- reading a node

export function entryText(p: EntryProps): string {
  switch (p.on) {
    case "init":
      return "on start";
    case "every":
      return `every ${p.ms} ms`;
    case "message":
      return `on message ${p.key}`;
    case "pin-edge":
      return `on ${p.label} ${p.edge === "rise" ? "rising" : p.edge === "fall" ? "falling" : "any"} edge`;
    case "entered":
      return `when ${p.state} is entered`;
  }
}

// ---- references (renames from the Pins and States views)

function renameExprPin(e: Expr, from: string, to: string, depth: number): Expr {
  if (depth > MAX_EXPR_DEPTH) return e;
  switch (e.kind) {
    case "pin":
      return e.label === from ? { ...e, label: to } : e;
    case "const":
    case "state":
      return e;
    case "not":
      return { ...e, a: renameExprPin(e.a, from, to, depth + 1) };
    default:
      return { ...e, a: renameExprPin(e.a, from, to, depth + 1), b: renameExprPin(e.b, from, to, depth + 1) };
  }
}

function renameExprState(e: Expr, machine: string, from: string, to: string, depth: number): Expr {
  if (depth > MAX_EXPR_DEPTH) return e;
  switch (e.kind) {
    case "state":
      if (e.machine !== machine) return e;
      return from === "" ? { ...e, machine: to } : e.state === from ? { ...e, state: to } : e;
    case "pin":
    case "const":
      return e;
    case "not":
      return { ...e, a: renameExprState(e.a, machine, from, to, depth + 1) };
    default:
      return { ...e, a: renameExprState(e.a, machine, from, to, depth + 1), b: renameExprState(e.b, machine, from, to, depth + 1) };
  }
}

function mapNodes(fs: readonly Flow[], fn: (n: FlowNode) => FlowNode): Flow[] {
  return fs.map((f) => ({ ...f, nodes: f.nodes.map(fn) }));
}

/** Pin label `from` renamed to `to` in edge entries, actions and branch
 * conditions. */
export function renameFlowPin(fs: readonly Flow[], from: string, to: string): Flow[] {
  return mapNodes(fs, (n) => {
    if (n.kind === "entry" && n.props.on === "pin-edge" && n.props.label === from) return { ...n, props: { ...n.props, label: to } };
    if (n.kind === "action" && (n.props.kind === "write" || n.props.kind === "toggle") && n.props.label === from) return { ...n, props: { ...n.props, label: to } };
    if (n.kind === "branch") return { ...n, props: { cond: renameExprPin(n.props.cond, from, to, 1) } };
    return n;
  });
}

/** A machine's state or event renamed, or (kind "machine") a machine id
 * renamed, in every flow reference. */
export function renameFlowRef(fs: readonly Flow[], kind: "state" | "event" | "machine", id: string, from: string, to: string): Flow[] {
  return mapNodes(fs, (n) => {
    if (n.kind === "entry" && n.props.on === "entered") {
      const dot = n.props.state.indexOf(".");
      const m = n.props.state.slice(0, dot);
      const s = n.props.state.slice(dot + 1);
      if (kind === "machine" && m === from) return { ...n, props: { on: "entered", state: `${to}.${s}` } };
      if (kind === "state" && m === id && s === from) return { ...n, props: { on: "entered", state: `${id}.${to}` } };
      return n;
    }
    if (n.kind === "action" && n.props.kind === "raise") {
      if (kind === "machine" && n.props.machine === from) return { ...n, props: { ...n.props, machine: to } };
      if (kind === "event" && raisesEvent(n.props, "", id, from)) return { ...n, props: { ...n.props, event: to } };
      return n;
    }
    if (n.kind === "branch") {
      if (kind === "machine") return { ...n, props: { cond: renameExprState(n.props.cond, from, "", to, 1) } };
      if (kind === "state") return { ...n, props: { cond: renameExprState(n.props.cond, id, from, to, 1) } };
    }
    return n;
  });
}

/** Pin labels the flows name, with the function each needs. */
export function flowPinNeeds(fs: readonly Flow[]): { label: string; func: PinFunc }[] {
  const out: { label: string; func: PinFunc }[] = [];
  const add = (label: string, func: PinFunc) => {
    if (label && !out.some((n) => n.label === label)) out.push({ label, func });
  };
  const expr = (e: Expr, depth: number): void => {
    if (depth > MAX_EXPR_DEPTH) return;
    if (e.kind === "pin") add(e.label, "gpio-in");
    else if (e.kind === "cmp" || e.kind === "and" || e.kind === "or") {
      expr(e.a, depth + 1);
      expr(e.b, depth + 1);
    } else if (e.kind === "not") expr(e.a, depth + 1);
  };
  for (const f of fs) {
    for (const n of f.nodes) {
      if (n.kind === "entry" && n.props.on === "pin-edge") add(n.props.label, "gpio-in");
      else if (n.kind === "action" && (n.props.kind === "write" || n.props.kind === "toggle")) add(n.props.label, "gpio-out");
      else if (n.kind === "branch") expr(n.props.cond, 1);
    }
  }
  return out;
}

// ---- the simulator's view

/** The log line a node writes when it runs. */
export function flowLog(id: string, node: string): string {
  return `flow ${id}: ${node}`;
}

const FLOW_LOG_RE = /^flow ([A-Za-z][A-Za-z0-9_]*): ([A-Za-z][A-Za-z0-9_]*)$/;

/** The flow and node a `flow` log line names; null for any other line. */
export function parseFlowLog(line: string): { id: string; node: string } | null {
  const m = FLOW_LOG_RE.exec(line);
  return m?.[1] && m[2] ? { id: m[1], node: m[2] } : null;
}

/** Each flow's latest run after `lines`: the nodes logged since its entry
 * last ran, in order, by flow id. `entries` maps a flow id to its entry
 * node's id. */
export function flowTrail(prev: ReadonlyMap<string, readonly string[]>, line: string, entries: ReadonlyMap<string, string>): Map<string, readonly string[]> {
  const p = parseFlowLog(line);
  if (!p) return new Map(prev);
  const out = new Map(prev);
  const before = entries.get(p.id) === p.node ? [] : (prev.get(p.id) ?? []);
  out.set(p.id, [...before.slice(-(MAX_FLOW_NODES - 1)), p.node]);
  return out;
}
