// Editor tab (U5, U6, D10): CodeMirror with WAT/AssemblyScript, in-browser
// compile, simulator with virtual GPIO (input switches raise interrupt
// edges), ABI 2 peripherals (timers view, SPI device panel, UART terminal,
// interrupt queue counters), and hand-off to the Device tab.

import { javascript } from "@codemirror/lang-javascript";
import { Compartment } from "@codemirror/state";
import { EditorView, basicSetup } from "codemirror";
import { checkAbi } from "../editor/abiCheck";
import { type Language, compile } from "../editor/compile";
import { template, templateNames } from "../editor/templates";
import type { PinState, SimEvent } from "../protocol/wasm";
import { type PeripheralView, SimSession } from "../sim/session";
import type { App } from "./app";
import { errorText, formatBytes, h, parseHex } from "./dom";

export function editorPage(app: App): HTMLElement {
  const { proto } = app;
  let lang: Language = "wat";
  let compiled: Uint8Array | null = null;
  let session: SimSession | null = null;

  const langMode = new Compartment();
  const view = new EditorView({
    doc: template("blink", "wat").source,
    extensions: [basicSetup, langMode.of([]), EditorView.updateListener.of((u) => u.docChanged && invalidate())],
  });

  const langSel = h(
    "select",
    {},
    h("option", { value: "wat" }, "WAT"),
    h("option", { value: "as" }, "AssemblyScript"),
  );
  const tplSel = h("select", {});
  function fillTemplates() {
    const keep = tplSel.value;
    const names = templateNames(lang);
    tplSel.replaceChildren(...names.map((n) => h("option", { value: n }, n)));
    tplSel.value = names.includes(keep) ? keep : (names[0] ?? "blink");
  }
  fillTemplates();
  const loadBtn = h("button", {}, "Load template");
  const compileBtn = h("button", { class: "primary" }, "Compile");
  const simBtn = h("button", { disabled: true }, "Simulate");
  const stopSimBtn = h("button", { disabled: true }, "Stop");
  const uploadBtn = h("button", { disabled: true }, "Upload to device");
  const downloadBtn = h("button", { disabled: true }, "Download .wasm");
  const out = h("pre", { class: "log" }, "Compile to see errors, size and imports.");

  function setDoc(text: string) {
    view.dispatch({ changes: { from: 0, to: view.state.doc.length, insert: text } });
  }

  function loadTemplate() {
    const t = template(tplSel.value, lang);
    setDoc(t.source);
    out.textContent = t.builtin
      ? "payloads/templates not found at build time; loaded the built-in minimal blink."
      : `Loaded ${tplSel.value}.${lang === "wat" ? "wat" : "ts"} from payloads/templates.`;
  }

  function invalidate() {
    compiled = null;
    simBtn.disabled = uploadBtn.disabled = downloadBtn.disabled = true;
  }

  langSel.addEventListener("change", () => {
    lang = langSel.value as Language;
    fillTemplates();
    view.dispatch({ effects: langMode.reconfigure(lang === "as" ? javascript({ typescript: true }) : []) });
    loadTemplate();
  });
  loadBtn.addEventListener("click", loadTemplate);

  compileBtn.addEventListener("click", async () => {
    compileBtn.disabled = true;
    out.textContent = lang === "as" ? "Compiling (loading the AssemblyScript compiler the first time) ..." : "Compiling ...";
    try {
      const r = await compile(lang, view.state.doc.toString());
      if (!r.ok) {
        invalidate();
        out.textContent = r.errors;
        return;
      }
      compiled = r.wasm;
      const report = checkAbi(r.wasm, proto.abi);
      out.textContent = [
        `OK: ${r.wasm.length} bytes.`,
        `Imports: ${report.imports.join(", ") || "none"}`,
        `Exports: ${report.exports.join(", ")}`,
        ...report.problems.map((p) => `ABI problem: ${p}`),
        ...report.warnings.map((w) => `ABI warning: ${w}`),
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
    const a = h("a", { href: url, download: `payload.wasm` });
    a.click();
    setTimeout(() => URL.revokeObjectURL(url), 1000);
  });

  uploadBtn.addEventListener("click", () => {
    if (!compiled) return;
    app.offerPayload({ name: `${tplSel.value}-${lang}.wasm`, wasm: compiled });
    app.showTab("device");
  });

  // ---- simulator panel
  const pinsEl = h("div", { class: "pins" });
  const msgLog = h("pre", { class: "log sim-log" });
  const subsEl = h("span", { class: "muted" });
  const simKey = h("input", { type: "text", value: "app/echo/in" });
  const simData = h("input", { type: "text", value: "ping" });
  const simHex = h("input", { type: "checkbox" });
  const simSend = h("button", { type: "submit", disabled: true }, "Deliver");
  const simForm = h(
    "form",
    { class: "row" },
    h("label", {}, "Key ", simKey),
    h("label", {}, "Payload ", simData),
    h("label", {}, simHex, " hex"),
    simSend,
  );

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
          if (s.mode === "output") {
            return h("div", { class: "pin" }, h("span", { class: `led${s.high ? " on" : ""}` }), `GPIO ${pin}`);
          }
          const sw = h("input", { type: "checkbox", checked: !!s.high });
          sw.addEventListener("change", () => session?.setInput(pin, sw.checked));
          return h("label", { class: "pin" }, h("span", { class: "switch" }, sw), `GPIO ${pin} (pull ${s.pull})`);
        }),
    );
    if (pins.size === 0) pinsEl.append(h("span", { class: "muted" }, "No pins configured."));
  }

  // ---- ABI 2 peripherals
  const hex = (b: Uint8Array) => [...b].map((x) => x.toString(16).padStart(2, "0")).join(" ") || "(none)";
  const irqEl = h("p", { class: "muted" });
  const timersEl = h("table", { class: "timers" });
  const spiMode = h(
    "select",
    {},
    h("option", { value: "loopback" }, "loopback"),
    h("option", { value: "pattern" }, "pattern"),
  );
  const spiPattern = h("input", { type: "text", value: "a5 5a", placeholder: "hex pattern" });
  const spiApply = h("button", { type: "submit" }, "Apply");
  const spiForm = h(
    "form",
    { class: "row" },
    h("label", {}, "Responder ", spiMode),
    h("label", {}, "Pattern ", spiPattern),
    spiApply,
  );
  const spiTx = h("code", {}, "(none)");
  const spiRx = h("code", {}, "(none)");
  const uartLog = h("pre", { class: "log uart-log" });
  const uartInput = h("input", { type: "text", value: "hello", placeholder: "bytes to send to the payload" });
  const uartHex = h("input", { type: "checkbox" });
  const uartNl = h("input", { type: "checkbox", checked: true });
  const uartSend = h("button", { type: "submit", disabled: true }, "Send");
  const uartForm = h(
    "form",
    { class: "row" },
    h("label", {}, "RX ", uartInput),
    h("label", {}, uartHex, " hex"),
    h("label", {}, uartNl, " newline"),
    uartSend,
  );

  function renderPeripherals(v: PeripheralView) {
    const irq = session?.handlesIrq ? "" : " (payload exports no on_irq; events are discarded)";
    irqEl.textContent = `Interrupt queue: ${v.pending} pending, ${v.irqs} dispatched, ${v.dropped} dropped (overflow)${irq}`;
    irqEl.classList.toggle("note", v.dropped > 0);
    timersEl.replaceChildren(
      h("tr", {}, h("th", {}, "Timer"), h("th", {}, "Period"), h("th", {}, "Mode"), h("th", {}, "State"), h("th", {}, "Fires")),
      ...v.timers.map((t) =>
        h(
          "tr",
          {},
          h("td", {}, String(t.id)),
          h("td", {}, t.periodUs >= 1000 ? `${t.periodUs / 1000} ms` : `${t.periodUs} µs`),
          h("td", {}, t.periodic ? "periodic" : "one-shot"),
          h("td", {}, t.active ? "active" : "stopped"),
          h("td", {}, String(t.fires)),
        ),
      ),
    );
    if (v.timers.length === 0) timersEl.append(h("tr", {}, h("td", { colspan: 5, class: "muted" }, "No timers started.")));
    spiTx.textContent = v.lastSpi ? hex(v.lastSpi.tx) : "(none)";
    spiRx.textContent = v.lastSpi ? hex(v.lastSpi.rx) : "(none)";
  }

  const uartLine = (dir: string, data: Uint8Array) => {
    uartLog.textContent += `${dir} ${formatBytes(data)}\n`;
    uartLog.scrollTop = uartLog.scrollHeight;
  };

  function applySpi() {
    if (!session) return;
    try {
      session.setSpiResponder(spiMode.value === "pattern" ? parseHex(spiPattern.value) : null);
      logLine(`spi  responder ${spiMode.value === "pattern" ? hex(parseHex(spiPattern.value)) : "loopback"}`);
    } catch (e) {
      logLine(errorText(e));
    }
  }
  spiForm.addEventListener("submit", (ev) => {
    ev.preventDefault();
    applySpi();
  });

  uartForm.addEventListener("submit", (ev) => {
    ev.preventDefault();
    if (!session) return;
    try {
      const text = uartNl.checked && !uartHex.checked ? `${uartInput.value}\n` : uartInput.value;
      const data = uartHex.checked ? parseHex(uartInput.value) : new TextEncoder().encode(text);
      const n = session.uartInject(data);
      uartLine("<-", data);
      if (n < data.length) uartLog.textContent += n === 0 ? "   (UART 1 not open for receive; dropped)\n" : `   (buffer full; ${data.length - n} dropped)\n`;
    } catch (e) {
      logLine(errorText(e));
    }
  });

  function onEvent(e: SimEvent) {
    if (e.kind === "log") logLine(`log  ${e.line}`);
    else if (e.kind === "publish") logLine(`pub  ${e.key} ${formatBytes(e.data)}`);
    else if (e.kind === "uart") uartLine("->", e.data);
    else if (e.kind === "spi") logLine(`spi  tx ${hex(e.tx)} rx ${hex(e.rx)}`);
    // Timer alarms are counted in the timers view rather than logged.
    else if (e.kind === "irq" && e.irq !== "Timer") logLine(`irq  ${e.irq} source ${e.source} value ${e.value}`);
  }

  function stopSim() {
    session?.dispose();
    session = null;
    stopSimBtn.disabled = simSend.disabled = uartSend.disabled = true;
  }

  simBtn.addEventListener("click", () => {
    if (!compiled) return;
    stopSim();
    msgLog.textContent = "";
    uartLog.textContent = "";
    try {
      session = new SimSession(proto, compiled, {
        onEvent,
        onPins: renderPins,
        onPeripherals: renderPeripherals,
        onError: (m) => {
          logLine(`stopped: ${m}`);
          stopSimBtn.disabled = simSend.disabled = uartSend.disabled = true;
        },
      });
      if (spiMode.value === "pattern") applySpi();
      session.start();
      renderPeripherals(session.peripherals());
      subsEl.textContent = `Subscriptions: ${session.subscriptions.join(", ") || "none"}`;
      stopSimBtn.disabled = simSend.disabled = uartSend.disabled = false;
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
      const data = simHex.checked ? parseHex(simData.value) : new TextEncoder().encode(simData.value);
      const key = simKey.value.trim();
      const hit = session.deliver(key, data);
      logLine(`in   ${key} ${formatBytes(data)}${hit ? "" : " (no subscription)"}`);
      subsEl.textContent = `Subscriptions: ${session?.subscriptions.join(", ") || "none"}`;
    } catch (e) {
      logLine(errorText(e));
    }
  });

  renderPins(new Map());
  renderPeripherals({ timers: [], dropped: 0, pending: 0, lastSpi: null, irqs: 0 });

  return h(
    "section",
    { class: "page editor" },
    h("h2", {}, "Payload editor"),
    h(
      "p",
      { class: "row" },
      h("label", {}, "Language ", langSel),
      h("label", {}, "Template ", tplSel),
      loadBtn,
      compileBtn,
      simBtn,
      stopSimBtn,
      uploadBtn,
      downloadBtn,
    ),
    h("div", { class: "cols" },
      h("div", { class: "code" }, view.dom),
      h(
        "div",
        {},
        h("h3", {}, "Build output"),
        out,
        h("h3", {}, "Simulator"),
        proto.hasSimulator ? h("p", { class: "muted" }, "Runs in wasm-host, the device runtime, compiled to WASM.") : h("p", { class: "note" }, "This build has no simulator."),
        pinsEl,
        irqEl,
        subsEl,
        simForm,
        msgLog,
        h("h3", {}, "Timers"),
        timersEl,
        h("h3", {}, "SPI device (bus 0)"),
        spiForm,
        h("p", {}, "Last transfer TX ", spiTx, h("br"), "Last transfer RX ", spiRx),
        h("h3", {}, "UART 1 terminal"),
        uartLog,
        uartForm,
      ),
    ),
  );
}
