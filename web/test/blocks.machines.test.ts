// State-machine tables -> AssemblyScript (machineGen.ts via generator.ts):
// the code's shape (constants, one switch per event, guards as if / else
// in table order, entry and log on a change of state, the bounded raise
// queue, init order), no machines = the code from before, a machine no
// block references still compiles, references to unknown machines, states
// and events are errors on the block, a bad table is reported and never
// crashes the generator, the pipeline's "states" stage, and the "mode"
// example and the raise bound running in the simulator.

import { describe, expect, it } from "vitest";
import { EXAMPLES, exampleWorkspace } from "../src/blocks/examples";
import { generate } from "../src/blocks/generator";
import { dropLog } from "../src/blocks/machineGen";
import { buildPayload, machineIssueText } from "../src/blocks/pipeline";
import type { BlockJson, WorkspaceJson } from "../src/blocks/workspace";
import { compileAssemblyScript } from "../src/editor/compile";
import {
  type Machine,
  type MachineIssue,
  event,
  machineLogInit,
  machineLogMove,
  machinePinNeeds,
  machineStates,
  state,
  transition,
} from "../src/model/machines";
import { EMPTY_PINS, type PinTable, boardSuggestions, checkTable, normalizeBuses, row, withNeeds } from "../src/model/pins";
import type { SimEvent } from "../src/protocol/wasm";
import { SimSession } from "../src/sim/session";
import { ManualClock } from "./clock";
import { protocol } from "./fakeDevice";

const proto = protocol();
const ws = (...blocks: BlockJson[]): WorkspaceJson => ({ blocks: { languageVersion: 0, blocks } });
const txt = (s: string) => ({ shadow: { type: "text", fields: { TEXT: s } } });
const raise = (ref: string, id = "r"): BlockJson => ({ type: "mw_sm_raise", id, fields: { EVENT: ref } });
const onInit = (body: BlockJson): BlockJson => ({ type: "mw_on_init", inputs: { DO: { block: body } } });

const pins: PinTable = normalizeBuses({
  rows: [row("gpio-out", "LED", 25), { ...row("gpio-in", "BUTTON", 15), pull: "up" }, { ...row("gpio-in", "DOOR", 14), pull: "up" }],
  buses: [],
});

/** Door: guarded rows on one cell, a self-transition, an entry with every action kind. */
const door: Machine = {
  id: "Door",
  name: "front door",
  initial: "Closed",
  states: [
    state("Closed", [{ kind: "write", label: "LED", level: 0 }]),
    state("Open", [
      { kind: "write", label: "LED", level: 1 },
      { kind: "publish", key: "app/door", text: "open" },
      { kind: "log", text: "door open" },
    ]),
    state("Alarm", [{ kind: "toggle", label: "LED" }]),
  ],
  events: [event("PUSH"), event("BEEP")],
  rows: [
    // Guarded rows first in the table, the unguarded one in the middle: the else still comes last.
    transition("Closed", "PUSH", "Alarm", [], { kind: "cmp", op: "eq", a: { kind: "pin", label: "DOOR" }, b: { kind: "const", value: 0 } }),
    transition("Closed", "PUSH", "Open", [{ kind: "log", text: "pushed" }]),
    transition("Closed", "PUSH", "Closed", [], {
      kind: "and",
      a: { kind: "not", a: { kind: "pin", label: "BUTTON" } },
      b: { kind: "or", a: { kind: "pin", label: "DOOR" }, b: { kind: "const", value: -1 } },
    }),
    transition("Open", "PUSH", "Closed"),
    transition("Alarm", "BEEP", "Alarm", [{ kind: "toggle", label: "LED" }]),
  ],
};

const twoStates = (id: string): Machine => ({
  id,
  name: "",
  initial: "A",
  states: [state("A"), state("B")],
  events: [event("GO")],
  rows: [transition("A", "GO", "B"), transition("B", "GO", "A")],
});

