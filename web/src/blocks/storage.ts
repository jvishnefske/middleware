// Persistence for the Blocks tab: the plant (model/plant.ts, IR v4) in
// localStorage under `PLANT_KEY`, every access in try/catch since storage
// may be missing, full or blocked. `loadPlant` falls back to the version
// 1-3 payload model under `MODEL_KEY` (and the legacy save below),
// migrated to a one-unit plant; `savePlant` also keeps `MODEL_KEY`
// current while the plant is a plain payload (one unit, no modules,
// phases or recipes), so an older copy of the site still finds it.
//
// The model lives under `MODEL_KEY`. A browser that only has the Blocks
// tab's earlier save (`LEGACY_KEY`: `{format: "middleware-blocks",
// version: 1, profile, workspace}`) gets it migrated on load (same
// workspace, the profile's board, no pins, so the generated code is what
// it was); the legacy entry is left in place, so an older copy of the site
// still finds its workspace.

import type { BoardId } from "../onboarding/boards";
import { type PayloadModel, parseModel, serializeModel } from "../model/payload";
import { type PlantModel, parsePlant, plainPayloadOf, plantFromPayload, serializePlant } from "../model/plant";

export const PLANT_KEY = "payload.plant";
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

export function savePlant(store: KeyValueStore | null, plant: PlantModel): boolean {
  try {
    store?.setItem(PLANT_KEY, serializePlant(plant));
    const plain = plainPayloadOf(plant);
    if (plain) store?.setItem(MODEL_KEY, serializeModel(plain));
    return store !== null;
  } catch {
    return false;
  }
}

/** The saved plant, else the saved (or legacy) payload as a one-unit
 * plant (`migrated`), else null. */
export function loadPlant(store: KeyValueStore | null, fallback: BoardId): { plant: PlantModel; migrated: boolean } | null {
  try {
    const text = store?.getItem(PLANT_KEY);
    if (text) {
      const r = parsePlant(text, fallback);
      if (r.ok) return { plant: r.plant, migrated: r.migrated };
    }
  } catch {
    return null;
  }
  const old = load(store, fallback);
  return old ? { plant: plantFromPayload(old.model), migrated: true } : null;
}

/** `window.localStorage`, or null where reading the property throws. */
export function browserStore(): KeyValueStore | null {
  try {
    return typeof localStorage === "undefined" ? null : localStorage;
  } catch {
    return null;
  }
}
