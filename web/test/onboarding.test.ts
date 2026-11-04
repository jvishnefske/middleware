// First-run guide (U16): the board list agrees with the bundled firmware
// manifest and the RP firmware, every board gets install -> connect ->
// payload steps that point at the right tabs, the Start tab opens on a
// first visit, and "Try it" asks before replacing other Blocks work.

import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";
import { EXAMPLES, exampleWorkspace } from "../src/blocks/examples";
import { wouldReplaceWork } from "../src/blocks/replace";
import { parseManifest } from "../src/flash/manifest";
import { formatSize } from "../src/flash/uf2";
import {
  BOARDS,
  FIRST_EXAMPLE,
  UF2_INTEGRITY,
  boardById,
  gettingStarted,
  textSpans,
  uf2Detail,
} from "../src/onboarding/boards";
import { makeModel } from "../src/model/payload";
import { boardDefaults, boardSuggestions, checkTable, findRow } from "../src/model/pins";
import { TAB_IDS, initialTab } from "../src/ui/app";
import { protocol } from "./fakeDevice";

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
      for (const x of s) if (x.action?.kind === "tab") expect(TAB_IDS).toContain(x.action.tab);
      expect(s[1]?.action).toMatchObject({ kind: "tab", tab: "device" });
      expect(s[2]?.action).toMatchObject({ kind: "tab", tab: "blocks" });
    }
  });

  it("sends ESP boards to the Flash tab and, without a UF2 in the manifest, Picos to the build hint", () => {
    for (const m of [null, manifest]) {
      for (const b of BOARDS) {
        const [install] = gettingStarted(b, m);
        if (b.install.kind === "web-serial") {
          expect(install?.action, b.id).toMatchObject({ kind: "tab", tab: "flash" });
        } else {
          expect(install?.action, b.id).toBeNull();
          const text = [install?.body, ...(install?.substeps ?? [])].join(" ");
          expect(text).toContain("BOOTSEL");
          expect(text).toContain(b.install.drive);
          expect(text).toContain(`firmware-rp/build.sh ${b.install.target}`);
        }
      }
    }
    // The Pico 2's build hint names the factory UF2, not the update image.
    expect(gettingStarted(boardById("pico2")!)[0]?.body).toContain("rp-base-rp2350-<version>-factory.uf2");
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

// U19: with the UF2s in the manifest (the output of firmware/build-images.sh
// rp2040 rp2350), the Picos get a checked download and the BOOTSEL steps.
const withUf2 = parseManifest(JSON.parse(readFileSync(new URL("./fixtures/manifest-uf2.json", import.meta.url), "utf8")));

describe("Pico UF2 download steps", () => {
  const picos = BOARDS.filter((b) => b.install.kind === "uf2");

  it("offers the manifest's UF2 for each Pico with file name, size and version", () => {
    expect(picos.map((b) => b.id).sort()).toEqual(["pico", "pico2"]);
    for (const b of picos) {
      if (b.install.kind !== "uf2") continue;
      const img = withUf2.uf2[b.install.target]!;
      const [install] = gettingStarted(b, withUf2);
      expect(install?.action, b.id).toEqual({
        kind: "download",
        label: "Download UF2",
        image: img,
        detail: uf2Detail(img),
        integrity: UF2_INTEGRITY,
      });
      expect(uf2Detail(img)).toBe(`${img.file} · ${formatSize(img.size)} · version ${img.version}`);
      expect(install?.body).not.toContain("build.sh");
      // The board's runtime agrees with the image's wasm flag.
      expect(img.wasm, b.id).toBe(b.runsPayloads);
    }
    expect(uf2Detail(withUf2.uf2.rp2350!)).toMatch(/^rp-base-rp2350-0\.1-factory\.uf2 · 1\.3 MB · version 0\.1$/);
  });

  it("says what the download check proves and what it does not", () => {
    expect(UF2_INTEGRITY).toContain("size and SHA-256");
    expect(UF2_INTEGRITY).toContain("not a signature");
  });

  it("follows the download with BOOTSEL onto the board's drive, then drag the file, in order", () => {
    for (const b of picos) {
      if (b.install.kind !== "uf2") continue;
      const img = withUf2.uf2[b.install.target]!;
      const sub = gettingStarted(b, withUf2)[0]?.substeps ?? [];
      expect(sub, b.id).toHaveLength(2);
      expect(sub[0]).toContain("BOOTSEL");
      expect(sub[0]).toContain(`named ${b.install.drive}`);
      expect(sub[1]).toContain(`Drag \`${img.file}\` onto ${b.install.drive}`);
    }
    expect(gettingStarted(boardById("pico")!, withUf2)[0]?.substeps[0]).toContain("RPI-RP2");
    expect(gettingStarted(boardById("pico2")!, withUf2)[0]?.substeps[0]).toContain("RP2350");
  });

  it("marks commands and file names as code spans", () => {
    expect(textSpans("run `mwctl fw update` now")).toEqual([
      ["run ", false],
      ["mwctl fw update", true],
      [" now", false],
    ]);
    expect(textSpans("`a`")).toEqual([["a", true]]);
    expect(textSpans("plain")).toEqual([["plain", false]]);
    expect(textSpans("odd `tail")).toEqual([
      ["odd ", false],
      ["tail", true],
    ]);
    const notes = gettingStarted(boardById("pico2")!, withUf2)[0]?.notes ?? [];
    const code = notes.flatMap((n) => textSpans(n).filter(([, c]) => c).map(([t]) => t));
    expect(code).toEqual(["mwctl fw update", "picotool load --ignore-partitions"]);
  });

  it("tells Pico 2 users the factory image sets up A/B and later updates use mwctl fw update", () => {
    for (const m of [null, withUf2]) {
      const notes = gettingStarted(boardById("pico2")!, m)[0]?.notes.join(" ") ?? "";
      expect(notes).toContain("factory image");
      expect(notes).toContain("A/B partition table");
      expect(notes).toContain("first install");
      expect(notes).toContain("mwctl fw update");
      expect(notes).toContain("picotool load --ignore-partitions");
      // No promise of updates from this site.
      expect(notes).not.toMatch(/this site|the site|browser/i);
    }
    const pico = gettingStarted(boardById("pico")!, withUf2)[0]?.notes.join(" ") ?? "";
    expect(pico).not.toContain("A/B");
    expect(pico).not.toContain("mwctl");
  });

  it("falls back to the build hint for a chip the manifest has no UF2 for", () => {
    const only2040 = { ...withUf2, uf2: { rp2040: withUf2.uf2.rp2040 } };
    expect(gettingStarted(boardById("pico")!, only2040)[0]?.action?.kind).toBe("download");
    expect(gettingStarted(boardById("pico2")!, only2040)[0]?.action).toBeNull();
    expect(gettingStarted(boardById("pico2")!, only2040)[0]?.body).toContain("firmware-rp/build.sh rp2350");
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
    const pins = boardDefaults("pico2");
    const next = makeModel("pico2", blink.workspace, pins);
    expect(wouldReplaceWork(makeModel("pico2", {}), next)).toBe(false);
    expect(wouldReplaceWork(makeModel("pico2", { blocks: { languageVersion: 0, blocks: [] } }), next)).toBe(false);
    expect(wouldReplaceWork(makeModel("pico2", echo.workspace, pins), next)).toBe(true);
    // The same blocks, moved and with new ids, are not work.
    const moved = structuredClone(blink.workspace);
    for (const b of moved.blocks?.blocks ?? []) {
      b.x = (b.x ?? 0) + 100;
      b.id = `moved-${b.id}`;
    }
    expect(wouldReplaceWork(makeModel("pico2", moved, pins), next)).toBe(false);
  });

  it("names the Blink pin per board, and every board's default pin passes its pin rules (Rust checkPins)", () => {
    const proto = protocol();
    for (const b of BOARDS) {
      const issues = checkTable(proto.checkPins, b.id, boardDefaults(b.id));
      expect(issues, b.id).toEqual([]);
      const run = gettingStarted(b).find((s) => s.title.startsWith("Run") || s.title.startsWith("Upload"));
      if (b.runsPayloads) expect(run?.body, b.id).toContain(`GPIO ${b.blink.pin}`);
      if (b.ledPin !== null) expect(b.blink.pin, b.id).toBe(b.ledPin);
    }
    expect(boardDefaults("pico2").rows[0]).toMatchObject({ label: "LED", pin: 25, func: "gpio-out" });
    expect(boardDefaults("esp32").rows[0]).toMatchObject({ label: "LED", pin: 4 });
  });

  it("suggests a BUTTON pin per board (input, pull-up) that passes its pin rules with the LED (Rust checkPins), outside the default table", () => {
    const proto = protocol();
    for (const b of BOARDS) {
      const t = boardSuggestions(b.id);
      expect(findRow(t, "BUTTON"), b.id).toMatchObject({ pin: b.button.pin, func: "gpio-in", pull: "up", note: b.button.note });
      expect(checkTable(proto.checkPins, b.id, t), b.id).toEqual([]);
      expect(findRow(boardDefaults(b.id), "BUTTON"), b.id).toBeUndefined();
    }
    expect(BOARDS.map((b) => [b.id, b.button.pin])).toEqual([
      ["esp32", 0],
      ["esp32s3", 0],
      ["esp32c3", 9],
      ["esp8266", 0],
      ["pico2", 15],
      ["pico", 15],
    ]);
  });
});
