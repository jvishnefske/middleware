// The plant document (IR version 4): the payload model split into the
// ISA-88 (S88) models (design.md, "S88 recipe model").
//
//   { format: "middleware-payload", version: 4,
//     physical:   { units: Unit[] },             1..8 units
//     procedural: ProceduralModel,               procedural.ts
//     recipes:    MasterRecipe[] }               recipe.ts, at most 4
//
//   Unit { id, name, class,                      unit class (identifier)
//          node: string | null,                  16 hex digits: the node id
//                                                sys/info reports, once bound
//          board: TargetId,
//          cms: PinTable,                        control modules: the pin table
//          ems: EquipmentModule[],               at most 8
//          logic: [BlocksSource, ...Flow[]],     the unit's payload logic
//          machines: Machine[] }
//   EquipmentModule { id, name, cms: string[] }  pin labels it owns
//
// Physical model: a unit is one board; its control modules are the rows
// of its pin table (checked by the firmwares' pin rules, pins.ts); an
// equipment module groups control modules under a name. Procedural model:
// phase classes and the equipment phases units implement them with, each
// acting state's body a flow of that unit's logic. Recipes: the master
// recipes, which name phase classes only.
//
// A unit's payload is the version-3 model of payload.ts restricted to that
// unit (`unitPayload`): board, pins = cms, logic, machines. That is what
// the generator compiles, so the split adds nothing to the generated code.
//
// Migration: a version 1-3 payload (or a legacy Blocks export) becomes a
// plant of one unit, "unit1" of class "Unit", unbound (node null), with no
// equipment modules, no phases and no recipes; its logic is kept as the
// unit's basic control. `unitPayload` of that unit is the original model,
// so its code is byte-for-byte unchanged (web/test/plant.test.ts). It is
// deliberately not wrapped in an "app" phase: a phase only runs when
// started, so wrapping would change what an existing payload does.
//
// `plantProblem` is strict about shape and references: unknown keys, wrong
// types, an equipment module owning a pin the unit's table lacks (or a
// pin two modules own), a phase on an unknown unit, module or class, a
// phase without a range for every class parameter (or with one outside the
// class's), a body naming a flow the unit does not have, whose entry is
// not that phase state's (`{on: "phase", phase: "<em>.<phase>", state}`;
// so no flow is two bodies), duplicate ids. `phaseSpecs` and
// `phaseDecls` give the generator and `checkFlows` a unit's phases. What a recipe's chart may contain and
// whether units can run it is recipe-core's (protocol-wasm `checkRecipe`,
// `allocateRecipe`, `simulateRecipe`).

import type { ProfileId } from "../blocks/profiles";
import type { WorkspaceJson } from "../blocks/workspace";
import type { BoardId } from "../onboarding/boards";
import type { Flow, PhaseDecl } from "./flows";
import type { PhaseSpec } from "../blocks/phaseGen";
import type { Machine } from "./machines";
import { type BlocksSource, MODEL_FORMAT, type PayloadModel, flowsOf, makeModel, modelProblem, parseModel } from "./payload";
import { copyFlows } from "./flows";
import { copyMachines } from "./machines";
import { type PinTable } from "./pins";
import {
  EMPTY_PROCEDURAL,
  type EquipmentPhase,
  MAX_CLASSES,
  MAX_TEXT,
  MAX_UNIT_PHASES,
  type PhaseClass,
  type ProceduralModel,
  classProblem,
  copyProcedural,
  extraKey,
  isIdent,
  isObj,
  phaseProblem,
  shortText,
} from "./procedural";
import { MAX_RECIPES, type MasterRecipe, copyRecipe, recipeProblem } from "./recipe";
import type { TargetId } from "./target";

export const PLANT_VERSION = 4;
export const MAX_UNITS = 8;
export const MAX_EMS = 8;
/** The unit a migrated payload becomes. */
export const DEFAULT_UNIT_ID = "unit1";
export const DEFAULT_UNIT_CLASS = "Unit";

