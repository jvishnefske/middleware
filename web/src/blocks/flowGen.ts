// Flows (model/flows.ts) lowered onto the shared statement IR (stmt.ts).
// Pure: a flow in, its entry and body statements out; the Blocks
// generator (generator.ts) places the body in the handler the entry names
// (init, the tick, on_message, on_irq for a pin edge, or the machine's
// enter function for "entered"), exactly where the equivalent hat block's
// body goes, and owns the host (strings, imports, pins, machines, loop
// variables).
//
// A valid flow is a tree under its entry (each input wired at most once,
// no cycles; checked in Rust by `checkFlows`). Lowering walks it: a chain
// follows `next` ports iteratively; a branch becomes `if (cond != 0) {
// then } else { else }` and continues with its `next`; a repeat becomes a
// `for` of its constant count around its `body` chain, then `next`; a
// sequence runs its step chains in order. Each node outside a repeat body
// (sequences aside) first logs `flow <id>: <node>` (flows.ts `flowLog`),
// which the canvas highlights; inside a body that would log up to
// `MAX_REPEAT` lines per run, so the repeat's own line stands for it.
//
// The walk recurses into `then`, `else`, `body` and sequence steps, and
// every call first marks its node as lowered and refuses one already
// lowered, so there are at most `MAX_FLOW_NODES` calls in all and the
// recursion depth is bounded by the 64-node limit (the planner refuses a
// larger flow). Like the machine code, it never trusts the Rust check: a
// flow over the limits, with a bad or duplicate id, no entry or two, is
// reported and left out; a node reached twice (a merge or a loop) is
// reported and lowered once; bad actions and conditions are reported and
// left out (a bad condition reads as 0). The output is always valid
// source.

import { type EntryProps, type Flow, type FlowNode, MAX_FLOW_NODES, MAX_FLOW_WIRES, MAX_FLOWS, MAX_REPEAT, flowLog, outputs, wireFrom } from "../model/flows";
import { type LowerHost, type Stmt, line, lowerAction, lowerExpr, textArgs } from "./stmt";

/** What lowering flows needs besides the shared lowering host. */
export interface FlowHost extends LowerHost {
  error(message: string): void;
  /** A bounded loop with a fresh loop variable (generator.ts `repeat`). */
  repeat(count: number, body: (v: string) => Stmt[]): Stmt;
}

/** A flow that passed the structural checks: its entry and body. */
export interface LoweredFlow {
  flow: Flow;
  entry: EntryProps;
  body: Stmt[];
}

const IDENT = /^[A-Za-z][A-Za-z0-9_]*$/;

function who(f: Flow, i: number): string {
  return IDENT.test(f.id) ? `flow ${f.id}` : `flow ${i + 1}`;
}

/** Flows that get code, lowered, in order; every flow left out has an
 * error explaining why. */
export function lowerFlows(flows: readonly Flow[], host: FlowHost): LoweredFlow[] {
  const out: LoweredFlow[] = [];
  const ids = new Set<string>();
  flows.forEach((f, i) => {
    const at = who(f, i);
    const fail = (msg: string) => host.error(`${at}: ${msg}`);
    if (i >= MAX_FLOWS) return fail(`a payload has at most ${MAX_FLOWS} flows`);
    if (!IDENT.test(f.id)) return fail(`the name "${f.id}" must start with a letter and hold only letters, digits and _`);
    if (ids.has(f.id.toLowerCase())) return fail("another flow has this name");
    ids.add(f.id.toLowerCase());
    if (f.nodes.length > MAX_FLOW_NODES) return fail(`has more than ${MAX_FLOW_NODES} nodes`);
    if (f.wires.length > MAX_FLOW_WIRES) return fail(`has more than ${MAX_FLOW_WIRES} wires`);
    const nodeIds = new Set<string>();
    for (const n of f.nodes) {
      if (!IDENT.test(n.id)) return fail(`node "${n.id}" must start with a letter and hold only letters, digits and _`);
      if (nodeIds.has(n.id)) return fail(`two nodes are named ${n.id}`);
      nodeIds.add(n.id);
    }
    const entries = f.nodes.filter((n) => n.kind === "entry");
    const entry = entries[0];
    if (!entry || entry.kind !== "entry") return fail("has no entry");
    if (entries.length > 1) return fail("has more than one entry");
    out.push({ flow: f, entry: entry.props, body: new Walk(f, host, at).run(entry) });
  });
  return out;
}

class Walk {
  private readonly byId: ReadonlyMap<string, FlowNode>;
  private readonly done = new Set<string>();

  constructor(
    private readonly f: Flow,
    private readonly host: FlowHost,
    private readonly at: string,
  ) {
    this.byId = new Map(f.nodes.map((n) => [n.id, n] as const));
  }

  private fail(n: FlowNode): (msg: string) => void {
    return (msg) => this.host.error(`${this.at} node ${n.id}: ${msg}`);
  }

  /** The node `port` of `n` leads to. */
  private target(n: FlowNode, port: string): FlowNode | undefined {
    const w = wireFrom(this.f, n.id, port);
    return w ? this.byId.get(w.w.to.node) : undefined;
  }

  run(entry: FlowNode): Stmt[] {
    return this.chain(entry, false);
  }

  /** The statements of the chain starting at `first`: each node, then the
   * node its `next` leads to (iterative along `next`). */
  private chain(first: FlowNode | undefined, inRepeat: boolean): Stmt[] {
    const out: Stmt[] = [];
    for (let n = first; n; ) {
      if (this.done.has(n.id)) {
        this.fail(n)("is reached twice (two wires lead into it, or a loop); it runs only the first time");
        break;
      }
      if (this.done.size >= MAX_FLOW_NODES) break;
      this.done.add(n.id);
      if (!inRepeat && n.kind !== "sequence") {
        this.host.use("log");
        out.push(line(`mw_log(${textArgs(this.host, flowLog(this.f.id, n.id))});`));
      }
      switch (n.kind) {
        case "entry":
          break;
        case "action":
          out.push(...lowerAction(n.props, this.host, this.fail(n), null));
          break;
        case "branch": {
          const cond = lowerExpr(n.props.cond, this.host, this.fail(n));
          const then = this.chain(this.target(n, "then"), inRepeat);
          const otherwise = this.chain(this.target(n, "else"), inRepeat);
          out.push({ kind: "if", arms: [{ cond, body: then }], else: otherwise.length > 0 ? otherwise : null });
          break;
        }
        case "repeat": {
          const t = n.props.times;
          if (!Number.isInteger(t) || t < 1 || t > MAX_REPEAT) this.fail(n)(`repeat 1 to ${MAX_REPEAT} times`);
          const count = Number.isInteger(t) ? Math.max(0, Math.min(t, MAX_REPEAT)) : 0;
          const body = this.target(n, "body");
          out.push(this.host.repeat(count, () => this.chain(body, true)));
          break;
        }
        case "sequence":
          for (const port of outputs(n)) out.push(...this.chain(this.target(n, port), inRepeat));
          // A sequence has no `next`: its last step is the continuation.
          return out;
      }
      n = this.target(n, "next");
    }
    return out;
  }
}
