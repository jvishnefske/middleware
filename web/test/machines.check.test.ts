// State-machine table checks through the real wasm (protocol-wasm
// checkMachines over the state-table crate): every rule's message,
// severity and fix, as the table editor shows them.

import { describe, expect, it } from "vitest";
import { type Expr, type Machine, type MachineIssue, event, hasErrors, state, transition } from "../src/model/machines";
import type { PinFunc } from "../src/model/pins";
import { protocol } from "./fakeDevice";

const proto = protocol();
const PINS: { label: string; func: PinFunc }[] = [
  { label: "LED", func: "gpio-out" },
  { label: "BUTTON", func: "gpio-in" },
  { label: "DOOR", func: "gpio-in" },
  { label: "POT", func: "adc" },
];
const check = (ms: readonly Machine[], pins = PINS): MachineIssue[] => proto.checkMachines(ms, pins);
const lines = (ms: readonly Machine[]) => check(ms).map((i) => `${i.list} ${i.row ?? "-"} ${i.field} ${i.severity}: ${i.message}`);

const pin = (label: string): Expr => ({ kind: "pin", label });
const eq = (label: string, value: number): Expr => ({ kind: "cmp", op: "eq", a: pin(label), b: { kind: "const", value } });

function mode(patch: Partial<Machine> = {}): Machine {
  return {
    id: "Mode",
    name: "LED mode",
    initial: "Off",
    states: [state("Off", [{ kind: "write", label: "LED", level: 0 }]), state("On", [{ kind: "write", label: "LED", level: 1 }])],
    events: [event("PRESS")],
    rows: [transition("Off", "PRESS", "On", [{ kind: "publish", key: "led/state", text: "on" }], eq("BUTTON", 1)), transition("On", "PRESS", "Off", [{ kind: "log", text: "off" }])],
    ...patch,
  };
}

