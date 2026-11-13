// A keyboard-editable grid: the behaviour the Pins and States views share.
// A grid is a real <table> whose cells hold labelled form controls; this
// module gives them ids and keyboard movement, and keeps focus across the
// full re-render those views do on every change.
//
// - Ids: the control of row `r`, column `c` is `<prefix>-<r>-<c>`; a cell
//   with several controls (the States view's guard and action editors)
//   gives its first control that id and the others `<prefix>-<r>-<c>-<sub>`.
//   The issue list under row `r` is `<prefix>-issues-<r>`. The prefix lets
//   several grids live on one page.
// - `control()` sets the id, `data-r` / `data-c`, the aria-label and
//   aria-describedby (pointing at the row's issue list).
// - Keys (`attachKeys`): Ctrl/Meta + arrow keys move between cells in any
//   direction, skipping cells without a control (a dash); moving to another
//   row onto such a cell lands on that row's `home` column. Up / Down in a
//   text input move between rows. Tab is left to the browser.
// - Focus (`focusKeeper`): before a re-render `remember()` notes the focused
//   control's id and caret (or a cell asked for with `want()`); after it,
//   `restore()` focuses that id, else the control's cell, else its row's
//   home cell.
// - `issueRow()` is the row under a table row holding its issue list, in
//   one cell spanning every column (or `span`, for a table with a header
//   column).

import { h } from "./dom";

export interface Grid<C extends string> {
  readonly cols: readonly C[];
  cellId(r: number, c: C, sub?: string): string;
  issuesId(r: number): string;
  /** Labels `el` as the control of cell (r, c) (or its `sub` control). */
  control<E extends HTMLElement>(r: number, c: C, el: E, label: string, described: string | null, sub?: string): E;
  /** A row holding `items` in a `<ul class="issues">` under row `r`, in
   * one cell spanning `span` columns (default: every column). */
  issueRow(r: number, items: readonly HTMLElement[], rowClass: string, span?: number): HTMLTableRowElement;
  /** Ctrl/Meta + arrows and Up/Down in text inputs, for `rows()` rows. */
  attachKeys(table: HTMLElement, rows: () => number): void;
}

export function grid<C extends string>(prefix: string, cols: readonly C[], home: C): Grid<C> {
  const cellId = (r: number, c: C, sub?: string) => (sub ? `${prefix}-${r}-${c}-${sub}` : `${prefix}-${r}-${c}`);
  return {
    cols,
    cellId,
    issuesId: (r) => `${prefix}-issues-${r}`,
    control(r, c, el, label, described, sub) {
      el.id = cellId(r, c, sub);
      el.dataset.r = String(r);
      el.dataset.c = c;
      el.dataset.cell = cellId(r, c);
      el.dataset.home = cellId(r, home);
      el.setAttribute("aria-label", label);
      if (described) el.setAttribute("aria-describedby", described);
      return el;
    },
    issueRow(r, items, rowClass, span = cols.length) {
      return h("tr", { class: rowClass }, h("td", { colspan: span }, h("ul", { class: "issues", id: `${prefix}-issues-${r}` }, ...items)));
    },
    attachKeys(table, rows) {
      table.addEventListener("keydown", (e) => {
        const t = e.target as HTMLElement;
        const r = Number(t.dataset.r);
        const c = cols.indexOf(t.dataset.c as C);
        if (Number.isNaN(r) || c < 0 || !t.id.startsWith(`${prefix}-`)) return;
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
        // into a dash cell lands on that row's home cell.
        const n = rows();
        for (let k = 1; k <= Math.max(cols.length, 1); k++) {
          const nr = r + dr * k;
          const nc = c + dc * k;
          if (nr < 0 || nr >= n || nc < 0 || nc >= cols.length) break;
          const el = document.getElementById(cellId(nr, cols[nc] as C)) ?? (dr !== 0 ? document.getElementById(cellId(nr, home)) : null);
          if (el) {
            e.preventDefault();
            el.focus();
            return;
          }
        }
      });
    },
  };
}

export interface FocusKeeper {
  /** Focus this id (with fallbacks) after the next render. */
  want(id: string, ...fallbacks: string[]): void;
  /** Before re-rendering: note the focused control inside `root`, unless
   * a `want` is pending. */
  remember(): void;
  /** After re-rendering: focus what was noted, restoring a text caret. */
  restore(): void;
}

export function focusKeeper(root: HTMLElement): FocusKeeper {
  let pending: { ids: string[]; start?: number | null; end?: number | null } | null = null;
  return {
    want(id, ...fallbacks) {
      pending = { ids: [id, ...fallbacks] };
    },
    remember() {
      const a = document.activeElement as HTMLElement | null;
      if (pending || !a || !root.contains(a) || !a.id) return;
      const input = a instanceof HTMLInputElement && a.type === "text" ? a : null;
      const ids = [a.id, a.dataset.cell, a.dataset.home].filter((x): x is string => !!x);
      pending = { ids, start: input?.selectionStart, end: input?.selectionEnd };
    },
    restore() {
      const f = pending;
      pending = null;
      if (!f) return;
      for (const id of f.ids) {
        const el = document.getElementById(id);
        if (!el || !root.contains(el)) continue;
        el.focus();
        if (el instanceof HTMLInputElement && el.type === "text" && f.start != null && id === f.ids[0]) el.setSelectionRange(f.start, f.end ?? f.start);
        return;
      }
    },
  };
}
