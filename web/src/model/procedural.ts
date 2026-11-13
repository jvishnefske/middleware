// The S88 procedural control model (IR v4, plant.ts): phase classes, the
// equipment-independent interface recipes ask for, and equipment phases,
// a unit's implementation of a class on one of its equipment modules.
//
// Every equipment phase runs the S88 phase state model (recipe-core
// `phase`, ISA-88 Part 1 §5.2 without Pausing / Paused, plus PackML's
// Resetting): the commands Start, Hold, Restart, Stop, Abort and Reset
// move it, and each *acting* state (running, holding, restarting,
// stopping, aborting, resetting) may name a flow of the unit's logic as
// its body; the body ends by reporting Done. A state with no body
// finishes at once. Pure data; the rules live in Rust (recipe-core
// through protocol-wasm `checkRecipe` / `allocateRecipe`) and in
// plant.ts's reference checks.
//
//   PhaseClass     { id, name, params: ParamDef[] }   at most 4 params
//   ParamDef       { id, name, min, max, unit }      whole 32-bit numbers
//   EquipmentPhase { unit, em, id, class,
//                    ranges: { [paramId]: [min, max] },   every class param
//                    bodies: { [actingState]: flowId } }

export const MAX_CLASSES = 16;
export const MAX_PARAMS = 4;
/** Equipment phases per unit (recipe-core MAX_UNIT_PHASES). */
export const MAX_UNIT_PHASES = 16;
export const MAX_TEXT = 200;

/** The S88 phase states in wire-code order (recipe-core `PhaseState`). */
export const PHASE_STATES = ["Idle", "Running", "Complete", "Holding", "Held", "Restarting", "Stopping", "Stopped", "Aborting", "Aborted", "Resetting"] as const;
export type PhaseStateName = (typeof PHASE_STATES)[number];
/** The commands in wire-code order (codes 1..6). */
export const PHASE_COMMANDS = ["Start", "Hold", "Restart", "Stop", "Abort", "Reset"] as const;
export type PhaseCommand = (typeof PHASE_COMMANDS)[number];
/** The states whose logic runs and ends with Done: the keys of a
 * phase's `bodies`. */
export const ACTING_STATES = ["running", "holding", "restarting", "stopping", "aborting", "resetting"] as const;
export type ActingState = (typeof ACTING_STATES)[number];

/** The wire code of an acting state (recipe-core `PhaseState::code`). */
export const ACTING_CODE: Readonly<Record<ActingState, number>> = { running: 1, holding: 3, restarting: 5, stopping: 6, aborting: 8, resetting: 10 };

/** The phase state model's table (recipe-core `phase::next`), the next
 * state's code for each state (rows, in code order) and input (columns:
 * Start, Hold, Restart, Stop, Abort, Reset, Done), -1 where the input is
 * rejected. Generated phase servers (blocks/phaseGen.ts) switch on it;
 * web/test/phase.gen.test.ts checks it against protocol-wasm
 * `phaseModel()` and tools/payload-sim/tests/phase.rs the generated code
 * against `recipe_core::phase::next`, cell by cell. */
export const PHASE_NEXT: readonly (readonly number[])[] = [
  [1, -1, -1, -1, -1, -1, -1],
  [-1, 3, -1, 6, 8, -1, 2],
  [-1, -1, -1, -1, -1, 10, -1],
  [-1, -1, -1, 6, 8, -1, 4],
  [-1, -1, 5, 6, 8, -1, -1],
  [-1, 3, -1, 6, 8, -1, 1],
  [-1, -1, -1, -1, 8, -1, 7],
  [-1, -1, -1, -1, -1, 10, -1],
  [-1, -1, -1, -1, -1, -1, 9],
  [-1, -1, -1, -1, -1, 10, -1],
  [-1, -1, -1, 6, 8, -1, 0],
];

export interface ParamDef {
  /** Identifier recipes give values for. */
  id: string;
  /** Free text for the reader. */
  name: string;
  min: number;
  max: number;
  /** Unit of measure for the reader (e.g. "ms"). */
  unit: string;
}

export interface PhaseClass {
  id: string;
  name: string;
  params: readonly ParamDef[];
}

export interface EquipmentPhase {
  /** The unit's id (plant.ts). */
  unit: string;
  /** The equipment module's id within the unit. */
  em: string;
  /** The phase's name within the module (an identifier). */
  id: string;
  /** The phase class it implements. */
  class: string;
  /** Accepted range per class parameter (within the class's). */
  ranges: Readonly<Record<string, readonly [number, number]>>;
  /** Flow ids of the unit's logic run in each acting state. */
  bodies: Readonly<Partial<Record<ActingState, string>>>;
}

export interface ProceduralModel {
  classes: readonly PhaseClass[];
  phases: readonly EquipmentPhase[];
}

export const EMPTY_PROCEDURAL: ProceduralModel = { classes: [], phases: [] };

/** A one-click fix for a recipe issue (protocol-wasm `RecipeFix`);
 * recipe.ts `applyRecipeFix` applies it. */
