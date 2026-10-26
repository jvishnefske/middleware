// Blocks editor, generator side: block JSON -> AssemblyScript golden files
// (test/fixtures/blocks/*.as), import and export selection, and the
// checks that keep generated code bounded and allocation-free.

import { describe, expect, it } from "vitest";
import { BLOCK_DEFS, BUILTIN_BLOCKS, EVENT_BLOCKS, blockSupported, featureOf, toolbox } from "../src/blocks/defs";
import { EXAMPLES } from "../src/blocks/examples";
import { generate, keyProblem } from "../src/blocks/generator";
import { buildPayload } from "../src/blocks/pipeline";
import type { BlockJson, WorkspaceJson } from "../src/blocks/workspace";
import { compileAssemblyScript } from "../src/editor/compile";
import { protocol } from "./fakeDevice";

const proto = protocol();

const ws = (...blocks: BlockJson[]): WorkspaceJson => ({ blocks: { languageVersion: 0, blocks } });
const num = (n: number) => ({ shadow: { type: "math_number", fields: { NUM: n } } });
const txt = (s: string) => ({ shadow: { type: "text", fields: { TEXT: s } } });
const getVar = (id: string) => ({ block: { type: "variables_get", fields: { VAR: { id } } } });
/** Chains statement blocks with `next`. */
function chain(...blocks: BlockJson[]): { block: BlockJson } {
  const [first, ...rest] = blocks;
  if (!first) throw new Error("empty chain");
  return { block: rest.length > 0 ? { ...first, next: chain(...rest) } : first };
}
const onInit = (...body: BlockJson[]): BlockJson => ({ type: "mw_on_init", inputs: { DO: chain(...body) } });

/** Every built-in the generator supports, in one workspace. */
const LOGIC: WorkspaceJson = {
  variables: [
    { name: "n", id: "n" },
    { name: "i", id: "i" },
    { name: "total value", id: "t" },
  ],
  blocks: {
    languageVersion: 0,
    blocks: [
      {
        type: "mw_every",
        fields: { MS: 1000 },
        inputs: {
          DO: chain(
            { type: "variables_set", fields: { VAR: { id: "t" } }, inputs: { VALUE: num(0) } },
            {
              type: "controls_for",
              fields: { VAR: { id: "i" } },
              inputs: {
                FROM: num(10),
                TO: num(0),
                BY: num(2),
                DO: chain(
                  {
                    type: "controls_if",
                    extraState: { elseIfCount: 1, hasElse: true },
                    inputs: {
                      IF0: {
                        block: {
                          type: "math_number_property",
                          fields: { PROPERTY: "DIVISIBLE_BY" },
                          inputs: { NUMBER_TO_CHECK: getVar("i"), DIVISOR: num(4) },
                        },
                      },
                      DO0: chain({ type: "controls_flow_statements", fields: { FLOW: "CONTINUE" } }),
                      IF1: {
                        block: {
                          type: "logic_operation",
                          fields: { OP: "OR" },
                          inputs: {
                            A: { block: { type: "logic_compare", fields: { OP: "GTE" }, inputs: { A: getVar("i"), B: num(8) } } },
                            B: { block: { type: "logic_negate", inputs: { BOOL: { block: { type: "logic_boolean", fields: { BOOL: "TRUE" } } } } } },
                          },
                        },
                      },
                      DO1: chain({ type: "math_change", fields: { VAR: { id: "t" } }, inputs: { DELTA: num(100) } }),
                      ELSE: chain({ type: "math_change", fields: { VAR: { id: "t" } }, inputs: { DELTA: getVar("i") } }),
                    },
                  },
                ),
              },
            },
            {
              type: "controls_repeat_ext",
              inputs: {
                TIMES: num(3),
                DO: chain({
                  type: "variables_set",
                  fields: { VAR: { id: "n" } },
                  inputs: {
                    VALUE: {
                      block: {
                        type: "math_constrain",
                        inputs: {
                          VALUE: {
                            block: {
                              type: "math_arithmetic",
                              fields: { OP: "POWER" },
                              inputs: { A: { block: { type: "math_random_int", inputs: { FROM: num(1), TO: num(6) } } }, B: num(2) },
                            },
                          },
                          LOW: num(0),
                          HIGH: num(20),
                        },
                      },
                    },
                  },
                }),
              },
            },
            {
              type: "text_print",
              inputs: {
                TEXT: {
                  block: {
                    type: "text_join",
                    extraState: { itemCount: 4 },
                    inputs: {
                      ADD0: txt("total="),
                      ADD1: getVar("t"),
                      ADD2: txt(" n="),
                      ADD3: {
                        block: {
                          type: "logic_ternary",
                          inputs: {
                            IF: { block: { type: "math_number_property", fields: { PROPERTY: "ODD" }, inputs: { NUMBER_TO_CHECK: getVar("n") } } },
                            THEN: { block: { type: "math_single", fields: { OP: "NEG" }, inputs: { NUM: getVar("n") } } },
                            ELSE: {
                              block: {
                                type: "math_arithmetic",
                                fields: { OP: "DIVIDE" },
                                inputs: { A: { block: { type: "math_modulo", inputs: { DIVIDEND: getVar("n"), DIVISOR: num(7) } } }, B: num(0) },
                              },
                            },
                          },
                        },
                      },
                    },
                  },
                },
              },
            },
            { type: "controls_repeat", fields: { TIMES: 2 }, inputs: { DO: chain({ type: "controls_flow_statements", fields: { FLOW: "BREAK" } }) } },
          ),
        },
      },
    ],
  },
};

