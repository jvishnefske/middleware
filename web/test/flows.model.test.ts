// Flows: the pure model (model/flows.ts): ports, edits that keep a flow a
// tree (connect replaces, refuses loops and wires into the entry), the
// outline order, renames from the Pins and States views, pin needs, the
// `flow` log line, the latest-run trail, and checkFlows (Rust) through
// protocol-wasm.

import { describe, expect, it } from "vitest";
import { MODE_FLOWS } from "../src/blocks/examples/mode.flows";
import { exampleWorkspace } from "../src/blocks/examples";
import {
  type Flow,
  type FlowNode,
  addNode,
  connect,
  defaultNode,
  disconnect,
  flowLog,
  flowPinNeeds,
  flowTrail,
  freeNodeId,
  moveNode,
  newFlow,
  outline,
  outputs,
  parseFlowLog,
  removeNode,
  renameFlowPin,
  renameFlowRef,
  setNode,
  wouldCycle,
} from "../src/model/flows";
import type { Machine } from "../src/model/machines";
import { row } from "../src/model/pins";
import { protocol } from "./fakeDevice";

const proto = protocol();

function chain(...kinds: FlowNode["kind"][]): Flow {
  let f = newFlow([]);
  for (const k of kinds) f = addNode(f, defaultNode(k, freeNodeId(f), 0, 0));
  return f;
}

describe("flow model", () => {
  it("names each kind's ports in run order; a sequence has 2 to 4 steps", () => {
    const n = (k: FlowNode["kind"]) => outputs(defaultNode(k, "x", 0, 0));
    expect(n("entry")).toEqual(["next"]);
    expect(n("action")).toEqual(["next"]);
    expect(n("branch")).toEqual(["then", "else", "next"]);
    expect(n("repeat")).toEqual(["body", "next"]);
    expect(n("sequence")).toEqual(["1", "2"]);
    expect(outputs({ ...defaultNode("sequence", "s", 0, 0), props: { steps: 9 } } as FlowNode)).toEqual(["1", "2", "3", "4"]);
  });

  it("connect keeps one wire per port and per input, and refuses self, entry and loop wires", () => {
    let f = chain("action", "action", "action"); // n1 entry, n2, n3, n4
    f = connect(f, "n1", "next", "n2");
    f = connect(f, "n2", "next", "n3");
    expect(f.wires).toHaveLength(2);
    // Re-wiring a port replaces its wire; wiring into a fed node replaces that one.
    const g = connect(f, "n1", "next", "n4");
    expect(g.wires.map((w) => `${w.from.node}->${w.to.node}`)).toEqual(["n2->n3", "n1->n4"]);
    const h = connect(f, "n4", "next", "n3");
    expect(h.wires.map((w) => `${w.from.node}->${w.to.node}`)).toEqual(["n1->n2", "n4->n3"]);
    expect(connect(f, "n2", "next", "n2")).toBe(f);
    expect(connect(f, "n2", "next", "n1")).toBe(f);
    expect(connect(f, "n2", "then", "n4")).toBe(f);
    expect(wouldCycle(f, "n3", "n1")).toBe(true);
    expect(connect(f, "n3", "next", "n2")).toBe(f);
    expect(disconnect(f, 0).wires).toHaveLength(1);
  });

  it("removing a node drops its wires; changing its kind drops wires from ports it lost; moves clamp", () => {
    let f = chain("branch", "action", "action");
    f = connect(connect(connect(f, "n1", "next", "n2"), "n2", "then", "n3"), "n2", "else", "n4");
    expect(removeNode(f, "n2").wires).toEqual([]);
    const asAction = setNode(f, "n2", defaultNode("action", "n2", 0, 0));
    expect(asAction.wires.map((w) => w.from.port)).toEqual(["next"]);
    expect(asAction.nodes[1]).toMatchObject({ kind: "action", x: 0, y: 0 });
    expect(moveNode(f, "n2", -5, 99999).nodes[1]).toMatchObject({ x: 0, y: 4000 });
    let big = newFlow([]);
    for (let i = 0; i < 70; i++) big = addNode(big, defaultNode("action", freeNodeId(big), 0, 0));
    expect(big.nodes).toHaveLength(64);
  });

  it("lists the outline in run order with each port's chain under its node, then nodes nothing leads to", () => {
    let f = chain("branch", "action", "action", "action", "action");
    f = connect(f, "n1", "next", "n2");
    f = connect(f, "n2", "then", "n3");
    f = connect(f, "n2", "else", "n4");
    f = connect(f, "n2", "next", "n5");
    const rows = outline(f).map((r) => `${"  ".repeat(r.depth)}${r.node.id}${r.via ? ` (${r.via.port})` : ""}${r.attached ? "" : " !"}`);
    expect(rows).toEqual(["n1", "  n2 (next)", "    n3 (then)", "    n4 (else)", "    n5 (next)", "n6 !"]);
    // A loop of unattached nodes is still listed, once each.
    const loop: Flow = { ...f, wires: [...f.wires, { from: { node: "n6", port: "next" }, to: { node: "n6", port: "in" } }] };
    expect(outline(loop).map((r) => r.node.id)).toEqual(["n1", "n2", "n3", "n4", "n5", "n6"]);
  });

  it("renames pins, machines, states and events in every reference", () => {
    const branch: FlowNode = {
      id: "b",
      kind: "branch",
      x: 0,
      y: 0,
      props: { cond: { kind: "and", a: { kind: "state", machine: "Mode", state: "Blink" }, b: { kind: "pin", label: "BUTTON" } } },
    };
    const fs: Flow[] = [{ ...structuredClone(MODE_FLOWS[0] as Flow), nodes: [...(MODE_FLOWS[0] as Flow).nodes, branch] }, structuredClone(MODE_FLOWS[2] as Flow)];
    const pin = renameFlowPin(fs, "BUTTON", "KEY");
    expect(pin[0]?.nodes[0]?.props).toMatchObject({ label: "KEY" });
    expect(pin[0]?.nodes[2]?.props).toMatchObject({ cond: { b: { label: "KEY" } } });
    const ev = renameFlowRef(fs, "event", "Mode", "PRESS", "PUSH");
    expect(ev[0]?.nodes[1]?.props).toMatchObject({ machine: "Mode", event: "PUSH" });
    const st = renameFlowRef(fs, "state", "Mode", "Blink", "Flash");
    expect(st[1]?.nodes[0]?.props).toEqual({ on: "entered", state: "Mode.Flash" });
    expect(st[0]?.nodes[2]?.props).toMatchObject({ cond: { a: { state: "Flash" } } });
    const m = renameFlowRef(fs, "machine", "", "Mode", "Led");
    expect(m[0]?.nodes[1]?.props).toMatchObject({ machine: "Led" });
    expect(m[1]?.nodes[0]?.props).toEqual({ on: "entered", state: "Led.Blink" });
    expect(m[0]?.nodes[2]?.props).toMatchObject({ cond: { a: { machine: "Led" } } });
    expect(flowPinNeeds(fs)).toEqual([{ label: "BUTTON", func: "gpio-in" }]);
  });

  it("parses flow log lines and keeps each flow's latest run", () => {
    expect(parseFlowLog(flowLog("Press", "n2"))).toEqual({ id: "Press", node: "n2" });
    expect(parseFlowLog("sm Mode: Off")).toBeNull();
    expect(parseFlowLog("flow Press: n2 extra")).toBeNull();
    const entries = new Map([["Press", "n1"]]);
    let t = new Map<string, readonly string[]>();
    for (const l of [flowLog("Press", "n1"), flowLog("Press", "n2"), "noise", flowLog("Press", "n1")]) t = flowTrail(t, l, entries);
    expect(t.get("Press")).toEqual(["n1"]);
    t = flowTrail(t, flowLog("Press", "n2"), entries);
    expect(t.get("Press")).toEqual(["n1", "n2"]);
  });
});

