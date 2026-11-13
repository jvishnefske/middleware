// Flows -> the shared statement IR -> AssemblyScript (flowGen.ts via
// generator.ts): the printer, lowering of each node kind, where each entry
// places its body, no flows = the code from before, bad flows reported and
// never crashing, the pipeline's "flows" stage, and the acceptance test:
// the "Button toggles LED mode" example as flows produces the same
// simulator trace as the Blocks version.

import { describe, expect, it } from "vitest";
import { exampleWorkspace } from "../src/blocks/examples";
import { generate } from "../src/blocks/generator";
import { buildPayload, flowIssueText } from "../src/blocks/pipeline";
import { type LowerHost, type Stmt, line, lowerAction, lowerExpr, printStmts } from "../src/blocks/stmt";
import type { BlockJson, WorkspaceJson } from "../src/blocks/workspace";
import { type Flow, type FlowIssue, type FlowNode, connect, flowLog, flowPinNeeds, parseFlowLog } from "../src/model/flows";
import { type Machine, machinePinNeeds } from "../src/model/machines";
import { EMPTY_PINS, type PinTable, normalizeBuses, row, withNeeds } from "../src/model/pins";
import type { SimEvent } from "../src/protocol/wasm";
import { SimSession } from "../src/sim/session";
import { ManualClock } from "./clock";
import { protocol } from "./fakeDevice";

const proto = protocol();
const ws = (...blocks: BlockJson[]): WorkspaceJson => ({ blocks: { languageVersion: 0, blocks } });
const pins: PinTable = normalizeBuses({
  rows: [row("gpio-out", "LED", 25), { ...row("gpio-in", "BUTTON", 15), pull: "up" }],
  buses: [],
});
const mode = (): Machine => structuredClone(exampleWorkspace("mode")!.machines![0] as Machine);

/** A flow from nodes (positions filled in) wired by [from, port, to]. */
function flow(id: string, nodes: Omit<FlowNode, "x" | "y">[], wires: [string, string, string][]): Flow {
  let f: Flow = { kind: "flow", id, name: "", nodes: nodes.map((n, i) => ({ ...n, x: 40, y: 40 + 100 * i }) as FlowNode), wires: [] };
  for (const [a, p, b] of wires) f = connect(f, a, p, b);
  return f;
}
const init = (id = "e"): Omit<FlowNode, "x" | "y"> => ({ id, kind: "entry", props: { on: "init" } });
const log = (id: string, text = id): Omit<FlowNode, "x" | "y"> => ({ id, kind: "action", props: { kind: "log", text } });

/** The text between `start` and the first line equal to `end` after it. */
function block(src: string, start: string, end = "}"): string {
  const lines = src.split("\n");
  const i = lines.findIndex((l) => l.startsWith(start));
  if (i < 0) throw new Error(`no ${start} in\n${src}`);
  const j = lines.findIndex((l, k) => k > i && l === end);
  return lines.slice(i, j + 1).join("\n");
}

