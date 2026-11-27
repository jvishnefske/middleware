// Software in the loop (SIL): runs a wiring's payload in the device runtime
// (src/sim/session.ts) on a virtual clock, drives its input pins with
// waveforms, optionally feeds its outputs back to its inputs through a
// simple plant, and records what happened as traces over virtual time.
//
// Time is virtual: it moves only when the engine advances it, one step at
// a time, so a run is reproducible and can go much faster than real time
// ("max"), or be paced to N times real time. One step is: set the inputs
// for time t, advance the clock by the step, let the payload run its
// main-loop pass. So the payload answers an input set at t at t + step,
// and a loopback carries an output written at t to its input at t (seen by
// the payload at t + step): nothing reacts in zero time.
//
// Every trace is sample-and-hold: a point is recorded when the value
// changes (plus the first and the last sample), and the value holds until
// the next point. Pin writes, PWM duties (`pwm_set`, 0..65535) and traced
// channels (publications on `wire/<channel>`, 4 bytes little-endian i32)
// are stamped with the virtual time of the step that produced them.
//
// No DOM here. The run yields between slices of a few milliseconds of wall
// time so a page stays responsive; tests inject the wall clock and the
// yield.

import type { Protocol, SimEvent } from "../protocol/wasm";
import { type Clock, SimSession } from "../sim/session";

// ---------------------------------------------------------------- waves

export type Wave =
  | { kind: "const"; value: number }
  /** `from` before `atMs`, `to` from then on. */
  | { kind: "step"; atMs: number; from: number; to: number }
  /** `high` for the first `duty` (0..1) of every period, starting at
   * `-phaseMs`. */
  | { kind: "square"; periodMs: number; duty: number; low: number; high: number; phaseMs?: number }
  /** Starts at the middle, rising. */
  | { kind: "sine"; periodMs: number; min: number; max: number }
  /** Sawtooth: `min` rising to `max` over every period. */
  | { kind: "ramp"; periodMs: number; min: number; max: number }
  /** `high` during [`atMs`, `atMs + widthMs`), `low` otherwise. */
  | { kind: "pulse"; atMs: number; widthMs: number; low: number; high: number }
  /** Uniform in [`min`, `max`), a new value every `holdMs` (default 1 ms);
   * the same `seed` gives the same values. */
  | { kind: "noise"; min: number; max: number; seed: number; holdMs?: number };

export type WaveKind = Wave["kind"];
export const WAVE_KINDS: readonly WaveKind[] = ["const", "step", "square", "sine", "ramp", "pulse", "noise"];

const fraction = (t: number, period: number) => {
  const f = (t / period) % 1;
  return f < 0 ? f + 1 : f;
};

/** One LCG step (Numerical Recipes constants). */
const lcg = (x: number) => (Math.imul(x, 1664525) + 1013904223) >>> 0;

/** The wave's value at `tMs`. A period that is not positive gives the
 * low (or minimum) value. */
export function waveAt(wave: Wave, tMs: number): number {
  switch (wave.kind) {
    case "const":
      return wave.value;
    case "step":
      return tMs < wave.atMs ? wave.from : wave.to;
    case "square":
      if (!(wave.periodMs > 0)) return wave.low;
      return fraction(tMs + (wave.phaseMs ?? 0), wave.periodMs) < wave.duty ? wave.high : wave.low;
    case "sine": {
      if (!(wave.periodMs > 0)) return wave.min;
      const mid = (wave.min + wave.max) / 2;
      return mid + ((wave.max - wave.min) / 2) * Math.sin(2 * Math.PI * fraction(tMs, wave.periodMs));
    }
    case "ramp":
      if (!(wave.periodMs > 0)) return wave.min;
      return wave.min + (wave.max - wave.min) * fraction(tMs, wave.periodMs);
    case "pulse":
      return tMs >= wave.atMs && tMs < wave.atMs + wave.widthMs ? wave.high : wave.low;
    case "noise": {
      const hold = wave.holdMs !== undefined && wave.holdMs > 0 ? wave.holdMs : 1;
      const n = Math.floor(tMs / hold);
      // The sample index is mixed into the seed, then two LCG steps: a
      // pure function of time, so any step size sees the same sequence.
      const x = lcg(lcg(((wave.seed | 0) ^ Math.imul(n, 0x9e3779b1)) >>> 0) ^ (n >>> 16));
      return wave.min + (wave.max - wave.min) * (x / 0x1_0000_0000);
    }
  }
}

