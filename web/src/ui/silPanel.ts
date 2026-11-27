// Simulation panel of the Wire tab: runs the current wiring software in
// the loop (src/mwctl/sil.ts) and plots what its pins and traced channels
// did over virtual time.
//
// The panel holds the run's configuration: how long, the step, how fast
// (1× real time up to as fast as possible), a waveform for every input of
// the wiring, and loopbacks that feed an output back to an input through
// a lag. "Run" asks the page for the wiring's payload (`deps.build`, mwctl
// in the Wire tab) and hands it to the engine; the status line says how
// much virtual time was simulated in how much wall time. The result is a
// stack of plots sharing one time axis, drawn as inline SVG with the
// app's colours; pointing at it (or the arrow keys, when it has focus)
// moves a crosshair and the readout line gives every trace's value there.
// The geometry is in silPlot.ts.

import * as sil from "../mwctl/sil";
import type { Protocol } from "../protocol/wasm";
import { errorText, h } from "./dom";
import * as plot from "./silPlot";

export interface SilPanelDeps {
  proto: Protocol;
  /** The current wiring's payload; rejects with the reason when the
   * wiring cannot be built. */
  build(): Promise<Uint8Array>;
}

export interface SilPanel {
  el: HTMLElement;
  /** The wiring to simulate, and whether it can be built (`ok`). Inputs
   * that are still there keep their waveforms. */
  setGraph(graph: sil.SilGraph, ok: boolean): void;
  /** Stops a run in progress. */
  stop(): void;
}

const SVG = "http://www.w3.org/2000/svg";
const BLOCKED = "Fix the wiring's problems to simulate it.";

function svg<K extends keyof SVGElementTagNameMap>(tag: K, attrs: Record<string, string | number> = {}, ...children: (Node | string)[]): SVGElementTagNameMap[K] {
  const el = document.createElementNS(SVG, tag);
  for (const [k, v] of Object.entries(attrs)) el.setAttribute(k, String(v));
  el.append(...children);
  return el;
}

/** The speeds on offer; the value is what `runSil` takes. */
export const SPEEDS: readonly { label: string; speed: "max" | number }[] = [
  { label: "1× real time", speed: 1 },
  { label: "10×", speed: 10 },
  { label: "100×", speed: 100 },
  { label: "1000×", speed: 1000 },
  { label: "max", speed: "max" },
];

export const WAVE_NAMES: Record<sil.WaveKind, string> = {
  const: "constant",
  step: "step",
  square: "square wave",
  sine: "sine",
  ramp: "ramp (sawtooth)",
  pulse: "pulse",
  noise: "noise",
};

interface WaveField {
  key: string;
  label: string;
  /** A level (low, high, min, max, from, to): not asked for a digital
   * pin, where it is 0 or 1. */
  level?: boolean;
  /** The value used when the wave has none (an optional parameter). */
  fallback?: number;
}

/** The parameters of each wave, in the order the panel asks for them. */
export const WAVE_FIELDS: Record<sil.WaveKind, readonly WaveField[]> = {
  const: [{ key: "value", label: "value" }],
  step: [
    { key: "atMs", label: "at ms" },
    { key: "from", label: "from", level: true },
    { key: "to", label: "to", level: true },
  ],
  square: [
    { key: "periodMs", label: "period ms" },
    { key: "duty", label: "duty 0–1" },
    { key: "phaseMs", label: "phase ms", fallback: 0 },
    { key: "low", label: "low", level: true },
    { key: "high", label: "high", level: true },
  ],
  sine: [
    { key: "periodMs", label: "period ms" },
    { key: "min", label: "min", level: true },
    { key: "max", label: "max", level: true },
  ],
  ramp: [
    { key: "periodMs", label: "period ms" },
    { key: "min", label: "min", level: true },
    { key: "max", label: "max", level: true },
  ],
  pulse: [
    { key: "atMs", label: "at ms" },
    { key: "widthMs", label: "width ms" },
    { key: "low", label: "low", level: true },
    { key: "high", label: "high", level: true },
  ],
  noise: [
    { key: "min", label: "min", level: true },
    { key: "max", label: "max", level: true },
    { key: "seed", label: "seed" },
    { key: "holdMs", label: "hold ms", fallback: 1 },
  ],
};

