// No silent overwrite of the Blocks workspace (U17): the Examples menu and
// Import JSON load straight away only when nothing would be lost, ask
// first when the workspace holds different blocks, and leave it alone when
// the file is not a workspace.

import { describe, expect, it } from "vitest";
import { EXAMPLES, exampleWorkspace } from "../src/blocks/examples";
import { planImport, wouldReplaceWork } from "../src/blocks/replace";
import { serialize } from "../src/blocks/storage";
import type { WorkspaceJson } from "../src/blocks/workspace";

const blink = exampleWorkspace("blink")!;
const echo = exampleWorkspace("echo")!;
const chase = exampleWorkspace("chase")!;
const empty: WorkspaceJson = {};

describe("Examples menu", () => {
  it("asks before any example replaces a different one, never for itself", () => {
    for (const cur of EXAMPLES) {
      for (const next of EXAMPLES) {
        expect(wouldReplaceWork(structuredClone(cur.workspace), cur.profile, next), `${cur.id} -> ${next.id}`).toBe(cur.id !== next.id);
      }
      expect(wouldReplaceWork(empty, "esp-base", cur), cur.id).toBe(false);
    }
  });
});

describe("Import JSON", () => {
  it("reports a file that is not a workspace and loads nothing", () => {
    const notJson = planImport("{ nope", blink.workspace, "esp-base");
    expect(notJson.kind).toBe("error");
    expect(notJson.kind === "error" && notJson.error).toMatch(/^not JSON/);
    const notWs = planImport(JSON.stringify({ blocks: 3 }), blink.workspace, "esp-base");
    expect(notWs.kind === "error" && notWs.error).toMatch(/^not a Blocks workspace/);
  });

  it("asks before an unrelated JSON object, which parses as an empty workspace, wipes work", () => {
    expect(planImport(JSON.stringify({ name: "package.json" }), blink.workspace, "esp-base").kind).toBe("confirm");
  });

  it("loads straight away over an empty workspace", () => {
    const p = planImport(serialize("badge", chase.workspace), empty, "esp-base");
    expect(p.kind).toBe("load");
    expect(p.kind !== "error" && p.saved.profile).toBe("badge");
  });

  it("asks before an import replaces different blocks", () => {
    const p = planImport(serialize("esp-base", echo.workspace), blink.workspace, "esp-base");
    expect(p.kind).toBe("confirm");
    expect(p.kind !== "error" && p.saved.workspace).toEqual(echo.workspace);
  });

  it("re-importing an export of the same blocks does not ask", () => {
    const moved = structuredClone(blink.workspace);
    for (const b of moved.blocks?.blocks ?? []) b.x = (b.x ?? 0) + 40;
    expect(planImport(serialize("esp-base", moved), blink.workspace, "esp-base").kind).toBe("load");
  });

  it("judges the file under its own profile, and a bare workspace under the current one", () => {
    // The chase blocks under the badge profile differ from the same blocks
    // under esp-base, where badge blocks generate nothing.
    expect(planImport(serialize("badge", chase.workspace), chase.workspace, "esp-base").kind).toBe("confirm");
    const bare = planImport(JSON.stringify(chase.workspace), chase.workspace, "badge");
    expect(bare.kind).toBe("load");
    expect(bare.kind !== "error" && bare.saved.profile).toBe("badge");
  });
});
