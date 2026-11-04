// First-run guide data (U16): the boards the base firmware runs on, how
// each one gets the firmware, whether it runs payloads, and the steps from
// an unboxed board to a running payload. Pure data and functions, so the
// Start tab only renders what this module decides and the tests check it
// without a DOM.
//
// * ESP32, ESP32-S3, ESP32-C3 and ESP8266 take the base firmware from the
//   Flash tab (esptool-js over Web Serial).
// * Raspberry Pi Pico (RP2040) and Pico 2 (RP2350) run rp-base, which is
//   installed by copying its UF2 onto the drive the board mounts in BOOTSEL
//   mode; the Flash tab is not used for them. When the firmware manifest
//   lists the chip's UF2 (`manifest.uf2`), the install step offers it as a
//   download (file, size, version, SHA-256) followed by the BOOTSEL and
//   drag steps; otherwise it says how to build it with firmware-rp/build.sh.
//   For the Pico 2 that file is the factory image: it also writes the A/B
//   partition table, so it is for the first install, and later firmware
//   updates go over USB with `mwctl fw update`.
// * ESP32-C3, ESP8266 and the Pico (RP2040) builds are protocol only: they
//   store uploaded payloads but have no WASM runtime, so they do not run
//   them (the `+no-wasm` firmware suffix; `wasm: false` in the manifest).

import type { Manifest, Uf2Image } from "../flash/manifest";
import { formatSize } from "../flash/uf2";
import type { TabId } from "../ui/app";

export type BoardId = "esp32" | "esp32s3" | "esp32c3" | "esp8266" | "pico2" | "pico";

/** How the base firmware gets onto the board. */
export type Install =
  /** The Flash tab: esptool-js over Web Serial. */
  | { kind: "web-serial" }
  /** Copy the UF2 built by `firmware-rp/build.sh <target>` onto the drive
   * the board mounts while BOOTSEL is held. */
  | { kind: "uf2"; target: "rp2040" | "rp2350"; drive: string };

export interface Board {
  id: BoardId;
  label: string;
  /** The chip name `sys/info` reports. */
  chip: string;
  install: Install;
  /** The firmware includes the WASM payload runtime. */
  runsPayloads: boolean;
  /** GPIO of an on-board LED a payload can drive, when the board has a
   * fixed one. */
  ledPin: number | null;
}

export const BOARDS: readonly Board[] = [
  { id: "esp32", label: "ESP32", chip: "esp32", install: { kind: "web-serial" }, runsPayloads: true, ledPin: null },
  { id: "esp32s3", label: "ESP32-S3", chip: "esp32s3", install: { kind: "web-serial" }, runsPayloads: true, ledPin: null },
  { id: "esp32c3", label: "ESP32-C3", chip: "esp32c3", install: { kind: "web-serial" }, runsPayloads: false, ledPin: null },
  { id: "esp8266", label: "ESP8266", chip: "esp8266", install: { kind: "web-serial" }, runsPayloads: false, ledPin: null },
  {
    id: "pico2",
    label: "Raspberry Pi Pico 2",
    chip: "rp2350",
    install: { kind: "uf2", target: "rp2350", drive: "RP2350" },
    runsPayloads: true,
    ledPin: 25,
  },
  {
    id: "pico",
    label: "Raspberry Pi Pico",
    chip: "rp2040",
    install: { kind: "uf2", target: "rp2040", drive: "RPI-RP2" },
    runsPayloads: false,
    ledPin: 25,
  },
];

export function boardById(id: string | null | undefined): Board | undefined {
  return BOARDS.find((b) => b.id === id);
}

/** What a step's button does. */
export type StepAction =
  /** Opens a tab. */
  | { kind: "tab"; tab: TabId; label: string }
  /** Downloads (and checks) a manifest UF2; `detail` is the file name,
   * size and version shown next to the button, `integrity` what the check
   * does and does not prove. */
  | { kind: "download"; label: string; image: Uf2Image; detail: string; integrity: string };

/** A step of the guide: a paragraph, the button that does it, if any, then
 * numbered sub-steps and notes. Text between backticks is a command or
 * file name, shown as code (`textSpans`). */
export interface Step {
  title: string;
  body: string;
  substeps: string[];
  notes: string[];
  action: StepAction | null;
}

function step(title: string, body: string, action: StepAction | null, substeps: string[] = [], notes: string[] = []): Step {
  return { title, body, substeps, notes, action };
}

/** The example the Start tab opens in the simulator. */
export const FIRST_EXAMPLE = "blink";

