// Blocks editor persistence: localStorage (fault tolerant) and the JSON
// export / import format.

import { describe, expect, it } from "vitest";
import { EXAMPLES } from "../src/blocks/examples";
import { type KeyValueStore, STORAGE_KEY, load, parseSaved, save, serialize } from "../src/blocks/storage";
import { workspaceProblem } from "../src/blocks/workspace";

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

describe("Blocks persistence", () => {
  it("saves and restores the workspace with its profile", () => {
    const store = new MapStore();
    const chase = EXAMPLES.find((e) => e.id === "chase");
    if (!chase) throw new Error("no chase example");
    expect(save(store, "badge", chase.workspace)).toBe(true);
    expect(store.map.has(STORAGE_KEY)).toBe(true);
    expect(load(store, "esp-base")).toEqual({ format: "middleware-blocks", version: 1, profile: "badge", workspace: chase.workspace });
  });

  it("survives storage that throws, is missing or holds garbage", () => {
    expect(save(throwing, "esp-base", {})).toBe(false);
    expect(load(throwing, "esp-base")).toBeNull();
    expect(save(null, "esp-base", {})).toBe(false);
    expect(load(null, "esp-base")).toBeNull();
    const store = new MapStore();
    store.setItem(STORAGE_KEY, "{not json");
    expect(load(store, "esp-base")).toBeNull();
  });

  it("round-trips the export format and imports bare Blockly workspaces", () => {
    const echo = EXAMPLES.find((e) => e.id === "echo");
    if (!echo) throw new Error("no echo example");
    const text = serialize("esp-base", echo.workspace);
    expect(parseSaved(text, "badge")).toEqual({ ok: true, saved: { format: "middleware-blocks", version: 1, profile: "esp-base", workspace: echo.workspace } });
    const bare = parseSaved(JSON.stringify(echo.workspace), "badge");
    expect(bare.ok && bare.saved.profile).toBe("badge");
    const unknownProfile = parseSaved(JSON.stringify({ format: "middleware-blocks", profile: "toaster", workspace: {} }), "esp-base");
    expect(unknownProfile.ok && unknownProfile.saved.profile).toBe("esp-base");
  });

  it("rejects files that are not workspaces", () => {
    expect(parseSaved("nope", "esp-base")).toMatchObject({ ok: false, error: expect.stringMatching(/^not JSON/) });
    expect(parseSaved("[1]", "esp-base")).toEqual({ ok: false, error: "not a Blocks workspace: not a JSON object" });
    expect(workspaceProblem({ blocks: { blocks: [{ id: "x" }] } })).toBe('every block needs a "type"');
    expect(workspaceProblem({ variables: {} })).toBe('"variables" must be an array');
    expect(workspaceProblem({ blocks: { languageVersion: 0, blocks: [] }, variables: [] })).toBeNull();
  });
});