type WaveParams = Record<string, number | string | undefined>;

/** A wave's parameter by name. */
export function waveParam(wave: sil.Wave, field: WaveField): number {
  const v = (wave as sil.Wave & WaveParams)[field.key];
  return typeof v === "number" ? v : (field.fallback ?? 0);
}

/** The wave with one parameter changed. */
export function withWaveParam(wave: sil.Wave, key: string, value: number): sil.Wave {
  return { ...wave, [key]: value } as sil.Wave;
}

/** The fields the panel shows for a wave on a pin of `pinKind`. */
export function waveFields(kind: sil.WaveKind, pinKind: sil.PinKind): readonly WaveField[] {
  return pinKind === "digital" ? WAVE_FIELDS[kind].filter((f) => !f.level) : WAVE_FIELDS[kind];
}

const sourceKey = (from: sil.LoopbackSource) => ("pwm" in from ? `pwm:${from.pwm[0]}.${from.pwm[1]}` : `pin:${from.pin}`);
const targetKey = (to: { pin: number; kind: sil.PinKind }) => `${to.kind}:${to.pin}`;

/** A configuration for `graph` that keeps what the user set in `prev`:
 * the waveform of every input that is still there, and every loopback
 * whose two ends are. */
export function mergeConfig(prev: sil.SilConfig | null, graph: sil.SilGraph, keep: { duration: boolean; step: boolean }): sil.SilConfig {
  const next = sil.defaultConfig(graph);
  if (!prev) return next;
  const ends = sil.loopbackEnds(graph);
  return {
    durationMs: keep.duration ? prev.durationMs : next.durationMs,
    stepMs: keep.step ? prev.stepMs : next.stepMs,
    stimuli: next.stimuli.map((s) => prev.stimuli.find((p) => p.pin === s.pin && p.kind === s.kind) ?? s),
    loopbacks: prev.loopbacks.filter(
      (l) => ends.sources.some((s) => sourceKey(s.from) === sourceKey(l.from)) && ends.targets.some((t) => targetKey(t.to) === targetKey(l.to)),
    ),
  };
}

/** What makes two graphs the same wiring to the panel (positions do not). */
function signature(graph: sil.SilGraph): string {
  return JSON.stringify([graph.period_ms, graph.nodes.map((n) => [n.id, n.kind, n.config?.pin, n.config?.timer, n.config?.channel]), graph.edges]);
}

function numberInput(value: number, attrs: Record<string, string | number>, set: (v: number) => void): HTMLInputElement {
  const input = h("input", { type: "number", step: "any", ...attrs, value });
  input.addEventListener("change", () => {
    const v = input.valueAsNumber;
    if (Number.isFinite(v)) set(v);
    else input.value = String(value);
  });
  return input;
}

/** Characters of the label font that fit in `px` (about 6.6 px each). */
const charsIn = (px: number) => Math.floor(px / 6.6);

