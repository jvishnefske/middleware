// The States view of the Blocks tab: the payload's state-machine tables
// (model/machines.ts) as keyboard-editable grids (grid.ts). One machine is
// edited at a time (a selector picks it when there are several): its id,
// name and initial state, then three grids: States (name, note, entry
// actions), Events (name, note) and Transitions.
//
// Transitions are a list by default, one row per transition: State,
// Event, Guard, Next, Actions. A state x event matrix is the classic
// picture of a table, but a cell has room for neither a guard (up to three
// terms) nor up to four actions with their parameters, and a cell may hold
// several guarded rows; the list gives each its own labelled controls, and
// it is what keyboard and phone users can edit (one row reads left to
// right and scrolls in its own box). The matrix is kept as an optional,
// read-only overview (the "Matrix overview" toggle, aria-pressed) for what
// a list hides: holes, the (state, event) cells with no row. Each matrix
// cell is a button: it focuses the cell's first transition row in the
// list, or adds a row for an empty cell.
//
// Guards are edited in a structured form: "always", or up to three
// `[pin] [op] [value]` terms joined by and / or (machines.ts SimpleGuard,
// via guardFromSimple / simpleGuard). A guard not in that form (from an
// imported file) is shown read-only as text with a "Replace" button.
// Actions (a row's and a state's entry actions, up to four) are a kind
// select (write / toggle / publish / log / raise) and that kind's
// parameters: a pin from the pin table's outputs, a level, a key and a
// text, a log text, an event of the machine.
//
// Every control has a label naming its row ("Guard pin of row 2"). The
// check's problems (protocol-wasm `checkMachines`, run by the page) show
// under their row with the row's controls pointing at them
// (aria-describedby); the field in question is aria-invalid for an error
// and marked as a warning otherwise (warnings do not block compiling).
// Fixes are buttons: add a state or event and set the initial state go
// through machines.ts; "add pin" goes to the page (`onAddPin`), which adds
// the row to the pin table. Renaming a state, event or machine id renames
// the table's own references and, once the name is valid and free, the
// blocks' (`rename`, statesModel.ts decides). While the simulator runs,
// the current state's States row, its matrix row header and the
// transitions from it are highlighted (class + aria-current / hidden text).
//
// All decisions are elsewhere (statesModel.ts, machines.ts, the Rust
// check); this file only renders and forwards.

import {
  ACTION_KINDS,
  type ActionKind,
  type ActionRef,
  CMP_OPS,
  CMP_TEXT,
  type CmpOp,
  MAX_ACTIONS,
  MAX_EVENTS,
  MAX_MACHINE_ROWS,
  MAX_MACHINES,
  MAX_SIMPLE_TERMS,
  MAX_STATES,
  type Machine,
  type MachineIssue,
  type SimpleGuard,
  type TransitionRow,
  addEvent,
  addMachine,
  addRow,
  addState,
  exprText,
  guardFromSimple,
  newMachine,
  removeEvent,
  removeMachine,
  removeRow,
  removeState,
  setEntry,
  setEventNote,
  setMachine,
  setMeta,
  setRow,
  setStateNote,
  simpleGuard,
  transition,
} from "../model/machines";
import type { PinFunc } from "../model/pins";
import { h } from "./dom";
import { type Grid, focusKeeper, grid } from "./grid";
import {
  ACTION_KIND_TEXT,
  INPUT_FUNCS,
  OUTPUT_FUNCS,
  type Option,
  type PinRef,
  type Rename,
  actionOfKind,
  addAction,
  addTerm,
  applyMachineFix,
  cellText,
  errorCount,
  fieldSeverity,
  fixText,
  issuesAt,
  labelOptions,
  matrix,
  nameOptions,
  newTransition,
  removeAction,
  removeTerm,
  setAction,
  setTerm,
  termValue,
  typeMachineId,
  typeName,
} from "./statesModel";

export interface StatesViewEvents {
  /** New machines; `rename` when blocks naming the old name should follow. */
  onChange(machines: Machine[], rename?: Rename): void;
  /** A fix asks for a pin-table row. */
  onAddPin(label: string, func: PinFunc): void;
}

