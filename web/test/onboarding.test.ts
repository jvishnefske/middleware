// First-run guide (U16): the board list agrees with the bundled firmware
// manifest and the RP firmware, every board gets install -> connect ->
// payload steps that point at the right tabs, the Start tab opens on a
// first visit, and "Try it" asks before replacing other Blocks work.

import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";
import { EXAMPLES, exampleWorkspace, wouldReplaceWork } from "../src/blocks/examples";
import { parseManifest } from "../src/flash/manifest";
import { BOARDS, FIRST_EXAMPLE, boardById, gettingStarted } from "../src/onboarding/boards";
import { TAB_IDS, initialTab } from "../src/ui/app";

const manifest = parseManifest(JSON.parse(readFileSync(new URL("../public/firmware/manifest.json", import.meta.url), "utf8")));

describe("boards", () => {
  it("covers every bundled ESP image with the manifest's runtime flag", () => {
    const esp = BOARDS.filter((b) => b.install.kind === "web-serial");
    expect(esp.map((b) => b.chip).sort()).toEqual(Object.keys(manifest.images).sort());
    for (const b of esp) expect(b.runsPayloads, b.id).toBe(manifest.images[b.chip]?.wasm);
  });

  it("installs the Picos from UF2, with only the Pico 2 running payloads", () => {
    expect(boardById("pico2")).toMatchObject({ chip: "rp2350", runsPayloads: true, ledPin: 25 });
    expect(boardById("pico2")?.install).toEqual({ kind: "uf2", target: "rp2350", drive: "RP2350" });
    expect(boardById("pico")).toMatchObject({ chip: "rp2040", runsPayloads: false, ledPin: 25 });
    expect(boardById("pico")?.install).toEqual({ kind: "uf2", target: "rp2040", drive: "RPI-RP2" });
    expect(boardById("nope")).toBeUndefined();
    expect(boardById(null)).toBeUndefined();
  });

  it("gives every board install, connect and payload steps that point at real tabs", () => {
    for (const b of BOARDS) {
      const s = gettingStarted(b);
      expect(s.map((x) => x.title), b.id).toEqual([
        "Install the base firmware (once)",
        "Connect",
        b.runsPayloads ? "Run a payload" : "Upload a payload",
      ]);
      for (const x of s) if (x.action) expect(TAB_IDS).toContain(x.action.tab);
      expect(s[1]?.action?.tab).toBe("device");
      expect(s[2]?.action?.tab).toBe("blocks");
    }
  });

  it("sends ESP boards to the Flash tab and Picos to BOOTSEL and the UF2", () => {
    for (const b of BOARDS) {
      const [install] = gettingStarted(b);
      if (b.install.kind === "web-serial") {
        expect(install?.action?.tab, b.id).toBe("flash");
      } else {
        expect(install?.action, b.id).toBeNull();
        expect(install?.body).toContain("BOOTSEL");
        expect(install?.body).toContain(b.install.drive);
        expect(install?.body).toContain(`firmware-rp/build.sh ${b.install.target}`);
      }
    }
  });

  it("says protocol-only boards store payloads without running them", () => {
    for (const b of BOARDS.filter((x) => !x.runsPayloads)) {
      const body = gettingStarted(b)[2]?.body ?? "";
      expect(body, b.id).toContain("does not run them");
      expect(body).not.toContain("started");
    }
    expect(gettingStarted(boardById("pico2")!)[2]?.body).toContain("GPIO 25");
  });
});

describe("first run", () => {
  it("opens Start without a valid saved tab and the saved tab otherwise", () => {
    expect(initialTab(null)).toBe("start");
    expect(initialTab(undefined)).toBe("start");
    expect(initialTab("garbage")).toBe("start");
    expect(initialTab("blocks")).toBe("blocks");
    expect(initialTab("flash")).toBe("flash");
  });

  it("tries an example that exists", () => {
    expect(EXAMPLES.map((e) => e.id)).toContain(FIRST_EXAMPLE);
  });

  it("asks before replacing blocks that differ from the example, not otherwise", () => {
    const blink = exampleWorkspace("blink")!;
    const echo = exampleWorkspace("echo")!;
    expect(wouldReplaceWork({}, "esp-base", blink)).toBe(false);
    expect(wouldReplaceWork({ blocks: { languageVersion: 0, blocks: [] } }, "esp-base", blink)).toBe(false);
    expect(wouldReplaceWork(echo.workspace, "esp-base", blink)).toBe(true);
    // The same blocks, moved and with new ids, are not work.
    const moved = structuredClone(blink.workspace);
    for (const b of moved.blocks?.blocks ?? []) {
      b.x = (b.x ?? 0) + 100;
      b.id = `moved-${b.id}`;
    }
    expect(wouldReplaceWork(moved, "esp-base", blink)).toBe(false);
  });
});