/** Draws `traces` over 0..`endMs` into `host`: the SVG and its readout. */
function drawPlot(host: HTMLElement, traces: readonly sil.Trace[], endMs: number, width: number): void {
  host.replaceChildren();
  if (traces.length === 0) {
    host.append(h("p", { class: "muted" }, "Nothing was recorded: the payload wrote no pin and published no traced channel."));
    return;
  }
  const layout = plot.layoutPlot(traces, width, endMs);
  const span = layout.t1 - layout.t0;
  const x = (t: number) => plot.timeToX(layout, t);
  const root = svg("svg", {
    class: "sil-plot",
    width: layout.width,
    height: layout.height,
    viewBox: `0 0 ${layout.width} ${layout.height}`,
    role: "img",
    tabindex: 0,
    "aria-label": `Simulation result: ${traces.length} trace${traces.length === 1 ? "" : "s"} over ${plot.formatTime(layout.t1, span)}. Arrow keys move the cursor.`,
  });

  const ticks = plot.timeTicks(layout.t0, layout.t1, Math.max(2, Math.floor((layout.x1 - layout.x0) / 80)));
  for (const t of ticks) root.append(svg("line", { class: "sil-grid", x1: x(t), x2: x(t), y1: 2, y2: layout.axisY }));

  const dots: SVGCircleElement[] = [];
  for (const lane of layout.lanes) {
    const { trace } = lane;
    const scaleText = trace.digital ? "" : lane.min === lane.max ? plot.formatValue(lane.min) : `min ${plot.formatValue(lane.min)} · max ${plot.formatValue(lane.max)}`;
    const room = charsIn(layout.x1 - layout.x0) - scaleText.length - 2;
    const label = svg("text", { class: "sil-label", x: layout.x0, y: lane.top + 12 }, plot.fitLabel(trace.label, Math.max(6, room)));
    label.append(svg("title", {}, trace.label));
    root.append(
      svg("rect", { class: "sil-band", x: layout.x0, y: lane.bandTop, width: layout.x1 - layout.x0, height: lane.bandHeight }),
      label,
      svg("path", { class: `sil-trace k-${trace.kind}`, d: plot.stepPath(trace.points, x, (v) => plot.valueToY(lane, v), layout.x1) }),
    );
    if (scaleText) root.append(svg("text", { class: "sil-scale", x: layout.x1, y: lane.top + 12, "text-anchor": "end" }, scaleText));
    const dot = svg("circle", { class: `sil-dot k-${trace.kind}`, r: 3, cx: 0, cy: 0, visibility: "hidden" });
    dots.push(dot);
    root.append(dot);
  }

  root.append(svg("line", { class: "sil-axis", x1: layout.x0, x2: layout.x1, y1: layout.axisY, y2: layout.axisY }));
  ticks.forEach((t, i) => {
    const anchor = i === 0 ? "start" : x(t) > layout.x1 - 24 ? "end" : "middle";
    root.append(
      svg("line", { class: "sil-axis", x1: x(t), x2: x(t), y1: layout.axisY, y2: layout.axisY + 4 }),
      svg("text", { class: "sil-scale", x: x(t), y: layout.axisY + 16, "text-anchor": anchor }, plot.formatTime(t, span)),
    );
  });
  const cross = svg("line", { class: "sil-cross", x1: 0, x2: 0, y1: 2, y2: layout.axisY, visibility: "hidden" });
  root.append(cross);

  const hint = "Point at the plot, or focus it and use ← →, to read the values at a time.";
  const readoutEl = h("p", { class: "sil-readout" }, h("span", { class: "muted" }, hint));
  let cursor: number | null = null;
  const show = (t: number | null): void => {
    cursor = t;
    if (t === null) {
      cross.setAttribute("visibility", "hidden");
      for (const dot of dots) dot.setAttribute("visibility", "hidden");
      readoutEl.replaceChildren(h("span", { class: "muted" }, hint));
      return;
    }
    const cx = x(t);
    cross.setAttribute("x1", String(cx));
    cross.setAttribute("x2", String(cx));
    cross.setAttribute("visibility", "visible");
    layout.lanes.forEach((lane, i) => {
      const v = plot.valueAt(lane.trace.points, t);
      dots[i].setAttribute("visibility", v === null ? "hidden" : "visible");
      if (v !== null) {
        dots[i].setAttribute("cx", String(cx));
        dots[i].setAttribute("cy", String(plot.valueToY(lane, v)));
      }
    });
    const r = plot.readout(traces, t, span);
    readoutEl.replaceChildren(h("strong", {}, `t = ${r.time}`), ...r.values.map((v) => h("span", { class: "sil-value" }, `${v.label}: `, h("b", {}, v.value))));
  };
  const timeUnder = (e: PointerEvent): number => {
    const box = root.getBoundingClientRect();
    // The drawing may be shown narrower than it was laid out.
    const k = box.width > 0 ? layout.width / box.width : 1;
    return plot.xToTime(layout, (e.clientX - box.left) * k);
  };
  root.addEventListener("pointermove", (e) => show(timeUnder(e)));
  root.addEventListener("pointerdown", (e) => show(timeUnder(e)));
  root.addEventListener("pointerleave", (e) => {
    // A finger or a key press leaves the cursor where it was put.
    if (e.pointerType === "mouse" && document.activeElement !== root) show(null);
  });
  root.addEventListener("keydown", (e) => {
    const stepMs = span / (e.shiftKey ? 10 : 100);
    const from = cursor ?? layout.t0;
    let to: number;
    if (e.key === "ArrowLeft") to = from - stepMs;
    else if (e.key === "ArrowRight") to = cursor === null ? layout.t0 : from + stepMs;
    else if (e.key === "Home") to = layout.t0;
    else if (e.key === "End") to = layout.t1;
    else if (e.key === "Escape") {
      show(null);
      return;
    } else return;
    e.preventDefault();
    show(Math.max(layout.t0, Math.min(layout.t1, to)));
  });
  host.append(root, readoutEl);
}

