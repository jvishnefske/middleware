// Geometry and wording of the simulation plots (silPanel.ts): where a time
// and a value land on the drawing, the path of a sample-and-hold trace,
// the value a trace holds at a time, axis ticks, and the numbers as the
// panel prints them. Pure functions, no DOM, so they are unit tested as
// they are.
//
// The plot is a stack of lanes sharing one time axis: a digital trace
// gets a low fixed lane, a numeric trace a taller one on its own scale.
// Each lane has its label on a line above its band.

import type { Trace, TracePoint } from "../mwctl/sil";

export const PLOT_PAD_X = 8;
export const LANE_LABEL_H = 16;
export const DIGITAL_H = 18;
export const NUMERIC_H = 56;
export const LANE_GAP = 6;
export const AXIS_H = 22;
/** Kept clear inside a band above its highest and below its lowest value. */
export const BAND_INSET = 3;
/** The width drawn when the panel has not been laid out yet. */
export const DEFAULT_PLOT_WIDTH = 640;
export const MIN_PLOT_WIDTH = 160;

/** Maps `v` from [`d0`, `d1`] to [`r0`, `r1`]; the middle of the range
 * when the domain is a single value. */
export function scale(v: number, d0: number, d1: number, r0: number, r1: number): number {
  if (d1 === d0) return (r0 + r1) / 2;
  return r0 + ((v - d0) / (d1 - d0)) * (r1 - r0);
}

export interface Lane {
  trace: Trace;
  /** Top of the label line. */
  top: number;
  /** The band the trace is drawn in. */
  bandTop: number;
  bandHeight: number;
  /** The trace's own value range (0..1 for a digital trace). */
  min: number;
  max: number;
}

export interface PlotLayout {
  width: number;
  height: number;
  /** The plot area's left and right edge. */
  x0: number;
  x1: number;
  /** The time axis: virtual milliseconds at `x0` and at `x1`. */
  t0: number;
  t1: number;
  /** Top of the axis line, below the last lane. */
  axisY: number;
  lanes: Lane[];
}

/** The lowest and highest value of a trace; 0..0 when it has no points. */
export function valueRange(points: readonly TracePoint[]): { min: number; max: number } {
  if (points.length === 0) return { min: 0, max: 0 };
  let min = points[0].v;
  let max = points[0].v;
  for (const p of points) {
    if (p.v < min) min = p.v;
    if (p.v > max) max = p.v;
  }
  return { min, max };
}

/** Lanes for `traces` in a drawing `width` wide, over the time from 0 to
 * `endMs` (or the last point, when that is later; at least 1 ms). */
export function layoutPlot(traces: readonly Trace[], width: number, endMs: number): PlotLayout {
  const w = Math.max(MIN_PLOT_WIDTH, Math.floor(width));
  let t1 = endMs > 0 ? endMs : 0;
  for (const trace of traces) {
    const last = trace.points[trace.points.length - 1];
    if (last && last.t > t1) t1 = last.t;
  }
  if (t1 <= 0) t1 = 1;
  let y = 4;
  const lanes: Lane[] = [];
  for (const trace of traces) {
    const bandHeight = trace.digital ? DIGITAL_H : NUMERIC_H;
    const range = trace.digital ? { min: 0, max: 1 } : valueRange(trace.points);
    lanes.push({ trace, top: y, bandTop: y + LANE_LABEL_H, bandHeight, ...range });
    y += LANE_LABEL_H + bandHeight + LANE_GAP;
  }
  return { width: w, height: y + AXIS_H, x0: PLOT_PAD_X, x1: w - PLOT_PAD_X, t0: 0, t1, axisY: y, lanes };
}

export function timeToX(layout: PlotLayout, tMs: number): number {
  return scale(tMs, layout.t0, layout.t1, layout.x0, layout.x1);
}

/** The time under `x`, kept within the axis. */
export function xToTime(layout: PlotLayout, x: number): number {
  const t = scale(x, layout.x0, layout.x1, layout.t0, layout.t1);
  return Math.max(layout.t0, Math.min(layout.t1, t));
}

/** Where `v` is in a lane: its maximum near the band's top, its minimum
 * near the bottom, a constant trace through the middle. */
export function valueToY(lane: Lane, v: number): number {
  return scale(v, lane.min, lane.max, lane.bandTop + lane.bandHeight - BAND_INSET, lane.bandTop + BAND_INSET);
}

const r1 = (n: number) => String(Math.round(n * 10) / 10);

/** The SVG path of a sample-and-hold trace: each value runs level to the
 * next point's time, then jumps, and the last one runs on to `endX`.
 * Empty for no points. */
