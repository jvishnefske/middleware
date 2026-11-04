// No silent overwrite of the payload (U17): the Examples menu and Import
// JSON load straight away only when nothing would be lost, ask first when
// the payload holds different blocks or pins, and leave it alone when the
// file is not a payload. Examples add the pins they need and never change
// or remove a pin the user chose.

import { describe, expect, it } from "vitest";
import { EXAMPLES, exampleWorkspace } from "../src/blocks/examples";
import { exampleModel, planImport, wouldReplaceWork } from "../src/blocks/replace";
import { type PayloadModel, makeModel, serializeModel } from "../src/model/payload";
import { boardDefaults, row } from "../src/model/pins";

const blink = exampleWorkspace("blink")!;
const echo = exampleWorkspace("echo")!;
const chase = exampleWorkspace("chase")!;
const empty = makeModel("pico2", {});
const legacy = (profile: string, workspace: unknown) => JSON.stringify({ format: "middleware-blocks", version: 1, profile, workspace });
const blinkModel: PayloadModel = exampleModel(blink, makeModel("pico2", {}, boardDefaults("pico2")), "esp32");

describe("Examples menu", () => {
  it("asks before any example replaces a different one, never for itself", () => {
    for (const cur of EXAMPLES) {
      const current = exampleModel(cur, makeModel("pico2", {}, boardDefaults("pico2")), "pico2");
      for (const next of EXAMPLES) {
        expect(wouldReplaceWork(current, exampleModel(next, current, "pico2")), `${cur.id} -> ${next.id}`).toBe(cur.id !== next.id);
      }
      expect(wouldReplaceWork(makeModel("pico2", {}), exampleModel(cur, makeModel("pico2", {}), "pico2")), cur.id).toBe(false);
    }
  });

  it("adds the pins an example needs from the board, keeping the user's", () => {
    // Blink on a Pico 2 with no table gets LED on GPIO 25.
    expect(exampleModel(blink, makeModel("pico2", {}), "esp32").pins.rows).toEqual([row("gpio-out", "LED", 25, 0, "on-board LED")]);
    // A table that already has LED (on GPIO 15) keeps it, and its other rows.
    const mine = { rows: [row("gpio-out", "LED", 15), row("gpio-in", "BUTTON", 14)], buses: [] };
    expect(exampleModel(blink, makeModel("pico2", {}, mine), "esp32").pins).toEqual(mine);
    // The badge example moves to the badge; a default table is cleared, a custom one kept.
    expect(exampleModel(chase, makeModel("pico2", {}, boardDefaults("pico2")), "esp32")).toMatchObject({ board: "badge", pins: { rows: [] } });
    expect(exampleModel(chase, makeModel("pico2", {}, mine), "esp32").pins).toEqual(mine);
    // Back from the badge, the fallback board's defaults.
    expect(exampleModel(blink, makeModel("badge", chase.workspace), "esp32")).toMatchObject({ board: "esp32", pins: { rows: [{ label: "LED", pin: 4 }] } });
  });
});

describe("Import JSON", () => {
  it("reports a file that is not a payload and loads nothing", () => {
    const notJson = planImport("{ nope", blinkModel, "esp32");
    expect(notJson.kind === "error" && notJson.error).toMatch(/^not JSON/);
    const notWs = planImport(JSON.stringify({ blocks: 3 }), blinkModel, "esp32");
    expect(notWs.kind === "error" && notWs.error).toMatch(/^not a Blocks workspace/);
    const badModel = planImport(JSON.stringify({ ...blinkModel, extra: 1 }), blinkModel, "esp32");
    expect(badModel.kind === "error" && badModel.error).toBe('not a payload model: unknown field "extra"');
  });

  it("asks before an unrelated JSON object, which parses as an empty workspace, wipes work", () => {
    expect(planImport(JSON.stringify({ name: "package.json" }), blinkModel, "esp32").kind).toBe("confirm");
  });

  it("loads straight away over an empty payload", () => {
    const p = planImport(legacy("badge", chase.workspace), empty, "esp32");
    expect(p.kind).toBe("load");
    expect(p.kind !== "error" && p.model.board).toBe("badge");
    const m = planImport(serializeModel(blinkModel), empty, "esp32");
    expect(m.kind !== "error" && m.model).toEqual(blinkModel);
  });

  it("asks before an import replaces different blocks or pins", () => {
    const p = planImport(legacy("esp-base", echo.workspace), blinkModel, "esp32");
    expect(p.kind).toBe("confirm");
    expect(p.kind !== "error" && p.model.logic[0].workspace).toEqual(echo.workspace);
    // Same blocks, different pin table: still work to lose.
    const moved = { ...blinkModel, pins: { rows: [row("gpio-out", "LED", 15)], buses: [] } };
    expect(planImport(serializeModel(moved), blinkModel, "esp32").kind).toBe("confirm");
  });

  it("re-importing an export of the same payload does not ask", () => {
    const moved = structuredClone(blinkModel);
    for (const b of moved.logic[0].workspace.blocks?.blocks ?? []) b.x = (b.x ?? 0) + 40;
    expect(planImport(serializeModel(moved), blinkModel, "esp32").kind).toBe("load");
  });

  it("a bare workspace keeps the current board and pins; a legacy export brings its profile", () => {
    const bare = planImport(JSON.stringify(blink.workspace), blinkModel, "esp32");
    expect(bare.kind).toBe("load");
    expect(bare.kind !== "error" && bare.model).toMatchObject({ board: "pico2", pins: blinkModel.pins });
    // The chase blocks under the badge profile differ from the same blocks
    // under esp-base, where badge blocks generate nothing.
    const chaseHere = makeModel("pico2", chase.workspace);
    expect(planImport(legacy("badge", chase.workspace), chaseHere, "esp32").kind).toBe("confirm");
  });
});