async function compiles(source: string): Promise<void> {
  const r = await compileAssemblyScript(source);
  expect(r.ok, r.ok ? "" : `${r.errors}\n${source}`).toBe(true);
}

describe("state machines -> AssemblyScript", () => {
  it("emits SM/EV constants per machine, its state and pending globals, and one shared smBusy", () => {
    const g = generate(ws(onInit(raise("Door.PUSH")), onInit(raise("Door.BEEP", "b")), onInit(raise("M2.GO", "c"))), "esp-base", pins, [door, twoStates("M2")]);
    expect(g.errors).toEqual([]);
    expect(g.source).toContain(
      [
        '// State machine 0: Door "front door"',
        "const SM0_Closed: i32 = 0;",
        "const SM0_Open: i32 = 1;",
        "const SM0_Alarm: i32 = 2;",
        "const EV0_PUSH: i32 = 0;",
        "const EV0_BEEP: i32 = 1;",
        "// State machine 1: M2",
        "const SM1_A: i32 = 0;",
        "const SM1_B: i32 = 1;",
        "const EV1_GO: i32 = 0;",
      ].join("\n"),
    );
    expect(g.source).toContain(
      [
        "// State machines: 0 idle, 1 stepping, 2 draining the queued raises.",
        "let smBusy: i32 = 0;",
        "// Door: the current state and the one queued event (-1: none).",
        "let sm0: i32 = SM0_Closed;",
        "let sm0Pending: i32 = -1;",
        "// M2: the current state and the one queued event (-1: none).",
        "let sm1: i32 = SM1_A;",
        "let sm1Pending: i32 = -1;",
      ].join("\n"),
    );
    expect(g.source.match(/let smBusy/g)).toHaveLength(1);
  });

  it("dispatches each event with one switch over the states that have rows for it", () => {
    const g = generate(ws(onInit(raise("Door.PUSH")), onInit(raise("Door.BEEP", "b"))), "esp-base", pins, [door]);
    const push = g.source.slice(g.source.indexOf("function sm0_on_PUSH"), g.source.indexOf("function sm0_on_BEEP"));
    expect(push.match(/switch/g)).toHaveLength(1);
    expect(push.match(/case SM0_\w+:/g)).toEqual(["case SM0_Closed:", "case SM0_Open:"]);
    expect(g.source).toContain(
      ["function sm0_on_BEEP(): void {", "  switch (sm0) {", "    case SM0_Alarm:", "      pinToggle(PIN_LED);", "      break;", "  }", "}"].join("\n"),
    );
    expect(g.source).toContain(
      [
        "function sm0_step(ev: i32): void {",
        "  switch (ev) {",
        "    case EV0_PUSH:",
        "      sm0_on_PUSH();",
        "      break;",
        "    case EV0_BEEP:",
        "      sm0_on_BEEP();",
        "      break;",
        "  }",
        "}",
      ].join("\n"),
    );
  });

  it("turns guarded rows into if / else if in table order with the unguarded row as the final else, and compiles", async () => {
    const g = generate(ws(onInit(raise("Door.PUSH")), onInit(raise("Door.BEEP", "b"))), "esp-base", pins, [door]);
    expect(g.errors).toEqual([]);
    const S = (t: string) => {
      const m = new RegExp(`// ${JSON.stringify(t).replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}\\nconst (S\\d+)`).exec(g.source);
      if (!m?.[1]) throw new Error(`no constant for ${t}`);
      return m[1];
    };
    expect(g.source).toContain(
      [
        "    case SM0_Closed:",
        "      if (<i32>(mw_gpio_read(PIN_DOOR) == 0) != 0) {",
        "        sm0 = SM0_Alarm;",
        `        mw_log(${S("sm Door: Closed -> Alarm on PUSH")}, ${S("sm Door: Closed -> Alarm on PUSH")}_LEN);`,
        "        sm0_enter_Alarm();",
        "      } else if (<i32>(<i32>(mw_gpio_read(PIN_BUTTON) == 0) != 0 && <i32>(mw_gpio_read(PIN_DOOR) != 0 || (-1) != 0) != 0) != 0) {",
        "      } else {",
        `        mw_log(${S("pushed")}, ${S("pushed")}_LEN);`,
        "        sm0 = SM0_Open;",
        `        mw_log(${S("sm Door: Closed -> Open on PUSH")}, ${S("sm Door: Closed -> Open on PUSH")}_LEN);`,
        "        sm0_enter_Open();",
        "      }",
        "      break;",
      ].join("\n"),
    );
    await compiles(g.source);
  });

  it("on a change of state sets the state, logs the move and enters the next state (entry actions, then on-enter blocks); a self-transition only runs its actions", () => {
    const hat: BlockJson = { type: "mw_sm_on_enter", id: "h", fields: { STATE: "Door.Open" }, inputs: { DO: { block: { type: "mw_log", inputs: { TEXT: txt("hat") } } } } };
    const g = generate(ws(onInit(raise("Door.PUSH")), onInit(raise("Door.BEEP", "b")), hat), "esp-base", pins, [door]);
    expect(g.errors).toEqual([]);
    const enterOpen = g.source.slice(g.source.indexOf("function sm0_enter_Open"));
    expect(enterOpen.slice(0, enterOpen.indexOf("\n}\n"))).toMatch(
      /^function sm0_enter_Open\(\): void \{\n {2}pinWrite\(PIN_LED, 1\);\n {2}mw_publish\(S\d+, S\d+_LEN, S\d+, S\d+_LEN\);\n {2}mw_log\(S\d+, S\d+_LEN\);\n {2}txLen = 0;\n {2}txBytes\(S\d+, S\d+_LEN\);\n {2}mw_log\(TX, txLen\);$/,
    );
    // The self-transition Alarm -BEEP-> Alarm: the toggle only.
    const beep = g.source.slice(g.source.indexOf("function sm0_on_BEEP"), g.source.indexOf("function sm0_step"));
    expect(beep).not.toContain("sm0 =");
    expect(beep).not.toContain("mw_log");
    expect(g.source).toContain(`// ${JSON.stringify(machineLogMove("Door", "Open", "Closed", "PUSH"))}`);
    expect(g.source).not.toContain(machineLogMove("Door", "Alarm", "Alarm", "BEEP"));
  });

  it("raises through the bounded queue: dropped while draining, one slot while stepping, drained once in machine order", () => {
    const g = generate(ws(onInit(raise("A.GO")), onInit(raise("B.GO", "b"))), "esp-base", EMPTY_PINS, [twoStates("A"), twoStates("B")]);
    expect(g.errors).toEqual([]);
    expect(g.source).toContain(
      [
        "function sm0_raise(ev: i32): void {",
        "  if (smBusy == 2) {",
        "    sm0_drop(ev);",
        "    return;",
        "  }",
        "  if (smBusy == 1) {",
        "    if (sm0Pending < 0) {",
        "      sm0Pending = ev;",
        "    } else {",
        "      sm0_drop(ev);",
        "    }",
        "    return;",
        "  }",
        "  smBusy = 1;",
        "  sm0_step(ev);",
        "  smDrain();",
        "}",
      ].join("\n"),
    );
    expect(g.source).toContain(
      [
        "function smDrain(): void {",
        "  smBusy = 2;",
        "  if (sm0Pending >= 0) {",
        "    const ev = sm0Pending;",
        "    sm0Pending = -1;",
        "    sm0_step(ev);",
        "  }",
        "  if (sm1Pending >= 0) {",
        "    const ev = sm1Pending;",
        "    sm1Pending = -1;",
        "    sm1_step(ev);",
        "  }",
        "  smBusy = 0;",
        "}",
      ].join("\n"),
    );
    expect(g.source).toContain(`// ${JSON.stringify(dropLog("A", "GO"))}`);
    // No loops and no allocation in the machine code.
    const sm = g.source.slice(g.source.indexOf("// State machine 0"));
    expect(sm).not.toMatch(/\bfor\b|\bwhile\b|\bnew\b|f32|f64/);
  });

  it("init runs the machines after the pin setup, subscriptions and tick, and before the on-init stacks", () => {
    const g = generate(
      ws(onInit({ type: "mw_log", inputs: { TEXT: txt("user init") } }), { type: "mw_every", fields: { MS: 100 }, inputs: { DO: { block: raise("Door.PUSH") } } }, onInit(raise("Door.BEEP", "b"))),
      "esp-base",
      pins,
      [door],
    );
    const init = g.source.slice(g.source.indexOf("export function init"));
    const at = (s: string) => {
      const i = init.indexOf(s);
      expect(i, s).toBeGreaterThan(0);
      return i;
    };
    expect(at("pinWrite(PIN_LED, 0);")).toBeLessThan(at("mw_set_tick(100);"));
    expect(at("mw_set_tick(100);")).toBeLessThan(at("  smBusy = 1;"));
    expect(at("  smBusy = 1;")).toBeLessThan(at("  sm0_enter_Closed();"));
    expect(at("  sm0_enter_Closed();")).toBeLessThan(at("  smDrain();"));
    expect(at("  smDrain();")).toBeLessThan(at("txLen = 0;"));
    expect(g.source).toContain(`// ${JSON.stringify(machineLogInit("Door", "Closed"))}`);
  });

  it("generates byte-identical code without machines", () => {
    for (const e of EXAMPLES.filter((x) => !x.machines)) {
      const p = withNeeds(EMPTY_PINS, e.profile === "badge" ? "badge" : "pico2", e.needs);
      expect(generate(e.workspace, e.profile, p, []).source, e.id).toBe(generate(e.workspace, e.profile, p).source);
      expect(generate(e.workspace, e.profile, p).source, e.id).not.toMatch(/smBusy|SM\d_/);
    }
  });

  it("compiles a machine that no block references (dead but valid)", async () => {
    const g = generate(ws(), "esp-base", pins, [door, { ...twoStates("Empty"), events: [], rows: [] }]);
    expect(g.errors).toEqual([]);
    expect(g.warnings.map((w) => w.message)).toEqual([
      "machine Door: event PUSH is never raised (no raise block or action names it)",
      "machine Door: event BEEP is never raised (no raise block or action names it)",
    ]);
    expect(g.exports).toEqual(["init"]);
    await compiles(g.source);
  });

  it("reports raise, is-current and on-enter blocks naming an unknown machine, state or event on the block", () => {
    const inState = (id: string, ref: string): BlockJson => ({ type: "mw_pin_write", fields: { LABEL: "LED" }, inputs: { LEVEL: { block: { type: "mw_sm_in_state", id, fields: { STATE: ref } } } } });
    const g = generate(
      ws(
        onInit(raise("Nope.GO", "r1")),
        onInit(raise("Door.OPEN", "r2")),
        onInit(raise("", "r3")),
        onInit(raise("Door", "r4")),
        onInit(inState("s1", "Door.Ajar")),
        onInit(inState("s2", "Door.Open")),
        { type: "mw_sm_on_enter", id: "h1", fields: { STATE: "Gate.Open" } },
        { type: "mw_sm_on_enter", id: "h2", fields: { STATE: "Door.PUSH" } },
      ),
      "esp-base",
      pins,
      [door],
    );
    expect(g.errors).toEqual([
      { blockId: "r1", message: 'no state machine is named "Nope"; add it under States' },
      { blockId: "r2", message: 'Door has no event "OPEN"' },
      { blockId: "r3", message: "pick a state machine event" },
      { blockId: "r4", message: '"Door" does not name a state machine event (machine.event)' },
      { blockId: "s1", message: 'Door has no state "Ajar"' },
      { blockId: "h1", message: 'no state machine is named "Gate"; add it under States' },
      { blockId: "h2", message: 'Door has no state "PUSH"' },
    ]);
    expect(g.source).toContain("pinWrite(PIN_LED, <i32>(sm0 == SM0_Open));");
  });

  it("reports an invalid table as errors naming the machine and row, leaves the bad parts out and still emits valid code", async () => {
    const bad: Machine = {
      id: "Bad",
      name: "",
      initial: "Nowhere",
      states: [state("A", [{ kind: "write", label: "BUTTON", level: 1 }, { kind: "raise", event: "NOPE" }]), state("B")],
      events: [event("GO")],
      rows: [
        transition("A", "GO", "B"),
        transition("A", "GO", "A"),
        transition("Z", "GO", "B"),
        transition("A", "STOP", "B"),
        transition("B", "GO", "Q"),
        transition("B", "GO", "A", [{ kind: "toggle", label: "LAMP" }, { kind: "publish", key: "sys/x", text: "" }]),
        transition("B", "GO", "B", [], { kind: "not", a: { kind: "not", a: { kind: "not", a: { kind: "not", a: { kind: "pin", label: "DOOR" } } } } }),
      ],
    };
    const tooMany: Machine = { ...twoStates("Big"), states: Array.from({ length: 17 }, (_, i) => state(`S${i}`)), initial: "S0" };
    const g = generate(ws(onInit(raise("Bad.GO")), onInit(raise("Big.GO", "b"))), "esp-base", pins, [
      bad,
      tooMany,
      twoStates("Bad"),
      { ...twoStates("x y"), id: "x y" },
    ]);
    expect(g.errors.map((e) => e.message)).toEqual([
      'machine Bad: the initial state "Nowhere" is not one of its states; the first state is used',
      "machine Big: has more than 16 states",
      "machine Bad: another state machine has this id",
      'machine 4: the id "x y" must start with a letter and hold only letters, digits and _',
      "machine Bad state A entry: BUTTON is not a GPIO output in the Pins table",
      'machine Bad state A entry: raises "NOPE", which is not one of its events',
      "machine Bad row 2: A already has a row without a guard for GO; only the first is used",
      'machine Bad row 3: "Z" is not one of its states',
      'machine Bad row 4: "STOP" is not one of its events',
      'machine Bad row 5: the next state "Q" is not one of its states',
      "machine Bad row 7: the guard is nested deeper than 4 levels",
      // Row 6 is the first usable unguarded row for (B, GO), so its actions are checked.
      'machine Bad row 6: no pin is labelled "LAMP" in the Pins table; add it there',
      "machine Bad row 6: keys under sys/ belong to the firmware",
    ]);
    expect(g.errors.every((e) => e.blockId === undefined)).toBe(true);
    // The raise of a machine left out (Big) emits nothing; the rest is valid code.
    expect(g.source).not.toContain("sm1_raise");
    await compiles(g.source);

    const more = generate(ws(), "esp-base", pins, [
      { ...twoStates("Dup"), events: [event("GO"), event("GO")] },
      { ...twoStates("None"), states: [] },
      twoStates("C"),
      twoStates("D"),
      twoStates("E"),
    ]);
    expect(more.errors.map((e) => e.message)).toEqual([
      "machine Dup: two events are named GO",
      "machine None: has no states",
      "machine E: a payload has at most 4 state machines",
    ]);
    await compiles(more.source);
  });
});

describe("pipeline: the states stage", () => {
  const issue = (severity: "error" | "warning", over: Partial<MachineIssue> = {}): MachineIssue => ({
    machine: 0,
    list: "rows",
    row: 1,
    field: "next",
    severity,
    message: "something",
    ...over,
  });

  it("stops the build at stage states on a machine error and never on a warning", async () => {
    const mode = exampleWorkspace("mode")!;
    const machines = mode.machines ?? [];
    const p = withNeeds(EMPTY_PINS, "pico2", mode.needs);
    const stopped = await buildPayload(mode.workspace, "esp-base", proto.abi, { pins: p, issues: [], machines, machineIssues: [issue("error"), issue("warning")] });
    expect(stopped.ok).toBe(false);
    expect(!stopped.ok && stopped.stage).toBe("states");
    expect(!stopped.ok && stopped.errors).toEqual(["States Mode row 2: something"]);
    const built = await buildPayload(mode.workspace, "esp-base", proto.abi, { pins: p, issues: [], machines, machineIssues: [issue("warning")] });
    expect(built.ok, built.ok ? "" : built.errors.join("; ")).toBe(true);
  });

  it("names the machine and the row, state or event in a states issue", () => {
    const ms = exampleWorkspace("mode")!.machines ?? [];
    expect(machineIssueText(issue("error", { list: "states", row: 2 }), ms)).toBe("States Mode state Blink: something");
    expect(machineIssueText(issue("error", { list: "events", row: 0 }), ms)).toBe("States Mode event PRESS: something");
    expect(machineIssueText(issue("error", { list: "machine", row: null }), ms)).toBe("States Mode: something");
    expect(machineIssueText(issue("error", { machine: 3, row: 0 }), ms)).toBe("States machine 4 row 1: something");
  });
});

describe("the mode example", () => {
  it("needs LED (output) and BUTTON (input) and raises both of its events", () => {
    const e = exampleWorkspace("mode")!;
    expect(e.needs).toEqual([
      { label: "LED", func: "gpio-out" },
      { label: "BUTTON", func: "gpio-in" },
    ]);
    expect(machinePinNeeds(e.machines?.[0] as Machine)).toEqual([{ label: "LED", func: "gpio-out" }]);
    const g = generate(e.workspace, "esp-base", withNeeds(EMPTY_PINS, "pico2", e.needs), e.machines);
    expect(g.errors).toEqual([]);
    expect(g.warnings).toEqual([]);
  });
});

function sim(wasm: Uint8Array) {
  const clock = new ManualClock();
  const events: SimEvent[] = [];
  const errors: string[] = [];
  const s = new SimSession(proto, wasm, { onEvent: (e) => events.push(e), onError: (m) => errors.push(m) }, clock);
  s.start();
  const logs = () => events.flatMap((e) => (e.kind === "log" ? [e.line] : []));
  return { s, clock, events, errors, logs };
}

describe.runIf(proto.hasSimulator)("state machines in the simulator", () => {
  for (const [target, led, button] of [
    ["pico2", 25, 15],
    ["esp32", 4, 0],
  ] as const) {
    it(`runs the mode example on ${target} (LED GPIO ${led}, BUTTON GPIO ${button}): PRESS steps Off -> On -> Blink -> Off and TICK toggles the LED in Blink`, async () => {
      const e = exampleWorkspace("mode")!;
      const machines = e.machines ?? [];
      const p = withNeeds(EMPTY_PINS, target, [...e.needs, ...machines.flatMap(machinePinNeeds)]);
      expect(p.rows.find((r) => r.label === "BUTTON")).toMatchObject({ pin: button, func: "gpio-in", pull: "up" });
      expect(p.rows.find((r) => r.label === "LED")).toMatchObject({ pin: led, func: "gpio-out" });
      const issues = checkTable(proto.checkPins, target, p);
      expect(issues).toEqual([]);
      const r = await buildPayload(e.workspace, "esp-base", proto.abi, { pins: p, issues, machines, machineIssues: [] });
      if (!r.ok) throw new Error(`${r.stage}: ${r.errors.join("; ")}`);
      const { s, clock, errors, logs } = sim(r.wasm);
      const ledHigh = () => s.pins.get(led)?.high;
      const press = () => {
        s.setInput(button, false);
        clock.advance(20);
        s.setInput(button, true);
        clock.advance(20);
      };
      clock.advance(20);
      expect(logs()).toContain("sm Mode: Off");
      expect(ledHigh()).toBe(false);

      press();
      expect(logs()).toContain("sm Mode: Off -> On on PRESS");
      expect(ledHigh()).toBe(true);
      clock.advance(500);
      expect(ledHigh()).toBe(true);

      press();
      expect(logs()).toContain("sm Mode: On -> Blink on PRESS");
      expect(logs()).toContain("mode: blinking");
      const seen: boolean[] = [];
      for (let i = 0; i < 4; i++) {
        clock.advance(250);
        seen.push(ledHigh() === true);
      }
      // Toggling every 250 ms: the level alternates.
      expect(seen[0]).not.toBe(seen[1]);
      expect(seen[1]).not.toBe(seen[2]);
      expect(seen[2]).not.toBe(seen[3]);

      press();
      expect(logs()).toContain("sm Mode: Blink -> Off on PRESS");
      expect(ledHigh()).toBe(false);
      clock.advance(1000);
      expect(ledHigh()).toBe(false);
      expect(machineStates(logs()).get("Mode")).toBe("Off");
      expect(errors).toEqual([]);
      s.dispose();
    });
  }

  it("drops a second raise in one step and a raise chained from the drain, logs each, and stays bounded without a trap", async () => {
    // A -GO-> B raises NEXT (queued) and MORE (dropped: the slot is full);
    // the drain runs NEXT: B -> C, whose entry raises GO (dropped: draining).
    const chain: Machine = {
      id: "Chain",
      name: "",
      initial: "A",
      states: [state("A"), state("B"), state("C", [{ kind: "raise", event: "GO" }])],
      events: [event("GO"), event("NEXT"), event("MORE")],
      rows: [
        transition("A", "GO", "B", [
          { kind: "raise", event: "NEXT" },
          { kind: "raise", event: "MORE" },
        ]),
        transition("B", "NEXT", "C"),
        transition("B", "MORE", "A"),
        transition("C", "GO", "A"),
      ],
    };
    // A second machine whose on-enter block raises Chain.GO from inside a step.
    const echo: Machine = { ...twoStates("Echo") };
    const w = ws(
      onInit({ ...raise("Chain.GO", "g"), next: { block: raise("Echo.GO", "e") } }),
      { type: "mw_sm_on_enter", id: "h", fields: { STATE: "Echo.B" }, inputs: { DO: { block: raise("Chain.GO", "eg") } } },
    );
    const r = await buildPayload(w, "esp-base", proto.abi, { pins: EMPTY_PINS, issues: [], machines: [chain, echo], machineIssues: [] });
    if (!r.ok) throw new Error(`${r.stage}: ${r.errors.join("; ")}\n${r.source}`);
    const { s, clock, errors, logs } = sim(r.wasm);
    clock.advance(50);
    expect(errors).toEqual([]);
    expect(logs()).toEqual([
      "sm Chain: A",
      "sm Echo: A",
      // raise Chain.GO from on init: the row's actions (NEXT queued, MORE
      // dropped), the move, then the drain.
      dropLog("Chain", "MORE"),
      "sm Chain: A -> B on GO",
      "sm Chain: B -> C on NEXT",
      dropLog("Chain", "GO"),
      // raise Echo.GO: Echo enters B, whose block raises Chain.GO: queued, then drained.
      "sm Echo: A -> B on GO",
      "sm Chain: C -> A on GO",
    ]);
    expect(machineStates(logs())).toEqual(
      new Map([
        ["Chain", "A"],
        ["Echo", "B"],
      ]),
    );
    s.dispose();
  });
});

describe("board button suggestions", () => {
  it("suggests BUTTON as a pulled-up GPIO input at the board's button pin, after the LED", () => {
    expect(boardSuggestions("pico2").rows).toEqual([
      row("gpio-out", "LED", 25, 0, "on-board LED"),
      { ...row("gpio-in", "BUTTON", 15, 0, "no user button: wire a push button from GPIO 15 to GND"), pull: "up" },
    ]);
    expect(boardSuggestions("badge")).toEqual(EMPTY_PINS);
  });
});