// ---------------------------------------------------------------- config

export type PinKind = "digital" | "analog";

/** A waveform on an input pin. Digital: high when the value is at least
 * 0.5. Analog: rounded and clamped to 0..4095 (what `adc_read` returns). */
export interface Stimulus {
  pin: number;
  kind: PinKind;
  wave: Wave;
}

export type LoopbackSource = { pin: number } | { pwm: [timer: number, channel: number] };

/** An output fed back to an input through a plant: the source as a
 * fraction (a digital output is 0 or 1, a PWM duty is duty / 65535),
 * through a first-order lag with time constant `lagMs` (none when 0 or
 * absent), then `fraction * gain + offset` compared with 0.5 for a digital
 * target, or `fraction * 4095 * gain + offset` for an analog one. A
 * loopback replaces a stimulus on the same pin. */
export interface Loopback {
  from: LoopbackSource;
  to: { pin: number; kind: PinKind };
  lagMs?: number;
  gain?: number;
  offset?: number;
}

export interface SilConfig {
  durationMs: number;
  stepMs: number;
  stimuli: Stimulus[];
  loopbacks: Loopback[];
  /** Default [`DEFAULT_MAX_POINTS`]. */
  maxPointsPerTrace?: number;
}

export const DEFAULT_MAX_POINTS = 5000;
export const ADC_MAX = 4095;
export const DUTY_MAX = 65535;
/** Log lines kept from one run. */
export const MAX_LOGS = 1000;
/** The key prefix a wiring publishes its traced channels under. */
export const TRACE_PREFIX = "wire/";

// ---------------------------------------------------------------- traces

export type TraceKind = "input" | "output" | "analog" | "pwm" | "channel";

export interface TracePoint {
  /** Virtual milliseconds since the payload was loaded. */
  t: number;
  v: number;
}

/** A value over virtual time, held between points. `input` and `output`
 * are pin levels (0 or 1), `analog` is an ADC reading (0..4095), `pwm` a
 * duty (0..65535), `channel` a traced channel's i32. */
export interface Trace {
  id: string;
  kind: TraceKind;
  label: string;
  /** Only ever 0 or 1: drawn as a lane, not on a scale. */
  digital: boolean;
  points: TracePoint[];
}

/** At most `max` (at least 4) of `points`: the first, the last, and in
 * between the lowest and the highest point of each of equal spans of
 * time, in time order, so a one-sample spike survives. */
export function decimate(points: readonly TracePoint[], max: number): TracePoint[] {
  const cap = Math.max(4, Math.floor(max));
  if (points.length <= cap) return points.slice();
  const first = points[0];
  const last = points[points.length - 1];
  const buckets = Math.floor((cap - 2) / 2);
  const span = last.t - first.t;
  const inner = points.length - 2;
  const out: TracePoint[] = [first];
  let current = -1;
  let lo: TracePoint | null = null;
  let hi: TracePoint | null = null;
  const flush = () => {
    if (!lo || !hi) return;
    if (lo === hi) out.push(lo);
    else if (lo.t <= hi.t) out.push(lo, hi);
    else out.push(hi, lo);
  };
  for (let i = 1; i <= inner; i++) {
    const p = points[i];
    const at = span > 0 ? ((p.t - first.t) / span) * buckets : ((i - 1) / inner) * buckets;
    const b = Math.min(buckets - 1, Math.max(0, Math.floor(at)));
    if (b !== current) {
      flush();
      current = b;
      lo = hi = p;
    } else {
      if (lo && p.v < lo.v) lo = p;
      if (hi && p.v > hi.v) hi = p;
    }
  }
  flush();
  out.push(last);
  return out;
}

/** Collects the traces of one run. */
class Recorder {
  private readonly traces = new Map<string, Trace>();
  private readonly last = new Map<string, number>();

  constructor(
    private readonly cap: number,
    private readonly label: (kind: TraceKind, ref: TraceRef) => string,
  ) {}