export type RecipeFix =
  | { kind: "setArg"; step: number; param: string; value: number }
  | { kind: "removeArg"; step: number; param: string }
  | { kind: "removeTransition"; transition: number }
  | { kind: "removeFromTransition"; transition: number; side: "from" | "to"; step: string }
  | { kind: "removeUnitProc"; unitProc: number };

/** One issue `checkRecipe` reports (protocol-wasm `recipes::RecipeIssue`). */
export interface RecipeIssue {
  at: "recipe" | "class" | "unitProc" | "step" | "transition";
  index: number | null;
  sub: number | null;
  severity: "error" | "warning";
  message: string;
  fix?: RecipeFix;
}

type Obj = Record<string, unknown>;

export function isObj(v: unknown): v is Obj {
  return v !== null && typeof v === "object" && !Array.isArray(v);
}

export function extraKey(o: Obj, allowed: readonly string[]): string | undefined {
  return Object.keys(o).find((k) => !allowed.includes(k));
}

export function isInt32(v: unknown): v is number {
  return typeof v === "number" && Number.isInteger(v) && v >= -(2 ** 31) && v < 2 ** 31;
}

export function shortText(v: unknown): v is string {
  return typeof v === "string" && v.length <= MAX_TEXT;
}

/** `[A-Za-z][A-Za-z0-9_]*`, at most 24 characters (recipe-core `ident_ok`). */
export function isIdent(v: unknown): v is string {
  return typeof v === "string" && v.length <= 24 && /^[A-Za-z][A-Za-z0-9_]*$/.test(v);
}

function paramProblem(v: unknown, at: string): string | null {
  if (!isObj(v)) return `${at} must be an object`;
  const extra = extraKey(v, ["id", "name", "min", "max", "unit"]);
  if (extra !== undefined) return `${at} has an unknown field "${extra}"`;
  if (!isIdent(v.id)) return `${at}.id must be an identifier (a letter, then letters, digits or _; at most 24)`;
  if (!shortText(v.name) || !shortText(v.unit)) return `${at}.name and .unit must be text (at most ${MAX_TEXT} characters)`;
  if (!isInt32(v.min) || !isInt32(v.max)) return `${at}.min and .max must be whole 32-bit numbers`;
  return null;
}

/** A phase class's shape (names unique ignoring case is Rust's check). */
export function classProblem(v: unknown, at: string): string | null {
  if (!isObj(v)) return `${at} must be an object`;
  const extra = extraKey(v, ["id", "name", "params"]);
  if (extra !== undefined) return `${at} has an unknown field "${extra}"`;
  if (!isIdent(v.id)) return `${at}.id must be an identifier (a letter, then letters, digits or _; at most 24)`;
  if (!shortText(v.name)) return `${at}.name must be text (at most ${MAX_TEXT} characters)`;
  if (!Array.isArray(v.params) || v.params.length > MAX_PARAMS) return `${at}.params must be a list of at most ${MAX_PARAMS} parameters`;
  for (const [i, p] of v.params.entries()) {
    const problem = paramProblem(p, `${at}.params[${i}]`);
    if (problem) return problem;
  }
  return null;
}

/** An equipment phase's own shape; references are checked by plant.ts. */
export function phaseProblem(v: unknown, at: string): string | null {
  if (!isObj(v)) return `${at} must be an object`;
  const extra = extraKey(v, ["unit", "em", "id", "class", "ranges", "bodies"]);
  if (extra !== undefined) return `${at} has an unknown field "${extra}"`;
  for (const k of ["unit", "em", "id", "class"] as const) {
    if (k === "unit" ? typeof v[k] !== "string" : !isIdent(v[k])) return `${at}.${k} must be ${k === "unit" ? "text" : "an identifier"}`;
  }
  if (!isObj(v.ranges)) return `${at}.ranges must be an object`;
  for (const [k, r] of Object.entries(v.ranges)) {
    if (!Array.isArray(r) || r.length !== 2 || !isInt32(r[0]) || !isInt32(r[1])) return `${at}.ranges.${k} must be [min, max] whole 32-bit numbers`;
    if (r[0] > r[1]) return `${at}.ranges.${k} has its minimum above its maximum`;
  }
  if (!isObj(v.bodies)) return `${at}.bodies must be an object`;
  for (const [k, f] of Object.entries(v.bodies)) {
    if (!(ACTING_STATES as readonly string[]).includes(k)) return `${at}.bodies.${k} is not an acting state (one of ${ACTING_STATES.join(", ")})`;
    if (typeof f !== "string") return `${at}.bodies.${k} must be a flow id`;
  }
  return null;
}

export function copyProcedural(p: ProceduralModel): ProceduralModel {
  return {
    classes: p.classes.map((c) => ({ ...c, params: c.params.map((x) => ({ ...x })) })),
    phases: p.phases.map((ph) => ({
      ...ph,
      ranges: Object.fromEntries(Object.entries(ph.ranges).map(([k, r]) => [k, [r[0], r[1]] as const])),
      bodies: { ...ph.bodies },
    })),
  };
}
