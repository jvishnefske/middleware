// The Recipe view of the Blocks tab (design.md Q18, Q19): the plant's
// master recipes, edited two ways over the same pure model, bound to
// units and run in the browser.
//
// * Canvas: hand-written SVG like the Flows canvas (and its styles): one
//   lane per unit procedure (the header names it and its unit class),
//   steps as boxes in their lane (phase class and arguments), transitions
//   as bars between the steps they join, with a double line for a
//   parallel divergence (several steps start) or convergence (several
//   must finish). Drag a step up or down to move it, sideways into another
//   lane to move it there; drag from a step's bottom port onto another
//   step for "after this comes that" (model/recipe.ts `connectSteps`,
//   which extends an existing transition into a divergence or
//   convergence and refuses a loop or a choice, saying why). Steps are
//   focusable: Up/Down move, Left/Right change lane, Enter selects,
//   Delete removes. Click a bar or a lane header to select it.
// * Outline: the steps in run order (model/recipe.ts `runOrder`), each a
//   row with what follows it and a menu to add a next step; the keyboard
//   and screen-reader editor and the default on narrow screens.
// * Properties of the selection: a step's id, name, operation, unit
//   procedure, phase class and one number field per class parameter with
//   the class's range (min/max, the range in its description,
//   aria-invalid when the check says it is out of range); a transition's
//   steps on each side with "take off"; a lane's id, name and unit class.
// * Problems: recipe-core's check (protocol-wasm `checkRecipe`) marks the
//   step, bar or lane, and lists under the chart with one-click fixes.
// * Allocation: per unit "online (simulated)" (default on), and whether a
//   unit's bound node is the board the Device tab sees, and whether that
//   board can serve phases (its `sys/info` capability bit; otherwise the
//   plain-language reason and the unit runs simulated); `allocateRecipe`
//   as one row per unit (what it runs, or why it cannot).
// * Run: a unit bound to the Device tab's board, when the board serves
//   phases, runs on the board (Q23: its payload uploaded from the Device
//   tab, commands over Web Serial, its `/state` reports fed back); every
//   other unit is compiled (the generated phase servers) and loaded on a
//   SimBus session, each analog input of the pin tables set to the level
//   in its field (sim/net.ts MixedNet joins both). recipe-core's engine
//   (sim/recipeRun.ts) runs the recipe. Start, Hold, Restart, Stop, Abort
//   and Reset are enabled exactly where the phase model accepts them in
//   the procedure's state (`operatorButtons`); Lose / Find take a unit
//   off the network and back (a board: stop and resume talking to it;
//   pulling its cable does the same by itself). Step status and phase
//   state show on the canvas and in the outline; the trace lists what
//   happened.
//
// All decisions are elsewhere (recipeModel.ts, model/recipe.ts, the Rust
// check, allocation and engine); this file renders and forwards.

import { type PlantModel, allocationUnits } from "../model/plant";
import { addRecipe, removeRecipe, setRecipe } from "../model/plantEdit";
import type { PhaseClass, RecipeIssue } from "../model/procedural";
import {
  type MasterRecipe,
  addStep,
  addUnitProcedure,
  applyRecipeFix,
  connectSteps,
  detach,
  incoming,
  moveStep,
  outgoing,
  removeStep,
  removeTransition,
  removeUnitProcedure,
  renameStep,
  runOrder,
  setArg,
  setStepClass,
  setStepLane,
  setStepText,
  setUnitProcedure,
} from "../model/recipe";
import type { Protocol, StepStatus } from "../protocol/wasm";
import { canServe, serveProblem } from "../device/requester";
import { SimBus } from "../sim/bus";
import { MixedNet } from "../sim/net";
import type { ConnectedBoard } from "./app";
import { RecipeRunner, buildUnit, operatorButtons } from "../sim/recipeRun";
import { realClock } from "../sim/session";
import { errorText, h } from "./dom";
import { wirePath, zoomStep } from "./flowsModel";
import { focusKeeper } from "./grid";
import {
  BAR_H,
  LANE_HEAD,
  LANE_W,
  STEP_H,
  STEP_W,
  allocationRows,
  argIssues,
  argsText,
  barGeom,
  chartSize,
  fixText,
  issuesAt,
  laneAt,
  laneX,
  rangeText,
  runSignature,
  severityOf,
  statusClass,
  stepBoxes,
  stepLabel,
  stepPort,
} from "./recipeModel";

export interface RecipeViewEvents {
  onChange(plant: PlantModel): void;
}

export interface RecipeView {
  el: HTMLElement;
  /** `deviceNode`: the node id of the board the Device tab sees;
   * `board`: that board as a unit link (runs a unit bound to it there). */
  render(plant: PlantModel, deviceNode: string | null, board?: ConnectedBoard | null): void;
  /** The check's problems of every recipe (for the tab text). */
  issues(plant: PlantModel): RecipeIssue[];
}

const SVG = "http://www.w3.org/2000/svg";
function s<K extends keyof SVGElementTagNameMap>(tag: K, attrs: Record<string, string | number> = {}, ...kids: (Node | string | null)[]): SVGElementTagNameMap[K] {
  const el = document.createElementNS(SVG, tag);
  for (const [k, v] of Object.entries(attrs)) el.setAttribute(k, String(v));
  for (const c of kids) if (c !== null) el.append(c);
  return el;
}
function clip(t: string, n = 26): string {
  return t.length > n ? `${t.slice(0, n - 1)}…` : t;
}
function select(opts: readonly (readonly [string, string])[], value: string, label: string, id?: string): HTMLSelectElement {
  const el = h("select", { "aria-label": label, id });
  for (const [v, t] of opts) {
    const o = h("option", { value: v }, t);
    o.selected = v === value;
    el.append(o);
  }
  return el;
}

