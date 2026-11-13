// The pin table (U22-U24): live checks come from Rust (protocol-wasm
// checkPins over board-pins, the firmwares' own rules), the table's
// transitions are pure, and switching boards re-checks with fixes.

import { describe, expect, it } from "vitest";
import {
  EMPTY_PINS,
  type PinTable,
  addRows,
  applyFix,
  boardDefaults,
  checkTable,
  labelsUsed,
  normalizeBuses,
  removeRow,
  renameLabel,
  row,
  seedForTarget,
  setRow,
} from "../src/model/pins";
import { protocol } from "./fakeDevice";

const proto = protocol();
const check = (target: Parameters<typeof checkTable>[1], t: PinTable) => checkTable(proto.checkPins, target, t);
const messages = (target: Parameters<typeof checkTable>[1], t: PinTable) => check(target, t).map((i) => `${i.row ?? "-"} ${i.field}: ${i.message}`);

const full: PinTable = normalizeBuses({
  rows: [
    row("gpio-out", "LED", 25),
    row("gpio-in", "BUTTON", 15),
    row("adc", "POT", 26),
    row("i2c-sda", "SDA", 4),
    row("i2c-scl", "SCL", 5),
    row("spi-sck", "SCK", 18),
    row("spi-mosi", "MOSI", 19),
    row("spi-miso", "MISO", 16),
    row("spi-cs", "CS", 17),
  ],
  buses: [],
});

describe("pin table checks (Rust)", () => {
  it("a complete Pico 2 table is clean; the board map comes from Rust", () => {
    expect(check("pico2", full)).toEqual([]);
    const info = proto.boardPins("rp2350");
    expect(info.gpios).toHaveLength(30);
    expect(info.gpios.find((g) => g.pin === 23)?.reserved).toBe("the regulator's power-save control");
    expect(info.gpios.find((g) => g.pin === 4)).toMatchObject({ i2c: { bus: 0, line: "sda" }, spi: { block: 0, signal: "miso" } });
    expect(info.adc).toEqual([26, 27, 28]);
    expect(proto.boardPins("esp32").pwm.map((p) => p.pin)).toEqual([0, 4, 13, 14]);
    expect(() => proto.boardPins("z80")).toThrow(/no pin map/);
  });

  it("reports duplicate pins and labels, reserved and incapable pins in plain language", () => {
    const t = normalizeBuses({
      rows: [row("gpio-out", "LED", 25), row("gpio-out", "led", 25), row("gpio-out", "PWR", 23), row("adc", "POT", 2), row("gpio-out", "", 3)],
      buses: [],
    });
    expect(messages("pico2", t)).toEqual([
      "0 label: The label LED is also used on row 2; labels must be unique.",
      "0 pin: GPIO 25 is also used by led (row 2); a pin can do one thing.",
      "1 label: The label led is also used on row 1; labels must be unique.",
      "1 pin: GPIO 25 is also used by LED (row 1); a pin can do one thing.",
      "2 pin: GPIO 23 is reserved on this board (the regulator's power-save control); choose another pin.",
      "3 pin: GPIO 2 has no analog input; the analog inputs are GPIO 26, 27 and 28.",
      "4 label: Give this pin a label, for example LED.",
    ]);
    expect(check("pico2", t)[5]?.fix).toEqual({ kind: "pin", pin: 26 });
  });

  it("checks SPI controllers and incomplete buses, with fixes", () => {
    const mixed = setRow(full, 6, { pin: 11 });
    const [issue] = check("pico2", mixed);
    expect(issue).toEqual({
      row: 6,
      field: "pin",
      message: "SCK on GPIO 18 is SPI0 but MOSI on GPIO 11 is SPI1; all SPI pins must be on one controller.",
      fix: { kind: "pin", pin: 3 },
    });
    expect(check("pico2", applyFix(mixed, issue!.row, issue!.fix!))).toEqual([]);
    const half = removeRow(full, 4);
    const [missing] = check("pico2", half);
    expect(missing).toMatchObject({ row: null, message: "I2C bus 0 needs an SCL row too (GPIO 5 on this board).", fix: { kind: "add", func: "i2c-scl", bus: 0, pin: 5 } });
    const fixed = applyFix(half, null, missing!.fix!);
    expect(fixed.rows.at(-1)).toMatchObject({ label: "SCL", func: "i2c-scl", pin: 5 });
    expect(check("pico2", fixed)).toEqual([]);
  });

  it("switching boards re-checks the same table, with the board's fixes", () => {
    // The Pico table on the ESP32-S3: I2C bus 0 is wired to 5 / 6 there, and
    // its analog inputs are ADC1's, GPIO 1-10.
    const s3 = check("esp32s3", full);
    const text = s3.map((i) => i.message);
    expect(text).toContain("I2C bus 0 SDA is fixed at GPIO 5 on this board.");
    expect(text).toContain("GPIO 26 has no analog input; the analog inputs are GPIO 1, 2, 3, 4, 5, 6, 7, 8, 9 and 10.");
    expect(s3.find((i) => i.message.startsWith("I2C bus 0 SDA"))?.fix).toEqual({ kind: "pin", pin: 5 });
    // On the ESP32 (ESP32-CAM) GPIO 16 is the PSRAM.
    expect(check("esp32", full).map((i) => i.message)).toContain("GPIO 16 is reserved on this board (the PSRAM); choose another pin.");
    // The badge has no pins at all.
    expect(check("badge", full)).toHaveLength(1);
    expect(check("badge", EMPTY_PINS)).toEqual([]);
  });

  it("checks the bus settings against the ABI ranges", () => {
    const slow = { ...full, buses: full.buses.map((b) => (b.kind === "i2c" ? { ...b, hz: 5 } : { ...b, mode: 7 })) };
    expect(messages("pico2", slow)).toEqual([
      "- setting: The I2C bus 0 clock must be 10000 to 1000000 Hz.",
      "- setting: The SPI mode must be 0, 1, 2 or 3.",
    ]);
  });
});

