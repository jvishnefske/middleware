// The States view's decisions (src/ui/statesModel.ts): typing a name
// renames references only once the name is valid and free, dropdowns keep
// a broken reference visible, new actions / terms / transitions get useful
// defaults, the matrix overview finds holes, issues map onto rows and
// fields, fixes do what they say, the simulator line reads well, and a
// pin-table rename reaches the machines.

import { describe, expect, it } from "vitest";
import { type Machine, type MachineIssue, event, guardFromSimple, newMachine, state, transition } from "../src/model/machines";
import {
  actionOfKind,
  addAction,
  addTerm,
  applyMachineFix,
  cellText,
  errorCount,
  fieldSeverity,
  issuesAt,
  labelOptions,
  matrix,
  nameFree,
  nameOptions,
  newTransition,
  removeTerm,
  renamePinLabel,
  setTerm,
  simStateText,
  statesTabText,
  termValue,
  typeMachineId,
  typeName,
} from "../src/ui/statesModel";

const mode: Machine = {
  id: "Mode",
  name: "LED mode",
  initial: "Off",
  states: [state("Off"), state("On", [{ kind: "write", label: "LED", level: 1 }])],
  events: [event("PRESS"), event("RESET")],
  rows: [
    transition("Off", "PRESS", "On", [], guardFromSimple({ join: "and", terms: [{ label: "BUTTON", op: "eq", value: 0 }] })),
    transition("On", "PRESS", "Off", [{ kind: "raise", event: "RESET" }]),
  ],
};
const pins = [
  { label: "LED", func: "gpio-out" as const },
  { label: "BUTTON", func: "gpio-in" as const },
  { label: "POT", func: "adc" as const },
];

function issue(p: Partial<MachineIssue>): MachineIssue {
  return { machine: 0, list: "rows", row: 0, field: "next", severity: "error", message: "m", ...p };
}

describe("typing names", () => {
  it("renames the state, its rows, the initial state and the blocks when the name is valid and free", () => {
    const r = typeName(mode, "state", 0, "Dark", "Off");
    expect(r.machine.states[0]?.name).toBe("Dark");
    expect(r.machine.initial).toBe("Dark");
    expect(r.machine.rows.map((x) => [x.state, x.next])).toEqual([
      ["Dark", "On"],
      ["On", "Dark"],
    ]);
    expect(r.applied).toBe("Dark");
    expect(r.rename).toEqual({ kind: "state", id: "Mode", from: "Off", to: "Dark" });
  });

  it("does not merge references when the typed name collides with another state", () => {
    const r = typeName(mode, "state", 0, "on", "Off");
    expect(r.machine.states.map((s) => s.name)).toEqual(["on", "On"]);
    expect(r.machine.rows[0]?.state).toBe("Off");
    expect(r.machine.rows[1]?.state).toBe("On");
    expect(r.applied).toBe("Off");
    expect(r.rename).toBeUndefined();
  });

  it("lets references catch up from the last applied name once the name is free again", () => {
    const mid = typeName(mode, "state", 0, "On", "Off").machine;
    const r = typeName(mid, "state", 0, "On2", "Off");
    expect(r.machine.rows.map((x) => [x.state, x.next])).toEqual([
      ["On2", "On"],
      ["On", "On2"],
    ]);
    expect(r.rename).toEqual({ kind: "state", id: "Mode", from: "Off", to: "On2" });
  });

  it("keeps references on an invalid identifier such as an empty or digit-led name", () => {
    expect(typeName(mode, "state", 0, "", "Off").machine.rows[0]?.state).toBe("Off");
    expect(typeName(mode, "state", 0, "1x", "Off").rename).toBeUndefined();
    expect(nameFree(["A", "B"], 0, "b")).toBe(false);
    expect(nameFree(["A", "B"], 1, "b")).toBe(true);
  });

  it("renames an event in rows and raise actions", () => {
    const r = typeName(mode, "event", 1, "CLEAR", "RESET");
    expect(r.machine.rows[1]?.actions).toEqual([{ kind: "raise", event: "CLEAR" }]);
    expect(r.rename).toEqual({ kind: "event", id: "Mode", from: "RESET", to: "CLEAR" });
  });

  it("renames a machine id for blocks only when no other machine has it", () => {
    const two = [mode, { ...newMachine([mode]), id: "Door" }];
    const ok = typeMachineId(two, 0, "Light", "Mode");
    expect(ok.machines[0]?.id).toBe("Light");
    expect(ok.rename).toEqual({ kind: "machine", id: "Mode", from: "Mode", to: "Light" });
    const clash = typeMachineId(two, 0, "door", "Mode");
    expect(clash.machines[0]?.id).toBe("door");
    expect(clash.rename).toBeUndefined();
    expect(clash.applied).toBe("Mode");
  });
});

describe("dropdown options", () => {
  it("lists names and keeps a missing current value, marked", () => {
    expect(nameOptions(["Off", "On"], "On", "not a state")).toEqual([
      ["Off", "Off"],
      ["On", "On"],
    ]);
    expect(nameOptions(["Off"], "Gone", "not a state").at(-1)).toEqual(["Gone", "Gone (not a state)"]);
  });

  it("offers only pins of the right function for actions and guards", () => {
    expect(labelOptions(pins, ["gpio-out"], "LED", "x").map((o) => o[0])).toEqual(["LED"]);
    expect(labelOptions(pins, ["gpio-in"], "POT", "not an input in Pins")).toEqual([
      ["BUTTON", "BUTTON"],
      ["POT", "POT (not an input in Pins)"],
    ]);
  });
});

