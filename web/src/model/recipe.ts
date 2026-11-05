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

import { MAX_TEXT, type PhaseClass, type RecipeFix, extraKey, isIdent, isInt32, isObj, shortText } from "./procedural";

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

// ---- pure edits (the Recipe view, ui/recipeView.ts)
//
// Each takes a recipe and returns a new one, or the same object when the
// edit does not apply. They keep round 7's chart rules by construction
// where the editor can: `connectSteps` never makes a loop or puts a step
// on one side of two transitions (it extends the transition the step
// already has: a parallel divergence or convergence), so a recipe edited
// only here stays a marked graph; a choice between branches (Q20) is not
// offered. What is still wrong (missing arguments, ranges, a chart
// imported from a file) is recipe-core's check, shown with fixes
// (`applyRecipeFix`).


export type Side = "from" | "to";

/** The transition step `id` leads into (its `from` side), or -1. */
export function outgoing(r: MasterRecipe, id: string): number {
  return r.transitions.findIndex((t) => t.from.includes(id));
}

/** The transition that starts step `id` (its `to` side), or -1. */
export function incoming(r: MasterRecipe, id: string): number {
  return r.transitions.findIndex((t) => t.to.includes(id));
}

/** Whether step `to` can be reached from step `from` through
 * transitions (`from` itself counts). Bounded: each step is visited
 * once. */
export function reaches(r: MasterRecipe, from: string, to: string): boolean {
  const seen = new Set<string>([from]);
  const todo = [from];
  for (let n = 0; n < MAX_STEPS * 4 && todo.length > 0; n++) {
    const s = todo.pop()!;
    if (s === to) return true;
    for (const t of r.transitions) {
      if (!t.from.includes(s)) continue;
      for (const x of t.to) {
        if (!seen.has(x)) {
          seen.add(x);
          todo.push(x);
        }
      }
    }
  }
  return false;
}

function freeId(prefix: string, taken: readonly string[]): string {
  const low = new Set(taken.map((x) => x.toLowerCase()));
  for (let i = 1; i <= 64; i++) if (!low.has(`${prefix}${i}`.toLowerCase())) return `${prefix}${i}`;
  return `${prefix}${taken.length + 1}`;
}

export function freeStepId(r: MasterRecipe): string {
  return freeId("Step", r.steps.map((s) => s.id));
}

function freeTransitionId(r: MasterRecipe): string {
  return freeId("t", r.transitions.map((t) => t.id));
}

/** Drops transitions with an empty side. */
function tidy(transitions: readonly RecipeTransition[]): RecipeTransition[] {
  return transitions.filter((t) => t.from.length > 0 && t.to.length > 0);
}

export type ConnectResult = { recipe: MasterRecipe; refused: string | null };

/** "After step `from` comes step `to`": joins them with a transition.
 * When `from` already leads into a transition, `to` is added to it (a
 * parallel divergence); when `to` is already started by one, `from` is
 * added to that (a convergence); otherwise a new transition. Refused (the
 * recipe unchanged, with the reason) for a step to itself, a loop, two
 * different transitions on the two ends, or the transition limit. */
export function connectSteps(r: MasterRecipe, from: string, to: string): ConnectResult {
  const refuse = (refused: string): ConnectResult => ({ recipe: r, refused });
  if (!r.steps.some((s) => s.id === from) || !r.steps.some((s) => s.id === to)) return refuse("there is no such step");
  if (from === to) return refuse("a step cannot follow itself");
  if (reaches(r, to, from)) return refuse(`"${to}" already leads to "${from}"; a recipe runs each step once`);
  const out = outgoing(r, from);
  const inc = incoming(r, to);
  if (out >= 0 && out === inc) return { recipe: r, refused: null };
  if (out >= 0 && inc >= 0) return refuse(`"${from}" already leads into another transition and "${to}" is already started by one; join them there`);
  const transitions = r.transitions.map((t, i) =>
    i === out ? { ...t, from: [...t.from], to: [...t.to, to] } : i === inc ? { ...t, from: [...t.from, from], to: [...t.to] } : { ...t, from: [...t.from], to: [...t.to] },
  );
  if (out < 0 && inc < 0) {
    if (r.transitions.length >= MAX_TRANSITIONS) return refuse(`at most ${MAX_TRANSITIONS} transitions`);
    transitions.push({ id: freeTransitionId(r), from: [from], to: [to] });
  }
  return { recipe: { ...r, transitions }, refused: null };
}

