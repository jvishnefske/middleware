// The plant document (IR v4, plant.ts): the S88 split into physical,
// procedural and recipe models, its strict shape and reference checks, and
// the lossless migration of version 1-3 payloads to a one-unit plant whose
// unit payload generates byte-for-byte the same code.

import { describe, expect, it } from "vitest";
import { exampleWorkspace } from "../src/blocks/examples";
import { MODE_FLOWS } from "../src/blocks/examples/mode.flows";
import { MODE_MACHINE } from "../src/blocks/examples/mode.machine";
import { generate } from "../src/blocks/generator";
import { type PayloadModel, flowsOf, makeModel, profileOfModel, serializeModel, workspaceOf } from "../src/model/payload";
import {
  DEFAULT_UNIT_ID,
  PLANT_VERSION,
  allocationUnits,
  makePlant,
  parsePlant,
  plainPayloadOf,
  plantFromPayload,
  plantProblem,
  serializePlant,
  unitPayload,
} from "../src/model/plant";
import { EXAMPLE_PLANT } from "../src/model/plantExample";
import { boardDefaults, row } from "../src/model/pins";
import { ACTING_STATES, PHASE_COMMANDS, PHASE_STATES } from "../src/model/procedural";

const blink = exampleWorkspace("blink")!.workspace;
const mode = exampleWorkspace("mode")!.workspace;
const code = (m: PayloadModel) => generate(workspaceOf(m), profileOfModel(m), m.pins, m.machines, flowsOf(m)).source;
const valid = () => JSON.parse(serializePlant(EXAMPLE_PLANT)) as Record<string, any>;

