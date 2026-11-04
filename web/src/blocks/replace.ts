// Guards against silently overwriting the Blocks workspace. Everything that
// replaces it wholesale (the Start tab's "Try it", the Examples menu and
// Import JSON) first asks whether that would lose work; the Blocks tab
// asks the user to confirm only then. Pure, so the decisions are tested
// without Blockly or a DOM.

import { generate } from "./generator";
import type { ProfileId } from "./profiles";
import { type SavedBlocks, parseSaved } from "./storage";
import type { WorkspaceJson } from "./workspace";

/** What would replace the workspace: blocks and the profile they are for. */
export interface Replacement {
  profile: ProfileId;
  workspace: WorkspaceJson;
}

/** Whether loading `next` over the workspace would lose work: true when
 * the workspace has blocks and generates different code (block ids and
 * positions do not count, so a moved copy of the same blocks is not
 * work). */
export function wouldReplaceWork(current: WorkspaceJson, profile: ProfileId, next: Replacement): boolean {
  if ((current.blocks?.blocks?.length ?? 0) === 0) return false;
  return generate(current, profile).source !== generate(next.workspace, next.profile).source;
}

/** What Import JSON does with a file: report why it is not a workspace,
 * load it straight away, or load it only after the user confirms. */
export type ImportPlan = { kind: "error"; error: string } | { kind: "load" | "confirm"; saved: SavedBlocks };

/** Decides an import of `text` over the workspace (a bare Blockly
 * workspace takes the current profile). */
export function planImport(text: string, current: WorkspaceJson, profile: ProfileId): ImportPlan {
  const r = parseSaved(text, profile);
  if (!r.ok) return { kind: "error", error: r.error };
  return { kind: wouldReplaceWork(current, profile, r.saved) ? "confirm" : "load", saved: r.saved };
}
