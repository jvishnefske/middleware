// Wire tab: a boolean wiring drawn as nodes and edges, written to a board
// by mwctl running in the page.
//
// Digital inputs drive channels, functions of channels drive channels,
// channels drive digital outputs. The canvas edits the graph
// (src/mwctl/wire.ts); after every change mwctl (the command-line tool,
// compiled for WASI, src/mwctl/host.ts) checks it and says what is wrong
// with which node or edge, and spells the same wiring as a command line.
// That line is editable: the canvas and the command line are two views of
// one wiring. The page holds no wiring rule of its own.
//
// The board bar reacts to a board: when a granted serial port appears,
// mwctl probes it and says whether it is ready to update (a wiring can be
// written) or ready to flash (and how). "Write to board" runs `mwctl
// --port web wire`: checked against the board's chip, uploaded, started.
// Nothing but static files is fetched.

import { type Mwctl, type PortHost, splitCommandLine } from "../mwctl/host";
import { WEB_PORT, WebPort } from "../mwctl/port";
import { type BoardState, canWrite, describeBoard, flashesHere, probe, usbId, watchPorts } from "../mwctl/ready";
import * as wire from "../mwctl/wire";
import type { PinState } from "../protocol/wasm";
import { SimSession } from "../sim/session";
import { silPanel } from "./silPanel";
import type { App } from "./app";
import { errorText, h } from "./dom";

const OWNER = "the Wire tab";
const SVG = "http://www.w3.org/2000/svg";
const NODE_W = 156;
const NODE_H = 46;
const CHIPS = ["rp2350", "rp2040", "esp32", "esp32s3", "esp32c3", "esp8266", "rt1050"];

type Selection = { node: string } | { edge: [string, string] } | null;

function svg<K extends keyof SVGElementTagNameMap>(tag: K, attrs: Record<string, string | number> = {}, ...children: (Node | string)[]): SVGElementTagNameMap[K] {
  const el = document.createElementNS(SVG, tag);
  for (const [k, v] of Object.entries(attrs)) el.setAttribute(k, String(v));
  el.append(...children);
  return el;
}

/** What a node shows: a small heading and its name. */
export function nodeLabel(n: wire.WireNode): { head: string; name: string } {
  const c = n.config ?? {};
  const pin = (word: string) => (c.pin === undefined ? "no pin" : `${word} ${c.pin}`);
  const rate = c.period_ms === undefined ? "" : ` · ${c.period_ms} ms`;
  const invert = c.invert ? " · inverted" : "";
  switch (n.kind) {
    case "input":
      return { head: `INPUT${c.pull === "up" ? " · pull-up" : c.pull === "down" ? " · pull-down" : ""}${invert}${rate}`, name: pin("GPIO") };
    case "analog":
      return { head: `ANALOG IN${rate}`, name: pin("ADC") };
    case "const":
      return { head: "CONSTANT", name: c.value === undefined ? "no value" : String(c.value) };
    case "output":
      return { head: `OUTPUT${invert}${rate}`, name: pin("GPIO") };
    case "pwm":
      return { head: `PWM OUT${rate}`, name: c.timer === undefined || c.channel === undefined ? "no timer" : `PWM ${c.timer}.${c.channel}` };
    case "channel":
      return { head: `CHANNEL${c.trace ? " · traced" : ""}`, name: n.id };
    default: {
      // The models of ukf and cbf are too long for a node: its settings panel shows them.
      const settings = (["shift", "n", "ms", "lo", "hi", "mul", "div"] as const).filter((k) => c[k] !== undefined).map((k) => `${k}=${c[k]}`);
      return { head: `FUNCTION${rate}`, name: `${n.kind.toUpperCase()}${settings.length ? ` ${settings.join(" ")}` : ""}` };
    }
  }
}

/** How a kind is drawn: sources like inputs, sinks like outputs. */
export function drawClass(kind: wire.WireKind): "input" | "output" | "channel" | "function" {
  if (kind === "input" || kind === "analog" || kind === "const") return "input";
  if (kind === "output" || kind === "pwm") return "output";
  return kind === "channel" ? "channel" : "function";
}

/** The curve of an edge between two node positions. */
export function edgePath(from: { x: number; y: number }, to: { x: number; y: number }): string {
  const x1 = from.x + NODE_W;
  const y1 = from.y + NODE_H / 2;
  const x2 = to.x;
  const y2 = to.y + NODE_H / 2;
  const bend = Math.max(40, Math.abs(x2 - x1) / 2);
  return `M ${x1} ${y1} C ${x1 + bend} ${y1}, ${x2 - bend} ${y2}, ${x2} ${y2}`;
}

/** Terminal text with carriage returns applied: `\r` restarts the line. */
export function applyOutput(text: string, chunk: string): string {
  for (const c of chunk) {
    if (c === "\r") text = text.slice(0, text.lastIndexOf("\n") + 1);
    else text += c;
  }
  return text;
}