describe("plant model (IR v4)", () => {
  it("the worked example is a valid plant and round-trips through JSON as a copy", () => {
    expect(plantProblem(valid())).toBeNull();
    const r = parsePlant(serializePlant(EXAMPLE_PLANT), "pico2");
    expect(r).toEqual({ ok: true, plant: EXAMPLE_PLANT, migrated: false });
    if (!r.ok) return;
    expect(r.plant.physical.units[0]).not.toBe(EXAMPLE_PLANT.physical.units[0]);
    expect(r.plant.procedural.phases[0].ranges).not.toBe(EXAMPLE_PLANT.procedural.phases[0].ranges);
    expect(JSON.parse(serializePlant(r.plant))).toMatchObject({ format: "middleware-payload", version: PLANT_VERSION });
  });

  it("migrates version-3 payloads to one unit whose payload generates the same code byte for byte", () => {
    const models = [
      makeModel("pico2", blink, boardDefaults("pico2")),
      makeModel("pico2", mode, { rows: [row("gpio-out", "LED", 25), { ...row("gpio-in", "BUTTON", 15), pull: "up" }], buses: [] }, [MODE_MACHINE], MODE_FLOWS),
      makeModel("esp32", blink, boardDefaults("esp32"), [MODE_MACHINE]),
    ];
    for (const m of models) {
      const r = parsePlant(serializeModel(m), "pico2");
      expect(r.ok && r.migrated).toBe(true);
      if (!r.ok) return;
      const [u] = r.plant.physical.units;
      expect(u).toMatchObject({ id: DEFAULT_UNIT_ID, class: "Unit", node: null, ems: [] });
      expect(r.plant.procedural).toEqual({ classes: [], phases: [] });
      expect(r.plant.recipes).toEqual([]);
      const back = unitPayload(r.plant, DEFAULT_UNIT_ID)!;
      expect(back).toEqual(m);
      expect(plainPayloadOf(r.plant)).toEqual(m);
      expect(code(back)).toBe(code(m));
      // And through a saved v4 file.
      const again = parsePlant(serializePlant(r.plant), "esp32");
      expect(again.ok && code(unitPayload(again.plant, DEFAULT_UNIT_ID)!)).toBe(code(m));
    }
  });

  it("migrates version-1 and -2 files and legacy Blocks exports through the version-3 reader", () => {
    const v1 = { ...JSON.parse(serializeModel(makeModel("pico2", blink, boardDefaults("pico2")))), version: 1 };
    const r1 = parsePlant(JSON.stringify(v1), "esp32");
    expect(r1.ok && unitPayload(r1.plant, DEFAULT_UNIT_ID)).toEqual(makeModel("pico2", blink, boardDefaults("pico2")));
    const legacy = { format: "middleware-blocks", version: 1, profile: "rp-base", workspace: blink };
    const r2 = parsePlant(JSON.stringify(legacy), "pico2");
    expect(r2.ok && r2.migrated).toBe(true);
    expect(r2.ok && workspaceOf(unitPayload(r2.plant, DEFAULT_UNIT_ID)!)).toEqual(blink);
    expect(parsePlant("{", "pico2")).toEqual({ ok: false, error: expect.stringMatching(/^not JSON/) });
  });

  it("each unit's payload is its own board, control modules, logic and machines", () => {
    const led = unitPayload(EXAMPLE_PLANT, "pico-led")!;
    expect(led.board).toBe("pico2");
    expect(led.pins.rows.map((r) => r.label)).toEqual(["LED"]);
    expect(flowsOf(led).map((f) => f.id)).toEqual(["BlinkRun"]);
    expect(unitPayload(EXAMPLE_PLANT, "nope")).toBeNull();
    expect(plainPayloadOf(EXAMPLE_PLANT)).toBeNull();
    expect(code(led)).toContain("PIN_LED");
  });

  it("lists allocation units with ranges in class parameter order", () => {
    const units = allocationUnits(EXAMPLE_PLANT, (u) => u.id !== "esp-sense", (ph) => (ph.class === "Blink" ? 300 : 100));
    expect(units).toEqual([
      { id: "pico-led", class: "LedUnit", online: true, phases: [{ em: "LED", name: "Blink", class: "Blink", ranges: [[50, 1000], [1, 10]], runMs: 300 }] },
      { id: "esp-sense", class: "SensorUnit", online: false, phases: [{ em: "ADC", name: "Measure", class: "Measure", ranges: [[1, 8]], runMs: 100 }] },
    ]);
  });

  it("mirrors the Rust phase model's names", () => {
    expect(PHASE_STATES).toHaveLength(11);
    expect(PHASE_COMMANDS).toEqual(["Start", "Hold", "Restart", "Stop", "Abort", "Reset"]);
    expect(ACTING_STATES).toEqual(["running", "holding", "restarting", "stopping", "aborting", "resetting"]);
  });

  it("rejects bad shapes and dangling references, naming the place", () => {
    const u0 = (v: Record<string, any>) => v.physical.units[0];
    const ph0 = (v: Record<string, any>) => v.procedural.phases[0];
    const cases: [(v: Record<string, any>) => void, string][] = [
      [(v) => (v.version = 5), "version must be 4"],
      [(v) => (v.extra = 1), 'unknown field "extra"'],
      [(v) => (v.physical.units = []), "physical.units must be a list of 1 to 8 units"],
      [(v) => (u0(v).id = "1st"), "physical.units[0].id must start with a letter and use only letters, digits, _ or - (at most 24)"],
      [(v) => (v.physical.units[1].id = "PICO-LED"), "physical.units[1].id repeats an earlier unit's (ignoring case)"],
      [(v) => (u0(v).class = "led unit"), "physical.units[0].class must be an identifier (a letter, then letters, digits or _; at most 24)"],
      [(v) => (u0(v).node = "ABCDEF0123456789"), "physical.units[0].node must be 16 lowercase hex digits or null"],
      [(v) => (u0(v).board = "arduino"), 'physical.units[0].board "arduino" is not a known board'],
      [(v) => (u0(v).cms.rows[0].pin = 99), "physical.units[0].cms.rows[0].pin must be a whole number from 0 to 63, or null"],
      [(v) => (u0(v).logic = []), "physical.units[0].logic must hold one Blocks workspace and at most 8 flows"],
      [(v) => (u0(v).ems[0].cms = ["BUTTON"]), 'physical.units[0].ems[0].cms names "BUTTON", which is not in the unit\'s pin table'],
      [(v) => u0(v).ems.push({ id: "led", name: "", cms: [] }), "physical.units[0].ems[1].id repeats an earlier equipment module's (ignoring case)"],
      [(v) => u0(v).ems.push({ id: "Other", name: "", cms: ["LED"] }), 'physical.units[0].ems[1].cms names "LED", which equipment module "LED" already owns'],
      [(v) => (v.procedural.classes[0].params[0].min = 1.5), "procedural.classes[0].params[0].min and .max must be whole 32-bit numbers"],
      [(v) => v.procedural.classes.push({ id: "blink", name: "", params: [] }), "procedural.classes[2].id repeats an earlier class's (ignoring case)"],
      [(v) => (ph0(v).unit = "ghost"), 'procedural.phases[0].unit "ghost" is not a unit of the plant'],
      [(v) => (ph0(v).em = "Lamp"), 'procedural.phases[0].em "Lamp" is not an equipment module of unit "pico-led"'],
      [(v) => (ph0(v).class = "Spin"), 'procedural.phases[0].class "Spin" is not a phase class'],
      [(v) => delete ph0(v).ranges.times, 'procedural.phases[0].ranges needs a range for Blink\'s parameter "times"'],
      [(v) => (ph0(v).ranges.period_ms = [5, 100]), "procedural.phases[0].ranges.period_ms 5..100 is outside Blink's 10..10000"],
      [(v) => (ph0(v).ranges.speed = [1, 2]), "procedural.phases[0].ranges.speed is not a parameter of Blink"],
      [(v) => (ph0(v).ranges.times = [5, 1]), "procedural.phases[0].ranges.times has its minimum above its maximum"],
      [(v) => (ph0(v).bodies = { pausing: "BlinkRun" }), "procedural.phases[0].bodies.pausing is not an acting state (one of running, holding, restarting, stopping, aborting, resetting)"],
      [(v) => (ph0(v).bodies.running = "Nope"), 'procedural.phases[0].bodies.running names flow "Nope", which unit "pico-led" does not have'],
      [(v) => (ph0(v).bodies.holding = "BlinkRun"), 'procedural.phases[0].bodies.holding uses flow "BlinkRun", already the body of LED.Blink running'],
      [(v) => v.procedural.phases.push({ ...ph0(v), id: "blink" }), 'procedural.phases[2] repeats phase LED.blink of unit "pico-led" (ignoring case)'],
      [(v) => (v.recipes[0].steps[0].params.times = "3"), "recipes[0].steps[0].params.times must be a whole 32-bit number"],
      [(v) => (v.recipes[0].steps[0].x = -1), "recipes[0].steps[0].x and .y must be whole numbers from 0 to 4000"],
      [(v) => (v.recipes[0].transitions[0].from = "BlinkLed"), "recipes[0].transitions[0].from and .to must be lists of at most 16 step ids"],
      [(v) => (v.recipes[0].steps = Array(17).fill(v.recipes[0].steps[0])), "recipes[0].steps must be a list of at most 16"],
      [(v) => (v.recipes[0].colour = 1), 'recipes[0] has an unknown field "colour"'],
      [(v) => v.recipes.push({ ...v.recipes[0], id: "blinkthenmeasure" }), "recipes[1].id repeats an earlier recipe's (ignoring case)"],
    ];
    for (const [mutate, error] of cases) {
      const v = valid();
      mutate(v);
      expect(plantProblem(v)).toEqual(error);
      expect(parsePlant(JSON.stringify(v), "pico2")).toEqual({ ok: false, error: v.version === 4 ? `not a plant: ${error}` : expect.any(String) });
    }
  });

  it("a payload with no logic beyond its own still makes a plant of one unit", () => {
    const p = plantFromPayload(makeModel("esp32", {}));
    expect(plantProblem(JSON.parse(serializePlant(p)))).toBeNull();
    expect(makePlant(p.physical.units).recipes).toEqual([]);
  });
});
