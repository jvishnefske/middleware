// Guards against silently overwriting the payload. Everything that
// replaces it wholesale (the Start tab's "Try it", the Examples menu and
// Import JSON) first asks whether that would lose work; the Blocks tab
// asks the user to confirm only then. Pure, so the decisions are tested
// without Blockly or a DOM.

import type { BoardId } from "../onboarding/boards";
import { type PayloadModel, parseModel, profileOfModel, withBoard, withPins, withWorkspace, workspaceOf } from "../model/payload";
import { seedForTarget, withNeeds } from "../model/pins";
import type { TargetId } from "../model/target";
import type { Example } from "./examples";
import { generate } from "./generator";
import { topBlocks } from "./workspace";

function source(m: PayloadModel): string {
  return generate(workspaceOf(m), profileOfModel(m), m.pins).source;
}

/** Whether replacing `current` by `next` would lose work: true when the
 * current payload has blocks or pins and generates different code (block
 * ids and positions do not count, so a moved copy of the same blocks is
 * not work). */
export function wouldReplaceWork(current: PayloadModel, next: PayloadModel): boolean {
  if (topBlocks(workspaceOf(current)).length === 0 && current.pins.rows.length === 0) return false;
  return source(current) !== source(next);
}

/** The payload after loading example `e` over `current`: the example's
 * blocks, the badge for a badge example (else the current board, or
 * `fallback` coming from the badge), and the current pin table with the
 * pins the example needs added (none removed or changed). */
export function exampleModel(e: Example, current: PayloadModel, fallback: BoardId): PayloadModel {
  const board: TargetId = e.profile === "badge" ? "badge" : current.board === "badge" ? fallback : current.board;
  const pins = withNeeds(seedForTarget(current.pins, current.board, board), board, e.needs);
  return withPins(withBoard(withWorkspace(current, structuredClone(e.workspace)), board), pins);
}

/** What Import JSON does with a file: report why it is not a payload,
 * load it straight away, or load it only after the user confirms. */
export type ImportPlan = { kind: "error"; error: string } | { kind: "load" | "confirm"; model: PayloadModel };

/** Decides an import of `text` over `current`: a payload model, a Blocks
 * export from before the model, or a bare Blockly workspace (which takes
 * the current profile and board, and keeps the current pins). */
export function planImport(text: string, current: PayloadModel, fallback: BoardId): ImportPlan {
  const profile = profileOfModel(current);
  const board = current.board === "badge" ? fallback : current.board;
  const r = parseModel(text, board, profile);
  if (!r.ok) return { kind: "error", error: r.error };
  let model = r.model;
  // A bare workspace carries neither board nor pins: keep the current ones.
  const bare = (() => {
    try {
      const v = JSON.parse(text) as { format?: unknown };
      return v === null || typeof v !== "object" || v.format === undefined;
    } catch {
      return false;
    }
  })();
  if (bare) model = withPins(withBoard(model, current.board), current.pins);
  return { kind: wouldReplaceWork(current, model) ? "confirm" : "load", model };
}
