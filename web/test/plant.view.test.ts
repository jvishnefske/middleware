// @vitest-environment jsdom
//
// The Units and Phases views in a DOM (jsdom, design.md Q21): real tables
// of labelled controls, Ctrl + arrow keys between cells and Up / Down
// between rows in text cells, focus kept across the re-render every change
// causes, each row's problems under it (aria-describedby, aria-invalid),
// tables inside their own scrolling container, and the unit picker. Page
// layout (no sideways scroll at 390 px) is not visible here: Q22.

import { afterEach, describe, expect, it } from "vitest";
import type { PlantModel } from "../src/model/plant";
import { plantIssues } from "../src/model/plantEdit";
import { EXAMPLE_PLANT } from "../src/model/plantExample";
import { phasesView, unitsView } from "../src/ui/plantViews";

afterEach(() => document.body.replaceChildren());

function mountUnits(plant: PlantModel = EXAMPLE_PLANT) {
  const selected: string[] = [];
  let current = plant;
  const v = unitsView({
    onChange: (p) => {
      current = p;
      v.render(p, "pico-led", plantIssues(p), "246f280102030000");
    },
    onSelect: (id) => selected.push(id),
  });
  document.body.replaceChildren(v.el);
  v.render(plant, "pico-led", plantIssues(plant), "246f280102030000");
  return { el: v.el, selected, plant: () => current };
}

const ctrl = (el: Element, k: string, extra: KeyboardEventInit = {}) => el.dispatchEvent(new KeyboardEvent("keydown", { key: k, bubbles: true, ...extra }));

describe("the Units view (Q21)", () => {
  it("is a table of labelled controls in its own scroller", () => {
    const { el } = mountUnits();
    const table = el.querySelector("table")!;
    expect(table.parentElement!.classList.contains("pin-scroll")).toBe(true);
    expect(table.querySelector("caption")!.textContent).toBe("Units: one row per board in the plant");
    const controls = [...table.querySelectorAll("input, select, button")];
    expect(controls.length).toBeGreaterThan(10);
    for (const c of controls) expect(c.getAttribute("aria-label"), c.outerHTML).toBeTruthy();
    expect((el.querySelector("#unit-0-id") as HTMLInputElement).value).toBe("pico-led");
    expect(el.querySelector("#unit-0-node")!.textContent).toBe("Bind to 246f280102030000");
  });

  it("moves between cells with Ctrl + arrows and between rows with Up / Down in text cells", () => {
    const { el } = mountUnits();
    const id0 = el.querySelector("#unit-0-id") as HTMLInputElement;
    id0.focus();
    ctrl(id0, "ArrowRight", { ctrlKey: true });
    expect(document.activeElement?.id).toBe("unit-0-name");
    ctrl(document.activeElement!, "ArrowDown");
    expect(document.activeElement?.id).toBe("unit-1-name");
    ctrl(document.activeElement!, "ArrowRight", { ctrlKey: true });
    expect(document.activeElement?.id).toBe("unit-1-class");
    ctrl(document.activeElement!, "ArrowUp", { ctrlKey: true });
    expect(document.activeElement?.id).toBe("unit-0-class");
  });

  it("keeps focus across a re-render and puts a row's problems under it", () => {
    const { el, plant } = mountUnits();
    const cls = el.querySelector("#unit-1-class") as HTMLInputElement;
    cls.focus();
    cls.value = "1bad";
    cls.dispatchEvent(new Event("change", { bubbles: true }));
    expect(plant().physical.units[1]!.class).toBe("1bad");
    const again = el.querySelector("#unit-1-class") as HTMLInputElement;
    expect(again).not.toBe(cls);
    expect(document.activeElement).toBe(again);
    expect(again.getAttribute("aria-invalid")).toBe("true");
    const list = el.querySelector(`#${again.getAttribute("aria-describedby")}`)!;
    expect(list.textContent).toMatch(/class/i);
    expect(list.closest("tr")!.previousElementSibling).toBe(again.closest("tr"));
    expect(el.querySelector(".pin-summary")!.textContent).toMatch(/1 problem to fix/);
    expect((el.querySelector("#unit-0-class") as HTMLInputElement).getAttribute("aria-invalid")).toBeNull();
  });

  it("the unit picker and Bind drive the plant", () => {
    const { el, selected, plant } = mountUnits();
    const radios = [...el.querySelectorAll('input[name="unit-pick"]')] as HTMLInputElement[];
    expect(radios.map((r) => [r.checked, r.getAttribute("aria-label")])).toEqual([
      [true, "Edit pico-led in the other views"],
      [false, "Edit esp-sense in the other views"],
    ]);
    radios[1]!.checked = true;
    radios[1]!.dispatchEvent(new Event("change", { bubbles: true }));
    expect(selected).toEqual(["esp-sense"]);
    (el.querySelector("#unit-0-node") as HTMLButtonElement).click();
    expect(plant().physical.units[0]!.node).toBe("246f280102030000");
    expect(document.activeElement?.id).toBe("unit-0-node");
  });
});

describe("the Phases view (Q21)", () => {
  it("tables of labelled controls per model, problems under their row, focus kept", () => {
    let current = EXAMPLE_PLANT;
    const model = { states: ["Idle"], commands: ["Start"], next: [["Running", null]] };
    const v = phasesView({
      onChange: (p) => {
        current = p;
        v.render(p, "pico-led", plantIssues(p), model);
      },
      onSelect: () => {},
    });
    document.body.replaceChildren(v.el);
    v.render(EXAMPLE_PLANT, "pico-led", plantIssues(EXAMPLE_PLANT), model);
    const tables = [...v.el.querySelectorAll("table")];
    expect(tables.map((t) => t.querySelector("caption")!.textContent)).toEqual([
      "Equipment modules of the unit: each groups pins",
      "Phase classes of the plant",
      "Parameters of the phase classes",
      "Equipment phases of the unit",
      "Body flow of each phase per acting state",
      "The phase state model (read only): the state each command or Done leads to; a dash is refused and leaves the state as it is.",
    ]);
    for (const t of tables) expect(t.parentElement!.classList.contains("pin-scroll")).toBe(true);
    for (const c of v.el.querySelectorAll("table input, table select, table button")) expect(c.getAttribute("aria-label"), c.outerHTML).toBeTruthy();
    // An empty module id: a problem under that row, focus kept.
    const id = v.el.querySelector("#em-0-id") as HTMLInputElement;
    id.focus();
    id.value = "";
    id.dispatchEvent(new Event("change", { bubbles: true }));
    expect(current.physical.units[0]!.ems[0]!.id).toBe("");
    const again = v.el.querySelector("#em-0-id") as HTMLInputElement;
    expect(document.activeElement).toBe(again);
    expect(again.getAttribute("aria-invalid")).toBe("true");
    expect(v.el.querySelector(`#${again.getAttribute("aria-describedby")}`)!.textContent).toBeTruthy();
    // Ctrl + arrows within the parameters table.
    const p0 = v.el.querySelector("#par-0-id") as HTMLInputElement;
    p0.focus();
    ctrl(p0, "ArrowRight", { ctrlKey: true });
    expect(document.activeElement?.id).toBe("par-0-name");
    ctrl(document.activeElement!, "ArrowDown");
    expect(document.activeElement?.id).toBe("par-1-name");
  });
});
