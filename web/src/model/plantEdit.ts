// Pure edits of the plant document (plant.ts) for the editor's Units and
// Phases views, and the plain-language issues those views show. Every
// edit returns a new plant and keeps references consistent: renaming a
// unit, an equipment module, a phase class or a parameter renames what
// names it (phases, recipe steps, ranges); removing one removes what
// depends on it (a unit's phases, a class's phases). The Blocks, Pins,
// States and Flows views edit one unit's payload (`unitPayload`), written
// back with `withUnitPayload`, which leaves every other unit untouched.
//
// `plantIssues` says, per row of the views' tables, what plantProblem
// would refuse, in words a user can act on ("Equipment module LED owns
// pin BUTTON, which ADC already owns."), so a plant being edited can be
// wrong for a while without losing work; it is saved as it stands.

import { type Flow, type FlowNode, newFlow } from "./flows";
import { type PayloadModel, flowsOf } from "./payload";
import { type EquipmentModule, MAX_EMS, MAX_UNITS, type PlantModel, type Unit, makePlant } from "./plant";
import { boardDefaults } from "./pins";
import { ACTING_STATES, type ActingState, type EquipmentPhase, MAX_CLASSES, MAX_PARAMS, MAX_UNIT_PHASES, type ParamDef, type PhaseClass, isIdent } from "./procedural";
import type { TargetId } from "./target";

const lower = (s: string) => s.toLowerCase();

function free(taken: readonly string[], base: string): string {
  const t = new Set(taken.map(lower));
  if (!t.has(lower(base))) return base;
  for (let i = 2; i < 100; i++) if (!t.has(lower(`${base}${i}`))) return `${base}${i}`;
  return `${base}_${taken.length}`;
}

function withUnits(p: PlantModel, units: readonly Unit[]): PlantModel {
  return makePlant(units, p.procedural, p.recipes);
}

function mapUnit(p: PlantModel, id: string, f: (u: Unit) => Unit): PlantModel {
  return withUnits(
    p,
    p.physical.units.map((u) => (u.id === id ? f(u) : u)),
  );
}

function withPhases(p: PlantModel, phases: readonly EquipmentPhase[]): PlantModel {
  return makePlant(p.physical.units, { ...p.procedural, phases }, p.recipes);
}

function withClasses(p: PlantModel, classes: readonly PhaseClass[], phases = p.procedural.phases): PlantModel {
  return makePlant(p.physical.units, { classes, phases }, p.recipes);
}

// ---- units

/** Unit `id`'s payload replaced by `m` (board, pins, logic, machines). */
export function withUnitPayload(p: PlantModel, id: string, m: PayloadModel): PlantModel {
  return mapUnit(p, id, (u) => ({ ...u, board: m.board, cms: m.pins, logic: m.logic, machines: m.machines }));
}

/** A new, empty unit on `board` (unchanged at the unit limit). */
export function addUnit(p: PlantModel, board: TargetId): PlantModel {
  if (p.physical.units.length >= MAX_UNITS) return p;
  const id = free(
    p.physical.units.map((u) => u.id),
    "unit",
  );
  const unit: Unit = { id, name: "", class: "Unit", node: null, board, cms: boardDefaults(board), ems: [], logic: [{ kind: "blocks", workspace: {} }], machines: [] };
  return withUnits(p, [...p.physical.units, unit]);
}

/** Unit `id` and its phases removed (the last unit stays). */
export function removeUnit(p: PlantModel, id: string): PlantModel {
  if (p.physical.units.length <= 1) return p;
  const units = p.physical.units.filter((u) => u.id !== id);
  return makePlant(units, { ...p.procedural, phases: p.procedural.phases.filter((ph) => ph.unit !== id) }, p.recipes);
}

export type UnitField = "id" | "name" | "class" | "board";

/** One field of unit `id` set; a new id is carried to its phases. */
export function setUnit(p: PlantModel, id: string, field: UnitField, value: string): PlantModel {
  const next = mapUnit(p, id, (u) => ({ ...u, [field]: value }) as Unit);
  if (field !== "id") return next;
  return withPhases(
    next,
    next.procedural.phases.map((ph) => (ph.unit === id ? { ...ph, unit: value } : ph)),
  );
}