const MODE_KEY = "recipe-mode";
type Mode = "canvas" | "outline";
type Selection = { step: string } | { transition: number } | { lane: number } | null;

interface Live {
  recipe: number;
  bus: SimBus;
  net: MixedNet;
  runner: RecipeRunner;
  sig: string;
}

export function recipeView(proto: Protocol, ev: RecipeViewEvents): RecipeView {
  let plant: PlantModel | null = null;
  let deviceNode: string | null = null;
  let board: ConnectedBoard | null = null;
  /** The board `u` runs on: bound to the Device tab's board, which serves. */
  const onBoard = (u: { node: string | null }): ConnectedBoard | null =>
    board && u.node && board.requester.node === u.node.toLowerCase() && canServe(board.info) && board.requester.connected ? board : null;
  let at = 0;
  let sel: Selection = null;
  let zoom = 1;
  let past: MasterRecipe[] = [];
  let status = "";
  /** Units taken offline for allocation (simulated). */
  const offline = new Set<string>();
  /** Analog input levels for the run, by `unit/label`. */
  const levels = new Map<string, number>();
  let live: Live | null = null;
  let building = false;
  let runError = "";
  let mode: Mode = (() => {
    try {
      const v = localStorage.getItem(MODE_KEY);
      if (v === "canvas" || v === "outline") return v;
    } catch {
      /* storage unavailable */
    }
    return window.matchMedia?.("(max-width: 700px)").matches ? "outline" : "canvas";
  })();

  const root = h("div", { class: "flows-view recipe-view" });
  const keeper = focusKeeper(root);

  const classes = (): readonly PhaseClass[] => plant?.procedural.classes ?? [];
  const recipe = (): MasterRecipe | undefined => plant?.recipes[at];
  const check = (r: MasterRecipe): RecipeIssue[] => {
    try {
      return proto.checkRecipe(r, classes());
    } catch (e) {
      return [{ at: "recipe", index: null, sub: null, severity: "error", message: errorText(e) }];
    }
  };

  function change(r: MasterRecipe) {
    const cur = recipe();
    if (!plant || !cur || r === cur) return;
    past = [...past, cur].slice(-50);
    ev.onChange(setRecipe(plant, at, r));
  }
  function undo() {
    const prev = past.at(-1);
    if (!plant || !prev) return;
    past = past.slice(0, -1);
    ev.onChange(setRecipe(plant, at, prev));
  }
  function say(t: string) {
    status = t;
    draw();
  }

  // ---- live run state
  const liveFor = (): Live | null => (live && live.recipe === at ? live : null);
  function stepStatus(id: string): StepStatus | undefined {
    return liveFor()?.runner.steps().find((x) => x.id === id)?.status;
  }
  function phaseOf(id: string): string | null {
    const l = liveFor();
    if (!l) return null;
    const b = l.runner.bindings().find((x) => x.step === id);
    const p = b && l.runner.phases().find((x) => x.unit === b.unit && x.em === b.em && x.phase === b.phase);
    return p ? `${b!.unit}/${p.em}.${p.phase} ${p.state ?? "unknown"}${p.values.length ? ` [${p.values.join(", ")}]` : ""}` : null;
  }

  // ---- toolbar

  function toolbar(): HTMLElement {
    const bar = h("div", { class: "row flow-bar" });
    const p = plant!;
    if (p.recipes.length > 0) {
      const pick = select(
        p.recipes.map((r, i) => [String(i), r.name || r.id] as const),
        String(at),
        "Recipe",
        "recipe-pick",
      );
      pick.addEventListener("change", () => {
        at = Number(pick.value);
        sel = null;
        past = [];
        draw();
      });
      bar.append(h("label", {}, "Recipe ", pick));
    }
    const add = h("button", { type: "button", disabled: p.recipes.length >= 4 }, "New recipe");
    add.addEventListener("click", () => {
      at = p.recipes.length;
      sel = null;
      past = [];
      ev.onChange(addRecipe(p));
    });
    bar.append(add);
    const r = recipe();
    if (r) {
      const id = h("input", { type: "text", value: r.id, size: 14, id: "recipe-id", "aria-label": "Recipe id (letters, digits, _)" });
      id.addEventListener("change", () => change({ ...r, id: id.value.trim() }));
      const name = h("input", { type: "text", value: r.name, size: 20, id: "recipe-name", "aria-label": "Recipe name" });
      name.addEventListener("change", () => change({ ...r, name: name.value }));
      const del = h("button", { type: "button" }, "Delete recipe");
      del.addEventListener("click", () => {
        stopRun();
        const next = removeRecipe(p, at);
        at = Math.max(0, Math.min(at, next.recipes.length - 1));
        sel = null;
        past = [];
        ev.onChange(next);
      });
      bar.append(h("label", {}, "Id ", id), h("label", {}, "Name ", name), del);
    }
    return bar;
  }

  function editBar(r: MasterRecipe): HTMLElement {
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
    const addS = h("button", { type: "button", id: "recipe-add-step", disabled: r.steps.length >= 16 || r.unitProcedures.length === 0 }, "+ Step");
    addS.addEventListener("click", () => {
      const cur = sel;
      const after = cur && "step" in cur ? r.steps.find((x) => x.id === cur.step) : undefined;
      const lane = sel && "lane" in sel ? r.unitProcedures[sel.lane]?.id : after?.unitProcedure;
      let { recipe: next, id } = addStep(r, classes(), lane);
      if (!id) return;
      if (after) {
        const c = connectSteps(next, after.id, id);
        next = moveStep(c.recipe, id, after.y + 120);
      }
      sel = { step: id };
      keeper.want(mode === "canvas" ? `rstep-${id}` : `ostep-${id}`);
      change(next);
    });
    const addU = h("button", { type: "button", disabled: r.unitProcedures.length >= 4 }, "+ Unit procedure");
    addU.addEventListener("click", () => {
      const next = addUnitProcedure(r);
      sel = { lane: next.unitProcedures.length - 1 };
      change(next);
    });
    const undoBtn = h("button", { type: "button", disabled: past.length === 0 }, "Undo");
    undoBtn.addEventListener("click", undo);
    const delText = !sel ? "Delete" : "step" in sel ? "Delete step" : "transition" in sel ? "Delete transition" : "Delete unit procedure";
    const delBtn = h("button", { type: "button", disabled: !sel }, delText);
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
    return h("div", { class: "row flow-bar" }, modes, h("div", { class: "seg palette", role: "group", "aria-label": "Add" }, addS, addU), undoBtn, delBtn, mode === "canvas" ? zooms : null);
  }

  function removeSelected() {
    const r = recipe();
    if (!r || !sel) return;
    let next = r;
    if ("step" in sel) next = removeStep(r, sel.step);
    else if ("transition" in sel) next = removeTransition(r, sel.transition);
    else {
      next = removeUnitProcedure(r, sel.lane);
      if (next === r) return say("Move or delete the steps in this unit procedure first.");
    }
    sel = null;
    change(next);
  }

  function connect(r: MasterRecipe, from: string, to: string) {
    const c = connectSteps(r, from, to);
    if (c.refused) return say(`Not connected: ${c.refused}.`);
    status = `${to} now follows ${from}.`;
    sel = { step: to };
    change(c.recipe);
  }

  // ---- canvas

  function canvas(r: MasterRecipe, issues: readonly RecipeIssue[]): HTMLElement {
    const size = chartSize(r);
    const svg = s("svg", {
      class: "flow-svg recipe-svg",
      width: Math.round(size.w * zoom),
      height: Math.round(size.h * zoom),
      viewBox: `0 0 ${size.w} ${size.h}`,
      role: "group",
      "aria-label": `Recipe ${r.id} chart. The Outline view edits the same recipe with a keyboard or screen reader.`,
    });
    const lanesG = s("g", { class: "lanes" });
    r.unitProcedures.forEach((u, i) => {
      const sev = severityOf(issuesAt(issues, "unitProc", i));
      const selected = !!sel && "lane" in sel && sel.lane === i;
      const g = s("g", {
        class: ["rlane", sev ?? "", selected ? "sel" : ""].filter(Boolean).join(" "),
        id: `rlane-${i}`,
        "data-lane": i,
        tabindex: 0,
        role: "button",
        "aria-pressed": String(selected),
        "aria-label": `Unit procedure ${u.id}${u.unitClass ? `, unit class ${u.unitClass}` : ", any unit"}${sev ? ` (${sev})` : ""}`,
      });
      g.append(
        s("rect", { class: "lane-bg", x: laneX(i), y: 0, width: LANE_W - 6, height: size.h, rx: 6 }),
        s("rect", { class: "lane-head", x: laneX(i), y: 0, width: LANE_W - 6, height: LANE_HEAD - 8, rx: 6 }),
        s("text", { x: laneX(i) + 10, y: 18, class: "ftitle" }, clip(u.id, 22)),
        s("text", { x: laneX(i) + 10, y: 34, class: "fkind" }, clip(u.unitClass ? `unit class ${u.unitClass}` : "any unit", 28)),
      );
      lanesG.append(g);
    });
    const boxes = stepBoxes(r);
    const barsG = s("g", { class: "bars" });
    r.transitions.forEach((_, ti) => {
      const b = barGeom(r, boxes, ti);
      if (!b) return;
      const sev = severityOf(issuesAt(issues, "transition", ti));
      const selected = !!sel && "transition" in sel && sel.transition === ti;
      const done = b.t.from.every((id) => stepStatus(id) === "done") && b.t.from.length > 0;
      const g = s("g", {
        class: ["rbar", sev ?? "", selected ? "sel" : "", done ? "live" : ""].filter(Boolean).join(" "),
        id: `rbar-${ti}`,
        "data-transition": ti,
        tabindex: 0,
        role: "button",
        "aria-pressed": String(selected),
        "aria-label": `Transition ${b.t.id}: after ${b.t.from.join(" and ")} comes ${b.t.to.join(" and ")}${b.t.to.length > 1 ? " (in parallel)" : ""}${sev ? ` (${sev})` : ""}`,
      });
      for (const w of b.wires) g.append(s("path", { class: "fwire", d: w.d }));
      if (b.join) g.append(s("line", { class: "rdouble", x1: b.join.x1, x2: b.join.x2, y1: b.join.y, y2: b.join.y }), s("line", { class: "rdouble", x1: b.join.x1, x2: b.join.x2, y1: b.join.y - 4, y2: b.join.y - 4 }));
      if (b.fork) g.append(s("line", { class: "rdouble", x1: b.fork.x1, x2: b.fork.x2, y1: b.fork.y, y2: b.fork.y }), s("line", { class: "rdouble", x1: b.fork.x1, x2: b.fork.x2, y1: b.fork.y + 4, y2: b.fork.y + 4 }));
      g.append(
        s("rect", { class: "rbar-hit", x: b.x1, y: b.y - 10, width: b.x2 - b.x1, height: 20 }, s("title", {}, `Transition ${b.t.id}`)),
        s("rect", { class: "rbar-line", x: b.x1 + 20, y: b.y - BAR_H / 2, width: Math.max(20, b.x2 - b.x1 - 40), height: BAR_H, rx: 2 }),
        s("text", { x: b.x2 - 16, y: b.y - 6, class: "fport", "text-anchor": "end" }, b.t.id),
      );
      barsG.append(g);
    });
    const stepsG = s("g", { class: "nodes" });
    for (const b of boxes) {
      const si = r.steps.indexOf(b.step);
      const sev = severityOf(issuesAt(issues, "step", si));
      const selected = !!sel && "step" in sel && sel.step === b.step.id;
      const st = stepStatus(b.step.id);
      const ph = phaseOf(b.step.id);
      const g = s("g", {
        class: ["fnode", "rstep", sev ?? "", selected ? "sel" : "", statusClass(st)].filter(Boolean).join(" "),
        id: `rstep-${b.step.id}`,
        "data-step": b.step.id,
        transform: `translate(${b.x} ${b.y})`,
        tabindex: 0,
        role: "button",
        "aria-pressed": String(selected),
        "aria-label": `${stepLabel(b.step, classes(), st, ph)}${sev ? ` (${sev})` : ""}`,
      });
      if (sev === "error") g.setAttribute("aria-invalid", "true");
      g.append(
        s("title", {}, stepLabel(b.step, classes(), st, ph)),
        s("rect", { width: STEP_W, height: STEP_H, rx: 8 }),
        s("text", { x: 10, y: 18, class: "fkind" }, clip(`Step · ${b.step.id}${b.step.operation ? ` · ${b.step.operation}` : ""}`, 30)),
        s("text", { x: 10, y: 37, class: "ftitle" }, clip(argsText(b.step, classes()))),
        s("text", { x: 10, y: 55, class: "fport" }, clip(st ? `${st}${ph ? ` · ${ph.split(" ").slice(1).join(" ")}` : ""}` : b.step.name, 32)),
        s("circle", { class: "port out", cx: STEP_W / 2, cy: STEP_H, r: 7, "data-port": "next" }, s("title", {}, "Drag onto a step: that step comes after this one")),
      );
      stepsG.append(g);
    }
    const temp = s("path", { class: "fwire temp", d: "" });
    svg.append(lanesG, barsG, stepsG, temp);
    const box = h("div", { class: "flow-canvas" }, svg);
    attachPointer(svg, box, temp, r);
    svg.addEventListener("keydown", (e) => {
      const t = e.target as Element;
      const stepEl = t.closest?.("[data-step]");
      const barEl = t.closest?.("[data-transition]");
      const laneEl = t.closest?.("[data-lane]");
      if (stepEl) {
        const id = stepEl.getAttribute("data-step")!;
        const st = r.steps.find((x) => x.id === id);
        if (!st) return;
        const lane = Math.max(0, r.unitProcedures.findIndex((u) => u.id === st.unitProcedure));
        const step = e.shiftKey ? 40 : 8;
        if (e.key === "ArrowUp" || e.key === "ArrowDown") {
          e.preventDefault();
          keeper.want(`rstep-${id}`);
          change(moveStep(r, id, st.y + (e.key === "ArrowUp" ? -step : step)));
        } else if (e.key === "ArrowLeft" || e.key === "ArrowRight") {
          const to = r.unitProcedures[lane + (e.key === "ArrowLeft" ? -1 : 1)];
          e.preventDefault();
          if (!to) return;
          keeper.want(`rstep-${id}`);
          change(setStepLane(r, id, to.id));
        } else if (e.key === "Enter" || e.key === " ") {
          e.preventDefault();
          sel = { step: id };
          keeper.want(`rstep-${id}`);
          draw();
        } else if (e.key === "Delete" || e.key === "Backspace") {
          e.preventDefault();
          sel = { step: id };
          removeSelected();
        }
      } else if (barEl || laneEl) {
        const pick: Selection = barEl ? { transition: Number(barEl.getAttribute("data-transition")) } : { lane: Number(laneEl!.getAttribute("data-lane")) };
        if (e.key === "Enter" || e.key === " ") {
          e.preventDefault();
          sel = pick;
          keeper.want((barEl ?? laneEl)!.id);
          draw();
        } else if (e.key === "Delete" || e.key === "Backspace") {
          e.preventDefault();
          sel = pick;
          removeSelected();
        }
      }
    });
    return box;
  }

  function attachPointer(svg: SVGSVGElement, box: HTMLElement, temp: SVGPathElement, r: MasterRecipe) {
    type Drag =
      | { kind: "step"; id: string; x0: number; y0: number; px: number; py: number; moved: boolean }
      | { kind: "wire"; from: string; a: { x: number; y: number } }
      | { kind: "pan"; px: number; py: number; sl: number; st: number; moved: boolean };
    let drag: Drag | null = null;
    const local = (e: PointerEvent) => {
      const rect = svg.getBoundingClientRect();
      return { x: (e.clientX - rect.left) / zoom, y: (e.clientY - rect.top) / zoom };
    };
    const boxes = stepBoxes(r);
    svg.addEventListener("pointerdown", (e) => {
      const t = e.target as Element;
      const stepEl = t.closest("[data-step]");
      const b = boxes.find((x) => x.step.id === stepEl?.getAttribute("data-step"));
      if (b && t.closest("[data-port]")) {
        drag = { kind: "wire", from: b.step.id, a: stepPort(b) };
      } else if (b) {
        drag = { kind: "step", id: b.step.id, x0: b.x, y0: b.y, px: e.clientX, py: e.clientY, moved: false };
      } else {
        const bar = t.closest("[data-transition]");
        const lane = t.closest("[data-lane]");
        if (bar || (lane && t.classList.contains("lane-head")) || (lane && t.tagName === "text")) {
          sel = bar ? { transition: Number(bar.getAttribute("data-transition")) } : { lane: Number(lane!.getAttribute("data-lane")) };
          draw();
          return;
        }
        drag = { kind: "pan", px: e.clientX, py: e.clientY, sl: box.scrollLeft, st: box.scrollTop, moved: false };
      }
      svg.setPointerCapture?.(e.pointerId);
      e.preventDefault();
    });
    svg.addEventListener("pointermove", (e) => {
      if (!drag) return;
      if (drag.kind === "wire") temp.setAttribute("d", wirePath(drag.a, local(e)));
      else if (drag.kind === "step") {
        const dx = (e.clientX - drag.px) / zoom;
        const dy = (e.clientY - drag.py) / zoom;
        if (Math.abs(dx) + Math.abs(dy) > 3) drag.moved = true;
        svg.querySelector(`[data-step="${drag.id}"]`)?.setAttribute("transform", `translate(${drag.x0 + dx} ${Math.max(LANE_HEAD, drag.y0 + dy)})`);
      } else {
        const dx = e.clientX - drag.px;
        const dy = e.clientY - drag.py;
        if (Math.abs(dx) + Math.abs(dy) > 3) drag.moved = true;
        box.scrollLeft = drag.sl - dx;
        box.scrollTop = drag.st - dy;
      }
    });
    svg.addEventListener("pointerup", (e) => {
      const d = drag;
      drag = null;
      if (!d) return;
      if (d.kind === "wire") {
        temp.setAttribute("d", "");
        const target = document.elementFromPoint?.(e.clientX, e.clientY)?.closest("[data-step]")?.getAttribute("data-step");
        if (target && target !== d.from) connect(r, d.from, target);
      } else if (d.kind === "step") {
        sel = { step: d.id };
        keeper.want(`rstep-${d.id}`);
        if (!d.moved) return draw();
        const dx = (e.clientX - d.px) / zoom;
        const dy = (e.clientY - d.py) / zoom;
        const lane = laneAt(d.x0 + dx + STEP_W / 2, r.unitProcedures.length);
        let next = moveStep(r, d.id, d.y0 + dy - LANE_HEAD);
        const up = r.unitProcedures[lane];
        if (up) next = setStepLane(next, d.id, up.id);
        change(next);
      } else if (!d.moved && sel) {
        sel = null;
        draw();
      }
    });
    svg.addEventListener("pointercancel", () => {
      drag = null;
      temp.setAttribute("d", "");
      draw();
    });
  }

  // ---- outline

  function outlineView(r: MasterRecipe, issues: readonly RecipeIssue[]): HTMLElement {
    const list = h("ol", { class: "flow-outline recipe-outline", "aria-label": `Recipe ${r.id} steps in run order` });
    for (const st of runOrder(r)) {
      const si = r.steps.indexOf(st);
      const sev = severityOf(issuesAt(issues, "step", si));
      const selected = !!sel && "step" in sel && sel.step === st.id;
      const stat = stepStatus(st.id);
      const ph = phaseOf(st.id);
      const inc = r.transitions[incoming(r, st.id)];
      const out = r.transitions[outgoing(r, st.id)];
      const pick = h(
        "button",
        { type: "button", class: "onode", id: `ostep-${st.id}`, "aria-pressed": String(selected) },
        h("span", { class: "fkind" }, `Step · ${st.id} · ${st.unitProcedure}`),
        " ",
        argsText(st, classes()),
        stat ? h("span", { class: "rstatus" }, ` — ${stat}${ph ? `, ${ph}` : ""}`) : null,
      );
      pick.addEventListener("click", () => {
        sel = { step: st.id };
        keeper.want(`ostep-${st.id}`);
        draw();
      });
      const after = h("div", { class: "row oports" });
      after.append(h("span", { class: "muted" }, inc ? `after ${inc.from.join(" and ")}` : "starts the recipe"));
      if (out) {
        after.append(h("span", {}, `then ${out.to.join(" and ")}${out.to.length > 1 ? " (in parallel)" : ""}`));
        for (const id of out.to) {
          const b = h("button", { type: "button", class: "small", "aria-label": `${id} no longer follows ${st.id}` }, `× ${id}`);
          b.addEventListener("click", () => change(detach(r, r.transitions.indexOf(out), "to", id)));
          after.append(b);
        }
      }
      const next = select(
        [["", "add a next step…"], ...r.steps.filter((x) => x.id !== st.id).map((x) => [x.id, x.id] as const), ["+new", "a new step"]],
        "",
        `Add a step after ${st.id}`,
        `onext-${st.id}`,
      );
      next.addEventListener("change", () => {
        if (next.value === "+new") {
          const { recipe: added, id } = addStep(r, classes(), st.unitProcedure, st.class);
          if (!id) return;
          const c = connectSteps(moveStep(added, id, st.y + 120), st.id, id);
          sel = { step: id };
          keeper.want(`ostep-${id}`);
          change(c.recipe);
        } else if (next.value) {
          keeper.want(`onext-${st.id}`);
          connect(r, st.id, next.value);
        }
      });
      after.append(next);
      list.append(
        h(
          "li",
          { class: ["orow", sev ?? "", statusClass(stat)].filter(Boolean).join(" "), "aria-invalid": sev === "error" ? "true" : undefined },
          pick,
          after,
        ),
      );
    }
    if (r.steps.length === 0) list.append(h("li", { class: "muted" }, "No steps yet: + Step adds one."));
    return list;
  }

  // ---- properties

  function issueList(list: readonly RecipeIssue[], r: MasterRecipe, id?: string): HTMLElement {
    return h(
      "ul",
      { class: "issues", id },
      ...list.map((i) => {
        const where =
          i.at === "step" ? `${r.steps[i.index ?? -1]?.id ?? "step"}: ` : i.at === "transition" ? `transition ${r.transitions[i.index ?? -1]?.id ?? ""}: ` : i.at === "unitProc" ? `${r.unitProcedures[i.index ?? -1]?.id ?? ""}: ` : "";
        const li = h("li", { class: i.severity }, h("span", { class: "sev" }, i.severity === "error" ? "Error: " : "Warning: "), `${where}${i.message}`);
        if (i.fix) {
          const fix = i.fix;
          const b = h("button", { type: "button", class: "fix" }, fixText(fix, r));
          b.addEventListener("click", () => change(applyRecipeFix(r, fix)));
          li.append(" ", b);
        }
        if ((i.at === "step" || i.at === "transition" || i.at === "unitProc") && i.index !== null) {
          li.classList.add("link");
          li.addEventListener("click", (e) => {
            if ((e.target as HTMLElement).tagName === "BUTTON") return;
            const k = i.index!;
            sel = i.at === "step" ? { step: r.steps[k]?.id ?? "" } : i.at === "transition" ? { transition: k } : { lane: k };
            keeper.want(i.at === "step" ? `rstep-${r.steps[k]?.id}` : i.at === "transition" ? `rbar-${k}` : `rlane-${k}`, `ostep-${r.steps[k]?.id}`);
            draw();
          });
        }
        return li;
      }),
    );
  }

  function field(label: string, control: HTMLElement): HTMLElement {
    return h("tr", {}, h("th", { scope: "row" }, label), h("td", {}, control));
  }

  function props(r: MasterRecipe, issues: readonly RecipeIssue[]): HTMLElement | null {
    if (!sel) return null;
    const table = h("table", { class: "grid props" });
    const wrap = h("div", { class: "recipe-props" }, h("h4", {}, "Properties"), table);
    const cur = sel;
    if ("step" in cur) {
      const st = r.steps.find((x) => x.id === cur.step);
      if (!st) return null;
      const si = r.steps.indexOf(st);
      const id = h("input", { type: "text", value: st.id, id: "rprop-id" });
      id.addEventListener("change", () => {
        const next = renameStep(r, st.id, id.value.trim());
        if (next === r) return say(`"${id.value}" is taken or unchanged.`);
        sel = { step: id.value.trim() };
        change(next);
      });
      const name = h("input", { type: "text", value: st.name, id: "rprop-name" });
      name.addEventListener("change", () => change(setStepText(r, st.id, { name: name.value })));
      const op = h("input", { type: "text", value: st.operation, id: "rprop-op" });
      op.addEventListener("change", () => change(setStepText(r, st.id, { operation: op.value })));
      const up = select(
        r.unitProcedures.map((u) => [u.id, u.id] as const),
        st.unitProcedure,
        "Unit procedure",
        "rprop-up",
      );
      up.addEventListener("change", () => change(setStepLane(r, st.id, up.value)));
      const cls = select(
        [...(classes().some((c) => c.id === st.class) ? [] : [[st.class, `${st.class || "(none)"} (missing)`] as const]), ...classes().map((c) => [c.id, c.name ? `${c.id} — ${c.name}` : c.id] as const)],
        st.class,
        "Phase class",
        "rprop-class",
      );
      cls.addEventListener("change", () => change(setStepClass(r, st.id, cls.value, classes())));
      table.append(field("Id", id), field("Name", name), field("Operation", op), field("Unit procedure", up), field("Phase class", cls));
      for (const p of classes().find((c) => c.id === st.class)?.params ?? []) {
        const mine = argIssues(issues, st, si, p.id);
        const descId = `rprop-arg-${p.id}-desc`;
        const v = st.params[p.id];
        const input = h("input", {
          type: "number",
          inputmode: "numeric",
          step: 1,
          min: p.min,
          max: p.max,
          value: v === undefined ? "" : String(v),
          id: `rprop-arg-${p.id}`,
          "aria-describedby": descId,
          "aria-invalid": severityOf(mine) === "error" ? "true" : undefined,
        });
        input.addEventListener("change", () => {
          const n = input.value.trim() === "" ? null : Number(input.value);
          if (n !== null && (!Number.isInteger(n) || Math.abs(n) > 2 ** 31 - 1)) return say(`${p.id} must be a whole number.`);
          keeper.want(`rprop-arg-${p.id}`);
          change(setArg(r, st.id, p.id, n));
        });
        const desc = h("span", { class: "muted", id: descId }, ` ${rangeText(p)}${mine.length ? ` — ${mine.map((i) => i.message).join("; ")}` : ""}`);
        table.append(field(p.name || p.id, h("span", {}, input, desc)));
      }
      const mine = issuesAt(issues, "step", si);
      if (mine.length) wrap.append(issueList(mine, r));
    } else if ("transition" in cur) {
      const ti = cur.transition;
      const t = r.transitions[ti];
      if (!t) return null;
      for (const side of ["from", "to"] as const) {
        const cell = h("span", {});
        for (const id of t[side]) {
          const b = h("button", { type: "button", class: "small", "aria-label": `Take ${id} off this transition` }, `${id} ×`);
          b.addEventListener("click", () => {
            const next = detach(r, ti, side, id);
            if (!next.transitions[ti]) sel = null;
            change(next);
          });
          cell.append(b, " ");
        }
        table.append(field(side === "from" ? "After (all done)" : "Then (start together)", cell));
      }
      const mine = issuesAt(issues, "transition", ti);
      if (mine.length) wrap.append(issueList(mine, r));
    } else {
      const li = cur.lane;
      const u = r.unitProcedures[li];
      if (!u) return null;
      const id = h("input", { type: "text", value: u.id, id: "rprop-up-id" });
      id.addEventListener("change", () => {
        const next = setUnitProcedure(r, li, { id: id.value.trim() });
        if (next === r) return say(`"${id.value}" is taken or unchanged.`);
        change(next);
      });
      const name = h("input", { type: "text", value: u.name, id: "rprop-up-name" });
      name.addEventListener("change", () => change(setUnitProcedure(r, li, { name: name.value })));
      const unitClasses = [...new Set(plant!.physical.units.map((x) => x.class))];
      const uc = select(
        [["", "any unit"], ...unitClasses.map((c) => [c, c] as const), ...(u.unitClass && !unitClasses.includes(u.unitClass) ? [[u.unitClass, `${u.unitClass} (no unit has it)`] as const] : [])],
        u.unitClass ?? "",
        "Unit class",
        "rprop-up-class",
      );
      uc.addEventListener("change", () => change(setUnitProcedure(r, li, { unitClass: uc.value || null })));
      table.append(field("Id", id), field("Name", name), field("Unit class", uc));
      const mine = issuesAt(issues, "unitProc", li);
      if (mine.length) wrap.append(issueList(mine, r));
    }
    return wrap;
  }

  // ---- allocation and run

  function allocation(r: MasterRecipe, hasErrors: boolean): HTMLElement {
    const p = plant!;
    const sec = h("section", { class: "recipe-alloc", "aria-labelledby": "recipe-alloc-h" }, h("h4", { id: "recipe-alloc-h" }, "Units for this recipe"));
    const a = hasErrors ? null : proto.allocateRecipe(r, classes(), allocationUnits(p, (u) => !offline.has(u.id)));
    const rows = a ? allocationRows(a, r, p.physical.units.map((u) => u.id)) : [];
    const table = h("table", { class: "grid alloc" }, h("tr", {}, h("th", {}, "Unit"), h("th", {}, "Online (simulated)"), h("th", {}, "Board"), h("th", {}, "Runs")));
    for (const u of p.physical.units) {
      const on = h("input", { type: "checkbox", id: `ralloc-on-${u.id}`, "aria-label": `${u.id} online in the simulator`, disabled: !!liveFor() });
      on.checked = !offline.has(u.id);
      on.addEventListener("change", () => {
        if (on.checked) offline.delete(u.id);
        else offline.add(u.id);
        keeper.want(`ralloc-on-${u.id}`);
        draw();
      });
      const here = board && u.node && board.info.node === u.node ? board : null;
      const problem = here ? serveProblem(here.info) : null;
      const boardText = u.node
        ? deviceNode === u.node
          ? problem
            ? `bound to ${u.node}: the board on the Device tab, which cannot serve phases (runs in the simulator). ${problem}`
            : `bound to ${u.node}: the board on the Device tab${here ? " (runs on the board)" : ""}`
          : `bound to ${u.node}: not seen on the Device tab`
        : "not bound (Units view)";
      const row = rows.find((x) => x.unit === u.id);
      table.append(h("tr", { class: row && !row.ok ? "error" : "" }, h("th", { scope: "row" }, u.id), h("td", {}, on), h("td", { class: problem ? "warning" : "muted" }, boardText), h("td", {}, row?.text ?? "—")));
    }
    sec.append(table);
    if (hasErrors) sec.append(h("p", { class: "note" }, "Fix the recipe's errors to bind it to units."));
    else if (a && !a.ok) sec.append(h("p", { class: "note", role: "status" }, a.message));
    return sec;
  }

  function stopRun() {
    if (!live) return;
    live.runner.dispose();
    live.net.dispose();
    live.bus.dispose();
    live = null;
  }

  async function startRun() {
    const p = plant;
    const r = recipe();
    if (!p || !r || building) return;
    stopRun();
    building = true;
    runError = "";
    say("Compiling the units…");
    try {
      // Compile every unit first, so the sessions start their clocks
      // together (a session loaded before a slow compile would run its
      // overdue ticks back to back).
      // A unit on the Device tab's board runs there (its payload is the
      // one uploaded from the Device tab); the others in the simulator.
      const built: { u: (typeof p.physical.units)[number]; wasm: Uint8Array }[] = [];
      for (const u of p.physical.units) if (!onBoard(u)) built.push({ u, wasm: await buildUnit(p, u.id, proto.abi) });
      const bus = new SimBus(proto, realClock);
      const net = new MixedNet(bus);
      for (const u of p.physical.units) {
        const b = onBoard(u);
        if (b) net.attach(u.id, b.requester);
      }
      for (const { u, wasm } of built) {
        const session = bus.add(u.id, wasm, { node: u.node });
        for (const row of u.cms.rows) if (row.func === "adc" && row.pin !== null) session.setAdc(row.pin, levels.get(`${u.id}/${row.label}`) ?? 2048);
      }
      for (const u of p.physical.units) if (offline.has(u.id)) net.setOnline(u.id, false);
      const runner = new RecipeRunner(proto, p, r, net, realClock, {
        onChange: () => {
          const l = live;
          if (!l) return;
          const sig = runSignature(l.runner.state, l.runner.steps(), l.runner.phases(), l.runner.trace.length);
          if (sig !== l.sig) {
            l.sig = sig;
            scheduleDraw();
          }
        },
      });
      live = { recipe: at, bus, net, runner, sig: "" };
      runner.start(10);
      const real = net.boards;
      status = real.length ? `Ready: ${real.join(", ")} on the board, the rest in the simulator. Press Start.` : "Running in the simulator: press Start.";
    } catch (e) {
      runError = errorText(e);
      status = "";
    } finally {
      building = false;
      draw();
    }
  }

  let drawPending: ReturnType<typeof setTimeout> | null = null;
  function scheduleDraw() {
    if (drawPending) return;
    drawPending = setTimeout(() => {
      drawPending = null;
      draw();
    }, 80);
  }

  function runPanel(hasErrors: boolean): HTMLElement {
    const p = plant!;
    const real = p.physical.units.filter((u) => onBoard(u)).map((u) => u.id);
    const title = real.length ? `Run (${real.join(", ")} on the board)` : "Run in the simulator";
    const sec = h("section", { class: "recipe-run", "aria-labelledby": "recipe-run-h" }, h("h4", { id: "recipe-run-h" }, title));
    const l = liveFor();
    const bar = h("div", { class: "row" });
    const runBtn = h("button", { type: "button", id: "recipe-run", disabled: hasErrors || building || !proto.hasSimulator }, l ? "Restart simulation" : real.length ? "Run" : "Run in simulator");
    runBtn.addEventListener("click", () => void startRun());
    bar.append(runBtn);
    if (l) {
      const stop = h("button", { type: "button", id: "recipe-run-stop" }, "Stop simulation");
      stop.addEventListener("click", () => {
        stopRun();
        status = "Simulation stopped.";
        draw();
      });
      bar.append(stop);
    }
    sec.append(bar);
    // Analog inputs the units read.
    const pots = p.physical.units.flatMap((u) => u.cms.rows.filter((row) => row.func === "adc").map((row) => ({ u, row })));
    if (pots.length) {
      const inputs = h("div", { class: "row" });
      for (const { u, row } of pots) {
        const k = `${u.id}/${row.label}`;
        const input = h("input", { type: "number", min: 0, max: 4095, step: 1, value: String(levels.get(k) ?? 2048), id: `rlevel-${u.id}-${row.label}`, inputmode: "numeric" });
        input.addEventListener("change", () => {
          const v = Math.max(0, Math.min(4095, Math.round(Number(input.value) || 0)));
          levels.set(k, v);
          if (row.pin !== null) l?.bus.unit(u.id)?.session.setAdc(row.pin, v);
        });
        inputs.append(h("label", {}, `${u.id} ${row.label} (0..4095) `, input));
      }
      sec.append(inputs);
    }
    if (runError) sec.append(h("p", { class: "note", role: "alert" }, runError));
    if (!l) return sec;
    const model = proto.phaseModel();
    const state = l.runner.state;
    const ops = h("div", { class: "seg operator", role: "group", "aria-label": "Operator commands" });
    for (const b of operatorButtons(model, state)) {
      const btn = h("button", { type: "button", id: `recipe-op-${b.command.toLowerCase()}`, disabled: !b.enabled }, b.command);
      btn.addEventListener("click", () => {
        keeper.want(`recipe-op-${b.command.toLowerCase()}`);
        status = "";
        l.runner.operator(b.command);
      });
      ops.append(btn);
    }
    sec.append(h("p", { class: "row" }, h("strong", { class: `rstate st-${state.toLowerCase()}`, role: "status", "aria-live": "polite" }, `Procedure: ${state}`), ops));
    const units = h("div", { class: "row" });
    for (const u of p.physical.units) {
      const on = l.net.online(u.id);
      const where = l.net.simulated(u.id) ? "" : " (board)";
      const b = h("button", { type: "button", id: `recipe-net-${u.id}` }, on ? `Lose ${u.id}` : `Find ${u.id}`);
      b.addEventListener("click", () => {
        keeper.want(`recipe-net-${u.id}`);
        if (on) l.runner.lose(u.id);
        else l.runner.find(u.id);
        draw();
      });
      units.append(h("span", { class: on ? "" : "warning" }, `${u.id}${where} ${on ? "online" : "offline"} `), b);
    }
    sec.append(units);
    const phases = h("ul", { class: "recipe-phases" }, ...l.runner.phases().map((ph) => h("li", {}, `${ph.unit}/${ph.em}.${ph.phase}: ${ph.state ?? "unknown"}${ph.values.length ? ` — reported ${ph.values.join(", ")}` : ""}`)));
    sec.append(phases);
    const lines = l.runner.trace.slice(-200);
    sec.append(
      h("h5", {}, "Trace"),
      h("ol", { class: "recipe-trace log", "aria-label": "What the run did" }, ...lines.map((x) => h("li", { class: x.type }, `${(x.t / 1000).toFixed(2)} s  ${x.text}`))),
    );
    return sec;
  }

  // ---- draw

  function draw() {
    if (!plant) return;
    keeper.remember();
    const r = recipe();
    const kids: (HTMLElement | null)[] = [toolbar()];
    if (!r) {
      kids.push(h("p", { class: "muted" }, "This plant has no recipe yet. New recipe makes one: unit procedures, steps that run phase classes, and transitions between them."));
    } else {
      const issues = check(r);
      const errors = issues.some((i) => i.severity === "error");
      kids.push(
        editBar(r),
        h("p", { class: "muted", role: "status", "aria-live": "polite", id: "recipe-status" }, status),
        mode === "canvas" ? canvas(r, issues) : outlineView(r, issues),
        props(r, issues),
        issues.length ? h("div", {}, h("h4", {}, "Problems"), issueList(issues, r, "recipe-issues")) : null,
        allocation(r, errors),
        runPanel(errors),
      );
    }
    root.replaceChildren(...kids.filter((x): x is HTMLElement => !!x));
    keeper.restore();
  }

  return {
    el: root,
    render(p, node, b) {
      plant = p;
      deviceNode = node;
      board = b ?? null;
      if (at >= p.recipes.length) at = Math.max(0, p.recipes.length - 1);
      if (sel && "step" in sel && !p.recipes[at]?.steps.some((x) => x.id === (sel as { step: string }).step)) sel = null;
      draw();
    },
    issues(p) {
      return p.recipes.flatMap((r) => {
        try {
          return proto.checkRecipe(r, p.procedural.classes);
        } catch {
          return [];
        }
      });
    },
  };
}

