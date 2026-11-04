// The Flows view's decisions (ui/flowsModel.ts, pure): port positions
// and wire curves, the canvas size and zoom steps, node titles, new
// entries / actions / conditions keeping what applies, the outline's
// connect menu and reordering, the undo history bound, issues by node and
// wire, the tab text, fixes, and the simulator's live line. The DOM
// itself (flowsView.ts) is verified in headless Chrome only (design.md
// U39).

import { describe, expect, it } from "vitest";
import { exampleWorkspace } from "../src/blocks/examples";
import { type Flow, type FlowIssue, type FlowNode, addNode, connect, defaultNode, freeNodeId, newFlow } from "../src/model/flows";
import type { Machine } from "../src/model/machines";
import {
  NODE_H,
  NODE_W,
  UNDO_DEPTH,
  applyFlowFix,
  canvasSize,
  condForm,
  condOf,
  condOfForm,
  connectTargets,
  entryOf,
  flowActionOf,
  flowFixText,
  flowTrailText,
  flowsTabText,
  issuesForNode,
  moveEarlier,
  moveLater,
  nodeLabel,
  nodeTitle,
  portPos,
  record,
  severityOf,
  wirePath,
  zoomStep,
} from "../src/ui/flowsModel";

const machines = exampleWorkspace("mode")!.machines as readonly Machine[];
const pins = [
  { label: "LED", func: "gpio-out" as const },
  { label: "BUTTON", func: "gpio-in" as const },
];
function chain(n: number): Flow {
  let f = newFlow([]);
  for (let i = 0; i < n; i++) {
    const id = freeNodeId(f);
    f = addNode(f, { ...defaultNode("action", id, 40, 140 + 100 * i), props: { kind: "log", text: id } } as FlowNode);
    f = connect(f, f.nodes[f.nodes.length - 2]?.id ?? "n1", "next", id);
  }
  return f;
}
const order = (f: Flow) => {
  const out: string[] = [];
  for (let id: string | undefined = "n1", k = 0; id && k < 10; k++) {
    out.push(id);
    id = f.wires.find((w) => w.from.node === id && w.from.port === "next")?.to.node;
  }
  return out;
};

