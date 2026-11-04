// The payload model (IR): one versioned document that every editor view
// reads and writes. The Blocks view edits `logic[0]` (a Blockly workspace),
// the Pins view edits `pins`, the States view edits `machines` and the
// Flows view edits the flow logic sources after it. Pure: JSON in, a
// validated model out, and back.
//
// Version 3:
//
//   { format: "middleware-payload", version: 3,
//     board: TargetId,                         // target.ts
//     pins: { rows: PinRow[], buses: BusSetting[] },   // pins.ts
//     logic: [ { kind: "blocks", workspace },          // exactly one, first
//              ...Flow[] ],                    // flows.ts, at most 8
//     machines: Machine[] }                    // machines.ts, at most 4
//
// Version 2 is version 3 with only the Blocks source in `logic` (and no
// raise naming a machine or state-test leaf, which version 3 added to
// machines.ts); version 1 is version 2 with `machines` always empty. Older
// files parse to the same model with `version: 3` (`migrated`), so their
// generated code is unchanged.
//
// `parseModel` is strict about the model's own shape: an unknown key, a
// wrong type, a pin that is not a whole number 0..63 or a version newer
// than this editor is an error naming the place, never silently dropped.
// The Blockly workspace inside is Blockly's own format; it is checked
// with `workspaceProblem` and otherwise kept as it is, so unknown Blockly
// properties round-trip. Whether pins suit the board is not a shape
// question: protocol-wasm `checkPins` answers it (pins.ts).
//
// Older files migrate losslessly: the version-1 Blocks export
// (`{format: "middleware-blocks", version: 1, profile, workspace}`) and a
// bare Blockly workspace become a model with the same workspace, a target
// implied by the profile (`targetForProfile`), and an empty pin table, so
// the generated code is byte-for-byte what it was (tested).

import { type ProfileId, isProfileId } from "../blocks/profiles";
import { type WorkspaceJson, workspaceProblem } from "../blocks/workspace";
import type { BoardId } from "../onboarding/boards";
import { type BusSetting, EMPTY_PINS, PIN_FUNCS, PULLS, type PinRow, type PinTable } from "./pins";
import { type Flow, MAX_FLOWS, copyFlows, flowProblem } from "./flows";
import { type Machine, copyMachines, machinesProblem } from "./machines";
import { type TargetId, isTargetId, profileOf, targetForProfile } from "./target";

export const MODEL_FORMAT = "middleware-payload";
export const MODEL_VERSION = 3;
/** The Blocks tab's export format before the model (still imported). */
export const LEGACY_BLOCKS_FORMAT = "middleware-blocks";

/** Bounds on the model's own lists, so a file cannot make the page do
 * unbounded work. */
export const MAX_ROWS = 64;
export const MAX_BUSES = 8;
export const MAX_TEXT = 200;

export interface BlocksSource {
  kind: "blocks";
  workspace: WorkspaceJson;
}

/** A logic source: the Blocks workspace, or a flow (version 3). */
export type LogicSource = BlocksSource | Flow;

export interface PayloadModel {
  format: typeof MODEL_FORMAT;
  version: typeof MODEL_VERSION;
  board: TargetId;
  pins: PinTable;
  /** The Blocks workspace first, then the flows. */
  logic: readonly [BlocksSource, ...Flow[]];
  /** State-machine tables (machines.ts); none in a version-1 file. */
  machines: readonly Machine[];
}

export function makeModel(
  board: TargetId,
  workspace: WorkspaceJson,
  pins: PinTable = EMPTY_PINS,
  machines: readonly Machine[] = [],
  flows: readonly Flow[] = [],
): PayloadModel {
  return { format: MODEL_FORMAT, version: MODEL_VERSION, board, pins, logic: [{ kind: "blocks", workspace }, ...flows], machines };
}

export function workspaceOf(m: PayloadModel): WorkspaceJson {
  return m.logic[0].workspace;
}

export function profileOfModel(m: PayloadModel): ProfileId {
  return profileOf(m.board);
}

/** The flows (the logic sources after the Blocks workspace). */
export function flowsOf(m: PayloadModel): readonly Flow[] {
  const [, ...flows] = m.logic;
  return flows;
}

/** A copy with the workspace replaced (the flows kept). */
export function withWorkspace(m: PayloadModel, workspace: WorkspaceJson): PayloadModel {
  return { ...m, logic: [{ kind: "blocks", workspace }, ...flowsOf(m)] };
}