describe("checkFlows (Rust state-table flow rules)", () => {
  const machines = exampleWorkspace("mode")!.machines as readonly Machine[];
  const pins = [row("gpio-out", "LED", 25), row("gpio-in", "BUTTON", 15)].map((r) => ({ label: r.label, func: r.func }));

  it("finds no issue in the mode example's flows", () => {
    expect(proto.checkFlows(MODE_FLOWS, machines, pins)).toEqual([]);
  });

  it("reports a missing entry, a merge, an unknown event with an add fix and an unreachable node, by index", () => {
    let f = chain("action", "action", "action");
    f = { ...f, nodes: f.nodes.map((n) => (n.id === "n2" ? { ...n, props: { kind: "raise", machine: "Mode", event: "HOLD" } } : n)) as FlowNode[] };
    f = connect(connect(f, "n1", "next", "n2"), "n2", "next", "n3");
    const merged: Flow = { ...f, wires: [...f.wires, { from: { node: "n1", port: "next" }, to: { node: "n4", port: "in" } }] };
    const issues = proto.checkFlows([merged, { ...newFlow([]), id: "Empty", nodes: [] }], machines, pins);
    expect(issues).toEqual([
      {
        flow: 0,
        node: 1,
        wire: null,
        severity: "error",
        message: "This action raises HOLD, which is not an event of Mode; add it or choose another.",
        fix: { kind: "add-event", name: "HOLD", machine: 0 },
      },
      expect.objectContaining({ flow: 0, node: null, wire: 2, severity: "error", message: expect.stringContaining("This port already leads to the wire n1.next -> n2") }),
      expect.objectContaining({ flow: 0, node: 3, severity: "warning", message: "Nothing leads from the entry to this node, so it never runs." }),
      expect.objectContaining({ flow: 1, node: null, wire: null, severity: "error", message: expect.stringContaining("This flow has no entry") }),
    ]);
  });

  it("checks branch conditions' state tests and pins like table guards", () => {
    let f = chain("branch", "action");
    f = { ...f, nodes: f.nodes.map((n) => (n.kind === "branch" ? { ...n, props: { cond: { kind: "state", machine: "Mode", state: "Dim" } } } : n)) as FlowNode[] };
    f = connect(connect(f, "n1", "next", "n2"), "n2", "then", "n3");
    expect(proto.checkFlows([f], machines, pins).map((i) => i.message)).toEqual(["The condition tests Mode is Dim, but Mode has no state Dim."]);
  });
});
