// Firmware manifest written by the firmware build into public/firmware/ (U16).
// Since 0.6 it is an ESP Web Tools manifest with two extra build fields:
//
//   {"name": "esp-base", "version": "0.6.0", "new_install_prompt_erase": false,
//    "builds": [{"chipFamily": "ESP32-C3", "parts": [{"path": "esp32c3.bin", "offset": 0}],
//                "wasm": false, "bsp": "esp32c3.bsp.json"}, ...]}
//
// Older builds wrote
//
//   {"version": "0.1.0",
//    "images": {"esp32c3": {"file": "esp32c3.bin", "offset": 0, "wasm": false}, ...}}
//
// and both parse to the same `Manifest`. A build must have exactly one part
// (the merged image); `bsp` names the chip's mw-bsp/1 descriptor (E15).
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
//
// `uf2` (optional, absent means none) lists the rp-base UF2s for the
// Raspberry Pi Picos, installed by dragging the file onto the drive the
// board mounts in BOOTSEL mode (the Start tab offers the download):
//
//   "uf2": {"rp2040": {"file": "rp-base-rp2040.uf2", "sha256": "<64 hex>",
//                      "size": 123904, "version": "0.1.0",
//                      "familyId": 3834380118, "factory": false, "wasm": false},
//           "rp2350": {"file": "rp-base-rp2350-0.1-factory.uf2", ...,
//                      "familyId": 3834380119, "factory": true, "wasm": true}}
//
// Only these two keys are accepted. `familyId` is the UF2 family of the
// file's blocks: `rp2040` (0xe48bff56) for the Pico; for the Pico 2 only
// the factory image is listed, which carries the A/B partition table and
// slot A at absolute addresses (family `absolute`, 0xe48bff57). Its update
// image (family `rp2350-arm-s`) is for `mwctl fw update`, not BOOTSEL, and
// is refused here. `sha256` is lower-case hex and `size` a whole number of
// 512-byte UF2 blocks; the page checks the downloaded bytes against both
// (`checkUf2` in uf2.ts). `wasm` must be present: false for the Pico.

export interface FirmwareImage {
  file: string;
  offset: number;
  /** The image includes the WASM payload runtime (absent in JSON: `true`). */
  wasm: boolean;
  /** The board support package descriptor next to the image, if listed. */
  bsp?: string;
}

/** The chips the manifest may list UF2s for. */
export type Uf2Chip = "rp2040" | "rp2350";
export const UF2_CHIPS: readonly Uf2Chip[] = ["rp2040", "rp2350"];

/** UF2 family ids (the `familyId` word of every block). */
export const UF2_FAMILY = {
  rp2040: 0xe48bff56,
  absolute: 0xe48bff57,
  rp2350ArmS: 0xe48bff59,
} as const;

/** The family a listed UF2 must have, by chip. */
const UF2_EXPECTED: Record<Uf2Chip, { familyId: number; factory: boolean; name: string }> = {
  rp2040: { familyId: UF2_FAMILY.rp2040, factory: false, name: "0xe48bff56 (rp2040)" },
  rp2350: { familyId: UF2_FAMILY.absolute, factory: true, name: "0xe48bff57 (absolute, the factory image)" },
};

export interface Uf2Image {
  file: string;
  /** SHA-256 of the file, lower-case hex. */
  sha256: string;
  /** File size in bytes, a multiple of 512. */
  size: number;
  /** Firmware version (the crate version on the RP2040, the image's
   * MAJOR.MINOR on the RP2350). */
  version: string;
  familyId: number;
  /** The RP2350 factory image: partition table plus slot A. */
  factory: boolean;
  /** The firmware includes the WASM payload runtime. */
  wasm: boolean;
}

export interface Manifest {
  version: string;
  images: Record<string, FirmwareImage>;
  /** UF2s for BOOTSEL installs; a chip without one is absent. */
  uf2: Partial<Record<Uf2Chip, Uf2Image>>;
}

function isRelativeFile(file: unknown): file is string {
  return (
    typeof file === "string" &&
    file.length > 0 &&
    !file.includes("..") &&
    !file.startsWith("/") &&
    !file.includes("\\") &&
    !file.includes("://")
  );
}

