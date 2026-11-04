// The Recipe view's pure edits (model/recipe.ts, design.md Q18): steps,
// transitions with parallel divergence and convergence, unit procedures,
// fixes from recipe-core's check (through the real wasm), the run order
// the outline lists. Round 7's chart rules hold by construction: no loop,
// no step on one side of two transitions, no choice between branches.

import { describe, expect, it } from "vitest";
import { addRecipe, removeRecipe, setRecipe } from "../src/model/plantEdit";
import { BLINK_THEN_MEASURE, EXAMPLE_PLANT } from "../src/model/plantExample";
import {
  type MasterRecipe,
  addStep,
  addUnitProcedure,
  applyRecipeFix,
  connectSteps,
  detach,
  incoming,
  newRecipe,
  outgoing,
  reaches,
  recipeProblem,
  removeStep,
  removeTransition,
  removeUnitProcedure,
  renameStep,
  runOrder,
  setArg,
  setStepClass,
  setStepLane,
  setUnitProcedure,
  moveStep,
} from "../src/model/recipe";
import { protocol } from "./fakeDevice";

const proto = protocol();
const classes = EXAMPLE_PLANT.procedural.classes;
const r0 = BLINK_THEN_MEASURE;

/** Adds steps until the recipe has ids A.. */
function withSteps(r: MasterRecipe, n: number): MasterRecipe {
  let x = r;
  for (let i = 0; i < n; i++) x = addStep(x, classes, "Signal", "Blink").recipe;
  return x;
}

