// Blocks tab: a Blockly visual payload editor. The workspace is turned
// into AssemblyScript by src/blocks/generator.ts (shown read-only in
// CodeMirror as you edit), compiled and checked by src/blocks/pipeline.ts,
// then run in the simulator (SimSession) or handed to the Device tab for
// upload, like the Editor tab.
//
// The board profile decides which blocks are enabled: blocks the profile
// does not support are disabled in the toolbox and in the workspace (with
// the "profile" disabled reason, so switching back re-enables them). The
// workspace and profile persist in localStorage and can be exported and
// imported as JSON; the Examples menu loads the bundled workspaces.

import { javascript } from "@codemirror/lang-javascript";
import { EditorState } from "@codemirror/state";
import { EditorView, basicSetup } from "codemirror";
import "blockly/blocks";
import * as Blockly from "blockly/core";
import * as En from "blockly/msg/en";
import { BLOCK_DEFS, blockSupported, toolbox } from "../blocks/defs";
import { EXAMPLES, exampleWorkspace } from "../blocks/examples";
import { type GenIssue, generate } from "../blocks/generator";
import { buildPayload } from "../blocks/pipeline";
import { PROFILES, PROFILE_IDS, type ProfileId, isProfileId } from "../blocks/profiles";
import { browserStore, load, parseSaved, save, serialize } from "../blocks/storage";
import type { WorkspaceJson } from "../blocks/workspace";
import type { PinState, SimEvent } from "../protocol/wasm";
import { SimSession } from "../sim/session";
import type { App } from "./app";
import { errorText, formatBytes, h } from "./dom";

/** Disabled reason for blocks the selected profile does not support. */
const PROFILE_REASON = "profile";
const BUTTONS: [number, string][] = [
  [1, "BOOT"],
  [2, "LEFT"],
  [3, "SELECT"],
  [4, "START"],
  [5, "RIGHT"],
];

/** The profile's toolbox, typed for Blockly (defs.ts stays Blockly-free). */
function toolboxFor(profile: ProfileId): Blockly.utils.toolbox.ToolboxDefinition {
  return toolbox(profile) as unknown as Blockly.utils.toolbox.ToolboxDefinition;
}

let registered = false;
function registerBlocks(): void {
  if (registered) return;
  registered = true;
  Blockly.setLocale(En as unknown as Record<string, string>);
  Blockly.defineBlocksWithJsonArray(BLOCK_DEFS as never[]);
}

function themes(): { light: Blockly.Theme; dark: Blockly.Theme } {
  const base = Blockly.Themes.Classic;
  const light = Blockly.Theme.defineTheme("mw-light", {
    name: "mw-light",
    base,
    componentStyles: { workspaceBackgroundColour: "#ffffff", toolboxBackgroundColour: "#f7f7f5", flyoutBackgroundColour: "#ececea" },
  });
  const dark = Blockly.Theme.defineTheme("mw-dark", {
    name: "mw-dark",
    base,
    componentStyles: {
      workspaceBackgroundColour: "#17181a",
      toolboxBackgroundColour: "#212226",
      toolboxForegroundColour: "#e8e8ea",
      flyoutBackgroundColour: "#2a2b30",
      flyoutForegroundColour: "#e8e8ea",
      flyoutOpacity: 1,
      scrollbarColour: "#5a5b62",
      insertionMarkerColour: "#ffffff",
      cursorColour: "#e8e8ea",
    },
  });
  return { light, dark };
}