/** Unit `id` bound to node `node` (16 lowercase hex digits) or unbound. */
export function bindNode(p: PlantModel, id: string, node: string | null): PlantModel {
  return mapUnit(p, id, (u) => ({ ...u, node }));
}

// ---- equipment modules

export function addEm(p: PlantModel, unit: string): PlantModel {
  return mapUnit(p, unit, (u) =>
    u.ems.length >= MAX_EMS
      ? u
      : {
          ...u,
          ems: [
            ...u.ems,
            {
              id: free(
                u.ems.map((e) => e.id),
                "EM",
              ),
              name: "",
              cms: [],
            },
          ],
        },
  );
}

/** Module `i` of the unit removed, with its phases. */
export function removeEm(p: PlantModel, unit: string, i: number): PlantModel {
  const em = p.physical.units.find((u) => u.id === unit)?.ems[i];
  if (!em) return p;
  const next = mapUnit(p, unit, (u) => ({ ...u, ems: u.ems.filter((_, k) => k !== i) }));
  return withPhases(
    next,
    next.procedural.phases.filter((ph) => !(ph.unit === unit && ph.em === em.id)),
  );
}

/** Module `i`'s id, name or control modules (pin labels) set; a new id is
 * carried to its phases. */
export function setEm(p: PlantModel, unit: string, i: number, patch: Partial<EquipmentModule>): PlantModel {
  const old = p.physical.units.find((u) => u.id === unit)?.ems[i];
  if (!old) return p;
  const next = mapUnit(p, unit, (u) => ({ ...u, ems: u.ems.map((e, k) => (k === i ? { ...e, ...patch } : e)) }));
  if (patch.id === undefined || patch.id === old.id) return next;
  const id = patch.id;
  return withPhases(
    next,
    next.procedural.phases.map((ph) => (ph.unit === unit && ph.em === old.id ? { ...ph, em: id } : ph)),
  );
}

/** Pin label `from` renamed to `to` in unit `unit`'s modules (the Pins
 * view renamed a control module). */
export function renameCm(p: PlantModel, unit: string, from: string, to: string): PlantModel {
  return mapUnit(p, unit, (u) => ({ ...u, ems: u.ems.map((e) => ({ ...e, cms: e.cms.map((l) => (l === from ? to : l)) })) }));
}

// ---- phase classes and their parameters

export function addClass(p: PlantModel): PlantModel {
  if (p.procedural.classes.length >= MAX_CLASSES) return p;
  const id = free(
    p.procedural.classes.map((c) => c.id),
    "Class",
  );
  return withClasses(p, [...p.procedural.classes, { id, name: "", params: [] }]);
}

/** Class `i` removed, with the phases implementing it. */
export function removeClass(p: PlantModel, i: number): PlantModel {
  const c = p.procedural.classes[i];
  if (!c) return p;
  return withClasses(
    p,
    p.procedural.classes.filter((_, k) => k !== i),
    p.procedural.phases.filter((ph) => ph.class !== c.id),
  );
}

/** Class `i`'s id or name set; a new id is carried to its phases and the
 * recipes' steps. */
export function setClass(p: PlantModel, i: number, patch: { id?: string; name?: string }): PlantModel {
  const old = p.procedural.classes[i];
  if (!old) return p;
  const classes = p.procedural.classes.map((c, k) => (k === i ? { ...c, ...patch } : c));
  if (patch.id === undefined || patch.id === old.id) return withClasses(p, classes);
  const id = patch.id;
  const phases = p.procedural.phases.map((ph) => (ph.class === old.id ? { ...ph, class: id } : ph));
  const recipes = p.recipes.map((r) => ({ ...r, steps: r.steps.map((s) => (s.class === old.id ? { ...s, class: id } : s)) }));
  return makePlant(p.physical.units, { classes, phases }, recipes);
}