describe("the statement IR", () => {
  it("prints nested if / else if / else and for loops with two spaces per level, iteratively", () => {
    const s: Stmt[] = [
      line("a();"),
      {
        kind: "if",
        arms: [
          { cond: "x", body: [{ kind: "for", v: "i0", count: 3, body: [line("b();")] }] },
          { cond: "y", body: [] },
        ],
        else: [line("c();")],
      },
      { kind: "if", arms: [], else: [line("d();")] },
    ];
    const out: string[] = [];
    printStmts(s, "  ", out);
    expect(out).toEqual([
      "  a();",
      "  if (x != 0) {",
      "    for (let i0: i32 = 0; i0 < 3; i0++) {",
      "      b();",
      "    }",
      "  } else if (y != 0) {",
      "  } else {",
      "    c();",
      "  }",
      "  d();",
    ]);
    // Deep nesting prints without recursion limits.
    let deep: Stmt = line("x();");
    for (let k = 0; k < 5000; k++) deep = { kind: "if", arms: [{ cond: "1", body: [deep] }], else: null };
    const big: string[] = [];
    printStmts([deep], "", big);
    expect(big).toHaveLength(10_001);
  });

  it("lowers the model's expressions and actions, state tests and raises through the host", () => {
    const fails: string[] = [];
    const host: LowerHost = {
      str: (t) => `S_${t.length}`,
      use: () => {},
      helper: () => {},
      pin: (label) => (label === "LED" ? { ident: "PIN_LED" } : { problem: `no pin ${label}` }),
      keyProblem: () => null,
      inState: (m, st) => (m === "Mode" ? { code: `<i32>(sm0 == SM0_${st})` } : { problem: `no machine ${m}` }),
      raise: (m, ev) => (m === "Mode" ? { code: `sm0_raise(EV0_${ev});` } : { problem: `no machine ${m}` }),
    };
    const fail = (m: string) => fails.push(m);
    expect(lowerExpr({ kind: "and", a: { kind: "state", machine: "Mode", state: "On" }, b: { kind: "not", a: { kind: "pin", label: "LED" } } }, host, fail)).toBe(
      "<i32>(<i32>(sm0 == SM0_On) != 0 && <i32>(mw_gpio_read(PIN_LED) == 0) != 0)",
    );
    expect(lowerAction({ kind: "raise", machine: "Mode", event: "GO" }, host, fail, null)).toEqual([line("sm0_raise(EV0_GO);")]);
    expect(lowerAction({ kind: "raise", event: "GO" }, host, fail, null)).toEqual([]);
    expect(lowerAction({ kind: "raise", event: "GO" }, host, fail, "Mode")).toEqual([line("sm0_raise(EV0_GO);")]);
    expect(lowerExpr({ kind: "state", machine: "Door", state: "Open" }, host, fail)).toBe("0");
    expect(fails).toEqual(["raise GO needs a machine", "no machine Door"]);
  });
});