export interface StatesView {
  el: HTMLElement;
  /** `current`: each machine's state in the running simulator, by id. */
  render(machines: readonly Machine[], issues: readonly MachineIssue[], pins: readonly PinRef[], current: ReadonlyMap<string, string> | null): void;
}

const STATE_COLS = ["name", "note", "entry", "remove"] as const;
const EVENT_COLS = ["name", "note", "remove"] as const;
const ROW_COLS = ["state", "event", "guard", "next", "actions", "remove"] as const;
type Field = MachineIssue["field"];
const MATRIX_KEY = "states-matrix";

function option(value: string, text: string, selected: boolean): HTMLOptionElement {
  const o = h("option", { value }, text);
  o.selected = selected;
  return o;
}
function select(opts: readonly Option[], value: string): HTMLSelectElement {
  return h("select", {}, ...opts.map(([v, t]) => option(v, t, v === value)));
}
const hidden = (t: string) => h("span", { class: "visually-hidden" }, t);

export function statesView(ev: StatesViewEvents): StatesView {
  let machines: readonly Machine[] = [];
  let issues: readonly MachineIssue[] = [];
  let pins: readonly PinRef[] = [];
  let current: ReadonlyMap<string, string> | null = null;
  /** The machine being edited. */
  let sel = 0;
  let showMatrix = false;
  try {
    showMatrix = localStorage.getItem(MATRIX_KEY) === "1";
  } catch {
    /* storage unavailable */
  }
  /** The name being typed: its control id and the name references use. */
  let edit: { key: string; applied: string } | null = null;

  const gs = grid("sm-state", STATE_COLS, "name");
  const ge = grid("sm-event", EVENT_COLS, "name");
  const gr = grid("sm-row", ROW_COLS, "state");

  const heading = h("h3", { id: "states-heading" }, "State machines");
  const intro = h(
    "p",
    { class: "muted" },
    "A state machine is a table: in each state, an event moves to a next state (when its guard holds) and runs actions. " +
      "Blocks raise events (raise Mode.PRESS) and react to states (when Mode enters On).",
  );
  const summary = h("p", { class: "pin-summary", role: "status" });
  const bar = h("div", { class: "row sm-bar" });
  const meta = h("div", { class: "sm-meta" });
  const body = h("div", { class: "sm-body" });
  const keys = h(
    "p",
    { class: "muted small", id: "sm-keys" },
    "Keys: Tab moves across; Ctrl + arrow keys move between cells; Up and Down move between rows in text cells.",
  );
  const el = h("div", { class: "states-view" }, heading, intro, summary, bar, meta, body, keys);
  const keeper = focusKeeper(el);
  el.addEventListener("focusin", (e) => {
    if (edit && (e.target as HTMLElement).id !== edit.key) edit = null;
  });

  // The three grids' tables persist (their key handlers are attached once);
  // their bodies are rebuilt on every render.
  const statesBody = h("tbody");
  const eventsBody = h("tbody");
  const rowsBody = h("tbody");
  const head = (cols: readonly string[]) =>
    h("thead", {}, h("tr", {}, ...cols.map((t) => (t ? h("th", { scope: "col" }, t) : h("th", { scope: "col", "aria-label": "Remove" })))));
  const statesTable = h(
    "table",
    { class: "sm-table", "aria-describedby": "sm-keys" },
    h("caption", { class: "visually-hidden" }, "States: one row per state"),
    head(["State", "Note", "On entry", ""]),
    statesBody,
  );
  const eventsTable = h(
    "table",
    { class: "sm-table", "aria-describedby": "sm-keys" },
    h("caption", { class: "visually-hidden" }, "Events: one row per event"),
    head(["Event", "Note", ""]),
    eventsBody,
  );
  const rowsTable = h(
    "table",
    { class: "sm-table sm-rows", "aria-describedby": "sm-keys" },
    h("caption", { class: "visually-hidden" }, "Transitions: one row per transition"),
    h(
      "thead",
      {},
      h(
        "tr",
        {},
        h("th", { scope: "col" }, "#"),
        ...["State", "Event", "Guard", "Next", "Actions"].map((t) => h("th", { scope: "col" }, t)),
        h("th", { scope: "col", "aria-label": "Remove" }),
      ),
    ),
    rowsBody,
  );
  const m = (): Machine | undefined => machines[sel];
  gs.attachKeys(statesTable, () => m()?.states.length ?? 0);
  ge.attachKeys(eventsTable, () => m()?.events.length ?? 0);
  gr.attachKeys(rowsTable, () => m()?.rows.length ?? 0);

  const matrixBtn = h("button", { type: "button", id: "sm-matrix-toggle", "aria-pressed": String(showMatrix), "aria-controls": "sm-matrix" }, "Matrix overview");
  matrixBtn.addEventListener("click", () => {
    showMatrix = !showMatrix;
    try {
      localStorage.setItem(MATRIX_KEY, showMatrix ? "1" : "0");
    } catch {
      /* storage unavailable */
    }
    draw();
  });

  /** Replaces the edited machine. */
  function put(next: Machine, rename?: Rename) {
    ev.onChange(setMachine(machines, sel, next), rename);
  }

  function issueItem(x: MachineIssue): HTMLElement {
    const li = h("li", { class: x.severity }, h("span", { class: "sev" }, x.severity === "error" ? "Error" : "Warning"), " ", x.message);
    if (x.fix) {
      const fix = x.fix;
      const b = h("button", { type: "button", class: "fix" }, fixText(fix));
      b.addEventListener("click", () => {
        // An add-event fix may name another machine (a raise of its event).
        const mi = fix.kind === "add-event" && fix.machine !== undefined ? fix.machine : x.machine;
        const target = machines[mi];
        if (!target) return;
        const r = applyMachineFix(target, fix);
        if ("pin" in r) ev.onAddPin(r.pin.label, r.pin.func);
        else ev.onChange(setMachine(machines, mi, r.machine));
      });
      li.append(" ", b);
    }
    return li;
  }

  /** Marks `ctl` with the worst issue on one of `fields`. */
  function mark<E extends HTMLElement>(ctl: E, on: readonly MachineIssue[], fields: readonly Field[]): E {
    const s = fieldSeverity(on, fields);
    if (s === "error") ctl.setAttribute("aria-invalid", "true");
    else if (s === "warning") ctl.classList.add("has-warning");
    return ctl;
  }

  /** The actions editor of cell (r, c): one line per action, add button. */
  function actionsEditor<C extends string>(
    g: Grid<C>,
    r: number,
    c: C,
    mc: Machine,
    as: readonly ActionRef[],
    who: string,
    described: string | null,
    on: readonly MachineIssue[],
    fields: readonly Field[],
    set: (as: ActionRef[]) => void,
  ): HTMLElement {
    const ctl = <E extends HTMLElement>(el_: E, label: string, sub?: string) => mark(g.control(r, c, el_, label, described, sub), on, fields);
    const lines = as.map((a, k) => {
      const n = `Action ${k + 1} of ${who}`;
      const kind = ctl(select(ACTION_KINDS.map((x) => [x, ACTION_KIND_TEXT[x]] as const), a.kind), `${n}: kind`, k === 0 ? undefined : `a${k}`);
      kind.addEventListener("change", () => set(setAction(as, k, actionOfKind(kind.value as ActionKind, mc, pins, a))));
      const params: HTMLElement[] = [];
      if (a.kind === "write" || a.kind === "toggle") {
        const lab = ctl(select(labelOptions(pins, OUTPUT_FUNCS, a.label, "not an output in Pins"), a.label), `${n}: pin`, `a${k}-pin`);
        lab.addEventListener("change", () => set(setAction(as, k, { ...a, label: lab.value })));
        params.push(lab);
      }
      if (a.kind === "write") {
        const lvl = ctl(select([["1", "high"], ["0", "low"]], String(a.level)), `${n}: level`, `a${k}-level`);
        lvl.addEventListener("change", () => set(setAction(as, k, { ...a, level: lvl.value === "1" ? 1 : 0 })));
        params.push(lvl);
      }
      if (a.kind === "publish") {
        const key = ctl(h("input", { type: "text", value: a.key, maxlength: 200, spellcheck: "false", autocomplete: "off", class: "sm-key" }), `${n}: key`, `a${k}-key`);
        key.addEventListener("input", () => set(setAction(as, k, { ...a, key: key.value })));
        params.push(key);
      }
      if (a.kind === "publish" || a.kind === "log") {
        const txt = ctl(h("input", { type: "text", value: a.text, maxlength: 512, autocomplete: "off", class: "sm-text" }), `${n}: text`, `a${k}-text`);
        txt.addEventListener("input", () => set(setAction(as, k, { ...a, text: txt.value })));
        params.push(txt);
      }
      if (a.kind === "raise") {
        const evSel = ctl(select(nameOptions(mc.events.map((e) => e.name), a.event, "not an event"), a.event), `${n}: event`, `a${k}-event`);
        evSel.addEventListener("change", () => set(setAction(as, k, { ...a, event: evSel.value })));
        params.push(evSel);
      }
      const rm = g.control(r, c, h("button", { type: "button", class: "icon" }, "×"), `Remove action ${k + 1} of ${who}`, null, `a${k}-rm`);
      rm.addEventListener("click", () => {
        keeper.want(g.cellId(r, c, as.length > 1 ? "add" : undefined), g.cellId(r, c));
        set(removeAction(as, k));
      });
      return h("div", { class: "sm-line" }, kind, ...params, rm);
    });
    const add = g.control(
      r,
      c,
      h("button", { type: "button", class: "sm-add", disabled: as.length >= MAX_ACTIONS }, "+ action"),
      `Add an action to ${who}`,
      described,
      as.length === 0 ? undefined : "add",
    );
    add.addEventListener("click", () => {
      keeper.want(g.cellId(r, c, as.length === 0 ? undefined : `a${as.length}`));
      set(addAction(as, mc, pins));
    });
    return h("div", { class: "sm-editor" }, ...lines, add);
  }

  /** The guard editor of transition row `r`. */
  function guardEditor(mc: Machine, r: number, row: TransitionRow, described: string | null, on: readonly MachineIssue[]): HTMLElement {
    const who = `row ${r + 1}`;
    const ctl = <E extends HTMLElement>(el_: E, label: string, sub?: string) => mark(gr.control(r, "guard", el_, label, described, sub), on, ["guard"]);
    const sg = simpleGuard(row.guard);
    if (!sg) {
      const t = row.guard ? exprText(row.guard) : "";
      const rep = ctl(h("button", { type: "button" }, "Replace"), `Replace the guard of ${who} (${t}) with "always"`);
      rep.addEventListener("click", () => put(setRow(mc, r, { guard: null })));
      return h("div", { class: "sm-editor" }, h("div", { class: "sm-line" }, h("code", {}, t), rep));
    }
    const setG = (g: SimpleGuard) => put(setRow(mc, r, { guard: guardFromSimple(g) }));
    const several = sg.terms.length > 1;
    const lines: HTMLElement[] = [];
    sg.terms.forEach((t, k) => {
      const of = `of ${who}${several ? `, term ${k + 1}` : ""}`;
      const pin = ctl(select(labelOptions(pins, INPUT_FUNCS, t.label, "not an input in Pins"), t.label), `Guard pin ${of}`, k === 0 ? undefined : `t${k}-pin`);
      pin.addEventListener("change", () => setG(setTerm(sg, k, { label: pin.value })));
      const op = ctl(select(CMP_OPS.map((o) => [o, CMP_TEXT[o]] as const), t.op), `Guard comparison ${of}`, `t${k}-op`);
      op.addEventListener("change", () => setG(setTerm(sg, k, { op: op.value as CmpOp })));
      const val = ctl(h("input", { type: "number", value: t.value, step: 1, inputmode: "numeric", class: "sm-num" }), `Guard value ${of}`, `t${k}-val`);
      val.addEventListener("change", () => {
        const v = termValue(val.value);
        if (v === null) val.value = String(t.value);
        else setG(setTerm(sg, k, { value: v }));
      });
      const rm = gr.control(r, "guard", h("button", { type: "button", class: "icon" }, "×"), `Remove guard term ${k + 1} of ${who}`, null, `t${k}-rm`);
      rm.addEventListener("click", () => {
        keeper.want(gr.cellId(r, "guard"));
        setG(removeTerm(sg, k));
      });
      const line = h("div", { class: "sm-line" });
      if (k === 1) {
        const join = ctl(select([["and", "and"], ["or", "or"]], sg.join), `Guard join of ${who} (and: every term holds; or: any term holds)`, "join");
        join.addEventListener("change", () => setG({ ...sg, join: join.value === "or" ? "or" : "and" }));
        line.append(join);
      } else if (k > 1) line.append(h("span", { class: "muted" }, sg.join));
      line.append(pin, op, val, rm);
      lines.push(line);
    });
    const add = gr.control(
      r,
      "guard",
      h("button", { type: "button", class: "sm-add", disabled: sg.terms.length >= MAX_SIMPLE_TERMS }, "+ condition"),
      `Add a guard condition to ${who}`,
      described,
      sg.terms.length === 0 ? undefined : "add",
    );
    add.addEventListener("click", () => {
      keeper.want(gr.cellId(r, "guard", sg.terms.length === 0 ? undefined : `t${sg.terms.length}-pin`));
      setG(addTerm(sg, pins));
    });
    if (sg.terms.length === 0) lines.push(h("div", { class: "sm-line" }, h("span", { class: "muted" }, "always"), mark(add, on, ["guard"])));
    else lines.push(add);
    return h("div", { class: "sm-editor" }, ...lines);
  }

  function stateRows(mc: Machine, cur: string | null): HTMLElement[] {
    return mc.states.flatMap((s, i) => {
      const on = issuesAt(issues, sel, "states", i);
      const desc = on.length ? gs.issuesId(i) : null;
      const label = s.name || `state ${i + 1}`;
      const name = mark(
        gs.control(i, "name", h("input", { type: "text", value: s.name, maxlength: 32, spellcheck: "false", autocomplete: "off" }), `Name of state ${i + 1}`, desc),
        on,
        ["name", "state", "id"],
      );
      name.addEventListener("input", () => {
        if (edit?.key !== name.id) edit = { key: name.id, applied: s.name };
        const r = typeName(mc, "state", i, name.value, edit.applied);
        edit.applied = r.applied;
        put(r.machine, r.rename);
      });
      const note = gs.control(i, "note", h("input", { type: "text", value: s.note, maxlength: 200 }), `Note for state ${label}`, null);
      note.addEventListener("input", () => put(setStateNote(mc, i, note.value)));
      const entry = actionsEditor(gs, i, "entry", mc, s.entry, `the entry of ${label}`, desc, on, ["entry", "actions"], (as) => put(setEntry(mc, i, as)));
      const rm = gs.control(i, "remove", h("button", { type: "button", class: "icon" }, "×"), `Remove state ${label} and its transitions`, null);
      rm.addEventListener("click", () => {
        keeper.want(gs.cellId(Math.max(0, Math.min(i, mc.states.length - 2)), "remove"), "sm-add-state");
        put(removeState(mc, i));
      });
      const isCur = cur !== null && s.name === cur;
      const tags = [
        s.name && s.name === mc.initial ? h("span", { class: "sm-tag" }, "start") : null,
        isCur ? h("span", { class: "sm-tag now", "aria-hidden": "true" }, "now") : null,
        isCur ? hidden(" (current state)") : null,
      ];
      const tr = h(
        "tr",
        { class: [on.length ? "has-issue" : "", isCur ? "current" : ""].join(" ").trim() },
        h("td", { class: "sticky" }, h("div", { class: "sm-line" }, name, ...tags)),
        h("td", {}, note),
        h("td", {}, entry),
        h("td", {}, rm),
      );
      if (isCur) tr.setAttribute("aria-current", "true");
      return on.length ? [tr, gs.issueRow(i, on.map(issueItem), "grid-issue-row")] : [tr];
    });
  }

  function eventRows(mc: Machine): HTMLElement[] {
    return mc.events.flatMap((e, i) => {
      const on = issuesAt(issues, sel, "events", i);
      const desc = on.length ? ge.issuesId(i) : null;
      const label = e.name || `event ${i + 1}`;
      const name = mark(
        ge.control(i, "name", h("input", { type: "text", value: e.name, maxlength: 32, spellcheck: "false", autocomplete: "off" }), `Name of event ${i + 1}`, desc),
        on,
        ["name", "event", "id"],
      );
      name.addEventListener("input", () => {
        if (edit?.key !== name.id) edit = { key: name.id, applied: e.name };
        const r = typeName(mc, "event", i, name.value, edit.applied);
        edit.applied = r.applied;
        put(r.machine, r.rename);
      });
      const note = ge.control(i, "note", h("input", { type: "text", value: e.note, maxlength: 200 }), `Note for event ${label}`, null);
      note.addEventListener("input", () => put(setEventNote(mc, i, note.value)));
      const rm = ge.control(i, "remove", h("button", { type: "button", class: "icon" }, "×"), `Remove event ${label}, its transitions and the actions raising it`, null);
      rm.addEventListener("click", () => {
        keeper.want(ge.cellId(Math.max(0, Math.min(i, mc.events.length - 2)), "remove"), "sm-add-event");
        put(removeEvent(mc, i));
      });
      const tr = h("tr", { class: on.length ? "has-issue" : "" }, h("td", { class: "sticky" }, name), h("td", {}, note), h("td", {}, rm));
      return on.length ? [tr, ge.issueRow(i, on.map(issueItem), "grid-issue-row")] : [tr];
    });
  }

  function transitionRows(mc: Machine, cur: string | null): HTMLElement[] {
    const states = mc.states.map((s) => s.name);
    const events = mc.events.map((e) => e.name);
    return mc.rows.flatMap((row, r) => {
      const on = issuesAt(issues, sel, "rows", r);
      const desc = on.length ? gr.issuesId(r) : null;
      const who = `row ${r + 1}`;
      const st = mark(gr.control(r, "state", select(nameOptions(states, row.state, "not a state"), row.state), `State of ${who}`, desc), on, ["state"]);
      st.addEventListener("change", () => put(setRow(mc, r, { state: st.value })));
      const evSel = mark(gr.control(r, "event", select(nameOptions(events, row.event, "not an event"), row.event), `Event of ${who}`, desc), on, ["event"]);
      evSel.addEventListener("change", () => put(setRow(mc, r, { event: evSel.value })));
      const next = mark(gr.control(r, "next", select(nameOptions(states, row.next, "not a state"), row.next), `Next state of ${who}`, desc), on, ["next"]);
      next.addEventListener("change", () => put(setRow(mc, r, { next: next.value })));
      const acts = actionsEditor(gr, r, "actions", mc, row.actions, who, desc, on, ["actions"], (as) => put(setRow(mc, r, { actions: as })));
      const rm = gr.control(r, "remove", h("button", { type: "button", class: "icon" }, "×"), `Remove ${who} (${row.state} on ${row.event})`, null);
      rm.addEventListener("click", () => {
        keeper.want(gr.cellId(Math.max(0, Math.min(r, mc.rows.length - 2)), "remove"), "sm-add-row");
        put(removeRow(mc, r));
      });
      const fromCur = cur !== null && row.state === cur;
      const tr = h(
        "tr",
        { class: [on.length ? "has-issue" : "", fromCur ? "from-current" : ""].join(" ").trim(), id: `sm-tr-${r}` },
        h("th", { scope: "row", class: "sm-num-col" }, String(r + 1), fromCur ? hidden(" (from the current state)") : null),
        h("td", {}, st),
        h("td", {}, evSel),
        h("td", {}, guardEditor(mc, r, row, desc, on)),
        h("td", {}, next),
        h("td", {}, acts),
        h("td", {}, rm),
      );
      return on.length ? [tr, gr.issueRow(r, on.map(issueItem), "grid-issue-row", ROW_COLS.length + 1)] : [tr];
    });
  }

  function matrixEl(mc: Machine, cur: string | null): HTMLElement {
    const cells = matrix(mc);
    const table = h(
      "table",
      { class: "sm-matrix" },
      h("caption", { class: "visually-hidden" }, "Overview: next state for each state (rows) and event (columns)"),
      h("thead", {}, h("tr", {}, h("td", { class: "muted small" }, "state \\ event"), ...mc.events.map((e) => h("th", { scope: "col" }, e.name)))),
      h(
        "tbody",
        {},
        ...cells.map((line, si) => {
          const s = mc.states[si]?.name ?? "";
          const isCur = cur !== null && s === cur;
          const th = h("th", { scope: "row", class: isCur ? "current" : "" }, s, isCur ? hidden(" (current state)") : null);
          if (isCur) th.setAttribute("aria-current", "true");
          return h(
            "tr",
            {},
            th,
            ...line.map((c, ei) => {
              const { text, label } = cellText(c);
              const b = h("button", { type: "button", id: `sm-mx-${si}-${ei}`, class: c.rows.length ? "" : "hole", "aria-label": label }, text);
              b.addEventListener("click", () => {
                const first = c.rows[0];
                if (first) {
                  const target = document.getElementById(gr.cellId(first.i, "state"));
                  target?.focus();
                  target?.scrollIntoView({ block: "nearest" });
                } else if (mc.rows.length < MAX_MACHINE_ROWS) {
                  keeper.want(gr.cellId(mc.rows.length, "next"));
                  put(addRow(mc, transition(c.state, c.event, c.state)));
                }
              });
              return h("td", {}, b);
            }),
          );
        }),
      ),
    );
    return h("div", { class: "pin-scroll sm-matrix-scroll", id: "sm-matrix" }, table);
  }

  function addButton(id: string, text: string, disabled: boolean, onClick: () => void): HTMLButtonElement {
    const b = h("button", { type: "button", id, disabled }, text);
    b.addEventListener("click", onClick);
    return b;
  }

  function listIssues(id: string, on: readonly MachineIssue[]): HTMLElement {
    return h("ul", { class: "issues", id }, ...on.map(issueItem));
  }

  /** Rebuilds everything from the current inputs. */
  function draw() {
    keeper.remember();
    sel = Math.max(0, Math.min(sel, machines.length - 1));
    const mc = m();

    const errs = errorCount(issues);
    const warns = issues.length - errs;
    summary.textContent =
      machines.length === 0
        ? ""
        : errs === 0 && warns === 0
          ? `${machines.length} machine${machines.length === 1 ? "" : "s"}, no problems.`
          : `${errs} problem${errs === 1 ? "" : "s"} to fix before compiling${warns ? `, ${warns} warning${warns === 1 ? "" : "s"}` : ""}.`;
    summary.className = `pin-summary ${errs ? "bad" : "good"}`;

    // Machine bar: selector, add, remove.
    const barItems: HTMLElement[] = [];
    if (machines.length > 1) {
      const pick = h(
        "select",
        { id: "sm-machine" },
        ...machines.map((x, i) => {
          const n = errorCount(issues, i);
          return option(String(i), `${x.id || `machine ${i + 1}`}${x.name ? ` (${x.name})` : ""}${n ? `: ${n} to fix` : ""}`, i === sel);
        }),
      );
      pick.addEventListener("change", () => {
        sel = Number(pick.value);
        draw();
      });
      barItems.push(h("label", {}, "Machine ", pick));
    }
    barItems.push(
      addButton("sm-add-machine", "Add machine", machines.length >= MAX_MACHINES, () => {
        sel = machines.length;
        keeper.want("sm-id");
        ev.onChange(addMachine(machines, newMachine(machines)));
      }),
    );
    if (mc) {
      const label = mc.id || `machine ${sel + 1}`;
      const rm = addButton("sm-remove-machine", `Remove machine ${label} (no undo)`, false, () => {
        keeper.want("sm-add-machine");
        ev.onChange(removeMachine(machines, sel));
      });
      rm.setAttribute("aria-label", `Remove machine ${label} with its states, events and transitions (no undo)`);
      barItems.push(rm);
    }
    bar.replaceChildren(...barItems);

    if (!mc) {
      meta.replaceChildren(h("p", { class: "muted" }, "No state machines yet. Add one to start from a two-state table."));
      body.replaceChildren();
      keeper.restore();
      return;
    }
    const cur = current?.get(mc.id) ?? null;

    // Machine fields.
    const mOn = issuesAt(issues, sel, "machine", null);
    const mDesc = mOn.length ? "sm-machine-issues" : null;
    const field = <E extends HTMLElement>(ctl: E, id: string, f: Field): E => {
      ctl.id = id;
      if (mDesc) ctl.setAttribute("aria-describedby", mDesc);
      return mark(ctl, mOn, [f]);
    };
    const idIn = field(h("input", { type: "text", value: mc.id, maxlength: 32, spellcheck: "false", autocomplete: "off" }), "sm-id", "id");
    idIn.addEventListener("input", () => {
      if (edit?.key !== idIn.id) edit = { key: idIn.id, applied: mc.id };
      const r = typeMachineId(machines, sel, idIn.value, edit.applied);
      edit.applied = r.applied;
      ev.onChange(r.machines, r.rename);
    });
    const nameIn = field(h("input", { type: "text", value: mc.name, maxlength: 200 }), "sm-name", "name");
    nameIn.addEventListener("input", () => put(setMeta(mc, { name: nameIn.value })));
    const initial = field(select(nameOptions(mc.states.map((s) => s.name), mc.initial, "not a state"), mc.initial), "sm-initial", "initial");
    initial.addEventListener("change", () => put(setMeta(mc, { initial: initial.value })));
    meta.replaceChildren(
      h(
        "p",
        { class: "row" },
        h("label", {}, "Id ", idIn),
        h("label", {}, "Name ", nameIn),
        h("label", {}, "Starts in ", initial),
        cur !== null ? h("span", { class: "sm-tag now" }, `now in ${cur}`) : null,
      ),
      listIssues("sm-machine-issues", mOn),
    );

    statesBody.replaceChildren(...stateRows(mc, cur));
    eventsBody.replaceChildren(...eventRows(mc));
    rowsBody.replaceChildren(...transitionRows(mc, cur));
    statesTable.hidden = mc.states.length === 0;
    eventsTable.hidden = mc.events.length === 0;
    rowsTable.hidden = mc.rows.length === 0;
    matrixBtn.setAttribute("aria-pressed", String(showMatrix));

    const addRowBtn = addButton("sm-add-row", "Add transition", mc.rows.length >= MAX_MACHINE_ROWS || !newTransition(mc), () => {
      const t = newTransition(mc);
      if (!t) return;
      keeper.want(gr.cellId(mc.rows.length, "state"));
      put(addRow(mc, t));
    });
    const parts: (HTMLElement | null)[] = [
      h("h4", {}, "States"),
      h("div", { class: "pin-scroll" }, statesTable),
      listIssues("sm-states-issues", issuesAt(issues, sel, "states", null)),
      h(
        "p",
        { class: "row" },
        addButton("sm-add-state", "Add state", mc.states.length >= MAX_STATES, () => {
          keeper.want(gs.cellId(mc.states.length, "name"));
          put(addState(mc));
        }),
      ),
      h("h4", {}, "Events"),
      h("div", { class: "pin-scroll" }, eventsTable),
      listIssues("sm-events-issues", issuesAt(issues, sel, "events", null)),
      h(
        "p",
        { class: "row" },
        addButton("sm-add-event", "Add event", mc.events.length >= MAX_EVENTS, () => {
          keeper.want(ge.cellId(mc.events.length, "name"));
          put(addEvent(mc));
        }),
      ),
      h("div", { class: "row sm-rows-head" }, h("h4", {}, "Transitions"), matrixBtn),
      showMatrix && mc.states.length > 0 && mc.events.length > 0 ? matrixEl(mc, cur) : null,
      h("div", { class: "pin-scroll" }, rowsTable),
      mc.rows.length === 0 ? h("p", { class: "muted" }, "No transitions yet: every event is ignored in every state.") : null,
      listIssues("sm-rows-issues", issuesAt(issues, sel, "rows", null)),
      h("p", { class: "row" }, addRowBtn),
    ];
    body.replaceChildren(...parts.filter((x): x is HTMLElement => x !== null));
    keeper.restore();
  }

  function render(ms: readonly Machine[], is: readonly MachineIssue[], ps: readonly PinRef[], cur: ReadonlyMap<string, string> | null) {
    machines = ms;
    issues = is;
    pins = ps;
    current = cur;
    draw();
  }

  return { el, render };
}
