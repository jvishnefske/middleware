// The browser path of a control recipe (design.md Q19): the worked
// example's two units, generated and compiled here, run as simulator
// sessions on a SimBus (served phases, distinct node ids), driven by
// protocol-wasm's RecipeRun (recipe-core's engine) through RecipeRunner.
// The three runs reproduce tools/payload-sim/tests/phase.rs p3-p5 on the
// TS/wasm stack: Complete; Hold -> Held -> Restart -> Complete; losing the
// LED unit holds, Restart is refused, Abort ends Aborted.

import { beforeAll, describe, expect, it } from "vitest";
import { EXAMPLE_PLANT, BLINK_THEN_MEASURE } from "../src/model/plantExample";
import { SimBus, defaultNode, keyNode } from "../src/sim/bus";
import { RecipeRunner, buildUnit, operatorButtons } from "../src/sim/recipeRun";
import { ManualClock } from "./clock";
import { protocol } from "./fakeDevice";

const proto = protocol();
let wasm: Record<string, Uint8Array> = {};

beforeAll(async () => {
  for (const u of EXAMPLE_PLANT.physical.units) wasm[u.id] = await buildUnit(EXAMPLE_PLANT, u.id, proto.abi);
}, 60_000);

/** The plant on a bus: LED writes and logs per unit, the POT at 2048. */
function plant() {
  const clock = new ManualClock();
  const bus = new SimBus(proto, clock);
  const led: boolean[] = [];
  const logs: Record<string, string[]> = { "pico-led": [], "pico-sense": [] };
  for (const u of EXAMPLE_PLANT.physical.units) {
    bus.add(u.id, wasm[u.id]!, {
      events: {
        onEvent: (e) => {
          if (e.kind === "write" && u.id === "pico-led") led.push(e.high);
          if (e.kind === "log") logs[u.id]!.push(e.line);
        },
      },
    });
  }
  bus.unit("pico-sense")!.session.setAdc(26, 2048);
  const run = new RecipeRunner(proto, EXAMPLE_PLANT, BLINK_THEN_MEASURE, bus, clock);
  run.start(10);
  const to = (t: number) => clock.advance(t - clock.t);
  const recipeStates = () => run.trace.filter((l) => l.type === "recipe").map((l) => l.text.split(" -> ")[1]);
  return { clock, bus, run, led, logs, to, recipeStates };
}

describe("the simulator serves phases", () => {
  it("gives each unit a distinct node and routes requests by the key's node", () => {
    const { bus } = plant();
    expect(bus.all.map((u) => u.session.nodeId)).toEqual([defaultNode(0), defaultNode(1)]);
    expect(bus.unit("pico-led")!.session.serves).toEqual(["unit/0000000000000001/phase/LED.Blink"]);
    expect(bus.unit("pico-sense")!.session.serves).toEqual(["unit/0000000000000002/phase/ADC.Measure"]);
    expect(keyNode("unit/0000000000000002/phase/ADC.Measure")).toBe("0000000000000002");
    expect(bus.route("unit/0000000000000002/phase/ADC.Measure")?.id).toBe("pico-sense");
    // A malformed body is the payload's InvalidRequest; an unserved key on
    // a known node is NotFound; nobody takes an unknown node.
    expect(bus.request("unit/0000000000000001/phase/LED.Blink", new Uint8Array([9]))?.result).toEqual({ ok: false, error: "InvalidRequest" });
    expect(bus.request("unit/0000000000000001/phase/LED.Other", new Uint8Array())?.result).toEqual({ ok: false, error: "NotFound" });
    expect(bus.request("unit/00000000000000ff/phase/LED.Blink", new Uint8Array())).toBeNull();
    bus.setOnline("pico-led", false);
    expect(bus.request("unit/0000000000000001/phase/LED.Blink", new Uint8Array([9]))).toBeNull();
  });

  it("publishes each unit's state reports on the bus and delivers to other units' subscriptions", () => {
    const clock = new ManualClock();
    const bus = new SimBus(proto, clock);
    const seen: string[] = [];
    bus.onPublish((unit, key) => seen.push(`${unit} ${key}`));
    bus.add("pico-led", wasm["pico-led"]!, { node: "246F280102030000" });
    expect(bus.unit("pico-led")!.session.nodeId).toBe("246f280102030000");
    expect(() => bus.add("other", wasm["pico-led"]!, { node: "246f280102030000" })).toThrow(/already/);
    // init reports Idle; the 1 s heartbeat reports again.
    expect(seen).toEqual(["pico-led unit/246f280102030000/phase/LED.Blink/state"]);
    bus.start(10);
    clock.advance(1_010);
    expect(seen.length).toBeGreaterThanOrEqual(2);
    bus.dispose();
  });
});

