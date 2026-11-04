// Blocks + pin table codegen (U25): the table generates init's setup
// (gpio_output + initial level, gpio_input with pull, i2c_open per I2C
// bus, spi_open for the SPI bus) and one constant per labelled pin; pin
// blocks name a label; the table + Blink example compiles with asc and
// blinks the label's pin in the simulator; a table with problems stops the
// build before compiling.

import { describe, expect, it } from "vitest";
import { exampleWorkspace } from "../src/blocks/examples";
import { generate } from "../src/blocks/generator";
import { buildPayload } from "../src/blocks/pipeline";
import type { BlockJson, WorkspaceJson } from "../src/blocks/workspace";
import { type PinTable, boardDefaults, checkTable, normalizeBuses, row, withNeeds } from "../src/model/pins";
import type { SimEvent } from "../src/protocol/wasm";
import { SimSession } from "../src/sim/session";
import { ManualClock } from "./clock";
import { protocol } from "./fakeDevice";

const proto = protocol();
const ws = (...blocks: BlockJson[]): WorkspaceJson => ({ blocks: { languageVersion: 0, blocks } });

const table: PinTable = normalizeBuses({
  rows: [
    { ...row("gpio-out", "LED", 25), initial: 1 },
    { ...row("gpio-in", "BUTTON", 15), pull: "up" },
    row("adc", "POT", 26),
    row("i2c-sda", "SDA", 4),
    row("i2c-scl", "SCL", 5),
    row("spi-sck", "SCK", 18),
    row("spi-mosi", "MOSI", 19),
    row("spi-cs", "CS", 17),
  ],
  buses: [{ kind: "i2c", bus: 0, hz: 400_000 }],
});

