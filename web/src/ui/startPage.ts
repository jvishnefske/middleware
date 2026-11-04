// Start tab (U16): what the site is for, what a payload is, a one-click
// path to a blinking LED in the simulator (no board needed), and the
// steps for the board the visitor picks (src/onboarding/boards.ts decides
// them from the board and the firmware manifest). Opens first on a visit
// with no saved tab. For a Pico whose UF2 the manifest lists, "Download
// UF2" fetches it, checks it (src/flash/uf2.ts) and only then saves it.
// Picking a board also sets the Blocks tab's board, which seeds its pin
// table with that board's defaults (the LED: GPIO 25 on the Picos).

import { FIRMWARE_BASE, type Manifest, type ManifestResult, type Uf2Image } from "../flash/manifest";
import { checkUf2 } from "../flash/uf2";
import { BOARDS, type Board, FIRST_EXAMPLE, type Step, boardById, gettingStarted, textSpans } from "../onboarding/boards";
import type { App } from "./app";
import { errorText, h } from "./dom";

const BOARD_KEY = "start-board";

function savedBoard(): Board | undefined {
  try {
    return boardById(localStorage.getItem(BOARD_KEY));
  } catch {
    return undefined;
  }
}

/** Fetches the UF2, checks it against the manifest, then saves it. */
async function downloadUf2(img: Uf2Image, status: HTMLElement, btn: HTMLButtonElement) {
  btn.disabled = true;
  status.textContent = `Downloading ${img.file} ...`;
  try {
    const r = await fetch(`${FIRMWARE_BASE}${img.file}`);
    if (!r.ok) throw new Error(`${img.file} is not on this site (HTTP ${r.status})`);
    const bytes = new Uint8Array(await r.arrayBuffer());
    const check = await checkUf2(bytes, img);
    if (!check.ok) throw new Error(`${check.reason}. Nothing was saved.`);
    const url = URL.createObjectURL(new Blob([bytes], { type: "application/octet-stream" }));
    const a = h("a", { href: url, download: img.file });
    document.body.append(a);
    a.click();
    a.remove();
    setTimeout(() => URL.revokeObjectURL(url), 10_000);
    status.textContent = `Saved ${img.file}: size and SHA-256 match the manifest. Now drag it onto the drive.`;
  } catch (e) {
    status.textContent = errorText(e);
  } finally {
    btn.disabled = false;
  }
}

/** Text with its backtick spans as <code>. */
function rich(text: string): (Node | string)[] {
  return textSpans(text).map(([t, code]) => (code ? h("code", {}, t) : t));
}

function renderStep({ title, body, substeps, notes, action }: Step, app: App): HTMLElement {
  let button: HTMLElement | null = null;
  if (action?.kind === "tab") {
    button = h("button", { onclick: () => app.showTab(action.tab) }, action.label);
  } else if (action?.kind === "download") {
    const status = h("p", { class: "muted", role: "status" });
    const btn = h("button", { class: "primary" }, action.label);
    btn.addEventListener("click", () => void downloadUf2(action.image, status, btn));
    button = h(
      "div",
      { class: "download" },
      h("p", { class: "row" }, btn, h("span", {}, action.detail)),
      h("p", { class: "muted sha" }, "SHA-256 ", h("code", {}, action.image.sha256)),
      h("p", { class: "muted" }, action.integrity),
      status,
    );
  }
  return h(
    "li",
    {},
    h("strong", {}, title),
    h("p", {}, ...rich(body)),
    button,
    substeps.length ? h("ol", { class: "substeps" }, ...substeps.map((x) => h("li", {}, ...rich(x)))) : null,
    ...notes.map((x) => h("p", { class: "muted" }, ...rich(x))),
  );
}

export function startPage(app: App, manifest: Promise<ManifestResult>): HTMLElement {
  const steps = h("ol", { class: "steps" });
  const picker = h("div", { class: "boards", role: "group", "aria-labelledby": "start-board" });
  const buttons = new Map<string, HTMLButtonElement>();
  let loaded: Manifest | null = null;
  let chosen: Board | undefined;

  function choose(b: Board, picked = false) {
    chosen = b;
    for (const [id, btn] of buttons) btn.setAttribute("aria-pressed", String(id === b.id));
    steps.replaceChildren(...gettingStarted(b, loaded).map((s) => renderStep(s, app)));
    if (!b.runsPayloads) {
      steps.append(h("li", { class: "muted" }, "Want to see payloads run on hardware? Use an ESP32, ESP32-S3 or Pico 2."));
    }
    try {
      localStorage.setItem(BOARD_KEY, b.id);
    } catch {
      /* storage unavailable */
    }
    // A click (not the restore at load) also sets the Blocks board.
    if (picked) app.chooseBoard(b.id);
  }

  for (const b of BOARDS) {
    const btn = h(
      "button",
      { "aria-pressed": "false", type: "button", onclick: () => choose(b, true) },
      b.label,
      h("span", { class: "muted" }, b.runsPayloads ? "runs payloads" : "stores payloads only"),
    );
    buttons.set(b.id, btn);
    picker.append(btn);
  }
  const initial = savedBoard();
  if (initial) choose(initial);
  else steps.append(h("li", { class: "muted" }, "Pick your board to see its steps."));
  void manifest.then((r) => {
    loaded = r.ok ? r.manifest : null;
    if (chosen) choose(chosen);
  });

  const tryBtn = h("button", { class: "primary big", onclick: () => app.tryExample(FIRST_EXAMPLE) }, "Try Blink in the simulator");

  return h(
    "section",
    { class: "page start" },
    h(
      "div",
      { class: "hero" },
      h("h2", {}, "Program a microcontroller from your browser"),
      h(
        "p",
        { class: "lead" },
        "Snap blocks together or write code, watch it run in a simulator, then send it to an ESP32 or ",
        "Raspberry Pi Pico over USB. Nothing to install.",
      ),
      h(
        "p",
        { class: "row" },
        tryBtn,
        h("span", { class: "muted" }, "No board needed: a virtual LED blinks on the Blocks tab."),
      ),
    ),
    h("h3", {}, "How it works"),
    h(
      "ol",
      { class: "how" },
      h("li", {}, h("strong", {}, "Base firmware. "), "Install it on the board once. It handles USB, storage and the pins."),
      h(
        "li",
        {},
        h("strong", {}, "Payload. "),
        "Your program: a small WebAssembly module built here from blocks (Blocks tab) or WAT and ",
        "AssemblyScript (Editor tab). Change it as often as you like without reflashing.",
      ),
      h(
        "li",
        {},
        h("strong", {}, "Simulate, then upload. "),
        "The simulator runs the same runtime as the board. Uploads are signed with a key kept in this ",
        "browser, so a board you claim installs only payloads you signed.",
      ),
    ),
    h("h3", { id: "start-board" }, "Get your board running"),
    picker,
    steps,
  );
}
