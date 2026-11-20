// A unit's control points: what a node lists at `sys/manifest/cm` (ABI 8,
// `middleware::loader::control`), the signals a payload binds its machine
// channels to on a board with no payload pins (posix-node on the KD240).
// They sit beside the pin table in the plant (`Unit.points`), and an
// equipment module may own points as it owns pin labels. Pure.
//
// The rules are the device's (`Point::check`): a channel name (1 to 24 of
// `a-z`, `0-9`, `_`), a machine kind, a flow and a unit by name, 32-bit
// `min <= max`, and a 32-bit unsigned `scale`; at most `MAX_POINTS` per
// node. Names are unique, and none may also be a pin label (ignoring case),
// so an equipment module's label names one thing.

import type { ControlPoint } from "../protocol/wasm";
import { extraKey, isInt32, isObj, shortText } from "./procedural";

export type PointKind = "digital" | "position" | "setpoint" | "segments";
export type PointFlow = "in" | "out" | "both";
export type PointUnit = "none" | "bool" | "um" | "count" | "rpm" | "station" | "um/s";

export interface PointRow {
  name: string;
  kind: PointKind;
  flow: PointFlow;
  unit: PointUnit;
  min: number;
  max: number;
  /** Counts per mm for a position point, 0 otherwise. */
  scale: number;
  note: string;
}

/** Points a node lists at most (`control::MAX_POINTS`). */
export const MAX_POINTS = 64;
/** Longest point name (`channels::MAX_NAME`). */
export const MAX_POINT_NAME = 24;

const KINDS: readonly PointKind[] = ["digital", "position", "setpoint", "segments"];
const FLOWS: readonly PointFlow[] = ["in", "out", "both"];
const UNITS: readonly PointUnit[] = ["none", "bool", "um", "count", "rpm", "station", "um/s"];

/** The device's channel-name rule. */
export function isPointName(v: unknown): v is string {
  return typeof v === "string" && /^[a-z0-9_]{1,24}$/.test(v);
}

/** The problem with one point at `at`, or null. */
export function pointProblem(v: unknown, at: string): string | null {
  if (!isObj(v)) return `${at} must be an object`;
  const extra = extraKey(v, ["name", "kind", "flow", "unit", "min", "max", "scale", "note"]);
  if (extra !== undefined) return `${at} has an unknown field "${extra}"`;
  if (!isPointName(v.name)) return `${at}.name must be 1 to ${MAX_POINT_NAME} of a-z, 0-9 and _`;
  if (!KINDS.includes(v.kind as PointKind)) return `${at}.kind must be one of ${KINDS.join(", ")}`;
  if (!FLOWS.includes(v.flow as PointFlow)) return `${at}.flow must be one of ${FLOWS.join(", ")}`;
  if (!UNITS.includes(v.unit as PointUnit)) return `${at}.unit must be one of ${UNITS.join(", ")}`;
  if (!isInt32(v.min) || !isInt32(v.max) || v.min > v.max) {
    return `${at}.min and .max must be whole 32-bit numbers with min <= max`;
  }
  const scale = v.scale;
  if (typeof scale !== "number" || !Number.isInteger(scale) || scale < 0 || scale >= 2 ** 32) {
    return `${at}.scale must be a whole number from 0 to 4294967295`;
  }
  if (!shortText(v.note)) return `${at}.note must be text`;
  return null;
}

/** The problem with a unit's point list at `at`, or null: each point,
 * at most `MAX_POINTS`, unique names, none also one of `pinLabels`
 * (ignoring case). */
export function pointsProblem(v: unknown, at: string, pinLabels: ReadonlySet<string>): string | null {
  if (!Array.isArray(v) || v.length > MAX_POINTS) return `${at} must be a list of at most ${MAX_POINTS} points`;
  const pins = new Set([...pinLabels].map((l) => l.toLowerCase()));
  const seen = new Set<string>();
  for (const [i, p] of v.entries()) {
    const problem = pointProblem(p, `${at}[${i}]`);
    if (problem) return problem;
    const name = (p as PointRow).name;
    if (seen.has(name)) return `${at}[${i}].name repeats an earlier point's`;
    seen.add(name);
    if (pins.has(name)) return `${at}[${i}].name "${name}" is also a pin label (ignoring case)`;
  }
  return null;
}

/** A point row from a decoded `sys/manifest/cm` point. */
export function pointFromControl(p: ControlPoint, note = ""): PointRow {
  return { name: p.name, kind: p.kind, flow: p.flow, unit: p.unit, min: p.min, max: p.max, scale: p.scale, note };
}

/** A copy of a point list. */
export function copyPoints(points: readonly PointRow[]): PointRow[] {
  return points.map((p) => ({ ...p }));
}
