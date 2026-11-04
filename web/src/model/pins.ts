// The pin table: the payload's pins by label, what each one does, and the
// bus settings. Pure data and transitions (every change returns a new
// table); what is allowed in a table is not decided here but by
// protocol-wasm `checkPins` (Rust: the board-pins rules the firmwares
// apply plus the ABI's i2c_open / spi_open argument rules), reached
// through a `PinChecker`.
//
// Blocks (and later flows) name a pin by its label, so the logic does not
// change when the table moves the LED from GPIO 8 to GPIO 25: the
// generator emits `const PIN_LED: i32 = 25;` and the `init` setup from the
// table (generator.ts).

import { PIN_LABEL_BLOCKS } from "../blocks/defs";
import { type WorkspaceJson, everyBlock } from "../blocks/workspace";
import { type TargetId, boardOf, chipOf } from "./target";

export const PIN_FUNCS = ["gpio-out", "gpio-in", "adc", "pwm", "i2c-sda", "i2c-scl", "spi-sck", "spi-mosi", "spi-miso", "spi-cs"] as const;
export type PinFunc = (typeof PIN_FUNCS)[number];

/** How the page names each function. */
export const FUNC_LABELS: Readonly<Record<PinFunc, string>> = {
  "gpio-out": "GPIO output",
  "gpio-in": "GPIO input",
  adc: "Analog input (ADC)",
  pwm: "PWM output",
  "i2c-sda": "I2C SDA",
  "i2c-scl": "I2C SCL",
  "spi-sck": "SPI SCK (clock)",
  "spi-mosi": "SPI MOSI (data out)",
  "spi-miso": "SPI MISO (data in)",
  "spi-cs": "SPI CS (chip select)",
};

export const PULLS = ["none", "up", "down"] as const;
export type Pull = (typeof PULLS)[number];

/** The ABI's pull codes for gpio_input. */
export const PULL_CODES: Readonly<Record<Pull, number>> = { none: 0, up: 1, down: 2 };

export interface PinRow {
  /** The name blocks use, `[A-Za-z][A-Za-z0-9_]*` (checked by checkPins). */
  label: string;
  /** The GPIO, or null while not chosen. */
  pin: number | null;
  func: PinFunc;
  /** Bus number for I2C and SPI rows; 0 otherwise. */
  bus: number;
  /** For GPIO inputs. */
  pull: Pull;
  /** Level a GPIO output starts at. */
  initial: 0 | 1;
  /** Free text for the reader. */
  note: string;
}

export type BusSetting = { kind: "i2c"; bus: number; hz: number } | { kind: "spi"; bus: number; khz: number; mode: number };

export interface PinTable {
  rows: readonly PinRow[];
  /** One setting per bus in use (`normalizeBuses`). */
  buses: readonly BusSetting[];
}

export const EMPTY_PINS: PinTable = { rows: [], buses: [] };

/** Standard-mode I2C. */
export const DEFAULT_I2C_HZ = 100_000;
export const DEFAULT_SPI_KHZ = 1_000;

/** One problem `checkPins` found (protocol-wasm `pins::Issue`). */
export interface PinIssue {
  /** Index into `rows`, or null for the table or a bus setting. */
  row: number | null;
  field: "label" | "pin" | "func" | "bus" | "setting" | "table";
  message: string;
  fix?: PinFix;
}

export type PinFix = { kind: "pin"; pin: number } | { kind: "add"; func: PinFunc; bus: number; pin: number | null };

/** protocol-wasm `checkPins`. */
export type PinChecker = (chip: string, rows: readonly PinRow[], buses: readonly BusSetting[]) => PinIssue[];

export function busKind(func: PinFunc): "i2c" | "spi" | null {
  return func.startsWith("i2c-") ? "i2c" : func.startsWith("spi-") ? "spi" : null;
}

export function row(func: PinFunc, label: string, pin: number | null = null, bus = 0, note = ""): PinRow {
  return { label, pin, func, bus, pull: "none", initial: 0, note };
}

/** The bus settings exactly for the buses the rows use: existing values
 * kept, defaults for new buses, unused ones dropped, ordered I2C then SPI
 * by bus. */
export function normalizeBuses(t: PinTable): PinTable {
  const used = new Map<string, BusSetting>();
  for (const r of t.rows) {
    const kind = busKind(r.func);
    if (!kind) continue;
    const key = `${kind}${r.bus}`;
    if (used.has(key)) continue;
    const prev = t.buses.find((b) => b.kind === kind && b.bus === r.bus);
    used.set(key, prev ?? (kind === "i2c" ? { kind, bus: r.bus, hz: DEFAULT_I2C_HZ } : { kind, bus: r.bus, khz: DEFAULT_SPI_KHZ, mode: 0 }));
  }
  const buses = [...used.values()].sort((a, b) => (a.kind === b.kind ? a.bus - b.bus : a.kind === "i2c" ? -1 : 1));
  return { rows: t.rows, buses };
}

export function setRow(t: PinTable, i: number, patch: Partial<PinRow>): PinTable {
  if (i < 0 || i >= t.rows.length) return t;
  return normalizeBuses({ ...t, rows: t.rows.map((r, j) => (j === i ? { ...r, ...patch } : r)) });
}

