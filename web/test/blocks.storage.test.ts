// Blocks persistence: the payload model in localStorage (fault tolerant)
// and the migration of the Blocks tab's earlier save, which keeps the
// workspace and the profile and so the generated code.

import { describe, expect, it } from "vitest";
import { EXAMPLES } from "../src/blocks/examples";
import { generate } from "../src/blocks/generator";
import { type KeyValueStore, LEGACY_KEY, MODEL_KEY, load, save } from "../src/blocks/storage";
import { workspaceProblem } from "../src/blocks/workspace";
import { makeModel, profileOfModel, workspaceOf } from "../src/model/payload";
import { boardDefaults } from "../src/model/pins";

class MapStore implements KeyValueStore {
  readonly map = new Map<string, string>();
  getItem(k: string) {
    return this.map.get(k) ?? null;
  }
  setItem(k: string, v: string) {
    this.map.set(k, v);
  }
}

const throwing: KeyValueStore = {
  getItem() {
    throw new Error("SecurityError");
  },
  setItem() {
    throw new Error("QuotaExceededError");
  },
};

const example = (id: string) => {
  const e = EXAMPLES.find((x) => x.id === id);
  if (!e) throw new Error(`no ${id} example`);
  return e;
};

describe("Blocks persistence", () => {
  it("saves and restores the model with its board and pins", () => {
    const store = new MapStore();
    const m = makeModel("pico2", example("blink").workspace, boardDefaults("pico2"));
    expect(save(store, m)).toBe(true);
    expect(store.map.has(MODEL_KEY)).toBe(true);
    expect(load(store, "esp32")).toEqual({ model: m, migrated: false });
  });

  it("survives storage that throws, is missing or holds garbage", () => {
    expect(save(throwing, makeModel("esp32", {}))).toBe(false);
    expect(load(throwing, "esp32")).toBeNull();
    expect(save(null, makeModel("esp32", {}))).toBe(false);
    expect(load(null, "esp32")).toBeNull();
    const store = new MapStore();
    store.setItem(MODEL_KEY, "{not json");
    expect(load(store, "esp32")).toBeNull();
  });

  it("migrates every legacy save losslessly: same workspace, same profile, same generated code", () => {
    // Legacy saves of each example, including the numeric-pin blocks of the
    // old Blink (GPIO 8).
    const oldBlink = {
      blocks: {
        languageVersion: 0,
        blocks: [
          { type: "mw_on_init", inputs: { DO: { block: { type: "mw_gpio_output", fields: { PIN: 8 } } } } },
          { type: "mw_every", fields: { MS: 500 }, inputs: { DO: { block: { type: "mw_gpio_toggle", fields: { PIN: 8 } } } } },
        ],
      },
    };
    const cases = [
      { profile: "esp-base" as const, workspace: oldBlink },
      { profile: "esp-base" as const, workspace: example("echo").workspace },
      { profile: "badge" as const, workspace: example("chase").workspace },
    ];
    for (const c of cases) {
      const store = new MapStore();
      const legacy = JSON.stringify({ format: "middleware-blocks", version: 1, profile: c.profile, workspace: c.workspace }, null, 2);
      store.setItem(LEGACY_KEY, legacy);
      const r = load(store, "pico2");
      expect(r?.migrated).toBe(true);
      const m = r!.model;
      expect(workspaceOf(m)).toEqual(c.workspace);
      expect(profileOfModel(m)).toBe(c.profile);
      expect(m.board).toBe(c.profile === "badge" ? "badge" : "pico2");
      expect(m.pins).toEqual({ rows: [], buses: [] });
      expect(generate(workspaceOf(m), profileOfModel(m), m.pins).source).toBe(generate(c.workspace, c.profile).source);
      // The legacy entry stays for an older copy of the site.
      expect(store.map.get(LEGACY_KEY)).toBe(legacy);
      // Once saved, the model wins over the legacy entry.
      save(store, m);
      expect(load(store, "esp32")).toEqual({ model: m, migrated: false });
    }
  });

  it("checks workspace shapes before handing them to Blockly", () => {
    expect(workspaceProblem({ blocks: { blocks: [{ id: "x" }] } })).toBe('every block needs a "type"');
    expect(workspaceProblem({ variables: {} })).toBe('"variables" must be an array');
    expect(workspaceProblem({ blocks: { languageVersion: 0, blocks: [] }, variables: [] })).toBeNull();
  });
});
