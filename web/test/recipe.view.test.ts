// @vitest-environment jsdom
//
// The Recipe view in a DOM (jsdom, design.md Q18, Q19): the worked
// example's chart (lanes per unit procedure, steps naming a class and its
// arguments, a transition bar, a parallel divergence's double line),
// check issues on their step with a fix, the outline in run order and its
// "next step" menu, the allocation panel per unit, and a run in the
// browser simulator to Complete with the operator buttons enabled exactly
// where the phase model accepts them and live step states. jsdom has no
// layout: what this cannot see (sizes, overflow at 390 px) stays
// screenshot-checked.

import { readFileSync } from "node:fs";
import { join } from "node:path";
import { afterEach, describe, expect, it } from "vitest";
import type { PlantModel } from "../src/model/plant";
import { EXAMPLE_PLANT } from "../src/model/plantExample";
import { recipeView } from "../src/ui/recipeView";
import { SerialRequester } from "../src/device/requester";
import { bindNode } from "../src/model/plantEdit";
import type { DeviceInfo } from "../src/protocol/wasm";
import { loadProtocolSync } from "../src/protocol/wasm";

// jsdom's URL is not node's, so fakeDevice's file-URL loader does not
// apply: vitest runs from web/.
const proto = loadProtocolSync(readFileSync(join(process.cwd(), "src/pkg/protocol_wasm_bg.wasm")));

/** A mounted view that re-renders on every change, like the Blocks page. */
function mount(plant: PlantModel = EXAMPLE_PLANT, mode: "canvas" | "outline" = "canvas") {
  localStorage.setItem("recipe-mode", mode);
  const changes: PlantModel[] = [];
  let current = plant;
  const v = recipeView(proto, {
    onChange: (p) => {
      changes.push(p);
      current = p;
      v.render(p, null);
    },
  });
  document.body.replaceChildren(v.el);
  v.render(plant, null);
  return { v, el: v.el, changes, plant: () => current };
}

const q = (el: Element, sel: string) => el.querySelector(sel);
const all = (el: Element, sel: string) => [...el.querySelectorAll(sel)];
const label = (el: Element | null) => el?.getAttribute("aria-label") ?? "";
function setValue(el: HTMLInputElement | HTMLSelectElement, value: string) {
  el.value = value;
  el.dispatchEvent(new Event("change", { bubbles: true }));
}
function key(el: Element, k: string) {
  el.dispatchEvent(new KeyboardEvent("keydown", { key: k, bubbles: true }));
}
async function until(f: () => boolean, ms = 15_000) {
  const t0 = Date.now();
  while (!f()) {
    if (Date.now() - t0 > ms) throw new Error("timed out");
    await new Promise((r) => setTimeout(r, 20));
  }
}

afterEach(() => document.body.replaceChildren());

