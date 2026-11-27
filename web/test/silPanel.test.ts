// @vitest-environment jsdom
//
// The simulation panel in a DOM (jsdom): labelled, keyboard-usable
// controls for the run and for every stimulus, the loopback editor, a run
// of a real payload through the engine with its status line, and the
// plots with their keyboard cursor and readout. Page layout (no sideways
// scroll at 390 px) is not visible in jsdom.

import { readFileSync } from "node:fs";
import { join } from "node:path";
import { afterEach, describe, expect, it } from "vitest";
import { compileWat } from "../src/editor/compile";
import type { SilGraph } from "../src/mwctl/sil";
import { loadProtocolSync } from "../src/protocol/wasm";
import { silPanel } from "../src/ui/silPanel";

// jsdom gives modules an http URL, so the file is found from the package root.
const proto = loadProtocolSync(readFileSync(join(process.cwd(), "src/pkg/protocol_wasm_bg.wasm")));

afterEach(() => document.body.replaceChildren());

/** GPIO 25 follows GPIO 15; ADC pin 26 is published on wire/raw. */
const FOLLOW = `(module
  (import "mw" "adc_read" (func $adc_read (param i32) (result i32)))
  (import "mw" "publish" (func $publish (param i32 i32 i32 i32) (result i32)))
  (import "mw" "set_tick" (func $set_tick (param i32) (result i32)))
  (import "mw" "gpio_input" (func $gpio_input (param i32 i32) (result i32)))
  (import "mw" "gpio_read" (func $gpio_read (param i32) (result i32)))
  (import "mw" "gpio_output" (func $gpio_output (param i32) (result i32)))
  (import "mw" "gpio_write" (func $gpio_write (param i32 i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 0) "wire/raw")
  (func (export "init")
    (drop (call $gpio_input (i32.const 15) (i32.const 0)))
    (drop (call $gpio_output (i32.const 25)))
    (drop (call $set_tick (i32.const 10))))
  (func (export "tick") (param i32)
    (drop (call $gpio_write (i32.const 25) (call $gpio_read (i32.const 15))))
    (i32.store (i32.const 64) (call $adc_read (i32.const 26)))
    (drop (call $publish (i32.const 0) (i32.const 8) (i32.const 64) (i32.const 4)))))`;

const GRAPH: SilGraph = {
  period_ms: 10,
  nodes: [
    { id: "gpio15", kind: "input", config: { pin: 15 } },
    { id: "button", kind: "channel" },
    { id: "adc26", kind: "analog", config: { pin: 26 } },
    { id: "raw", kind: "channel", config: { trace: true } },
    { id: "gpio25", kind: "output", config: { pin: 25 } },
  ],
  edges: [
    ["gpio15", "button"],
    ["adc26", "raw"],
    ["button", "gpio25"],
  ],
};

async function payload(): Promise<Uint8Array> {
  const r = await compileWat(FOLLOW);
  if (!r.ok) throw new Error(r.errors);
  return r.wasm;
}

function mount(build: () => Promise<Uint8Array> = payload) {
  const panel = silPanel({ proto, build });
  document.body.replaceChildren(panel.el);
  const q = <T extends Element>(sel: string) => panel.el.querySelector<T>(sel)!;
  const button = (text: string) => [...panel.el.querySelectorAll("button")].find((b) => b.textContent === text)!;
  const status = () => q<HTMLElement>(".sil-status").textContent ?? "";
  const settled = async () => {
    for (let i = 0; i < 2000 && !/^(Simulated|Stopped|Cannot)/.test(status()); i++) await new Promise((done) => setTimeout(done, 1));
  };
  return { panel, q, button, status, settled };
}

function set(input: HTMLInputElement | HTMLSelectElement, value: string): void {
  input.value = value;
  input.dispatchEvent(new Event("change", { bubbles: true }));
}

