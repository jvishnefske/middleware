// Firmware manifest written by the firmware build into public/firmware/:
//
//   {"version": "0.1.0",
//    "images": {"esp32c3": {"file": "esp32c3.bin", "offset": 0, "wasm": false}, ...}}
//
// Keys are chip names in lower case without dashes, matching what
// `chipKey` derives from esptool-js's CHIP_NAME ("ESP32-C3" -> "esp32c3",
// "ESP8266" -> "esp8266").
//
// `wasm` says whether the image includes the WASM payload runtime. Images
// without it (ESP32-C3, ESP8266) still store uploaded payloads but do not run
// them. An absent `wasm` means `true`: manifests written before the flag
// existed only described runtime builds. Any present value must be a boolean.
// The manifest may be absent (no firmware built yet); the flasher then asks
// for a local .bin instead.

export interface FirmwareImage {
  file: string;
  offset: number;
  /** The image includes the WASM payload runtime (absent in JSON: `true`). */
  wasm: boolean;
}

export interface Manifest {
  version: string;
  images: Record<string, FirmwareImage>;
}

/** Validates a parsed manifest; throws with a reason when malformed. */
export function parseManifest(json: unknown): Manifest {
  if (typeof json !== "object" || json === null) throw new Error("manifest is not an object");
  const { version, images } = json as Record<string, unknown>;
  if (typeof version !== "string") throw new Error("manifest.version must be a string");
  if (typeof images !== "object" || images === null) throw new Error("manifest.images must be an object");
  const out: Record<string, FirmwareImage> = {};
  for (const [chip, v] of Object.entries(images as Record<string, unknown>)) {
    const { file, offset, wasm } = (v ?? {}) as Record<string, unknown>;
    if (typeof file !== "string" || file.includes("..") || file.startsWith("/")) {
      throw new Error(`manifest.images.${chip}.file must be a relative file name`);
    }
    const off = offset ?? 0;
    if (typeof off !== "number" || !Number.isInteger(off) || off < 0) {
      throw new Error(`manifest.images.${chip}.offset must be a non-negative integer`);
    }
    const runtime = wasm ?? true;
    if (typeof runtime !== "boolean") {
      throw new Error(`manifest.images.${chip}.wasm must be a boolean`);
    }
    out[chip] = { file, offset: off, wasm: runtime };
  }
  return { version, images: out };
}

/** Manifest key for an esptool-js chip name: "ESP32-S3" -> "esp32s3". */
export function chipKey(chipName: string): string {
  return chipName.toLowerCase().replace(/[^a-z0-9]/g, "");
}

/** The image for a detected chip, if the manifest has one. */
export function imageForChip(m: Manifest, chipName: string): FirmwareImage | undefined {
  return m.images[chipKey(chipName)];
}

/** Note shown next to an image that lacks the payload runtime, or `null`. */
export function runtimeNote(img: FirmwareImage): string | null {
  return img.wasm ? null : "protocol only — payloads are stored but not run";
}

export type ManifestResult = { ok: true; manifest: Manifest } | { ok: false; reason: string };

/** Fetches `<base>manifest.json`. Absent, unparsable (a dev server may
 * answer with index.html) or malformed manifests are reported, not thrown. */
export async function fetchManifest(
  base: string,
  fetchFn: (url: string) => Promise<Response> = (u) => fetch(u),
): Promise<ManifestResult> {
  try {
    const r = await fetchFn(`${base}manifest.json`);
    if (!r.ok) return { ok: false, reason: `no firmware manifest (HTTP ${r.status})` };
    const text = await r.text();
    let json: unknown;
    try {
      json = JSON.parse(text);
    } catch {
      return { ok: false, reason: "no firmware manifest (not JSON)" };
    }
    return { ok: true, manifest: parseManifest(json) };
  } catch (e) {
    return { ok: false, reason: e instanceof Error ? e.message : String(e) };
  }
}
