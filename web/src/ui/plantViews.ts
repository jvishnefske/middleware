// The Units and Phases views of the Blocks tab: the plant's physical and
// procedural models (model/plant.ts, IR v4) as keyboard-editable tables
// (grid.ts: real <table>s of labelled controls, Ctrl + arrow keys between
// cells, Up / Down between rows in text cells, focus kept across the
// re-render every change causes), each scrolling inside its own container
// so a phone never scrolls the page sideways.
//
// * Units: one row per unit (id, name, unit class, board, node binding,
//   remove) and which unit the other views edit. "Bind" takes the node id
//   the connected device's sys/info reported (Device tab); "Unbind"
//   clears it.
// * Phases, for the selected unit: its equipment modules (id, name, the
//   pin-table labels they own as checkboxes), the plant's phase classes
//   (id, name) and their parameters (class, id, name, min, max, unit),
//   the unit's equipment phases (module, id, class, a range per class
//   parameter), each phase's body flow per acting state (a unit flow,
//   none, or a new body flow), and the read-only phase state model from
//   protocol-wasm `phaseModel()`.
//
// Problems (model/plantEdit.ts `plantIssues`) show in plain words under
// their row; the row's controls point at them (aria-describedby,
// aria-invalid). All decisions are in plantEdit.ts; this file renders and
// forwards.

import type { PlantModel } from "../model/plant";
import {
  type PlantIssue,
  addClass,
  addEm,
  addParam,
  addPhase,
  addUnit,
  bindNode,
  bodyChoices,
  emIssues,
  newBodyFlow,
  removeClass,
  removeEm,
  removeParam,
  removePhase,
  removeUnit,
  setBody,
  setClass,
  setEm,
  setParam,
  setPhase,
  setRange,
  setUnit,
  unitPhaseIndices,
} from "../model/plantEdit";
import { ACTING_STATES, type ActingState } from "../model/procedural";
import { TARGET_IDS, isTargetId, targetLabel } from "../model/target";
import { h } from "./dom";
import { type Grid, focusKeeper, grid } from "./grid";

export interface PlantViewEvents {
  onChange(plant: PlantModel): void;
  /** The unit the Blocks, Pins, States, Flows and Phases views edit. */
  onSelect(unit: string): void;
  /** Open a flow in the Flows view. */
  onOpenFlow?(flow: string): void;
}

function option(value: string, text: string, selected: boolean): HTMLOptionElement {
  const o = h("option", { value }, text);
  o.selected = selected;
  return o;
}

function textInput(value: string, max = 24): HTMLInputElement {
  return h("input", { type: "text", value, spellcheck: "false", autocomplete: "off", maxlength: max });
}

function numInput(value: number): HTMLInputElement {
  return h("input", { type: "number", value: String(value), inputmode: "numeric", step: 1 });
}

const KEYS_TEXT = "Keys: Tab moves across; Ctrl + arrow keys move between cells; Up and Down move between rows in text cells.";

/** A captioned grid table in a sideways-scrolling container. */
function table(caption: string, headers: readonly string[], keysId: string): { scroller: HTMLElement; table: HTMLTableElement; tbody: HTMLTableSectionElement } {
  const tbody = h("tbody");
  const t = h(
    "table",
    { class: "pin-table plant-table", "aria-describedby": keysId },
    h("caption", { class: "visually-hidden" }, caption),
    h("thead", {}, h("tr", {}, ...headers.map((x) => h("th", { scope: "col", ...(x === "" ? { "aria-label": "Remove" } : {}) }, x)))),
    tbody,
  );
  return { scroller: h("div", { class: "pin-scroll" }, t), table: t, tbody };
}

