// The editor on the plant (IR v4): pure edits of the Units and Phases
// views (model/plantEdit.ts) keep references consistent and the plant
// valid, plain-language issues by place, saving version 4 and loading
// version 1-3 saves, and switching units leaving every unit's generated
// code byte for byte as it was.

import { describe, expect, it } from "vitest";
import { generate } from "../src/blocks/generator";
import { LEGACY_KEY, MODEL_KEY, PLANT_KEY, type KeyValueStore, loadPlant, savePlant } from "../src/blocks/storage";
import { exampleWorkspace } from "../src/blocks/examples";
import { flowsOf, makeModel, profileOfModel, serializeModel, withFlows, workspaceOf } from "../src/model/payload";
import { type PlantModel, parsePlant, phaseSpecs, plantFromPayload, plantProblem, serializePlant, unitPayload } from "../src/model/plant";
import {
  addClass,
  addEm,
  addParam,
  addPhase,
  addUnit,
  bindNode,
  newBodyFlow,
  nodeFromInfo,
  plantIssues,
  removeClass,
  removeEm,
  removeParam,
  removeUnit,
  renameCm,
  setBody,
  setClass,
  setEm,
  setParam,
  setPhase,
  setRange,
  setUnit,
  withUnitPayload,
} from "../src/model/plantEdit";
import { EXAMPLE_PLANT } from "../src/model/plantExample";
import { boardDefaults } from "../src/model/pins";

function memoryStore(): KeyValueStore & { data: Map<string, string> } {
  const data = new Map<string, string>();
  return { data, getItem: (k) => data.get(k) ?? null, setItem: (k, v) => void data.set(k, v) };
}

const code = (p: PlantModel, id: string) => {
  const m = unitPayload(p, id)!;
  return generate(workspaceOf(m), profileOfModel(m), m.pins, m.machines, flowsOf(m), phaseSpecs(p, id)).source;
};