/** Takes step `id` off `side` of transition `t`; a transition left with
 * an empty side goes. */
export function detach(r: MasterRecipe, t: number, side: Side, id: string): MasterRecipe {
  const tr = r.transitions[t];
  if (!tr || !tr[side].includes(id)) return r;
  const next = r.transitions.map((x, i) => (i === t ? { ...x, [side]: x[side].filter((s) => s !== id) } : x));
  return { ...r, transitions: tidy(next) };
}

export function removeTransition(r: MasterRecipe, t: number): MasterRecipe {
  if (!r.transitions[t]) return r;
  return { ...r, transitions: r.transitions.filter((_, i) => i !== t) };
}

/** Each parameter of `cls` at its minimum. */
export function defaultArgs(cls: PhaseClass | undefined): Record<string, number> {
  return Object.fromEntries((cls?.params ?? []).map((p) => [p.id, p.min]));
}

/** Canvas rows: a new step goes below its lane's lowest one. */
export const STEP_ROW = 120;

/** Adds a step of class `cls` (default: the first class) in unit
 * procedure `up` (default: the first), every parameter at the class's
 * minimum. */
export function addStep(r: MasterRecipe, classes: readonly PhaseClass[], up?: string, cls?: string): { recipe: MasterRecipe; id: string | null } {
  const lane = up ?? r.unitProcedures[0]?.id;
  if (lane === undefined || r.steps.length >= MAX_STEPS) return { recipe: r, id: null };
  const c = classes.find((x) => x.id === cls) ?? classes[0];
  const id = freeStepId(r);
  const bottom = r.steps.filter((s) => s.unitProcedure === lane).reduce((y, s) => Math.max(y, s.y + STEP_ROW), 40);
  const step: RecipeStep = { id, name: "", unitProcedure: lane, operation: "", class: c?.id ?? "", params: defaultArgs(c), x: 0, y: Math.min(MAX_COORD, bottom) };
  return { recipe: { ...r, steps: [...r.steps, step] }, id };
}

export function removeStep(r: MasterRecipe, id: string): MasterRecipe {
  if (!r.steps.some((s) => s.id === id)) return r;
  const transitions = tidy(r.transitions.map((t) => ({ ...t, from: t.from.filter((s) => s !== id), to: t.to.filter((s) => s !== id) })));
  return { ...r, steps: r.steps.filter((s) => s.id !== id), transitions };
}

function mapStep(r: MasterRecipe, id: string, f: (s: RecipeStep) => RecipeStep): MasterRecipe {
  if (!r.steps.some((s) => s.id === id)) return r;
  return { ...r, steps: r.steps.map((s) => (s.id === id ? f(s) : s)) };
}

/** Renames step `from` to `to` everywhere (refused when `to` is taken,
 * ignoring case). */
export function renameStep(r: MasterRecipe, from: string, to: string): MasterRecipe {
  if (from === to || r.steps.some((s) => s.id !== from && s.id.toLowerCase() === to.toLowerCase())) return r;
  const swap = (ids: readonly string[]) => ids.map((x) => (x === from ? to : x));
  const next = mapStep(r, from, (s) => ({ ...s, id: to }));
  if (next === r) return r;
  return { ...next, transitions: next.transitions.map((t) => ({ ...t, from: swap(t.from), to: swap(t.to) })) };
}

export function setStepText(r: MasterRecipe, id: string, patch: { name?: string; operation?: string }): MasterRecipe {
  return mapStep(r, id, (s) => ({ ...s, ...patch }));
}

/** Changes a step's class: arguments of the new class's parameters that
 * the step already had stay when in range, the rest start at the
 * minimum. */
export function setStepClass(r: MasterRecipe, id: string, cls: string, classes: readonly PhaseClass[]): MasterRecipe {
  const c = classes.find((x) => x.id === cls);
  return mapStep(r, id, (s) => {
    const params: Record<string, number> = {};
    for (const p of c?.params ?? []) {
      const v = s.params[p.id];
      params[p.id] = v !== undefined && v >= p.min && v <= p.max ? v : p.min;
    }
    return { ...s, class: cls, params };
  });
}

/** Sets (or, with null, removes) one argument. */
export function setArg(r: MasterRecipe, id: string, param: string, value: number | null): MasterRecipe {
  return mapStep(r, id, (s) => {
    const params = { ...s.params };
    if (value === null) delete params[param];
    else params[param] = value;
    return { ...s, params };
  });
}

