// The payload model (IR, U21): a versioned document with a strict shape
// check, JSON round trip, and the lossless migration of older files.

import { describe, expect, it } from "vitest";
import { exampleWorkspace } from "../src/blocks/examples";
import { generate } from "../src/blocks/generator";
import { MAX_ROWS, MODEL_VERSION, makeModel, modelProblem, parseModel, serializeModel, workspaceOf } from "../src/model/payload";
import { MAX_MACHINES, type Expr, type Machine, event, state, transition } from "../src/model/machines";
import { boardDefaults, row } from "../src/model/pins";

/** A two-state machine used by the shape tests. */
const mode = (): Machine => ({
  id: "Mode",
  name: "LED mode",
  initial: "Off",
  states: [state("Off", [{ kind: "write", label: "LED", level: 0 }]), state("On", [{ kind: "write", label: "LED", level: 1 }], "lit")],
  events: [event("PRESS", "BUTTON pressed")],
  rows: [
    transition("Off", "PRESS", "On", [], { kind: "cmp", op: "eq", a: { kind: "pin", label: "BUTTON" }, b: { kind: "const", value: 0 } }),
    transition("On", "PRESS", "Off", [{ kind: "publish", key: "app/mode", text: "off" }]),
  ],
});
/** `not` nested `n` levels around a pin. */
const deep = (n: number): Expr => (n <= 1 ? { kind: "pin", label: "B" } : { kind: "not", a: deep(n - 1) });
import { TARGET_IDS, chipOf, profileOf } from "../src/model/target";

const blink = exampleWorkspace("blink")!.workspace;
const valid = () => JSON.parse(serializeModel(makeModel("pico2", blink, boardDefaults("pico2")))) as Record<string, unknown>;