/** A row of cells (null: a dash) followed by its issue row. */
function rowWithIssues<C extends string>(g: Grid<C>, r: number, cells: readonly (HTMLElement | null)[], issues: readonly PlantIssue[]): HTMLElement[] {
  const dash = () => h("span", { class: "muted", "aria-hidden": "true" }, "—");
  const tr = h("tr", { class: issues.length ? "has-issue" : "" }, ...cells.map((c, i) => h("td", i === 0 ? { class: "sticky" } : {}, c ?? dash())));
  if (issues.length) {
    for (const el of tr.querySelectorAll("input, select")) el.setAttribute("aria-invalid", "true");
    return [tr, g.issueRow(r, issues.map((x) => h("li", { class: "error" }, x.message)), "pin-issue-row")];
  }
  return [tr];
}

function summaryText(n: number, what: string): string {
  return n === 0 ? `${what}: no problems.` : `${what}: ${n} problem${n === 1 ? "" : "s"} to fix before compiling.`;
}

// ---- Units

const UNIT_COLS = ["id", "name", "class", "board", "node", "remove"] as const;
type UnitCol = (typeof UNIT_COLS)[number];

export interface UnitsView {
  el: HTMLElement;
  /** `device`: the node id the connected device reported, if any. */
  render(plant: PlantModel, selected: string, issues: readonly PlantIssue[], device: string | null): void;
}

export function unitsView(ev: PlantViewEvents): UnitsView {
  const t = table("Units: one row per board in the plant", ["Id", "Name", "Unit class", "Board", "Node", ""], "units-keys");
  const g = grid<UnitCol>("unit", UNIT_COLS, "id");
  const keeper = focusKeeper(t.table);
  const summary = h("p", { class: "pin-summary", role: "status" });
  const addBtn = h("button", { type: "button" }, "Add a unit");
  const intro = h(
    "p",
    { class: "muted" },
    "A unit is one board: its pins, modules, logic and phases. The other views edit the unit chosen here. Bind a unit to the connected device so recipes can address it by its node id.",
  );
  let plant: PlantModel | null = null;
  let rowCount = 0;
  g.attachKeys(t.table, () => rowCount);
  addBtn.addEventListener("click", () => {
    if (!plant) return;
    const next = addUnit(plant, plant.physical.units[0]?.board ?? "pico2");
    keeper.want(g.cellId(next.physical.units.length - 1, "id"));
    ev.onChange(next);
  });

  function render(p: PlantModel, selected: string, issues: readonly PlantIssue[], device: string | null) {
    keeper.remember();
    plant = p;
    rowCount = p.physical.units.length;
    const rows = p.physical.units.flatMap((u, r) => {
      const mine = issues.filter((x) => x.place === "units" && x.row === r);
      const desc = mine.length ? g.issuesId(r) : null;
      const name = u.id || `unit ${r + 1}`;
      const id = g.control(r, "id", textInput(u.id), `Id of unit ${r + 1}`, desc);
      id.addEventListener("change", () => {
        const next = setUnit(p, u.id, "id", id.value.trim());
        ev.onChange(next);
        if (u.id === selected) ev.onSelect(id.value.trim());
      });
      const nm = g.control(r, "name", textInput(u.name, 200), `Name of ${name}`, desc);
      nm.addEventListener("change", () => ev.onChange(setUnit(p, u.id, "name", nm.value)));
      const cls = g.control(r, "class", textInput(u.class), `Unit class of ${name}`, desc);
      cls.addEventListener("change", () => ev.onChange(setUnit(p, u.id, "class", cls.value.trim())));
      const board = g.control(r, "board", h("select", {}, ...TARGET_IDS.map((b) => option(b, targetLabel(b), b === u.board))), `Board of ${name}`, desc);
      board.addEventListener("change", () => isTargetId(board.value) && ev.onChange(setUnit(p, u.id, "board", board.value)));
      const nodeText = u.node ?? "not bound";
      const bind = g.control(
        r,
        "node",
        h("button", { type: "button", disabled: u.node === null && device === null }, u.node === null ? (device ? `Bind to ${device}` : "Connect a device to bind") : `${nodeText} (unbind)`),
        u.node === null ? `Bind ${name} to the connected device` : `${name} is bound to node ${nodeText}; unbind`,
        desc,
      );
      bind.addEventListener("click", () => {
        keeper.want(g.cellId(r, "node"));
        ev.onChange(bindNode(p, u.id, u.node === null ? device : null));
      });
      const pick = h("input", { type: "radio", name: "unit-pick", checked: u.id === selected, "aria-label": `Edit ${name} in the other views` });
      pick.addEventListener("change", () => pick.checked && ev.onSelect(u.id));
      const remove = g.control(r, "remove", h("button", { type: "button", class: "icon", disabled: p.physical.units.length <= 1 }, "×"), `Remove ${name} and its phases`, null);
      remove.addEventListener("click", () => {
        keeper.want(g.cellId(Math.max(0, r - 1), "id"));
        ev.onChange(removeUnit(p, u.id));
        if (u.id === selected) ev.onSelect(p.physical.units[r === 0 ? 1 : r - 1]?.id ?? "");
      });
      const idCell = h("span", { class: "row tight" }, pick, id);
      return rowWithIssues(g, r, [idCell, nm, cls, board, bind, remove], mine);
    });
    t.tbody.replaceChildren(...rows);
    const n = issues.filter((x) => x.place === "units").length;
    summary.textContent = summaryText(n, `${p.physical.units.length} unit${p.physical.units.length === 1 ? "" : "s"}`);
    summary.className = `pin-summary ${n ? "bad" : "good"}`;
    addBtn.disabled = p.physical.units.length >= 8;
    keeper.restore();
  }

  const el = h("div", { class: "plant-view" }, h("h3", {}, "Units"), intro, summary, t.scroller, h("p", { class: "row" }, addBtn), h("p", { class: "muted small", id: "units-keys" }, KEYS_TEXT));
  return { el, render };
}