  /** Records `v` at `t` if it differs from the trace's last value. */
  sample(kind: TraceKind, ref: TraceRef, t: number, v: number): void {
    const id = traceId(kind, ref);
    let trace = this.traces.get(id);
    if (!trace) {
      trace = { id, kind, label: this.label(kind, ref), digital: kind === "input" || kind === "output", points: [] };
      this.traces.set(id, trace);
    } else if (this.last.get(id) === v) {
      return;
    }
    this.last.set(id, v);
    trace.points.push({ t, v });
    // Decimated in passes, so a long run costs a bounded amount of memory.
    if (trace.points.length > this.cap * 2) trace.points = decimate(trace.points, this.cap);
  }

  /** The last value of a trace; `fallback` before its first sample. */
  value(kind: TraceKind, ref: TraceRef, fallback = 0): number {
    return this.last.get(traceId(kind, ref)) ?? fallback;
  }

  /** The traces as of `endMs`, each closed with its value then, in signal
   * order: stimuli, channels, outputs. */
  finish(endMs: number): Trace[] {
    const out: Trace[] = [];
    for (const trace of this.traces.values()) {
      let points = trace.points.slice();
      const tail = points[points.length - 1];
      if (tail && tail.t < endMs) points.push({ t: endMs, v: tail.v });
      if (points.length > this.cap) points = decimate(points, this.cap);
      const digital = trace.kind === "channel" ? points.every((p) => p.v === 0 || p.v === 1) : trace.digital;
      out.push({ ...trace, digital, points });
    }
    const rank: Record<TraceKind, number> = { input: 0, analog: 1, channel: 2, output: 3, pwm: 4 };
    return out.sort((a, b) => rank[a.kind] - rank[b.kind]);
  }
}

// ---------------------------------------------------------------- graph

/** The part of a wiring graph the engine reads. `WireGraph` (wire.ts) has
 * this shape; so do graphs with the node kinds `analog` and `pwm`. */
export interface SilGraph {
  period_ms: number;
  nodes: readonly SilNode[];
  edges: readonly (readonly [string, string])[];
}

export interface SilNode {
  id: string;
  kind: string;
  config?: { pin?: number; timer?: number; channel?: number; trace?: boolean };
}

/** What a trace is of: a pin, a PWM `[timer, channel]`, or a channel name. */
export type TraceRef = number | readonly [number, number] | string;

export function traceId(kind: TraceKind, ref: TraceRef): string {
  return `${kind}:${typeof ref === "object" ? `${ref[0]}.${ref[1]}` : ref}`;
}

/** A trace's label: a pin with the channels its node connects to
 * ("gpio15 → button", "lit → gpio25", "adc26 → level", "duty → pwm0.0"),
 * a channel by its name. Without a graph, or for a pin the graph does not
 * use, the pin alone. */
export function traceLabel(graph: SilGraph | null | undefined, kind: TraceKind, ref: TraceRef): string {
  if (kind === "channel") return String(ref);
  const own = kind === "pwm" && typeof ref === "object" ? `pwm${ref[0]}.${ref[1]}` : `${kind === "analog" ? "adc" : "gpio"}${String(ref)}`;
  if (!graph) return own;
  const node = graph.nodes.find((n) => {
    if (kind === "pwm") return n.kind === "pwm" && typeof ref === "object" && n.config?.timer === ref[0] && (n.config?.channel ?? 0) === ref[1];
    return n.kind === kind && n.config?.pin === ref;
  });
  if (!node) return own;
  const source = kind === "input" || kind === "analog";
  const others = graph.edges.flatMap(([a, b]) => (source ? (a === node.id ? [b] : []) : b === node.id ? [a] : []));
  if (others.length === 0) return own;
  return source ? `${own} → ${others.join(", ")}` : `${others.join(", ")} → ${own}`;
}

export interface LoopbackEnds {
  /** The graph's digital outputs and PWM sinks. */
  sources: { label: string; from: LoopbackSource }[];
  /** The graph's digital and analog inputs. */
  targets: { label: string; to: { pin: number; kind: PinKind } }[];
}