/** A copy with the flows replaced (at most `MAX_FLOWS` kept). */
export function withFlows(m: PayloadModel, flows: readonly Flow[]): PayloadModel {
  return { ...m, logic: [m.logic[0], ...flows.slice(0, MAX_FLOWS)] };
}

export function withPins(m: PayloadModel, pins: PinTable): PayloadModel {
  return { ...m, pins };
}

export function withMachines(m: PayloadModel, machines: readonly Machine[]): PayloadModel {
  return { ...m, machines };
}

export function withBoard(m: PayloadModel, board: TargetId): PayloadModel {
  return { ...m, board };
}

export function serializeModel(m: PayloadModel): string {
  return JSON.stringify(m, null, 2);
}

export type Parsed = { ok: true; model: PayloadModel; migrated: boolean } | { ok: false; error: string };

type Obj = Record<string, unknown>;

function isObj(v: unknown): v is Obj {
  return v !== null && typeof v === "object" && !Array.isArray(v);
}

/** The first key of `o` not in `allowed`, if any. */
function extraKey(o: Obj, allowed: readonly string[]): string | undefined {
  return Object.keys(o).find((k) => !allowed.includes(k));
}

function isWhole(v: unknown, lo: number, hi: number): v is number {
  return typeof v === "number" && Number.isInteger(v) && v >= lo && v <= hi;
}

function shortText(v: unknown): v is string {
  return typeof v === "string" && v.length <= MAX_TEXT;
}

const ROW_KEYS = ["label", "pin", "func", "bus", "pull", "initial", "note"] as const;

function rowProblem(v: unknown, at: string): string | null {
  if (!isObj(v)) return `${at} must be an object`;
  const extra = extraKey(v, ROW_KEYS);
  if (extra !== undefined) return `${at} has an unknown field "${extra}"`;
  if (!shortText(v.label)) return `${at}.label must be text (at most ${MAX_TEXT} characters)`;
  if (v.pin !== null && !isWhole(v.pin, 0, 63)) return `${at}.pin must be a whole number from 0 to 63, or null`;
  if (!(PIN_FUNCS as readonly unknown[]).includes(v.func)) return `${at}.func must be one of ${PIN_FUNCS.join(", ")}`;
  if (!isWhole(v.bus, 0, 7)) return `${at}.bus must be a whole number from 0 to 7`;
  if (!(PULLS as readonly unknown[]).includes(v.pull)) return `${at}.pull must be one of ${PULLS.join(", ")}`;
  if (v.initial !== 0 && v.initial !== 1) return `${at}.initial must be 0 or 1`;
  if (!shortText(v.note)) return `${at}.note must be text (at most ${MAX_TEXT} characters)`;
  return null;
}

function busProblem(v: unknown, at: string): string | null {
  if (!isObj(v)) return `${at} must be an object`;
  if (v.kind === "i2c") {
    const extra = extraKey(v, ["kind", "bus", "hz"]);
    if (extra !== undefined) return `${at} has an unknown field "${extra}"`;
    if (!isWhole(v.bus, 0, 7) || !isWhole(v.hz, 0, 0x7fffffff)) return `${at} needs whole-number bus and hz`;
    return null;
  }
  if (v.kind === "spi") {
    const extra = extraKey(v, ["kind", "bus", "khz", "mode"]);
    if (extra !== undefined) return `${at} has an unknown field "${extra}"`;
    if (!isWhole(v.bus, 0, 7) || !isWhole(v.khz, 0, 0x7fffffff) || !isWhole(v.mode, 0, 0x7fffffff)) {
      return `${at} needs whole-number bus, khz and mode`;
    }
    return null;
  }
  return `${at}.kind must be "i2c" or "spi"`;
}

/** Checks a version-1, -2 or -3 model's shape; returns the problem, or
 * null. */