// ---- Phases

const EM_COLS = ["id", "name", "cms", "remove"] as const;
const CLASS_COLS = ["id", "name", "remove"] as const;
const PARAM_COLS = ["class", "id", "name", "min", "max", "unit", "remove"] as const;
const PHASE_COLS = ["em", "id", "class", "ranges", "remove"] as const;
const BODY_COLS = ["phase", ...ACTING_STATES] as const;

export interface PhaseModelInfo {
  states: readonly string[];
  commands: readonly string[];
  next: readonly (readonly (string | null)[])[];
}

export interface PhasesView {
  el: HTMLElement;
  render(plant: PlantModel, unit: string, issues: readonly PlantIssue[], model: PhaseModelInfo | null): void;
}

export function phasesView(ev: PlantViewEvents): PhasesView {
  const em = table("Equipment modules of the unit: each groups pins", ["Id", "Name", "Pins it owns", ""], "phases-keys");
  const cls = table("Phase classes of the plant", ["Id", "Name", ""], "phases-keys");
  const par = table("Parameters of the phase classes", ["Class", "Id", "Name", "Min", "Max", "Unit", ""], "phases-keys");
  const ph = table("Equipment phases of the unit", ["Module", "Id", "Class", "Accepted ranges", ""], "phases-keys");
  const bod = table("Body flow of each phase per acting state", ["Phase", ...ACTING_STATES], "phases-keys");
  const gEm = grid<(typeof EM_COLS)[number]>("em", EM_COLS, "id");
  const gCls = grid<(typeof CLASS_COLS)[number]>("cls", CLASS_COLS, "id");
  const gPar = grid<(typeof PARAM_COLS)[number]>("par", PARAM_COLS, "id");
  const gPh = grid<(typeof PHASE_COLS)[number]>("ph", PHASE_COLS, "id");
  const gBod = grid<(typeof BODY_COLS)[number]>("bod", BODY_COLS, "running");
  const root = h("div", { class: "plant-view" });
  const keeper = focusKeeper(root);
  const counts = { em: 0, cls: 0, par: 0, ph: 0, bod: 0 };
  gEm.attachKeys(em.table, () => counts.em);
  gCls.attachKeys(cls.table, () => counts.cls);
  gPar.attachKeys(par.table, () => counts.par);
  gPh.attachKeys(ph.table, () => counts.ph);
  gBod.attachKeys(bod.table, () => counts.bod);
  const summary = h("p", { class: "pin-summary", role: "status" });
  const addEmBtn = h("button", { type: "button" }, "Add an equipment module");
  const addClsBtn = h("button", { type: "button" }, "Add a phase class");
  const addParSel = h("select", { "aria-label": "Add a parameter to a class" });
  const addPhSel = h("select", { "aria-label": "Add a phase to a module" });
  const matrix = h("div", { class: "pin-scroll" });
  const heading = h("h3", {}, "Phases");
  let plant: PlantModel | null = null;
  let unit = "";

  addEmBtn.addEventListener("click", () => {
    if (!plant) return;
    keeper.want(gEm.cellId(counts.em, "id"));
    ev.onChange(addEm(plant, unit));
  });
  addClsBtn.addEventListener("click", () => {
    if (!plant) return;
    keeper.want(gCls.cellId(counts.cls, "id"));
    ev.onChange(addClass(plant));
  });
  addParSel.addEventListener("change", () => {
    const v = addParSel.value;
    addParSel.value = "";
    const i = Number(v);
    if (!plant || v === "" || Number.isNaN(i)) return;
    keeper.want(gPar.cellId(counts.par, "id"));
    ev.onChange(addParam(plant, i));
  });
  addPhSel.addEventListener("change", () => {
    const v = addPhSel.value;
    addPhSel.value = "";
    if (!plant || !v) return;
    keeper.want(gPh.cellId(counts.ph, "id"));
    ev.onChange(addPhase(plant, unit, v));
  });

  function renderMatrix(m: PhaseModelInfo | null) {
    if (!m) {
      matrix.replaceChildren(h("p", { class: "muted" }, "The phase state model is not available in this build."));
      return;
    }
    const cols = [...m.commands, "Done"];
    matrix.replaceChildren(
      h(
        "table",
        { class: "pin-table phase-matrix" },
        h("caption", {}, "The phase state model (read only): the state each command or Done leads to; a dash is refused and leaves the state as it is."),
        h("thead", {}, h("tr", {}, h("th", { scope: "col" }, "State"), ...cols.map((c) => h("th", { scope: "col" }, c)))),
        h(
          "tbody",
          {},
          ...m.states.map((s, r) =>
            h(
              "tr",
              {},
              h("th", { scope: "row", class: "sticky" }, s),
              ...cols.map((c, k) => {
                const to = m.next[r]?.[k] ?? null;
                return h("td", to === null ? { class: "muted", "aria-label": `${c} in ${s}: refused` } : {}, to ?? "—");
              }),
            ),
          ),
        ),
      ),
    );
  }

  function render(p: PlantModel, u: string, issues: readonly PlantIssue[], m: PhaseModelInfo | null) {
    keeper.remember();
    plant = p;
    unit = u;
    const theUnit = p.physical.units.find((x) => x.id === u);
    heading.textContent = theUnit ? `Phases of ${theUnit.id}` : "Phases";
    const ems = theUnit?.ems ?? [];
    const labels = theUnit?.cms.rows.map((r) => r.label).filter(Boolean) ?? [];

    // Equipment modules.
    const myEmIssues = emIssues(issues, u);
    counts.em = ems.length;
    em.tbody.replaceChildren(
      ...ems.flatMap((e, r) => {
        const mine = myEmIssues.filter((x) => x.row === r);
        const desc = mine.length ? gEm.issuesId(r) : null;
        const name = e.id || `module ${r + 1}`;
        const id = gEm.control(r, "id", textInput(e.id), `Id of module ${r + 1}`, desc);
        id.addEventListener("change", () => ev.onChange(setEm(p, u, r, { id: id.value.trim() })));
        const nm = gEm.control(r, "name", textInput(e.name, 200), `Name of ${name}`, desc);
        nm.addEventListener("change", () => ev.onChange(setEm(p, u, r, { name: nm.value })));
        const boxes = h("fieldset", { class: "cms" }, h("legend", { class: "visually-hidden" }, `Pins ${name} owns`));
        const all = [...new Set([...labels, ...e.cms])];
        all.forEach((l, k) => {
          const cb = h("input", { type: "checkbox", checked: e.cms.includes(l) });
          if (k === 0) gEm.control(r, "cms", cb, `${name} owns ${l}`, desc);
          else gEm.control(r, "cms", cb, `${name} owns ${l}`, desc, String(k));
          cb.addEventListener("change", () => ev.onChange(setEm(p, u, r, { cms: cb.checked ? [...e.cms, l] : e.cms.filter((x) => x !== l) })));
          boxes.append(h("label", {}, cb, ` ${l}${labels.includes(l) ? "" : " (not in Pins)"}`));
        });
        if (all.length === 0) boxes.append(h("span", { class: "muted" }, "no pins yet (add them under Pins)"));
        const rm = gEm.control(r, "remove", h("button", { type: "button", class: "icon" }, "×"), `Remove ${name} and its phases`, null);
        rm.addEventListener("click", () => {
          keeper.want(gEm.cellId(Math.max(0, r - 1), "id"));
          ev.onChange(removeEm(p, u, r));
        });
        return rowWithIssues(gEm, r, [id, nm, boxes, rm], mine);
      }),
    );
    em.table.hidden = ems.length === 0;

    // Classes and parameters.
    const classes = p.procedural.classes;
    counts.cls = classes.length;
    cls.tbody.replaceChildren(
      ...classes.flatMap((c, r) => {
        const mine = issues.filter((x) => x.place === "classes" && x.row === r);
        const desc = mine.length ? gCls.issuesId(r) : null;
        const id = gCls.control(r, "id", textInput(c.id), `Id of class ${r + 1}`, desc);
        id.addEventListener("change", () => ev.onChange(setClass(p, r, { id: id.value.trim() })));
        const nm = gCls.control(r, "name", textInput(c.name, 200), `Name of ${c.id || `class ${r + 1}`}`, desc);
        nm.addEventListener("change", () => ev.onChange(setClass(p, r, { name: nm.value })));
        const rm = gCls.control(r, "remove", h("button", { type: "button", class: "icon" }, "×"), `Remove ${c.id || `class ${r + 1}`} and the phases implementing it`, null);
        rm.addEventListener("click", () => {
          keeper.want(gCls.cellId(Math.max(0, r - 1), "id"));
          ev.onChange(removeClass(p, r));
        });
        return rowWithIssues(gCls, r, [id, nm, rm], mine);
      }),
    );
    cls.table.hidden = classes.length === 0;
    const params = classes.flatMap((c, i) => c.params.map((x, j) => ({ c, i, x, j })));
    counts.par = params.length;
    par.tbody.replaceChildren(
      ...params.flatMap(({ c, i, x, j }, r) => {
        const mine = issues.filter((y) => y.place === "params" && y.row === i && y.sub === j);
        const desc = mine.length ? gPar.issuesId(r) : null;
        const name = `${c.id}.${x.id}`;
        const id = gPar.control(r, "id", textInput(x.id), `Id of parameter ${j + 1} of ${c.id}`, desc);
        id.addEventListener("change", () => ev.onChange(setParam(p, i, j, { id: id.value.trim() })));
        const nm = gPar.control(r, "name", textInput(x.name, 200), `Name of ${name}`, desc);
        nm.addEventListener("change", () => ev.onChange(setParam(p, i, j, { name: nm.value })));
        const lo = gPar.control(r, "min", numInput(x.min), `Minimum of ${name}`, desc);
        lo.addEventListener("change", () => ev.onChange(setParam(p, i, j, { min: Math.trunc(Number(lo.value)) || 0 })));
        const hi = gPar.control(r, "max", numInput(x.max), `Maximum of ${name}`, desc);
        hi.addEventListener("change", () => ev.onChange(setParam(p, i, j, { max: Math.trunc(Number(hi.value)) || 0 })));
        const un = gPar.control(r, "unit", textInput(x.unit, 16), `Unit of measure of ${name}`, desc);
        un.addEventListener("change", () => ev.onChange(setParam(p, i, j, { unit: un.value })));
        const rm = gPar.control(r, "remove", h("button", { type: "button", class: "icon" }, "×"), `Remove ${name}`, null);
        rm.addEventListener("click", () => ev.onChange(removeParam(p, i, j)));
        return rowWithIssues(gPar, r, [h("span", {}, c.id || `class ${i + 1}`), id, nm, lo, hi, un, rm], mine);
      }),
    );
    par.table.hidden = params.length === 0;
    addParSel.replaceChildren(option("", "Add a parameter to...", true), ...classes.map((c, i) => option(String(i), c.id || `class ${i + 1}`, false)));
    addParSel.disabled = classes.length === 0;

    // Phases of the unit.
    const idx = unitPhaseIndices(p, u);
    counts.ph = idx.length;
    counts.bod = idx.length;
    const flows = bodyChoices(p, u);
    const phRows: HTMLElement[] = [];
    const bodyRows: HTMLElement[] = [];
    idx.forEach((pi, r) => {
      const x = p.procedural.phases[pi];
      if (!x) return;
      const mine = issues.filter((y) => y.place === "phases" && y.row === pi);
      const desc = mine.length ? gPh.issuesId(r) : null;
      const name = `${x.em}.${x.id}`;
      const emSel = gPh.control(r, "em", h("select", {}, ...[...new Set([...ems.map((e) => e.id), x.em])].map((e) => option(e, ems.some((y) => y.id === e) ? e : `${e} (no such module)`, e === x.em))), `Module of phase ${name}`, desc);
      emSel.addEventListener("change", () => ev.onChange(setPhase(p, pi, { em: emSel.value })));
      const id = gPh.control(r, "id", textInput(x.id), `Id of phase ${r + 1}`, desc);
      id.addEventListener("change", () => ev.onChange(setPhase(p, pi, { id: id.value.trim() })));
      const clsSel = gPh.control(r, "class", h("select", {}, ...[...new Set([...classes.map((c) => c.id), x.class])].map((c) => option(c, classes.some((y) => y.id === c) ? c : `${c || "choose"} (no such class)`, c === x.class))), `Class of ${name}`, desc);
      clsSel.addEventListener("change", () => ev.onChange(setPhase(p, pi, { class: clsSel.value })));
      const cl = classes.find((c) => c.id === x.class);
      const ranges = h("span", { class: "ranges" });
      (cl?.params ?? []).forEach((q, k) => {
        const range = x.ranges[q.id] ?? [q.min, q.max];
        const lo = numInput(range[0]);
        const hi = numInput(range[1]);
        gPh.control(r, "ranges", lo, `Lowest ${q.id} ${name} accepts (class allows ${q.min} to ${q.max}${q.unit ? ` ${q.unit}` : ""})`, desc, k === 0 ? undefined : `${k}lo`);
        gPh.control(r, "ranges", hi, `Highest ${q.id} ${name} accepts`, desc, `${k}hi`);
        lo.addEventListener("change", () => ev.onChange(setRange(p, pi, q.id, [Math.trunc(Number(lo.value)) || 0, range[1]])));
        hi.addEventListener("change", () => ev.onChange(setRange(p, pi, q.id, [range[0], Math.trunc(Number(hi.value)) || 0])));
        ranges.append(h("label", {}, `${q.id} `, lo, " to ", hi, q.unit ? ` ${q.unit}` : ""));
      });
      if (!cl?.params.length) ranges.append(h("span", { class: "muted" }, cl ? "no parameters" : "choose a class"));
      const rm = gPh.control(r, "remove", h("button", { type: "button", class: "icon" }, "×"), `Remove phase ${name}`, null);
      rm.addEventListener("click", () => {
        keeper.want(gPh.cellId(Math.max(0, r - 1), "id"));
        ev.onChange(removePhase(p, pi));
      });
      phRows.push(...rowWithIssues(gPh, r, [emSel, id, clsSel, ranges, rm], mine));

      const bmine = issues.filter((y) => y.place === "bodies" && y.row === pi);
      const bdesc = bmine.length ? gBod.issuesId(r) : null;
      const cells = ACTING_STATES.map((st: ActingState) => {
        const cur = x.bodies[st] ?? "";
        const sel = gBod.control(
          r,
          st,
          h(
            "select",
            {},
            option("", st === "running" ? "none (done at once)" : "none (done at once)", cur === ""),
            ...[...new Set([...flows, ...(cur ? [cur] : [])])].map((f) => option(f, flows.includes(f) ? f : `${f} (no such flow)`, f === cur)),
            option("+new", "new body flow...", false),
          ),
          `${st} body of ${name}`,
          bdesc,
        );
        sel.addEventListener("change", () => {
          if (sel.value === "+new") {
            const made = newBodyFlow(p, pi, st);
            ev.onChange(made.plant);
            if (made.flow) ev.onOpenFlow?.(made.flow);
            return;
          }
          ev.onChange(setBody(p, pi, st, sel.value === "" ? null : sel.value));
        });
        return sel;
      });
      bodyRows.push(...rowWithIssues(gBod, r, [h("span", {}, name), ...cells], bmine));
    });
    ph.tbody.replaceChildren(...phRows);
    bod.tbody.replaceChildren(...bodyRows);
    ph.table.hidden = bod.table.hidden = idx.length === 0;
    addPhSel.replaceChildren(option("", "Add a phase to module...", true), ...ems.filter((e) => e.id).map((e) => option(e.id, e.id, false)));
    addPhSel.disabled = ems.length === 0 || classes.length === 0;

    const n = myEmIssues.length + issues.filter((x) => x.place === "classes" || x.place === "params" || ((x.place === "phases" || x.place === "bodies") && idx.includes(x.row))).length;
    summary.textContent = summaryText(n, `${ems.length} module${ems.length === 1 ? "" : "s"}, ${classes.length} class${classes.length === 1 ? "" : "es"}, ${idx.length} phase${idx.length === 1 ? "" : "s"}`);
    summary.className = `pin-summary ${n ? "bad" : "good"}`;
    renderMatrix(m);
    keeper.restore();
  }

  root.append(
    heading,
    h("p", { class: "muted" }, "Equipment modules group the unit's pins; phases are what recipes ask for: each implements a phase class on a module, accepts a range of each class parameter and runs a body flow in each acting state."),
    summary,
    h("h4", {}, "Equipment modules"),
    em.scroller,
    h("p", { class: "row" }, addEmBtn),
    h("h4", {}, "Phase classes (whole plant)"),
    cls.scroller,
    par.scroller,
    h("p", { class: "row" }, addClsBtn, addParSel),
    h("h4", {}, "Phases of this unit"),
    ph.scroller,
    h("p", { class: "row" }, addPhSel),
    h("h4", {}, "Body flows"),
    bod.scroller,
    h("h4", {}, "Phase state model"),
    matrix,
    h("p", { class: "muted small", id: "phases-keys" }, KEYS_TEXT),
  );
  return { el: root, render };
}