export interface EquipmentModule {
  id: string;
  name: string;
  /** Labels of the unit's pin-table rows (control modules) it owns. */
  cms: readonly string[];
}

export interface Unit {
  /** Unique ignoring case: a letter, then letters, digits, _ or -; at most 24. */
  id: string;
  name: string;
  /** Unit class (an identifier) recipes may constrain to. */
  class: string;
  /** The node id (16 lowercase hex digits) once bound to a board, or null. */
  node: string | null;
  board: TargetId;
  /** Control modules: the unit's pin table. */
  cms: PinTable;
  ems: readonly EquipmentModule[];
  /** The Blocks workspace first, then the flows. */
  logic: readonly [BlocksSource, ...Flow[]];
  machines: readonly Machine[];
}

export interface PlantModel {
  format: typeof MODEL_FORMAT;
  version: typeof PLANT_VERSION;
  physical: { units: readonly Unit[] };
  procedural: ProceduralModel;
  recipes: readonly MasterRecipe[];
}

export function makePlant(units: readonly Unit[], procedural: ProceduralModel = EMPTY_PROCEDURAL, recipes: readonly MasterRecipe[] = []): PlantModel {
  return { format: MODEL_FORMAT, version: PLANT_VERSION, physical: { units }, procedural, recipes };
}

/** A unit holding `m`'s board, pins, logic and machines. */
export function unitFromPayload(m: PayloadModel, id: string = DEFAULT_UNIT_ID): Unit {
  return { id, name: "", class: DEFAULT_UNIT_CLASS, node: null, board: m.board, cms: m.pins, ems: [], logic: m.logic, machines: m.machines };
}

/** The version-3 model's plant: one unit, no phases, no recipes. */
export function plantFromPayload(m: PayloadModel): PlantModel {
  return makePlant([unitFromPayload(m)]);
}

export function unitById(p: PlantModel, id: string): Unit | undefined {
  return p.physical.units.find((u) => u.id === id);
}

/** The unit's payload: what the generator compiles for it. */
export function unitPayload(p: PlantModel, id: string): PayloadModel | null {
  const u = unitById(p, id);
  return u ? unitPayloadOf(u) : null;
}

/** The one unit's payload when the plant is a plain payload (one unit, no
 * equipment modules, phases, classes or recipes), else null. */
export function plainPayloadOf(p: PlantModel): PayloadModel | null {
  const [u, ...rest] = p.physical.units;
  if (!u || rest.length > 0 || u.ems.length > 0) return null;
  if (p.procedural.classes.length > 0 || p.procedural.phases.length > 0 || p.recipes.length > 0) return null;
  return unitPayload(p, u.id);
}

export function serializePlant(p: PlantModel): string {
  return JSON.stringify(p, null, 2);
}

/** The equipment phases of one unit, in plant order. */
export function phasesOf(p: PlantModel, unit: string): EquipmentPhase[] {
  return p.procedural.phases.filter((ph) => ph.unit === unit);
}

/** The unit's equipment phases as the generator serves them
 * (phaseGen.ts): ranges in class parameter order. A phase whose class is
 * missing gets no parameters (the plant check reports it). */
export function phaseSpecs(p: PlantModel, unit: string): PhaseSpec[] {
  const classes = new Map(p.procedural.classes.map((c) => [c.id, c]));
  return phasesOf(p, unit).map((ph) => ({
    em: ph.em,
    id: ph.id,
    params: (classes.get(ph.class)?.params ?? []).map((param) => {
      const r = ph.ranges[param.id] ?? [param.min, param.max];
      return { id: param.id, lo: r[0], hi: r[1] };
    }),
  }));
}

/** The unit's equipment phases as `checkFlows` reads them. */
export function phaseDecls(p: PlantModel, unit: string): PhaseDecl[] {
  return phaseSpecs(p, unit).map((s) => ({ em: s.em, phase: s.id, params: s.params.map((x) => x.id) }));
}