/** A parameter added to class `i`, with a range in each of its phases. */
export function addParam(p: PlantModel, i: number): PlantModel {
  const c = p.procedural.classes[i];
  if (!c || c.params.length >= MAX_PARAMS) return p;
  const param: ParamDef = {
    id: free(
      c.params.map((x) => x.id),
      "param",
    ),
    name: "",
    min: 0,
    max: 100,
    unit: "",
  };
  const classes = p.procedural.classes.map((x, k) => (k === i ? { ...x, params: [...x.params, param] } : x));
  const phases = p.procedural.phases.map((ph) => (ph.class === c.id ? { ...ph, ranges: { ...ph.ranges, [param.id]: [param.min, param.max] as const } } : ph));
  return withClasses(p, classes, phases);
}

/** Parameter `j` of class `i` removed, with its ranges and the recipes'
 * values for it. */
export function removeParam(p: PlantModel, i: number, j: number): PlantModel {
  const c = p.procedural.classes[i];
  const param = c?.params[j];
  if (!c || !param) return p;
  const classes = p.procedural.classes.map((x, k) => (k === i ? { ...x, params: x.params.filter((_, n) => n !== j) } : x));
  const phases = p.procedural.phases.map((ph) => {
    if (ph.class !== c.id) return ph;
    const { [param.id]: _drop, ...ranges } = ph.ranges;
    return { ...ph, ranges };
  });
  const recipes = p.recipes.map((r) => ({
    ...r,
    steps: r.steps.map((s) => {
      if (s.class !== c.id) return s;
      const { [param.id]: _drop, ...params } = s.params;
      return { ...s, params };
    }),
  }));
  return makePlant(p.physical.units, { classes, phases }, recipes);
}

/** Parameter `j` of class `i` changed; a new id renames its ranges and
 * the recipes' values. */
export function setParam(p: PlantModel, i: number, j: number, patch: Partial<ParamDef>): PlantModel {
  const c = p.procedural.classes[i];
  const old = c?.params[j];
  if (!c || !old) return p;
  const classes = p.procedural.classes.map((x, k) => (k === i ? { ...x, params: x.params.map((q, n) => (n === j ? { ...q, ...patch } : q)) } : x));
  if (patch.id === undefined || patch.id === old.id) return withClasses(p, classes);
  const id = patch.id;
  const rename = <T>(o: Readonly<Record<string, T>>): Record<string, T> => Object.fromEntries(Object.entries(o).map(([k, v]) => [k === old.id ? id : k, v]));
  const phases = p.procedural.phases.map((ph) => (ph.class === c.id ? { ...ph, ranges: rename(ph.ranges) } : ph));
  const recipes = p.recipes.map((r) => ({ ...r, steps: r.steps.map((s) => (s.class === c.id ? { ...s, params: rename(s.params) } : s)) }));
  return makePlant(p.physical.units, { classes, phases }, recipes);
}

// ---- equipment phases

/** Indices (into the plant's phases) of unit `unit`'s phases. */
export function unitPhaseIndices(p: PlantModel, unit: string): number[] {
  return p.procedural.phases.flatMap((ph, i) => (ph.unit === unit ? [i] : []));
}

/** A new phase on module `em` of unit `unit`, of the first class, its
 * ranges the class's. */
export function addPhase(p: PlantModel, unit: string, em: string): PlantModel {
  const mine = p.procedural.phases.filter((ph) => ph.unit === unit);
  if (mine.length >= MAX_UNIT_PHASES) return p;
  const cls = p.procedural.classes[0];
  const id = free(
    mine.filter((ph) => ph.em === em).map((ph) => ph.id),
    cls?.id ?? "Phase",
  );
  const ph: EquipmentPhase = {
    unit,
    em,
    id,
    class: cls?.id ?? "",
    ranges: Object.fromEntries((cls?.params ?? []).map((x) => [x.id, [x.min, x.max] as const])),
    bodies: {},
  };
  return withPhases(p, [...p.procedural.phases, ph]);
}