function parseUf2(chip: string, v: unknown): Uf2Image {
  const at = `manifest.uf2.${chip}`;
  if (!(UF2_CHIPS as readonly string[]).includes(chip)) {
    throw new Error(`${at}: unknown chip (expected ${UF2_CHIPS.join(" or ")})`);
  }
  const want = UF2_EXPECTED[chip as Uf2Chip];
  if (typeof v !== "object" || v === null) throw new Error(`${at} must be an object`);
  const { file, sha256, size, version, familyId, factory, wasm } = v as Record<string, unknown>;
  if (!isRelativeFile(file) || !file.endsWith(".uf2")) throw new Error(`${at}.file must be a relative .uf2 file name`);
  if (typeof sha256 !== "string" || !/^[0-9a-f]{64}$/.test(sha256)) {
    throw new Error(`${at}.sha256 must be 64 lower-case hex digits`);
  }
  if (typeof size !== "number" || !Number.isInteger(size) || size <= 0 || size % 512 !== 0) {
    throw new Error(`${at}.size must be a positive multiple of 512`);
  }
  if (typeof version !== "string" || version === "") throw new Error(`${at}.version must be a non-empty string`);
  if (familyId !== want.familyId) throw new Error(`${at}.familyId must be ${want.name}`);
  const isFactory = factory ?? false;
  if (typeof isFactory !== "boolean") throw new Error(`${at}.factory must be a boolean`);
  if (isFactory !== want.factory) {
    throw new Error(want.factory ? `${at} must be the factory image (factory: true)` : `${at} has no factory image`);
  }
  if (typeof wasm !== "boolean") throw new Error(`${at}.wasm must be a boolean`);
  return { file, sha256, size, version, familyId, factory: isFactory, wasm };
}

/** An ESP Web Tools manifest as the old `images` map. */
function fromBuilds(builds: unknown): Record<string, unknown> {
  if (!Array.isArray(builds)) throw new Error("manifest.builds must be an array");
  const images: Record<string, unknown> = {};
  builds.forEach((b, i) => {
    const { chipFamily, parts, wasm, bsp } = (b ?? {}) as Record<string, unknown>;
    if (typeof chipFamily !== "string") throw new Error(`manifest.builds[${i}].chipFamily must be a string`);
    if (!Array.isArray(parts) || parts.length !== 1) {
      throw new Error(`manifest.builds[${i}].parts must hold exactly one merged image`);
    }
    if (bsp !== undefined && !isRelativeFile(bsp)) {
      throw new Error(`manifest.builds[${i}].bsp must be a relative file name`);
    }
    const { path, offset } = (parts[0] ?? {}) as Record<string, unknown>;
    images[chipKey(chipFamily)] = { file: path, offset, wasm, bsp };
  });
  return images;
}

/** Validates a parsed manifest, in either format; throws with a reason when
 * malformed. */
export function parseManifest(json: unknown): Manifest {
  if (typeof json !== "object" || json === null) throw new Error("manifest is not an object");
  const { version, builds, uf2 } = json as Record<string, unknown>;
  let { images } = json as Record<string, unknown>;
  if (typeof version !== "string") throw new Error("manifest.version must be a string");
  if (builds !== undefined) images = fromBuilds(builds);
  if (typeof images !== "object" || images === null) throw new Error("manifest.images must be an object");
  const out: Record<string, FirmwareImage> = {};
  for (const [chip, v] of Object.entries(images as Record<string, unknown>)) {
    const { file, offset, wasm, bsp } = (v ?? {}) as Record<string, unknown>;
    if (!isRelativeFile(file)) {
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
    out[chip] = { file, offset: off, wasm: runtime, ...(typeof bsp === "string" ? { bsp } : {}) };
  }
  const uf2Out: Partial<Record<Uf2Chip, Uf2Image>> = {};
  if (uf2 !== undefined) {
    if (typeof uf2 !== "object" || uf2 === null || Array.isArray(uf2)) throw new Error("manifest.uf2 must be an object");
    for (const [chip, v] of Object.entries(uf2 as Record<string, unknown>)) uf2Out[chip as Uf2Chip] = parseUf2(chip, v);
  }
  return { version, images: out, uf2: uf2Out };
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

/** Where the bundled firmware and its manifest are served, relative to the page. */
export const FIRMWARE_BASE = "firmware/";

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