export function silPanel(deps: SilPanelDeps): SilPanel {
  let graph: sil.SilGraph = { period_ms: 10, nodes: [], edges: [] };
  let seen = "";
  let ok = false;
  let config = sil.defaultConfig(graph);
  const touched = { duration: false, step: false };
  let speed: "max" | number = "max";
  let abort: AbortController | null = null;
  let shown: { traces: sil.Trace[]; endMs: number } | null = null;
  let drawnWidth = 0;

  const durationIn = numberInput(config.durationMs, { min: 1, "aria-label": "Duration in milliseconds" }, (v) => {
    config.durationMs = Math.max(1, v);
    touched.duration = true;
  });
  const stepIn = numberInput(config.stepMs, { min: 0.001, "aria-label": "Step in milliseconds" }, (v) => {
    if (v > 0) config.stepMs = v;
    touched.step = true;
  });
  const speedSel = h("select", { "aria-label": "Simulation speed" });
  SPEEDS.forEach((s, i) => speedSel.append(h("option", { value: i }, s.label)));
  speedSel.value = String(SPEEDS.findIndex((s) => s.speed === speed));
  speedSel.addEventListener("change", () => (speed = SPEEDS[Number(speedSel.value)]?.speed ?? "max"));

  const stimuliEl = h("div", { class: "sil-stimuli" });
  const loopsEl = h("div", { class: "sil-loops" });
  const runBtn = h("button", { type: "button", class: "primary" }, "Run");
  const stopBtn = h("button", { type: "button", disabled: true }, "Stop");
  const status = h("p", { class: "sil-status", "aria-live": "polite" });
  const notes = h("div", { class: "sil-notes" });
  const plotHost = h("div", { class: "sil-plots" });

  const stimulusLabel = (s: sil.Stimulus) => sil.traceLabel(graph, s.kind === "analog" ? "analog" : "input", s.pin);

  function renderStimuli(): void {
    stimuliEl.replaceChildren();
    if (config.stimuli.length === 0) {
      stimuliEl.append(h("p", { class: "muted" }, "This wiring has no input pin to drive."));
      return;
    }
    config.stimuli.forEach((s, i) => {
      const name = stimulusLabel(s);
      const params = h("span", { class: "row sil-params" });
      const renderParams = (): void => {
        const wave = config.stimuli[i].wave;
        params.replaceChildren(
          ...waveFields(wave.kind, s.kind).map((f) =>
            h(
              "label",
              {},
              `${f.label} `,
              numberInput(waveParam(wave, f), { "aria-label": `${name}: ${f.label}` }, (v) => {
                config.stimuli[i] = { ...config.stimuli[i], wave: withWaveParam(config.stimuli[i].wave, f.key, v) };
              }),
            ),
          ),
        );
      };
      const kindSel = h("select", { "aria-label": `Waveform for ${name}` });
      for (const k of sil.WAVE_KINDS) kindSel.append(h("option", { value: k }, WAVE_NAMES[k]));
      kindSel.value = s.wave.kind;
      kindSel.addEventListener("change", () => {
        const kind = sil.WAVE_KINDS.find((k) => k === kindSel.value) ?? "const";
        config.stimuli[i] = { ...config.stimuli[i], wave: sil.defaultWave(kind, s.kind, config.durationMs) };
        renderParams();
      });
      renderParams();
      const fed = config.loopbacks.some((l) => targetKey(l.to) === targetKey(s));
      stimuliEl.append(
        h(
          "div",
          { class: "row sil-stim", role: "group", "aria-label": `Stimulus for ${name}` },
          h("span", { class: "sil-name" }, name, h("span", { class: "muted" }, s.kind === "analog" ? " · analog 0–4095" : " · digital")),
          kindSel,
          params,
          fed && h("span", { class: "muted" }, "replaced by a loopback"),
        ),
      );
    });
  }

  const loopText = (l: sil.Loopback): string => {
    const from = "pwm" in l.from ? sil.traceLabel(graph, "pwm", l.from.pwm) : sil.traceLabel(graph, "output", l.from.pin);
    const to = sil.traceLabel(graph, l.to.kind === "analog" ? "analog" : "input", l.to.pin);
    return `${from}  feeds  ${to}${l.lagMs ? `, lag ${l.lagMs} ms` : ", no lag"}`;
  };

  function renderLoops(): void {
    const ends = sil.loopbackEnds(graph);
    loopsEl.replaceChildren();
    if (config.loopbacks.length > 0) {
      const list = h("ul", { class: "sil-looplist" });
      config.loopbacks.forEach((l, i) => {
        const remove = h("button", { type: "button", "aria-label": `Remove loopback: ${loopText(l)}` }, "Remove");
        remove.addEventListener("click", () => {
          config.loopbacks.splice(i, 1);
          renderLoops();
          renderStimuli();
        });
        list.append(h("li", {}, loopText(l), " ", remove));
      });
      loopsEl.append(list);
    }
    if (ends.sources.length === 0 || ends.targets.length === 0) {
      loopsEl.append(h("p", { class: "muted" }, "A loopback feeds an output back to an input; this wiring needs one of each."));
      return;
    }
    const fromSel = h("select", { "aria-label": "Loopback from output" });
    ends.sources.forEach((s, i) => fromSel.append(h("option", { value: i }, s.label)));
    const toSel = h("select", { "aria-label": "Loopback to input" });
    ends.targets.forEach((t, i) => toSel.append(h("option", { value: i }, t.label)));
    let lagMs = 0;
    const lagIn = numberInput(lagMs, { min: 0, "aria-label": "Loopback lag in milliseconds" }, (v) => (lagMs = Math.max(0, v)));
    const add = h("button", { type: "button" }, "Add loopback");
    add.addEventListener("click", () => {
      const from = ends.sources[Number(fromSel.value)];
      const to = ends.targets[Number(toSel.value)];
      if (!from || !to) return;
      // One driver per input: a new loopback to a pin replaces the old one.
      config.loopbacks = config.loopbacks.filter((l) => targetKey(l.to) !== targetKey(to.to));
      config.loopbacks.push({ from: from.from, to: to.to, lagMs });
      renderLoops();
      renderStimuli();
    });
    loopsEl.append(h("div", { class: "row sil-loopadd" }, h("label", {}, "From ", fromSel), h("label", {}, "to ", toSel), h("label", {}, "lag ms ", lagIn), add));
  }

  function renderControls(): void {
    durationIn.value = String(config.durationMs);
    stepIn.value = String(config.stepMs);
    renderStimuli();
    renderLoops();
  }

  function buttons(): void {
    runBtn.disabled = abort !== null || !ok;
    stopBtn.disabled = abort === null;
  }

  function redraw(): void {
    if (!shown) return;
    drawnWidth = plotHost.clientWidth || plot.DEFAULT_PLOT_WIDTH;
    drawPlot(plotHost, shown.traces, shown.endMs, drawnWidth);
  }

  function showResult(r: sil.SilResult): void {
    shown = { traces: r.traces, endMs: r.simulatedMs };
    redraw();
    notes.replaceChildren();
    if (r.error !== undefined) notes.append(h("p", { class: "sil-error", role: "alert" }, `Ended at ${plot.formatTime(r.simulatedMs, r.simulatedMs)}: ${r.error}`));
    for (const w of r.warnings) notes.append(h("p", { class: "muted" }, `Note: ${w}.`));
    if (r.logs.length > 0) {
      const text = r.logs.map((l) => `${plot.formatTime(l.t, r.simulatedMs).padStart(9)}  ${l.line}`).join("\n");
      notes.append(h("details", {}, h("summary", {}, `Payload log (${r.logs.length} line${r.logs.length === 1 ? "" : "s"})`), h("pre", { class: "sil-log" }, text)));
    }
  }

  async function run(): Promise<void> {
    if (abort || !ok) return;
    const mine = new AbortController();
    abort = mine;
    buttons();
    notes.replaceChildren();
    status.textContent = "Building the wiring…";
    try {
      const wasm = await deps.build();
      if (mine.signal.aborted) {
        status.textContent = "Stopped.";
        return;
      }
      const cfg: sil.SilConfig = { ...config, stimuli: config.stimuli.map((s) => ({ ...s })), loopbacks: config.loopbacks.map((l) => ({ ...l })) };
      let drawn = performance.now();
      const result = await sil.runSil(deps.proto, wasm, cfg, {
        speed,
        signal: mine.signal,
        graph,
        onProgress: (p) => {
          status.textContent = plot.statusText(p, "running");
          // A slow run shows its traces as they grow, a few times a second.
          const now = performance.now();
          if (now - drawn >= 250) {
            drawn = now;
            shown = { traces: p.snapshot(), endMs: cfg.durationMs };
            redraw();
          }
        },
      });
      status.textContent = plot.statusText(result, result.stopped ? "stopped" : "done");
      showResult(result);
    } catch (e) {
      status.textContent = `Cannot simulate: ${errorText(e)}`;
    } finally {
      if (abort === mine) abort = null;
      buttons();
    }
  }

  runBtn.addEventListener("click", () => void run());
  stopBtn.addEventListener("click", () => abort?.abort());

  // The drawing follows the panel's width (a phone turned, a window resized).
  if (typeof ResizeObserver !== "undefined") {
    new ResizeObserver(() => {
      if (shown && plotHost.clientWidth > 0 && plotHost.clientWidth !== drawnWidth) redraw();
    }).observe(plotHost);
  }

  const el = h(
    "section",
    { class: "sil-panel", "aria-label": "Simulation" },
    h("h3", {}, "Simulate"),
    h("p", { class: "muted" }, "Runs this wiring in the device runtime on a virtual clock: waveforms drive its inputs, and the plots show what its pins and traced channels did."),
    h("div", { class: "row sil-run" }, h("label", {}, "Duration ", durationIn, " ms"), h("label", {}, "Step ", stepIn, " ms"), h("label", {}, "Speed ", speedSel)),
    h("h4", {}, "Stimuli"),
    stimuliEl,
    h("h4", {}, "Loopbacks"),
    loopsEl,
    h("div", { class: "row sil-actions" }, runBtn, stopBtn),
    status,
    notes,
    plotHost,
  );
  renderControls();
  buttons();

  return {
    el,
    setGraph(next, nextOk) {
      const sig = signature(next);
      const changed = sig !== seen;
      ok = nextOk;
      if (changed) {
        seen = sig;
        graph = { period_ms: next.period_ms, nodes: next.nodes.map((n) => ({ id: n.id, kind: n.kind, config: n.config ? { ...n.config } : undefined })), edges: next.edges.map(([a, b]) => [a, b] as const) };
        config = mergeConfig(config, graph, touched);
        renderControls();
      }
      buttons();
      // Said only while it is true; a finished run's line stays otherwise.
      if (!ok && !abort) status.textContent = BLOCKED;
      else if (ok && status.textContent === BLOCKED) status.textContent = "";
    },
    stop() {
      abort?.abort();
    },
  };
}
