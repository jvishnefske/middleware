// Entry point: loads protocol-wasm, builds the five tabs (Start, Flash,
// Device, Editor, Blocks) and the shared serial-port bar, and opens the
// last-shown tab, or Start on a first visit.

import "./style.css";
import { loadProtocol } from "./protocol/wasm";
import { PortManager, describePort, webSerialProblem } from "./serial/ports";
import { type App, type PayloadHandoff, type TabId, initialTab } from "./ui/app";
import { blocksPage } from "./ui/blocksPage";
import { devicePage } from "./ui/devicePage";
import { errorText, h } from "./ui/dom";
import { editorPage } from "./ui/editorPage";
import { flashPage } from "./ui/flashPage";
import { startPage } from "./ui/startPage";

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
  const tryListeners: ((id: string) => void)[] = [];
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
    tryExample(id) {
      app.showTab("blocks");
      tryListeners.forEach((f) => f(id));
    },
    onTryExample: (f) => tryListeners.push(f),
  };

  pages.start = startPage(app);
  pages.flash = flashPage(app);
  pages.device = devicePage(app);
  pages.editor = editorPage(app);
  pages.blocks = blocksPage(app);

  const nav = h("nav", { role: "tablist" });
  for (const [id, label] of [
    ["start", "Start"],
    ["flash", "Flash"],
    ["device", "Device"],
    ["editor", "Editor"],
    ["blocks", "Blocks"],
  ] as [TabId, string][]) {
    tabs[id] = h("button", { role: "tab", id: `tab-${id}`, "aria-controls": `page-${id}`, onclick: () => app.showTab(id) }, label);
    pages[id].id = `page-${id}`;
    pages[id].setAttribute("role", "tabpanel");
    pages[id].setAttribute("aria-labelledby", `tab-${id}`);
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
      h("h1", {}, "Middleware"),
      nav,
      h("div", { class: "portbar" }, "Serial port: ", portLabel, " ", chooseBtn),
    ),
    serialProblem ? h("p", { class: "note warn" }, serialProblem, " Flashing and the device console are disabled; the editors and simulator still work.") : "",
    pages.start,
    pages.flash,
    pages.device,
    pages.editor,
    pages.blocks,
  );

  let saved: string | null = null;
  try {
    saved = localStorage.getItem("tab");
  } catch {
    /* storage unavailable */
  }
  app.showTab(initialTab(saved));
}

void main();