export function stepPath(points: readonly TracePoint[], x: (tMs: number) => number, y: (v: number) => number, endX: number): string {
  if (points.length === 0) return "";
  const parts = [`M${r1(x(points[0].t))} ${r1(y(points[0].v))}`];
  let lastY = y(points[0].v);
  for (let i = 1; i < points.length; i++) {
    const py = y(points[i].v);
    parts.push(`H${r1(x(points[i].t))}`);
    if (py !== lastY) parts.push(`V${r1(py)}`);
    lastY = py;
  }
  const lastX = x(points[points.length - 1].t);
  // A single point still shows: a level line to the end, or a short dash.
  parts.push(`H${r1(endX > lastX ? endX : lastX + 1)}`);
  return parts.join("");
}

/** Index of the point whose value holds at `tMs` (the last one at or
 * before it), by binary search; -1 before the first point. */
export function sampleIndex(points: readonly TracePoint[], tMs: number): number {
  let lo = 0;
  let hi = points.length;
  while (lo < hi) {
    const mid = (lo + hi) >>> 1;
    if (points[mid].t <= tMs) lo = mid + 1;
    else hi = mid;
  }
  return lo - 1;
}

/** The value a trace holds at `tMs`; null before its first point. */
export function valueAt(points: readonly TracePoint[], tMs: number): number | null {
  const i = sampleIndex(points, tMs);
  return i < 0 ? null : points[i].v;
}

/** At most `max` round times (1, 2 or 5 × a power of ten apart) from
 * `t0` to `t1`. */
export function timeTicks(t0: number, t1: number, max: number): number[] {
  const span = t1 - t0;
  if (!(span > 0) || max < 1) return [t0];
  const rough = span / Math.max(1, max);
  const pow = 10 ** Math.floor(Math.log10(rough));
  const step = [1, 2, 5, 10].map((m) => m * pow).find((s) => s >= rough) ?? 10 * pow;
  const ticks: number[] = [];
  for (let k = Math.ceil(t0 / step); k * step <= t1 + step * 1e-9; k++) ticks.push(Math.round(k * step * 1e6) / 1e6);
  return ticks;
}

const trim = (text: string) => (text.includes(".") ? text.replace(/0+$/, "").replace(/\.$/, "") : text);

/** A time for the axis and the readout: seconds when the axis spans at
 * least one, milliseconds below ("1.25 s", "250 ms"). */
export function formatTime(tMs: number, spanMs: number): string {
  if (spanMs >= 1000) return `${trim((tMs / 1000).toFixed(3))} s`;
  return `${trim(tMs.toFixed(2))} ms`;
}

/** A value: an integer as it is, anything else to three decimals. */
export function formatValue(v: number): string {
  return Number.isInteger(v) ? String(v) : trim(v.toFixed(3));
}

/** A trace's value in words: a pin level is "high" or "low". */
export function describeValue(trace: Trace, v: number | null): string {
  if (v === null) return "–";
  if (trace.kind === "input" || trace.kind === "output") return v >= 0.5 ? "high" : "low";
  return formatValue(v);
}

/** `text`, cut to `maxChars` with an ellipsis. */
export function fitLabel(text: string, maxChars: number): string {
  const n = Math.max(1, Math.floor(maxChars));
  return text.length <= n ? text : `${text.slice(0, n - 1)}…`;
}

/** "146×", "12×", "2.5×": how many times real time. */
export function formatSpeedup(speedup: number): string {
  return `${speedup >= 10 ? Math.round(speedup) : Math.round(speedup * 10) / 10}×`;
}

/** The status line: "Simulated 60.0 s in 0.41 s (146× real time)". */
export function statusText(p: { simulatedMs: number; wallMs: number; speedup: number }, state: "running" | "done" | "stopped" = "done"): string {
  const verb = state === "running" ? "Simulating:" : state === "stopped" ? "Stopped. Simulated" : "Simulated";
  const rate = p.speedup > 0 ? ` (${formatSpeedup(p.speedup)} real time)` : "";
  return `${verb} ${(p.simulatedMs / 1000).toFixed(1)} s in ${(p.wallMs / 1000).toFixed(2)} s${rate}`;
}

export interface Readout {
  time: string;
  values: { label: string; value: string }[];
}

/** What the readout line says at `tMs`: the time, then every trace's
 * label and the value it holds then. */
export function readout(traces: readonly Trace[], tMs: number, spanMs: number): Readout {
  return {
    time: formatTime(tMs, spanMs),
    values: traces.map((trace) => ({ label: trace.label, value: describeValue(trace, valueAt(trace.points, tMs)) })),
  };
}
