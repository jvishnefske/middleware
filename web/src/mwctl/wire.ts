// The Wire tab's model: a boolean wiring as a graph of nodes and edges.
//
// The page edits the graph (add, move, rename, connect, remove) and asks
// mwctl about it after every change: `mwctl wire --json wire.json --emit
// report` answers with the problems (each naming its node or edge), the
// statements and the full command line that spell the same wiring, and
// the wiring itself with a position for every node. So the rules live in
// one place (tools/wire-core); nothing here decides what may connect to
// what. `connect` asks before it adds an edge and refuses one mwctl
// reports as an error.
//
// The same command line typed into the page (`fromCommandLine`) replaces
// the graph: the canvas and the command line are two views of one wiring.

import { type Mwctl, type PortHost, type RunOptions, type RunResult, splitCommandLine } from "./host";
import { WEB_PORT } from "./port";

/** A node kind as mwctl names it (`input`, `analog`, `const`, `channel`,
 * `and`, `lowpass`, ..., `output`, `pwm`). The list, with each kind's
 * settings, comes from mwctl ([`KindSpec`]). */
export type WireKind = string;
export type WirePull = "none" | "up" | "down";

/** A node's settings; which apply to which kind is mwctl's to say. */
export interface WireConfig {
  pin?: number;
  pull?: WirePull;
  invert?: boolean;
  value?: number;
  timer?: number;
  channel?: number;
  freq_hz?: number;
  mul?: number;
  div?: number;
  lo?: number;
  hi?: number;
  shift?: number;
  n?: number;
  ms?: number;
  /** Model-based blocks (`ukf`, `cbf`): `;`-separated expressions or
   * numbers, as mwctl reads them. */
  f?: string;
  h?: string;
  g?: string;
  q?: string;
  r?: string;
  x0?: string;
  p0?: string;
  alpha?: number;
  beta?: number;
  kappa?: number;
  scale?: number;
  umin?: number;
  umax?: number;
  /** How often the node updates; absent: the wiring's own period. */
  period_ms?: number;
  /** Channels: publish the value on `wire/<name>` when it changes. */
  trace?: boolean;
}

/** One setting of a kind, from mwctl's report. */
export interface ParamSpec {
  name: keyof WireConfig;
  /** `real`: any number; `text`: expressions or numbers separated by `;`. */
  type: "int" | "bool" | "pull" | "real" | "text";
  min: number | null;
  max: number | null;
  default: number | null;
  required: boolean;
  options?: string[];
  /** An example of a `text` setting. */
  hint?: string;
}

/** One node kind, from mwctl's report. */
export interface KindSpec {
  kind: WireKind;
  class: "source" | "channel" | "function" | "sink";
  /** Fewest and most channels it reads. */
  inputs: [number, number];
  /** Fewest and most channels it drives, each with its own value. */
  outputs?: [number, number];
  stateful: boolean;
  params: ParamSpec[];
}

export interface WireNode {
  id: string;
  kind: WireKind;
  config?: WireConfig;
  x?: number;
  y?: number;
}

export interface WireGraph {
  name: string;
  period_ms: number;
  nodes: WireNode[];
  edges: [string, string][];
}

export interface WireIssue {
  severity: "error" | "warning";
  node?: string;
  edge?: [string, string];
  message: string;
}

/** What `mwctl wire --emit report` prints. */
export interface WireReport {
  ok: boolean;
  issues: WireIssue[];
  statements: string[];
  command: string;
  graph: WireGraph;
  bytes: number;
  /** Every node kind mwctl knows, in palette order. */
  kinds: KindSpec[];
}

/** The file names the page gives mwctl. */
const JSON_FILE = "wire.json";
const WASM_FILE = "wire.wasm";

export const MAX_COORD = 4000;

export function emptyGraph(): WireGraph {
  return { name: "wire", period_ms: 10, nodes: [], edges: [] };
}

/** A push button that lights a lamp while a switch arms it. */
export const EXAMPLE_STATEMENTS = ["button=gpio15,pullup,invert", "armed=gpio14,pullup,invert", "lit=and(button,armed)", "gpio25=lit"];

export function copyGraph(g: WireGraph): WireGraph {
  return {
    name: g.name,
    period_ms: g.period_ms,
    nodes: g.nodes.map((n) => ({ ...n, config: n.config ? { ...n.config } : undefined })),
    edges: g.edges.map(([a, b]) => [a, b]),
  };
}

const NOT_FUNCTIONS = ["input", "analog", "const", "channel", "output", "pwm"];

/** Whether a kind is a function (for how a node is drawn; mwctl decides
 * what it may connect to). */
export function isFunction(kind: WireKind): boolean {
  return !NOT_FUNCTIONS.includes(kind);
}