export function removePhase(p: PlantModel, i: number): PlantModel {
  return withPhases(
    p,
    p.procedural.phases.filter((_, k) => k !== i),
  );
}

/** Phase `i`'s module, id or class set. A new class resets the ranges to
 * its own; a new module or id carries the phase's body flows along (their
 * entries and phase actions follow the name). */
export function setPhase(p: PlantModel, i: number, patch: { em?: string; id?: string; class?: string }): PlantModel {
  const old = p.procedural.phases[i];
  if (!old) return p;
  let ph: EquipmentPhase = { ...old, ...patch };
  if (patch.class !== undefined && patch.class !== old.class) {
    const cls = p.procedural.classes.find((c) => c.id === patch.class);
    ph = { ...ph, ranges: Object.fromEntries((cls?.params ?? []).map((x) => [x.id, [x.min, x.max] as const])) };
  }
  let next = withPhases(
    p,
    p.procedural.phases.map((x, k) => (k === i ? ph : x)),
  );
  const from = `${old.em}.${old.id}`;
  const to = `${ph.em}.${ph.id}`;
  if (from !== to) next = mapUnit(next, old.unit, (u) => ({ ...u, logic: [u.logic[0], ...flowsOfUnit(u).map((f) => renamePhaseInFlow(f, from, to))] }));
  return next;
}

/** Phase `i`'s range for parameter `param`. */
export function setRange(p: PlantModel, i: number, param: string, range: readonly [number, number]): PlantModel {
  return withPhases(
    p,
    p.procedural.phases.map((ph, k) => (k === i ? { ...ph, ranges: { ...ph.ranges, [param]: [range[0], range[1]] as const } } : ph)),
  );
}

/** Phase `i`'s body for acting `state`: a flow id, or none. */
export function setBody(p: PlantModel, i: number, state: ActingState, flow: string | null): PlantModel {
  return withPhases(
    p,
    p.procedural.phases.map((ph, k) => {
      if (k !== i) return ph;
      const bodies = { ...ph.bodies };
      if (flow === null) delete bodies[state];
      else bodies[state] = flow;
      return { ...ph, bodies };
    }),
  );
}

function flowsOfUnit(u: Unit): Flow[] {
  const [, ...flows] = u.logic;
  return [...flows];
}

function renamePhaseInFlow(f: Flow, from: string, to: string): Flow {
  return {
    ...f,
    nodes: f.nodes.map((n): FlowNode => {
      if (n.kind === "entry" && n.props.on === "phase" && n.props.phase === from) return { ...n, props: { ...n.props, phase: to } };
      if (n.kind === "action" && (n.props.kind === "phase-done" || n.props.kind === "phase-report") && n.props.phase === from) return { ...n, props: { ...n.props, phase: to } };
      return n;
    }),
  };
}

/** A new body flow for phase `i` in `state` (entry `phase <em>.<phase>
 * <state>`, then `phase done`), added to the unit's flows and set as the
 * body; also the new flow's id. Unchanged (null id) at the flow limit. */
export function newBodyFlow(p: PlantModel, i: number, state: ActingState): { plant: PlantModel; flow: string | null } {
  const ph = p.procedural.phases[i];
  const unit = ph ? p.physical.units.find((u) => u.id === ph.unit) : undefined;
  if (!ph || !unit) return { plant: p, flow: null };
  const flows = flowsOfUnit(unit);
  if (flows.length >= 8) return { plant: p, flow: null };
  const base = newFlow(flows);
  const id = free(
    flows.map((f) => f.id),
    `${ph.id}${state[0]?.toUpperCase() ?? ""}${state.slice(1)}`,
  );
  const phase = `${ph.em}.${ph.id}`;
  const f: Flow = {
    ...base,
    id,
    name: `${phase} ${state}`,
    nodes: [
      { id: "n1", kind: "entry", x: 40, y: 40, props: { on: "phase", phase, state } },
      { id: "n2", kind: "action", x: 40, y: 160, props: { kind: "phase-done", phase } },
    ],
    wires: [{ from: { node: "n1", port: "next" }, to: { node: "n2", port: "in" } }],
  };
  const next = mapUnit(p, unit.id, (u) => ({ ...u, logic: [u.logic[0], ...flows, f] }));
  return { plant: setBody(next, i, state, id), flow: id };
}