describe("pin table -> AssemblyScript", () => {
  it("emits the pin constants and the init setup from the table", () => {
    const g = generate(ws(), "esp-base", table);
    expect(g.errors).toEqual([]);
    expect(g.source).toContain(
      [
        "// Pins (from the Pins table).",
        "const PIN_LED: i32 = 25; // GPIO output",
        "const PIN_BUTTON: i32 = 15; // GPIO input",
        "const PIN_POT: i32 = 26; // analog input (adc_read)",
        "const PIN_SDA: i32 = 4; // I2C SDA",
        "const PIN_SCL: i32 = 5; // I2C SCL",
        "const PIN_SCK: i32 = 18; // SPI SCK",
        "const PIN_MOSI: i32 = 19; // SPI MOSI",
        "const PIN_CS: i32 = 17; // SPI CS",
      ].join("\n"),
    );
    expect(g.source).toContain(
      [
        "export function init(): void {",
        "  mw_gpio_output(PIN_LED);",
        "  pinWrite(PIN_LED, 1);",
        "  mw_gpio_input(PIN_BUTTON, 1);",
        "  mw_i2c_open(0, 400000);",
        "  mw_spi_open(0, PIN_SCK, PIN_MOSI, -1, PIN_CS, 1000, 0);",
        "}",
      ].join("\n"),
    );
    // Imports in ABI order, only what is used.
    expect(g.imports).toEqual(["gpio_output", "gpio_input", "gpio_write", "spi_open", "i2c_open"]);
  });

  it("label blocks use the constants and check the row's function", () => {
    const w = ws(
      { type: "mw_every", fields: { MS: 100 }, inputs: { DO: { block: { type: "mw_pin_toggle", id: "t", fields: { LABEL: "LED" } } } } },
      { type: "mw_on_pin", id: "p", fields: { LABEL: "BUTTON", EDGE: "2" }, inputs: { DO: { block: { type: "mw_pin_write", id: "w", fields: { LABEL: "LED" }, inputs: { LEVEL: { block: { type: "mw_pin_read", fields: { LABEL: "BUTTON" } } } } } } } },
    );
    const g = generate(w, "esp-base", table);
    expect(g.errors).toEqual([]);
    expect(g.source).toContain("    pinToggle(PIN_LED);");
    expect(g.source).toContain("    pinWrite(PIN_LED, mw_gpio_read(PIN_BUTTON));");
    // The edge handler configures BUTTON (with the table's pull) and its interrupt.
    expect(g.source).toContain("  mw_gpio_input(15, 1);\n  mw_gpio_irq(15, 2);");
    expect(g.source).not.toContain("mw_gpio_input(PIN_BUTTON, 1);");
    expect(g.source).toContain("if (kind == 1 && source == 15 && value == 0) {");

    const bad = generate(
      ws(
        { type: "mw_every", fields: { MS: 100 }, inputs: { DO: { block: { type: "mw_pin_toggle", id: "t", fields: { LABEL: "LAMP" } } } } },
        { type: "mw_on_pin", id: "p", fields: { LABEL: "LED", EDGE: "1" } },
        { type: "mw_on_init", inputs: { DO: { block: { type: "mw_pin_write", id: "w", fields: { LABEL: "POT" } } } } },
      ),
      "esp-base",
      table,
    );
    expect(bad.errors).toEqual([
      { blockId: "t", message: 'no pin is labelled "LAMP" in the Pins table; add it there' },
      { blockId: "p", message: "LED is not a GPIO input in the Pins table" },
      { blockId: "w", message: "POT is not a GPIO output in the Pins table" },
    ]);
  });

  it("an empty table changes nothing (numeric pin blocks keep working)", () => {
    const w = ws({ type: "mw_on_init", inputs: { DO: { block: { type: "mw_gpio_output", fields: { PIN: 8 } } } } });
    expect(generate(w, "esp-base", { rows: [], buses: [] }).source).toBe(generate(w, "esp-base").source);
    expect(generate(w, "esp-base").source).not.toContain("Pins table");
  });

  it("a table with problems stops the build before compiling", async () => {
    const t = normalizeBuses({ rows: [row("gpio-out", "LED", 23)], buses: [] });
    const issues = checkTable(proto.checkPins, "pico2", t);
    const r = await buildPayload(exampleWorkspace("blink")!.workspace, "esp-base", proto.abi, { pins: t, issues });
    expect(r.ok).toBe(false);
    expect(!r.ok && r.stage).toBe("pins");
    expect(!r.ok && r.errors).toEqual([
      "Pins row 1 (LED): GPIO 23 is reserved on this board (the regulator's power-save control); choose another pin.",
    ]);
  });
});

describe.runIf(proto.hasSimulator)("table + Blink in the simulator", () => {
  for (const [target, pin] of [
    ["pico2", 25],
    ["esp32", 4],
  ] as const) {
    it(`blinks the LED label on GPIO ${pin} (${target})`, async () => {
      const blink = exampleWorkspace("blink")!;
      const pins = withNeeds(boardDefaults(target), target, blink.needs);
      const issues = checkTable(proto.checkPins, target, pins);
      expect(issues).toEqual([]);
      const r = await buildPayload(blink.workspace, "esp-base", proto.abi, { pins, issues });
      if (!r.ok) throw new Error(`${r.stage}: ${r.errors.join("; ")}`);
      expect(r.source).toContain(`const PIN_LED: i32 = ${pin};`);
      const clock = new ManualClock();
      const events: SimEvent[] = [];
      const errors: string[] = [];
      const s = new SimSession(proto, r.wasm, { onEvent: (e) => events.push(e), onError: (m) => errors.push(m) }, clock);
      s.start();
      expect(s.pins.get(pin)).toEqual({ mode: "output", high: false });
      clock.advance(1600);
      expect(errors).toEqual([]);
      const writes = events.flatMap((e) => (e.kind === "write" && e.pin === pin ? [e.high] : []));
      // The initial low from the table, then three toggles.
      expect(writes).toEqual([false, true, false, true]);
      expect(events.some((e) => e.kind === "log" && e.line === "blink: started")).toBe(true);
      s.dispose();
    });
  }
});
