// The Flows view of the Blocks tab: the payload's flows (model/flows.ts),
// each edited two ways over the same pure model.
//
// * Canvas: a hand-written SVG editor (no canvas library: a few hundred
//   lines over the pure model cost less than any graph library's bundle,
//   and every node stays a real focusable element). Nodes are boxes with
//   their input port on top and their output ports along the bottom in
//   run order; drag a node to move it, drag from an output port onto
//   another node to wire it (a port or input that already has a wire gets
//   the new one instead), click a wire to select it. Drag the background
//   to pan; the drawing scrolls inside its box, so the page never scrolls
//   sideways. Zoom is three buttons. Nodes are focusable: arrow keys move
//   the focused node (Shift: further), Delete removes it or the selected
//   wire, Enter selects it.
// * Outline: the same flow as an indented list in run order (flows.ts
//   `outline`), every node a row with its ports as menus ("then ->
//   [nothing | n3 ... | new action ...]"), Earlier / Later to reorder a
//   chain and Delete. It is the keyboard and screen-reader editor and the
//   default on narrow screens (the choice is remembered).
//
// Both share a palette (add a node of each kind; with a node selected, the
// new node is wired to its first free output port), Undo (the last 50
// changes), Delete, and the properties panel of the selected node: a
// keyboard grid (grid.ts, like Pins and States) whose controls are that
// kind's fields: what starts the flow, the action and its parameters, the
// branch condition (a pin compared with a number, or a machine being in a
// state), the repeat count, the number of sequence steps.
//
// The check's problems (protocol-wasm `checkFlows`, run by the page) mark
// their node or wire (error / warning styles, aria-invalid on the node),
// list under the canvas and under the properties, with fixes as buttons.
// While the simulator runs, the nodes of each flow's latest run (from the
// `flow <id>: <node>` log lines, flows.ts `flowTrail`) are highlighted on
// the canvas and in the outline, the last one marked current.
//
// All decisions are elsewhere (flowsModel.ts, flows.ts, the Rust check);
// this file only renders and forwards.

import { ACTION_KINDS, type ActionKind, type ActionRef, CMP_TEXT, type Machine } from "../model/machines";
import {
  EDGES,
  ENTRY_ONS,
  type EntryOn,
  type Flow,
  type FlowFix,
  type FlowIssue,
  type FlowNode,
  MAX_FLOWS,
  NODE_KINDS,
  type NodeKind,
  addNode,
  connect,
  defaultNode,
  disconnect,
  freeNodeId,
  moveNode,
  newFlow,
  outline,
  outputs,
  portText,
  removeNode,
  setFlow,
  setNode,
  wireFrom,
} from "../model/flows";
import type { PinFunc } from "../model/pins";
import { h } from "./dom";
import { focusKeeper, grid } from "./grid";
import {
  ENTRY_TEXT,
  KIND_TEXT,
  NODE_H,
  NODE_W,
  PORT_R,
  SEQUENCE_STEPS,
  applyFlowFix,
  canvasSize,
  condForm,
  condOf,
  condOfForm,
  connectTargets,
  entryOf,
  flowActionOf,
  flowFixText,
  issuesForNode,
  issuesForWire,
  moveEarlier,
  moveLater,
  newNodeSpot,
  nodeLabel,
  nodeTitle,
  portPos,
  record,
  severityOf,
  stateRefs,
  wirePath,
  zoomStep,
} from "./flowsModel";
import { ACTION_KIND_TEXT, INPUT_FUNCS, OUTPUT_FUNCS, type Option, type PinRef, labelOptions, nameOptions } from "./statesModel";

export interface FlowsViewEvents {
  onChange(flows: Flow[]): void;
  /** A fix adds an event to a machine. */
  onMachines(machines: Machine[]): void;
  /** A fix asks for a pin-table row. */
  onAddPin(label: string, func: PinFunc): void;
}

export interface FlowsView {
  el: HTMLElement;
  /** `trail`: each flow's latest run in the simulator, by flow id. */
  render(flows: readonly Flow[], issues: readonly FlowIssue[], machines: readonly Machine[], pins: readonly PinRef[], trail: ReadonlyMap<string, readonly string[]> | null): void;
}

const SVG = "http://www.w3.org/2000/svg";
function s<K extends keyof SVGElementTagNameMap>(tag: K, attrs: Record<string, string | number> = {}, ...kids: (Node | string | null)[]): SVGElementTagNameMap[K] {
  const el = document.createElementNS(SVG, tag);
  for (const [k, v] of Object.entries(attrs)) el.setAttribute(k, String(v));
  for (const c of kids) if (c !== null) el.append(c);
  return el;
}

