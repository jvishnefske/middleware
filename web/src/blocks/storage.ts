// Persistence for the Blocks tab: the workspace and the board profile in
// localStorage (every access in try/catch, since storage may be missing,
// full or blocked), and the JSON file format for export and import.

import { type ProfileId, isProfileId } from "./profiles";
import { type WorkspaceJson, workspaceProblem } from "./workspace";

export const STORAGE_KEY = "blocks.workspace";

/** What an exported file holds: the profile and the Blockly workspace. */
export interface SavedBlocks {
  format: "middleware-blocks";
  version: 1;
  profile: ProfileId;
  workspace: WorkspaceJson;
}

/** The subset of `Storage` used, so tests can pass a map. */
export interface KeyValueStore {
  getItem(key: string): string | null;
  setItem(key: string, value: string): void;
}

export function serialize(profile: ProfileId, workspace: WorkspaceJson): string {
  const saved: SavedBlocks = { format: "middleware-blocks", version: 1, profile, workspace };
  return JSON.stringify(saved, null, 2);
}

/** Parses an exported file, or a bare Blockly workspace (profile then
 * defaults to `fallback`). */
export function parseSaved(text: string, fallback: ProfileId): { ok: true; saved: SavedBlocks } | { ok: false; error: string } {
  let value: unknown;
  try {
    value = JSON.parse(text);
  } catch (e) {
    return { ok: false, error: `not JSON: ${e instanceof Error ? e.message : String(e)}` };
  }
  const v = value as Partial<SavedBlocks> | null;
  const wrapped = v !== null && typeof v === "object" && v.format === "middleware-blocks";
  const workspace = wrapped ? v.workspace : value;
  const problem = workspaceProblem(workspace);
  if (problem) return { ok: false, error: `not a Blocks workspace: ${problem}` };
  const profile = wrapped && isProfileId(v.profile) ? v.profile : fallback;
  return { ok: true, saved: { format: "middleware-blocks", version: 1, profile, workspace: workspace as WorkspaceJson } };
}

export function save(store: KeyValueStore | null, profile: ProfileId, workspace: WorkspaceJson): boolean {
  try {
    store?.setItem(STORAGE_KEY, serialize(profile, workspace));
    return store !== null;
  } catch {
    return false;
  }
}

export function load(store: KeyValueStore | null, fallback: ProfileId): SavedBlocks | null {
  try {
    const text = store?.getItem(STORAGE_KEY);
    if (!text) return null;
    const r = parseSaved(text, fallback);
    return r.ok ? r.saved : null;
  } catch {
    return null;
  }
}

/** `window.localStorage`, or null where reading the property throws. */
export function browserStore(): KeyValueStore | null {
  try {
    return typeof localStorage === "undefined" ? null : localStorage;
  } catch {
    return null;
  }
}