/** What a loopback can connect in this wiring. */
export function loopbackEnds(graph: SilGraph): LoopbackEnds {
  const ends: LoopbackEnds = { sources: [], targets: [] };
  for (const n of graph.nodes) {
    const pin = n.config?.pin;
    if (n.kind === "output" && pin !== undefined) ends.sources.push({ label: traceLabel(graph, "output", pin), from: { pin } });
    else if (n.kind === "pwm" && n.config?.timer !== undefined) {
      const pwm: [number, number] = [n.config.timer, n.config.channel ?? 0];
      ends.sources.push({ label: traceLabel(graph, "pwm", pwm), from: { pwm } });
    } else if (n.kind === "input" && pin !== undefined) ends.targets.push({ label: traceLabel(graph, "input", pin), to: { pin, kind: "digital" } });
    else if (n.kind === "analog" && pin !== undefined) ends.targets.push({ label: traceLabel(graph, "analog", pin), to: { pin, kind: "analog" } });
  }
  return ends;
}

/** A wave of `kind` with parameters that show something within
 * `durationMs` on a pin of `pinKind`. */
export function defaultWave(kind: WaveKind, pinKind: PinKind, durationMs: number): Wave {
  const high = pinKind === "analog" ? ADC_MAX : 1;
  const periodMs = Math.max(1, Math.round(durationMs / 4));
  switch (kind) {
    case "const":
      return { kind, value: 0 };
    case "step":
      return { kind, atMs: Math.round(durationMs / 2), from: 0, to: high };
    case "square":
      return { kind, periodMs, duty: 0.5, low: 0, high };
    case "sine":
      return { kind, periodMs, min: 0, max: high };
    case "ramp":
      return { kind, periodMs, min: 0, max: high };
    case "pulse":
      return { kind, atMs: Math.round(durationMs / 4), widthMs: Math.max(1, Math.round(durationMs / 10)), low: 0, high };
    case "noise":
      return { kind, min: 0, max: high, seed: 1, holdMs: Math.max(1, Math.round(durationMs / 100)) };
  }
}

/** A configuration that exercises `graph`: the step is its scan period;
 * every digital input gets a square wave, each twice as slow as the one
 * before, so the inputs count through their combinations; every analog
 * input gets a sine over the full range. Long enough for 200 to 2000
 * steps (2 s at 10 ms and at 1 ms). When mwctl's `report` carries the
 * wiring as it understood it, that is the graph used. */
export function defaultConfig(graph: SilGraph, report?: { graph?: SilGraph } | null): SilConfig {
  const g = report?.graph ?? graph;
  const stepMs = g.period_ms > 0 ? g.period_ms : 10;
  const durationMs = Math.min(2000 * stepMs, Math.max(2000, 200 * stepMs));
  const stimuli: Stimulus[] = [];
  let digital = 0;
  let analog = 0;
  for (const n of g.nodes) {
    const pin = n.config?.pin;
    if (pin === undefined || stimuli.some((s) => s.pin === pin)) continue;
    if (n.kind === "input") {
      const periodMs = Math.min(durationMs, 20 * stepMs * 2 ** digital++);
      stimuli.push({ pin, kind: "digital", wave: { kind: "square", periodMs, duty: 0.5, low: 0, high: 1 } });
    } else if (n.kind === "analog") {
      const periodMs = durationMs / 2 ** Math.min(analog++, 3);
      stimuli.push({ pin, kind: "analog", wave: { kind: "sine", periodMs, min: 0, max: ADC_MAX } });
    }
  }
  return { durationMs, stepMs, stimuli, loopbacks: [] };
}

// ---------------------------------------------------------------- run

/** Time that moves only when its owner moves it. */
export class VirtualClock implements Clock {
  t = 0;
  now(): number {
    return this.t;
  }
  /** Nothing polls on this clock: the engine steps the session itself. */
  every(): () => void {
    return () => {};
  }
}

export interface SilProgress {
  simulatedMs: number;
  wallMs: number;
  /** `simulatedMs / wallMs`; 0 while no wall time has passed. */
  speedup: number;
  /** The traces so far (copies), each closed at `simulatedMs`. */
  snapshot(): Trace[];
}