function option(value: string, text: string, selected: boolean): HTMLOptionElement {
  const o = h("option", { value }, text);
  o.selected = selected;
  return o;
}
function select(opts: readonly Option[], value: string): HTMLSelectElement {
  return h("select", {}, ...opts.map(([v, t]) => option(v, t, v === value)));
}
function clip(t: string, n = 24): string {
  return t.length > n ? `${t.slice(0, n - 1)}…` : t;
}

const MODE_KEY = "flows-mode";
type Mode = "canvas" | "outline";
type Selection = { node: string } | { wire: number } | null;

export function flowsView(ev: FlowsViewEvents): FlowsView {
  let flows: readonly Flow[] = [];
  let issues: readonly FlowIssue[] = [];
  let machines: readonly Machine[] = [];
  let pins: readonly PinRef[] = [];
  let trail: ReadonlyMap<string, readonly string[]> | null = null;
  let at = 0;
  let sel: Selection = null;
  let zoom = 1;
  let past: (readonly Flow[])[] = [];
  /** The flows this view last sent; anything else came from outside
   * (an example, an import) and clears the undo history. */
  let sent: readonly Flow[] | null = null;
  let mode: Mode = (() => {
    try {
      const v = localStorage.getItem(MODE_KEY);
      if (v === "canvas" || v === "outline") return v;
    } catch {
      /* storage unavailable */
    }
    return window.matchMedia?.("(max-width: 700px)").matches ? "outline" : "canvas";
  })();

  const root = h("div", { class: "flows-view" });
  const keeper = focusKeeper(root);
  const props = grid("flow-prop", ["value"] as const, "value");

  const flow = (): Flow | undefined => flows[at];
  function change(next: readonly Flow[]) {
    past = record(past, flows);
    sent = next;
    ev.onChange([...next]);
  }
  function changeFlow(f: Flow) {
    change(setFlow(flows, at, f));
  }
  function undo() {
    const prev = past[past.length - 1];
    if (!prev) return;
    past = past.slice(0, -1);
    sent = prev;
    ev.onChange([...prev]);
  }
  function applyFix(fix: FlowFix) {
    const r = applyFlowFix(machines, fix);
    if (!r) return;
    if ("pin" in r) ev.onAddPin(r.pin.label, r.pin.func);
    else ev.onMachines(r.machines);
  }
  function issueItems(list: readonly FlowIssue[], f: Flow): HTMLElement[] {
    return list.map((i) => {
      const where = i.node !== null ? `${f.nodes[i.node]?.id ?? ""}: ` : i.wire !== null ? `wire ${f.wires[i.wire]?.from.node}.${f.wires[i.wire]?.from.port} → ${f.wires[i.wire]?.to.node}: ` : "";
      const li = h("li", { class: i.severity }, h("span", { class: "sev" }, i.severity === "error" ? "Error: " : "Warning: "), `${where}${i.message}`);
      if (i.fix) {
        const fix = i.fix;
        const b = h("button", { type: "button", class: "fix" }, flowFixText(fix, machines));
        b.addEventListener("click", () => applyFix(fix));
        li.append(" ", b);
      }
      if (i.node !== null) {
        const id = f.nodes[i.node]?.id;
        if (id) {
          li.classList.add("link");
          li.addEventListener("click", (e) => {
            if ((e.target as HTMLElement).tagName === "BUTTON") return;
            sel = { node: id };
            keeper.want(`fnode-${id}`, `onode-${id}`);
            draw();
          });
        }
      }
      return li;
    });
  }

  /** Adds a node of `kind`; wired from the selected node's first free
   * output port when there is one. */
  function add(kind: NodeKind) {
    const f = flow();
    if (!f) return;
    const id = freeNodeId(f);
    const spot = newNodeSpot(f);
    const selId = sel && "node" in sel ? sel.node : null;
    const from = selId === null ? undefined : f.nodes.find((n) => n.id === selId);
    const port = from ? outputs(from).find((p) => !wireFrom(f, from.id, p)) : undefined;
    const pos = from && port ? { x: from.x, y: from.y + NODE_H + 64 } : spot;
    let g = addNode(f, defaultNode(kind, id, pos.x, pos.y));
    if (g === f) return;
    if (kind === "action") g = setNode(g, id, { ...defaultNode(kind, id, 0, 0), props: flowActionOf(machines.length ? "raise" : "log", { kind: "log", text: "" }, pins, machines) } as FlowNode);
    if (from && port) g = connect(g, from.id, port, id);
    sel = { node: id };
    keeper.want(mode === "canvas" ? `fnode-${id}` : `onode-${id}`);
    changeFlow(g);
  }
  function removeSelected() {
    const f = flow();
    if (!f || !sel) return;
    const next = "node" in sel ? removeNode(f, sel.node) : disconnect(f, sel.wire);
    sel = null;
    changeFlow(next);
  }

  // ---- toolbar

  function toolbar(f: Flow | undefined): HTMLElement {
    const bar = h("div", { class: "row flow-bar" });
    if (flows.length > 0) {
      const pick = select(
        flows.map((x, i): Option => [String(i), x.id || `flow ${i + 1}`]),
        String(at),
      );
      pick.setAttribute("aria-label", "Flow");
      pick.id = "flow-pick";
      pick.addEventListener("change", () => {
        at = Number(pick.value);
        sel = null;
        draw();
      });
      bar.append(h("label", {}, "Flow ", pick));
    }
    const addFlow = h("button", { type: "button", disabled: flows.length >= MAX_FLOWS }, "New flow");
    addFlow.addEventListener("click", () => {
      at = flows.length;
      sel = null;
      change([...flows, newFlow(flows)]);
    });
    bar.append(addFlow);
    if (f) {
      const del = h("button", { type: "button" }, "Delete flow");
      del.addEventListener("click", () => {
        const next = flows.filter((_, i) => i !== at);
        at = Math.max(0, Math.min(at, next.length - 1));
        sel = null;
        change(next);
      });
      const id = h("input", { type: "text", value: f.id, size: 12, id: "flow-id", "aria-label": "Flow name (letters, digits, _)" });
      id.addEventListener("change", () => changeFlow({ ...f, id: id.value.trim() }));
      const note = h("input", { type: "text", value: f.name, size: 22, id: "flow-note", "aria-label": "Flow description" });
      note.addEventListener("change", () => changeFlow({ ...f, name: note.value }));
      bar.append(h("label", {}, "Name ", id), h("label", {}, "About ", note), del);
    }
    return bar;
  }

  function editBar(f: Flow): HTMLElement {
    const modes = h("div", { class: "seg", role: "group", "aria-label": "Edit as" });
    for (const [m, text] of [
      ["canvas", "Canvas"],
      ["outline", "Outline"],
    ] as const) {
      const b = h("button", { type: "button", "aria-pressed": String(mode === m) }, text);
      b.addEventListener("click", () => {
        mode = m;
        try {
          localStorage.setItem(MODE_KEY, m);
        } catch {
          /* storage unavailable */
        }
        draw();
      });
      modes.append(b);
    }
    const palette = h("div", { class: "seg palette", role: "group", "aria-label": "Add a node" });
    for (const k of NODE_KINDS) {
      const b = h("button", { type: "button", title: `Add ${KIND_TEXT[k].toLowerCase()}` }, `+ ${KIND_TEXT[k]}`);
      b.addEventListener("click", () => add(k));
      palette.append(b);
    }
    const undoBtn = h("button", { type: "button", disabled: past.length === 0 }, "Undo");
    undoBtn.addEventListener("click", undo);
    const delBtn = h("button", { type: "button", disabled: !sel }, sel && "wire" in sel ? "Delete wire" : "Delete node");
    delBtn.addEventListener("click", removeSelected);
    const zooms = h("div", { class: "seg", role: "group", "aria-label": "Zoom" });
    if (mode === "canvas") {
      for (const [dir, text, label] of [
        [-1, "−", "Zoom out"],
        [0, `${Math.round(zoom * 100)}%`, "Reset zoom"],
        [1, "+", "Zoom in"],
      ] as const) {
        const b = h("button", { type: "button", "aria-label": label }, text);
        b.addEventListener("click", () => {
          zoom = zoomStep(zoom, dir);
          draw();
        });
        zooms.append(b);
      }
    }
    return h("div", { class: "row flow-bar" }, modes, palette, undoBtn, delBtn, f.nodes.length > 0 ? zooms : null);
  }

  // ---- canvas

  function canvas(f: Flow): HTMLElement {
    const live = new Set(trail?.get(f.id) ?? []);
    const current = trail?.get(f.id)?.at(-1);
    const size = canvasSize(f);
    const svg = s("svg", {
      class: "flow-svg",
      width: Math.round(size.w * zoom),
      height: Math.round(size.h * zoom),
      viewBox: `0 0 ${size.w} ${size.h}`,
      role: "group",
      "aria-label": `Flow ${f.id} canvas. The Outline view edits the same flow with a keyboard or screen reader.`,
    });
    const byId = new Map(f.nodes.map((n) => [n.id, n] as const));
    const wiresG = s("g", { class: "wires" });
    f.wires.forEach((w, i) => {
      const a = byId.get(w.from.node);
      const b = byId.get(w.to.node);
      if (!a || !b) return;
      const d = wirePath(portPos(a, w.from.port), portPos(b, "in"));
      const sev = severityOf(issuesForWire(issues, at, i));
      const on = live.has(w.from.node) && live.has(w.to.node);
      const cls = ["fwire", sev ?? "", on ? "live" : "", sel && "wire" in sel && sel.wire === i ? "sel" : ""].filter(Boolean).join(" ");
      wiresG.append(
        s("path", { class: cls, d }),
        s("path", { class: "fwire-hit", d, "data-wire": i }, s("title", {}, `${w.from.node} ${portText(w.from.port)} → ${w.to.node}`)),
      );
    });
    const nodesG = s("g", { class: "nodes" });
    f.nodes.forEach((n, ni) => {
      const sev = severityOf(issuesForNode(issues, at, ni));
      const selected = sel && "node" in sel && sel.node === n.id;
      const cls = ["fnode", `k-${n.kind}`, sev ?? "", live.has(n.id) ? "live" : "", current === n.id ? "current" : "", selected ? "sel" : ""].filter(Boolean).join(" ");
      const g = s("g", {
        class: cls,
        id: `fnode-${n.id}`,
        "data-node": n.id,
        transform: `translate(${n.x} ${n.y})`,
        tabindex: 0,
        role: "button",
        "aria-pressed": String(!!selected),
        "aria-label": `${nodeLabel(n)}${sev ? ` (${sev})` : ""}${current === n.id ? " (running)" : ""}`,
      });
      if (sev === "error") g.setAttribute("aria-invalid", "true");
      g.append(
        s("title", {}, nodeLabel(n)),
        s("rect", { width: NODE_W, height: NODE_H, rx: 8 }),
        s("text", { x: 10, y: 18, class: "fkind" }, `${KIND_TEXT[n.kind]} · ${n.id}`),
        s("text", { x: 10, y: 37, class: "ftitle" }, clip(nodeTitle(n))),
      );
      if (n.kind !== "entry") g.append(s("circle", { class: "port in", cx: NODE_W / 2, cy: 0, r: PORT_R }));
      for (const p of outputs(n)) {
        const pos = portPos({ ...n, x: 0, y: 0 } as FlowNode, p);
        g.append(
          s("text", { x: pos.x, y: NODE_H - 12, class: "fport", "text-anchor": "middle" }, portText(p)),
          s("circle", { class: `port out${wireFrom(f, n.id, p) ? " wired" : ""}`, cx: pos.x, cy: pos.y, r: PORT_R, "data-port": p }, s("title", {}, `${portText(p)}: drag onto a node to wire it`)),
        );
      }
      nodesG.append(g);
    });
    const temp = s("path", { class: "fwire temp", d: "" });
    svg.append(wiresG, nodesG, temp);

    const box = h("div", { class: "flow-canvas" }, svg);
    attachPointer(svg, box, temp, f);
    svg.addEventListener("keydown", (e) => {
      const g = (e.target as Element).closest?.("[data-node]");
      const id = g?.getAttribute("data-node");
      const n = id ? f.nodes.find((x) => x.id === id) : undefined;
      if (!n) {
        if ((e.key === "Delete" || e.key === "Backspace") && sel) {
          e.preventDefault();
          removeSelected();
        }
        return;
      }
      const step = e.shiftKey ? 40 : 8;
      const d = { ArrowLeft: [-step, 0], ArrowRight: [step, 0], ArrowUp: [0, -step], ArrowDown: [0, step] }[e.key];
      if (d) {
        e.preventDefault();
        keeper.want(`fnode-${n.id}`);
        changeFlow(moveNode(f, n.id, n.x + (d[0] ?? 0), n.y + (d[1] ?? 0)));
      } else if (e.key === "Enter" || e.key === " ") {
        e.preventDefault();
        sel = { node: n.id };
        keeper.want(`fnode-${n.id}`);
        draw();
      } else if (e.key === "Delete" || e.key === "Backspace") {
        e.preventDefault();
        sel = { node: n.id };
        removeSelected();
      }
    });
    return box;
  }

  /** Dragging: a node moves it, an output port draws a wire, the
   * background pans the box. Clicks select. */
  function attachPointer(svg: SVGSVGElement, box: HTMLElement, temp: SVGPathElement, f: Flow) {
    type Drag =
      | { kind: "node"; id: string; x0: number; y0: number; px: number; py: number; moved: boolean }
      | { kind: "wire"; from: string; port: string; a: { x: number; y: number } }
      | { kind: "pan"; px: number; py: number; sl: number; st: number; moved: boolean };
    let drag: Drag | null = null;
    const local = (e: PointerEvent) => {
      const r = svg.getBoundingClientRect();
      return { x: (e.clientX - r.left) / zoom, y: (e.clientY - r.top) / zoom };
    };
    svg.addEventListener("pointerdown", (e) => {
      const t = e.target as Element;
      const portEl = t.closest("[data-port]");
      const nodeEl = t.closest("[data-node]");
      const id = nodeEl?.getAttribute("data-node") ?? "";
      const n = f.nodes.find((x) => x.id === id);
      if (portEl && n) {
        const port = portEl.getAttribute("data-port") ?? "next";
        drag = { kind: "wire", from: n.id, port, a: portPos(n, port) };
      } else if (n) {
        drag = { kind: "node", id: n.id, x0: n.x, y0: n.y, px: e.clientX, py: e.clientY, moved: false };
      } else {
        const hit = t.closest("[data-wire]");
        if (hit) {
          sel = { wire: Number(hit.getAttribute("data-wire")) };
          draw();
          return;
        }
        drag = { kind: "pan", px: e.clientX, py: e.clientY, sl: box.scrollLeft, st: box.scrollTop, moved: false };
      }
      svg.setPointerCapture(e.pointerId);
      e.preventDefault();
    });
    svg.addEventListener("pointermove", (e) => {
      if (!drag) return;
      if (drag.kind === "wire") {
        temp.setAttribute("d", wirePath(drag.a, local(e)));
      } else if (drag.kind === "node") {
        const dx = (e.clientX - drag.px) / zoom;
        const dy = (e.clientY - drag.py) / zoom;
        if (Math.abs(dx) + Math.abs(dy) > 3) drag.moved = true;
        const g = svg.querySelector(`[data-node="${drag.id}"]`);
        g?.setAttribute("transform", `translate(${Math.max(0, drag.x0 + dx)} ${Math.max(0, drag.y0 + dy)})`);
      } else {
        const dx = e.clientX - drag.px;
        const dy = e.clientY - drag.py;
        if (Math.abs(dx) + Math.abs(dy) > 3) drag.moved = true;
        box.scrollLeft = drag.sl - dx;
        box.scrollTop = drag.st - dy;
      }
    });
    const end = (e: PointerEvent) => {
      const d = drag;
      drag = null;
      if (!d) return;
      if (d.kind === "wire") {
        temp.setAttribute("d", "");
        const target = document.elementFromPoint(e.clientX, e.clientY)?.closest("[data-node]")?.getAttribute("data-node");
        if (target && target !== d.from) {
          const g = connect(f, d.from, d.port, target);
          if (g !== f) {
            sel = { node: target };
            changeFlow(g);
          }
        }
      } else if (d.kind === "node") {
        if (d.moved) {
          const dx = (e.clientX - d.px) / zoom;
          const dy = (e.clientY - d.py) / zoom;
          sel = { node: d.id };
          keeper.want(`fnode-${d.id}`);
          changeFlow(moveNode(f, d.id, d.x0 + dx, d.y0 + dy));
        } else {
          sel = { node: d.id };
          keeper.want(`fnode-${d.id}`);
          draw();
        }
      } else if (!d.moved && sel) {
        sel = null;
        draw();
      }
    };
    svg.addEventListener("pointerup", end);
    svg.addEventListener("pointercancel", () => {
      drag = null;
      temp.setAttribute("d", "");
      draw();
    });
  }

  // ---- outline

  function outlineView(f: Flow): HTMLElement {
    const live = new Set(trail?.get(f.id) ?? []);
    const current = trail?.get(f.id)?.at(-1);
    const list = h("ol", { class: "flow-outline", "aria-label": `Flow ${f.id} in run order` });
    for (const r of outline(f)) {
      const n = r.node;
      const ni = f.nodes.indexOf(n);
      const sev = severityOf(issuesForNode(issues, at, ni));
      const selected = !!sel && "node" in sel && sel.node === n.id;
      const via = r.via ? ` (${portText(r.via.port)} of ${r.via.node})` : "";
      const pick = h(
        "button",
        { type: "button", class: "onode", id: `onode-${n.id}`, "aria-pressed": String(selected) },
        h("span", { class: "fkind" }, `${KIND_TEXT[n.kind]} · ${n.id}`),
        " ",
        nodeTitle(n),
        h("span", { class: "muted" }, via),
        r.attached ? null : h("span", { class: "warning" }, " (not reached from the entry)"),
        current === n.id ? h("span", { class: "visually-hidden" }, " (running)") : null,
      );
      if (sev === "error") pick.setAttribute("aria-invalid", "true");
      pick.addEventListener("click", () => {
        sel = { node: n.id };
        keeper.want(`onode-${n.id}`);
        draw();
      });
      const ports = outputs(n).map((p) => {
        const w = wireFrom(f, n.id, p);
        const targets = connectTargets(f, n.id);
        const opts: Option[] = [["", "(nothing)"], ...targets.map((t): Option => [t.id, `${t.id}: ${clip(nodeTitle(t), 32)}`]), ...NODE_KINDS.filter((k) => k !== "entry").map((k): Option => [`+${k}`, `new ${KIND_TEXT[k].toLowerCase()}`])];
        if (w && !targets.some((t) => t.id === w.w.to.node)) opts.splice(1, 0, [w.w.to.node, w.w.to.node]);
        const m = select(opts, w?.w.to.node ?? "");
        m.id = `oport-${n.id}-${p}`;
        m.addEventListener("change", () => {
          const v = m.value;
          keeper.want(`oport-${n.id}-${p}`);
          if (v === "") {
            if (w) changeFlow(disconnect(f, w.i));
          } else if (v.startsWith("+")) {
            const kind = v.slice(1) as NodeKind;
            const id = freeNodeId(f);
            const pos = { x: n.x, y: n.y + NODE_H + 64 };
            let g = addNode(f, defaultNode(kind, id, pos.x, pos.y));
            if (kind === "action") g = setNode(g, id, { ...defaultNode(kind, id, pos.x, pos.y), props: flowActionOf(machines.length ? "raise" : "log", { kind: "log", text: "" }, pins, machines) } as FlowNode);
            sel = { node: id };
            changeFlow(connect(g, n.id, p, id));
          } else changeFlow(connect(f, n.id, p, v));
        });
        return h("label", { class: "oport" }, `${portText(p)} → `, m);
      });
      const earlier = h("button", { type: "button", id: `oup-${n.id}`, "aria-label": `Run ${n.id} earlier` }, "Earlier");
      earlier.addEventListener("click", () => {
        keeper.want(`oup-${n.id}`);
        changeFlow(moveEarlier(f, n.id));
      });
      const later = h("button", { type: "button", id: `odown-${n.id}`, "aria-label": `Run ${n.id} later` }, "Later");
      later.addEventListener("click", () => {
        keeper.want(`odown-${n.id}`);
        changeFlow(moveLater(f, n.id));
      });
      const del = h("button", { type: "button", "aria-label": `Delete ${n.id}` }, "Delete");
      del.addEventListener("click", () => {
        sel = { node: n.id };
        removeSelected();
      });
      const li = h("li", { class: ["orow", live.has(n.id) ? "live" : "", current === n.id ? "current" : "", sev ?? ""].filter(Boolean).join(" ") }, pick, h("div", { class: "row oports" }, ...ports, n.kind === "entry" ? null : earlier, n.kind === "entry" ? null : later, del));
      li.style.setProperty("--depth", String(r.depth));
      if (current === n.id) li.setAttribute("aria-current", "step");
      list.append(li);
    }
    return list;
  }

  // ---- properties of the selected node

  function properties(f: Flow, n: FlowNode): HTMLElement {
    const ni = f.nodes.indexOf(n);
    const mine = issuesForNode(issues, at, ni);
    const described = mine.length > 0 ? props.issuesId(0) : null;
    const rows: [string, HTMLElement][] = [];
    const set = (node: FlowNode) => changeFlow(setNode(f, n.id, node));
    const kind = select(
      NODE_KINDS.map((k): Option => [k, KIND_TEXT[k]]),
      n.kind,
    );
    kind.addEventListener("change", () => set(defaultNode(kind.value as NodeKind, n.id, n.x, n.y)));
    rows.push(["Kind", kind]);
    const text = (value: string, onSet: (v: string) => void, type = "text") => {
      const el = h("input", { type, value });
      el.addEventListener("change", () => onSet(el.value));
      return el;
    };
    const num = (value: number, onSet: (v: number) => void) => text(String(value), (v) => onSet(Math.trunc(Number(v)) || 0), "number");
    switch (n.kind) {
      case "entry": {
        const on = select(
          ENTRY_ONS.map((o): Option => [o, ENTRY_TEXT[o]]),
          n.props.on,
        );
        on.addEventListener("change", () => set({ ...n, props: entryOf(on.value as EntryOn, n.props, pins, machines) }));
        rows.push(["Starts", on]);
        const p = n.props;
        if (p.on === "every") rows.push(["Every (ms)", num(p.ms, (ms) => set({ ...n, props: { ...p, ms } }))]);
        if (p.on === "message") rows.push(["Key", text(p.key, (key) => set({ ...n, props: { ...p, key: key.trim() } }))]);
        if (p.on === "pin-edge") {
          const pin = select(labelOptions(pins, INPUT_FUNCS, p.label, "not an input"), p.label);
          pin.addEventListener("change", () => set({ ...n, props: { ...p, label: pin.value } }));
          const edge = select(
            EDGES.map((e): Option => [e, e === "rise" ? "rising (press, for a pull-down)" : e === "fall" ? "falling (press, for a pull-up)" : "both"]),
            p.edge,
          );
          edge.addEventListener("change", () => set({ ...n, props: { ...p, edge: edge.value as (typeof EDGES)[number] } }));
          rows.push(["Pin", pin], ["Edge", edge]);
        }
        if (p.on === "entered") {
          const st = select(nameOptions(stateRefs(machines), p.state, "not in States"), p.state);
          st.addEventListener("change", () => set({ ...n, props: { on: "entered", state: st.value } }));
          rows.push(["State", st]);
        }
        break;
      }
      case "action": {
        const a = n.props;
        const k = select(
          ACTION_KINDS.map((x): Option => [x, ACTION_KIND_TEXT[x]]),
          a.kind,
        );
        k.addEventListener("change", () => set({ ...n, props: flowActionOf(k.value as ActionKind, a, pins, machines) }));
        rows.push(["Does", k]);
        const put = (next: ActionRef) => set({ ...n, props: next });
        if (a.kind === "write" || a.kind === "toggle") {
          const pin = select(labelOptions(pins, OUTPUT_FUNCS, a.label, "not an output"), a.label);
          pin.addEventListener("change", () => put({ ...a, label: pin.value }));
          rows.push(["Pin", pin]);
          if (a.kind === "write") {
            const lv = select(
              [
                ["1", "high"],
                ["0", "low"],
              ],
              String(a.level),
            );
            lv.addEventListener("change", () => put({ ...a, level: lv.value === "1" ? 1 : 0 }));
            rows.push(["Level", lv]);
          }
        }
        if (a.kind === "publish") rows.push(["Key", text(a.key, (key) => put({ ...a, key: key.trim() }))], ["Text", text(a.text, (t) => put({ ...a, text: t }))]);
        if (a.kind === "log") rows.push(["Text", text(a.text, (t) => put({ ...a, text: t }))]);
        if (a.kind === "raise") {
          const m = select(nameOptions(machines.map((x) => x.id), a.machine ?? "", "not in States"), a.machine ?? "");
          m.addEventListener("change", () => put(flowActionOf("raise", { ...a, machine: m.value }, pins, machines)));
          const evs = machines.find((x) => x.id === a.machine)?.events.map((e) => e.name) ?? [];
          const e = select(nameOptions(evs, a.event, "not an event"), a.event);
          e.addEventListener("change", () => put({ ...a, event: e.value }));
          rows.push(["Machine", m], ["Event", e]);
        }
        break;
      }
      case "branch": {
        const c = condForm(n.props.cond);
        const form = select(
          [
            ["pin", "a pin compared with a number"],
            ["state", "a machine is in a state"],
            ...(c.form === "other" ? ([["other", `as written: ${clip(c.text, 30)}`]] as Option[]) : []),
          ],
          c.form,
        );
        form.addEventListener("change", () => {
          if (form.value === "pin" || form.value === "state") set({ ...n, props: { cond: condOfForm(form.value, c, pins, machines) } });
        });
        rows.push(["Condition", form]);
        if (c.form === "pin") {
          const pin = select(labelOptions(pins, [...INPUT_FUNCS, ...OUTPUT_FUNCS], c.label, "not a GPIO"), c.label);
          pin.addEventListener("change", () => set({ ...n, props: { cond: condOf({ ...c, label: pin.value }) } }));
          const op = select(
            (["eq", "ne"] as const).map((o): Option => [o, CMP_TEXT[o]]),
            c.op,
          );
          op.addEventListener("change", () => set({ ...n, props: { cond: condOf({ ...c, op: op.value === "ne" ? "ne" : "eq" }) } }));
          rows.push(["Pin", pin], ["Is", op], ["Value", num(c.value, (value) => set({ ...n, props: { cond: condOf({ ...c, value }) } }))]);
        } else if (c.form === "state") {
          const st = select(nameOptions(stateRefs(machines), c.ref, "not in States"), c.ref);
          st.addEventListener("change", () => set({ ...n, props: { cond: condOf({ form: "state", ref: st.value }) } }));
          rows.push(["State", st]);
        }
        break;
      }
      case "repeat":
        rows.push(["Times", num(n.props.times, (times) => set({ ...n, props: { times } }))]);
        break;
      case "sequence": {
        const st = select(
          SEQUENCE_STEPS.map((k): Option => [String(k), `${k} steps`]),
          String(n.props.steps),
        );
        st.addEventListener("change", () => set({ ...n, props: { steps: Number(st.value) } }));
        rows.push(["Steps", st]);
        break;
      }
    }
    const table = h(
      "table",
      { class: "grid flow-props" },
      h("caption", {}, `Node ${n.id}`),
      h("tbody", {}, ...rows.map(([label, el], r) => h("tr", {}, h("th", { scope: "row" }, label), h("td", {}, props.control(r, "value", el, `${label} of ${n.id}`, described))))),
    );
    if (mine.length > 0) table.append(props.issueRow(0, issueItems(mine, f), "issue-row", 2));
    for (const r of rows) if (severityOf(mine) === "error") r[1].setAttribute("aria-invalid", "true");
    props.attachKeys(table, () => rows.length);
    return h("div", { class: "flow-props-box" }, table);
  }

  function emptyState(): HTMLElement {
    return h(
      "div",
      { class: "empty" },
      h("p", {}, "A flow is a small picture of what happens when something starts it: on start, every N ms, on a message, on a pin edge, or when a state machine enters a state. Wire actions, branches and repeats under it; it compiles to the same bounded code as Blocks."),
      h("p", { class: "muted" }, 'Press "New flow", or load "Button toggles LED mode, as flows" from Examples.'),
    );
  }

  function draw() {
    keeper.remember();
    const f = flow();
    const parts: (HTMLElement | null)[] = [toolbar(f)];
    if (!f) parts.push(emptyState());
    else {
      parts.push(editBar(f), mode === "canvas" ? canvas(f) : outlineView(f));
      const selId = sel && "node" in sel ? sel.node : null;
      const node = selId === null ? undefined : f.nodes.find((n) => n.id === selId);
      if (sel && "node" in sel && !node) sel = null;
      parts.push(node ? properties(f, node) : h("p", { class: "muted" }, mode === "canvas" ? "Select a node to edit it; drag from a port at its bottom onto another node to wire them." : "Select a node to edit it; each port's menu chooses what runs next."));
      const all = issues.filter((i) => i.flow === at);
      if (all.length > 0) parts.push(h("h4", {}, "Problems"), h("ul", { class: "issues", id: "flow-issues" }, ...issueItems(all, f)));
    }
    root.replaceChildren(...parts.filter((x): x is HTMLElement => x !== null));
    keeper.restore();
  }

  return {
    el: root,
    render(fs, is, ms, ps, tr) {
      // A change this view did not send (an example, an import, a fix)
      // starts a new undo history.
      const same = (a: readonly Flow[] | null) => a !== null && JSON.stringify(a) === JSON.stringify(fs);
      if (!same(sent) && !same(flows)) past = [];
      sent = null;
      flows = fs;
      issues = is;
      machines = ms;
      pins = ps;
      trail = tr;
      at = Math.max(0, Math.min(at, fs.length - 1));
      draw();
    },
  };
}
