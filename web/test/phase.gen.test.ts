// The S88 phase server (blocks/phaseGen.ts via generator.ts): the table
// it switches on is protocol-wasm's phaseModel(), units without phases
// generate exactly what they did, the generated server's shape, bad
// phase actions are reported, and the wasm fixtures the payload-sim tests
// run (tools/payload-sim/tests/phase.rs: the 77 cells of the phase model
// and the worked example end to end with recipe-core's engine) are what
// this generator produces today. `UPDATE_FIXTURES=1 npx vitest run
// test/phase.gen.test.ts` rewrites them.

import { mkdirSync, readFileSync, writeFileSync, existsSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";
import { generate } from "../src/blocks/generator";
import { type PhaseSpec } from "../src/blocks/phaseGen";
import { buildPayload } from "../src/blocks/pipeline";
import type { Flow, FlowNode } from "../src/model/flows";
import { flowsOf, profileOfModel, workspaceOf } from "../src/model/payload";
import { phaseDecls, phaseSpecs, unitPayload } from "../src/model/plant";
import { EXAMPLE_PLANT } from "../src/model/plantExample";
import { row } from "../src/model/pins";
import { ACTING_STATES, PHASE_COMMANDS, PHASE_NEXT, PHASE_STATES } from "../src/model/procedural";
import { protocol } from "./fakeDevice";

const proto = protocol();
const FIXTURES = join(dirname(fileURLToPath(import.meta.url)), "../../tools/payload-sim/tests/fixtures/phase");
const empty = { blocks: { languageVersion: 0, blocks: [] } };

function chain(id: string, entry: FlowNode["props"], ...actions: FlowNode["props"][]): Flow {
  const nodes = [entry, ...actions].map((props, i) => ({ id: `n${i + 1}`, kind: i === 0 ? "entry" : "action", x: 40, y: 40 + 100 * i, props }) as FlowNode);
  const wires = nodes.slice(1).map((n, i) => ({ from: { node: `n${i + 1}`, port: "next" }, to: { node: n.id, port: "in" } }));
  return { kind: "flow", id, name: "", nodes, wires };
}

/** The 77-cell fixture: one phase X.P (one parameter a in 0..10) whose
 * acting states all have bodies that never finish, so every state can be
 * held at; a message on t/done finishes the current one. */
const MATRIX_PHASES: PhaseSpec[] = [{ em: "X", id: "P", params: [{ id: "a", lo: 0, hi: 10 }] }];
const MATRIX_FLOWS: Flow[] = [
  ...ACTING_STATES.map((st) => chain(`B_${st}`, { on: "phase", phase: "X.P", state: st }, { kind: "log", text: `body ${st}` })),
  chain("Done", { on: "message", key: "t/done" }, { kind: "phase-done", phase: "X.P" }),
];

async function build(flows: readonly Flow[], phases: readonly PhaseSpec[], pins = { rows: [row("gpio-out", "LED", 25)], buses: [] }) {
  const r = await buildPayload(empty, "esp-base", proto.abi, { pins, issues: [], flows, flowIssues: [], phases });
  if (!r.ok) throw new Error(`${r.stage}: ${r.errors.join("; ")}\n${r.source}`);
  return r;
}

function fixture(name: string, wasm: Uint8Array): void {
  const path = join(FIXTURES, `${name}.wasm`);
  if (process.env.UPDATE_FIXTURES === "1" || !existsSync(path)) {
    mkdirSync(FIXTURES, { recursive: true });
    writeFileSync(path, wasm);
  }
  expect(Buffer.from(readFileSync(path)).equals(Buffer.from(wasm)), `${path} is stale: run UPDATE_FIXTURES=1 npx vitest run test/phase.gen.test.ts`).toBe(true);
}

describe("the phase server", () => {
  it("switches on the phase model's table", () => {
    const m = proto.phaseModel();
    const code = (name: string | null) => (name === null ? -1 : PHASE_STATES.indexOf(name as (typeof PHASE_STATES)[number]));
    expect(m.next.map((row) => row.map(code))).toEqual(PHASE_NEXT);
    expect(m.commands).toEqual([...PHASE_COMMANDS]);
    expect(PHASE_NEXT).toHaveLength(11);
  });

  it("leaves a unit without phases exactly as before", () => {
    for (const f of [[], MATRIX_FLOWS.slice(-1)]) {
      expect(generate(empty, "esp-base", undefined, [], f, []).source).toBe(generate(empty, "esp-base", undefined, [], f).source);
    }
  });

  it("generates a bounded command switch, serve per phase, reports and the heartbeat", () => {
    const g = generate(empty, "esp-base", { rows: [row("gpio-out", "LED", 25)], buses: [] }, [], MATRIX_FLOWS, MATRIX_PHASES);
    expect(g.errors).toEqual([]);
    expect(g.imports).toEqual(["log", "publish", "subscribe", "set_tick", "gpio_output", "gpio_write", "serve", "node_id"]);
    expect(g.exports).toEqual(["init", "tick", "mw_alloc", "on_message", "on_request"]);
    expect(g.source).toContain("const PH0_KEY_LEN: i32 = 31;");
    expect(g.source).toContain("    case 1: return c == 2 ? 3 : c == 4 ? 6 : c == 5 ? 8 : c == 0 ? 2 : -1;");
    expect(g.source).toContain("    phSet(PH_SLOT, 0, mw_serve(PH0_KEY, PH0_KEY_LEN));");
    expect(g.source).toContain("  if (phBeat >= 1000) {");
    expect(g.source).toContain("  return len < 0 || len > 1040 ? 0 : RQ;");
    // Every export settles pending Dones last.
    for (const fn of ["init(): void", "tick(nowMs: i32): void", "on_message(slot: i32, ptr: usize, len: i32): void"]) {
      const body = g.source.split(`export function ${fn} {`)[1]?.split("\n}\n")[0] ?? "";
      expect(body.trimEnd().endsWith("phSettle();"), fn).toBe(true);
    }
  });

  it("reports phase actions on unknown phases, parameters and run counts outside the body", () => {
    const flows = [
      chain("A", { on: "init" }, { kind: "phase-done", phase: "X.Q" }, { kind: "phase-done", phase: "X.P", after: 2 }, { kind: "phase-report", phase: "X.P", values: [{ kind: "param", param: "b" }] }),
      chain("B", { on: "phase", phase: "Y.Z", state: "running" }, { kind: "log", text: "x" }),
    ];
    const g = generate(empty, "esp-base", undefined, [], flows, MATRIX_PHASES);
    expect(g.errors.map((e) => e.message)).toEqual([
      'flow A node n2: "X.Q" is not a phase of this unit that gets code',
      "flow A node n3: counting runs only works in the phase's own body flow",
      "flow A node n4: phase X.P has no parameter b",
      'flow B: "Y.Z" is not a phase of this unit that gets code',
    ]);
    // Without phases a phase entry or action is an error, not a crash.
    const none = generate(empty, "esp-base", undefined, [], flows);
    expect(none.errors.map((e) => e.message)).toEqual(['flow A node n2: "X.Q" is not a phase of this unit', 'flow A node n3: "X.P" is not a phase of this unit', 'flow A node n4: "X.P" is not a phase of this unit', 'flow B: "Y.Z" is not a phase of this unit']);
  });

  it("the worked example's units check clean against their phases", () => {
    for (const u of EXAMPLE_PLANT.physical.units) {
      const m = unitPayload(EXAMPLE_PLANT, u.id)!;
      expect(proto.checkFlows(flowsOf(m), m.machines, m.pins.rows, phaseDecls(EXAMPLE_PLANT, u.id))).toEqual([]);
    }
  });

  it("compiles the payload-sim fixtures: the 77-cell matrix unit and the worked example's two units", async () => {
    fixture("matrix", (await build(MATRIX_FLOWS, MATRIX_PHASES)).wasm);
    for (const u of EXAMPLE_PLANT.physical.units) {
      const m = unitPayload(EXAMPLE_PLANT, u.id)!;
      const r = await buildPayload(workspaceOf(m), profileOfModel(m), proto.abi, {
        pins: m.pins,
        issues: [],
        machines: m.machines,
        machineIssues: [],
        flows: flowsOf(m),
        flowIssues: [],
        phases: phaseSpecs(EXAMPLE_PLANT, u.id),
      });
      if (!r.ok) throw new Error(`${u.id} ${r.stage}: ${r.errors.join("; ")}`);
      expect(r.report.imports).toContain("mw.serve");
      fixture(u.id, r.wasm);
    }
  });
});
