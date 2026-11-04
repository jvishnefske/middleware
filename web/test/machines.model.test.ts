// State machines in the payload model (machines.ts): the pure transitions
// keep references consistent, the guard editor's simple form round-trips,
// block references follow renames, and the simulator reads the current
// state from the generated code's log lines.

import { describe, expect, it } from "vitest";
import {
  type Expr,
  type Machine,
  event,
  exprText,
  guardFromSimple,
  machineLogInit,
  machineLogMove,
  machinePinNeeds,
  machineRefsUsed,
  machineStates,
  newMachine,
  parseMachineLog,
  removeEvent,
  removeState,
  renameEvent,
  renameMachineRef,
  renameState,
  simpleGuard,
  state,
  transition,
} from "../src/model/machines";
import type { WorkspaceJson } from "../src/blocks/workspace";

const pin = (label: string): Expr => ({ kind: "pin", label });
const k = (value: number): Expr => ({ kind: "const", value });

const m: Machine = {
  id: "Mode",
  name: "",
  initial: "Off",
  states: [state("Off", [{ kind: "write", label: "LED", level: 0 }]), state("On", [{ kind: "raise", event: "TICK" }])],
  events: [event("PRESS"), event("TICK")],
  rows: [
    transition("Off", "PRESS", "On", [{ kind: "raise", event: "TICK" }], { kind: "cmp", op: "eq", a: pin("BUTTON"), b: k(0) }),
    transition("On", "PRESS", "Off"),
    transition("On", "TICK", "On", [{ kind: "toggle", label: "LED" }]),
  ],
};

describe("state machine transitions", () => {
  it("renaming a state or event renames every reference", () => {
    const s = renameState(m, 0, "Dark");
    expect(s.initial).toBe("Dark");
    expect(s.rows.map((r) => [r.state, r.next])).toEqual([
      ["Dark", "On"],
      ["On", "Dark"],
      ["On", "On"],
    ]);
    const e = renameEvent(m, 1, "BEAT");
    expect(e.rows.map((r) => r.event)).toEqual(["PRESS", "PRESS", "BEAT"]);
    expect(e.rows[0]?.actions).toEqual([{ kind: "raise", event: "BEAT" }]);
    expect(e.states[1]?.entry).toEqual([{ kind: "raise", event: "BEAT" }]);
    // The input is never changed.
    expect(m.initial).toBe("Off");
  });

  it("removing a state or event drops what depends on it", () => {
    const s = removeState(m, 0);
    expect(s.initial).toBe("On");
    expect(s.rows).toEqual([transition("On", "TICK", "On", [{ kind: "toggle", label: "LED" }])]);
    const e = removeEvent(m, 1);
    expect(e.rows.map((r) => r.event)).toEqual(["PRESS", "PRESS"]);
    expect(e.rows[0]?.actions).toEqual([]);
    expect(e.states[1]?.entry).toEqual([]);
  });

  it("new machines get free ids", () => {
    const a = newMachine([]);
    expect(a.id).toBe("Machine");
    expect(newMachine([a]).id).toBe("Machine2");
  });

  it("lists the pins a machine needs by function", () => {
    expect(machinePinNeeds(m)).toEqual([
      { label: "LED", func: "gpio-out" },
      { label: "BUTTON", func: "gpio-in" },
    ]);
  });
});

describe("guards", () => {
  it("the simple form round-trips through Expr within the depth limit", () => {
    const g = {
      join: "or" as const,
      terms: [
        { label: "A", op: "eq" as const, value: 1 },
        { label: "B", op: "ne" as const, value: 0 },
        { label: "C", op: "ge" as const, value: 1 },
      ],
    };
    const e = guardFromSimple(g);
    expect(e).not.toBeNull();
    expect(simpleGuard(e)).toEqual(g);
    expect(exprText(e!)).toBe("A = 1 or B ≠ 0 or C ≥ 1");
    expect(simpleGuard(null)).toEqual({ join: "and", terms: [] });
    expect(guardFromSimple({ join: "and", terms: [] })).toBeNull();
  });

  it("an expression the editor did not write has no simple form but has text", () => {
    const e: Expr = { kind: "not", a: { kind: "and", a: pin("A"), b: { kind: "or", a: pin("B"), b: k(1) } } };
    expect(simpleGuard(e)).toBeNull();
    expect(exprText(e)).toBe("not (A and (B or 1))");
  });
});

describe("blocks referring to machines", () => {
  const ws: WorkspaceJson = {
    blocks: {
      languageVersion: 0,
      blocks: [
        { type: "mw_sm_on_enter", id: "a", fields: { STATE: "Mode.On" }, inputs: { DO: { block: { type: "mw_sm_raise", id: "b", fields: { EVENT: "Mode.TICK" } } } } },
        { type: "mw_sm_in_state", id: "c", fields: { STATE: "Mode.Off" } },
      ],
    },
  };

  it("lists and renames state and event references", () => {
    expect(machineRefsUsed(ws)).toEqual({ events: ["Mode.TICK"], states: ["Mode.Off", "Mode.On"] });
    const r = renameMachineRef(ws, "state", "Mode", "On", "Lit");
    expect(machineRefsUsed(r).states).toEqual(["Mode.Lit", "Mode.Off"]);
    // A state rename leaves an event of the same name alone.
    expect(machineRefsUsed(renameMachineRef(ws, "state", "Mode", "TICK", "X")).events).toEqual(["Mode.TICK"]);
    const id = renameMachineRef(ws, "machine", "", "Mode", "Lamp");
    expect(machineRefsUsed(id)).toEqual({ events: ["Lamp.TICK"], states: ["Lamp.Off", "Lamp.On"] });
    expect(machineRefsUsed(ws).states).toEqual(["Mode.Off", "Mode.On"]);
  });
});

describe("the simulator's view of machine state", () => {
  it("parses the init and move log lines and nothing else", () => {
    expect(parseMachineLog(machineLogInit("Mode", "Off"))).toEqual({ id: "Mode", state: "Off" });
    expect(parseMachineLog(machineLogMove("Mode", "Off", "On", "PRESS"))).toEqual({ id: "Mode", state: "On", from: "Off", event: "PRESS" });
    for (const l of ["blink: started", "sm Mode: Off -> On", "sm Mode: Off on PRESS", "sm 1x: A", "sm Mode: A -> B on C extra"]) {
      expect(parseMachineLog(l)).toBeNull();
    }
    expect(machineStates(["sm Mode: Off", "hello", "sm Mode: Off -> On on PRESS", "sm Door: Shut"])).toEqual(
      new Map([
        ["Mode", "On"],
        ["Door", "Shut"],
      ]),
    );
  });
});