/** JSON as mwctl reads it: no `undefined`, no empty config. */
export function graphJson(g: WireGraph): string {
  return JSON.stringify({
    name: g.name,
    period_ms: g.period_ms,
    nodes: g.nodes.map((n) => {
      // Unset, false and "none" are the defaults: left out.
      const config: Record<string, number | boolean | string> = {};
      for (const [k, v] of Object.entries(n.config ?? {})) {
        if (v !== undefined && v !== false && v !== "none" && v !== "" && !(typeof v === "number" && Number.isNaN(v))) config[k] = v;
      }
      const node: WireNode = { id: n.id, kind: n.kind };
      if (Object.keys(config).length > 0) node.config = config as WireConfig;
      if (n.x !== undefined && n.y !== undefined) {
        node.x = Math.round(n.x);
        node.y = Math.round(n.y);
      }
      return node;
    }),
    edges: g.edges,
  });
}

/** A graph from JSON text that has the wiring's outer shape; mwctl checks
 * the rest. Throws on anything else. */
export function parseGraph(text: string): WireGraph {
  const v = JSON.parse(text) as Partial<WireGraph> | null;
  if (!v || typeof v !== "object" || typeof v.name !== "string" || !Array.isArray(v.nodes) || !Array.isArray(v.edges)) {
    throw new Error("not a wiring: expected name, nodes and edges");
  }
  return { name: v.name, period_ms: typeof v.period_ms === "number" ? v.period_ms : 10, nodes: v.nodes, edges: v.edges };
}

// ---------------------------------------------------------------- edits

function freshId(g: WireGraph, stem: string): string {
  for (let n = 1; ; n++) {
    const id = `${stem}${n}`;
    if (!g.nodes.some((node) => node.id === id)) return id;
  }
}

const clamp = (v: number) => Math.max(0, Math.min(MAX_COORD, Math.round(v)));

/** Adds a node of `kind` at a canvas position; returns the new graph and
 * the node's id. Pin nodes start without a pin: the report asks for one. */
export function addNode(g: WireGraph, kind: WireKind, x: number, y: number): { graph: WireGraph; id: string } {
  const graph = copyGraph(g);
  const stems: Record<string, string> = { input: "in", output: "out", channel: "ch", analog: "adc", const: "k" };
  // Ids take letters, digits, _ and .; "pwm" and function names are fine.
  const stem = stems[kind] ?? kind;
  const id = freshId(graph, stem);
  graph.nodes.push({ id, kind, x: clamp(x), y: clamp(y) });
  return { graph, id };
}

export function moveNode(g: WireGraph, id: string, x: number, y: number): WireGraph {
  const graph = copyGraph(g);
  const node = graph.nodes.find((n) => n.id === id);
  if (node) {
    node.x = clamp(x);
    node.y = clamp(y);
  }
  return graph;
}

/** Removes a node and every edge at it. */
export function removeNode(g: WireGraph, id: string): WireGraph {
  const graph = copyGraph(g);
  graph.nodes = graph.nodes.filter((n) => n.id !== id);
  graph.edges = graph.edges.filter(([a, b]) => a !== id && b !== id);
  return graph;
}

export function removeEdge(g: WireGraph, from: string, to: string): WireGraph {
  const graph = copyGraph(g);
  graph.edges = graph.edges.filter(([a, b]) => !(a === from && b === to));
  return graph;
}

/** Renames a node and its edges. A name already taken, or an empty one,
 * changes nothing. */
export function renameNode(g: WireGraph, id: string, to: string): WireGraph {
  if (!to || to === id || g.nodes.some((n) => n.id === to)) return g;
  const graph = copyGraph(g);
  for (const n of graph.nodes) if (n.id === id) n.id = to;
  graph.edges = graph.edges.map(([a, b]) => [a === id ? to : a, b === id ? to : b]);
  return graph;
}

export function setConfig(g: WireGraph, id: string, change: WireConfig): WireGraph {
  const graph = copyGraph(g);
  const node = graph.nodes.find((n) => n.id === id);
  if (node) node.config = { ...node.config, ...change };
  return graph;
}

/** Changes which function a function node is. Its settings belong to
 * the old function and are dropped, except its rate. */
export function setFunction(g: WireGraph, id: string, kind: WireKind): WireGraph {
  const graph = copyGraph(g);
  const node = graph.nodes.find((n) => n.id === id);
  if (node && isFunction(node.kind) && isFunction(kind)) {
    node.kind = kind;
    node.config = node.config?.period_ms === undefined ? undefined : { period_ms: node.config.period_ms };
  }
  return graph;
}

/** The same wiring with every channel traced: what the simulation runs,
 * so every channel can be plotted. */
export function withTrace(g: WireGraph): WireGraph {
  const graph = copyGraph(g);
  for (const n of graph.nodes) if (n.kind === "channel") n.config = { ...n.config, trace: true };
  return graph;
}

// ---------------------------------------------------------------- mwctl

function chipArgs(chip: string | null | undefined): string[] {
  return chip ? ["--chip", chip] : [];
}

function parseReport(r: RunResult): WireReport {
  if (r.code !== 0 || !r.stdout.trim()) {
    throw new Error(r.stderr.replace(/^mwctl: /, "").split("\n")[0] || `mwctl exited with status ${r.code}`);
  }
  return JSON.parse(r.stdout) as WireReport;
}