/** GPIO, a pin interrupt, a timer and two ticks with different periods. */
const IRQ: WorkspaceJson = ws(
  onInit(
    { type: "mw_gpio_output", fields: { PIN: 8 } },
    { type: "mw_gpio_input", fields: { PIN: 5, PULL: "2" } },
    { type: "mw_timer_start", fields: { ID: 0, MS: 250, MODE: "1" } },
  ),
  {
    type: "mw_on_pin_edge",
    fields: { PIN: 0, PULL: "1", EDGE: "2" },
    inputs: { DO: chain({ type: "mw_gpio_write", fields: { PIN: 8 }, inputs: { LEVEL: { block: { type: "mw_pin_level" } } } }) },
  },
  { type: "mw_on_pin_edge", fields: { PIN: 0, PULL: "1", EDGE: "1" }, inputs: { DO: chain({ type: "mw_log", inputs: { TEXT: txt("released") } }) } },
  { type: "mw_on_timer", fields: { ID: 0 }, inputs: { DO: chain({ type: "mw_timer_stop", fields: { ID: 0 } }) } },
  { type: "mw_every", fields: { MS: 300 }, inputs: { DO: chain({ type: "mw_gpio_toggle", fields: { PIN: 8 } }) } },
  {
    type: "mw_every",
    fields: { MS: 500 },
    inputs: {
      DO: chain({
        type: "mw_publish",
        fields: { KEY: "app/state" },
        inputs: {
          DATA: {
            block: {
              type: "math_arithmetic",
              fields: { OP: "ADD" },
              inputs: { A: { block: { type: "mw_gpio_read", fields: { PIN: 5 } } }, B: { block: { type: "mw_now_ms" } } },
            },
          },
        },
      }),
    },
  },
);

const GOLDEN: [string, WorkspaceJson, "esp-base" | "badge"][] = [
  ...EXAMPLES.map((e): [string, WorkspaceJson, "esp-base" | "badge"] => [e.id, e.workspace, e.profile]),
  ["logic", LOGIC, "esp-base"],
  ["irq", IRQ, "esp-base"],
];

describe("AssemblyScript generator golden files", () => {
  for (const [name, w, profile] of GOLDEN) {
    it(`${name} (${profile}) matches test/fixtures/blocks/${name}.as and compiles`, async () => {
      const g = generate(w, profile);
      expect(g.errors).toEqual([]);
      await expect(g.source).toMatchFileSnapshot(`./fixtures/blocks/${name}.as`);
      const r = await compileAssemblyScript(g.source);
      expect(r.ok, r.ok ? "" : r.errors).toBe(true);
    });
  }

  it("the logic and irq workspaces pass checkAbi", async () => {
    for (const w of [LOGIC, IRQ]) {
      const r = await buildPayload(w, "esp-base", proto.abi);
      expect(r.ok, r.ok ? "" : r.errors.join("; ")).toBe(true);
      if (r.ok) expect(r.report.warnings).toEqual([]);
    }
  });
});