describe("flows view decisions", () => {
  it("places the input on top and outputs along the bottom in run order", () => {
    const b = defaultNode("branch", "b", 100, 200);
    expect(portPos(b, "in")).toEqual({ x: 100 + NODE_W / 2, y: 200 });
    const xs = ["then", "else", "next"].map((p) => portPos(b, p));
    expect(xs.every((p) => p.y === 200 + NODE_H)).toBe(true);
    expect(xs[0]!.x < xs[1]!.x && xs[1]!.x < xs[2]!.x).toBe(true);
    expect(wirePath({ x: 0, y: 0 }, { x: 10, y: 200 })).toBe("M 0 0 C 0 100 10 100 10 200");
    expect(wirePath({ x: 0, y: 100 }, { x: 10, y: 100 })).toBe("M 0 100 C 0 170 10 30 10 100");
    expect(canvasSize(newFlow([]))).toEqual({ w: 480, h: 320 });
    expect(canvasSize({ ...newFlow([]), nodes: [defaultNode("action", "a", 1000, 900)] })).toEqual({ w: 1000 + NODE_W + 160, h: 900 + NODE_H + 160 });
    expect([zoomStep(1, 1), zoomStep(1.5, 1), zoomStep(0.5, -1), zoomStep(0.75, 0)]).toEqual([1.25, 1.5, 0.5, 1]);
  });

  it("says what each node does", () => {
    const f = exampleWorkspace("mode-flows")!.flows![0]!;
    expect(f.nodes.map(nodeTitle)).toEqual(["on BUTTON falling edge", "raise Mode.PRESS"]);
    expect(nodeLabel(f.nodes[1]!)).toBe("n2, action: raise Mode.PRESS");
    expect(nodeTitle(defaultNode("repeat", "r", 0, 0))).toBe("repeat 3 times");
    expect(nodeTitle({ ...defaultNode("branch", "b", 0, 0), props: { cond: { kind: "state", machine: "Mode", state: "On" } } } as FlowNode)).toBe("if Mode is On");
  });

  it("makes new entries, actions and conditions from the pins and machines, keeping what applies", () => {
    expect(entryOf("pin-edge", { on: "init" }, pins, machines)).toEqual({ on: "pin-edge", label: "BUTTON", edge: "fall" });
    expect(entryOf("every", { on: "every", ms: 250 }, pins, machines)).toEqual({ on: "every", ms: 250 });
    expect(entryOf("entered", { on: "init" }, pins, machines)).toEqual({ on: "entered", state: "Mode.Off" });
    expect(flowActionOf("raise", { kind: "log", text: "" }, pins, machines)).toEqual({ kind: "raise", machine: "Mode", event: "PRESS" });
    expect(flowActionOf("toggle", { kind: "write", label: "X", level: 1 }, pins, machines)).toEqual({ kind: "toggle", label: "X" });
    expect(flowActionOf("raise", { kind: "log", text: "" }, pins, [])).toEqual({ kind: "raise", machine: "", event: "" });
    const pin = condForm({ kind: "cmp", op: "ne", a: { kind: "pin", label: "BUTTON" }, b: { kind: "const", value: 1 } });
    expect(pin).toEqual({ form: "pin", label: "BUTTON", op: "ne", value: 1 });
    expect(condOf(pin)).toEqual({ kind: "cmp", op: "ne", a: { kind: "pin", label: "BUTTON" }, b: { kind: "const", value: 1 } });
    expect(condOfForm("state", pin, pins, machines)).toEqual({ kind: "state", machine: "Mode", state: "Off" });
    expect(condForm({ kind: "not", a: { kind: "pin", label: "B" } })).toEqual({ form: "other", text: "not (B)" });
  });

  it("offers as targets only nodes that keep the flow a tree, and reorders a chain", () => {
    const f = chain(3); // n1 -> n2 -> n3 -> n4
    expect(connectTargets(f, "n3").map((n) => n.id)).toEqual(["n4"]);
    expect(connectTargets(f, "n4").map((n) => n.id)).toEqual([]);
    expect(order(moveEarlier(f, "n3"))).toEqual(["n1", "n3", "n2", "n4"]);
    expect(order(moveLater(f, "n2"))).toEqual(["n1", "n3", "n2", "n4"]);
    expect(order(moveEarlier(f, "n4"))).toEqual(["n1", "n2", "n4", "n3"]);
    // The node right after the entry cannot move above it.
    expect(moveEarlier(f, "n2")).toBe(f);
    expect(moveLater(f, "n4")).toBe(f);
  });

  it("keeps at most fifty undo steps", () => {
    let past: (readonly Flow[])[] = [];
    for (let i = 0; i < 60; i++) past = record(past, [{ ...newFlow([]), id: `F${i}` }]);
    expect(past).toHaveLength(UNDO_DEPTH);
    expect(past[UNDO_DEPTH - 1]?.[0]?.id).toBe("F59");
  });

  it("maps issues to nodes, counts errors on the tab, and turns fixes into changes", () => {
    const issues: FlowIssue[] = [
      { flow: 0, node: 1, wire: null, severity: "warning", message: "w" },
      { flow: 0, node: 1, wire: null, severity: "error", message: "e", fix: { kind: "add-event", name: "HOLD", machine: 0 } },
      { flow: 1, node: 1, wire: null, severity: "error", message: "x" },
    ];
    expect(issuesForNode(issues, 0, 1)).toHaveLength(2);
    expect(severityOf(issuesForNode(issues, 0, 1))).toBe("error");
    expect(severityOf(issues.slice(0, 1))).toBe("warning");
    expect(severityOf([])).toBeNull();
    expect(flowsTabText(issues)).toBe("Flows (2 to fix)");
    expect(flowsTabText(issues.slice(0, 1))).toBe("Flows");
    const fix = issues[1]!.fix!;
    expect(flowFixText(fix, machines)).toBe("Add event HOLD to Mode");
    const r = applyFlowFix(machines, fix);
    expect(r && "machines" in r && r.machines[0]?.events.map((e) => e.name)).toEqual(["PRESS", "TICK", "HOLD"]);
    expect(applyFlowFix(machines, { kind: "add-pin", label: "SW", func: "gpio-in" })).toEqual({ pin: { label: "SW", func: "gpio-in" } });
  });

  it("names the node each flow last ran in the simulator line", () => {
    expect(
      flowTrailText(
        new Map([
          ["Press", ["n1", "n2"]],
          ["Tick", ["n1"]],
        ]),
      ),
    ).toBe("Flows: Press at n2, Tick at n1");
    expect(flowTrailText(new Map())).toBe("");
  });
});
