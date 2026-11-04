// Persistence for the Blocks tab: the payload model (model/payload.ts) in
// localStorage, every access in try/catch since storage may be missing,
// full or blocked.
//
// The model lives under `MODEL_KEY`. A browser that only has the Blocks
// tab's earlier save (`LEGACY_KEY`: `{format: "middleware-blocks",
// version: 1, profile, workspace}`) gets it migrated on load (same
// workspace, the profile's board, no pins, so the generated code is what
// it was); the legacy entry is left in place, so an older copy of the site
// still finds its workspace.

import type { BoardId } from "../onboarding/boards";
import { type PayloadModel, parseModel, serializeModel } from "../model/payload";

export const MODEL_KEY = "payload.model";
/** Where the Blocks tab saved before the payload model. */
export const LEGACY_KEY = "blocks.workspace";

/** The subset of `Storage` used, so tests can pass a map. */
export interface KeyValueStore {
  getItem(key: string): string | null;
  setItem(key: string, value: string): void;
}

export function save(store: KeyValueStore | null, model: PayloadModel): boolean {
  try {
    store?.setItem(MODEL_KEY, serializeModel(model));
    return store !== null;
  } catch {
    return false;
  }
}

/** The saved model, the migrated legacy save, or null. `fallback` is the
 * board a legacy save of the esp-base profile gets. */
export function load(store: KeyValueStore | null, fallback: BoardId): { model: PayloadModel; migrated: boolean } | null {
  for (const key of [MODEL_KEY, LEGACY_KEY]) {
    try {
      const text = store?.getItem(key);
      if (!text) continue;
      const r = parseModel(text, fallback);
      if (r.ok) return { model: r.model, migrated: r.migrated };
    } catch {
      return null;
    }
  }
  return null;
}

/** `window.localStorage`, or null where reading the property throws. */
export function browserStore(): KeyValueStore | null {
  try {
    return typeof localStorage === "undefined" ? null : localStorage;
  } catch {
    return null;
  }
}