describe("the editor on the plant", () => {
  it("saves version 4 and loads it back; version 1-3 saves load as a one-unit plant", () => {
    const s = memoryStore();
    expect(savePlant(s, EXAMPLE_PLANT)).toBe(true);
    expect(JSON.parse(s.data.get(PLANT_KEY)!)).toMatchObject({ version: 4 });
    expect(s.data.has(MODEL_KEY)).toBe(false);
    expect(loadPlant(s, "pico2")).toEqual({ plant: EXAMPLE_PLANT, migrated: false });

    const blink = makeModel("pico2", exampleWorkspace("blink")!.workspace, boardDefaults("pico2"));
    const old = memoryStore();
    old.setItem(MODEL_KEY, serializeModel(blink));
    const r = loadPlant(old, "pico2");
    expect(r?.migrated).toBe(true);
    expect(unitPayload(r!.plant, "unit1")).toEqual(blink);
    // A plain plant also keeps the version-3 save for older copies.
    savePlant(old, r!.plant);
    expect(JSON.parse(old.data.get(MODEL_KEY)!)).toMatchObject({ version: 3 });
    expect(loadPlant(old, "pico2")).toEqual({ plant: r!.plant, migrated: false });
    // A legacy Blocks save still loads; nothing saved loads nothing.
    const legacy = memoryStore();
    legacy.setItem(LEGACY_KEY, JSON.stringify({ format: "middleware-blocks", version: 1, profile: "esp-base", workspace: exampleWorkspace("blink")!.workspace }));
    expect(loadPlant(legacy, "esp32")?.migrated).toBe(true);
    expect(loadPlant(memoryStore(), "pico2")).toBeNull();
    // Export and import: the exported text is a plant parsePlant reads.
    expect(parsePlant(serializePlant(EXAMPLE_PLANT), "pico2")).toEqual({ ok: true, plant: EXAMPLE_PLANT, migrated: false });
  });

  it("switching units keeps each unit's code byte for byte", () => {
    const before = EXAMPLE_PLANT.physical.units.map((u) => code(EXAMPLE_PLANT, u.id));
    // What the page does on a switch: write the shown unit back, show another.
    let p = withUnitPayload(EXAMPLE_PLANT, "pico-led", unitPayload(EXAMPLE_PLANT, "pico-led")!);
    expect(p).toEqual(EXAMPLE_PLANT);
    p = withUnitPayload(p, "esp-sense", unitPayload(p, "esp-sense")!);
    expect(EXAMPLE_PLANT.physical.units.map((u) => code(p, u.id))).toEqual(before);
    // Editing one unit's flows leaves the other unit's code alone.
    const sense = unitPayload(p, "esp-sense")!;
    p = withUnitPayload(p, "esp-sense", withFlows(sense, flowsOf(sense).map((f) => ({ ...f, name: "renamed" }))));
    expect(code(p, "pico-led")).toBe(before[0]);
    expect(plantProblem(JSON.parse(serializePlant(p)))).toBeNull();
  });

  it("unit edits: add, rename (phases follow), bind, remove (with phases)", () => {
    let p = addUnit(EXAMPLE_PLANT, "esp32");
    expect(p.physical.units.map((u) => u.id)).toEqual(["pico-led", "esp-sense", "unit"]);
    p = setUnit(p, "pico-led", "id", "led-board");
    expect(p.procedural.phases.map((ph) => ph.unit)).toEqual(["led-board", "esp-sense"]);
    p = bindNode(p, "led-board", "246f28aa01fe0000");
    expect(p.physical.units[0]?.node).toBe("246f28aa01fe0000");
    expect(plantProblem(JSON.parse(serializePlant(p)))).toBeNull();
    p = removeUnit(p, "esp-sense");
    expect(p.procedural.phases.map((ph) => ph.unit)).toEqual(["led-board"]);
    expect(removeUnit(plantFromPayload(makeModel("pico2", {})), "unit1").physical.units).toHaveLength(1);
  });

  it("module and phase edits carry names into phases and body flows", () => {
    let p = setEm(EXAMPLE_PLANT, "pico-led", 0, { id: "Lamp" });
    expect(p.procedural.phases[0]).toMatchObject({ em: "Lamp", id: "Blink" });
    // The body flows still name LED.Blink, so the plant says so ...
    expect(plantIssues(p).map((i) => i.message)).toEqual([
      'Flow BlinkRun is the running body, so its entry must be "phase Lamp.Blink running"; change its entry in Flows or pick another flow.',
      'Flow BlinkReset is the resetting body, so its entry must be "phase Lamp.Blink resetting"; change its entry in Flows or pick another flow.',
    ]);
    // ... while renaming the phase itself moves its flows along.
    p = setPhase(EXAMPLE_PLANT, 0, { id: "Flash" });
    expect(plantIssues(p)).toEqual([]);
    expect(code(p, "pico-led")).toContain("phase/LED.Flash");
    p = setPhase(p, 0, { em: "LED", id: "Blink" });
    expect(code(p, "pico-led")).toBe(code(EXAMPLE_PLANT, "pico-led"));
    // Pins: a module owns labels; a rename in Pins follows; removing a module removes its phases.
    expect(renameCm(EXAMPLE_PLANT, "pico-led", "LED", "LAMP").physical.units[0]?.ems[0]?.cms).toEqual(["LAMP"]);
    p = addEm(EXAMPLE_PLANT, "pico-led");
    p = setEm(p, "pico-led", 1, { cms: ["LED"] });
    expect(plantIssues(p).map((i) => i.message)).toEqual(["Pin LED already belongs to module LED; a pin belongs to one module."]);
    p = removeEm(EXAMPLE_PLANT, "pico-led", 0);
    expect(p.procedural.phases.map((x) => x.unit)).toEqual(["esp-sense"]);
  });

  it("class and parameter edits keep phases and recipe steps in step", () => {
    let p = setClass(EXAMPLE_PLANT, 0, { id: "Flash" });
    expect(p.procedural.phases[0]?.class).toBe("Flash");
    expect(p.recipes[0]?.steps[0]?.class).toBe("Flash");
    expect(plantProblem(JSON.parse(serializePlant(p)))).toBeNull();
    p = setParam(EXAMPLE_PLANT, 0, 1, { id: "count" });
    expect(Object.keys(p.procedural.phases[0]?.ranges ?? {})).toEqual(["period_ms", "count"]);
    expect(p.recipes[0]?.steps[0]?.params).toEqual({ period_ms: 250, count: 3 });
    p = addParam(p, 0);
    expect(p.procedural.phases[0]?.ranges.param).toEqual([0, 100]);
    p = removeParam(p, 0, 2);
    p = removeParam(p, 0, 1);
    expect(p.recipes[0]?.steps[0]?.params).toEqual({ period_ms: 250 });
    expect(plantProblem(JSON.parse(serializePlant(p)))).toBeNull();
    p = removeClass(EXAMPLE_PLANT, 1);
    expect(p.procedural.phases.map((x) => x.id)).toEqual(["Blink"]);
    p = addClass(EXAMPLE_PLANT);
    expect(p.procedural.classes.map((c) => c.id)).toEqual(["Blink", "Measure", "Class"]);
  });

  it("phases: add, ranges, bodies and a new body flow that checks clean", () => {
    let p = addPhase(EXAMPLE_PLANT, "pico-led", "LED");
    expect(p.procedural.phases[2]).toMatchObject({ unit: "pico-led", em: "LED", id: "Blink2", class: "Blink", ranges: { period_ms: [10, 10_000], times: [1, 100] }, bodies: {} });
    p = setRange(p, 2, "times", [1, 5]);
    expect(p.procedural.phases[2]?.ranges.times).toEqual([1, 5]);
    const made = newBodyFlow(p, 2, "holding");
    expect(made.flow).toBe("Blink2Holding");
    expect(made.plant.procedural.phases[2]?.bodies).toEqual({ holding: "Blink2Holding" });
    expect(plantIssues(made.plant)).toEqual([]);
    expect(plantProblem(JSON.parse(serializePlant(made.plant)))).toBeNull();
    const cleared = setBody(made.plant, 2, "holding", null);
    expect(cleared.procedural.phases[2]?.bodies).toEqual({});
    // A body naming a flow whose entry is another state's is flagged.
    const wrong = setBody(EXAMPLE_PLANT, 0, "holding", "BlinkRun");
    expect(plantIssues(wrong)).toEqual([
      { place: "bodies", row: 0, message: 'Flow BlinkRun is the holding body, so its entry must be "phase LED.Blink holding"; change its entry in Flows or pick another flow.' },
    ]);
  });

  it("issues name the place in plain words and agree with the plant check", () => {
    expect(plantIssues(EXAMPLE_PLANT)).toEqual([]);
    const cases: [PlantModel, string][] = [
      [setUnit(EXAMPLE_PLANT, "esp-sense", "id", "Pico-LED"), "Unit 1 already has the id Pico-LED; unit ids must be unique (ignoring case)."],
      [setUnit(EXAMPLE_PLANT, "pico-led", "class", "led unit"), 'The unit class "led unit" starts with a letter and has only letters, digits and _ (at most 24); recipes ask for units by class.'],
      [setParam(EXAMPLE_PLANT, 1, 0, { min: 9, max: 1 }), "The minimum 9 is above the maximum 1."],
      [setRange(EXAMPLE_PLANT, 1, "samples", [1, 99]), "The range of samples, 1 to 99, must lie within Measure's 1 to 16."],
      [setPhase(EXAMPLE_PLANT, 0, { class: "Nope" }), "There is no phase class Nope; choose one or add it."],
      [setEm(EXAMPLE_PLANT, "pico-led", 0, { cms: ["LED", "GHOST"] }), "Pin GHOST is not in the unit's pin table; add it under Pins or remove it here."],
    ];
    for (const [p, message] of cases) {
      expect(plantIssues(p).map((i) => i.message)).toContain(message);
      expect(plantProblem(JSON.parse(serializePlant(p)))).not.toBeNull();
    }
    const two = bindNode(bindNode(EXAMPLE_PLANT, "pico-led", "00000000000000aa"), "esp-sense", "00000000000000aa");
    expect(plantIssues(two)).toEqual([{ place: "units", row: 1, message: "Unit pico-led is bound to the same device; a device runs one unit." }]);
  });

  it("binds to the node id sys/info reports", () => {
    expect(nodeFromInfo("246F28AA01FE0000")).toBe("246f28aa01fe0000");
    expect(nodeFromInfo(undefined)).toBeNull();
    expect(nodeFromInfo("1234")).toBeNull();
  });
});