describe("recipe edits", () => {
  it("adds a step at the class minimums below its lane, and it checks clean", () => {
    const { recipe, id } = addStep(r0, classes, "Signal", "Blink");
    expect(id).toBe("Step1");
    const s = recipe.steps.at(-1)!;
    expect(s).toMatchObject({ unitProcedure: "Signal", class: "Blink", params: { period_ms: 10, times: 1 }, y: 160 });
    expect(recipeProblem(recipe, "r")).toBeNull();
    // Unconnected, it is a second initial step: the check is still clean.
    expect(proto.checkRecipe(recipe, classes)).toEqual([]);
    expect(r0.steps).toHaveLength(2);
  });

  it("connect makes a transition, a parallel divergence and a convergence; never a loop or a choice", () => {
    let r = withSteps(r0, 2); // Step1, Step2 (initial)
    // TakeReading -> Step1: new transition.
    r = connectSteps(r, "TakeReading", "Step1").recipe;
    expect(r.transitions.at(-1)).toMatchObject({ from: ["TakeReading"], to: ["Step1"] });
    // TakeReading -> Step2 joins that transition: a parallel divergence.
    r = connectSteps(r, "TakeReading", "Step2").recipe;
    expect(r.transitions.at(-1)).toMatchObject({ from: ["TakeReading"], to: ["Step1", "Step2"] });
    expect(r.transitions).toHaveLength(2);
    // A third step after Step1 and Step2: a convergence.
    r = addStep(r, classes, "Sense", "Measure").recipe; // Step3
    r = connectSteps(r, "Step1", "Step3").recipe;
    r = connectSteps(r, "Step2", "Step3").recipe;
    expect(r.transitions.at(-1)).toMatchObject({ from: ["Step1", "Step2"], to: ["Step3"] });
    expect(proto.checkRecipe(r, classes)).toEqual([]);
    expect(runOrder(r).map((s) => s.id)).toEqual(["BlinkLed", "TakeReading", "Step1", "Step2", "Step3"]);
    // Refused: a loop, itself, two different transitions on the two ends.
    expect(connectSteps(r, "Step3", "BlinkLed").refused).toMatch(/already leads to/);
    expect(connectSteps(r, "Step3", "Step3").refused).toMatch(/itself/);
    expect(connectSteps(r, "BlinkLed", "Step3").refused).toMatch(/join them there/);
    expect(connectSteps(r, "BlinkLed", "Step3").recipe).toBe(r);
    // Already joined: no change.
    expect(connectSteps(r, "Step1", "Step3")).toEqual({ recipe: r, refused: null });
    expect(reaches(r, "BlinkLed", "Step3")).toBe(true);
    expect(reaches(r, "Step3", "BlinkLed")).toBe(false);
    expect([outgoing(r, "Step1"), incoming(r, "Step1"), outgoing(r, "Step3")]).toEqual([2, 1, -1]);
  });

  it("detach and remove keep transitions non-empty", () => {
    let r = withSteps(r0, 1);
    r = connectSteps(r, "BlinkLed", "Step1").recipe; // t1: BlinkLed -> TakeReading, Step1
    expect(r.transitions[0]!.to).toEqual(["TakeReading", "Step1"]);
    r = detach(r, 0, "to", "Step1");
    expect(r.transitions[0]!.to).toEqual(["TakeReading"]);
    expect(detach(r, 0, "to", "Step1")).toBe(r);
    r = detach(r, 0, "from", "BlinkLed");
    expect(r.transitions).toEqual([]);
    const s = removeStep(r0, "TakeReading");
    expect(s.steps.map((x) => x.id)).toEqual(["BlinkLed"]);
    expect(s.transitions).toEqual([]);
    expect(removeTransition(r0, 0).transitions).toEqual([]);
    expect(removeTransition(r0, 5)).toBe(r0);
  });

  it("renames carry to transitions; class changes keep arguments in range", () => {
    const r = renameStep(r0, "BlinkLed", "Flash");
    expect(r.transitions[0]!.from).toEqual(["Flash"]);
    expect(renameStep(r0, "BlinkLed", "takereading")).toBe(r0);
    const c = setStepClass(setArg(r0, "TakeReading", "samples", 99), "TakeReading", "Blink", classes);
    expect(c.steps[1]!.params).toEqual({ period_ms: 10, times: 1 });
    const back = setStepClass(r0, "TakeReading", "Measure", classes);
    expect(back.steps[1]!.params).toEqual({ samples: 4 });
    expect(moveStep(r0, "BlinkLed", -5).steps[0]!.y).toBe(0);
    expect(setStepLane(r0, "BlinkLed", "Sense").steps[0]!.unitProcedure).toBe("Sense");
    expect(setStepLane(r0, "BlinkLed", "Nowhere")).toBe(r0);
  });

  it("unit procedures: add, rename (steps follow), remove only when unused", () => {
    let r = addUnitProcedure(r0);
    expect(r.unitProcedures.at(-1)).toEqual({ id: "Proc1", name: "", unitClass: null });
    expect(removeUnitProcedure(r, 0)).toBe(r);
    expect(removeUnitProcedure(r, 2).unitProcedures).toHaveLength(2);
    r = setUnitProcedure(r0, 1, { id: "Read", unitClass: null });
    expect(r.steps[1]!.unitProcedure).toBe("Read");
    expect(setUnitProcedure(r0, 1, { id: "signal" })).toBe(r0);
  });

  it("applies the check's fixes until the recipe is clean", () => {
    let r: MasterRecipe = {
      ...r0,
      steps: [{ ...r0.steps[0]!, params: { period_ms: 5, speed: 2 } }, r0.steps[1]!],
      transitions: [...r0.transitions, { id: "t2", from: ["TakeReading"], to: ["BlinkLed"] }],
    };
    r = addUnitProcedure(r);
    for (let i = 0; i < 10; i++) {
      const fixable = proto.checkRecipe(r, classes).find((x) => x.fix);
      if (!fixable) break;
      r = applyRecipeFix(r, fixable.fix!);
    }
    expect(proto.checkRecipe(r, classes)).toEqual([]);
    expect(r.steps[0]!.params).toEqual({ period_ms: 10, times: 1 });
    expect(r.unitProcedures).toHaveLength(2);
  });

  it("plant recipes: add, set, remove", () => {
    let p = addRecipe(EXAMPLE_PLANT);
    expect(p.recipes.map((r) => r.id)).toEqual(["BlinkThenMeasure", "Recipe1"]);
    expect(newRecipe(p.recipes).id).toBe("Recipe2");
    p = setRecipe(p, 1, { ...p.recipes[1]!, name: "x" });
    expect(p.recipes[1]!.name).toBe("x");
    expect(removeRecipe(p, 1).recipes).toHaveLength(1);
    expect(setRecipe(p, 9, r0)).toBe(p);
  });
});
