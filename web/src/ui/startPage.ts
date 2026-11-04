// Start tab (U16): what the site is for, what a payload is, a one-click
// path to a blinking LED in the simulator (no board needed), and the
// steps for the board the visitor picks (src/onboarding/boards.ts). Opens
// first on a visit with no saved tab.

import { BOARDS, type Board, FIRST_EXAMPLE, boardById, gettingStarted } from "../onboarding/boards";
import type { App } from "./app";
import { h } from "./dom";

const BOARD_KEY = "start-board";

function savedBoard(): Board | undefined {
  try {
    return boardById(localStorage.getItem(BOARD_KEY));
  } catch {
    return undefined;
  }
}

export function startPage(app: App): HTMLElement {
  const steps = h("ol", { class: "steps" });
  const picker = h("div", { class: "boards", role: "group", "aria-labelledby": "start-board" });
  const buttons = new Map<string, HTMLButtonElement>();

  function choose(b: Board) {
    for (const [id, btn] of buttons) btn.setAttribute("aria-pressed", String(id === b.id));
    steps.replaceChildren(
      ...gettingStarted(b).map(({ title, body, action }) =>
        h(
          "li",
          {},
          h("strong", {}, title),
          h("p", {}, body),
          action ? h("button", { onclick: () => app.showTab(action.tab) }, action.label) : null,
        ),
      ),
    );
    if (!b.runsPayloads) {
      steps.append(h("li", { class: "muted" }, "Want to see payloads run on hardware? Use an ESP32, ESP32-S3 or Pico 2."));
    }
    try {
      localStorage.setItem(BOARD_KEY, b.id);
    } catch {
      /* storage unavailable */
    }
  }

  for (const b of BOARDS) {
    const btn = h(
      "button",
      { "aria-pressed": "false", type: "button", onclick: () => choose(b) },
      b.label,
      h("span", { class: "muted" }, b.runsPayloads ? "runs payloads" : "stores payloads only"),
    );
    buttons.set(b.id, btn);
    picker.append(btn);
  }
  const initial = savedBoard();
  if (initial) choose(initial);
  else steps.append(h("li", { class: "muted" }, "Pick your board to see its steps."));

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