export function blocksPage(app: App): HTMLElement {
  const { proto } = app;
  const store = browserStore();
  const saved = load(store, "esp-base");
  let profile: ProfileId = saved?.profile ?? "esp-base";
  let compiled: Uint8Array | null = null;
  let session: SimSession | null = null;
  let ws: Blockly.WorkspaceSvg | null = null;
  let saveTimer: ReturnType<typeof setTimeout> | null = null;
  /** Set while the code view and block warnings are refreshed, so the
   * change events that causes are not handled again. */
  let regenerating = false;

  // ---- controls
  const profileSel = h("select", {}, ...PROFILE_IDS.map((id) => h("option", { value: id }, PROFILES[id].label)));
  profileSel.value = profile;
  const profileNote = h("span", { class: "muted" }, PROFILES[profile].note);
  const exampleSel = h(
    "select",
    {},
    h("option", { value: "" }, "Examples..."),
    ...EXAMPLES.map((e) => h("option", { value: e.id }, `${e.label} (${PROFILES[e.profile].label})`)),
  );
  const compileBtn = h("button", { class: "primary" }, "Compile");
  const simBtn = h("button", { disabled: true }, "Simulate");
  const stopSimBtn = h("button", { disabled: true }, "Stop");
  const uploadBtn = h("button", { disabled: true }, "Upload to device");
  const downloadBtn = h("button", { disabled: true }, "Download .wasm");
  const exportBtn = h("button", {}, "Export JSON");
  const importFile = h("input", { type: "file", accept: ".json,application/json", hidden: true });
  const importBtn = h("button", {}, "Import JSON");
  const out = h("pre", { class: "log" }, "Compile to see errors, size and imports.");
  const issuesEl = h("ul", { class: "issues" });

  const code = new EditorView({
    doc: "",
    extensions: [basicSetup, javascript({ typescript: true }), EditorState.readOnly.of(true), EditorView.editable.of(false)],
  });

  const area = h("div", { class: "blockly-area" });

  function current(): WorkspaceJson {
    return ws ? (Blockly.serialization.workspaces.save(ws) as WorkspaceJson) : (saved?.workspace ?? {});
  }

  function invalidate() {
    if (compiled) out.textContent += "\n(The workspace or board changed; compile again.)";
    compiled = null;
    simBtn.disabled = uploadBtn.disabled = downloadBtn.disabled = true;
  }

  function showIssues(errors: GenIssue[], warnings: GenIssue[]) {
    if (ws) for (const b of ws.getAllBlocks(false)) b.setWarningText(null);
    const items = [...errors.map((e) => ["error", e] as const), ...warnings.map((w) => ["warning", w] as const)];
    issuesEl.replaceChildren(
      ...items.map(([kind, issue]) => {
        const block = issue.blockId ? ws?.getBlockById(issue.blockId) : null;
        block?.setWarningText(issue.message);
        const li = h("li", { class: kind }, `${kind}: ${issue.message}`);
        if (block) {
          li.classList.add("link");
          li.addEventListener("click", () => {
            ws?.centerOnBlock(block.id);
            block.select();
          });
        }
        return li;
      }),
    );
  }

  /** Regenerates the code view from the workspace. */
  function regenerate() {
    regenerating = true;
    try {
      const g = generate(current(), profile);
      code.dispatch({ changes: { from: 0, to: code.state.doc.length, insert: g.source } });
      showIssues(g.errors, g.warnings);
    } finally {
      regenerating = false;
    }
  }

  function persist() {
    if (saveTimer) clearTimeout(saveTimer);
    saveTimer = setTimeout(() => save(store, profile, current()), 300);
  }

  /** Disables blocks the profile does not support (and re-enables the
   * ones it does), and swaps the toolbox. */
  function applyProfile() {
    profileNote.textContent = PROFILES[profile].note;
    ws?.updateToolbox(toolboxFor(profile));
    markBlocks();
  }

  function markBlocks() {
    if (ws) for (const b of ws.getAllBlocks(false)) b.setDisabledReason(!blockSupported(b.type, profile), PROFILE_REASON);
  }

  function loadWorkspace(w: WorkspaceJson, p: ProfileId) {
    profile = p;
    profileSel.value = p;
    if (ws) {
      Blockly.Events.disable();
      try {
        ws.clear();
        Blockly.serialization.workspaces.load(w, ws);
      } finally {
        Blockly.Events.enable();
      }
      applyProfile();
    }
    invalidate();
    regenerate();
    persist();
  }

  function inject() {
    if (ws) return;
    registerBlocks();
    const { light, dark } = themes();
    const dark_ = window.matchMedia?.("(prefers-color-scheme: dark)");
    ws = Blockly.inject(area, {
      toolbox: toolboxFor(profile),
      theme: dark_?.matches ? dark : light,
      trashcan: true,
      zoom: { controls: true, wheel: true, startScale: 0.9 },
      grid: { spacing: 24, length: 2, colour: "#8884", snap: true },
      move: { scrollbars: true, drag: true, wheel: false },
    });
    dark_?.addEventListener("change", (e) => ws?.setTheme(e.matches ? dark : light));
    ws.addChangeListener(Blockly.Events.disableOrphans);
    try {
      Blockly.serialization.workspaces.load(saved?.workspace ?? exampleWorkspace("blink")?.workspace ?? {}, ws);
    } catch (e) {
      out.textContent = `Could not restore the saved workspace: ${errorText(e)}`;
    }
    applyProfile();
    ws.addChangeListener((e: Blockly.Events.Abstract) => {
      if (e.isUiEvent || regenerating) return;
      if (e.type === Blockly.Events.BLOCK_CREATE) markBlocks();
      invalidate();
      regenerate();
      persist();
    });
    new ResizeObserver(() => ws && Blockly.svgResize(ws)).observe(area);
    regenerate();
  }
  // Blockly needs its container in the document; main.ts appends the page
  // right after building it.
  queueMicrotask(inject);

  profileSel.addEventListener("change", () => {
    if (!isProfileId(profileSel.value)) return;
    profile = profileSel.value;
    applyProfile();
    invalidate();
    regenerate();
    persist();
    renderBadge();
  });

  exampleSel.addEventListener("change", () => {
    const e = exampleWorkspace(exampleSel.value);
    exampleSel.value = "";
    if (!e) return;
    loadWorkspace(e.workspace, e.profile);
    renderBadge();
    out.textContent = `Loaded the ${e.label} example.`;
  });

  exportBtn.addEventListener("click", () => {
    const url = URL.createObjectURL(new Blob([serialize(profile, current())], { type: "application/json" }));
    h("a", { href: url, download: "blocks.json" }).click();
    setTimeout(() => URL.revokeObjectURL(url), 1000);
  });
  importBtn.addEventListener("click", () => importFile.click());
  importFile.addEventListener("change", async () => {
    const f = importFile.files?.[0];
    importFile.value = "";
    if (!f) return;
    const r = parseSaved(await f.text(), profile);
    if (!r.ok) {
      out.textContent = `Import failed: ${r.error}`;
      return;
    }
    try {
      loadWorkspace(r.saved.workspace, r.saved.profile);
      renderBadge();
      out.textContent = `Imported ${f.name}.`;
    } catch (e) {
      out.textContent = `Import failed: ${errorText(e)}`;
    }
  });

  compileBtn.addEventListener("click", async () => {
    compileBtn.disabled = true;
    out.textContent = "Compiling (loading the AssemblyScript compiler the first time) ...";
    try {
      const r = await buildPayload(current(), profile, proto.abi);
      if (!r.ok) {
        invalidate();
        out.textContent = [`${r.stage} failed:`, ...r.errors].join("\n");
        if (r.issues.length > 0) showIssues(r.issues, []);
        return;
      }
      compiled = r.wasm;
      out.textContent = [
        `OK: ${r.wasm.length} bytes, ${r.memoryBytes} bytes of linear memory${r.memoryBytes < 65536 ? " (1-byte pages)" : ""}.`,
        `Imports: ${r.report.imports.join(", ") || "none"}`,
        `Exports: ${r.report.exports.join(", ")}`,
        ...r.report.warnings.map((w) => `ABI warning: ${w}`),
        r.log,
      ]
        .filter(Boolean)
        .join("\n");
      simBtn.disabled = !proto.hasSimulator;
      uploadBtn.disabled = downloadBtn.disabled = false;
    } catch (e) {
      out.textContent = `Compiler failed: ${errorText(e)}`;
    } finally {
      compileBtn.disabled = false;
    }
  });

  downloadBtn.addEventListener("click", () => {
    if (!compiled) return;
    const url = URL.createObjectURL(new Blob([compiled as BlobPart], { type: "application/wasm" }));
    h("a", { href: url, download: `blocks-${profile}.wasm` }).click();
    setTimeout(() => URL.revokeObjectURL(url), 1000);
  });

  uploadBtn.addEventListener("click", () => {
    if (!compiled) return;
    app.offerPayload({ name: `blocks-${profile}.wasm`, wasm: compiled });
    app.showTab("device");
  });

  // ---- simulator
  const pinsEl = h("div", { class: "pins" });
  const ledsEl = h("div", { class: "pins badge-leds" });
  const screenEl = h("code", {}, "(nothing shown)");
  const petEl = h("span", {}, "-");
  const buttonsEl = h("div", { class: "row" });
  const badgeEl = h(
    "div",
    {},
    h("h3", {}, "Badge"),
    ledsEl,
    h("p", {}, "Screen ", screenEl, h("br"), "Pet mode ", petEl),
    buttonsEl,
  );
  const msgLog = h("pre", { class: "log sim-log" });
  const subsEl = h("span", { class: "muted" });
  const simKey = h("input", { type: "text", value: "app/echo/in" });
  const simData = h("input", { type: "text", value: "ping" });
  const simSend = h("button", { type: "submit", disabled: true }, "Deliver");
  const simForm = h("form", { class: "row" }, h("label", {}, "Key ", simKey), h("label", {}, "Payload ", simData), simSend);

  const logLine = (s: string) => {
    const t = session ? `${(session.nowMs() / 1000).toFixed(2)}s` : "";
    msgLog.textContent += `${t.padStart(8)} ${s}\n`;
    msgLog.scrollTop = msgLog.scrollHeight;
  };

  function renderPins(pins: ReadonlyMap<number, PinState>) {
    pinsEl.replaceChildren(
      ...[...pins.entries()]
        .sort(([a], [b]) => a - b)
        .map(([pin, s]) => {
          if (s.mode === "output") return h("div", { class: "pin" }, h("span", { class: `led${s.high ? " on" : ""}` }), `GPIO ${pin}`);
          const sw = h("input", { type: "checkbox", checked: !!s.high });
          sw.addEventListener("change", () => session?.setInput(pin, sw.checked));
          return h("label", { class: "pin" }, h("span", { class: "switch" }, sw), `GPIO ${pin} (pull ${s.pull})`);
        }),
    );
    if (pins.size === 0) pinsEl.append(h("span", { class: "muted" }, "No pins configured."));
  }

  function renderLeds(rgb: Uint8Array) {
    ledsEl.replaceChildren(
      ...[...Array(8).keys()].map((i) => {
        const [r, g, b] = [rgb[i * 3] ?? 0, rgb[i * 3 + 1] ?? 0, rgb[i * 3 + 2] ?? 0];
        const lit = r + g + b > 0;
        const led = h("span", { class: "led", title: `LED ${i}: ${r} ${g} ${b}` });
        if (lit) {
          led.style.background = `rgb(${r}, ${g}, ${b})`;
          led.style.boxShadow = `0 0 8px rgb(${r}, ${g}, ${b})`;
        }
        return led;
      }),
    );
  }

  function renderBadge() {
    badgeEl.hidden = profile !== "badge";
  }

  function pressButton(n: number) {
    if (!session) return;
    for (const pressed of [1, 0]) {
      const hit = session.deliver("badge/buttons", Uint8Array.of(n, pressed));
      logLine(`in   badge/buttons [${n}, ${pressed}]${hit ? "" : " (no subscription)"}`);
    }
  }
  buttonsEl.append(
    ...BUTTONS.map(([n, label]) => {
      const b = h("button", { type: "button" }, label);
      b.addEventListener("click", () => pressButton(n));
      return b;
    }),
  );

  function onEvent(e: SimEvent) {
    if (e.kind === "log") logLine(`log  ${e.line}`);
    else if (e.kind === "publish") {
      logLine(`pub  ${e.key} ${formatBytes(e.data)}`);
      if (e.key === "badge/leds" && e.data.length === 24) renderLeds(e.data);
      else if (e.key === "badge/text") screenEl.textContent = new TextDecoder().decode(e.data);
      else if (e.key === "badge/pet" && e.data.length === 1) petEl.textContent = ["auto", "idle", "walk", "eat", "play", "sleep"][e.data[0] ?? 0] ?? "?";
    } else if (e.kind === "irq" && e.irq !== "Timer") logLine(`irq  ${e.irq} source ${e.source} value ${e.value}`);
  }

  function stopSim() {
    session?.dispose();
    session = null;
    stopSimBtn.disabled = simSend.disabled = true;
  }

  simBtn.addEventListener("click", () => {
    if (!compiled) return;
    stopSim();
    msgLog.textContent = "";
    renderLeds(new Uint8Array(24));
    screenEl.textContent = "(nothing shown)";
    petEl.textContent = "-";
    try {
      session = new SimSession(proto, compiled, {
        onEvent,
        onPins: renderPins,
        onError: (m) => {
          logLine(`stopped: ${m}`);
          stopSimBtn.disabled = simSend.disabled = true;
        },
      });
      session.start();
      subsEl.textContent = `Subscriptions: ${session.subscriptions.join(", ") || "none"}`;
      stopSimBtn.disabled = simSend.disabled = false;
    } catch (e) {
      session = null;
      logLine(`load failed: ${errorText(e)}`);
    }
  });
  stopSimBtn.addEventListener("click", () => {
    stopSim();
    logLine("stopped");
  });
  simForm.addEventListener("submit", (ev) => {
    ev.preventDefault();
    if (!session) return;
    try {
      const key = simKey.value.trim();
      const data = new TextEncoder().encode(simData.value);
      const hit = session.deliver(key, data);
      logLine(`in   ${key} ${formatBytes(data)}${hit ? "" : " (no subscription)"}`);
    } catch (e) {
      logLine(errorText(e));
    }
  });

  renderPins(new Map());
  renderLeds(new Uint8Array(24));
  renderBadge();

  return h(
    "section",
    { class: "page blocks" },
    h("h2", {}, "Blocks"),
    h(
      "p",
      { class: "row" },
      h("label", {}, "Board ", profileSel),
      exampleSel,
      compileBtn,
      simBtn,
      stopSimBtn,
      uploadBtn,
      downloadBtn,
      exportBtn,
      importBtn,
      importFile,
    ),
    h("p", {}, profileNote),
    h(
      "div",
      { class: "cols" },
      h("div", {}, area, issuesEl),
      h(
        "div",
        {},
        h("h3", {}, "Generated AssemblyScript"),
        h("div", { class: "code generated" }, code.dom),
        h("h3", {}, "Build output"),
        out,
        h("h3", {}, "Simulator"),
        proto.hasSimulator
          ? h("p", { class: "muted" }, "Runs in wasm-host, the device runtime, compiled to WASM, with its default fuel per call (the badge grants less).")
          : h("p", { class: "note" }, "This build has no simulator."),
        pinsEl,
        badgeEl,
        subsEl,
        simForm,
        msgLog,
      ),
    ),
  );
}
