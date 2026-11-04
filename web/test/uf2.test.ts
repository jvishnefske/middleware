// U19: a downloaded UF2 is checked against its manifest entry (size,
// SHA-256, UF2 magic and family) before the Start tab saves it.

import { createHash } from "node:crypto";
import { describe, expect, it } from "vitest";
import { UF2_FAMILY, type Uf2Image } from "../src/flash/manifest";
import { checkUf2, formatSize } from "../src/flash/uf2";

/** `n` UF2 blocks of `family` with the magic words rp_base::ota::uf2 writes. */
function uf2(n: number, family: number): Uint8Array {
  const out = new Uint8Array(n * 512);
  const v = new DataView(out.buffer);
  for (let i = 0; i < n; i++) {
    const o = i * 512;
    const words = [0x0a324655, 0x9e5d5157, 0x2000, 0x10000000 + i * 256, 256, i, n, family];
    words.forEach((w, k) => v.setUint32(o + 4 * k, w, true));
    out.fill(i & 0xff, o + 32, o + 32 + 256);
    v.setUint32(o + 508, 0x0ab16f30, true);
  }
  return out;
}

const sha = (b: Uint8Array) => createHash("sha256").update(b).digest("hex");

function entry(bytes: Uint8Array, over: Partial<Uf2Image> = {}): Uf2Image {
  return {
    file: "rp-base-rp2040.uf2",
    sha256: sha(bytes),
    size: bytes.length,
    version: "0.1.0",
    familyId: UF2_FAMILY.rp2040,
    factory: false,
    wasm: false,
    ...over,
  };
}

describe("UF2 download check", () => {
  it("accepts the file the manifest names, with WebCrypto's SHA-256", async () => {
    const b = uf2(3, UF2_FAMILY.rp2040);
    expect(await checkUf2(b, entry(b))).toEqual({ ok: true });
    const f = uf2(2, UF2_FAMILY.absolute);
    expect(await checkUf2(f, entry(f, { familyId: UF2_FAMILY.absolute, factory: true }))).toEqual({ ok: true });
  });

  it("refuses a truncated or padded download", async () => {
    const b = uf2(3, UF2_FAMILY.rp2040);
    const r = await checkUf2(b.subarray(0, 1024), entry(b));
    expect(r).toEqual({ ok: false, reason: "rp-base-rp2040.uf2 is 1024 bytes, the manifest says 1536" });
  });

  it("refuses a file whose SHA-256 differs from the manifest", async () => {
    const b = uf2(3, UF2_FAMILY.rp2040);
    const e = entry(b);
    b[100] ^= 1;
    const r = await checkUf2(b, e);
    expect(r.ok).toBe(false);
    expect(!r.ok && r.reason).toMatch(/SHA-256 [0-9a-f]{64}, the manifest says/);
  });

  it("refuses something that is not a UF2, such as the index.html a server answers for a missing file", async () => {
    const b = uf2(3, UF2_FAMILY.rp2040);
    const html = new TextEncoder().encode("<!doctype html>".padEnd(1756, " "));
    expect(await checkUf2(html, entry(b))).toEqual({ ok: false, reason: "rp-base-rp2040.uf2 is not a UF2 file" });
    const tiny = new TextEncoder().encode("<!doctype html>");
    expect(await checkUf2(tiny, entry(b))).toEqual({ ok: false, reason: "rp-base-rp2040.uf2 is not a UF2 file" });
  });

  it("refuses a UF2 of another family, such as the RP2350 update image", async () => {
    const b = uf2(2, UF2_FAMILY.rp2350ArmS);
    const r = await checkUf2(b, entry(b, { familyId: UF2_FAMILY.absolute }));
    expect(r).toEqual({ ok: false, reason: "rp-base-rp2040.uf2 has UF2 family 0xe48bff59, expected 0xe48bff57" });
  });

  it("formats sizes for people", () => {
    expect(formatSize(512)).toBe("512 B");
    expect(formatSize(189952)).toBe("190 KB");
    expect(formatSize(1272832)).toBe("1.3 MB");
  });
});
