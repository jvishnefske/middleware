// U1: manifest parsing and chip -> image selection (the DOM-free part of the flasher).

import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";
import { UF2_FAMILY, chipKey, fetchManifest, imageForChip, parseManifest, runtimeNote } from "../src/flash/manifest";
import { describePort, webSerialProblem } from "../src/serial/ports";
import { isUsbJtag } from "../src/flash/flasher";

const good = {
  version: "0.1.0",
  images: {
    esp32: { file: "esp32.bin", offset: 0 },
    esp32s3: { file: "esp32s3.bin", offset: 0 },
    esp32c3: { file: "esp32c3.bin", wasm: false },
    esp8266: { file: "esp8266.bin", offset: 0, wasm: false },
  },
};

describe("firmware manifest", () => {
  it("maps esptool-js chip names to manifest keys", () => {
    expect(["ESP32", "ESP32-S3", "ESP32-C3", "ESP8266"].map(chipKey)).toEqual([
      "esp32",
      "esp32s3",
      "esp32c3",
      "esp8266",
    ]);
    const m = parseManifest(good);
    expect(imageForChip(m, "ESP32-C3")).toEqual({ file: "esp32c3.bin", offset: 0, wasm: false });
    expect(imageForChip(m, "ESP8266")).toEqual({ file: "esp8266.bin", offset: 0, wasm: false });
    expect(imageForChip(m, "ESP32-C6")).toBeUndefined();
  });

  it("parses the wasm runtime flag, defaulting to true when absent", () => {
    const m = parseManifest(good);
    expect(m.images.esp32.wasm).toBe(true);
    expect(m.images.esp32s3.wasm).toBe(true);
    expect(m.images.esp8266.wasm).toBe(false);
    expect(runtimeNote(m.images.esp32)).toBeNull();
    expect(runtimeNote(m.images.esp8266)).toMatch(/protocol only/);
    expect(parseManifest({ version: "1", images: { esp32: { file: "a.bin", wasm: true } } }).images.esp32.wasm).toBe(
      true,
    );
    expect(() => parseManifest({ version: "1", images: { esp32: { file: "a.bin", wasm: "no" } } })).toThrow(/wasm/);
    expect(() => parseManifest({ version: "1", images: { esp32: { file: "a.bin", wasm: 0 } } })).toThrow(/boolean/);
  });

  it("U16: parses the ESP Web Tools manifest to the same images, with the BSP", () => {
    const tools = {
      name: "esp-base",
      version: "0.6.0",
      new_install_prompt_erase: false,
      builds: [
        { chipFamily: "ESP32-S3", parts: [{ path: "esp32s3.bin", offset: 0 }], wasm: true, bsp: "esp32s3.bsp.json" },
        { chipFamily: "ESP32-C3", parts: [{ path: "esp32c3.bin", offset: 0 }], wasm: false },
        { chipFamily: "ESP8266", parts: [{ path: "esp8266.bin", offset: 0 }] },
      ],
    };
    const m = parseManifest(tools);
    expect(m.version).toBe("0.6.0");
    expect(imageForChip(m, "ESP32-S3")).toEqual({ file: "esp32s3.bin", offset: 0, wasm: true, bsp: "esp32s3.bsp.json" });
    expect(imageForChip(m, "ESP32-C3")).toEqual({ file: "esp32c3.bin", offset: 0, wasm: false });
    expect(imageForChip(m, "ESP8266")?.wasm).toBe(true);
    const build = (b: object) => ({ version: "1", builds: [{ chipFamily: "ESP32", ...b }] });
    expect(() => parseManifest({ version: "1", builds: {} })).toThrow(/array/);
    expect(() => parseManifest(build({ parts: [] }))).toThrow(/exactly one/);
    expect(() => parseManifest(build({ parts: [{ path: "a.bin" }, { path: "b.bin" }] }))).toThrow(/exactly one/);
    expect(() => parseManifest(build({ parts: [{ path: "https://x/a.bin" }] }))).toThrow(/relative/);
    expect(() => parseManifest(build({ parts: [{ path: "a.bin" }], bsp: "../a.json" }))).toThrow(/bsp/);
    expect(() => parseManifest({ version: "1", builds: [{ parts: [{ path: "a.bin" }] }] })).toThrow(/chipFamily/);
  });

  it("rejects malformed manifests and unsafe file names", () => {
    expect(() => parseManifest(null)).toThrow();
    expect(() => parseManifest({ images: {} })).toThrow(/version/);
    expect(() => parseManifest({ version: "1", images: { esp32: { file: "../x.bin" } } })).toThrow(/relative/);
    expect(() => parseManifest({ version: "1", images: { esp32: { file: "a.bin", offset: -1 } } })).toThrow(/offset/);
  });

  it("tolerates an absent manifest, including an index.html fallback", async () => {
    const notFound = await fetchManifest("firmware/", async () => new Response("", { status: 404 }));
    expect(notFound).toEqual({ ok: false, reason: "no firmware manifest (HTTP 404)" });
    const html = await fetchManifest("firmware/", async () => new Response("<!doctype html>", { status: 200 }));
    expect(html.ok).toBe(false);
    const offline = await fetchManifest("firmware/", async () => {
      throw new Error("offline");
    });
    expect(offline).toEqual({ ok: false, reason: "offline" });
    const ok = await fetchManifest("firmware/", async (url) => {
      expect(url).toBe("firmware/manifest.json");
      return new Response(JSON.stringify(good));
    });
    expect(ok.ok && ok.manifest.version).toBe("0.1.0");
  });
});