describe("flows -> AssemblyScript", () => {
  it("lowers branch, repeat and sequence onto the shared if / for, logging each node outside repeat bodies", () => {
    const f = flow(
      "Demo",
      [
        init(),
        { id: "s", kind: "sequence", props: { steps: 2 } },
        { id: "b", kind: "branch", props: { cond: { kind: "cmp", op: "eq", a: { kind: "pin", label: "BUTTON" }, b: { kind: "const", value: 0 } } } },
        { id: "on", kind: "action", props: { kind: "write", label: "LED", level: 1 } },
        { id: "off", kind: "action", props: { kind: "write", label: "LED", level: 0 } },
        { id: "r", kind: "repeat", props: { times: 3 } },
        { id: "t", kind: "action", props: { kind: "toggle", label: "LED" } },
        log("done"),
      ],
      [
        ["e", "next", "s"],
        ["s", "1", "b"],
        ["b", "then", "on"],
        ["b", "else", "off"],
        ["s", "2", "r"],
        ["r", "body", "t"],
        ["r", "next", "done"],
      ],
    );
    const g = generate(ws(), "esp-base", pins, [], [f]);
    expect(g.errors).toEqual([]);
    expect(g.source).toContain(`// "flow Demo: e"`);
    const init_ = block(g.source, "export function init(): void {");
    const body = init_.split("\n").slice(init_.split("\n").findIndex((l) => l.includes("mw_log")));
    expect(body.map((l) => l.replace(/S\d+/g, "S"))).toEqual([
      "  mw_log(S, S_LEN);", // e
      "  mw_log(S, S_LEN);", // b
      "  if (<i32>(mw_gpio_read(PIN_BUTTON) == 0) != 0) {",
      "    mw_log(S, S_LEN);", // on
      "    pinWrite(PIN_LED, 1);",
      "  } else {",
      "    mw_log(S, S_LEN);", // off
      "    pinWrite(PIN_LED, 0);",
      "  }",
      "  mw_log(S, S_LEN);", // r
      "  for (let i0: i32 = 0; i0 < 3; i0++) {",
      "    pinToggle(PIN_LED);",
      "  }",
      "  mw_log(S, S_LEN);", // done
      "  mw_log(S, S_LEN);",
      "}",
    ]);
    // Nodes inside the repeat body and the sequence itself log nothing.
    for (const n of ["e", "b", "on", "off", "r", "done"]) expect(g.source).toContain(`// "${flowLog("Demo", n)}"`);
    for (const n of ["t", "s"]) expect(g.source).not.toContain(`// "${flowLog("Demo", n)}"`);
  });

  it("places each entry's body where its hat block's would go", async () => {
    const m = mode();
    const flows = [
      flow("Boot", [init(), log("hi")], [["e", "next", "hi"]]),
      flow("Tick", [{ id: "e", kind: "entry", props: { on: "every", ms: 500 } }, log("t")], [["e", "next", "t"]]),
      flow("Msg", [{ id: "e", kind: "entry", props: { on: "message", key: "app/in" } }, log("m")], [["e", "next", "m"]]),
      flow("Edge", [{ id: "e", kind: "entry", props: { on: "pin-edge", label: "BUTTON", edge: "fall" } }, { id: "p", kind: "action", props: { kind: "raise", machine: "Mode", event: "PRESS" } }], [["e", "next", "p"]]),
      flow("Lit", [{ id: "e", kind: "entry", props: { on: "entered", state: "Mode.On" } }, log("lit")], [["e", "next", "lit"]]),
    ];
    const g = generate(ws(), "esp-base", pins, [m], flows);
    expect(g.errors).toEqual([]);
    expect(g.exports).toEqual(["init", "tick", "mw_alloc", "on_message", "on_irq"]);
    expect(g.source).toContain("  mw_set_tick(500);");
    expect(g.source).toContain("  mw_gpio_irq(15, 2);");
    expect(block(g.source, "export function on_irq(")).toContain("  if (kind == 1 && source == 15 && value == 0) {\n    mw_log(");
    expect(block(g.source, "export function on_irq(")).toContain("    sm0_raise(EV0_PRESS);");
    expect(block(g.source, "function sm0_enter_On(): void {")).toMatch(/pinWrite\(PIN_LED, 1\);\n {2}mw_log\(S\d+, S\d+_LEN\);\n {2}mw_log\(/);
    expect(block(g.source, "export function on_message(")).toContain("  if (slot == slot0) {\n    mw_log(");
    const r = await buildPayload(ws(), "esp-base", proto.abi, { pins, issues: [], machines: [m], machineIssues: [], flows, flowIssues: [] });
    if (!r.ok) throw new Error(`${r.stage}: ${r.errors.join("; ")}\n${r.source}`);
  });

  it("generates byte-identical Blocks code when there are no flows", () => {
    const e = exampleWorkspace("mode")!;
    const p = withNeeds(EMPTY_PINS, "pico2", e.needs);
    expect(generate(e.workspace, "esp-base", p, e.machines, []).source).toBe(generate(e.workspace, "esp-base", p, e.machines).source);
  });

  it("reports bad flows, leaves the bad parts out and still emits valid code", async () => {
    const twoIn = flow("Merge", [init(), log("a"), log("b"), log("c")], [["e", "next", "a"]]);
    const merged: Flow = {
      ...twoIn,
      wires: [...twoIn.wires, { from: { node: "a", port: "next" }, to: { node: "c", port: "in" } }, { from: { node: "e", port: "next" }, to: { node: "c", port: "in" } }],
    };
    const flows = [
      flow("NoEntry", [log("a")], []),
      flow("bad name", [init()], []),
      merged,
      flow("Raise", [init(), { id: "r", kind: "action", props: { kind: "raise", event: "GO" } }], [["e", "next", "r"]]),
      flow("Every", [{ id: "e", kind: "entry", props: { on: "every", ms: 1 } }], []),
      flow("Gone", [{ id: "e", kind: "entry", props: { on: "entered", state: "Nope.On" } }], []),
    ];
    const g = generate(ws(), "esp-base", pins, [], flows);
    expect(g.errors.map((e) => e.message)).toEqual([
      "flow NoEntry: has no entry",
      'flow 2: the name "bad name" must start with a letter and hold only letters, digits and _',
      "flow Raise node r: raise GO needs a machine",
      "flow Every: the period must be a whole number from 10 to 2147483647 ms",
      'flow Gone: "Nope.On" is not a state of a state machine that gets code',
    ]);
    // The second wire into c is not followed (the first wire per port wins).
    expect(g.source).toContain("export function init(): void {");
    const r = await buildPayload(ws(), "esp-base", proto.abi, { pins, issues: [], flows: [merged], flowIssues: [] });
    expect(r.ok).toBe(true);
  });

  it("stops the build at stage flows on a flow error and never on a warning", async () => {
    const f = flow("Main", [init(), log("a")], [["e", "next", "a"]]);
    const issue = (severity: FlowIssue["severity"]): FlowIssue => ({ flow: 0, node: 1, wire: null, severity, message: "something" });
    const bad = await buildPayload(ws(), "esp-base", proto.abi, { pins, issues: [], flows: [f], flowIssues: [issue("error")] });
    expect(bad).toMatchObject({ ok: false, stage: "flows", errors: ["Flows Main node a: something"] });
    const ok = await buildPayload(ws(), "esp-base", proto.abi, { pins, issues: [], flows: [f], flowIssues: [issue("warning")] });
    expect(ok.ok).toBe(true);
    expect(flowIssueText({ ...issue("error"), node: null, wire: 0 }, [f])).toBe("Flows Main wire e.next -> a: something");
    expect(flowIssueText({ ...issue("error"), flow: 3, node: null }, [f])).toBe("Flows flow 4: something");
  });
});

function sim(wasm: Uint8Array) {
  const clock = new ManualClock();
  const events: SimEvent[] = [];
  const errors: string[] = [];
  const s = new SimSession(proto, wasm, { onEvent: (e) => events.push(e), onError: (m) => errors.push(m) }, clock);
  s.start();
  return { s, clock, events, errors };
}

/** The trace a user sees: every event (logs, publishes, pin setup and
 * writes, interrupts), with the flows' own `flow <id>: <node>` lines kept
 * apart, and the LED level after each step. */
async function trace(exampleId: string, target: "pico2" | "esp32", led: number, button: number) {
  const e = exampleWorkspace(exampleId)!;
  const machines = e.machines ?? [];
  const flows = e.flows ?? [];
  const p = withNeeds(EMPTY_PINS, target, [...e.needs, ...machines.flatMap(machinePinNeeds), ...flowPinNeeds(flows)]);
  const flowIssues = proto.checkFlows(flows, machines, p.rows);
  expect(flowIssues).toEqual([]);
  const r = await buildPayload(e.workspace, "esp-base", proto.abi, { pins: p, issues: [], machines, machineIssues: [], flows, flowIssues });
  if (!r.ok) throw new Error(`${r.stage}: ${r.errors.join("; ")}`);
  const { s, clock, events, errors } = sim(r.wasm);
  const levels: (boolean | undefined)[] = [];
  const step = (ms: number) => {
    clock.advance(ms);
    levels.push(s.pins.get(led)?.high);
  };
  const press = () => {
    s.setInput(button, false);
    step(20);
    s.setInput(button, true);
    step(20);
  };
  step(20);
  press();
  step(500);
  press();
  for (let i = 0; i < 4; i++) step(250);
  press();
  step(1000);
  s.dispose();
  const flowLines = events.flatMap((ev) => (ev.kind === "log" && parseFlowLog(ev.line) ? [ev.line] : []));
  const rest = events.filter((ev) => !(ev.kind === "log" && parseFlowLog(ev.line)));
  return { rest, levels, flowLines, errors };
}

describe.runIf(proto.hasSimulator)("the mode example as flows", () => {
  for (const [target, led, button] of [
    ["pico2", 25, 15],
    ["esp32", 4, 0],
  ] as const) {
    it(`produces the same simulator trace as the Blocks version on ${target}`, async () => {
      const blocks = await trace("mode", target, led, button);
      const flows = await trace("mode-flows", target, led, button);
      expect(blocks.errors).toEqual([]);
      expect(flows.errors).toEqual([]);
      expect(blocks.flowLines).toEqual([]);
      expect(flows.rest).toEqual(blocks.rest);
      expect(flows.levels).toEqual(blocks.levels);
      // The trace is the mode example's: Off -> On -> Blink -> Off.
      const logs = blocks.rest.flatMap((ev) => (ev.kind === "log" ? [ev.line] : []));
      expect(logs).toEqual(["sm Mode: Off", "sm Mode: Off -> On on PRESS", "sm Mode: On -> Blink on PRESS", "mode: blinking", "sm Mode: Blink -> Off on PRESS"]);
      // And the flows said which of their nodes ran.
      expect(flows.flowLines.slice(0, 2)).toEqual([flowLog("Press", "n1"), flowLog("Press", "n2")]);
      expect(flows.flowLines).toContain(flowLog("Blinking", "n2"));
      expect(flows.flowLines.filter((l) => l.startsWith("flow Tick"))).toHaveLength(2 * Math.floor((20 + 40 + 500 + 40 + 1000 + 40 + 1000) / 250));
    });
  }
});