describe("the simulation panel", () => {
  it("SIL18: every control is labelled, and Run waits for a wiring that builds", () => {
    const { panel, q, button, status } = mount();
    expect(q(".sil-status").getAttribute("aria-live")).toBe("polite");
    expect(button("Run").disabled).toBe(true);
    panel.setGraph(GRAPH, false);
    expect(button("Run").disabled).toBe(true);
    expect(status()).toBe("Fix the wiring's problems to simulate it.");
    panel.setGraph(GRAPH, true);
    expect(button("Run").disabled).toBe(false);
    expect(button("Stop").disabled).toBe(true);
    expect(status()).toBe("");

    expect(q<HTMLInputElement>('[aria-label="Duration in milliseconds"]').value).toBe("2000");
    expect(q<HTMLInputElement>('[aria-label="Step in milliseconds"]').value).toBe("10");
    const speed = q<HTMLSelectElement>('[aria-label="Simulation speed"]');
    expect([...speed.options].map((o) => o.textContent)).toEqual(["1× real time", "10×", "100×", "1000×", "max"]);
    expect(speed.selectedOptions[0].textContent).toBe("max");

    const rows = [...panel.el.querySelectorAll(".sil-stim")];
    expect(rows.map((r) => r.getAttribute("aria-label"))).toEqual(["Stimulus for gpio15 → button", "Stimulus for adc26 → raw"]);
    expect(rows.map((r) => r.querySelector("select")!.value)).toEqual(["square", "sine"]);
    // A digital pin is not asked for levels; an analog one is.
    const labels = (row: Element) => [...row.querySelectorAll("label")].map((l) => l.textContent!.trim());
    expect(labels(rows[0])).toEqual(["period ms", "duty 0–1", "phase ms"]);
    expect(labels(rows[1])).toEqual(["period ms", "min", "max"]);
    // Every field is a native control with a name: reachable by Tab.
    for (const c of panel.el.querySelectorAll("input, select")) {
      expect(c.getAttribute("aria-label") || c.closest("label")?.textContent, c.outerHTML).toBeTruthy();
      expect(c.getAttribute("tabindex")).toBeNull();
    }
  });

  it("SIL19: choosing a waveform shows its parameters, and the select keeps the focus", () => {
    const { panel } = mount();
    panel.setGraph(GRAPH, true);
    const row = panel.el.querySelector(".sil-stim")!;
    const kind = row.querySelector("select")!;
    kind.focus();
    set(kind, "pulse");
    expect(document.activeElement).toBe(kind);
    expect([...row.querySelectorAll("label")].map((l) => l.textContent!.trim())).toEqual(["at ms", "width ms"]);
    const at = row.querySelector<HTMLInputElement>('[aria-label="gpio15 → button: at ms"]')!;
    expect(at.value).toBe("500");
    // Text that is not a number is put back.
    set(at, "");
    expect(at.value).toBe("500");
    set(at, "40");
    // Moving a node changes nothing the panel shows: the edit stays.
    panel.setGraph({ ...GRAPH, nodes: GRAPH.nodes.map((n) => ({ ...n })) }, true);
    expect(panel.el.querySelector(".sil-stim select")).toBe(kind);
    // A changed wiring keeps the waveform of the pin that is still there.
    panel.setGraph({ ...GRAPH, nodes: GRAPH.nodes.filter((n) => n.id !== "adc26"), edges: GRAPH.edges.filter(([a]) => a !== "adc26") }, true);
    expect(panel.el.querySelectorAll(".sil-stim").length).toBe(1);
    expect(panel.el.querySelector<HTMLInputElement>('[aria-label="gpio15 → button: at ms"]')!.value).toBe("40");
  });

  it("SIL20: a loopback is added from an output to an input and removed again", () => {
    const { panel, q, button } = mount();
    panel.setGraph({ ...GRAPH, nodes: GRAPH.nodes.filter((n) => n.kind !== "output") }, true);
    expect(panel.el.querySelector(".sil-loops")!.textContent).toContain("this wiring needs one of each");
    panel.setGraph(GRAPH, true);
    const from = q<HTMLSelectElement>('[aria-label="Loopback from output"]');
    const to = q<HTMLSelectElement>('[aria-label="Loopback to input"]');
    expect([...from.options].map((o) => o.textContent)).toEqual(["button → gpio25"]);
    expect([...to.options].map((o) => o.textContent)).toEqual(["gpio15 → button", "adc26 → raw"]);
    set(to, "1");
    set(q<HTMLInputElement>('[aria-label="Loopback lag in milliseconds"]'), "50");
    button("Add loopback").click();
    const items = () => [...panel.el.querySelectorAll(".sil-looplist li")].map((li) => li.firstChild!.textContent);
    expect(items()).toEqual(["button → gpio25  feeds  adc26 → raw, lag 50 ms"]);
    expect(panel.el.querySelectorAll(".sil-stim")[1].textContent).toContain("replaced by a loopback");
    // A second loopback to the same input replaces the first.
    set(q<HTMLSelectElement>('[aria-label="Loopback to input"]'), "1");
    button("Add loopback").click();
    expect(items()).toEqual(["button → gpio25  feeds  adc26 → raw, no lag"]);
    q<HTMLButtonElement>(".sil-looplist button").click();
    expect(items()).toEqual([]);
    expect(panel.el.querySelectorAll(".sil-stim")[1].textContent).not.toContain("replaced by a loopback");
  });

  it("SIL21: Run simulates the built payload, says how fast, and plots every trace on one time axis", async () => {
    const { panel, q, button, status, settled } = mount();
    panel.setGraph(GRAPH, true);
    set(q<HTMLInputElement>('[aria-label="Duration in milliseconds"]'), "1000");
    button("Run").click();
    expect(button("Run").disabled).toBe(true);
    expect(button("Stop").disabled).toBe(false);
    await settled();
    expect(status()).toMatch(/^Simulated 1\.0 s in \d+\.\d\d s \(\d+(\.\d)?× real time\)$/);
    expect(button("Run").disabled).toBe(false);
    expect(button("Stop").disabled).toBe(true);

    const svg = q<SVGSVGElement>("svg.sil-plot");
    const width = Number(svg.getAttribute("width"));
    expect(svg.getAttribute("viewBox")).toBe(`0 0 ${width} ${svg.getAttribute("height")}`);
    expect([...svg.querySelectorAll("text.sil-label")].map((t) => t.firstChild!.textContent)).toEqual(["gpio15 → button", "adc26 → raw", "raw", "button → gpio25"]);
    const paths = [...svg.querySelectorAll("path.sil-trace")];
    expect(paths.map((p) => p.getAttribute("class"))).toEqual(["sil-trace k-input", "sil-trace k-analog", "sil-trace k-channel", "sil-trace k-output"]);
    for (const p of paths) {
      const d = p.getAttribute("d")!;
      expect(d).toMatch(/^M[\d.]+ [\d.]+(H[\d.]+|V[\d.]+)+$/);
      // Nothing is drawn outside the drawing.
      for (const m of d.matchAll(/[MH]([\d.]+)/g)) expect(Number(m[1])).toBeLessThanOrEqual(width);
    }
    // Numeric lanes say their scale (half a period of the default sine);
    // the axis is in seconds.
    expect([...svg.querySelectorAll("text.sil-scale")].map((t) => t.textContent)).toEqual(expect.arrayContaining(["min 2048 · max 4095", "0 s", "1 s"]));

    // The keyboard cursor: the readout gives the time and every value.
    const readout = () => q<HTMLElement>(".sil-readout").textContent ?? "";
    expect(readout()).toContain("use ← →");
    expect(svg.getAttribute("tabindex")).toBe("0");
    const key = (k: string) => svg.dispatchEvent(new KeyboardEvent("keydown", { key: k, bubbles: true }));
    key("ArrowRight");
    expect(readout()).toBe("t = 0 sgpio15 → button: highadc26 → raw: 2048raw: –button → gpio25: low");
    key("End");
    expect(readout()).toMatch(/^t = 1 sgpio15 → button: (high|low)adc26 → raw: \d+raw: \d+button → gpio25: (high|low)$/);
    key("ArrowLeft");
    expect(readout()).toContain("t = 0.99 s");
    expect(svg.querySelector(".sil-cross")!.getAttribute("visibility")).toBe("visible");
    key("Escape");
    expect(svg.querySelector(".sil-cross")!.getAttribute("visibility")).toBe("hidden");
    // The payload's log would be listed; this one logs nothing.
    expect(panel.el.querySelector(".sil-log")).toBeNull();
  });

  it("SIL22: a wiring that does not build, a trap and Stop are reported in the status line", async () => {
    const failing = mount(() => Promise.reject(new Error("this board has no GPIO 99")));
    failing.panel.setGraph(GRAPH, true);
    failing.button("Run").click();
    await failing.settled();
    expect(failing.status()).toBe("Cannot simulate: this board has no GPIO 99");
    expect(failing.button("Run").disabled).toBe(false);

    const trap = await compileWat(`(module (import "mw" "set_tick" (func $t (param i32) (result i32))) (memory (export "memory") 1)
      (func (export "init") (drop (call $t (i32.const 10)))) (func (export "tick") (param i32) (unreachable)))`);
    if (!trap.ok) throw new Error(trap.errors);
    const trapping = mount(() => Promise.resolve(trap.wasm));
    trapping.panel.setGraph(GRAPH, true);
    trapping.button("Run").click();
    await trapping.settled();
    expect(trapping.panel.el.querySelector(".sil-error")!.textContent).toMatch(/^Ended at 10 ms: /);
    expect(trapping.panel.el.querySelector(".sil-error")!.getAttribute("role")).toBe("alert");

    // Stop at 1× real time: a 60 s run ends at once, with what it has.
    const slow = mount();
    slow.panel.setGraph(GRAPH, true);
    set(slow.q<HTMLInputElement>('[aria-label="Duration in milliseconds"]'), "60000");
    set(slow.q<HTMLSelectElement>('[aria-label="Simulation speed"]'), "0");
    slow.button("Run").click();
    await new Promise((done) => setTimeout(done, 30));
    expect(slow.status()).toMatch(/^Simulating: /);
    slow.panel.stop();
    await slow.settled();
    expect(slow.status()).toMatch(/^Stopped\. Simulated 0\.0 s in 0\.\d\d s/);
    expect(slow.panel.el.querySelector("svg.sil-plot")).not.toBeNull();
  });
});