export function wirePage(app: App, loading: Promise<Mwctl>): HTMLElement {
  let mwctl: Mwctl | null = null;
  let graph = wire.loadGraph(globalThis.localStorage) ?? wire.emptyGraph();
  let latest: wire.WireReport | null = null;
  let selection: Selection = null;
  let board: BoardState | null = null;
  let busy = false;
  let sim: SimSession | null = null;
  let simPins: ReadonlyMap<number, PinState> = new Map();
  let revision = 0;
  let kinds: wire.KindSpec[] = [];
  const history = new wire.History();

  // ---- board bar
  const boardText = h("span", {}, "No board yet. Plug one in, or choose its port.");
  const boardBadge = h("span", { class: "badge" }, "no board");
  const checkBtn = h("button", { disabled: app.serialProblem !== null }, "Check board");
  const writeBtn = h("button", { class: "primary", disabled: true }, "Write to board");
  const flashBtn = h("button", { hidden: true, onclick: () => app.showTab("flash") }, "Open the Flash tab");
  const chipSel = h("select", { "aria-label": "Board for the pin rules" }, h("option", { value: "auto" }, "the connected board"), h("option", { value: "" }, "any board (no pin rules)"));
  for (const c of CHIPS) chipSel.append(h("option", { value: c }, c));

  const chip = (): string | null => (chipSel.value === "auto" ? (board?.fields.chip ?? null) : chipSel.value || null);

  function showBoard() {
    boardBadge.textContent = board ? `ready to ${board.ready === "unknown" ? "?" : board.ready}` : "no board";
    boardBadge.dataset.state = board?.ready === "update" ? "running" : (board?.ready ?? "");
    if (board) boardText.textContent = describeBoard(board);
    flashBtn.hidden = !flashesHere(board);
    updateButtons();
  }

  function updateButtons() {
    const ok = latest?.ok === true;
    writeBtn.disabled = busy || !ok || !mwctl?.canWait || app.serialProblem !== null || (board !== null && !canWrite(board));
    writeBtn.title = !ok ? "Fix the wiring's problems first" : board && !canWrite(board) ? "This board is not ready to run a wiring" : "";
    checkBtn.disabled = busy || !mwctl?.canWait || app.serialProblem !== null;
    downloadBtn.disabled = !ok;
    deviceBtn.disabled = !ok;
    simBtn.disabled = !ok && !sim;
    undoBtn.disabled = history.size === 0;
    deleteBtn.disabled = selection === null;
  }

  // ---- mwctl console
  const termEl = h("pre", { class: "wire-term", "aria-live": "polite", tabindex: 0 });
  let termText = "";
  function term(chunk: string) {
    termText = applyOutput(termText, chunk).slice(-20000);
    termEl.textContent = termText;
    termEl.scrollTop = termEl.scrollHeight;
  }
  const termInput = h("input", { type: "text", class: "mono", placeholder: "--port web info", "aria-label": "mwctl arguments", spellcheck: false });
  const termRun = h("button", { type: "submit" }, "Run");
  const termForm = h("form", { class: "row wire-cmd" }, h("code", {}, "mwctl"), termInput, termRun);

  /** Runs `f` with the page's port as mwctl's `--port web`. */
  async function withPort<T>(f: (port: PortHost) => Promise<T>): Promise<T> {
    let host: WebPort | null = null;
    const serial = await app.ports.acquire(OWNER, async () => {
      await host?.close();
    });
    host = new WebPort(serial);
    try {
      return await f(host);
    } finally {
      await host.close();
      app.ports.released(OWNER);
    }
  }

  async function exclusive(f: () => Promise<void>) {
    if (busy) return;
    busy = true;
    updateButtons();
    try {
      await f();
    } catch (e) {
      term(`${errorText(e)}\n`);
    } finally {
      busy = false;
      updateButtons();
    }
  }

  async function runMwctl(args: string[]) {
    const m = mwctl;
    if (!m) return;
    term(`$ mwctl ${args.join(" ")}\n`);
    const opts = { onStdout: term, onStderr: term };
    const r = args.includes("--port") ? await withPort((port) => m.run(args, { ...opts, port })) : await m.run(args, opts);
    if (!termText.endsWith("\n")) term("\n");
    if (r.code !== 0) term(`(exit status ${r.code})\n`);
  }

  termForm.addEventListener("submit", (ev) => {
    ev.preventDefault();
    let args = splitCommandLine(termInput.value);
    if (args[0] === "mwctl") args = args.slice(1);
    void exclusive(() => runMwctl(args));
  });

  async function checkBoard(announce: boolean) {
    const m = mwctl;
    if (!m?.canWait || !app.ports.current) return;
    const usb = usbId(app.ports.current.getInfo());
    try {
      board = await withPort((port) => probe(m, port, usb));
      if (announce) term(`board: ${describeBoard(board)}\n`);
    } catch (e) {
      board = null;
      boardText.textContent = `Could not check the board: ${errorText(e)}`;
    }
    showBoard();
    void refresh();
  }

  checkBtn.addEventListener("click", () =>
    void exclusive(async () => {
      if (!app.ports.current) await app.ports.choose();
      await checkBoard(true);
    }),
  );

  writeBtn.addEventListener("click", () =>
    void exclusive(async () => {
      const m = mwctl;
      if (!m) return;
      stopSim();
      term(`$ ${latest?.command ?? "mwctl wire"} --port ${WEB_PORT}\n`);
      const r = await withPort((port) => wire.write(m, graph, port, { onStdout: term, onStderr: term }));
      if (!termText.endsWith("\n")) term("\n");
      term(r.code === 0 ? "The board is running this wiring.\n" : `(exit status ${r.code})\n`);
    }),
  );

  // ---- canvas
  const edgeLayer = svg("g");
  const nodeLayer = svg("g");
  const tempWire = svg("path", { class: "fwire temp", d: "" });
  const canvas = svg("svg", { class: "flow-svg wire-svg", role: "group", "aria-label": "Wiring canvas" }, edgeLayer, tempWire, nodeLayer);
  const canvasBox = h("div", { class: "flow-canvas wire-canvas" });
  canvasBox.append(canvas);
  const statusEl = h("p", { class: "muted wire-status", "aria-live": "polite" });

  const at = (id: string) => {
    const n = graph.nodes.find((node) => node.id === id);
    return { x: n?.x ?? 0, y: n?.y ?? 0 };
  };
  const issuesFor = (id: string) => (latest?.issues ?? []).filter((i) => i.node === id);
  const worst = (list: wire.WireIssue[]) => (list.some((i) => i.severity === "error") ? "error" : list.length ? "warning" : "");

  function point(e: PointerEvent) {
    const r = canvas.getBoundingClientRect();
    return { x: e.clientX - r.left, y: e.clientY - r.top };
  }

  function render() {
    const w = Math.max(600, ...graph.nodes.map((n) => (n.x ?? 0) + NODE_W + 40));
    const hgt = Math.max(440, ...graph.nodes.map((n) => (n.y ?? 0) + NODE_H + 80));
    canvas.setAttribute("width", String(w));
    canvas.setAttribute("height", String(hgt));
    canvas.setAttribute("viewBox", `0 0 ${w} ${hgt}`);

    edgeLayer.replaceChildren();
    for (const [from, to] of graph.edges) {
      if (!graph.nodes.some((n) => n.id === from) || !graph.nodes.some((n) => n.id === to)) continue;
      const d = edgePath(at(from), at(to));
      const bad = (latest?.issues ?? []).some((i) => i.edge?.[0] === from && i.edge?.[1] === to);
      const selected = selection !== null && "edge" in selection && selection.edge[0] === from && selection.edge[1] === to;
      const hit = svg("path", { class: "fwire-hit", d });
      hit.addEventListener("pointerdown", (e) => {
        e.stopPropagation();
        select({ edge: [from, to] });
        canvas.focus();
      });
      edgeLayer.append(svg("path", { class: `fwire${bad ? " error" : ""}${selected ? " sel" : ""}`, d }), hit);
    }

    nodeLayer.replaceChildren();
    for (const n of graph.nodes) {
      const { head, name } = nodeLabel(n);
      const selected = selection !== null && "node" in selection && selection.node === n.id;
      const pinLevel = n.config?.pin === undefined ? undefined : simPins.get(n.config.pin)?.high;
      const live = sim && pinLevel ? " live" : "";
      const group = svg(
        "g",
        {
          class: `fnode wnode k-${drawClass(n.kind)} ${worst(issuesFor(n.id))}${selected ? " sel" : ""}${live}`,
          transform: `translate(${n.x ?? 0} ${n.y ?? 0})`,
          tabindex: 0,
          role: "button",
          "aria-label": `${head.toLowerCase()} ${n.kind === "channel" || wire.isFunction(n.kind) ? n.id : name}`,
          "data-node": n.id,
        },
        svg("rect", { width: NODE_W, height: NODE_H, rx: n.kind === "channel" ? 23 : 6 }),
        svg("text", { class: "fkind", x: 12, y: 17 }, head),
        svg("text", { x: 12, y: 35 }, name),
      );
      if (drawClass(n.kind) !== "input") group.append(svg("circle", { class: "port in", cx: 0, cy: NODE_H / 2, r: 5 }));
      if (drawClass(n.kind) !== "output") {
        const out = svg("circle", { class: `port out${graph.edges.some(([a]) => a === n.id) ? " wired" : ""}`, cx: NODE_W, cy: NODE_H / 2, r: 6 });
        out.append(svg("title", {}, "Drag onto a node to connect"));
        out.addEventListener("pointerdown", (e) => startWire(e, n.id));
        group.append(out);
      }
      group.addEventListener("pointerdown", (e) => startDrag(e, n.id, group));
      group.addEventListener("focus", () => {
        if (!(selection && "node" in selection && selection.node === n.id)) select({ node: n.id }, false);
      });
      group.addEventListener("keydown", (e) => nodeKey(e, n.id));
      nodeLayer.append(group);
    }
    renderProps();
    renderIssues();
    updateButtons();
    sil.setGraph(graph, latest?.ok === true);
  }

  function select(s: Selection, redraw = true) {
    selection = s;
    if (redraw) render();
    else {
      renderProps();
      updateButtons();
    }
  }

  function startDrag(e: PointerEvent, id: string, group: SVGGElement) {
    if (e.button !== 0) return;
    const start = point(e);
    const origin = at(id);
    let moved = false;
    try {
      group.setPointerCapture(e.pointerId);
    } catch {
      /* a synthetic event has no pointer to capture */
    }
    const move = (m: PointerEvent) => {
      const p = point(m);
      const x = Math.max(0, origin.x + p.x - start.x);
      const y = Math.max(0, origin.y + p.y - start.y);
      if (Math.abs(p.x - start.x) + Math.abs(p.y - start.y) > 3) moved = true;
      if (!moved) return;
      group.setAttribute("transform", `translate(${x} ${y})`);
      const node = graph.nodes.find((n) => n.id === id);
      if (node) {
        node.x = x;
        node.y = y;
      }
      // Redraw the edges at this node while it moves.
      edgeLayer.querySelectorAll("path").forEach((p2) => p2.remove());
      for (const [from, to] of graph.edges) edgeLayer.append(svg("path", { class: "fwire", d: edgePath(at(from), at(to)) }));
    };
    const up = () => {
      group.removeEventListener("pointermove", move);
      group.removeEventListener("pointerup", up);
      group.removeEventListener("pointercancel", up);
      if (moved) {
        const here = at(id);
        const node = graph.nodes.find((n) => n.id === id);
        if (node) {
          node.x = origin.x;
          node.y = origin.y;
        }
        change(wire.moveNode(graph, id, here.x, here.y), { node: id });
      } else {
        select({ node: id });
        refocus(id);
      }
    };
    group.addEventListener("pointermove", move);
    group.addEventListener("pointerup", up);
    group.addEventListener("pointercancel", up);
  }

  function startWire(e: PointerEvent, from: string) {
    if (e.button !== 0) return;
    e.stopPropagation();
    e.preventDefault();
    const origin = at(from);
    const move = (m: PointerEvent) => {
      const p = point(m);
      tempWire.setAttribute("d", edgePath(origin, { x: p.x, y: p.y - NODE_H / 2 }));
    };
    const up = (u: PointerEvent) => {
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", up);
      tempWire.setAttribute("d", "");
      const target = document.elementFromPoint(u.clientX, u.clientY)?.closest("[data-node]");
      const to = target?.getAttribute("data-node");
      if (to) void connect(from, to);
    };
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", up);
  }

  async function connect(from: string, to: string) {
    if (!mwctl) return;
    const c = await wire.connect(mwctl, graph, from, to);
    if (c.refused) {
      statusEl.textContent = `Not connected: ${c.refused}.`;
      return;
    }
    statusEl.textContent = `Connected ${from} to ${to}.`;
    change(c.graph, { edge: [from, to] });
  }

  function nodeKey(e: KeyboardEvent, id: string) {
    const step = e.shiftKey ? 48 : 12;
    const moves: Record<string, [number, number]> = { ArrowLeft: [-step, 0], ArrowRight: [step, 0], ArrowUp: [0, -step], ArrowDown: [0, step] };
    const d = moves[e.key];
    if (d) {
      e.preventDefault();
      const p = at(id);
      change(wire.moveNode(graph, id, p.x + d[0], p.y + d[1]), { node: id });
      refocus(id);
    } else if (e.key === "Delete" || e.key === "Backspace") {
      e.preventDefault();
      removeSelection();
    } else if (e.key === " " || e.key === "Enter") {
      e.preventDefault();
      toggleInput(id);
    }
  }

  function refocus(id: string) {
    nodeLayer.querySelector<SVGGElement>(`[data-node="${CSS.escape(id)}"]`)?.focus();
  }

  canvas.addEventListener("keydown", (e) => {
    if ((e.key === "Delete" || e.key === "Backspace") && selection && "edge" in selection) {
      e.preventDefault();
      removeSelection();
    }
  });
  canvas.setAttribute("tabindex", "0");
  canvas.addEventListener("pointerdown", (e) => {
    if (e.target === canvas) select(null);
  });

  function removeSelection() {
    if (!selection) return;
    change("node" in selection ? wire.removeNode(graph, selection.node) : wire.removeEdge(graph, selection.edge[0], selection.edge[1]), null);
  }

  /** Adopts an edited graph: undo step, storage, redraw, then mwctl's view. */
  function change(next: wire.WireGraph, sel: Selection | undefined) {
    if (next === graph) return;
    history.push(graph);
    graph = next;
    if (sel !== undefined) selection = sel;
    wire.saveGraph(globalThis.localStorage, graph);
    stopSim();
    render();
    void refresh();
  }

  /** Asks mwctl about the graph; a newer edit supersedes the answer. */
  async function refresh() {
    if (!mwctl) return;
    const mine = ++revision;
    try {
      const r = graph.nodes.length === 0 ? null : await wire.report(mwctl, graph, chip());
      if (mine !== revision) return;
      latest = r;
      commandErr.textContent = "";
      if (r) {
        // mwctl places nodes that have no position yet.
        for (const n of graph.nodes) {
          const placed = r.graph.nodes.find((p) => p.id === n.id);
          if ((n.x === undefined || n.y === undefined) && placed) {
            n.x = placed.x;
            n.y = placed.y;
          }
        }
        if (document.activeElement !== commandInput) commandInput.value = r.command;
      } else {
        commandInput.value = "mwctl wire";
      }
    } catch (e) {
      if (mine !== revision) return;
      latest = null;
      commandErr.textContent = errorText(e);
    }
    render();
  }

  // ---- palette
  const palette = h("div", { class: "row palette wire-palette", role: "toolbar", "aria-label": "Add a node" });
  let placed = 0;
  const addButton = (kind: wire.WireKind, label: string, column: number) =>
    h(
      "button",
      {
        title: `Add ${label}`,
        onclick: () => {
          const y = 30 + (placed++ % 6) * 62;
          const r = wire.addNode(graph, kind, canvasBox.scrollLeft + 30 + column * 170, canvasBox.scrollTop + y);
          change(r.graph, { node: r.id });
          refocus(r.id);
        },
      },
      `+ ${label}`,
    );
  /** The palette, from mwctl's list of kinds: a button per source, the
   * channel and each sink, and one menu for the functions. */
  function buildPalette() {
    const names: Record<string, string> = { input: "Input", analog: "Analog in", const: "Constant", channel: "Channel", output: "Output", pwm: "PWM out" };
    const fnSel = h("select", { "aria-label": "Function to add" });
    for (const k of kinds.filter((x) => x.class === "function")) fnSel.append(h("option", { value: k.kind }, `${k.kind.toUpperCase()}${k.stateful ? " (keeps state)" : ""}`));
    const addFn = h("button", { title: "Add the chosen function" }, "+ Function");
    addFn.addEventListener("click", () => {
      const y = 30 + (placed++ % 6) * 62;
      const r = wire.addNode(graph, fnSel.value, canvasBox.scrollLeft + 30 + 2 * 190, canvasBox.scrollTop + y);
      change(r.graph, { node: r.id });
      refocus(r.id);
    });
    palette.replaceChildren();
    for (const k of kinds.filter((x) => x.class === "source")) palette.append(addButton(k.kind, names[k.kind] ?? k.kind, 0));
    palette.append(addButton("channel", "Channel", 1), fnSel, addFn);
    for (const k of kinds.filter((x) => x.class === "sink")) palette.append(addButton(k.kind, names[k.kind] ?? k.kind, 4));
  }

  const PARAM_LABELS: Partial<Record<keyof wire.WireConfig, string>> = {
    pin: "Pin",
    pull: "Pull",
    invert: "Active low",
    value: "Value",
    timer: "PWM timer",
    channel: "PWM channel",
    freq_hz: "Frequency (Hz)",
    period_ms: "Update every (ms)",
    trace: "Publish as wire/<name>",
    shift: "Smoothing (shift)",
    n: "Samples (n)",
    ms: "Time (ms)",
    f: "Model f",
    h: "Model h",
    g: "Input matrix g",
    q: "Process noise q",
    r: "Measurement noise r",
    x0: "Initial state",
    p0: "Initial variance",
    scale: "Counts per unit",
    umin: "Lowest output",
    umax: "Highest output",
  };

  function inputsText(spec: wire.KindSpec): string {
    const [lo, hi] = spec.inputs;
    return `reads ${lo === hi ? lo : `${lo} to ${hi}`} channel${hi === 1 ? "" : "s"}, in the order connected${spec.stateful ? "; keeps state between updates" : ""}`;
  }

  /** One setting's control, by its type in mwctl's list. */
  function paramField(node: wire.WireNode, param: wire.ParamSpec): HTMLElement {
    const id = node.id;
    const label = PARAM_LABELS[param.name] ?? param.name;
    const current = node.config?.[param.name];
    if (param.type === "bool") {
      const box = h("input", { type: "checkbox", checked: current === true });
      box.addEventListener("change", () => change(wire.setConfig(graph, id, { [param.name]: box.checked }), undefined));
      return h("label", {}, box, ` ${label}`);
    }
    if (param.type === "pull") {
      const sel = h("select", {});
      for (const o of param.options ?? ["none", "up", "down"]) sel.append(h("option", { value: o }, o));
      sel.value = typeof current === "string" ? current : "none";
      sel.addEventListener("change", () => change(wire.setConfig(graph, id, { pull: sel.value as wire.WirePull }), undefined));
      return h("label", {}, `${label} `, sel);
    }
    if (param.type === "text") {
      const box = h("input", { type: "text", class: "mono", value: typeof current === "string" ? current : "", placeholder: param.hint ?? "", required: param.required, spellcheck: false });
      box.addEventListener("change", () => change(wire.setConfig(graph, id, { [param.name]: box.value.trim() || undefined }), undefined));
      return h("label", { class: "wire-wide" }, `${label} `, box);
    }
    if (param.type === "real") {
      const box = h("input", { type: "number", step: "any", value: typeof current === "number" ? current : "", placeholder: param.default === null ? "" : String(param.default), required: param.required });
      if (param.min !== null) box.min = String(param.min);
      if (param.max !== null) box.max = String(param.max);
      box.addEventListener("change", () => {
        const v = box.value.trim();
        change(wire.setConfig(graph, id, { [param.name]: v === "" || !Number.isFinite(Number(v)) ? undefined : Number(v) }), undefined);
      });
      return h("label", {}, `${label} `, box);
    }
    const placeholder = param.name === "period_ms" ? String(graph.period_ms) : param.default === null ? "" : String(param.default);
    const box = h("input", { type: "number", min: param.min ?? undefined, max: param.max ?? undefined, value: typeof current === "number" ? current : "", placeholder, required: param.required });
    if (param.name === "period_ms") box.step = String(graph.period_ms);
    box.addEventListener("change", () => {
      const v = box.value.trim();
      const n = v === "" || !Number.isInteger(Number(v)) ? undefined : Number(v);
      change(wire.setConfig(graph, id, { [param.name]: n }), undefined);
    });
    return h("label", {}, `${label} `, box);
  }

  const undoBtn = h(
    "button",
    {
      disabled: true,
      onclick: () => {
        const back = history.pop();
        if (!back) return;
        graph = back;
        selection = null;
        wire.saveGraph(globalThis.localStorage, graph);
        stopSim();
        render();
        void refresh();
      },
    },
    "Undo",
  );
  const deleteBtn = h("button", { disabled: true, onclick: removeSelection }, "Delete");
  const exampleBtn = h(
    "button",
    {
      onclick: () => {
        if (graph.nodes.length > 0 && !confirm("Replace the wiring on the canvas with the example? Undo brings it back.")) return;
        void applyCommand(`--name lamp ${wire.EXAMPLE_STATEMENTS.join(" ")}`);
      },
    },
    "Example",
  );
  const clearBtn = h(
    "button",
    {
      onclick: () => {
        if (graph.nodes.length > 0) change(wire.emptyGraph(), null);
      },
    },
    "Clear",
  );

  // ---- command line
  const commandInput = h("input", { type: "text", class: "mono", value: "mwctl wire", "aria-label": "The wiring as a mwctl command line", spellcheck: false });
  const commandErr = h("span", { class: "issues" });
  const commandForm = h("form", { class: "row wire-cmd" }, commandInput, h("button", { type: "submit" }, "Apply"));
  async function applyCommand(line: string) {
    if (!mwctl) return;
    try {
      const r = await wire.fromCommandLine(mwctl, line, chip());
      commandErr.textContent = "";
      commandInput.blur();
      change(r.graph, null);
    } catch (e) {
      commandErr.replaceChildren(h("span", { class: "error" }, errorText(e)));
    }
  }
  commandForm.addEventListener("submit", (ev) => {
    ev.preventDefault();
    void applyCommand(commandInput.value);
  });

  // ---- properties
  const props = h("div", { class: "wire-props" });
  function renderProps() {
    props.replaceChildren();
    if (!selection) {
      const nameIn = h("input", { type: "text", value: graph.name, maxlength: 24 });
      nameIn.addEventListener("change", () => change({ ...wire.copyGraph(graph), name: nameIn.value.trim() }, undefined));
      const periodIn = h("input", { type: "number", min: 10, max: 60000, step: 10, value: graph.period_ms });
      periodIn.addEventListener("change", () => change({ ...wire.copyGraph(graph), period_ms: Number(periodIn.value) || 0 }, undefined));
      props.append(h("h3", {}, "Wiring"), h("label", {}, "Name ", nameIn), h("label", {}, "Scan every ", periodIn, " ms"), h("p", { class: "muted" }, "Select a node or a connection to edit it."));
      return;
    }
    if ("edge" in selection) {
      const [from, to] = selection.edge;
      props.append(h("h3", {}, "Connection"), h("p", {}, `${from} → ${to}`), h("button", { onclick: removeSelection }, "Remove connection"));
      return;
    }
    const id = selection.node;
    const node = graph.nodes.find((n) => n.id === id);
    if (!node) return;
    props.append(h("h3", {}, wire.isFunction(node.kind) ? "Function" : `${node.kind[0]!.toUpperCase()}${node.kind.slice(1)}`));
    if (node.kind === "channel") {
      const nameIn = h("input", { type: "text", value: id, maxlength: 24, spellcheck: false });
      nameIn.addEventListener("change", () => {
        const to = nameIn.value.trim();
        const next = wire.renameNode(graph, id, to);
        if (next === graph) {
          statusEl.textContent = to && to !== id ? `${to} is already taken.` : "";
          nameIn.value = id;
        } else change(next, { node: to });
      });
      props.append(h("label", {}, "Name ", nameIn));
    }
    // The kind's own settings, as mwctl lists them.
    const spec = kinds.find((k) => k.kind === node.kind);
    if (spec?.class === "function") {
      const kindSel = h("select", {});
      for (const k of kinds.filter((x) => x.class === "function")) kindSel.append(h("option", { value: k.kind }, k.kind.toUpperCase()));
      kindSel.value = node.kind;
      kindSel.addEventListener("change", () => change(wire.setFunction(graph, id, kindSel.value), undefined));
      props.append(h("label", {}, "Function ", kindSel), h("span", { class: "muted" }, inputsText(spec)));
    }
    for (const param of spec?.params ?? []) props.append(paramField(node, param));
    if (drawClass(node.kind) !== "output") {
      // Connecting without a pointer: pick the target from a menu.
      const toSel = h("select", { "aria-label": `Connect ${id} to` }, h("option", { value: "" }, "Connect to…"));
      for (const n of graph.nodes) {
        if (n.id !== id && !graph.edges.some(([a, b]) => a === id && b === n.id)) {
          const l = nodeLabel(n);
          toSel.append(h("option", { value: n.id }, `${n.id} (${l.head.split(" ")[0]!.toLowerCase()})`));
        }
      }
      toSel.addEventListener("change", () => {
        if (toSel.value) void connect(id, toSel.value);
      });
      props.append(h("label", {}, toSel));
    }
    const edges = graph.edges.filter(([a, b]) => a === id || b === id);
    if (edges.length > 0) {
      const list = h("ul", { class: "wire-edges" });
      for (const [a, b] of edges) {
        list.append(h("li", {}, `${a} → ${b} `, h("button", { title: "Remove this connection", onclick: () => change(wire.removeEdge(graph, a, b), undefined) }, "×")));
      }
      props.append(list);
    }
    props.append(h("button", { onclick: removeSelection }, "Delete node"));
  }

  // ---- problems
  const issuesEl = h("ul", { class: "issues wire-issues", "aria-live": "polite" });
  const summaryEl = h("p", { class: "muted" });
  function renderIssues() {
    issuesEl.replaceChildren();
    if (graph.nodes.length === 0) {
      summaryEl.textContent = "Add a digital input, a channel and a digital output, connect them, then write the wiring to a board. Or load the example.";
      return;
    }
    const list = latest?.issues ?? [];
    for (const i of list) {
      const item = h("li", { class: i.severity }, `${i.severity === "error" ? "Problem" : "Note"}: ${i.message} `);
      const target: Selection = i.node ? { node: i.node } : i.edge ? { edge: i.edge } : null;
      if (target) item.append(h("button", { class: "link", onclick: () => select(target) }, "show"));
      issuesEl.append(item);
    }
    summaryEl.textContent = !latest ? "" : latest.ok ? `Ready: a ${latest.bytes}-byte payload${chip() ? `, checked for ${chip()}` : ""}.` : "The wiring has problems; nothing can be written yet.";
  }

  // ---- simulate, download, hand-off
  function stopSim() {
    if (!sim) return;
    sim.dispose();
    sim = null;
    simPins = new Map();
    simBtn.textContent = "Simulate";
    simNote.textContent = "";
  }
  function toggleInput(id: string) {
    const node = graph.nodes.find((n) => n.id === id);
    if (!sim || node?.kind !== "input" || node.config?.pin === undefined) return;
    sim.setInput(node.config.pin, !(simPins.get(node.config.pin)?.high ?? false));
  }
  const simNote = h("span", { class: "muted" });
  const simBtn = h(
    "button",
    {
      disabled: true,
      onclick: async () => {
        if (sim) {
          stopSim();
          render();
          return;
        }
        if (!mwctl) return;
        try {
          const wasm = await wire.build(mwctl, graph, chip());
          sim = new SimSession(app.proto, wasm, {
            onPins: (pins) => {
              simPins = new Map(pins);
              render();
            },
            onError: (m) => term(`simulator: ${m}\n`),
          });
          sim.start(10);
          simPins = new Map(sim.pins);
          simBtn.textContent = "Stop simulating";
          simNote.textContent = "Running in the simulator: select an input and press Space, or double-click it, to flip its pin. Lit nodes are high.";
          render();
        } catch (e) {
          term(`simulator: ${errorText(e)}\n`);
        }
      },
    },
    "Simulate",
  );
  nodeLayer.addEventListener("dblclick", (e) => {
    const id = (e.target as Element).closest("[data-node]")?.getAttribute("data-node");
    if (id) toggleInput(id);
  });

  const downloadBtn = h(
    "button",
    {
      disabled: true,
      onclick: async () => {
        if (!mwctl) return;
        const wasm = await wire.build(mwctl, graph, chip());
        const url = URL.createObjectURL(new Blob([wasm.slice()], { type: "application/wasm" }));
        const a = h("a", { href: url, download: `${graph.name || "wire"}.wasm` });
        a.click();
        URL.revokeObjectURL(url);
      },
    },
    "Download .wasm",
  );
  const deviceBtn = h(
    "button",
    {
      disabled: true,
      title: "Hand the payload to the Device tab (signed uploads, console)",
      onclick: async () => {
        if (!mwctl) return;
        app.offerPayload({ name: `${graph.name || "wire"}.wasm`, wasm: await wire.build(mwctl, graph, chip()) });
        app.showTab("device");
      },
    },
    "Send to Device tab",
  );

  chipSel.addEventListener("change", () => void refresh());

  // ---- software in the loop: the built payload, every channel traced,
  // run on a virtual clock faster than real time.
  const sil = silPanel({
    proto: app.proto,
    build: async () => {
      if (!mwctl) throw new Error("mwctl is still loading");
      return wire.build(mwctl, wire.withTrace(graph), chip());
    },
  });

  // ---- start: load mwctl, then watch for boards
  const loadNote = h("p", { class: "note" }, "Loading mwctl…");
  void loading.then(
    async (m) => {
      mwctl = m;
      loadNote.hidden = m.canWait;
      loadNote.textContent =
        "This browser cannot let mwctl wait for a board (no JavaScript Promise Integration). The wiring is checked and built here; write it from Chrome or Edge, or download the .wasm.";
      kinds = await wire.kinds(m);
      buildPalette();
      await refresh();
      if (app.serialProblem !== null || !m.canWait) return;
      app.ports.onChange(() => {
        if (!app.ports.current) {
          board = null;
          boardText.textContent = "No board yet. Plug one in, or choose its port.";
          showBoard();
        }
      });
      await watchPorts<SerialPort>(
        navigator.serial,
        (port) => {
          // A board appeared: take its port if none is chosen, and ask.
          if (!app.ports.current) app.ports.adopt(port);
          if (app.ports.current === port && !app.ports.currentOwner) void exclusive(() => checkBoard(true));
        },
        (port) => {
          if (app.ports.current === port) app.ports.forget(port);
        },
      );
    },
    (e) => {
      loadNote.textContent = `Could not load mwctl: ${errorText(e)}. Run npm run build:mwctl.`;
    },
  );

  render();
  return h(
    "section",
    { class: "page wire" },
    h("h2", {}, "Wire"),
    h("p", { class: "muted" }, "Inputs drive named channels, functions of channels drive channels, and channels drive outputs. Channels carry integers (a boolean is 0 or 1). Each node updates at its own multiple of the scan period, in dependency order; a loop needs a DELAY in it."),
    loadNote,
    h("div", { class: "row wire-board" }, boardBadge, boardText, checkBtn, flashBtn, writeBtn),
    palette,
    h("div", { class: "row" }, undoBtn, deleteBtn, exampleBtn, clearBtn, simBtn, downloadBtn, deviceBtn, h("label", {}, "Pin rules: ", chipSel)),
    simNote,
    h("div", { class: "wire-main" }, canvasBox, props),
    statusEl,
    summaryEl,
    issuesEl,
    sil.el,
    h("h3", {}, "As a command line"),
    h("p", { class: "muted" }, "The same wiring for mwctl in a terminal. Edit it and press Apply to redraw the canvas."),
    commandForm,
    commandErr,
    h("h3", {}, "mwctl"),
    h("p", { class: "muted" }, "mwctl runs in this page. Its port is ", h("code", {}, "--port web"), ", the serial port chosen above; try ", h("code", {}, "--port web info"), " or ", h("code", {}, "--help"), "."),
    termForm,
    termEl,
  );
}