describe("AssemblyScript generator", () => {
  it("imports only what the blocks use and exports only the handlers needed", () => {
    const empty = generate(ws(), "esp-base");
    expect(empty.imports).toEqual([]);
    expect(empty.exports).toEqual([]);
    expect(empty.source).not.toContain("@external");

    const g = generate(ws(onInit({ type: "mw_log", inputs: { TEXT: txt("hi") } })), "esp-base");
    expect(g.imports).toEqual(["log"]);
    expect(g.exports).toEqual(["init"]);
    expect(g.source).not.toMatch(/memory\.data\(512\);\nconst RX|mw_alloc/);

    const irq = generate(IRQ, "esp-base");
    expect(irq.imports).toEqual(["log", "publish", "now_ms", "set_tick", "gpio_output", "gpio_input", "gpio_write", "gpio_read", "gpio_irq", "timer_start", "timer_stop"]);
    expect(irq.exports).toEqual(["init", "tick", "on_irq"]);
  });

  it("shares one tick across every blocks (their gcd) and one interrupt per pin", () => {
    const g = generate(IRQ, "esp-base");
    expect(g.source).toContain("mw_set_tick(100);");
    expect(g.source).toContain("every0 += 100;");
    expect(g.source).toContain("if (every1 >= 500) {");
    // Falling | rising = any edge, filtered by level per handler.
    expect(g.source).toContain("mw_gpio_irq(0, 3);");
    expect(g.source).toContain("if (kind == 1 && source == 0 && value == 0) {");
    expect(g.source).toContain("if (kind == 1 && source == 0 && value == 1) {");
  });

  it("uses fixed buffers and never allocates", () => {
    for (const [, w, profile] of GOLDEN) {
      const src = generate(w, profile).source;
      expect(src).not.toMatch(/\bnew\b|String\.UTF8|heap\.|__alloc|memory\.grow|f32|f64|\bwhile\s*\(true/);
    }
    expect(generate(EXAMPLES[1]?.workspace ?? ws(), "esp-base").source).toContain("const RX: usize = memory.data(512);");
  });

  it("rejects unbounded and non-constant loops", () => {
    const whileLoop = generate(ws(onInit({ type: "controls_whileUntil", fields: { MODE: "WHILE" } })), "esp-base");
    expect(whileLoop.errors.map((e) => e.message)).toEqual(["while/until loops are unbounded; use repeat or for with constant bounds"]);
    const varRepeat = generate(
      { variables: [{ name: "x", id: "x" }], ...ws(onInit({ type: "controls_repeat_ext", inputs: { TIMES: getVar("x") } })) },
      "esp-base",
    );
    expect(varRepeat.errors.map((e) => e.message)).toEqual(["repeat needs a constant whole number of times"]);
    const huge = generate(
      { variables: [{ name: "i", id: "i" }], ...ws(onInit({ type: "controls_for", fields: { VAR: { id: "i" } }, inputs: { FROM: num(0), TO: num(100000), BY: num(1) } })) },
      "esp-base",
    );
    expect(huge.errors.map((e) => e.message)).toEqual(["for would run 100001 times; at most 10000"]);
    const brk = generate(ws(onInit({ type: "controls_flow_statements", fields: { FLOW: "BREAK" } })), "esp-base");
    expect(brk.errors.map((e) => e.message)).toEqual(["break and continue must be inside a loop"]);
  });

  it("rejects floating point, text in variables and values outside their handler", () => {
    const issues = (w: WorkspaceJson) => generate(w, "esp-base").errors.map((e) => e.message);
    expect(issues(ws(onInit({ type: "mw_log", inputs: { TEXT: num(1.5) } })))).toEqual([
      "numbers must be whole (there is no floating point on the device)",
    ]);
    expect(issues({ variables: [{ name: "s", id: "s" }], ...ws(onInit({ type: "variables_set", fields: { VAR: { id: "s" } }, inputs: { VALUE: txt("x") } })) })).toEqual([
      "variables hold numbers; text can only be published, logged or shown",
    ]);
    expect(issues(ws(onInit({ type: "mw_publish", fields: { KEY: "app/x" }, inputs: { DATA: { block: { type: "mw_message_text" } } } })))).toEqual([
      "message text is only available inside on message",
    ]);
    expect(issues(ws(onInit({ type: "mw_publish", fields: { KEY: "sys/x" }, inputs: { DATA: num(1) } })))).toEqual([
      "keys under sys/ belong to the firmware",
    ]);
    expect(issues(ws({ type: "mw_every", fields: { MS: 15 } }, { type: "mw_every", fields: { MS: 20 } }))).toEqual([
      "the every periods share a 5 ms tick; periods must have a common divisor of at least 10 ms",
    ]);
  });

  it("skips disabled blocks and reports blocks the profile does not support", () => {
    const toggle = { type: "mw_gpio_toggle", id: "t", fields: { PIN: 8 } };
    const badge = generate(ws(onInit({ type: "mw_log", inputs: { TEXT: txt("a") } }, toggle)), "badge");
    expect(badge.errors).toEqual([{ blockId: "t", message: 'block "mw_gpio_toggle" is not available on the badge profile' }]);
    const disabled = generate(ws(onInit({ ...toggle, disabledReasons: ["profile"] })), "badge");
    expect(disabled.errors).toEqual([]);
    expect(disabled.imports).toEqual([]);
    const legacy = generate(ws(onInit({ ...toggle, enabled: false })), "badge");
    expect(legacy.errors).toEqual([]);
    const orphan = generate(ws({ type: "mw_log", id: "o" }), "esp-base");
    expect(orphan.warnings).toEqual([{ blockId: "o", message: "not inside an event block; ignored" }]);
    const leds = generate(ws(onInit({ type: "badge_set_leds", id: "l" })), "esp-base");
    expect(leds.errors).toEqual([{ blockId: "l", message: 'block "badge_set_leds" is not available on the esp-base profile' }]);
  });

  it("validates keys like the host", () => {
    expect(keyProblem("app/echo/in")).toBeNull();
    expect(keyProblem("")).toBe("key is empty");
    expect(keyProblem("a b")).toBe("key contains whitespace");
    expect(keyProblem("a//b")).toBe("key has an empty segment");
    expect(keyProblem("sys")).toBe("keys under sys/ belong to the firmware");
  });
});

describe("block library and profiles", () => {
  it("defines every custom block once and maps each to a feature", () => {
    const types = BLOCK_DEFS.map((d) => String(d.type));
    expect(new Set(types).size).toBe(types.length);
    for (const t of [...types, ...BUILTIN_BLOCKS]) expect(featureOf(t), t).not.toBeNull();
    for (const t of EVENT_BLOCKS) expect(types).toContain(t);
    expect(featureOf("controls_whileUntil")).toBeNull();
  });

  it("supports GPIO and timers on esp-base and the badge blocks only on the badge", () => {
    for (const t of ["mw_gpio_output", "mw_gpio_input", "mw_gpio_write", "mw_gpio_read", "mw_gpio_toggle", "mw_on_pin_edge", "mw_timer_start", "mw_timer_stop", "mw_on_timer"]) {
      expect(blockSupported(t, "esp-base"), t).toBe(true);
      expect(blockSupported(t, "badge"), t).toBe(false);
    }
    for (const t of ["badge_set_leds", "badge_led", "badge_leds_fill", "badge_show_text", "badge_pet_mode", "badge_on_button", "badge_button", "badge_pressed"]) {
      expect(blockSupported(t, "badge"), t).toBe(true);
      expect(blockSupported(t, "esp-base"), t).toBe(false);
    }
    for (const t of ["mw_on_init", "mw_every", "mw_on_message", "mw_publish", "mw_log", "controls_repeat", "text_join"]) {
      expect(blockSupported(t, "badge") && blockSupported(t, "esp-base"), t).toBe(true);
    }
  });

  it("shows unsupported blocks disabled in the toolbox", () => {
    type Item = { kind: string; name?: string; type?: string; disabled?: boolean; contents?: Item[] };
    const disabledIn = (profile: "esp-base" | "badge") =>
      ((toolbox(profile) as Item).contents ?? [])
        .flatMap((c) => c.contents ?? [])
        .filter((b) => b.disabled)
        .map((b) => b.type);
    expect(disabledIn("badge")).toEqual(
      expect.arrayContaining(["mw_gpio_output", "mw_on_pin_edge", "mw_timer_start", "mw_on_timer"]),
    );
    expect(disabledIn("badge").every((t) => t?.startsWith("mw_"))).toBe(true);
    expect(disabledIn("esp-base").every((t) => t?.startsWith("badge_"))).toBe(true);
    expect(disabledIn("esp-base")).toHaveLength(8);
  });
});