/** What allocation reads per unit (protocol-wasm `allocateRecipe`):
 * ranges in the class's parameter order. `online` says whether discovery
 * sees a unit; `runMs` how long a simulated phase runs. */
export interface AllocUnit {
  id: string;
  class: string;
  online: boolean;
  phases: { em: string; name: string; class: string; ranges: [number, number][]; runMs: number }[];
}

export function allocationUnits(p: PlantModel, online: (unit: Unit) => boolean = () => true, runMs: (ph: EquipmentPhase) => number = () => 100): AllocUnit[] {
  const classes = new Map(p.procedural.classes.map((c) => [c.id, c]));
  return p.physical.units.map((u) => ({
    id: u.id,
    class: u.class,
    online: online(u),
    phases: phasesOf(p, u.id).map((ph) => ({
      em: ph.em,
      name: ph.id,
      class: ph.class,
      ranges: (classes.get(ph.class)?.params ?? []).map((param) => {
        const r = ph.ranges[param.id];
        return r ? [r[0], r[1]] : [1, 0];
      }),
      runMs: runMs(ph),
    })),
  }));
}

function isUnitId(v: unknown): v is string {
  return typeof v === "string" && /^[A-Za-z][A-Za-z0-9_-]{0,23}$/.test(v);
}

function firstDuplicate(ids: readonly string[]): number {
  const seen = new Set<string>();
  for (const [i, id] of ids.entries()) {
    const k = id.toLowerCase();
    if (seen.has(k)) return i;
    seen.add(k);
  }
  return -1;
}

function unitProblem(v: unknown, at: string): string | null {
  if (!isObj(v)) return `${at} must be an object`;
  const extra = extraKey(v, ["id", "name", "class", "node", "board", "cms", "ems", "logic", "machines"]);
  if (extra !== undefined) return `${at} has an unknown field "${extra}"`;
  if (!isUnitId(v.id)) return `${at}.id must start with a letter and use only letters, digits, _ or - (at most 24)`;
  if (!shortText(v.name)) return `${at}.name must be text (at most ${MAX_TEXT} characters)`;
  if (!isIdent(v.class)) return `${at}.class must be an identifier (a letter, then letters, digits or _; at most 24)`;
  if (v.node !== null && !(typeof v.node === "string" && /^[0-9a-f]{16}$/.test(v.node))) return `${at}.node must be 16 lowercase hex digits or null`;
  // The unit's payload part is exactly a version-3 model's.
  const payload = modelProblem({ format: MODEL_FORMAT, version: 3, board: v.board, pins: v.cms, logic: v.logic, machines: v.machines });
  if (payload) return `${at}.${payload.replace(/^pins\b/, "cms")}`;
  if (!Array.isArray(v.ems) || v.ems.length > MAX_EMS) return `${at}.ems must be a list of at most ${MAX_EMS} equipment modules`;
  const labels = new Set((v.cms as PinTable).rows.map((r) => r.label));
  const owner = new Map<string, string>();
  for (const [i, em] of v.ems.entries()) {
    const eat = `${at}.ems[${i}]`;
    if (!isObj(em)) return `${eat} must be an object`;
    const x = extraKey(em, ["id", "name", "cms"]);
    if (x !== undefined) return `${eat} has an unknown field "${x}"`;
    if (!isIdent(em.id)) return `${eat}.id must be an identifier (a letter, then letters, digits or _; at most 24)`;
    if (!shortText(em.name)) return `${eat}.name must be text (at most ${MAX_TEXT} characters)`;
    if (!Array.isArray(em.cms) || em.cms.length > 64 || !em.cms.every((c) => typeof c === "string")) return `${eat}.cms must be a list of pin labels`;
    for (const label of em.cms as string[]) {
      if (!labels.has(label)) return `${eat}.cms names "${label}", which is not in the unit's pin table`;
      const other = owner.get(label);
      if (other !== undefined) return `${eat}.cms names "${label}", which equipment module "${other}" already owns`;
      owner.set(label, em.id);
    }
  }
  const dup = firstDuplicate((v.ems as EquipmentModule[]).map((e) => e.id));
  if (dup >= 0) return `${at}.ems[${dup}].id repeats an earlier equipment module's (ignoring case)`;
  return null;
}