export interface SilOptions {
  /** `"max"` (the default): as fast as possible. A number N: N times
   * real time; virtual time never runs ahead of wall time × N. */
  speed?: "max" | number;
  /** Aborting it stops the run at the next step. */
  signal?: AbortSignal;
  /** Called after every slice. */
  onProgress?(p: SilProgress): void;
  /** The wiring the payload was built from, for the traces' labels. */
  graph?: SilGraph | null;
  /** Wall milliseconds of stepping between two yields (default 8). */
  sliceMs?: number;
  /** The wall clock in milliseconds (default `performance.now`). */
  wallNow?(): number;
  /** Lets the page run for `ms` (0: just yield). Default: a
   * MessageChannel turn for 0, a timer otherwise. */
  sleep?(ms: number): Promise<void>;
}

export interface SilLog {
  t: number;
  line: string;
}

export interface SilResult {
  traces: Trace[];
  /** The payload's log lines, the first [`MAX_LOGS`]. */
  logs: SilLog[];
  /** Stimuli and loopbacks that could not be applied, in words. */
  warnings: string[];
  simulatedMs: number;
  wallMs: number;
  /** `simulatedMs / wallMs`; 0 when no wall time was measured. */
  speedup: number;
  steps: number;
  /** Ended by `signal` before `durationMs`. */
  stopped: boolean;
  /** Why the run ended early: the payload was refused or trapped, or the
   * configuration cannot be run. */
  error?: string;
}

function defaultSleep(ms: number): Promise<void> {
  if (ms > 0 || typeof MessageChannel === "undefined") return new Promise((done) => setTimeout(done, Math.max(0, ms)));
  // A timer of 0 is clamped to 4 ms in a page; a message is not.
  return new Promise((done) => {
    const channel = new MessageChannel();
    channel.port1.onmessage = () => {
      channel.port1.close();
      done();
    };
    channel.port2.postMessage(0);
  });
}

/** The longest the engine sleeps before it looks at `signal` again. */
const MAX_NAP_MS = 50;

/** An input the engine drives every step. */
interface Driver {
  pin: number;
  kind: PinKind;
  /** The value to apply at `tMs` (before clamping to the pin's range). */
  next(tMs: number): number;
}

const clampAdc = (v: number) => Math.max(0, Math.min(ADC_MAX, Math.round(v)));

function configError(config: SilConfig): string | null {
  if (!(config.stepMs > 0) || !Number.isFinite(config.stepMs)) return "the step must be a positive number of milliseconds";
  if (!(config.durationMs >= 0) || !Number.isFinite(config.durationMs)) return "the duration must be a number of milliseconds";
  return null;
}

/** Runs `wasm` for `config.durationMs` of virtual time. Never throws for a
 * bad payload or configuration: the result carries `error`. */