describe("the recipe engine in the browser drives the simulated units (payload-sim p3-p5)", () => {
  it("p3: Blink then Measure runs to Complete", () => {
    const p = plant();
    expect(p.run.state).toBe("Idle");
    p.run.operator("Start");
    p.to(3_000);
    expect(p.run.state, p.run.trace.map((l) => `${l.t} ${l.text}`).join("\n")).toBe("Complete");
    expect(p.recipeStates()).toEqual(["Running", "Complete"]);
    expect(p.led).toEqual([false, true, false, true, false]);
    expect(p.run.phases().map((ph) => [ph.unit, ph.state, ph.values])).toEqual([
      ["pico-led", "Idle", expect.any(Array)],
      ["pico-sense", "Idle", [2048, 4]],
    ]);
    expect(p.run.sent).toEqual([
      { unit: "pico-led", command: "Start", outcome: "ok" },
      { unit: "pico-led", command: "Reset", outcome: "ok" },
      { unit: "pico-sense", command: "Start", outcome: "ok" },
      { unit: "pico-sense", command: "Reset", outcome: "ok" },
    ]);
    const measureAt = p.run.trace.find((l) => l.type === "send" && l.text.startsWith("Start -> pico-sense"))!.t;
    expect(measureAt).toBeGreaterThanOrEqual(500);
    expect(p.logs["pico-sense"]).toContain("phase ADC.Measure: Complete");
    // Replies arrive before the reports they cause: nothing is ignored.
    expect(p.run.trace.filter((l) => l.type === "ignored")).toEqual([]);
    expect(p.run.steps()).toEqual([
      { id: "BlinkLed", status: "done" },
      { id: "TakeReading", status: "done" },
    ]);
    expect(p.run.bindings()).toEqual([
      { step: "BlinkLed", unit: "pico-led", em: "LED", phase: "Blink" },
      { step: "TakeReading", unit: "pico-sense", em: "ADC", phase: "Measure" },
    ]);
  });

  it("p4: a Hold mid-Blink holds the batch and Restart finishes it", () => {
    const p = plant();
    p.run.operator("Start");
    p.to(300);
    p.run.operator("Hold");
    p.to(1_000);
    expect(p.run.state).toBe("Held");
    expect(p.run.sent.some((s) => s.unit === "pico-sense")).toBe(false);
    p.run.operator("Restart");
    p.to(4_000);
    expect(p.run.state).toBe("Complete");
    expect(p.recipeStates()).toEqual(["Running", "Holding", "Held", "Restarting", "Running", "Complete"]);
    expect(p.run.sent.filter((s) => s.unit === "pico-led").map((s) => s.command)).toEqual(["Start", "Hold", "Restart", "Reset"]);
  });

  it("p5: losing the LED unit holds, Restart is refused, Abort ends Aborted", () => {
    const p = plant();
    p.run.operator("Start");
    p.to(100);
    p.run.lose("pico-led");
    p.to(200);
    p.run.operator("Restart");
    p.to(300);
    p.run.operator("Abort");
    p.to(2_000);
    const texts = p.run.trace.map((l) => l.text);
    expect(texts).toContain("unit pico-led left the network (resumable: hold)");
    expect(texts).toContain("Restart refused: unit pico-led is offline");
    expect(p.run.state).toBe("Aborted");
    expect(p.run.trace.some((l) => l.t >= 100 && l.type === "send" && l.text.includes("-> pico-led/"))).toBe(false);
    // The batch is held, the equipment is not: Blink finished by itself.
    expect(p.logs["pico-led"]).toContain("phase LED.Blink: Complete");
  });
});

describe("operator buttons", () => {
  it("are enabled exactly where the phase model accepts the command", () => {
    const m = proto.phaseModel();
    for (const [i, state] of m.states.entries()) {
      const b = operatorButtons(m, state);
      expect(b.map((x) => x.command)).toEqual(m.commands);
      expect(b.map((x) => x.enabled)).toEqual(m.commands.map((_, c) => m.next[i]![c] !== null));
    }
    expect(operatorButtons(m, "Idle").filter((b) => b.enabled).map((b) => b.command)).toEqual(["Start"]);
    expect(operatorButtons(m, "Running").filter((b) => b.enabled).map((b) => b.command)).toEqual(["Hold", "Stop", "Abort"]);
    expect(operatorButtons(m, "Held").filter((b) => b.enabled).map((b) => b.command)).toEqual(["Restart", "Stop", "Abort"]);
    expect(operatorButtons(m, "nonsense").some((b) => b.enabled)).toBe(false);
  });
});
