// Checks a downloaded rp-base UF2 against its manifest entry before the
// page hands it to the visitor (Start tab, Pico boards): the first block's
// UF2 magic, the byte count, the first block's family id and the SHA-256.
// The size is a multiple of 512 by the manifest's own validation.
//
// What this proves: the file is the one the manifest names, not a
// truncated, corrupted or stale (cached) copy, and it is a UF2 of the
// family the board's boot ROM will accept for that install. What it does
// not prove: the manifest and the file come from the same site, so a
// compromised site could serve a matching pair. It is not a signature.

import type { Uf2Image } from "./manifest";

export const UF2_BLOCK = 512;
const MAGIC_START0 = 0x0a324655;
const MAGIC_START1 = 0x9e5d5157;
const MAGIC_END = 0x0ab16f30;
const FLAG_FAMILY_ID = 0x2000;

export type Uf2Check = { ok: true } | { ok: false; reason: string };

function hex(bytes: ArrayBuffer): string {
  return [...new Uint8Array(bytes)].map((b) => b.toString(16).padStart(2, "0")).join("");
}

/** Checks `bytes` against `img`; `digest` is WebCrypto's (injectable for tests). */
export async function checkUf2(
  bytes: Uint8Array,
  img: Uf2Image,
  digest: (data: Uint8Array) => Promise<ArrayBuffer> = (d) => crypto.subtle.digest("SHA-256", d as BufferSource),
): Promise<Uf2Check> {
  // Structure first: a server without the file may answer with its
  // index.html, which should read as "not a UF2", not as a size mismatch.
  if (bytes.length < UF2_BLOCK) return { ok: false, reason: `${img.file} is not a UF2 file` };
  const v = new DataView(bytes.buffer, bytes.byteOffset, UF2_BLOCK);
  const w = (o: number) => v.getUint32(o, true);
  if (w(0) !== MAGIC_START0 || w(4) !== MAGIC_START1 || w(508) !== MAGIC_END) {
    return { ok: false, reason: `${img.file} is not a UF2 file` };
  }
  if (bytes.length !== img.size) {
    return { ok: false, reason: `${img.file} is ${bytes.length} bytes, the manifest says ${img.size}` };
  }
  if ((w(8) & FLAG_FAMILY_ID) === 0 || w(28) !== img.familyId) {
    return {
      ok: false,
      reason: `${img.file} has UF2 family 0x${w(28).toString(16)}, expected 0x${img.familyId.toString(16)}`,
    };
  }
  const sum = hex(await digest(bytes));
  if (sum !== img.sha256) return { ok: false, reason: `${img.file} has SHA-256 ${sum}, the manifest says ${img.sha256}` };
  return { ok: true };
}

/** A byte count for people: "512 B", "124 KB", "1.2 MB" (powers of 1000). */
export function formatSize(bytes: number): string {
  if (bytes < 1000) return `${bytes} B`;
  if (bytes < 1_000_000) return `${Math.round(bytes / 1000)} KB`;
  return `${(bytes / 1_000_000).toFixed(1)} MB`;
}
