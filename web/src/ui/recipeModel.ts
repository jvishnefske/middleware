// The Recipe view's decisions, pure (recipeView.ts only renders them and
// forwards events): the chart's geometry (one lane per unit procedure,
// steps in their lane at their row, transitions as bars between the steps
// they join, double lines for parallel divergence and convergence), how
// check issues map onto steps, bars and lanes, what a fix button says,
// the allocation panel's rows in plain language, and the live classes of
// a run (step status, phase state).

import type { Allocation, MasterRecipe, RecipeStep, RecipeTransition } from "../model/recipe";
import type { PhaseClass, RecipeFix, RecipeIssue } from "../model/procedural";
import type { StepStatus } from "../protocol/wasm";
import { type Point, wirePath } from "./flowsModel";

// ---- geometry (canvas units; the SVG scales them by the zoom)

export const LANE_W = 220;
export const LANE_HEAD = 48;
export const STEP_W = 184;
export const STEP_H = 64;
export const PAD = 12;
export const BAR_H = 6;

/** The left edge of lane `i`. */
export function laneX(i: number): number {
  return PAD + i * LANE_W;
}

/** The lane under canvas x (clamped to the lanes there are). */
export function laneAt(x: number, lanes: number): number {
  return Math.max(0, Math.min(lanes - 1, Math.floor((x - PAD) / LANE_W)));
}

export interface StepBox {
  step: RecipeStep;
  lane: number;
  x: number;
  y: number;
}

/** Where each step is drawn: its unit procedure's lane (a step whose
 * procedure is missing goes in the first) at its row. */
export function stepBoxes(r: MasterRecipe): StepBox[] {
  return r.steps.map((step) => {
    const lane = Math.max(0, r.unitProcedures.findIndex((u) => u.id === step.unitProcedure));
    return { step, lane, x: laneX(lane) + (LANE_W - STEP_W) / 2, y: LANE_HEAD + step.y };
  });
}

export interface BarGeom {
  t: RecipeTransition;
  index: number;
  x1: number;
  x2: number;
  y: number;
  /** Lines from each `from` step down to the bar, and from the bar to
   * each `to` step. */
  wires: { d: string; step: string; side: "from" | "to" }[];
  /** A double line above (convergence) or below (divergence) the bar. */
  join: { x1: number; x2: number; y: number } | null;
  fork: { x1: number; x2: number; y: number } | null;
}

const center = (b: StepBox): number => b.x + STEP_W / 2;

/** A transition's bar: below its lowest `from` step (or above its
 * highest `to` step when it has none), spanning every step it joins. */
export function barGeom(r: MasterRecipe, boxes: readonly StepBox[], index: number): BarGeom | null {
  const t = r.transitions[index];
  if (!t) return null;
  const byId = new Map(boxes.map((b) => [b.step.id, b] as const));
  const from = t.from.map((id) => byId.get(id)).filter((b): b is StepBox => !!b);
  const to = t.to.map((id) => byId.get(id)).filter((b): b is StepBox => !!b);
  const all = [...from, ...to];
  if (all.length === 0) return null;
  const xs = all.map(center);
  const y = from.length > 0 ? Math.max(...from.map((b) => b.y + STEP_H)) + 28 : Math.min(...to.map((b) => b.y)) - 28;
  const x1 = Math.min(...xs) - 34;
  const x2 = Math.max(...xs) + 34;
  const wires = [
    ...from.map((b) => ({ d: wirePath({ x: center(b), y: b.y + STEP_H }, { x: center(b), y: y - (from.length > 1 ? 10 : 0) }), step: b.step.id, side: "from" as const })),
    ...to.map((b) => ({ d: wirePath({ x: center(b), y: y + (to.length > 1 ? 10 : 0) }, { x: center(b), y: b.y }), step: b.step.id, side: "to" as const })),
  ];
  const span = (bs: StepBox[]) => ({ x1: Math.min(...bs.map(center)) - 30, x2: Math.max(...bs.map(center)) + 30 });
  return {
    t,
    index,
    x1,
    x2,
    y,
    wires,
    join: from.length > 1 ? { ...span(from), y: y - 10 } : null,
    fork: to.length > 1 ? { ...span(to), y: y + 10 } : null,
  };
}

/** The drawing's size: every lane and step plus room to drag. */
export function chartSize(r: MasterRecipe): { w: number; h: number } {
  const lanes = Math.max(1, r.unitProcedures.length);
  const bottom = r.steps.reduce((y, s) => Math.max(y, LANE_HEAD + s.y + STEP_H), LANE_HEAD);
  return { w: Math.max(480, laneX(lanes) + PAD), h: Math.max(320, bottom + 160) };
}