function referencesProblem(units: readonly Unit[], proc: ProceduralModel): string | null {
  const classes = new Map<string, PhaseClass>(proc.classes.map((c) => [c.id, c]));
  const perUnit = new Map<string, number>();
  const seen = new Set<string>();
  for (const [i, ph] of proc.phases.entries()) {
    const at = `procedural.phases[${i}]`;
    const unit = units.find((u) => u.id === ph.unit);
    if (!unit) return `${at}.unit "${ph.unit}" is not a unit of the plant`;
    if (!unit.ems.some((e) => e.id === ph.em)) return `${at}.em "${ph.em}" is not an equipment module of unit "${unit.id}"`;
    const cls = classes.get(ph.class);
    if (!cls) return `${at}.class "${ph.class}" is not a phase class`;
    const n = (perUnit.get(unit.id) ?? 0) + 1;
    perUnit.set(unit.id, n);
    if (n > MAX_UNIT_PHASES) return `unit "${unit.id}" has more than ${MAX_UNIT_PHASES} equipment phases`;
    const key = `${unit.id}/${ph.em}.${ph.id}`.toLowerCase();
    if (seen.has(key)) return `${at} repeats phase ${ph.em}.${ph.id} of unit "${unit.id}" (ignoring case)`;
    seen.add(key);
    for (const param of cls.params) {
      const r = ph.ranges[param.id];
      if (!r) return `${at}.ranges needs a range for ${cls.id}'s parameter "${param.id}"`;
      if (r[0] < param.min || r[1] > param.max) return `${at}.ranges.${param.id} ${r[0]}..${r[1]} is outside ${cls.id}'s ${param.min}..${param.max}`;
    }
    const extra = Object.keys(ph.ranges).find((k) => !cls.params.some((p) => p.id === k));
    if (extra !== undefined) return `${at}.ranges.${extra} is not a parameter of ${cls.id}`;
    const unitFlows = flowsOf(unitPayloadOf(unit));
    for (const [state, flow] of Object.entries(ph.bodies)) {
      if (flow === undefined) continue;
      const f = unitFlows.find((x) => x.id === flow);
      if (!f) return `${at}.bodies.${state} names flow "${flow}", which unit "${unit.id}" does not have`;
      const entry = f.nodes.find((n) => n.kind === "entry");
      const want = `${ph.em}.${ph.id}`;
      if (!entry || entry.kind !== "entry" || entry.props.on !== "phase" || entry.props.phase !== want || entry.props.state !== state) {
        return `${at}.bodies.${state} names flow "${flow}", whose entry is not "phase ${want} ${state}"`;
      }
      // A flow's entry names one phase state, so it is the body of at
      // most one: no flow can serve two bodies.
    }
  }
  return null;
}

function unitPayloadOf(u: Unit): PayloadModel {
  const [blocks, ...flows] = u.logic;
  return makeModel(u.board, blocks.workspace as WorkspaceJson, u.cms, u.machines, flows);
}