describe("the Recipe view (Q18)", () => {
  it("draws a lane per unit procedure, a step per step and a bar per transition", () => {
    const { el } = mount();
    expect(all(el, "[data-lane]").map(label)).toEqual(["Unit procedure Signal, any unit", "Unit procedure Sense, unit class SensorUnit"]);
    expect(all(el, "[data-step]").map(label)).toEqual([
      "Step BlinkLed (Blink three times): Blink period_ms=250 times=3, in Signal",
      "Step TakeReading (Take a reading): Measure samples=4, in Sense",
    ]);
    expect(all(el, "[data-transition]").map(label)).toEqual(["Transition t1: after BlinkLed comes TakeReading"]);
    // Steps sit in their lane: TakeReading's box is right of BlinkLed's.
    const x = (id: string) => Number(/translate\((\d+)/.exec(q(el, `#rstep-${id}`)!.getAttribute("transform")!)![1]);
    expect(x("TakeReading")).toBeGreaterThan(x("BlinkLed"));
    expect(all(el, "[data-step]").every((s) => s.getAttribute("tabindex") === "0")).toBe(true);
    expect(q(el, "#recipe-issues")).toBeNull();
  });

  it("selects a step from the keyboard; arguments carry the class range and the check's issues, with a fix", () => {
    const { el, plant } = mount();
    key(q(el, "#rstep-BlinkLed")!, "Enter");
    const period = q(el, "#rprop-arg-period_ms") as HTMLInputElement;
    expect([period.min, period.max, period.value]).toEqual(["10", "10000", "250"]);
    expect(q(el, `#${period.getAttribute("aria-describedby")}`)!.textContent).toContain("10..10000 ms");
    setValue(period, "5");
    expect(plant().recipes[0]!.steps[0]!.params.period_ms).toBe(5);
    const again = q(el, "#rprop-arg-period_ms") as HTMLInputElement;
    expect(again.getAttribute("aria-invalid")).toBe("true");
    expect(q(el, `#${again.getAttribute("aria-describedby")}`)!.textContent).toContain("outside the phase class's range 10..10000");
    expect(q(el, "#rstep-BlinkLed")!.getAttribute("aria-invalid")).toBe("true");
    expect(q(el, "#rstep-BlinkLed")!.classList.contains("error")).toBe(true);
    const fix = all(el, "#recipe-issues button.fix")[0] as HTMLButtonElement;
    expect(fix.textContent).toBe("Set period_ms to 10");
    fix.click();
    expect(plant().recipes[0]!.steps[0]!.params.period_ms).toBe(10);
    expect(q(el, "#recipe-issues")).toBeNull();
    // Focus is kept on the field across the re-render.
    setValue(q(el, "#rprop-arg-times") as HTMLInputElement, "4");
    expect(document.activeElement?.id).toBe("rprop-arg-times");
  });

  it("+ Step after a selected step makes a parallel divergence drawn with a double line", () => {
    const { el, plant } = mount();
    key(q(el, "#rstep-BlinkLed")!, "Enter");
    (q(el, "#recipe-add-step") as HTMLButtonElement).click();
    const r = plant().recipes[0]!;
    expect(r.transitions).toEqual([{ id: "t1", from: ["BlinkLed"], to: ["TakeReading", "Step1"] }]);
    expect(label(q(el, "[data-transition]"))).toBe("Transition t1: after BlinkLed comes TakeReading and Step1 (in parallel)");
    expect(all(el, ".rdouble")).toHaveLength(2);
    // Arrow keys move the new step down and into the next lane.
    key(q(el, "#rstep-Step1")!, "ArrowRight");
    expect(plant().recipes[0]!.steps[2]!.unitProcedure).toBe("Sense");
    const y = plant().recipes[0]!.steps[2]!.y;
    key(q(el, "#rstep-Step1")!, "ArrowDown");
    expect(plant().recipes[0]!.steps[2]!.y).toBe(y + 8);
    expect(document.activeElement?.id).toBe("rstep-Step1");
  });

  it("the outline lists steps in run order and its menu connects, refusing a loop", () => {
    const { el, plant } = mount(EXAMPLE_PLANT, "outline");
    const rows = all(el, ".recipe-outline > li");
    expect(rows.map((r) => r.querySelector(".onode")!.textContent)).toEqual(["Step · BlinkLed · Signal Blink period_ms=250 times=3", "Step · TakeReading · Sense Measure samples=4"]);
    expect(rows[0]!.textContent).toContain("starts the recipe");
    expect(rows[1]!.textContent).toContain("after BlinkLed");
    setValue(q(el, "#onext-TakeReading") as HTMLSelectElement, "BlinkLed");
    expect(q(el, "#recipe-status")!.textContent).toMatch(/Not connected: "BlinkLed" already leads to "TakeReading"/);
    expect(plant().recipes[0]!.transitions).toHaveLength(1);
    setValue(q(el, "#onext-TakeReading") as HTMLSelectElement, "+new");
    expect(plant().recipes[0]!.transitions.map((t) => [t.from, t.to])).toEqual([
      [["BlinkLed"], ["TakeReading"]],
      [["TakeReading"], ["Step1"]],
    ]);
    expect(document.activeElement?.id).toBe("ostep-Step1");
  });
});

describe("allocation and a run in the browser (Q19)", () => {
  it("lists per unit what it runs, or why it cannot", () => {
    const { el } = mount();
    const rows = () => all(el, ".alloc tr").slice(1).map((tr) => [...tr.children].map((c) => c.textContent).filter((_, i) => i !== 1).join(" | "));
    expect(rows()).toEqual(["pico-led | not bound (Units view) | Signal: BlinkLed on LED.Blink", "pico-sense | not bound (Units view) | Sense: TakeReading on ADC.Measure"]);
    const on = q(el, "#ralloc-on-pico-sense") as HTMLInputElement;
    on.checked = false;
    on.dispatchEvent(new Event("change", { bubbles: true }));
    expect(rows()[1]).toBe("pico-sense | not bound (Units view) | cannot run Sense: offline (discovery does not see it)");
    expect(rows()[0]).toBe("pico-led | not bound (Units view) | cannot run Sense: not of the unit class the procedure asks for");
    expect(document.activeElement?.id).toBe("ralloc-on-pico-sense");
  });

  it("runs the recipe on the simulated units to Complete, buttons following the phase model", async () => {
    const { el } = mount();
    (q(el, "#recipe-run") as HTMLButtonElement).click();
    await until(() => !!q(el, "#recipe-op-start"));
    const enabled = () => all(el, ".operator button").filter((b) => !(b as HTMLButtonElement).disabled).map((b) => b.textContent);
    expect(enabled()).toEqual(["Start"]);
    expect(q(el, ".rstate")!.textContent).toBe("Procedure: Idle");
    (q(el, "#recipe-op-start") as HTMLButtonElement).click();
    await until(() => q(el, ".rstate")?.textContent === "Procedure: Running");
    expect(enabled()).toEqual(["Hold", "Stop", "Abort"]);
    await until(() => !!q(el, "#rstep-BlinkLed.st-started"));
    await until(() => q(el, ".rstate")?.textContent === "Procedure: Complete");
    expect(enabled()).toEqual(["Reset"]);
    expect(all(el, "[data-step]").map((s) => s.getAttribute("class")!.split(" ").find((c) => c.startsWith("st-")))).toEqual(["st-done", "st-done"]);
    const trace = all(el, ".recipe-trace li").map((l) => l.textContent!.replace(/^\s*[\d.]+ s\s+/, ""));
    expect(trace[0]).toBe("recipe Idle -> Running");
    expect(trace.at(-1)).toBe("recipe Running -> Complete");
    expect(q(el, ".recipe-phases")!.textContent).toContain("pico-sense/ADC.Measure: Idle — reported 2048, 4");
    (q(el, "#recipe-run-stop") as HTMLButtonElement).click();
    expect(q(el, "#recipe-op-start")).toBeNull();
  }, 30_000);
});

describe("a unit on the Device tab's board (Q23)", () => {
  const node = "e6613852831f2a0b";
  const info = (serves: boolean): DeviceInfo => ({ abiVersion: 5, capacity: 65536, status: 2, statusName: "running", chip: "rp2350", firmware: "0.1.0", node, caps: serves ? 1 : 0, serves });
  const board = (serves: boolean) => {
    const i = info(serves);
    return { info: i, requester: new SerialRequester({ request: async () => new Uint8Array() }, i) };
  };
  const boardCell = (el: Element) => (all(el, ".alloc tr")[1]!.children[2] as HTMLElement);

  it("runs a bound unit on a board that serves, and says why not on one that cannot", () => {
    const plant = bindNode(EXAMPLE_PLANT, "pico-led", node);
    const v = recipeView(proto, { onChange: () => {} });
    document.body.replaceChildren(v.el);
    v.render(plant, node, board(true));
    expect(boardCell(v.el).textContent).toBe(`bound to ${node}: the board on the Device tab (runs on the board)`);
    expect(q(v.el, "#recipe-run-h")!.textContent).toBe("Run (pico-led on the board)");
    v.render(plant, node, board(false));
    expect(boardCell(v.el).className).toBe("warning");
    expect(boardCell(v.el).textContent).toBe(
      `bound to ${node}: the board on the Device tab, which cannot serve phases (runs in the simulator). This board (rp2350 0.1.0) cannot serve phases: its firmware does not pass requests to the payload. Update its firmware on the Flash tab, or run this unit in the simulator.`,
    );
    expect(q(v.el, "#recipe-run-h")!.textContent).toBe("Run in the simulator");
    // No board on the Device tab: as before.
    v.render(plant, null, null);
    expect(boardCell(v.el).textContent).toBe(`bound to ${node}: not seen on the Device tab`);
  });
});