/** A step's bottom port (drag from it onto another step to connect). */
export function stepPort(b: StepBox): Point {
  return { x: center(b), y: b.y + STEP_H };
}

// ---- what a step says

/** `Blink period_ms=250 times=3`, parameters in class order (others
 * after them). */
export function argsText(s: RecipeStep, classes: readonly PhaseClass[]): string {
  const c = classes.find((x) => x.id === s.class);
  const order = [...(c?.params.map((p) => p.id) ?? []), ...Object.keys(s.params).filter((k) => !c?.params.some((p) => p.id === k))];
  const args = order.filter((k) => s.params[k] !== undefined).map((k) => `${k}=${s.params[k]}`);
  return [s.class || "(no class)", ...args].join(" ");
}

/** A parameter's range for the reader: `10..10000 ms`. */
export function rangeText(p: { min: number; max: number; unit: string }): string {
  return `${p.min}..${p.max}${p.unit ? ` ${p.unit}` : ""}`;
}

export function stepLabel(s: RecipeStep, classes: readonly PhaseClass[], status?: StepStatus, phase?: string | null): string {
  const run = status ? `, ${status}${phase ? `, phase ${phase}` : ""}` : "";
  return `Step ${s.id}${s.name ? ` (${s.name})` : ""}: ${argsText(s, classes)}, in ${s.unitProcedure}${run}`;
}

// ---- issues

export function issuesAt(issues: readonly RecipeIssue[], at: RecipeIssue["at"], index: number): RecipeIssue[] {
  return issues.filter((i) => i.at === at && i.index === index);
}

/** The issues of step `index`'s argument `param` (argument indices are
 * the step's `params` key order, as protocol-wasm reads them). */
export function argIssues(issues: readonly RecipeIssue[], s: RecipeStep, index: number, param: string): RecipeIssue[] {
  const k = Object.keys(s.params).indexOf(param);
  return issues.filter((i) => i.at === "step" && i.index === index && ((i.sub !== null && i.sub === k) || (i.fix?.kind === "setArg" && i.fix.param === param)));
}

export function severityOf(list: readonly RecipeIssue[]): "error" | "warning" | null {
  if (list.some((i) => i.severity === "error")) return "error";
  return list.length > 0 ? "warning" : null;
}

export function recipeTabText(issues: readonly RecipeIssue[]): string {
  const n = issues.filter((i) => i.severity === "error").length;
  return n ? `Recipe (${n} to fix)` : "Recipe";
}

export function fixText(f: RecipeFix, r: MasterRecipe): string {
  switch (f.kind) {
    case "setArg":
      return `Set ${f.param} to ${f.value}`;
    case "removeArg":
      return `Remove ${f.param}`;
    case "removeTransition":
      return `Remove transition ${r.transitions[f.transition]?.id ?? f.transition + 1}`;
    case "removeFromTransition":
      return `Take ${f.step} off transition ${r.transitions[f.transition]?.id ?? f.transition + 1}`;
    case "removeUnitProc":
      return `Remove unit procedure ${r.unitProcedures[f.unitProc]?.id ?? ""}`;
  }
}

// ---- allocation

export interface AllocRow {
  unit: string;
  /** What the unit runs, in words. */
  text: string;
  ok: boolean;
}

/** The allocation as one row per unit: the unit procedure and steps it
 * runs ("Signal: BlinkLed on LED.Blink"), why it cannot run the procedure
 * that failed, or that it is not needed. */
export function allocationRows(a: Allocation, r: MasterRecipe, units: readonly string[]): AllocRow[] {
  if (!a.ok) {
    return units.map((u) => {
      const why = a.reasons.find((x) => x.unit === u);
      return { unit: u, text: why ? `cannot run ${a.unitProcedure ?? "the procedure"}: ${why.reason}` : "not needed for that procedure", ok: !why };
    });
  }
  return units.map((u) => {
    const mine = a.bindings.filter((b) => b.unit === u);
    if (mine.length === 0) return { unit: u, text: "not used by this recipe", ok: true };
    const up = r.steps.find((s) => s.id === mine[0]!.step)?.unitProcedure ?? "";
    return { unit: u, text: `${up}: ${mine.map((b) => `${b.step} on ${b.em}.${b.phase}`).join(", ")}`, ok: true };
  });
}

// ---- a run, live

/** The CSS class of a step's status on the canvas and in the outline. */
export function statusClass(s: StepStatus | undefined): string {
  return s ? `st-${s}` : "";
}

/** A short change signature of a run (the view re-renders only when it
 * changes). */
export function runSignature(state: string, steps: readonly { status: string }[], phases: readonly { state: string | null; values: readonly number[] }[], trace: number): string {
  return [state, steps.map((s) => s.status).join(","), phases.map((p) => `${p.state}:${p.values.join("/")}`).join(","), trace].join("|");
}
