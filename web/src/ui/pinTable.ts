// The Pins view of the Blocks tab: the payload's pin table as a keyboard-
// editable grid. Every cell is a labelled form control in a real <table>
// (caption, column headers); Tab moves through the cells, Ctrl+arrow keys
// move between cells in any direction, and Up / Down move between rows in
// the text cells. Each change goes to `onChange` as a new table (the page
// re-checks it with protocol-wasm `checkPins` and calls `render` again);
// focus and caret survive the re-render. Problems show in plain language
// in a line under their row (the row's controls point at it with
// aria-describedby), table-wide ones under the table, each with its
// one-click fix when the check offers one. The table scrolls inside its
// own container, so a phone never scrolls the page sideways.
//
// All decisions are elsewhere: what is allowed (checkPins, Rust), the
// transitions (model/pins.ts). This file only renders and forwards.

import {
  type BusSetting,
  FUNC_LABELS,
  PIN_FUNCS,
  PULLS,
  type PinFunc,
  type PinIssue,
  type PinRow,
  type PinTable,
  addRows,
  applyFix,
  boardDefaults,
  busKind,
  findRow,
  freeLabel,
  removeRow,
  row,
  setBus,
  setRow,
} from "../model/pins";
import { type TargetId, boardOf, targetLabel } from "../model/target";
import type { BoardPins, GpioInfo } from "../protocol/wasm";
import { h } from "./dom";

const LABEL_RE = /^[A-Za-z][A-Za-z0-9_]{0,23}$/;
const COLS = ["label", "pin", "func", "bus", "pull", "initial", "note", "remove"] as const;
type Col = (typeof COLS)[number];

export interface PinTableView {
  el: HTMLElement;
  render(target: TargetId, table: PinTable, issues: readonly PinIssue[], info: BoardPins | null): void;
}

export interface PinTableEvents {
  /** A new table; `rename` when blocks naming `from` should now name `to`. */
  onChange(table: PinTable, rename?: { from: string; to: string }): void;
}

function pinText(g: GpioInfo): string {
  const tags: string[] = [];
  if (g.reserved) tags.push(`reserved: ${g.reserved}`);
  else if (g.inputOnly) tags.push("input only");
  if (g.adc !== undefined) tags.push(`ADC${g.adc}`);
  if (g.i2c) tags.push(`I2C${g.i2c.bus} ${g.i2c.line.toUpperCase()}`);
  if (g.spi) tags.push(`SPI${g.spi.block} ${g.spi.signal.toUpperCase()}`);
  if (g.pwm !== undefined) tags.push(`PWM ${g.pwm.timer}.${g.pwm.channel}`);
  return `GPIO ${g.pin}${tags.length ? ` (${tags.join(", ")})` : ""}`;
}

function option(value: string, text: string, selected: boolean): HTMLOptionElement {
  const o = h("option", { value }, text);
  o.selected = selected;
  return o;
}

/** Board-chosen pins for a new I2C or SPI bus, where the board fixes or
 * suggests them. */
function busRows(kind: "i2c" | "spi", bus: number, t: PinTable, info: BoardPins | null): PinRow[] {
  if (kind === "i2c") {
    const wired = info?.i2c.find((b) => b.bus === bus);
    return [
      row("i2c-sda", freeLabel(t, bus === 0 ? "SDA" : `SDA${bus}`), wired?.sda ?? null, bus),
      row("i2c-scl", freeLabel(t, bus === 0 ? "SCL" : `SCL${bus}`), wired?.scl ?? null, bus),
    ];
  }
  const used = new Set(t.rows.map((r) => r.pin));
  const pick = (signal: "sck" | "mosi" | "miso") =>
    info?.gpios.find((g) => g.spi?.signal === signal && g.spi.block === 0 && !g.reserved && !used.has(g.pin))?.pin ?? null;
  const sck = pick("sck");
  // On the RP chips CS sits next to SCK (GPIO 17 beside 18); elsewhere it is the user's choice.
  const cs = sck !== null && info?.spiRouting === "rp-function-table" && !used.has(sck - 1) ? sck - 1 : null;
  return [
    row("spi-sck", freeLabel(t, "SCK"), sck),
    row("spi-mosi", freeLabel(t, "MOSI"), pick("mosi")),
    row("spi-miso", freeLabel(t, "MISO"), pick("miso")),
    row("spi-cs", freeLabel(t, "CS"), cs),
  ];
}

