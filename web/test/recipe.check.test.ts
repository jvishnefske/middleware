// S88 recipes through the real wasm (protocol-wasm checkRecipe,
// allocateRecipe, simulateRecipe and phaseModel over recipe-core): the
// worked example "Blink then Measure" checks clean, binds Blink to the LED
// unit and Measure to the sensor unit, runs to Complete against simulated
// units, holds and restarts, and aborts after losing a unit.

import { describe, expect, it } from "vitest";
import { allocationUnits } from "../src/model/plant";
import { BLINK_THEN_MEASURE, EXAMPLE_PLANT } from "../src/model/plantExample";
import { PHASE_COMMANDS, PHASE_STATES } from "../src/model/procedural";
import type { MasterRecipe } from "../src/model/recipe";
import { protocol } from "./fakeDevice";

const proto = protocol();
const classes = EXAMPLE_PLANT.procedural.classes;
const units = (online = (_: string) => true) =>
  allocationUnits(EXAMPLE_PLANT, (u) => online(u.id), (ph) => (ph.class === "Blink" ? 300 : 100));

describe("S88 recipes (Rust)", () => {
  it("the worked example checks clean and allocates Blink to the LED unit, Measure to the sensor unit", () => {
    expect(proto.checkRecipe(BLINK_THEN_MEASURE, classes)).toEqual([]);
    expect(proto.allocateRecipe(BLINK_THEN_MEASURE, classes, units())).toEqual({
      ok: true,
      bindings: [
        { step: "BlinkLed", unit: "pico-led", em: "LED", phase: "Blink" },
        { step: "TakeReading", unit: "esp-sense", em: "ADC", phase: "Measure" },
      ],
    });
  });

  it("reports recipe errors in plain language, by place", () => {
    const bad: MasterRecipe = {
      ...BLINK_THEN_MEASURE,
      steps: [{ ...BLINK_THEN_MEASURE.steps[0], params: { period_ms: 5 } }, BLINK_THEN_MEASURE.steps[1]],
      transitions: [...BLINK_THEN_MEASURE.transitions, { id: "t2", from: ["TakeReading"], to: ["BlinkLed"] }],
    };
    expect(proto.checkRecipe(bad, classes).map((i) => `${i.at} ${i.index} ${i.severity}: ${i.message}`)).toEqual([
      "step 0 error: 5 is outside the phase class's range 10..10000",
      'step 0 error: step "BlinkLed" needs a value for "times"',
      "transition 0 error: this transition is part of a loop; a recipe runs each step once",
      "transition 1 error: this transition is part of a loop; a recipe runs each step once",
    ]);
  });

  it("an allocation failure says, per unit, why it cannot host the procedure", () => {
    const r = proto.allocateRecipe(BLINK_THEN_MEASURE, classes, units((id) => id !== "esp-sense"));
    expect(r).toEqual({
      ok: false,
      unitProcedure: "Sense",
      message: 'no unit can run unit procedure "Sense" (pico-led: not of the unit class the procedure asks for; esp-sense: offline (discovery does not see it))',
      reasons: [
        { unit: "pico-led", reason: "not of the unit class the procedure asks for" },
        { unit: "esp-sense", reason: "offline (discovery does not see it)" },
      ],
    });
  });

  it("runs to Complete against simulated units: Blink first, Measure when Blink completes", () => {
    const r = proto.simulateRecipe(BLINK_THEN_MEASURE, classes, units(), [{ at: 0, kind: "start" }], 2000);
    expect(r.ok && r.state).toBe("Complete");
    if (!r.ok) return;
    const sends = r.lines.filter((l) => l.kind === "send" && l.text.startsWith("Start")).map((l) => `${l.t} ${l.text}`);
    expect(sends).toEqual(['0 Start -> pico-led/LED.Blink (#1, step "BlinkLed")', '300 Start -> esp-sense/ADC.Measure (#3, step "TakeReading")']);
  });

  it("a Hold mid-run holds the batch until Restart, and losing a unit holds it until Abort ends it Aborted", () => {
    const held = proto.simulateRecipe(BLINK_THEN_MEASURE, classes, units(), [
      { at: 0, kind: "start" },
      { at: 100, kind: "hold" },
      { at: 400, kind: "restart" },
    ], 3000);
    expect(held.ok && held.lines.filter((l) => l.kind === "recipe").map((l) => l.text)).toEqual([
      "recipe Idle -> Running",
      "recipe Running -> Holding",
      "recipe Holding -> Held",
      "recipe Held -> Restarting",
      "recipe Restarting -> Running",
      "recipe Running -> Complete",
    ]);
    const lost = proto.simulateRecipe(BLINK_THEN_MEASURE, classes, units(), [
      { at: 0, kind: "start" },
      { at: 100, kind: "lose", unit: "pico-led" },
      { at: 200, kind: "restart" },
      { at: 300, kind: "abort" },
    ], 1000);
    expect(lost.ok && lost.state).toBe("Aborted");
    expect(lost.ok && lost.lines.filter((l) => l.kind === "failed" || l.kind === "refused").map((l) => `${l.t} ${l.text}`)).toEqual([
      "100 unit pico-led left the network (resumable: hold)",
      "200 Restart refused: unit pico-led is offline",
    ]);
  });

  it("exposes the phase state model matching the page's names", () => {
    const m = proto.phaseModel();
    expect(m.states).toEqual([...PHASE_STATES]);
    expect(m.commands).toEqual([...PHASE_COMMANDS]);
    expect(m.next[PHASE_STATES.indexOf("Held")]).toEqual([null, null, "Restarting", "Stopping", "Aborting", null, null]);
  });

  it("an unknown script kind throws", () => {
    expect(() => proto.simulateRecipe(BLINK_THEN_MEASURE, classes, units(), [{ at: 0, kind: "pause" } as never], 100)).toThrow(/not a command/);
  });
});