describe("state machine checks (Rust)", () => {
  it("a valid machine has no issues", () => {
    expect(check([mode()])).toEqual([]);
    expect(check([])).toEqual([]);
  });

  it("machine ids must be identifiers, unique ignoring case", () => {
    expect(lines([mode(), mode({ id: "MODE" }), mode({ id: "" })])).toEqual([
      "machine - id error: The id Mode is also used by machine 2; machine ids must be unique (ignoring case).",
      "machine - id error: The id MODE is also used by machine 1; machine ids must be unique (ignoring case).",
      "machine - id error: Give this machine an id, for example Mode.",
    ]);
    expect(check([mode({ id: "2fast" })])[0]).toMatchObject({ machine: 0, list: "machine", row: null, field: "id", severity: "error" });
  });

  it("a machine with 17 states reports its size and nothing else", () => {
    const states = Array.from({ length: 17 }, (_, k) => state(`S${k}`));
    expect(lines([mode({ states, initial: "nowhere" })])).toEqual([
      "machine - size error: This machine has 17 states; at most 16 are allowed. Remove some or split the machine.",
    ]);
  });

  it("state and event names must be identifiers, unique ignoring case", () => {
    const m = mode({ states: [...mode().states, state("off")], events: [event("PRESS"), event("go now")] });
    expect(lines([m]).filter((l) => l.includes("name error"))).toEqual([
      "states 0 name error: The name Off is also used by state 3; state names must be unique (ignoring case).",
      "states 2 name error: The name off is also used by state 1; state names must be unique (ignoring case).",
      "events 1 name error: An event name starts with a letter and has only letters, digits and _ (at most 24).",
    ]);
  });

  it("an unknown initial state offers the first state", () => {
    const [i] = check([mode({ initial: "Idle" })]);
    expect(i).toMatchObject({ list: "machine", row: null, field: "initial", severity: "error", fix: { kind: "set-initial", name: "Off" } });
    expect(i?.message).toBe("The initial state Idle is not a state of this machine; choose one, for example Off.");
  });

  it("rows naming missing states or events offer to add them", () => {
    const m = mode({ rows: [...mode().rows, transition("Busy", "HOLD", "on")] });
    const errors = check([m]).filter((i) => i.severity === "error");
    expect(errors.map((i) => [i.row, i.field, i.message, i.fix])).toEqual([
      [2, "state", "There is no state Busy; add it or choose another.", { kind: "add-state", name: "Busy" }],
      [2, "next", "There is no state on. Did you mean On?", undefined],
      [2, "event", "There is no event HOLD; add it or choose another.", { kind: "add-event", name: "HOLD" }],
    ]);
  });

  it("guard pins must be in the pin table and digital", () => {
    const g: Expr = { kind: "or", a: eq("POT", 1), b: eq("SENSOR", 1) };
    const m = mode({ rows: [transition("Off", "PRESS", "On", [], g), mode().rows[1]!] });
    expect(check([m]).map((i) => [i.row, i.field, i.message, i.fix])).toEqual([
      [0, "guard", "The condition reads POT, which is an analog input; a condition can read only GPIO inputs and outputs.", undefined],
      [0, "guard", "The condition reads SENSOR, which is not in the pin table; add it as a GPIO input.", { kind: "add-pin", label: "SENSOR", func: "gpio-in" }],
    ]);
  });

  it("guards nested deeper than four levels are reported, not read", () => {
    let g: Expr = pin("BUTTON");
    for (let k = 0; k < 40; k++) g = { kind: "not", a: g };
    const m = mode({ rows: [transition("Off", "PRESS", "On", [], g), mode().rows[1]!] });
    expect(lines([m])).toEqual(["rows 0 guard error: This condition is nested more than 4 levels deep; simplify it."]);
  });

  it("write and toggle need a gpio-out pin, with an add-pin fix", () => {
    const m = mode({
      rows: [
        transition("Off", "PRESS", "On", [
          { kind: "toggle", label: "BUTTON" },
          { kind: "write", label: "BUZZER", level: 1 },
        ]),
        mode().rows[1]!,
      ],
    });
    const issues = check([m]);
    expect(issues.map((i) => i.message)).toEqual([
      "Action 1 drives BUTTON, which is a GPIO input; only a GPIO output can be written or toggled.",
      "Action 2 drives BUZZER, which is not in the pin table; add it as a GPIO output.",
    ]);
    expect(issues[1]).toMatchObject({ list: "rows", row: 0, field: "actions", fix: { kind: "add-pin", label: "BUZZER", func: "gpio-out" } });
  });

  it("entry actions are checked like row actions", () => {
    const m = mode({ states: [state("Off", [{ kind: "toggle", label: "NOPE" }]), state("On")] });
    expect(check([m])).toMatchObject([{ list: "states", row: 0, field: "entry", severity: "error", fix: { kind: "add-pin", label: "NOPE", func: "gpio-out" } }]);
  });

  it("publish keys follow the key rule and avoid sys/; text fits a message", () => {
    const m = mode({
      rows: [
        transition("Off", "PRESS", "On", [
          { kind: "publish", key: "sys/reboot", text: "" },
          { kind: "publish", key: "a//b", text: "" },
          { kind: "log", text: "x".repeat(513) },
        ]),
        mode().rows[1]!,
      ],
    });
    expect(lines([m])).toEqual([
      "rows 0 actions error: Action 1 publishes on sys/reboot, but keys under sys/ belong to the firmware; choose another key.",
      "rows 0 actions error: Action 2 publishes on a//b, which has an empty part (a leading, trailing or doubled /).",
      "rows 0 actions error: Action 3's text is 513 bytes; at most 512 fit in a message.",
    ]);
  });

  it("raise must name an event of the same machine", () => {
    const m = mode({ states: [state("Off"), state("On", [{ kind: "raise", event: "TICK" }])] });
    expect(check([m])).toMatchObject([
      {
        list: "states",
        row: 1,
        field: "entry",
        message: "Action 1 raises TICK, which is not an event of this machine; add it or choose another.",
        fix: { kind: "add-event", name: "TICK" },
      },
    ]);
  });

  it("a cell has at most one unguarded row", () => {
    const m = mode({ rows: [...mode().rows, transition("On", "PRESS", "On")] });
    expect(lines([m])).toEqual([
      "rows 2 guard error: Row 2 already handles PRESS in On without a condition; give this row a condition or remove one of them.",
    ]);
  });

  it("overlapping guards name the earlier row and a witness assignment", () => {
    const m = mode({ rows: [...mode().rows, transition("Off", "PRESS", "Off", [], eq("DOOR", 1))] });
    expect(lines([m])).toEqual([
      "rows 2 guard error: This condition and row 1's can both be true (when BUTTON = 1 and DOOR = 1), so which row fires depends on their order; make them exclusive.",
    ]);
    const exclusive = mode({ rows: [...mode().rows, transition("Off", "PRESS", "Off", [], eq("BUTTON", 0))] });
    expect(check([exclusive])).toEqual([]);
  });

  it("warns about never-true guards, unreachable states and dead events", () => {
    const never: Expr = { kind: "and", a: eq("BUTTON", 1), b: eq("BUTTON", 0) };
    const m = mode({
      states: [...mode().states, state("Island")],
      events: [event("PRESS"), event("HOLD")],
      rows: [transition("Off", "PRESS", "On", [], never), mode().rows[1]!],
    });
    const issues = check([m]);
    expect(hasErrors(issues)).toBe(false);
    expect(lines([m])).toEqual([
      "rows 0 guard warning: This condition is never true, so the row never fires.",
      "states 2 name warning: No row leads from the initial state Off to Island, so the machine never enters it.",
      "events 1 name warning: No row handles HOLD, so raising it does nothing.",
    ]);
  });

  it("an unknown expression kind throws instead of being guessed", () => {
    const bad = { kind: "xor", a: pin("A"), b: pin("B") } as unknown as Expr;
    expect(() => check([mode({ rows: [transition("Off", "PRESS", "On", [], bad)] })])).toThrow(/not an expression/);
  });
});