/** Every node kind mwctl knows, with its settings (asked once, through
 * the report of a two-pin wiring). */
export async function kinds(mwctl: Mwctl): Promise<KindSpec[]> {
  return parseReport(await mwctl.run(["wire", "a=gpio1", "gpio2=a", "--emit", "report"])).kinds;
}

/** Checks `g` (against `chip`'s board when given). Throws when mwctl
 * cannot read the wiring at all, with its message. */
export async function report(mwctl: Mwctl, g: WireGraph, chip?: string | null): Promise<WireReport> {
  const files = new Map([[JSON_FILE, new TextEncoder().encode(graphJson(g))]]);
  return parseReport(await mwctl.run(["wire", "--json", JSON_FILE, "--emit", "report", ...chipArgs(chip)], { files }));
}

/** The wiring a command line describes: `mwctl wire ...`, `wire ...` or
 * bare statements; only wiring options are kept (`--name`, `--period`,
 * `--trace`). */
export async function fromCommandLine(mwctl: Mwctl, line: string, chip?: string | null): Promise<WireReport> {
  let words = splitCommandLine(line);
  if (words[0] === "mwctl") words = words.slice(1);
  if (words[0] === "wire") words = words.slice(1);
  const args: string[] = [];
  for (let i = 0; i < words.length; i++) {
    const w = words[i];
    if (w === "--name" || w === "--period") args.push(w, words[++i] ?? "");
    else if (w === "--trace") args.push(w);
    else if (w.startsWith("-")) throw new Error(`${w} is not part of a wiring; type statements, --name, --period and --trace`);
    else args.push(w);
  }
  return parseReport(await mwctl.run(["wire", ...args, "--emit", "report", ...chipArgs(chip)]));
}

export interface Connected {
  graph: WireGraph;
  /** Why the edge was refused; null when it was added. */
  refused: string | null;
}

/** Adds the edge `from -> to` unless mwctl reports it as an error. */
export async function connect(mwctl: Mwctl, g: WireGraph, from: string, to: string): Promise<Connected> {
  if (from === to) return { graph: g, refused: "a node cannot connect to itself" };
  const graph = copyGraph(g);
  graph.edges.push([from, to]);
  const r = await report(mwctl, graph);
  const bad = r.issues.find((i) => i.severity === "error" && i.edge?.[0] === from && i.edge?.[1] === to);
  return bad ? { graph: g, refused: bad.message } : { graph, refused: null };
}

/** Builds the payload. Throws with the first error when the wiring has one. */
export async function build(mwctl: Mwctl, g: WireGraph, chip?: string | null): Promise<Uint8Array> {
  const files = new Map([[JSON_FILE, new TextEncoder().encode(graphJson(g))]]);
  const r = await mwctl.run(["wire", "--json", JSON_FILE, "-o", WASM_FILE, ...chipArgs(chip)], { files });
  const wasm = files.get(WASM_FILE);
  if (r.code !== 0 || !wasm) {
    const first = r.stderr.split("\n").find((l) => l.startsWith("error: ") || l.startsWith("mwctl: "));
    throw new Error(first?.replace(/^(error|mwctl): /, "") ?? `mwctl exited with status ${r.code}`);
  }
  return wasm;
}

/** Writes the wiring to the board on `port`: mwctl checks it against the
 * board's own chip, uploads it and starts it. Output streams through
 * `opts`. */
export function write(mwctl: Mwctl, g: WireGraph, port: PortHost, opts: RunOptions = {}): Promise<RunResult> {
  const files = new Map([[JSON_FILE, new TextEncoder().encode(graphJson(g))]]);
  return mwctl.run(["--port", WEB_PORT, "wire", "--json", JSON_FILE], { ...opts, files, port });
}

// ---------------------------------------------------------------- undo

export const UNDO_LIMIT = 50;

/** The last [`UNDO_LIMIT`] graphs before the current one. */
export class History {
  private readonly past: string[] = [];

  push(g: WireGraph): void {
    const text = graphJson(g);
    if (this.past[this.past.length - 1] === text) return;
    this.past.push(text);
    if (this.past.length > UNDO_LIMIT) this.past.shift();
  }

  pop(): WireGraph | null {
    const text = this.past.pop();
    return text === undefined ? null : parseGraph(text);
  }

  get size(): number {
    return this.past.length;
  }
}

// ---------------------------------------------------------------- storage

export const STORAGE_KEY = "wire";

export function saveGraph(storage: Pick<Storage, "setItem"> | undefined, g: WireGraph): void {
  try {
    storage?.setItem(STORAGE_KEY, graphJson(g));
  } catch {
    /* storage unavailable or full: the wiring stays in the page */
  }
}

export function loadGraph(storage: Pick<Storage, "getItem"> | undefined): WireGraph | null {
  try {
    const text = storage?.getItem(STORAGE_KEY);
    return text ? parseGraph(text) : null;
  } catch {
    return null;
  }
}
