// U1: manifest parsing and chip -> image selection (the DOM-free part of the flasher).

import { describe, expect, it } from "vitest";
import { chipKey, fetchManifest, imageForChip, parseManifest, runtimeNote } from "../src/flash/manifest";
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