describe("payload model", () => {
  it("round-trips through JSON unchanged", () => {
    const m = makeModel("esp32", blink, {
      rows: [row("gpio-out", "LED", 4), row("i2c-sda", "SDA", 26), row("i2c-scl", "SCL", 27)],
      buses: [{ kind: "i2c", bus: 0, hz: 400_000 }],
    });
    const r = parseModel(serializeModel(m), "pico2");
    expect(r).toEqual({ ok: true, model: m, migrated: false });
    expect(JSON.parse(serializeModel(m))).toMatchObject({ format: "middleware-payload", version: MODEL_VERSION, board: "esp32", machines: [] });
  });

  it("rejects unknown shapes instead of dropping them", () => {
    const cases: [(v: Record<string, unknown>) => void, string][] = [
      [(v) => (v.version = 3), "version 3 was written by a newer editor; this one reads version 2"],
      [(v) => (v.version = "1"), "version must be a whole number"],
      [(v) => (v.colour = "red"), 'unknown field "colour"'],
      [(v) => (v.board = "arduino"), 'board "arduino" is not a known board'],
      [(v) => ((v.pins as Record<string, unknown>).extra = []), 'pins has an unknown field "extra"'],
      [(v) => ((v.pins as { rows: unknown[] }).rows[0] = { ...row("gpio-out", "LED", 25), colour: 1 }), 'pins.rows[0] has an unknown field "colour"'],
      [(v) => ((v.pins as { rows: unknown[] }).rows[0] = { ...row("gpio-out", "LED", 25), pin: 2.5 }), "pins.rows[0].pin must be a whole number from 0 to 63, or null"],
      [(v) => ((v.pins as { rows: unknown[] }).rows[0] = { ...row("gpio-out", "LED", 25), pin: 64 }), "pins.rows[0].pin must be a whole number from 0 to 63, or null"],
      [(v) => ((v.pins as { rows: unknown[] }).rows[0] = { ...row("gpio-out", "LED", 25), func: "uart" }), expect.stringContaining("pins.rows[0].func must be one of gpio-out")],
      [(v) => ((v.pins as { rows: unknown[] }).rows[0] = { ...row("gpio-out", "LED", 25), initial: 2 }), "pins.rows[0].initial must be 0 or 1"],
      [(v) => ((v.pins as { rows: unknown[] }).rows = Array(MAX_ROWS + 1).fill(row("gpio-out", "X", 1))), "pins.rows must be a list of at most 64 rows"],
      [(v) => ((v.pins as { buses: unknown[] }).buses = [{ kind: "can", bus: 0 }]), 'pins.buses[0].kind must be "i2c" or "spi"'],
      [(v) => ((v.pins as { buses: unknown[] }).buses = [{ kind: "spi", bus: 0, khz: 1000 }]), "pins.buses[0] needs whole-number bus, khz and mode"],
      [(v) => (v.logic = []), "logic must hold exactly one Blocks workspace"],
      [(v) => (v.logic = [{ kind: "flow", nodes: [] }]), 'logic[0].kind must be "blocks" (other logic sources need a newer editor)'],
      [(v) => (v.logic = [{ kind: "blocks", workspace: { blocks: 3 } }]), 'logic[0].workspace is not a Blocks workspace: "blocks" must be an object'],
      [(v) => (v.machines = [{ states: [] }]), "machines[0].id must be text"],
      [(v) => ((v.version = 1), (v.machines = [mode()])), "machines must be empty in version 1"],
      [(v) => (v.machines = [{ ...mode(), colour: 1 }]), 'machines[0] has an unknown field "colour"'],
      [(v) => (v.machines = Array(MAX_MACHINES + 1).fill(mode())), "machines must be a list of at most 4 state machines"],
      [(v) => (v.machines = [{ ...mode(), rows: [{ ...transition("Off", "PRESS", "On"), guard: { kind: "pin" } }] }]), "machines[0].rows[0].guard.label must be text"],
      [(v) => (v.machines = [{ ...mode(), rows: [{ ...transition("Off", "PRESS", "On"), guard: deep(5) }] }]), "machines[0].rows[0].guard.a.a.a.a is nested deeper than 4 levels"],
      [(v) => (v.machines = [{ ...mode(), rows: [transition("Off", "PRESS", "On", Array(5).fill({ kind: "toggle", label: "LED" }))] }]), "machines[0].rows[0].actions must be a list of at most 4 actions"],
      [(v) => (v.machines = [{ ...mode(), rows: [transition("Off", "PRESS", "On", [{ kind: "write", label: "LED", level: 2 } as never])] }]), "machines[0].rows[0].actions[0].level must be 0 or 1"],
      [(v) => (v.machines = [{ ...mode(), rows: [transition("Off", "PRESS", "On", [{ kind: "exec", code: "x" } as never])] }]), "machines[0].rows[0].actions[0].kind must be one of write, toggle, publish, log, raise"],
      [(v) => (v.machines = [{ ...mode(), rows: [{ ...transition("Off", "PRESS", "On"), guard: { kind: "const", value: 2 ** 31 } }] }]), "machines[0].rows[0].guard.value must be a whole 32-bit number"],
    ];
    for (const [mutate, error] of cases) {
      const v = valid();
      mutate(v);
      expect(modelProblem(v)).toEqual(error);
      expect(parseModel(JSON.stringify(v), "esp32")).toEqual({ ok: false, error: expect.stringMatching(/^not a payload model: /) });
    }
    expect(modelProblem(valid())).toBeNull();
  });

  it("round-trips state machines and copies them", () => {
    const m = makeModel("pico2", blink, boardDefaults("pico2"), [mode()]);
    const r = parseModel(serializeModel(m), "esp32");
    expect(r).toEqual({ ok: true, model: m, migrated: false });
    if (!r.ok) return;
    expect(r.model.machines[0]).not.toBe(m.machines[0]);
  });

  it("migrates a version-1 model to version 2 losslessly (no machines, same code)", () => {
    const v1 = { ...valid(), version: 1, machines: [] };
    const r = parseModel(JSON.stringify(v1), "esp32");
    expect(r.ok && r.migrated).toBe(true);
    if (!r.ok) return;
    expect(r.model).toEqual(makeModel("pico2", blink, boardDefaults("pico2")));
    expect(r.model.version).toBe(2);
    expect(generate(workspaceOf(r.model), "esp-base", r.model.pins).source).toBe(generate(blink, "esp-base", boardDefaults("pico2")).source);
    expect(JSON.parse(serializeModel(r.model))).toMatchObject({ version: 2, machines: [] });
  });

  it("migrates a legacy Blocks export and a bare workspace losslessly", () => {
    const legacy = JSON.stringify({ format: "middleware-blocks", version: 1, profile: "esp-base", workspace: blink });
    const r = parseModel(legacy, "esp32s3");
    expect(r.ok && r.migrated).toBe(true);
    if (!r.ok) return;
    expect(r.model).toEqual(makeModel("esp32s3", blink));
    expect(generate(workspaceOf(r.model), "esp-base", r.model.pins).source).toBe(generate(blink, "esp-base").source);
    const bare = parseModel(JSON.stringify(blink), "pico", "badge");
    expect(bare.ok && bare.model.board).toBe("badge");
    // An unknown legacy profile falls back, as the Blocks import did.
    const odd = parseModel(JSON.stringify({ format: "middleware-blocks", profile: "toaster", workspace: {} }), "pico");
    expect(odd.ok && odd.model.board).toBe("pico");
  });

  it("maps every target to a profile and a pin-rule chip", () => {
    for (const t of TARGET_IDS) expect(profileOf(t)).toBe(t === "badge" ? "badge" : "esp-base");
    expect(chipOf("pico2")).toBe("rp2350");
    expect(chipOf("pico")).toBe("rp2040");
    expect(chipOf("esp32")).toBe("esp32");
    expect(chipOf("badge")).toBeNull();
  });
});