export function addRows(t: PinTable, ...rows: PinRow[]): PinTable {
  return normalizeBuses({ ...t, rows: [...t.rows, ...rows] });
}

export function removeRow(t: PinTable, i: number): PinTable {
  return normalizeBuses({ ...t, rows: t.rows.filter((_, j) => j !== i) });
}

export function setBus(t: PinTable, s: BusSetting): PinTable {
  return { ...t, buses: t.buses.map((b) => (b.kind === s.kind && b.bus === s.bus ? s : b)) };
}

/** `base`, or `base2`, `base3`, ... : the first label no row has (case
 * ignored, as checkPins compares them). */
export function freeLabel(t: PinTable, base: string): string {
  const taken = new Set(t.rows.map((r) => r.label.toLowerCase()));
  if (!taken.has(base.toLowerCase())) return base;
  for (let i = 2; i < 100; i++) if (!taken.has(`${base}${i}`.toLowerCase())) return `${base}${i}`;
  return `${base}_${t.rows.length}`;
}

/** The row with exactly this label. */
export function findRow(t: PinTable, label: string): PinRow | undefined {
  return t.rows.find((r) => r.label === label);
}

/** A board's starting table: the LED (the Blink example's pin). The badge
 * gives payloads no pins. */
export function boardDefaults(target: TargetId): PinTable {
  const b = boardOf(target);
  if (!b) return EMPTY_PINS;
  return { rows: [row("gpio-out", "LED", b.blink.pin, 0, b.blink.note)], buses: [] };
}

function sameTable(a: PinTable, b: PinTable): boolean {
  return JSON.stringify(normalizeBuses(a)) === JSON.stringify(normalizeBuses(b));
}

/** The table after switching from target `from` to `to`: a table that is
 * empty or still the old board's defaults becomes the new board's
 * defaults; anything the user changed is kept (and re-checked). */
export function seedForTarget(t: PinTable, from: TargetId, to: TargetId): PinTable {
  if (t.rows.length === 0 || sameTable(t, boardDefaults(from))) return boardDefaults(to);
  return t;
}

/** A pin an example needs by label. */
export interface PinNeed {
  label: string;
  func: PinFunc;
}

/** Rows the board suggests for labels examples use: its defaults (LED)
 * plus BUTTON, a GPIO input with the pull-up on at the board's button
 * pin (onboarding/boards.ts `button`). Not a starting table: only
 * `withNeeds` adds from it, and only labels an example needs. */
export function boardSuggestions(target: TargetId): PinTable {
  const b = boardOf(target);
  if (!b) return EMPTY_PINS;
  return { rows: [...boardDefaults(target).rows, { ...row("gpio-in", "BUTTON", b.button.pin, 0, b.button.note), pull: "up" }], buses: [] };
}

/** The table with every needed label present: a missing one is added from
 * the board's suggestions (`boardSuggestions`; unassigned when the board
 * suggests nothing for it). Rows already
 * there are never changed, so loading an example never loses a pin
 * choice. */
export function withNeeds(t: PinTable, target: TargetId, needs: readonly PinNeed[]): PinTable {
  const defaults = boardSuggestions(target);
  // A label needed twice is added once.
  const missing = needs.filter((n, i) => !findRow(t, n.label) && needs.findIndex((x) => x.label === n.label) === i);
  if (missing.length === 0) return t;
  return addRows(t, ...missing.map((n) => findRow(defaults, n.label) ?? row(n.func, n.label)));
}

/** Applies a check's one-click fix to row `i`. */
export function applyFix(t: PinTable, i: number | null, fix: PinFix): PinTable {
  if (fix.kind === "pin") return i === null ? t : setRow(t, i, { pin: fix.pin });
  // "i2c-scl" -> SCL, "spi-mosi" -> MOSI.
  const label = freeLabel(t, fix.func.slice(4).toUpperCase());
  return addRows(t, row(fix.func, label, fix.pin, fix.bus));
}

/** Checks the table for a target. The badge has no pins, so any row is an
 * issue; otherwise `checker` (protocol-wasm) decides. */
export function checkTable(checker: PinChecker, target: TargetId, t: PinTable): PinIssue[] {
  const chip = chipOf(target);
  if (chip === null) {
    return t.rows.length === 0
      ? []
      : [{ row: null, field: "table", message: "The badge firmware gives payloads no pins; remove these rows or pick another board." }];
  }
  return checker(chip, t.rows, t.buses);
}

/** Labels the workspace's pin blocks name, sorted. */
export function labelsUsed(ws: WorkspaceJson): string[] {
  const out: string[] = [];
  for (const b of everyBlock(ws)) {
    if (!PIN_LABEL_BLOCKS.includes(b.type)) continue;
    const v = b.fields?.LABEL;
    if (typeof v === "string" && v !== "" && !out.includes(v)) out.push(v);
  }
  return out.sort();
}

/** The workspace with every pin block naming `from` renamed to `to` (the
 * pin table renamed a label). A deep copy; nothing else changes. */
export function renameLabel(ws: WorkspaceJson, from: string, to: string): WorkspaceJson {
  const copy = structuredClone(ws);
  for (const b of everyBlock(copy)) {
    if (PIN_LABEL_BLOCKS.includes(b.type) && b.fields?.LABEL === from) b.fields.LABEL = to;
  }
  return copy;
}