/** Checks a version-4 plant's shape and references; the problem, or null. */
export function plantProblem(v: unknown): string | null {
  if (!isObj(v)) return "not a JSON object";
  if (v.format !== MODEL_FORMAT) return `format must be "${MODEL_FORMAT}"`;
  if (v.version !== PLANT_VERSION) return `version must be ${PLANT_VERSION}`;
  const extra = extraKey(v, ["format", "version", "physical", "procedural", "recipes"]);
  if (extra !== undefined) return `unknown field "${extra}"`;
  if (!isObj(v.physical)) return "physical must be an object";
  const px = extraKey(v.physical, ["units"]);
  if (px !== undefined) return `physical has an unknown field "${px}"`;
  const units = v.physical.units;
  if (!Array.isArray(units) || units.length < 1 || units.length > MAX_UNITS) return `physical.units must be a list of 1 to ${MAX_UNITS} units`;
  for (const [i, u] of units.entries()) {
    const p = unitProblem(u, `physical.units[${i}]`);
    if (p) return p;
  }
  const dup = firstDuplicate((units as Unit[]).map((u) => u.id));
  if (dup >= 0) return `physical.units[${dup}].id repeats an earlier unit's (ignoring case)`;
  if (!isObj(v.procedural)) return "procedural must be an object";
  const qx = extraKey(v.procedural, ["classes", "phases"]);
  if (qx !== undefined) return `procedural has an unknown field "${qx}"`;
  const { classes, phases } = v.procedural;
  if (!Array.isArray(classes) || classes.length > MAX_CLASSES) return `procedural.classes must be a list of at most ${MAX_CLASSES} phase classes`;
  for (const [i, c] of classes.entries()) {
    const p = classProblem(c, `procedural.classes[${i}]`);
    if (p) return p;
  }
  const cdup = firstDuplicate((classes as PhaseClass[]).map((c) => c.id));
  if (cdup >= 0) return `procedural.classes[${cdup}].id repeats an earlier class's (ignoring case)`;
  if (!Array.isArray(phases) || phases.length > MAX_UNITS * MAX_UNIT_PHASES) return `procedural.phases must be a list of at most ${MAX_UNITS * MAX_UNIT_PHASES} phases`;
  for (const [i, ph] of phases.entries()) {
    const p = phaseProblem(ph, `procedural.phases[${i}]`);
    if (p) return p;
  }
  const refs = referencesProblem(units as Unit[], v.procedural as unknown as ProceduralModel);
  if (refs) return refs;
  if (!Array.isArray(v.recipes) || v.recipes.length > MAX_RECIPES) return `recipes must be a list of at most ${MAX_RECIPES} master recipes`;
  for (const [i, r] of v.recipes.entries()) {
    const p = recipeProblem(r, `recipes[${i}]`);
    if (p) return p;
  }
  const rdup = firstDuplicate((v.recipes as MasterRecipe[]).map((r) => r.id));
  if (rdup >= 0) return `recipes[${rdup}].id repeats an earlier recipe's (ignoring case)`;
  return null;
}

function copyUnit(u: Unit): Unit {
  const [blocks, ...flows] = u.logic;
  return {
    ...u,
    cms: { rows: u.cms.rows.map((r) => ({ ...r })), buses: u.cms.buses.map((b) => ({ ...b })) },
    ems: u.ems.map((e) => ({ ...e, cms: [...e.cms] })),
    logic: [blocks, ...copyFlows(flows)],
    machines: copyMachines(u.machines),
  };
}

export type ParsedPlant = { ok: true; plant: PlantModel; migrated: boolean } | { ok: false; error: string };

/** Parses a plant (version 4), or any older payload file payload.ts reads
 * (migrated to a one-unit plant). */
export function parsePlant(text: string, fallback: BoardId, fallbackProfile: ProfileId = "esp-base"): ParsedPlant {
  let value: unknown;
  try {
    value = JSON.parse(text);
  } catch (e) {
    return { ok: false, error: `not JSON: ${e instanceof Error ? e.message : String(e)}` };
  }
  if (isObj(value) && value.format === MODEL_FORMAT && value.version === PLANT_VERSION) {
    const p = plantProblem(value);
    if (p) return { ok: false, error: `not a plant: ${p}` };
    const m = value as unknown as PlantModel;
    return {
      ok: true,
      plant: makePlant(m.physical.units.map(copyUnit), copyProcedural(m.procedural), m.recipes.map(copyRecipe)),
      migrated: false,
    };
  }
  const r = parseModel(text, fallback, fallbackProfile);
  if (!r.ok) return r;
  return { ok: true, plant: plantFromPayload(r.model), migrated: true };
}