describe("actions, guard terms and transitions", () => {
  it("builds a new action of a kind, keeping the label or text that still applies", () => {
    const w = { kind: "write", label: "LED", level: 0 } as const;
    expect(actionOfKind("toggle", mode, pins, w)).toEqual({ kind: "toggle", label: "LED" });
    expect(actionOfKind("raise", mode, pins)).toEqual({ kind: "raise", event: "PRESS" });
    expect(actionOfKind("publish", mode, pins, { kind: "log", text: "hi" })).toEqual({ kind: "publish", key: "app/mode", text: "hi" });
    expect(actionOfKind("write", mode, [], undefined)).toEqual({ kind: "write", label: "LED", level: 1 });
  });

  it("adds at most four actions, toggling the first output pin", () => {
    let as = addAction([], mode, pins);
    expect(as).toEqual([{ kind: "toggle", label: "LED" }]);
    for (let i = 0; i < 6; i++) as = addAction(as, mode, pins);
    expect(as).toHaveLength(4);
    expect(addAction([], mode, [])).toEqual([{ kind: "log", text: "" }]);
  });

  it("edits guard terms within three and round-trips them through Expr", () => {
    let g = addTerm({ join: "and", terms: [] }, pins);
    expect(g.terms).toEqual([{ label: "BUTTON", op: "eq", value: 1 }]);
    g = addTerm(addTerm(addTerm(g, pins), pins), pins);
    expect(g.terms).toHaveLength(3);
    g = setTerm(g, 1, { op: "ne", value: 0 });
    expect(guardFromSimple(removeTerm(g, 0))).toEqual(guardFromSimple({ join: "and", terms: g.terms.slice(1) }));
    expect(termValue(" 12 ")).toBe(12);
    expect(termValue("1.5")).toBeNull();
    expect(termValue("")).toBeNull();
  });

  it("adds a new transition in the first empty cell, as a self-transition", () => {
    expect(newTransition(mode)).toEqual(transition("Off", "RESET", "Off"));
    expect(newTransition({ ...mode, events: [] })).toBeNull();
  });
});

describe("matrix overview", () => {
  it("lists each cell's rows with next state and guard marker, and labels holes", () => {
    const mx = matrix(mode);
    expect(mx).toHaveLength(2);
    expect(mx[0]?.[0]?.rows).toEqual([{ i: 0, next: "On", guarded: true }]);
    expect(cellText(mx[0]![0]!)).toEqual({ text: "On (if)", label: "Off on PRESS: row 1 to On, guarded. Go to row 1" });
    expect(cellText(mx[0]![1]!).text).toBe("—");
    expect(cellText(mx[0]![1]!).label).toMatch(/no transition/);
  });
});

describe("issues and fixes", () => {
  const issues = [
    issue({ row: 0, field: "next" }),
    issue({ row: 0, field: "guard", severity: "warning" }),
    issue({ row: null, list: "machine", field: "initial" }),
    issue({ machine: 1, row: 0 }),
  ];

  it("finds the issues of one row of one machine and the worst severity per field", () => {
    const on = issuesAt(issues, 0, "rows", 0);
    expect(on).toHaveLength(2);
    expect(fieldSeverity(on, ["next"])).toBe("error");
    expect(fieldSeverity(on, ["guard"])).toBe("warning");
    expect(fieldSeverity(on, ["state"])).toBeNull();
    expect(issuesAt(issues, 0, "machine", null)).toHaveLength(1);
  });

  it("counts only errors on the tab, since warnings do not block compiling", () => {
    expect(errorCount(issues)).toBe(3);
    expect(errorCount(issues, 1)).toBe(1);
    expect(statesTabText(issues)).toBe("States (3 to fix)");
    expect(statesTabText([issue({ severity: "warning" })])).toBe("States");
  });

  it("applies table fixes to the machine and hands pin fixes to the page", () => {
    const r1 = applyMachineFix(mode, { kind: "add-state", name: "Blink" });
    expect("machine" in r1 && r1.machine.states.at(-1)?.name).toBe("Blink");
    const r2 = applyMachineFix(mode, { kind: "set-initial", name: "On" });
    expect("machine" in r2 && r2.machine.initial).toBe("On");
    const r3 = applyMachineFix(mode, { kind: "add-event", name: "HOLD" });
    expect("machine" in r3 && r3.machine.events.at(-1)?.name).toBe("HOLD");
    expect(applyMachineFix(mode, { kind: "add-pin", label: "DOOR", func: "gpio-in" })).toEqual({ pin: { label: "DOOR", func: "gpio-in" } });
  });
});

describe("simulator and pin renames", () => {
  it("says the state and the transition that led to it", () => {
    expect(simStateText({ id: "Mode", state: "On", from: "Off", event: "PRESS" })).toBe("State: Mode = On (from Off on PRESS)");
    expect(simStateText({ id: "Mode", state: "Off" })).toBe("State: Mode = Off");
  });

  it("follows a pin-table label rename into guards and pin actions", () => {
    const [m] = renamePinLabel(renamePinLabel([mode], "BUTTON", "KEY"), "LED", "LAMP");
    expect(m?.rows[0]?.guard).toEqual(guardFromSimple({ join: "and", terms: [{ label: "KEY", op: "eq", value: 0 }] }));
    expect(m?.states[1]?.entry).toEqual([{ kind: "write", label: "LAMP", level: 1 }]);
    expect(m?.rows[1]?.actions).toEqual([{ kind: "raise", event: "RESET" }]);
  });
});