describe("serial port helpers", () => {
  it("recognises native USB-Serial-JTAG and bridge chips", () => {
    expect(isUsbJtag({ usbVendorId: 0x303a, usbProductId: 0x1001 })).toBe(true);
    expect(isUsbJtag({ usbVendorId: 0x10c4, usbProductId: 0xea60 })).toBe(false);
    expect(describePort({ usbVendorId: 0x1a86, usbProductId: 0x7523 })).toBe("CH340 bridge (1a86:7523)");
  });

  it("explains when Web Serial is unavailable", () => {
    const nav = {} as Navigator;
    expect(webSerialProblem({ isSecureContext: false, navigator: nav })).toMatch(/secure context/);
    expect(webSerialProblem({ isSecureContext: true, navigator: nav })).toMatch(/Chrome or Edge/);
    expect(webSerialProblem({ isSecureContext: true, navigator: { serial: {} } as unknown as Navigator })).toBeNull();
  });
});

// U19: the rp-base UF2 entries (Start tab download for the Picos).
const fixture = JSON.parse(
  readFileSync(new URL("./fixtures/manifest-uf2.json", import.meta.url), "utf8"),
) as { uf2: Record<string, Record<string, unknown>> };
const rp2040 = fixture.uf2.rp2040!;
const rp2350 = fixture.uf2.rp2350!;
const withUf2 = (uf2: unknown) => ({ version: "1", images: {}, uf2 });

