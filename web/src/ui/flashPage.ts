// Flash tab (U1): esptool-js over Web Serial, image from the manifest by
// detected chip or from a local .bin.

import { type ChosenImage, flashFirmware } from "../flash/flasher";
import { type ManifestResult, chipKey, fetchManifest, imageForChip, runtimeNote } from "../flash/manifest";
import type { App } from "./app";
import { errorText, h, progressBar } from "./dom";

const FIRMWARE_BASE = "firmware/";

export function flashPage(app: App): HTMLElement {
  const log = h("pre", { class: "log" });
  const say = (line: string) => {
    log.textContent += line.endsWith("\n") ? line : line + "\n";
    log.scrollTop = log.scrollHeight;
  };
  const bar = progressBar();
  const manifestInfo = h("p", { class: "muted" }, "Looking for firmware/manifest.json ...");
  const useManifest = h("input", { type: "radio", name: "src", value: "manifest", checked: true });
  const useLocal = h("input", { type: "radio", name: "src", value: "local" });
  const file = h("input", { type: "file", accept: ".bin,application/octet-stream" });
  const offset = h("input", { type: "text", value: "0x0", size: 8, title: "Flash offset" });
  const baud = h(
    "select",
    {},
    ...[115200, 460800, 921600].map((b) => h("option", { value: b, selected: b === 460800 }, String(b))),
  );
  const go = h("button", { class: "primary" }, "Connect & flash");

  let manifest: ManifestResult = { ok: false, reason: "not loaded" };
  void fetchManifest(FIRMWARE_BASE).then((m) => {
    manifest = m;
    if (m.ok) {
      const chips = Object.entries(m.manifest.images).map(([chip, img]) => {
        const n = runtimeNote(img);
        return n ? `${chip} (${n})` : chip;
      });
      manifestInfo.textContent = `Bundled firmware ${m.manifest.version}: ${chips.join(", ")}.`;
    } else {
      manifestInfo.textContent = `${m.reason}. Choose a local .bin file instead.`;
      useLocal.checked = true;
    }
  });

  async function chooseImage(chip: string): Promise<ChosenImage> {
    if (useLocal.checked) {
      const f = file.files?.[0];
      if (!f) throw new Error("choose a local .bin file");
      const off = Number.parseInt(offset.value, offset.value.trim().toLowerCase().startsWith("0x") ? 16 : 10);
      if (!Number.isInteger(off) || off < 0) throw new Error("invalid offset");
      return { data: new Uint8Array(await f.arrayBuffer()), offset: off, label: f.name };
    }
    if (!manifest.ok) throw new Error(`${manifest.reason}; choose a local .bin`);
    const img = imageForChip(manifest.manifest, chip);
    if (!img) throw new Error(`the manifest has no image for ${chip} (key "${chipKey(chip)}"); choose a local .bin`);
    const r = await fetch(FIRMWARE_BASE + img.file);
    if (!r.ok) throw new Error(`could not fetch ${img.file}: HTTP ${r.status}`);
    const n = runtimeNote(img);
    if (n) say(`Note: the ${chip} image is ${n}.`);
    return { data: new Uint8Array(await r.arrayBuffer()), offset: img.offset, label: img.file };
  }

  go.addEventListener("click", async () => {
    go.disabled = true;
    log.textContent = "";
    bar.set(0, 1);
    try {
      const port = await app.ports.acquire("flash", async () => {});
      const r = await flashFirmware(
        port,
        { log: say, progress: (w, t) => bar.set(w, t), chooseImage },
        Number(baud.value),
      );
      say(`Done: ${r.image} on ${r.chip}. The device is rebooting into the base firmware; open the Device tab.`);
    } catch (e) {
      say(`Error: ${errorText(e)}`);
    } finally {
      app.ports.released("flash");
      go.disabled = app.serialProblem !== null;
    }
  });
  go.disabled = app.serialProblem !== null;

  return h(
    "section",
    { class: "page" },
    h("h2", {}, "Flash base firmware"),
    h(
      "div",
      { class: "note" },
      h("strong", {}, "Requirements: "),
      "Web Serial works only in Chrome or Edge on desktop, from a secure context (https or http://localhost). ",
      "Boards with a USB-serial bridge (CP2102, CH340, FTDI) are reset into the bootloader automatically via DTR/RTS; ",
      "this includes ESP8266 boards such as the NodeMCU and Wemos D1 mini, whose CH340 drives EN and GPIO0 through ",
      "the classic two-transistor auto-reset circuit. ",
      "On the native USB port (USB-Serial-JTAG on ESP32-S3/C3) the reset is automatic too, unless the running ",
      "application has taken over USB: then hold BOOT, press and release RESET, release BOOT, and try again.",
    ),
    h(
      "p",
      { class: "muted" },
      "ESP32-C3 and ESP8266 images are protocol only: they answer sys/info and store uploaded payloads, ",
      "but have no WASM runtime, so payloads are not run.",
    ),
    manifestInfo,
    h(
      "fieldset",
      {},
      h("legend", {}, "Image"),
      h("label", {}, useManifest, " Bundled image for the detected chip"),
      h("label", {}, useLocal, " Local file ", file, " at offset ", offset),
    ),
    h("p", {}, h("label", {}, "Baud rate ", baud), " ", go),
    bar.el,
    log,
  );
}