// ---- plain-language issues

export type PlantPlace = "units" | "ems" | "classes" | "params" | "phases" | "bodies";

export interface PlantIssue {
  place: PlantPlace;
  /** Row in that table (units: unit index; ems: module index of the
   * selected unit; classes: class index; params: [class, param] flattened
   * by the view; phases / bodies: index into the plant's phases). */
  row: number;
  /** For params: the parameter index within its class. */
  sub?: number;
  /** For ems: the unit whose module it is. */
  unit?: string;
  message: string;
}

const ID_RULE = "starts with a letter and has only letters, digits and _ (at most 24)";

/** What the Units and Phases views flag, by place; empty when
 * plantProblem would accept the plant's physical and procedural parts. */
export function plantIssues(p: PlantModel): PlantIssue[] {
  const out: PlantIssue[] = [];
  const units = p.physical.units;
  const seenUnit = new Map<string, number>();
  units.forEach((u, r) => {
    const say = (message: string) => out.push({ place: "units", row: r, message });
    if (!/^[A-Za-z][A-Za-z0-9_-]{0,23}$/.test(u.id)) say(u.id === "" ? "Give this unit an id, for example pico-led." : `The unit id "${u.id}" must start with a letter and use only letters, digits, _ or - (at most 24).`);
    const k = lower(u.id);
    const other = seenUnit.get(k);
    if (other !== undefined) say(`Unit ${other + 1} already has the id ${u.id}; unit ids must be unique (ignoring case).`);
    else seenUnit.set(k, r);
    if (!isIdent(u.class)) say(`The unit class "${u.class}" ${ID_RULE}; recipes ask for units by class.`);
    if (u.node !== null && !/^[0-9a-f]{16}$/.test(u.node)) say(`The node id "${u.node}" must be 16 lowercase hex digits, as the device reports it.`);
  });
  const nodes = new Map<string, number>();
  units.forEach((u, r) => {
    if (u.node === null) return;
    const o = nodes.get(u.node);
    if (o !== undefined) out.push({ place: "units", row: r, message: `Unit ${units[o]?.id} is bound to the same device; a device runs one unit.` });
    else nodes.set(u.node, r);
  });
  const classes = p.procedural.classes;
  const seenClass = new Map<string, number>();
  classes.forEach((c, r) => {
    const say = (message: string) => out.push({ place: "classes", row: r, message });
    if (!isIdent(c.id)) say(c.id === "" ? "Give this class an id, for example Blink." : `The class id "${c.id}" ${ID_RULE}.`);
    const o = seenClass.get(lower(c.id));
    if (o !== undefined) say(`Class ${o + 1} already has the id ${c.id}; class ids must be unique (ignoring case).`);
    else seenClass.set(lower(c.id), r);
    const seenParam = new Set<string>();
    c.params.forEach((x, j) => {
      const sayP = (message: string) => out.push({ place: "params", row: r, sub: j, message });
      if (!isIdent(x.id)) sayP(`The parameter id "${x.id}" ${ID_RULE}.`);
      if (seenParam.has(lower(x.id))) sayP(`${c.id} already has a parameter ${x.id}.`);
      seenParam.add(lower(x.id));
      if (!Number.isInteger(x.min) || !Number.isInteger(x.max)) sayP("The minimum and maximum must be whole numbers.");
      else if (x.min > x.max) sayP(`The minimum ${x.min} is above the maximum ${x.max}.`);
    });
  });
  const seenPhase = new Map<string, number>();
  p.procedural.phases.forEach((ph, r) => {
    const say = (message: string) => out.push({ place: "phases", row: r, message });
    const unit = units.find((u) => u.id === ph.unit);
    if (!unit) {
      say(`This phase belongs to unit "${ph.unit}", which the plant does not have.`);
      return;
    }
    if (!unit.ems.some((e) => e.id === ph.em)) say(`Unit ${unit.id} has no equipment module ${ph.em}; choose one or add it.`);
    if (!isIdent(ph.id)) say(ph.id === "" ? "Give this phase an id, for example Blink." : `The phase id "${ph.id}" ${ID_RULE}.`);
    const key = lower(`${ph.unit}/${ph.em}.${ph.id}`);
    const o = seenPhase.get(key);
    if (o !== undefined) say(`Phase ${o + 1} is also ${ph.em}.${ph.id} on this unit; a module's phases need different ids.`);
    else seenPhase.set(key, r);
    const cls = classes.find((c) => c.id === ph.class);
    if (!cls) say(ph.class === "" ? "Choose the phase class this phase implements." : `There is no phase class ${ph.class}; choose one or add it.`);
    for (const x of cls?.params ?? []) {
      const range = ph.ranges[x.id];
      if (!range) say(`Give a range for ${x.id}.`);
      else if (range[0] > range[1]) say(`The range of ${x.id} has its minimum ${range[0]} above its maximum ${range[1]}.`);
      else if (range[0] < x.min || range[1] > x.max) say(`The range of ${x.id}, ${range[0]} to ${range[1]}, must lie within ${cls?.id}'s ${x.min} to ${x.max}.`);
    }
    const flows = flowsOfUnit(unit);
    for (const st of ACTING_STATES) {
      const id = ph.bodies[st];
      if (id === undefined) continue;
      const sayB = (message: string) => out.push({ place: "bodies", row: r, message });
      const f = flows.find((x) => x.id === id);
      if (!f) {
        sayB(`The ${st} body is flow ${id}, which unit ${unit.id} does not have.`);
        continue;
      }
      const e = f.nodes.find((n) => n.kind === "entry");
      if (!e || e.kind !== "entry" || e.props.on !== "phase" || e.props.phase !== `${ph.em}.${ph.id}` || e.props.state !== st) {
        sayB(`Flow ${id} is the ${st} body, so its entry must be "phase ${ph.em}.${ph.id} ${st}"; change its entry in Flows or pick another flow.`);
      }
    }
  });
  for (const u of units) {
    const labels = new Set(u.cms.rows.map((r) => r.label));
    const owner = new Map<string, string>();
    const seenEm = new Set<string>();
    u.ems.forEach((em, r) => {
      const say = (message: string) => out.push({ place: "ems", row: r, unit: u.id, message });
      if (!isIdent(em.id)) say(em.id === "" ? "Give this equipment module an id, for example LED." : `The module id "${em.id}" ${ID_RULE}.`);
      if (seenEm.has(lower(em.id))) say(`Another module already has the id ${em.id}.`);
      seenEm.add(lower(em.id));
      for (const l of em.cms) {
        if (!labels.has(l)) say(`Pin ${l} is not in the unit's pin table; add it under Pins or remove it here.`);
        const o = owner.get(l);
        if (o !== undefined) say(`Pin ${l} already belongs to module ${o}; a pin belongs to one module.`);
        else owner.set(l, em.id);
      }
    });
  }
  return out;
}

/** The issues of one unit's modules (the Phases view shows one unit). */
export function emIssues(issues: readonly PlantIssue[], unit: string): PlantIssue[] {
  return issues.filter((i) => i.place === "ems" && i.unit === unit);
}

/** Which unit flows can be a phase state's body: the unit's flows, by id. */
export function bodyChoices(p: PlantModel, unit: string): string[] {
  const u = p.physical.units.find((x) => x.id === unit);
  return u ? flowsOf({ format: "middleware-payload", version: 3, board: u.board, pins: u.cms, logic: u.logic, machines: u.machines } as PayloadModel).map((f) => f.id) : [];
}

/** A node id (16 lowercase hex digits) from what `sys/info` reported,
 * or null when it reported none or something else. */
export function nodeFromInfo(node: string | undefined): string | null {
  if (node === undefined) return null;
  const n = node.trim().toLowerCase();
  return /^[0-9a-f]{16}$/.test(n) ? n : null;
}
