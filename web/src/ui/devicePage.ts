// Device tab (U2, U3, U13): console, sys/info, publications, publish form,
// the Trust panel, and payload upload/start/stop over the base firmware's
// serial protocol. Uploads (including payloads handed over by the Editor and
// Blocks tabs) go through `trustedUpload`: signed when the device pins this
// browser's key, unsigned after a confirmation when it is unclaimed, and
// unsigned with a note when its firmware predates trust.
//
// Plant units (Q23): the units of the Blocks tab's plant bound to this
// board's node id are listed with an Upload button each, which compiles
// the unit's phase server and installs it with the same signed upload,
// then starts it. The connected board is also offered to the Recipe view
// as a unit link (device/requester.ts: requests over this client, its
// publications, and its port closing as the unit leaving), so a recipe
// run drives a unit bound to it on the board.

import { FitAddon } from "@xterm/addon-fit";
import { Terminal } from "@xterm/xterm";
import "@xterm/xterm/css/xterm.css";
import { DeviceClient } from "../device/client";
import { firmwareRunsPayloads, trustedUpload } from "../device/upload";
import { SerialRequester } from "../device/requester";
import { boardUnitsNote, unitsOnBoard, uploadUnit } from "../device/unitUpload";
import type { PlantModel } from "../model/plant";
import { indexedDbStorage } from "../trust/keyStore";
import type { DeviceInfo } from "../protocol/wasm";
import { describePort } from "../serial/ports";
import type { App, PayloadHandoff } from "./app";
import { errorText, formatBytes, h, parseHex, progressBar } from "./dom";
import { modal, trustPanel } from "./trustPanel";

const BAUD = 115200;
const MAX_ROWS = 100;
const OWNER = "device";

export function devicePage(app: App): HTMLElement {
  const { proto } = app;
  const keys = proto.constants.keys;
  let client: DeviceClient | null = null;
  let port: SerialPort | null = null;
  let offered: PayloadHandoff | null = null;
  // The user agreed to unsigned uploads to this unclaimed device; reset on
  // each connection.
  let unsignedOk = false;
  const trust = trustPanel(indexedDbStorage());
  /** The connected board as a recipe unit (when it reports a node id). */
  let requester: SerialRequester | null = null;
  let info: DeviceInfo | null = null;
  let plant: PlantModel | null = null;

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
    info = i;
    app.reportDeviceInfo(i);
    if (i && client && i.node) {
      // The same board after a reconnect keeps its requester, so a run
      // it was a unit of sees it come back; another board gets a new one.
      if (requester?.node !== i.node.toLowerCase()) {
        requester?.closed();
        requester = new SerialRequester(client, i);
      } else if (!requester.connected) {
        requester.reopened(client, i);
      }
      app.reportBoard({ info: i, requester });
    }
    renderUnits();
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
      ["Serves phases", i.serves ? "yes (requests reach the payload)" : "no (firmware too old to pass requests to the payload)"],
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
  const trustNote = h("p", { class: "muted" });
  trust.onChange(() => {
    const n = trust.uploadNote();
    trustNote.textContent = n.text;
    trustNote.className = n.warn ? "note" : "muted";
  });

  async function confirmUnsigned(why: "unclaimed" | "legacy-firmware"): Promise<boolean> {
    if (why === "legacy-firmware" || unsignedOk) return true;
    const r = await modal(
      "Upload unsigned?",
      [
        h("p", {}, "This device is unclaimed, so it accepts any payload, signed or not, from anyone with a cable."),
        h("p", {}, "Claim it in the Trust panel to pin your key; after that it installs only payloads you signed."),
      ],
      "Upload unsigned",
    );
    unsignedOk = r !== null;
    return unsignedOk;
  }

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
      const sent = await trustedUpload(client, proto, p.wasm, trust.key(), {
        start,
        onProgress: (d, t) => upBar.set(d, t),
        confirmUnsigned,
      });
      const how =
        sent.kind === "signed"
          ? ` signed, version ${sent.version}${sent.bumped ? " (warning: the clock is not ahead of the device's last version, so it was bumped to min_version + 1)" : ""}`
          : sent.reason === "unclaimed"
            ? " unsigned (device unclaimed)"
            : " unsigned (this firmware predates signed payloads)";
      upMsg.textContent = `Installed ${p.name} (${p.wasm.length} bytes),${how}${start ? ", and started" : runtime ? "" : " (stored only: this firmware does not run payloads)"}.`;
    } catch (e) {
      upMsg.textContent = errorText(e);
    } finally {
      upBtn.disabled = !client;
      await trust.refresh();
    }
  });
  // ---- plant units bound to this board
  const unitsNote = h("p", { class: "muted", id: "device-units-note" });
  const unitsList = h("ul", { class: "device-units" });
  const unitsMsg = h("p", { class: "muted", role: "status", "aria-live": "polite" });
  function renderUnits() {
    unitsNote.textContent = boardUnitsNote(plant, info);
    const units = unitsOnBoard(plant, info?.node);
    unitsList.replaceChildren(
      ...units.map((u) => {
        const b = h("button", { type: "button", id: `device-unit-${u.id}`, disabled: !client }, `Upload ${u.id}'s payload`);
        b.addEventListener("click", async () => {
          const c = client;
          const p = plant;
          if (!c || !p) return;
          b.disabled = true;
          unitsMsg.textContent = `Compiling and uploading ${u.id} ...`;
          try {
            const sent = await uploadUnit(c, proto, p, u.id, trust.key(), { onProgress: (d, t) => upBar.set(d, t), confirmUnsigned });
            unitsMsg.textContent = `Installed and started ${u.id} (${sent.kind === "signed" ? `signed, version ${sent.version}` : "unsigned"}). Run the recipe from Blocks → Recipe.`;
            await refreshInfo();
          } catch (e) {
            unitsMsg.textContent = errorText(e);
          } finally {
            b.disabled = !client;
            await trust.refresh();
          }
        });
        return h("li", {}, `${u.id} (${u.name}) `, b);
      }),
    );
  }
  app.onPlant((p) => {
    plant = p;
    renderUnits();
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
    // The board leaves any recipe run it is a unit of.
    requester?.closed();
    info = null;
    app.reportBoard(null);
    renderUnits();
    await c?.close();
    try {
      await p?.close();
    } catch {
      /* already closed or lost */
    }
    setConnected(false);
    trust.setClient(null);
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
        requester?.publication(key, payload);
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
    unsignedOk = false;
    trust.setClient(client);
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
    h("h3", {}, "Trust"),
    trust.el,
    h("h3", {}, "Publish"),
    pubForm,
    h("h3", {}, "Payload"),
    h(
      "div",
      { class: "upload" },
      h("label", {}, useFile, " File ", wasmFile),
      h("label", {}, useEditor, " ", fromEditor),
      h("p", { class: "row" }, upBtn, h("label", {}, autoStart, " start after upload"), startBtn, stopBtn, autoStartNote),
      trustNote,
      upBar.el,
      upMsg,
    ),
    h("h3", {}, "Plant units on this board"),
    h("div", { class: "upload" }, unitsNote, unitsList, unitsMsg),
    h("h3", {}, "Recent publications"),
    h("table", {}, h("thead", {}, h("tr", {}, h("th", {}, "Time"), h("th", {}, "Key"), h("th", {}, "Payload"))), rows),
  );
}