describe("firmware manifest UF2 entries", () => {
  it("parses what firmware/build-images.sh rp2040 rp2350 writes", () => {
    const m = parseManifest(fixture);
    expect(m.images).toEqual({});
    expect(m.uf2.rp2040).toMatchObject({
      file: "rp-base-rp2040.uf2",
      familyId: UF2_FAMILY.rp2040,
      factory: false,
      wasm: false,
    });
    expect(m.uf2.rp2350).toMatchObject({ familyId: UF2_FAMILY.absolute, factory: true, wasm: true });
    expect(m.uf2.rp2350?.file).toMatch(/^rp-base-rp2350-\d+\.\d+-factory\.uf2$/);
    for (const img of Object.values(m.uf2)) {
      expect(img.sha256).toMatch(/^[0-9a-f]{64}$/);
      expect(img.size % 512).toBe(0);
    }
  });

  it("treats an absent uf2 section as none, as in the committed manifest", () => {
    expect(parseManifest(good).uf2).toEqual({});
    const committed = parseManifest(JSON.parse(readFileSync(new URL("../public/firmware/manifest.json", import.meta.url), "utf8")));
    expect(committed.uf2).toEqual({});
  });

  it("rejects an unknown chip and a non-object section", () => {
    expect(() => parseManifest(withUf2({ rp2354: rp2350 }))).toThrow(/manifest\.uf2\.rp2354: unknown chip/);
    expect(() => parseManifest(withUf2({ esp32: rp2040 }))).toThrow(/unknown chip/);
    expect(() => parseManifest(withUf2([rp2040]))).toThrow(/manifest\.uf2 must be an object/);
    expect(() => parseManifest(withUf2({ rp2040: null }))).toThrow(/must be an object/);
  });

  it("rejects a wrong family, including an entry filed under the other chip", () => {
    expect(() => parseManifest(withUf2({ rp2040: { ...rp2040, familyId: UF2_FAMILY.rp2350ArmS } }))).toThrow(
      /rp2040\.familyId must be 0xe48bff56/,
    );
    expect(() => parseManifest(withUf2({ rp2040: { ...rp2040, familyId: "0xe48bff56" } }))).toThrow(/familyId/);
    // The Pico 2 entry under rp2040, and the Pico entry under rp2350.
    expect(() => parseManifest(withUf2({ rp2040: rp2350 }))).toThrow(/familyId/);
    expect(() => parseManifest(withUf2({ rp2350: rp2040 }))).toThrow(/familyId/);
  });

  it("lists only the RP2350 factory image, not the mwctl update image", () => {
    expect(() =>
      parseManifest(withUf2({ rp2350: { ...rp2350, familyId: UF2_FAMILY.rp2350ArmS, factory: false } })),
    ).toThrow(/rp2350\.familyId/);
    expect(() => parseManifest(withUf2({ rp2350: { ...rp2350, factory: false } }))).toThrow(/factory image/);
    const { factory: _, ...noFactory } = rp2350;
    expect(() => parseManifest(withUf2({ rp2350: noFactory }))).toThrow(/factory image/);
    expect(() => parseManifest(withUf2({ rp2040: { ...rp2040, factory: true } }))).toThrow(/no factory image/);
    expect(() => parseManifest(withUf2({ rp2040: { ...rp2040, factory: "no" } }))).toThrow(/boolean/);
  });

  it("rejects a missing or malformed sha256, size, version, wasm flag or file name", () => {
    const { sha256: _s, ...noSha } = rp2040;
    expect(() => parseManifest(withUf2({ rp2040: noSha }))).toThrow(/sha256 must be 64 lower-case hex/);
    expect(() => parseManifest(withUf2({ rp2040: { ...rp2040, sha256: "AB".repeat(32) } }))).toThrow(/sha256/);
    expect(() => parseManifest(withUf2({ rp2040: { ...rp2040, sha256: "ab".repeat(31) } }))).toThrow(/sha256/);
    expect(() => parseManifest(withUf2({ rp2040: { ...rp2040, size: 1000 } }))).toThrow(/multiple of 512/);
    expect(() => parseManifest(withUf2({ rp2040: { ...rp2040, size: 0 } }))).toThrow(/size/);
    expect(() => parseManifest(withUf2({ rp2040: { ...rp2040, version: "" } }))).toThrow(/version/);
    const { wasm: _w, ...noWasm } = rp2040;
    expect(() => parseManifest(withUf2({ rp2040: noWasm }))).toThrow(/wasm must be a boolean/);
    expect(() => parseManifest(withUf2({ rp2040: { ...rp2040, file: "../rp.uf2" } }))).toThrow(/relative \.uf2/);
    expect(() => parseManifest(withUf2({ rp2040: { ...rp2040, file: "rp.bin" } }))).toThrow(/\.uf2/);
  });
});