const INSTALL = "Install the base firmware (once)";

/** Splits text at backticks into plain and code spans: "run `x` now" ->
 * [["run ", false], ["x", true], [" now", false]]. Empty spans are dropped;
 * an unpaired backtick leaves the rest as code. */
export function textSpans(text: string): [string, boolean][] {
  return text
    .split("`")
    .map((t, i): [string, boolean] => [t, i % 2 === 1])
    .filter(([t]) => t !== "");
}

export const UF2_INTEGRITY =
  "Checked before saving: the size and SHA-256 must match this site's firmware manifest. That catches a " +
  "corrupted or outdated download; it is not a signature.";

/** "rp-base-rp2350-0.1-factory.uf2 · 1.3 MB · version 0.1" */
export function uf2Detail(img: Uf2Image): string {
  return `${img.file} · ${formatSize(img.size)} · version ${img.version}`;
}

function installStep(b: Board, m: Manifest | null): Step {
  if (b.install.kind === "web-serial") {
    return step(
      INSTALL,
      `Plug the ${b.label} in over USB, open Flash and press Connect & flash. ` +
        "The site detects the chip and writes the matching image.",
      { kind: "tab", tab: "flash", label: "Open Flash" },
    );
  }
  const { target, drive } = b.install;
  const factory =
    target === "rp2350"
      ? [
          "This is the factory image: besides the firmware it writes the A/B partition table, so it is for the " +
            "first install. Later firmware updates go over USB with `mwctl fw update`, and a failed update rolls " +
            "back to the firmware that was running.",
          "Dragging works on a Pico 2 without a partition table, as shipped. If yours already has one from other " +
            "firmware, load the file with `picotool load --ignore-partitions` instead.",
        ]
      : [];
  const img = m?.uf2[target];
  if (!img) {
    const file = target === "rp2350" ? "rp-base-rp2350-<version>-factory.uf2" : "rp-base-rp2040.uf2";
    return step(
      INSTALL,
      `This site has no prebuilt ${b.label} firmware yet. Build \`${file}\` with \`firmware-rp/build.sh ${target}\` ` +
        "(needs a Rust toolchain and picotool), then:",
      null,
      [
        `Hold BOOTSEL while plugging the ${b.label} in: it mounts as a USB drive named ${drive}.`,
        `Copy \`${file}\` onto ${drive}. The board reboots into the firmware.`,
      ],
      [...factory, "The Flash tab is for ESP boards only."],
    );
  }
  return step(
    INSTALL,
    `Download rp-base for the ${b.label}, then copy it onto the board; no tools needed.`,
    { kind: "download", label: "Download UF2", image: img, detail: uf2Detail(img), integrity: UF2_INTEGRITY },
    [
      `Hold BOOTSEL while plugging the ${b.label} in over USB: it mounts as a drive named ${drive}.`,
      `Drag \`${img.file}\` onto ${drive}. The drive disappears when the copy finishes and the firmware starts.`,
    ],
    [...factory, "The Flash tab is for ESP boards only."],
  );
}

function connectStep(b: Board): Step {
  return step(
    "Connect",
    `Open Device and press Connect, then pick the ${b.label}'s serial port. ` +
      "Device info shows the chip, the firmware version and the payload capacity.",
    { kind: "tab", tab: "device", label: "Open Device" },
  );
}

function payloadStep(b: Board): Step {
  const pin =
    b.ledPin === null
      ? ""
      : ` The Blink example drives GPIO 8; set it to GPIO ${b.ledPin} for the on-board LED.`;
  const blocks: StepAction = { kind: "tab", tab: "blocks", label: "Open Blocks" };
  if (b.runsPayloads) {
    return step(
      "Run a payload",
      "Build one in Blocks, starting from the Blink example. Press Compile and Simulate to watch it run here, " +
        `then Upload to device: it is installed and started.${pin}`,
      blocks,
    );
  }
  return step(
    "Upload a payload",
    `The ${b.label} firmware is protocol only: it stores the payloads you upload and answers sys/info, ` +
      "but has no WASM runtime, so it does not run them. Build and simulate in Blocks, then Upload to device " +
      "to store it.",
    blocks,
  );
}

/** The steps from an unprogrammed board to an uploaded payload. `m` is
 * the firmware manifest, or `null` when there is none (or it has not
 * loaded); Picos then get the build hint instead of a download. */
export function gettingStarted(b: Board, m: Manifest | null = null): Step[] {
  return [installStep(b, m), connectStep(b), payloadStep(b)];
}