/** Moves a step on the canvas (clamped to the drawing). */
export function moveStep(r: MasterRecipe, id: string, y: number): MasterRecipe {
  return mapStep(r, id, (s) => ({ ...s, y: Math.max(0, Math.min(MAX_COORD, Math.round(y))) }));
}

/** Moves a step into unit procedure `up`'s lane, at its row unless a
 * step there is within a row of it (then below that lane's lowest). */
export function setStepLane(r: MasterRecipe, id: string, up: string): MasterRecipe {
  if (!r.unitProcedures.some((u) => u.id === up)) return r;
  const others = r.steps.filter((s) => s.unitProcedure === up && s.id !== id);
  return mapStep(r, id, (s) => {
    const clash = others.some((o) => Math.abs(o.y - s.y) < STEP_ROW * 0.75);
    const y = clash ? Math.min(MAX_COORD, others.reduce((m, o) => Math.max(m, o.y + STEP_ROW), 0)) : s.y;
    return { ...s, unitProcedure: up, y };
  });
}

export function addUnitProcedure(r: MasterRecipe): MasterRecipe {
  if (r.unitProcedures.length >= MAX_UNIT_PROCEDURES) return r;
  const id = freeId("Proc", r.unitProcedures.map((u) => u.id));
  return { ...r, unitProcedures: [...r.unitProcedures, { id, name: "", unitClass: null }] };
}

/** Removes unit procedure `i` when no step runs in it. */
export function removeUnitProcedure(r: MasterRecipe, i: number): MasterRecipe {
  const u = r.unitProcedures[i];
  if (!u || r.steps.some((s) => s.unitProcedure === u.id)) return r;
  return { ...r, unitProcedures: r.unitProcedures.filter((_, j) => j !== i) };
}

/** Edits unit procedure `i`; a new id is carried to its steps (refused
 * when taken, ignoring case). */
export function setUnitProcedure(r: MasterRecipe, i: number, patch: Partial<UnitProcedure>): MasterRecipe {
  const u = r.unitProcedures[i];
  if (!u) return r;
  const id = patch.id ?? u.id;
  if (id !== u.id && r.unitProcedures.some((x, j) => j !== i && x.id.toLowerCase() === id.toLowerCase())) return r;
  return {
    ...r,
    unitProcedures: r.unitProcedures.map((x, j) => (j === i ? { ...x, ...patch, id } : x)),
    steps: id === u.id ? r.steps : r.steps.map((s) => (s.unitProcedure === u.id ? { ...s, unitProcedure: id } : s)),
  };
}

/** Applies a check's one-click fix. */
export function applyRecipeFix(r: MasterRecipe, fix: RecipeFix): MasterRecipe {
  switch (fix.kind) {
    case "setArg":
    case "removeArg": {
      const s = r.steps[fix.step];
      if (!s) return r;
      return setArg(r, s.id, fix.param, fix.kind === "setArg" ? fix.value : null);
    }
    case "removeTransition":
      return removeTransition(r, fix.transition);
    case "removeFromTransition":
      return detach(r, fix.transition, fix.side, fix.step);
    case "removeUnitProc":
      return removeUnitProcedure(r, fix.unitProc);
  }
}

/** The steps in run order: the initial ones (no transition starts them)
 * first, then each as soon as everything before it is placed (ties in
 * list order), then whatever a loop leaves unplaced. Bounded by the step
 * count. */
export function runOrder(r: MasterRecipe): RecipeStep[] {
  const placed = new Set<string>();
  const out: RecipeStep[] = [];
  for (let round = 0; round <= r.steps.length && out.length < r.steps.length; round++) {
    let moved = false;
    for (const s of r.steps) {
      if (placed.has(s.id)) continue;
      const t = r.transitions[incoming(r, s.id)];
      if (!t || t.from.every((f) => placed.has(f) || !r.steps.some((x) => x.id === f))) {
        placed.add(s.id);
        out.push(s);
        moved = true;
      }
    }
    if (!moved) break;
  }
  for (const s of r.steps) if (!placed.has(s.id)) out.push(s);
  return out;
}

/** A new recipe with one unit procedure and no steps. */
export function newRecipe(existing: readonly MasterRecipe[]): MasterRecipe {
  const id = freeId("Recipe", existing.map((r) => r.id));
  return { id, name: "", unitProcedures: [{ id: "Proc1", name: "", unitClass: null }], steps: [], transitions: [] };
}
