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
//   mode; the Flash tab is not used for them.
// * ESP32-C3, ESP8266 and the Pico (RP2040) builds are protocol only: they
//   store uploaded payloads but have no WASM runtime, so they do not run
//   them (the `+no-wasm` firmware suffix; `wasm: false` in the manifest).

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

/** A step of the guide, with the tab that does it, if any. */
export interface Step {
  title: string;
  body: string;
  action: { tab: TabId; label: string } | null;
}

/** The example the Start tab opens in the simulator. */
export const FIRST_EXAMPLE = "blink";

function installStep(b: Board): Step {
  if (b.install.kind === "web-serial") {
    return {
      title: "Install the base firmware (once)",
      body:
        `Plug the ${b.label} in over USB, open Flash and press Connect & flash. ` +
        "The site detects the chip and writes the matching image.",
      action: { tab: "flash", label: "Open Flash" },
    };
  }
  return {
    title: "Install the base firmware (once)",
    body:
      `Hold BOOTSEL while plugging the ${b.label} in: it mounts as a USB drive named ${b.install.drive}. ` +
      `Copy the rp-base UF2 (built by firmware-rp/build.sh ${b.install.target}) onto it; ` +
      "the board reboots into the firmware. The Flash tab is for ESP boards only.",
    action: null,
  };
}

function connectStep(b: Board): Step {
  return {
    title: "Connect",
    body:
      `Open Device and press Connect, then pick the ${b.label}'s serial port. ` +
      "Device info shows the chip, the firmware version and the payload capacity.",
    action: { tab: "device", label: "Open Device" },
  };
}

function payloadStep(b: Board): Step {
  const pin =
    b.ledPin === null
      ? ""
      : ` The Blink example drives GPIO 8; set it to GPIO ${b.ledPin} for the on-board LED.`;
  if (b.runsPayloads) {
    return {
      title: "Run a payload",
      body:
        "Build one in Blocks, starting from the Blink example. Press Compile and Simulate to watch it run here, " +
        `then Upload to device: it is installed and started.${pin}`,
      action: { tab: "blocks", label: "Open Blocks" },
    };
  }
  return {
    title: "Upload a payload",
    body:
      `The ${b.label} firmware is protocol only: it stores the payloads you upload and answers sys/info, ` +
      "but has no WASM runtime, so it does not run them. Build and simulate in Blocks, then Upload to device " +
      "to store it.",
    action: { tab: "blocks", label: "Open Blocks" },
  };
}

/** The steps from an unprogrammed board to an uploaded payload. */
export function gettingStarted(b: Board): Step[] {
  return [installStep(b), connectStep(b), payloadStep(b)];
}
