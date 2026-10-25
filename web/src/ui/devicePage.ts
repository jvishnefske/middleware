// Device tab (U2, U3): console, sys/info, publications, publish form, and
// payload upload/start/stop over the base firmware's serial protocol.

import { FitAddon } from "@xterm/addon-fit";
import { Terminal } from "@xterm/xterm";
import "@xterm/xterm/css/xterm.css";
import { DeviceClient } from "../device/client";
import { firmwareRunsPayloads, uploadPayload } from "../device/upload";
import type { DeviceInfo } from "../protocol/wasm";
import { describePort } from "../serial/ports";
import type { App, PayloadHandoff } from "./app";
import { errorText, formatBytes, h, parseHex, progressBar } from "./dom";

const BAUD = 115200;
const MAX_ROWS = 100;
const OWNER = "device";

export function devicePage(app: App): HTMLElement {
  const { proto } = app;
  const keys = proto.constants.keys;
  let client: DeviceClient | null = null;
  let port: SerialPort | null = null;
  let offered: PayloadHandoff | null = null;

  // ---- console
  const termEl = h("div", { class: "terminal" });
  const term = new Terminal({ convertEol: true, fontSize: 13, scrollback: 5000, disableStdin: true });
  const fit = new FitAddon();
  term.loadAddon(fit);
  queueMicrotask(() => {
    term.open(termEl);
    fit.fit();
  });
  new ResizeObserver(() => {
    if (termEl.offsetParent) fit.fit();
  }).observe(termEl);
  const note = (s: string) => term.writeln(`\x1b[36m-- ${s}\x1b[0m`);

  // ---- status
  const connBtn = h("button", { class: "primary" }, "Connect");
  const resetBtn = h("button", { disabled: true, title: "Pulse RTS (EN) to reboot the chip" }, "Reset device");
  const infoBtn = h("button", { disabled: true }, "Refresh info");
  const infoEl = h("dl", { class: "info" });
  const statusEl = h("span", { class: "badge" }, "disconnected");

  function showInfo(i: DeviceInfo | null) {
    infoEl.replaceChildren();
    if (!i) {
      applyRuntime(null);
      return;
    }
    for (const [k, v] of [
      ["Chip", i.chip],
      ["Firmware", i.firmware],
      ["ABI", `v${i.abiVersion}${i.abiVersion === proto.abi.version ? "" : ` (site expects v${proto.abi.version})`}`],
      ["Payload capacity", `${i.capacity} bytes`],
    ]) {
      infoEl.append(h("dt", {}, k), h("dd", {}, v));
    }
    setStatus(i.statusName);
    applyRuntime(i);
  }
  function setStatus(s: string) {
    statusEl.textContent = s;
    statusEl.dataset.state = s;
  }

  // ---- publications
  const rows = h("tbody");
  function addRow(key: string, payload: Uint8Array) {
    const time = new Date().toLocaleTimeString();
    rows.prepend(h("tr", {}, h("td", {}, time), h("td", {}, key), h("td", { class: "mono" }, formatBytes(payload))));
    while (rows.children.length > MAX_ROWS) rows.lastElementChild?.remove();
  }

  // ---- publish form
  const pubKey = h("input", { type: "text", placeholder: "app/led", value: "app/echo/in" });
  const pubData = h("input", { type: "text", placeholder: "payload", value: "hello" });
  const pubHex = h("input", { type: "checkbox" });
  const pubBtn = h("button", { disabled: true, type: "submit" }, "Publish");
  const pubForm = h(
    "form",
    { class: "row" },
    h("label", {}, "Key ", pubKey),
    h("label", {}, "Payload ", pubData),
    h("label", {}, pubHex, " hex"),
    pubBtn,
  );
  pubForm.addEventListener("submit", async (ev) => {
    ev.preventDefault();
    if (!client) return;
    try {
      const data = pubHex.checked ? parseHex(pubData.value) : new TextEncoder().encode(pubData.value);
      await client.publish(pubKey.value.trim(), data);
    } catch (e) {
      note(`publish failed: ${errorText(e)}`);
    }
  });

  // ---- upload
  const wasmFile = h("input", { type: "file", accept: ".wasm,application/wasm" });
  const fromEditor = h("span", { class: "muted" }, "Nothing from the editor yet.");
  const useEditor = h("input", { type: "radio", name: "upsrc", disabled: true });
  const useFile = h("input", { type: "radio", name: "upsrc", checked: true });
  const autoStart = h("input", { type: "checkbox", checked: true });
  const autoStartNote = h("span", { class: "muted" });
  // Whether the connected firmware has the WASM runtime; unknown until
  // sys/info answers, and assumed present so start stays available.
  let runtime = true;
  function applyRuntime(i: DeviceInfo | null) {
    runtime = i ? firmwareRunsPayloads(i.firmware) : true;
    autoStart.disabled = !runtime;
    if (!runtime) autoStart.checked = false;
    startBtn.disabled = !client || !runtime;
    const why = `${i?.firmware ?? ""} has no WASM runtime: payloads are stored but not run`;
    autoStart.title = runtime ? "" : why;
    startBtn.title = runtime ? "" : why;
    autoStartNote.textContent = runtime ? "" : `(protocol-only firmware: ${why})`;
  }
  const upBtn = h("button", { disabled: true, class: "primary" }, "Upload");
  const startBtn = h("button", { disabled: true }, "Start");
  const stopBtn = h("button", { disabled: true }, "Stop");
  const upBar = progressBar();
  const upMsg = h("span", { class: "muted" });

  app.onPayloadOffered((p) => {
    offered = p;
    fromEditor.textContent = `${p.name} (${p.wasm.length} bytes) from the editor`;
    useEditor.disabled = false;
    useEditor.checked = true;
  });

  async function payloadBytes(): Promise<{ name: string; wasm: Uint8Array }> {
    if (useEditor.checked && offered) return offered;
    const f = wasmFile.files?.[0];
    if (!f) throw new Error("choose a .wasm file or send one from the editor");
    return { name: f.name, wasm: new Uint8Array(await f.arrayBuffer()) };
  }

  upBtn.addEventListener("click", async () => {
    if (!client) return;
    upBtn.disabled = true;
    try {
      const p = await payloadBytes();
      const start = runtime && autoStart.checked;
      upMsg.textContent = `Uploading ${p.name} ...`;
      await uploadPayload(client, proto, p.wasm, {
        start,
        onProgress: (d, t) => upBar.set(d, t),
      });
      upMsg.textContent = `Installed ${p.name} (${p.wasm.length} bytes)${start ? " and started" : runtime ? "" : " (stored only: this firmware does not run payloads)"}.`;
    } catch (e) {
      upMsg.textContent = errorText(e);
    } finally {
      upBtn.disabled = !client;
    }
  });
  startBtn.addEventListener("click", () => client?.startPayload().catch((e) => note(errorText(e))));
  stopBtn.addEventListener("click", () => client?.stopPayload().catch((e) => note(errorText(e))));

  async function refreshInfo() {
    if (!client) return;
    try {
      showInfo(await client.info());
    } catch (e) {
      note(`sys/info: ${errorText(e)}`);
    }
  }
  infoBtn.addEventListener("click", refreshInfo);

  // ---- connection
  function setConnected(on: boolean) {
    connBtn.textContent = on ? "Disconnect" : "Connect";
    for (const b of [resetBtn, infoBtn, pubBtn, upBtn, startBtn, stopBtn]) b.disabled = !on;
    if (!on) {
      setStatus("disconnected");
      applyRuntime(null);
    }
  }

  async function release() {
    const c = client;
    const p = port;
    client = null;
    port = null;
    await c?.close();
    try {
      await p?.close();
    } catch {
      /* already closed or lost */
    }
    setConnected(false);
    app.ports.released(OWNER);
  }

  async function connect() {
    const p = await app.ports.acquire(OWNER, release);
    await p.open({ baudRate: BAUD });
    // Both lines released: on boards with the DTR/RTS auto-reset circuit
    // this keeps EN high and GPIO0 high, so the chip runs normally.
    await p.setSignals({ dataTerminalReady: false, requestToSend: false }).catch(() => {});
    if (!p.readable || !p.writable) throw new Error("port has no streams");
    port = p;
    client = new DeviceClient({ readable: p.readable, writable: p.writable }, proto, {
      onText: (t) => term.write(t),
      onPublish: (key, payload) => {
        if (key === keys.log) term.writeln(`\x1b[33m[log]\x1b[0m ${new TextDecoder().decode(payload)}`);
        else if (key === keys.status && payload.length === 1) {
          setStatus(proto.payloadStatusName(payload[0]) ?? `status ${payload[0]}`);
        }
        addRow(key, payload);
      },
      onInvalid: (reason, len) => note(`dropped ${len}-byte segment (${reason})`),
      onClosed: (err) => {
        if (err) note(`port closed: ${errorText(err)}`);
        if (client) void release();
      },
    });
    client.start();
    setConnected(true);
    note(`connected to ${describePort(p.getInfo())} at ${BAUD} baud`);
    await refreshInfo();
  }

  connBtn.addEventListener("click", async () => {
    connBtn.disabled = true;
    try {
      if (client) await release();
      else await connect();
    } catch (e) {
      note(`connect failed: ${errorText(e)}`);
      await release();
    } finally {
      connBtn.disabled = app.serialProblem !== null;
    }
  });
  connBtn.disabled = app.serialProblem !== null;

  resetBtn.addEventListener("click", async () => {
    if (!port) return;
    // EN low via RTS with DTR (GPIO0) released: a normal reboot, on bridge
    // boards and on the USB-Serial-JTAG peripheral alike.
    await port.setSignals({ dataTerminalReady: false, requestToSend: true });
    await new Promise((r) => setTimeout(r, 100));
    await port.setSignals({ requestToSend: false });
    note("reset");
  });

  return h(
    "section",
    { class: "page device" },
    h("h2", {}, "Device"),
    h("p", { class: "row" }, connBtn, resetBtn, infoBtn, h("span", {}, "Payload: ", statusEl)),
    h("div", { class: "cols" },
      h("div", {}, h("h3", {}, "Console"), termEl),
      h("div", {}, h("h3", {}, "Device info"), infoEl),
    ),
    h("h3", {}, "Publish"),
    pubForm,
    h("h3", {}, "Payload"),
    h(
      "div",
      { class: "upload" },
      h("label", {}, useFile, " File ", wasmFile),
      h("label", {}, useEditor, " ", fromEditor),
      h("p", { class: "row" }, upBtn, h("label", {}, autoStart, " start after upload"), startBtn, stopBtn, autoStartNote),
      upBar.el,
      upMsg,
    ),
    h("h3", {}, "Recent publications"),
    h("table", {}, h("thead", {}, h("tr", {}, h("th", {}, "Time"), h("th", {}, "Key"), h("th", {}, "Payload"))), rows),
  );
}