export async function runSil(proto: Protocol, wasm: Uint8Array, config: SilConfig, opts: SilOptions = {}): Promise<SilResult> {
  const wall = opts.wallNow ?? (() => performance.now());
  const sleep = opts.sleep ?? defaultSleep;
  const sliceMs = opts.sliceMs ?? 8;
  const speed = opts.speed ?? "max";
  const { stepMs } = config;
  const started = wall();
  const logs: SilLog[] = [];
  const warnings: string[] = [];
  const recorder = new Recorder(config.maxPointsPerTrace ?? DEFAULT_MAX_POINTS, (kind, ref) => traceLabel(opts.graph, kind, ref));
  let error: string | undefined;
  let stopped = false;
  let done = 0;

  const result = (): SilResult => {
    const simulatedMs = done * stepMs;
    const wallMs = wall() - started;
    return { traces: recorder.finish(simulatedMs), logs, warnings, simulatedMs, wallMs, speedup: wallMs > 0 ? simulatedMs / wallMs : 0, steps: done, stopped, error };
  };

  error = configError(config) ?? (typeof speed === "number" && !(speed > 0) ? "the speed must be positive" : undefined);
  if (error !== undefined) return result();
  const steps = Math.ceil(config.durationMs / stepMs);
  const clock = new VirtualClock();

  const onEvent = (e: SimEvent): void => {
    switch (e.kind) {
      case "log":
        if (logs.length < MAX_LOGS) logs.push({ t: clock.t, line: e.line });
        break;
      case "output":
        recorder.sample("output", e.pin, clock.t, 0);
        break;
      case "write":
        recorder.sample("output", e.pin, clock.t, e.high ? 1 : 0);
        break;
      case "pwmSet":
        recorder.sample("pwm", [e.timer, e.channel], clock.t, e.duty);
        break;
      case "publish":
        if (e.key.startsWith(TRACE_PREFIX) && e.data.byteLength === 4) {
          const value = new DataView(e.data.buffer, e.data.byteOffset, 4).getInt32(0, true);
          recorder.sample("channel", e.key.slice(TRACE_PREFIX.length), clock.t, value);
        }
        break;
      default:
        break;
    }
  };

  let session: SimSession;
  try {
    session = new SimSession(proto, wasm, { onEvent, onError: (message) => (error = message) }, clock);
  } catch (e) {
    error = e instanceof Error ? e.message : String(e);
    return result();
  }

  try {
    // One driver per input pin; a loopback replaces a stimulus there.
    const drivers = new Map<string, Driver>();
    for (const s of config.stimuli) drivers.set(`${s.kind}:${s.pin}`, { pin: s.pin, kind: s.kind, next: (t) => waveAt(s.wave, t) });
    for (const l of config.loopbacks) {
      const { from } = l;
      const alpha = l.lagMs !== undefined && l.lagMs > 0 ? 1 - Math.exp(-stepMs / l.lagMs) : 1;
      const scale = (l.to.kind === "analog" ? ADC_MAX : 1) * (l.gain ?? 1);
      const offset = l.offset ?? 0;
      let y = 0;
      const next = () => {
        const u = "pwm" in from ? recorder.value("pwm", from.pwm) / DUTY_MAX : recorder.value("output", from.pin);
        y += alpha * (u - y);
        return y * scale + offset;
      };
      drivers.set(`${l.to.kind}:${l.to.pin}`, { pin: l.to.pin, kind: l.to.kind, next });
    }
    const applied = new Map<Driver, number>();
    const refused = new Set<Driver>();

    const step = (k: number): void => {
      const t = k * stepMs;
      for (const d of drivers.values()) {
        if (refused.has(d)) continue;
        const raw = d.next(t);
        const v = d.kind === "analog" ? clampAdc(raw) : raw >= 0.5 ? 1 : 0;
        if (applied.get(d) === v) continue;
        applied.set(d, v);
        const ok = d.kind === "analog" ? session.setAdc(d.pin, v) : session.setInput(d.pin, v === 1);
        if (!ok) {
          refused.add(d);
          warnings.push(d.kind === "analog" ? `the board has no ADC pin ${d.pin}; nothing drives it` : `GPIO ${d.pin} is not an input of this payload; nothing drives it`);
          continue;
        }
        recorder.sample(d.kind === "analog" ? "analog" : "input", d.pin, t, v);
      }
      clock.t = (k + 1) * stepMs;
      session.step();
    };

    if (session.trapped && error === undefined) error = "the payload stopped in init";
    for (;;) {
      const sliceStart = wall();
      let waitMs = 0;
      let inSlice = 0;
      while (done < steps && error === undefined) {
        if (opts.signal?.aborted) {
          stopped = true;
          break;
        }
        const now = wall();
        if (inSlice > 0 && now - sliceStart >= sliceMs) break;
        if (speed !== "max") {
          // The step ends at this virtual time: not before wall × speed.
          const dueMs = ((done + 1) * stepMs) / speed;
          if (dueMs > now - started) {
            waitMs = dueMs - (now - started);
            break;
          }
        }
        step(done);
        done++;
        inSlice++;
      }
      if (opts.onProgress) {
        const simulatedMs = done * stepMs;
        const wallMs = wall() - started;
        opts.onProgress({ simulatedMs, wallMs, speedup: wallMs > 0 ? simulatedMs / wallMs : 0, snapshot: () => recorder.finish(simulatedMs) });
      }
      if (done >= steps || error !== undefined || stopped) break;
      // Short naps, so a stop is noticed soon at a slow pace.
      await sleep(Math.min(waitMs, MAX_NAP_MS));
    }
    return result();
  } finally {
    session.dispose();
  }
}