describe("pin table transitions", () => {
  it("keeps one setting per bus in use", () => {
    expect(full.buses).toEqual([
      { kind: "i2c", bus: 0, hz: 100_000 },
      { kind: "spi", bus: 0, khz: 1000, mode: 0 },
    ]);
    const noSpi = full.rows.reduce<PinTable>((t, r, i) => (r.func.startsWith("spi-") ? t : addRows(t, full.rows[i]!)), EMPTY_PINS);
    expect(noSpi.buses).toEqual([{ kind: "i2c", bus: 0, hz: 100_000 }]);
  });

  it("seeds a board's defaults only over an empty or untouched table", () => {
    expect(seedForTarget(EMPTY_PINS, "esp32", "pico2")).toEqual(boardDefaults("pico2"));
    expect(seedForTarget(boardDefaults("esp32"), "esp32", "pico2")).toEqual(boardDefaults("pico2"));
    const mine = setRow(boardDefaults("esp32"), 0, { pin: 13 });
    expect(seedForTarget(mine, "esp32", "pico2")).toBe(mine);
  });

  it("renames labels in pin blocks and lists the labels used", () => {
    const ws = {
      blocks: {
        languageVersion: 0,
        blocks: [
          { type: "mw_every", fields: { MS: 100 }, inputs: { DO: { block: { type: "mw_pin_toggle", fields: { LABEL: "LED" } } } } },
          { type: "mw_on_pin", fields: { LABEL: "BUTTON", EDGE: "2" } },
          { type: "mw_gpio_toggle", fields: { PIN: 8, LABEL: "LED" } },
        ],
      },
    };
    expect(labelsUsed(ws)).toEqual(["BUTTON", "LED"]);
    const renamed = renameLabel(ws, "LED", "LAMP");
    expect(labelsUsed(renamed)).toEqual(["BUTTON", "LAMP"]);
    // Only pin blocks follow; the original is untouched.
    expect(renamed.blocks?.blocks?.[2]?.fields).toEqual({ PIN: 8, LABEL: "LED" });
    expect(labelsUsed(ws)).toEqual(["BUTTON", "LED"]);
  });
});
