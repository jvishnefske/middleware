// Entry point: loads protocol-wasm, builds the four tabs and the shared
// serial-port bar.

import "./style.css";
import { loadProtocol } from "./protocol/wasm";
import { PortManager, describePort, webSerialProblem } from "./serial/ports";
import type { App, PayloadHandoff, TabId } from "./ui/app";
import { blocksPage } from "./ui/blocksPage";
import { devicePage } from "./ui/devicePage";
import { errorText, h } from "./ui/dom";
import { editorPage } from "./ui/editorPage";
import { flashPage } from "./ui/flashPage";

async function main() {
  const root = document.getElementById("app");
  if (!root) return;
  let proto;
  try {
    proto = await loadProtocol();
  } catch (e) {
    root.append(h("p", { class: "note" }, `Could not load protocol-wasm: ${errorText(e)}. Run npm run build:wasm.`));
    return;
  }

  const serialProblem = webSerialProblem(window);
  const ports = new PortManager(() => navigator.serial.requestPort());
  const offerListeners: ((p: PayloadHandoff) => void)[] = [];
  const tabs: Record<TabId, HTMLButtonElement> = {} as Record<TabId, HTMLButtonElement>;
  const pages: Record<TabId, HTMLElement> = {} as Record<TabId, HTMLElement>;

  const app: App = {
    proto,
    ports,
    serialProblem,
    showTab(id) {
      for (const k of Object.keys(pages) as TabId[]) {
        pages[k].hidden = k !== id;
        tabs[k].setAttribute("aria-selected", String(k === id));
      }
      try {
        localStorage.setItem("tab", id);
      } catch {
        /* storage unavailable */
      }
    },
    offerPayload: (p) => offerListeners.forEach((f) => f(p)),
    onPayloadOffered: (f) => offerListeners.push(f),
  };

  pages.flash = flashPage(app);
  pages.device = devicePage(app);
  pages.editor = editorPage(app);
  pages.blocks = blocksPage(app);

  const nav = h("nav", { role: "tablist" });
  for (const [id, label] of [
    ["flash", "Flash"],
    ["device", "Device"],
    ["editor", "Editor"],
    ["blocks", "Blocks"],
  ] as [TabId, string][]) {
    tabs[id] = h("button", { role: "tab", onclick: () => app.showTab(id) }, label);
    nav.append(tabs[id]);
  }

  const portLabel = h("span", { class: "muted" }, "No port selected");
  const chooseBtn = h("button", { disabled: serialProblem !== null }, "Choose port");
  chooseBtn.addEventListener("click", () => ports.choose().catch(() => {}));
  ports.onChange(() => {
    const p = ports.current;
    portLabel.textContent = p
      ? `${describePort(p.getInfo())}${ports.currentOwner ? ` (in use by ${ports.currentOwner})` : ""}`
      : "No port selected";
  });

  root.append(
    h(
      "header",
      {},
      h("h1", {}, "Middleware device tools"),
      nav,
      h("div", { class: "portbar" }, "Serial port: ", portLabel, " ", chooseBtn),
    ),
    serialProblem ? h("p", { class: "note warn" }, serialProblem, " Flashing and the device console are disabled; the editors and simulator still work.") : "",
    pages.flash,
    pages.device,
    pages.editor,
    pages.blocks,
  );

  let initial: TabId = "flash";
  try {
    const saved = localStorage.getItem("tab");
    if (saved === "flash" || saved === "device" || saved === "editor" || saved === "blocks") initial = saved;
  } catch {
    /* storage unavailable */
  }
  app.showTab(initial);
}

void main();
