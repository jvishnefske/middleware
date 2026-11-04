// The worked example of the S88 split (design.md, "S88 recipe model"):
// master recipe "Blink then Measure" over phase classes Blink(period_ms,
// times) and Measure(samples), and two units: a Pico 2 with an LED
// equipment module offering LED.Blink, and an ESP32 with an ADC equipment
// module offering ADC.Measure. Allocation binds Blink to the Pico and
// Measure to the ESP32 (web/test/recipe.check.test.ts). The phase bodies
// are ordinary flows for now; the phase-state flow entry they will use
// arrives with the phase codegen (round 8 in the backlog).

import type { Flow } from "./flows";
import { makePlant, type PlantModel, type Unit } from "./plant";
import { row } from "./pins";
import type { ProceduralModel } from "./procedural";
import type { MasterRecipe } from "./recipe";

const BLINK_RUN: Flow = {
  kind: "flow",
  id: "BlinkRun",
  name: "Blink: toggle the LED",
  nodes: [
    { id: "n1", kind: "entry", x: 40, y: 40, props: { on: "every", ms: 250 } },
    { id: "n2", kind: "action", x: 40, y: 160, props: { kind: "toggle", label: "LED" } },
  ],
  wires: [{ from: { node: "n1", port: "next" }, to: { node: "n2", port: "in" } }],
};

const SAMPLE: Flow = {
  kind: "flow",
  id: "Sample",
  name: "Measure: take a sample",
  nodes: [
    { id: "n1", kind: "entry", x: 40, y: 40, props: { on: "every", ms: 100 } },
    { id: "n2", kind: "action", x: 40, y: 160, props: { kind: "log", text: "sample" } },
  ],
  wires: [{ from: { node: "n1", port: "next" }, to: { node: "n2", port: "in" } }],
};

const LED_UNIT: Unit = {
  id: "pico-led",
  name: "Pico 2 with the status LED",
  class: "LedUnit",
  node: null,
  board: "pico2",
  cms: { rows: [row("gpio-out", "LED", 25)], buses: [] },
  ems: [{ id: "LED", name: "Status LED", cms: ["LED"] }],
  logic: [{ kind: "blocks", workspace: {} }, BLINK_RUN],
  machines: [],
};

const SENSOR_UNIT: Unit = {
  id: "esp-sense",
  name: "ESP32 with a potentiometer",
  class: "SensorUnit",
  node: null,
  board: "esp32",
  cms: { rows: [row("adc", "POT", 34)], buses: [] },
  ems: [{ id: "ADC", name: "Analog input", cms: ["POT"] }],
  logic: [{ kind: "blocks", workspace: {} }, SAMPLE],
  machines: [],
};

const PROCEDURAL: ProceduralModel = {
  classes: [
    {
      id: "Blink",
      name: "Blink a light",
      params: [
        { id: "period_ms", name: "Period", min: 10, max: 10_000, unit: "ms" },
        { id: "times", name: "Times", min: 1, max: 100, unit: "" },
      ],
    },
    { id: "Measure", name: "Take a reading", params: [{ id: "samples", name: "Samples", min: 1, max: 16, unit: "" }] },
  ],
  phases: [
    { unit: "pico-led", em: "LED", id: "Blink", class: "Blink", ranges: { period_ms: [50, 1_000], times: [1, 10] }, bodies: { running: "BlinkRun" } },
    { unit: "esp-sense", em: "ADC", id: "Measure", class: "Measure", ranges: { samples: [1, 8] }, bodies: { running: "Sample" } },
  ],
};

export const BLINK_THEN_MEASURE: MasterRecipe = {
  id: "BlinkThenMeasure",
  name: "Blink then Measure",
  unitProcedures: [
    { id: "Signal", name: "Signal the operator", unitClass: null },
    { id: "Sense", name: "Read the sensor", unitClass: "SensorUnit" },
  ],
  steps: [
    { id: "BlinkLed", name: "Blink three times", unitProcedure: "Signal", operation: "Announce", class: "Blink", params: { period_ms: 250, times: 3 }, x: 40, y: 40 },
    { id: "TakeReading", name: "Take a reading", unitProcedure: "Sense", operation: "Measure", class: "Measure", params: { samples: 4 }, x: 40, y: 200 },
  ],
  transitions: [{ id: "t1", from: ["BlinkLed"], to: ["TakeReading"] }],
};

export const EXAMPLE_PLANT: PlantModel = makePlant([LED_UNIT, SENSOR_UNIT], PROCEDURAL, [BLINK_THEN_MEASURE]);