export function modelProblem(v: unknown): string | null {
  if (!isObj(v)) return "not a JSON object";
  if (v.format !== MODEL_FORMAT) return `format must be "${MODEL_FORMAT}"`;
  if (typeof v.version !== "number" || !Number.isInteger(v.version) || v.version < 1) return "version must be a whole number";
  if (v.version > MODEL_VERSION) return `version ${v.version} was written by a newer editor; this one reads version ${MODEL_VERSION}`;
  const extra = extraKey(v, ["format", "version", "board", "pins", "logic", "machines"]);
  if (extra !== undefined) return `unknown field "${extra}"`;
  if (!isTargetId(v.board)) return `board "${String(v.board)}" is not a known board`;
  if (!isObj(v.pins)) return "pins must be an object";
  const pinsExtra = extraKey(v.pins, ["rows", "buses"]);
  if (pinsExtra !== undefined) return `pins has an unknown field "${pinsExtra}"`;
  const { rows, buses } = v.pins;
  if (!Array.isArray(rows) || rows.length > MAX_ROWS) return `pins.rows must be a list of at most ${MAX_ROWS} rows`;
  for (const [i, r] of rows.entries()) {
    const p = rowProblem(r, `pins.rows[${i}]`);
    if (p) return p;
  }
  if (!Array.isArray(buses) || buses.length > MAX_BUSES) return `pins.buses must be a list of at most ${MAX_BUSES} settings`;
  for (const [i, b] of buses.entries()) {
    const p = busProblem(b, `pins.buses[${i}]`);
    if (p) return p;
  }
  const maxLogic = v.version >= 3 ? 1 + MAX_FLOWS : 1;
  if (!Array.isArray(v.logic) || v.logic.length < 1 || v.logic.length > maxLogic) {
    return v.version >= 3
      ? `logic must hold one Blocks workspace and at most ${MAX_FLOWS} flows`
      : "logic must hold exactly one Blocks workspace";
  }
  const src: unknown = v.logic[0];
  if (!isObj(src) || src.kind !== "blocks") return 'logic[0].kind must be "blocks" (other logic sources need a newer editor)';
  const srcExtra = extraKey(src, ["kind", "workspace"]);
  if (srcExtra !== undefined) return `logic[0] has an unknown field "${srcExtra}"`;
  const ws = workspaceProblem(src.workspace);
  if (ws) return `logic[0].workspace is not a Blocks workspace: ${ws}`;
  for (const [i, f] of v.logic.slice(1).entries()) {
    const p = flowProblem(f, `logic[${i + 1}]`);
    if (p) return p;
  }
  if (v.version === 1) {
    if (!Array.isArray(v.machines) || v.machines.length !== 0) return "machines must be empty in version 1";
    return null;
  }
  return machinesProblem(v.machines);
}

/** The model from a legacy Blocks export or a bare workspace: same
 * workspace, the profile's target, no pins. */
export function migrateBlocks(profile: ProfileId, workspace: WorkspaceJson, fallback: BoardId): PayloadModel {
  return makeModel(targetForProfile(profile, fallback), workspace, EMPTY_PINS);
}

/** Parses a saved or exported file: a model, a legacy Blocks export, or a
 * bare Blockly workspace. `fallback` is the board a legacy file without
 * one gets (and `fallbackProfile` the profile a bare workspace gets). */
export function parseModel(text: string, fallback: BoardId, fallbackProfile: ProfileId = "esp-base"): Parsed {
  let value: unknown;
  try {
    value = JSON.parse(text);
  } catch (e) {
    return { ok: false, error: `not JSON: ${e instanceof Error ? e.message : String(e)}` };
  }
  if (isObj(value) && value.format === MODEL_FORMAT) {
    const p = modelProblem(value);
    if (p) return { ok: false, error: `not a payload model: ${p}` };
    // Shape checked above; copy the lists so later edits never alias the input.
    const m = value as unknown as PayloadModel;
    return {
      ok: true,
      model: {
        ...m,
        version: MODEL_VERSION,
        pins: { rows: m.pins.rows.map((r: PinRow) => ({ ...r })), buses: m.pins.buses.map((b: BusSetting) => ({ ...b })) },
        logic: [m.logic[0], ...copyFlows(flowsOf(m))],
        machines: copyMachines(m.machines),
      },
      migrated: (value.version as number) < MODEL_VERSION,
    };
  }
  const legacy = isObj(value) && value.format === LEGACY_BLOCKS_FORMAT;
  const workspace = legacy ? (value as Obj).workspace : value;
  const problem = workspaceProblem(workspace);
  if (problem) return { ok: false, error: `not a Blocks workspace: ${problem}` };
  const profile = legacy && isProfileId((value as Obj).profile) ? ((value as Obj).profile as ProfileId) : fallbackProfile;
  return { ok: true, model: migrateBlocks(profile, workspace as WorkspaceJson, fallback), migrated: true };
}
