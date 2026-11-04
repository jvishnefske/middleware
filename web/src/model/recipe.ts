// The S88 master recipe (IR v4, plant.ts): an equipment-independent
// procedure. Unit procedures say what must run on one unit (optionally of
// a unit class); steps run a phase class with parameter values within one
// unit procedure (operations are a grouping label); transitions join
// steps into a sequential function chart: when every `from` step's phase
// is Complete, every `to` step starts. Steps never name pins or boards;
// allocation (recipe-core `alloc`, protocol-wasm `allocateRecipe`) binds
// unit procedures to units and steps to equipment phases.
//
//   MasterRecipe   { id, name, unitProcedures, steps, transitions }
//   UnitProcedure  { id, name, unitClass: string | null }
//   RecipeStep     { id, name, unitProcedure, operation, class,
//                    params: { [paramId]: number }, x, y }   canvas position
//   RecipeTransition { id, from: stepId[], to: stepId[] }
//
// The shape check here is strict and bounded; what the chart may contain
// (known names, every parameter in range, no step in two transitions on
// one side, no cycle) is recipe-core's `recipe::check`, reached through
// `checkRecipe` (protocol-wasm).

import { MAX_TEXT, extraKey, isIdent, isInt32, isObj, shortText } from "./procedural";

export const MAX_RECIPES = 4;
export const MAX_UNIT_PROCEDURES = 4;
export const MAX_STEPS = 16;
export const MAX_TRANSITIONS = 16;
/** Canvas coordinates, as for flows. */
export const MAX_COORD = 4000;

export interface UnitProcedure {
  id: string;
  name: string;
  /** The unit class a bound unit must have, or null for any. */
  unitClass: string | null;
}

export interface RecipeStep {
  id: string;
  name: string;
  /** The unit procedure's id. */
  unitProcedure: string;
  /** Grouping label (an ISA-88 operation) for the reader. */
  operation: string;
  /** The phase class's id. */
  class: string;
  /** One whole 32-bit value per class parameter. */
  params: Readonly<Record<string, number>>;
  x: number;
  y: number;
}

export interface RecipeTransition {
  id: string;
  from: readonly string[];
  to: readonly string[];
}

export interface MasterRecipe {
  id: string;
  name: string;
  unitProcedures: readonly UnitProcedure[];
  steps: readonly RecipeStep[];
  transitions: readonly RecipeTransition[];
}

function idList(v: unknown, max: number): v is string[] {
  return Array.isArray(v) && v.length <= max && v.every((x) => typeof x === "string" && x.length <= MAX_TEXT);
}

function upProblem(v: unknown, at: string): string | null {
  if (!isObj(v)) return `${at} must be an object`;
  const extra = extraKey(v, ["id", "name", "unitClass"]);
  if (extra !== undefined) return `${at} has an unknown field "${extra}"`;
  if (!shortText(v.id) || !shortText(v.name)) return `${at}.id and .name must be text (at most ${MAX_TEXT} characters)`;
  if (v.unitClass !== null && !shortText(v.unitClass)) return `${at}.unitClass must be text or null`;
  return null;
}

function stepProblem(v: unknown, at: string): string | null {
  if (!isObj(v)) return `${at} must be an object`;
  const extra = extraKey(v, ["id", "name", "unitProcedure", "operation", "class", "params", "x", "y"]);
  if (extra !== undefined) return `${at} has an unknown field "${extra}"`;
  for (const k of ["id", "name", "unitProcedure", "operation", "class"] as const) {
    if (!shortText(v[k])) return `${at}.${k} must be text (at most ${MAX_TEXT} characters)`;
  }
  if (!isObj(v.params) || Object.keys(v.params).length > 8) return `${at}.params must be an object of at most 8 values`;
  for (const [k, x] of Object.entries(v.params)) {
    if (!isInt32(x)) return `${at}.params.${k} must be a whole 32-bit number`;
  }
  const coord = (c: unknown) => typeof c === "number" && Number.isInteger(c) && c >= 0 && c <= MAX_COORD;
  if (!coord(v.x) || !coord(v.y)) return `${at}.x and .y must be whole numbers from 0 to ${MAX_COORD}`;
  return null;
}

function transitionProblem(v: unknown, at: string): string | null {
  if (!isObj(v)) return `${at} must be an object`;
  const extra = extraKey(v, ["id", "from", "to"]);
  if (extra !== undefined) return `${at} has an unknown field "${extra}"`;
  if (!shortText(v.id)) return `${at}.id must be text`;
  if (!idList(v.from, MAX_STEPS) || !idList(v.to, MAX_STEPS)) return `${at}.from and .to must be lists of at most ${MAX_STEPS} step ids`;
  return null;
}

/** A master recipe's shape. */
export function recipeProblem(v: unknown, at: string): string | null {
  if (!isObj(v)) return `${at} must be an object`;
  const extra = extraKey(v, ["id", "name", "unitProcedures", "steps", "transitions"]);
  if (extra !== undefined) return `${at} has an unknown field "${extra}"`;
  if (!isIdent(v.id)) return `${at}.id must be an identifier (a letter, then letters, digits or _; at most 24)`;
  if (!shortText(v.name)) return `${at}.name must be text (at most ${MAX_TEXT} characters)`;
  const lists: [string, unknown, number, (x: unknown, at: string) => string | null][] = [
    ["unitProcedures", v.unitProcedures, MAX_UNIT_PROCEDURES, upProblem],
    ["steps", v.steps, MAX_STEPS, stepProblem],
    ["transitions", v.transitions, MAX_TRANSITIONS, transitionProblem],
  ];
  for (const [name, list, max, check] of lists) {
    if (!Array.isArray(list) || list.length > max) return `${at}.${name} must be a list of at most ${max}`;
    for (const [i, x] of list.entries()) {
      const p = check(x, `${at}.${name}[${i}]`);
      if (p) return p;
    }
  }
  return null;
}

export function copyRecipe(r: MasterRecipe): MasterRecipe {
  return {
    ...r,
    unitProcedures: r.unitProcedures.map((u) => ({ ...u })),
    steps: r.steps.map((s) => ({ ...s, params: { ...s.params } })),
    transitions: r.transitions.map((t) => ({ ...t, from: [...t.from], to: [...t.to] })),
  };
}

/** Allocation's result (protocol-wasm `allocateRecipe`). */
export type Allocation =
  | { ok: true; bindings: { step: string; unit: string; em: string; phase: string }[] }
  | { ok: false; message: string; unitProcedure: string | null; reasons: { unit: string; reason: string }[] };

/** A scripted event for `simulateRecipe`. */
export type ScriptEvent = { at: number; kind: "start" | "hold" | "restart" | "stop" | "abort" | "reset" } | { at: number; kind: "lose" | "find"; unit: string };

/** A simulated run (protocol-wasm `simulateRecipe`). */
export type SimulatedRun =
  | { ok: true; state: string; bindings: { step: string; unit: string; em: string; phase: string }[]; lines: { t: number; kind: string; text: string }[] }
  | Extract<Allocation, { ok: false }>;