export function pinTableView(ev: PinTableEvents): PinTableView {
  let table: PinTable = { rows: [], buses: [] };
  let target: TargetId = "esp32";
  let info: BoardPins | null = null;

  const heading = h("h3", { id: "pins-heading" }, "Pins");
  const intro = h("p", { class: "muted" });
  const summary = h("p", { class: "pin-summary", role: "status" });
  const tbody = h("tbody");
  const tableEl = h(
    "table",
    { class: "pin-table", "aria-describedby": "pins-keys" },
    h("caption", { class: "visually-hidden" }, "Pin table: one row per pin the payload uses"),
    h(
      "thead",
      {},
      h(
        "tr",
        {},
        ...["Label", "Pin", "Function", "Bus", "Pull", "Starts", "Note", ""].map((t, i) =>
          h("th", { scope: "col", ...(i === 7 ? { "aria-label": "Remove" } : {}) }, t),
        ),
      ),
    ),
    tbody,
  );
  const scroller = h("div", { class: "pin-scroll" }, tableEl);
  const keys = h(
    "p",
    { class: "muted small", id: "pins-keys" },
    "Keys: Tab moves across; Ctrl + arrow keys move between cells; Up and Down move between rows in text cells.",
  );
  const tableIssues = h("ul", { class: "issues pin-table-issues" });
  const busesEl = h("div", { class: "pin-buses" });
  const addSel = h("select", { "aria-label": "Add a pin" });
  const empty = h("p", { class: "muted" });

  /** The label being typed in: its row and the label its blocks have. */
  let edit: { row: number; applied: string } | null = null;
  tableEl.addEventListener("focusin", (e) => {
    const t = e.target as HTMLElement;
    if (edit && !(t.dataset.c === "label" && Number(t.dataset.r) === edit.row)) edit = null;
  });

  /** Cell to focus after the next render. */
  let pendingFocus: { r: number; c: Col; start?: number | null; end?: number | null } | null = null;
  function focusCell(r: number, c: Col) {
    pendingFocus = { r, c };
  }

  function change(next: PinTable, rename?: { from: string; to: string }) {
    ev.onChange(next, rename);
  }

  function renderAdd() {
    const opts: [string, string][] = [
      ["", "Add a pin..."],
      ["gpio-out", "GPIO output"],
      ["gpio-in", "GPIO input"],
      ["adc", "Analog input (ADC)"],
      ["pwm", "PWM output"],
    ];
    for (let b = 0; b < (info?.i2cBuses ?? 0); b++) {
      if (!table.rows.some((r) => busKind(r.func) === "i2c" && r.bus === b)) opts.push([`i2c:${b}`, `I2C bus ${b} (SDA + SCL)`]);
    }
    if (info && !table.rows.some((r) => busKind(r.func) === "spi")) opts.push(["spi:0", "SPI bus 0 (SCK, MOSI, MISO, CS)"]);
    const defaults = boardDefaults(target).rows.filter((d) => !findRow(table, d.label));
    if (defaults.length > 0) opts.push(["defaults", `Board defaults (${defaults.map((d) => d.label).join(", ")})`]);
    addSel.replaceChildren(...opts.map(([v, t]) => option(v, t, v === "")));
  }

  addSel.addEventListener("change", () => {
    const v = addSel.value;
    addSel.value = "";
    if (!v) return;
    focusCell(table.rows.length, "label");
    if (v === "defaults") {
      change(addRows(table, ...boardDefaults(target).rows.filter((d) => !findRow(table, d.label))));
    } else if (v.includes(":")) {
      const [kind, bus] = v.split(":");
      change(addRows(table, ...busRows(kind === "i2c" ? "i2c" : "spi", Number(bus), table, info)));
    } else {
      const func = v as PinFunc;
      const bases: Partial<Record<PinFunc, string>> = { "gpio-out": "OUT", "gpio-in": "BUTTON", adc: "ANALOG", pwm: "PWM" };
      change(addRows(table, row(func, freeLabel(table, bases[func] ?? "PIN"))));
    }
  });

  function cellId(r: number, c: Col) {
    return `pin-${r}-${c}`;
  }

  function control<E extends HTMLElement>(r: number, c: Col, el: E, label: string, described: string | null): E {
    el.id = cellId(r, c);
    el.dataset.r = String(r);
    el.dataset.c = c;
    el.setAttribute("aria-label", label);
    if (described) el.setAttribute("aria-describedby", described);
    return el;
  }

  function issueItem(x: PinIssue): HTMLElement {
    const li = h("li", { class: "error" }, x.message);
    if (x.fix) {
      const fix = x.fix;
      const text = fix.kind === "pin" ? `Use GPIO ${fix.pin}` : `Add ${FUNC_LABELS[fix.func]}${fix.pin !== null ? ` on GPIO ${fix.pin}` : ""}`;
      const b = h("button", { type: "button", class: "fix" }, text);
      b.addEventListener("click", () => {
        focusCell(x.row ?? table.rows.length, x.row === null ? "label" : "pin");
        change(applyFix(table, x.row, fix));
      });
      li.append(" ", b);
    }
    return li;
  }

  function rowEls(r: PinRow, i: number, rowIssues: readonly PinIssue[]): HTMLElement[] {
    const name = r.label || `row ${i + 1}`;
    const issueId = rowIssues.length ? `pin-issues-${i}` : null;
    const mark = <E extends HTMLElement>(el: E, f: PinIssue["field"]): E => {
      if (rowIssues.some((x) => x.field === f)) el.setAttribute("aria-invalid", "true");
      return el;
    };

    const label = control(i, "label", h("input", { type: "text", value: r.label, spellcheck: "false", autocomplete: "off", maxlength: 24 }), `Label, row ${i + 1}`, issueId);
    label.addEventListener("input", () => {
      // Blocks follow a rename as it is typed: from the last label they
      // were given to the new one, whenever the new one is a valid label
      // no other row has (so an intermediate "LED" never merges LED2's
      // blocks into LED's).
      if (edit?.row !== i) edit = { row: i, applied: r.label };
      const v = label.value;
      const free = !table.rows.some((x, j) => j !== i && x.label.toLowerCase() === v.toLowerCase());
      let rename: { from: string; to: string } | undefined;
      if (LABEL_RE.test(v) && free && edit.applied && v !== edit.applied) {
        rename = { from: edit.applied, to: v };
        edit.applied = v;
      }
      change(setRow(table, i, { label: v }), rename);
    });

    const pin = control(i, "pin", h("select"), `Pin of ${name}`, issueId);
    const gpios = info?.gpios ?? [...Array(49).keys()].map((p): GpioInfo => ({ pin: p, inputOnly: false }));
    const opts = [option("", "choose...", r.pin === null), ...gpios.map((g) => option(String(g.pin), pinText(g), g.pin === r.pin))];
    if (r.pin !== null && !gpios.some((g) => g.pin === r.pin)) opts.push(option(String(r.pin), `GPIO ${r.pin} (not on this board)`, true));
    pin.replaceChildren(...opts);
    pin.addEventListener("change", () => change(setRow(table, i, { pin: pin.value === "" ? null : Number(pin.value) })));

    const func = control(i, "func", h("select"), `Function of ${name}`, issueId);
    func.replaceChildren(...PIN_FUNCS.map((f) => option(f, FUNC_LABELS[f], f === r.func)));
    func.addEventListener("change", () => change(setRow(table, i, { func: func.value as PinFunc, bus: 0 })));

    const kind = busKind(r.func);
    let bus: HTMLSelectElement | null = null;
    if (kind) {
      const count = Math.max(kind === "i2c" ? (info?.i2cBuses ?? 2) : (info?.spiBuses ?? 1), r.bus + 1);
      const sel = control(i, "bus", h("select"), `Bus of ${name}`, issueId);
      sel.replaceChildren(...[...Array(count).keys()].map((b) => option(String(b), String(b), b === r.bus)));
      sel.addEventListener("change", () => change(setRow(table, i, { bus: Number(sel.value) })));
      bus = sel;
    }

    let pull: HTMLSelectElement | null = null;
    if (r.func === "gpio-in") {
      const sel = control(i, "pull", h("select"), `Pull of ${name}`, issueId);
      sel.replaceChildren(...PULLS.map((p) => option(p, p === "none" ? "none" : `pull-${p}`, p === r.pull)));
      sel.addEventListener("change", () => change(setRow(table, i, { pull: sel.value as PinRow["pull"] })));
      pull = sel;
    }

    let initial: HTMLSelectElement | null = null;
    if (r.func === "gpio-out") {
      const sel = control(i, "initial", h("select"), `Starting level of ${name}`, issueId);
      sel.replaceChildren(option("0", "low", r.initial === 0), option("1", "high", r.initial === 1));
      sel.addEventListener("change", () => change(setRow(table, i, { initial: sel.value === "1" ? 1 : 0 })));
      initial = sel;
    }

    const note = control(i, "note", h("input", { type: "text", value: r.note, maxlength: 200 }), `Note for ${name}`, null);
    note.addEventListener("input", () => change(setRow(table, i, { note: note.value })));

    const remove = control(i, "remove", h("button", { type: "button", class: "icon" }, "×"), `Remove ${name}`, null);
    remove.addEventListener("click", () => {
      focusCell(Math.max(0, Math.min(i, table.rows.length - 2)), "remove");
      change(removeRow(table, i));
    });

    const dash = () => h("span", { class: "muted", "aria-hidden": "true" }, "—");
    const out: HTMLElement[] = [
      h(
        "tr",
        { class: rowIssues.length ? "has-issue" : "" },
        h("td", { class: "sticky" }, mark(label, "label")),
        h("td", {}, mark(pin, "pin")),
        h("td", {}, mark(func, "func")),
        h("td", {}, bus ? mark(bus, "bus") : dash()),
        h("td", {}, pull ?? dash()),
        h("td", {}, initial ?? dash()),
        h("td", {}, note),
        h("td", {}, remove),
      ),
    ];
    if (issueId) {
      out.push(
        h(
          "tr",
          { class: "pin-issue-row" },
          h("td", { colspan: COLS.length }, h("ul", { class: "issues", id: issueId }, ...rowIssues.map(issueItem))),
        ),
      );
    }
    return out;
  }

  function busControls(s: BusSetting): HTMLElement {
    if (s.kind === "i2c") {
      const hz = h("input", { type: "number", value: s.hz, min: info?.i2cHz[0] ?? 10000, max: info?.i2cHz[1] ?? 1000000, step: 1000, inputmode: "numeric" });
      hz.addEventListener("change", () => change(setBus(table, { ...s, hz: Math.trunc(Number(hz.value)) })));
      return h("p", { class: "row" }, h("label", {}, `I2C bus ${s.bus} clock (Hz) `, hz));
    }
    const khz = h("input", { type: "number", value: s.khz, min: 1, max: info?.maxSpiKhz ?? 40000, inputmode: "numeric" });
    khz.addEventListener("change", () => change(setBus(table, { ...s, khz: Math.trunc(Number(khz.value)) })));
    const mode = h("select", {}, ...[0, 1, 2, 3].map((m) => option(String(m), `mode ${m}`, m === s.mode)));
    mode.addEventListener("change", () => change(setBus(table, { ...s, mode: Number(mode.value) })));
    return h("p", { class: "row" }, h("label", {}, `SPI bus ${s.bus} clock (kHz) `, khz), h("label", {}, "Mode ", mode));
  }

  // Keyboard movement between cells.
  tableEl.addEventListener("keydown", (e) => {
    const t = e.target as HTMLElement;
    const r = Number(t.dataset.r);
    const c = COLS.indexOf(t.dataset.c as Col);
    if (Number.isNaN(r) || c < 0) return;
    const text = t instanceof HTMLInputElement && t.type === "text";
    let dr = 0;
    let dc = 0;
    if (e.ctrlKey || e.metaKey) {
      if (e.key === "ArrowUp") dr = -1;
      else if (e.key === "ArrowDown") dr = 1;
      else if (e.key === "ArrowLeft") dc = -1;
      else if (e.key === "ArrowRight") dc = 1;
    } else if (text && !e.altKey && !e.shiftKey) {
      if (e.key === "ArrowUp") dr = -1;
      else if (e.key === "ArrowDown") dr = 1;
    }
    if (dr === 0 && dc === 0) return;
    // The next cell with a control (some cells show a dash); moving rows
    // into a dash cell lands on that row's label.
    for (let k = 1; k <= COLS.length; k++) {
      const nr = r + dr * k;
      const nc = c + dc * k;
      if (nr < 0 || nr >= table.rows.length || nc < 0 || nc >= COLS.length) break;
      const el = document.getElementById(cellId(nr, COLS[nc] as Col)) ?? (dr !== 0 ? document.getElementById(cellId(nr, "label")) : null);
      if (el) {
        e.preventDefault();
        el.focus();
        return;
      }
    }
  });

  function render(t: TargetId, next: PinTable, issues: readonly PinIssue[], board: BoardPins | null) {
    // Remember the focused cell (and caret) to restore it.
    const active = document.activeElement as HTMLElement | null;
    if (!pendingFocus && active && tableEl.contains(active) && active.dataset.r !== undefined) {
      const input = active instanceof HTMLInputElement ? active : null;
      pendingFocus = { r: Number(active.dataset.r), c: active.dataset.c as Col, start: input?.selectionStart, end: input?.selectionEnd };
    }
    target = t;
    table = next;
    info = board;
    const b = boardOf(t);
    heading.textContent = `Pins for the ${targetLabel(t)}`;
    intro.textContent = b
      ? "Name each pin once here; blocks use the label (write LED, toggle LED) and the generated init sets the pins up. " +
        (b.runsPayloads ? "" : `The ${b.label} firmware stores payloads but does not run them. `) +
        "The checks are the firmware's own pin rules."
      : "The badge firmware gives payloads no GPIO: use the badge blocks for its LEDs and buttons.";
    tbody.replaceChildren(...next.rows.flatMap((r, i) => rowEls(r, i, issues.filter((x) => x.row === i))));
    tableEl.hidden = next.rows.length === 0;
    empty.hidden = next.rows.length > 0;
    empty.textContent = b ? "No pins yet. Add one below, or add the board defaults." : "";
    tableIssues.replaceChildren(...issues.filter((x) => x.row === null).map(issueItem));
    busesEl.replaceChildren(...next.buses.map(busControls));
    const n = issues.length;
    const count = `${next.rows.length} pin${next.rows.length === 1 ? "" : "s"}`;
    summary.textContent = next.rows.length === 0 && n === 0 ? "" : n === 0 ? `${count}, no problems.` : `${n} problem${n === 1 ? "" : "s"} to fix before compiling.`;
    summary.className = `pin-summary ${n ? "bad" : "good"}`;
    renderAdd();
    addSel.disabled = !b;
    if (pendingFocus) {
      const f = pendingFocus;
      pendingFocus = null;
      const el = document.getElementById(cellId(f.r, f.c)) ?? document.getElementById(cellId(f.r, "label"));
      if (el) {
        el.focus();
        if (el instanceof HTMLInputElement && el.type === "text" && f.start != null) el.setSelectionRange(f.start, f.end ?? f.start);
      }
    }
  }

  const el = h("div", { class: "pins-view" }, heading, intro, summary, scroller, empty, tableIssues, busesEl, h("p", { class: "row" }, addSel), keys);
  return { el, render };
}
