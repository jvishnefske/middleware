// Software in the loop (src/mwctl/sil.ts): waveforms, the engine on its
// virtual clock against the real device runtime (wasm-host compiled to
// WASM), and the plot geometry (src/ui/silPlot.ts). Digital wirings are
// built by the real mwctl; analog, PWM, traced-channel and trapping
// payloads are small WAT programs. The wall clock and the yield are
// injected, so pacing is tested without waiting.

import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { beforeAll, describe, expect, it } from "vitest";
import { compileWat } from "../src/editor/compile";
import { Mwctl } from "../src/mwctl/host";
import * as sil from "../src/mwctl/sil";
import * as wire from "../src/mwctl/wire";
import type { Protocol } from "../src/protocol/wasm";
import { mergeConfig, waveFields, waveParam, withWaveParam } from "../src/ui/silPanel";
import * as plot from "../src/ui/silPlot";
import { protocol } from "./fakeDevice";

let mwctl: Mwctl;
let proto: Protocol;

beforeAll(() => {
  const path = fileURLToPath(new URL("../src/pkg/mwctl.wasm", import.meta.url));
  mwctl = new Mwctl(new WebAssembly.Module(readFileSync(path)));
  proto = protocol();
});

const LAMP = "button=gpio15,pullup,invert armed=gpio14,pullup,invert lit=and(button,armed) gpio25=lit";

async function lamp(): Promise<{ graph: wire.WireGraph; wasm: Uint8Array }> {
  const { graph } = await wire.fromCommandLine(mwctl, LAMP);
  return { graph, wasm: await wire.build(mwctl, graph, "rp2350") };
}

async function wat(source: string): Promise<Uint8Array> {
  const r = await compileWat(source);
  if (!r.ok) throw new Error(r.errors);
  return r.wasm;
}

const IMPORTS = `
  (import "mw" "pwm_open" (func $pwm_open (param i32 i32) (result i32)))
  (import "mw" "pwm_set" (func $pwm_set (param i32 i32 i32) (result i32)))
  (import "mw" "adc_read" (func $adc_read (param i32) (result i32)))
  (import "mw" "publish" (func $publish (param i32 i32 i32 i32) (result i32)))
  (import "mw" "set_tick" (func $set_tick (param i32) (result i32)))
  (import "mw" "gpio_output" (func $gpio_output (param i32) (result i32)))
  (import "mw" "gpio_write" (func $gpio_write (param i32 i32) (result i32)))
  (memory (export "memory") 1)`;

/** Every 10 ms: reads ADC pin 26, publishes it on wire/raw and its
 * distance from mid-scale (negative below) on wire/err, and sets PWM
 * timer 0 channel 0 to the reading × 16. */
const ADC_PWM = `(module ${IMPORTS}
  (data (i32.const 0) "wire/raw")
  (data (i32.const 16) "wire/err")
  (func (export "init")
    (drop (call $pwm_open (i32.const 0) (i32.const 1000)))
    (drop (call $set_tick (i32.const 10))))
  (func (export "tick") (param i32)
    (local $v i32)
    (local.set $v (call $adc_read (i32.const 26)))
    (i32.store (i32.const 64) (local.get $v))
    (drop (call $publish (i32.const 0) (i32.const 8) (i32.const 64) (i32.const 4)))
    (i32.store (i32.const 68) (i32.sub (local.get $v) (i32.const 2048)))
    (drop (call $publish (i32.const 16) (i32.const 8) (i32.const 68) (i32.const 4)))
    (drop (call $pwm_set (i32.const 0) (i32.const 0) (i32.mul (local.get $v) (i32.const 16))))))`;

const THRESHOLD = 2000;

/** A bang-bang controller: every 10 ms GPIO 25 is high while ADC pin 26
 * reads below the threshold. */
const BANG_BANG = `(module ${IMPORTS}
  (func (export "init")
    (drop (call $gpio_output (i32.const 25)))
    (drop (call $set_tick (i32.const 10))))
  (func (export "tick") (param i32)
    (drop (call $gpio_write (i32.const 25) (i32.lt_s (call $adc_read (i32.const 26)) (i32.const ${THRESHOLD}))))))`;

/** Toggles GPIO 25 every 10 ms and traps in its fifth tick. */
const TRAPS = `(module ${IMPORTS}
  (global $n (mut i32) (i32.const 0))
  (func (export "init")
    (drop (call $gpio_output (i32.const 25)))
    (drop (call $set_tick (i32.const 10))))
  (func (export "tick") (param i32)
    (global.set $n (i32.add (global.get $n) (i32.const 1)))
    (if (i32.ge_s (global.get $n) (i32.const 5)) (then (unreachable)))
    (drop (call $gpio_write (i32.const 25) (i32.and (global.get $n) (i32.const 1))))))`;

/** A wall clock for tests. Every reading costs `perRead` ms; a sleep
 * takes as long as it was asked for (a bare yield `perYield`). */
function fakeWall(perRead: number, perYield = 0.01) {
  const wall = {
    t: 1000,
    yields: 0,
    naps: [] as number[],
    onSleep: null as (() => void) | null,
    now: () => {
      wall.t += perRead;
      return wall.t;
    },
    sleep: (ms: number) => {
      wall.yields++;
      wall.naps.push(ms);
      wall.t += Math.max(ms, perYield);
      wall.onSleep?.();
      return Promise.resolve();
    },
  };
  return wall;
}

const trace = (r: { traces: sil.Trace[] }, id: string): sil.Trace => {
  const t = r.traces.find((x) => x.id === id);
  if (!t) throw new Error(`no trace ${id}; have ${r.traces.map((x) => x.id).join(", ")}`);
  return t;
};

const square = (periodMs: number, phaseMs = 0): sil.Wave => ({ kind: "square", periodMs, duty: 0.5, low: 0, high: 1, phaseMs });

describe("waves", () => {
  it("SIL1: waveAt gives every wave's value at a time", () => {
    expect(sil.waveAt({ kind: "const", value: 7 }, 123)).toBe(7);

    const step: sil.Wave = { kind: "step", atMs: 100, from: 1, to: 5 };
    expect([0, 99.9, 100, 500].map((t) => sil.waveAt(step, t))).toEqual([1, 1, 5, 5]);

    const sq: sil.Wave = { kind: "square", periodMs: 100, duty: 0.25, low: 2, high: 9 };
    expect([0, 24, 25, 99, 100, 124, 125].map((t) => sil.waveAt(sq, t))).toEqual([9, 9, 2, 2, 9, 9, 2]);
    // A phase moves the wave earlier; a negative time still has a value.
    expect([0, 14, 15, 90].map((t) => sil.waveAt({ ...sq, phaseMs: 10 }, t))).toEqual([9, 9, 2, 9]);
    expect(sil.waveAt(sq, -10)).toBe(2);
    expect(sil.waveAt({ ...sq, periodMs: 0 }, 5)).toBe(2);

    const sine: sil.Wave = { kind: "sine", periodMs: 1000, min: 0, max: 4095 };
    expect(sil.waveAt(sine, 0)).toBeCloseTo(2047.5, 6);
    expect(sil.waveAt(sine, 250)).toBeCloseTo(4095, 6);
    expect(sil.waveAt(sine, 750)).toBeCloseTo(0, 6);
    expect(sil.waveAt(sine, 1250)).toBeCloseTo(4095, 6);

    const ramp: sil.Wave = { kind: "ramp", periodMs: 200, min: 10, max: 30 };
    expect([0, 50, 100, 199, 200, 250].map((t) => sil.waveAt(ramp, t))).toEqual([10, 15, 20, 29.9, 10, 15]);

    const pulse: sil.Wave = { kind: "pulse", atMs: 50, widthMs: 20, low: 0, high: 1 };
    expect([0, 49, 50, 69, 70, 500].map((t) => sil.waveAt(pulse, t))).toEqual([0, 0, 1, 1, 0, 0]);
  });

  it("SIL2: noise is reproducible from its seed and stays in range", () => {
    const noise: sil.Wave = { kind: "noise", min: 100, max: 200, seed: 42 };
    const run = (w: sil.Wave) => Array.from({ length: 2000 }, (_, t) => sil.waveAt(w, t));
    const a = run(noise);
    expect(run(noise)).toEqual(a);
    expect(run({ ...noise, seed: 43 })).not.toEqual(a);
    expect(Math.min(...a)).toBeGreaterThanOrEqual(100);
    expect(Math.max(...a)).toBeLessThan(200);
    // Spread over the range, not stuck: the mean is near the middle and
    // most neighbours differ.
    expect(a.reduce((s, v) => s + v, 0) / a.length).toBeGreaterThan(140);
    expect(a.reduce((s, v) => s + v, 0) / a.length).toBeLessThan(160);
    expect(a.filter((v, i) => i > 0 && v !== a[i - 1]).length).toBeGreaterThan(1990);
    // Held for holdMs: the same value within a hold, at any step size.
    const held: sil.Wave = { ...noise, holdMs: 50 };
    expect(sil.waveAt(held, 0)).toBe(sil.waveAt(held, 49.5));
    expect(sil.waveAt(held, 50)).not.toBe(sil.waveAt(held, 49));
  });
});

describe("the engine", () => {
  it("SIL3: a digital wiring's output is its boolean function of the stimuli at every step", async () => {
    const { graph, wasm } = await lamp();
    const stimuli: sil.Stimulus[] = [
      { pin: 15, kind: "digital", wave: square(200) },
      { pin: 14, kind: "digital", wave: square(300, 70) },
    ];
    const config: sil.SilConfig = { durationMs: 3000, stepMs: 10, stimuli, loopbacks: [] };
    const r = await sil.runSil(proto, wasm, config, { graph });
    expect(r.error).toBeUndefined();
    expect(r.warnings).toEqual([]);
    expect(r).toMatchObject({ steps: 300, simulatedMs: 3000, stopped: false });
    expect(r.logs).toEqual([{ t: 0, line: "wire wire: 2 in, 3 ch, 1 out" }]);
    expect(r.traces.map((t) => [t.id, t.kind, t.label, t.digital])).toEqual([
      ["input:15", "input", "gpio15 → button", true],
      ["input:14", "input", "gpio14 → armed", true],
      ["output:25", "output", "lit → gpio25", true],
    ]);
    const out = trace(r, "output:25").points;
    const high = (pin: number, t: number) => sil.waveAt(stimuli.find((s) => s.pin === pin)!.wave, t) >= 0.5;
    let lit = 0;
    for (let k = 0; k < 300; k++) {
      const t = k * 10;
      // What the stimuli put on the pins, as recorded.
      expect(plot.valueAt(trace(r, "input:15").points, t)).toBe(high(15, t) ? 1 : 0);
      expect(plot.valueAt(trace(r, "input:14").points, t)).toBe(high(14, t) ? 1 : 0);
      // Both inputs are active low: lit while both pins are low. The
      // payload answers the inputs of time t one step later.
      const expected = !high(15, t) && !high(14, t) ? 1 : 0;
      expect(plot.valueAt(out, t + 10), `output at ${t + 10} ms`).toBe(expected);
      lit += expected;
    }
    expect(lit).toBeGreaterThan(30);
    expect(lit).toBeLessThan(270);
    // Recorded on change only, plus the first and the last sample.
    expect(out[0]).toEqual({ t: 0, v: 0 });
    expect(out[out.length - 1].t).toBe(3000);
    expect(out.length).toBeLessThan(40);
    for (let i = 1; i < out.length - 1; i++) expect(out[i].v).not.toBe(out[i - 1].v);
  });

  it("SIL4: at speed max, a minute of virtual time takes far less wall time and the speed-up is measured on the wall clock", async () => {
    const { graph, wasm } = await lamp();
    const config = { ...sil.defaultConfig(graph), durationMs: 60_000 };
    const wall = fakeWall(0.001);
    const progress: sil.SilProgress[] = [];
    const r = await sil.runSil(proto, wasm, config, { speed: "max", wallNow: wall.now, sleep: wall.sleep, sliceMs: 1, onProgress: (p) => progress.push(p) });
    expect(r).toMatchObject({ steps: 6000, simulatedMs: 60_000, stopped: false });
    expect(r.error).toBeUndefined();
    // 6000 steps cost about 6 ms of the injected clock.
    expect(r.wallMs).toBeGreaterThan(5);
    expect(r.wallMs).toBeLessThan(100);
    expect(r.speedup).toBeCloseTo(r.simulatedMs / r.wallMs, 9);
    expect(r.speedup).toBeGreaterThan(600);
    // It ran in slices of about 1 ms, yielding between them, never napping.
    expect(wall.yields).toBeGreaterThanOrEqual(5);
    expect(new Set(wall.naps)).toEqual(new Set([0]));
    expect(progress.length).toBe(wall.yields + 1);
    for (let i = 1; i < progress.length; i++) expect(progress[i].simulatedMs).toBeGreaterThan(progress[i - 1].simulatedMs);
    expect(progress[progress.length - 1].simulatedMs).toBe(60_000);
    expect(progress[1].snapshot().map((t) => t.id)).toEqual(["input:15", "input:14", "output:25"]);
  });

  it("SIL5: at speed 10, virtual time never runs ahead of ten times the wall time", async () => {
    const { wasm } = await lamp();
    const config: sil.SilConfig = { durationMs: 2000, stepMs: 10, stimuli: [{ pin: 15, kind: "digital", wave: square(100) }], loopbacks: [] };
    const wall = fakeWall(0);
    const start = wall.t;
    const progress: sil.SilProgress[] = [];
    // Checked after every slice, and against the wall clock at every sleep.
    let simulated = 0;
    let checks = 0;
    wall.onSleep = () => {
      checks++;
      expect(simulated).toBeLessThanOrEqual((wall.t - start) * 10 + 1e-6);
    };
    const r = await sil.runSil(proto, wasm, config, {
      speed: 10,
      wallNow: wall.now,
      sleep: wall.sleep,
      onProgress: (p) => {
        simulated = p.simulatedMs;
        progress.push(p);
        expect(p.simulatedMs).toBeLessThanOrEqual(p.wallMs * 10 + 1e-6);
      },
    });
    expect(r).toMatchObject({ steps: 200, simulatedMs: 2000, stopped: false });
    expect(checks).toBeGreaterThan(100);
    // 2 s at 10× is 200 ms of wall time: not faster, and barely slower.
    expect(r.wallMs).toBeGreaterThanOrEqual(200 - 1e-6);
    expect(r.wallMs).toBeLessThan(220);
    expect(r.speedup).toBeLessThanOrEqual(10 + 1e-6);
    expect(r.speedup).toBeGreaterThan(9);
    // It waited for the wall clock rather than spinning.
    expect(wall.naps.filter((ms) => ms > 0).length).toBeGreaterThan(100);
    expect(Math.max(...wall.naps)).toBeLessThanOrEqual(50);
  });

  it("SIL6: aborting mid-run stops it with the traces so far", async () => {
    const { graph, wasm } = await lamp();
    const config = { ...sil.defaultConfig(graph), durationMs: 60_000 };
    const wall = fakeWall(0.001);
    const abort = new AbortController();
    wall.onSleep = () => {
      if (wall.yields === 3) abort.abort();
    };
    const r = await sil.runSil(proto, wasm, config, { signal: abort.signal, wallNow: wall.now, sleep: wall.sleep, sliceMs: 1 });
    expect(r.stopped).toBe(true);
    expect(r.error).toBeUndefined();
    expect(r.steps).toBeGreaterThan(100);
    expect(r.steps).toBeLessThan(6000);
    expect(r.simulatedMs).toBe(r.steps * 10);
    expect(wall.yields).toBe(3);
    const out = trace(r, "output:25").points;
    expect(out.length).toBeGreaterThan(2);
    // Every trace ends where the run did.
    for (const t of r.traces) expect(t.points[t.points.length - 1].t).toBe(r.simulatedMs);

    // Already aborted: nothing runs.
    const none = await sil.runSil(proto, wasm, config, { signal: abort.signal, wallNow: wall.now, sleep: wall.sleep });
    expect(none).toMatchObject({ stopped: true, steps: 0, simulatedMs: 0 });
  });

  it("SIL7: an analog stimulus, a traced channel (4 bytes LE, signed) and a PWM duty are recorded", async () => {
    const wasm = await wat(ADC_PWM);
    const wave: sil.Wave = { kind: "sine", periodMs: 1000, min: 0, max: 4095 };
    const config: sil.SilConfig = { durationMs: 2000, stepMs: 10, stimuli: [{ pin: 26, kind: "analog", wave }], loopbacks: [] };
    const graph: sil.SilGraph = {
      period_ms: 10,
      nodes: [
        { id: "adc26", kind: "analog", config: { pin: 26 } },
        { id: "raw", kind: "channel", config: { trace: true } },
        { id: "fan", kind: "pwm", config: { timer: 0, channel: 0 } },
      ],
      edges: [
        ["adc26", "raw"],
        ["raw", "fan"],
      ],
    };
    const r = await sil.runSil(proto, wasm, config, { graph });
    expect(r.error).toBeUndefined();
    expect(r.warnings).toEqual([]);
    expect(r.traces.map((t) => [t.id, t.kind, t.label, t.digital])).toEqual([
      ["analog:26", "analog", "adc26 → raw", false],
      ["channel:raw", "channel", "raw", false],
      ["channel:err", "channel", "err", false],
      ["pwm:0.0", "pwm", "raw → pwm0.0", false],
    ]);
    const adc = trace(r, "analog:26").points;
    const raw = trace(r, "channel:raw").points;
    const err = trace(r, "channel:err").points;
    const pwm = trace(r, "pwm:0.0").points;
    for (let k = 0; k < 200; k++) {
      const t = k * 10;
      const level = Math.round(sil.waveAt(wave, t));
      expect(plot.valueAt(adc, t)).toBe(level);
      // The payload reads at t + 10 what was set at t.
      expect(plot.valueAt(raw, t + 10)).toBe(level);
      expect(plot.valueAt(err, t + 10)).toBe(level - 2048);
      expect(plot.valueAt(pwm, t + 10)).toBe(level * 16);
    }
    expect(plot.valueRange(adc)).toEqual({ min: 0, max: 4095 });
    expect(plot.valueRange(err)).toEqual({ min: -2048, max: 2047 });
    expect(plot.valueRange(pwm)).toEqual({ min: 0, max: 65520 });
    // Nothing is known of a channel before its first publication.
    expect(plot.valueAt(raw, 0)).toBeNull();
    expect(raw[0].t).toBe(10);
  });

  it("SIL8: a lagged loopback closes the loop: a bang-bang controller holds its input around the threshold", async () => {
    const wasm = await wat(BANG_BANG);
    const loop: sil.Loopback = { from: { pin: 25 }, to: { pin: 26, kind: "analog" }, lagMs: 200 };
    // The stimulus on the same pin is replaced by the loopback.
    const stimuli: sil.Stimulus[] = [{ pin: 26, kind: "analog", wave: { kind: "const", value: 4095 } }];
    const r = await sil.runSil(proto, wasm, { durationMs: 5000, stepMs: 10, stimuli, loopbacks: [loop] });
    expect(r.error).toBeUndefined();
    expect(r.steps).toBe(500);
    const adc = trace(r, "analog:26").points;
    const out = trace(r, "output:25").points;
    // The plant starts at rest, so the heater is on and the level rises.
    expect(adc[0]).toEqual({ t: 0, v: 0 });
    expect(plot.valueAt(out, 10)).toBe(1);
    const reached = adc.find((p) => p.v >= THRESHOLD);
    expect(reached).toBeDefined();
    // 1 - exp(-t/200) reaches 2000/4095 after about 134 ms.
    expect(reached!.t).toBeGreaterThan(100);
    expect(reached!.t).toBeLessThan(180);
    // From then on it stays in a narrow band around the threshold and
    // keeps crossing it, the output switching each time.
    const settled = adc.filter((p) => p.t >= reached!.t);
    const range = plot.valueRange(settled);
    expect(range.min).toBeGreaterThan(THRESHOLD - 300);
    expect(range.min).toBeLessThan(THRESHOLD);
    expect(range.max).toBeLessThan(THRESHOLD + 300);
    expect(range.max).toBeGreaterThanOrEqual(THRESHOLD);
    let crossings = 0;
    for (let i = 1; i < settled.length; i++) if (settled[i - 1].v < THRESHOLD !== settled[i].v < THRESHOLD) crossings++;
    expect(crossings).toBeGreaterThan(50);
    expect(out.length).toBeGreaterThan(50);
    // A plant does not react in zero time: the output switches one step
    // after the level it answers, and the level moves one step after that.
    for (let i = 1; i < out.length - 1; i++) {
      const before = plot.valueAt(adc, out[i].t - 10)!;
      expect(before < THRESHOLD ? 1 : 0, `output at ${out[i].t} ms`).toBe(out[i].v);
    }

    // Without a lag and with a gain the fed-back level is the output scaled.
    const direct = await sil.runSil(proto, wasm, { durationMs: 100, stepMs: 10, stimuli: [], loopbacks: [{ ...loop, lagMs: 0, gain: 0.5, offset: 100 }] });
    expect(new Set(trace(direct, "analog:26").points.map((p) => p.v))).toEqual(new Set([100, Math.round(4095 * 0.5 + 100)]));

    // A PWM duty fed back to a digital input, as a fraction against 0.5.
    const pwmLoop = await sil.runSil(proto, await wat(ADC_PWM), {
      durationMs: 300,
      stepMs: 10,
      stimuli: [{ pin: 26, kind: "analog", wave: { kind: "step", atMs: 100, from: 1000, to: 3000 } }],
      loopbacks: [{ from: { pwm: [0, 0] }, to: { pin: 7, kind: "digital" } }],
    });
    // GPIO 7 is not an input of that payload: said, not fatal.
    expect(pwmLoop.error).toBeUndefined();
    expect(pwmLoop.warnings).toEqual(["GPIO 7 is not an input of this payload; nothing drives it"]);
  });

  it("SIL9: a trapping payload ends the run with an error and the traces so far", async () => {
    const wasm = await wat(TRAPS);
    const r = await sil.runSil(proto, wasm, { durationMs: 1000, stepMs: 10, stimuli: [], loopbacks: [] });
    expect(r.error).toBeDefined();
    expect(r.error).toMatch(/unreachable|trap/i);
    expect(r.stopped).toBe(false);
    expect(r.steps).toBe(5);
    expect(r.simulatedMs).toBe(50);
    expect(trace(r, "output:25").points).toEqual([
      { t: 0, v: 0 },
      { t: 10, v: 1 },
      { t: 20, v: 0 },
      { t: 30, v: 1 },
      { t: 40, v: 0 },
      { t: 50, v: 0 },
    ]);
    // Not a payload at all, and a step that cannot be: errors, not throws.
    const junk = await sil.runSil(proto, Uint8Array.of(1, 2, 3), { durationMs: 100, stepMs: 10, stimuli: [], loopbacks: [] });
    expect(junk.error).toBeDefined();
    expect(junk).toMatchObject({ steps: 0, traces: [] });
    const bad = await sil.runSil(proto, wasm, { durationMs: 100, stepMs: 0, stimuli: [], loopbacks: [] });
    expect(bad).toMatchObject({ steps: 0, error: "the step must be a positive number of milliseconds" });
  });

  it("SIL10: a long trace is decimated to its cap, keeping the first and last sample and the spikes", async () => {
    const points: sil.TracePoint[] = Array.from({ length: 10_000 }, (_, i) => ({ t: i, v: 100 + (i % 7) }));
    points[5123] = { t: 5123, v: 9000 };
    points[7001] = { t: 7001, v: -500 };
    const small = sil.decimate(points, 100);
    expect(small.length).toBeLessThanOrEqual(100);
    expect(small.length).toBeGreaterThan(50);
    expect(small[0]).toEqual(points[0]);
    expect(small[small.length - 1]).toEqual(points[9999]);
    expect(small).toContainEqual({ t: 5123, v: 9000 });
    expect(small).toContainEqual({ t: 7001, v: -500 });
    for (let i = 1; i < small.length; i++) expect(small[i].t).toBeGreaterThan(small[i - 1].t);
    // Under the cap nothing changes; decimating again keeps the extremes.
    expect(sil.decimate(points.slice(0, 50), 100)).toEqual(points.slice(0, 50));
    expect(plot.valueRange(sil.decimate(small, 10))).toEqual({ min: -500, max: 9000 });
    expect(sil.decimate(small, 10).length).toBeLessThanOrEqual(10);
    // Points at one time (no span) are bucketed by position.
    const flat = Array.from({ length: 100 }, (_, i) => ({ t: 5, v: i === 60 ? 50 : 1 }));
    expect(sil.decimate(flat, 10)).toContainEqual({ t: 5, v: 50 });

    // In a run: a 20 s sine sampled every 10 ms, capped at 60 points.
    const wave: sil.Wave = { kind: "sine", periodMs: 5000, min: 0, max: 4095 };
    const config: sil.SilConfig = { durationMs: 20_000, stepMs: 10, stimuli: [{ pin: 26, kind: "analog", wave }], loopbacks: [], maxPointsPerTrace: 60 };
    const r = await sil.runSil(proto, await wat(ADC_PWM), config);
    for (const t of r.traces) {
      expect(t.points.length, t.id).toBeLessThanOrEqual(60);
      expect(t.points[t.points.length - 1].t).toBe(20_000);
    }
    const adc = trace(r, "analog:26").points;
    expect(adc[0]).toEqual({ t: 0, v: 2048 });
    expect(plot.valueRange(adc)).toEqual({ min: 0, max: 4095 });
    expect(plot.valueRange(trace(r, "pwm:0.0").points)).toEqual({ min: 0, max: 65520 });
  });
});

describe("configuration from a wiring", () => {
  it("SIL11: defaultConfig drives every input of a graph and takes its step from the scan period", async () => {
    const { graph } = await lamp();
    const c = sil.defaultConfig(graph);
    expect(c).toMatchObject({ stepMs: 10, durationMs: 2000, loopbacks: [] });
    expect(c.stimuli).toEqual([
      { pin: 15, kind: "digital", wave: { kind: "square", periodMs: 200, duty: 0.5, low: 0, high: 1 } },
      { pin: 14, kind: "digital", wave: { kind: "square", periodMs: 400, duty: 0.5, low: 0, high: 1 } },
    ]);
    // The two squares count through all four input combinations.
    const combos = new Set<string>();
    for (let t = 0; t < c.durationMs; t += c.stepMs) combos.add(c.stimuli.map((s) => (sil.waveAt(s.wave, t) >= 0.5 ? 1 : 0)).join(""));
    expect(combos.size).toBe(4);

    // 200 to 2000 steps whatever the scan period.
    for (const period of [1, 2, 5, 10, 50, 100, 1000]) {
      const d = sil.defaultConfig({ ...graph, period_ms: period });
      expect(d.stepMs).toBe(period);
      expect(d.durationMs / d.stepMs).toBeGreaterThanOrEqual(200);
      expect(d.durationMs / d.stepMs).toBeLessThanOrEqual(2000);
    }
    expect(sil.defaultConfig({ ...graph, period_ms: 1 }).durationMs).toBe(2000);

    const mixed: sil.SilGraph = {
      period_ms: 1,
      nodes: [
        { id: "gpio2", kind: "input", config: { pin: 2 } },
        { id: "adc26", kind: "analog", config: { pin: 26 } },
        { id: "nopin", kind: "input" },
        { id: "level", kind: "channel", config: { trace: true } },
        { id: "fan", kind: "pwm", config: { timer: 1, channel: 2 } },
        { id: "gpio25", kind: "output", config: { pin: 25 } },
      ],
      edges: [
        ["adc26", "level"],
        ["level", "fan"],
        ["level", "gpio25"],
      ],
    };
    const m = sil.defaultConfig(mixed);
    expect(m.stimuli).toEqual([
      { pin: 2, kind: "digital", wave: { kind: "square", periodMs: 20, duty: 0.5, low: 0, high: 1 } },
      { pin: 26, kind: "analog", wave: { kind: "sine", periodMs: 2000, min: 0, max: 4095 } },
    ]);
    // mwctl's report carries the wiring as it understood it.
    expect(sil.defaultConfig(mixed, { graph: { ...mixed, period_ms: 5 } }).stepMs).toBe(5);
    expect(sil.loopbackEnds(mixed)).toEqual({
      sources: [
        { label: "level → pwm1.2", from: { pwm: [1, 2] } },
        { label: "level → gpio25", from: { pin: 25 } },
      ],
      targets: [
        { label: "gpio2", to: { pin: 2, kind: "digital" } },
        { label: "adc26 → level", to: { pin: 26, kind: "analog" } },
      ],
    });
    expect(sil.traceLabel(null, "input", 4)).toBe("gpio4");
    expect(sil.traceLabel(mixed, "output", 9)).toBe("gpio9");
    expect(sil.traceLabel(mixed, "channel", "level")).toBe("level");
    expect(sil.traceId("pwm", [1, 2])).toBe("pwm:1.2");
  });

  it("SIL12: the panel keeps a user's waveforms and loopbacks while their pins are in the wiring", async () => {
    const { graph } = await lamp();
    const first = mergeConfig(null, graph, { duration: false, step: false });
    expect(first).toEqual(sil.defaultConfig(graph));
    const pulse: sil.Wave = { kind: "pulse", atMs: 5, widthMs: 50, low: 0, high: 1 };
    const edited: sil.SilConfig = {
      ...first,
      durationMs: 9000,
      stimuli: [{ pin: 15, kind: "digital", wave: pulse }, first.stimuli[1]],
      loopbacks: [{ from: { pin: 25 }, to: { pin: 14, kind: "digital" }, lagMs: 20 }],
    };
    const same = mergeConfig(edited, { ...graph, period_ms: 5 }, { duration: true, step: false });
    expect(same).toMatchObject({ durationMs: 9000, stepMs: 5, loopbacks: edited.loopbacks });
    expect(same.stimuli[0].wave).toEqual(pulse);
    // GPIO 14 leaves the wiring: its stimulus and the loopback to it go.
    const without = mergeConfig(edited, wire.removeNode(graph, "gpio14"), { duration: false, step: true });
    expect(without).toMatchObject({ durationMs: 2000, stepMs: 10, loopbacks: [] });
    expect(without.stimuli).toEqual([{ pin: 15, kind: "digital", wave: pulse }]);

    // The fields asked for: levels only for an analog pin.
    expect(waveFields("square", "digital").map((f) => f.key)).toEqual(["periodMs", "duty", "phaseMs"]);
    expect(waveFields("square", "analog").map((f) => f.key)).toEqual(["periodMs", "duty", "phaseMs", "low", "high"]);
    for (const kind of sil.WAVE_KINDS) {
      for (const pin of ["digital", "analog"] as const) {
        const wave = sil.defaultWave(kind, pin, 2000);
        expect(wave.kind).toBe(kind);
        for (const f of waveFields(kind, pin)) {
          expect(Number.isFinite(waveParam(wave, f)), `${kind}.${f.key}`).toBe(true);
          expect(waveParam(withWaveParam(wave, f.key, 3), f)).toBe(3);
        }
        const v = sil.waveAt(wave, 500);
        expect(v).toBeGreaterThanOrEqual(0);
        expect(v).toBeLessThanOrEqual(pin === "analog" ? 4095 : 1);
      }
    }
  });
});

describe("plot geometry", () => {
  const digital: sil.Trace = { id: "input:15", kind: "input", label: "gpio15 → button", digital: true, points: [{ t: 0, v: 0 }, { t: 100, v: 1 }, { t: 300, v: 0 }, { t: 1000, v: 0 }] };
  const numeric: sil.Trace = { id: "analog:26", kind: "analog", label: "adc26 → raw", digital: false, points: [{ t: 0, v: 1000 }, { t: 500, v: 3000 }, { t: 1000, v: 2000 }] };

  it("SIL13: time maps to x and a value to y inside its lane, at any width", () => {
    expect(plot.scale(5, 0, 10, 100, 200)).toBe(150);
    expect(plot.scale(5, 3, 3, 100, 200)).toBe(150);
    for (const width of [390 - 2 * 16 - 22, 640, 1200]) {
      const l = plot.layoutPlot([digital, numeric], width, 1000);
      expect(l.width).toBe(width);
      expect([l.t0, l.t1]).toEqual([0, 1000]);
      expect(plot.timeToX(l, 0)).toBe(plot.PLOT_PAD_X);
      expect(plot.timeToX(l, 1000)).toBe(width - plot.PLOT_PAD_X);
      expect(plot.timeToX(l, 500)).toBe(width / 2);
      expect(plot.xToTime(l, plot.timeToX(l, 250))).toBeCloseTo(250, 9);
      // Outside the axis the time is its nearest end.
      expect(plot.xToTime(l, -50)).toBe(0);
      expect(plot.xToTime(l, width + 50)).toBe(1000);
      // Lanes are stacked without overlap, the axis below them.
      const [a, b] = l.lanes;
      expect(a.bandHeight).toBe(plot.DIGITAL_H);
      expect(b.bandHeight).toBe(plot.NUMERIC_H);
      expect(a.bandTop).toBe(a.top + plot.LANE_LABEL_H);
      expect(b.top).toBeGreaterThanOrEqual(a.bandTop + a.bandHeight);
      expect(l.axisY).toBeGreaterThanOrEqual(b.bandTop + b.bandHeight);
      expect(l.height).toBe(l.axisY + plot.AXIS_H);
      // High is up; every value is inside its band.
      expect(plot.valueToY(a, 1)).toBe(a.bandTop + plot.BAND_INSET);
      expect(plot.valueToY(a, 0)).toBe(a.bandTop + a.bandHeight - plot.BAND_INSET);
      expect([b.min, b.max]).toEqual([1000, 3000]);
      expect(plot.valueToY(b, 3000)).toBe(b.bandTop + plot.BAND_INSET);
      expect(plot.valueToY(b, 1000)).toBe(b.bandTop + b.bandHeight - plot.BAND_INSET);
      expect(plot.valueToY(b, 2000)).toBe(b.bandTop + b.bandHeight / 2);
    }
    // Narrower than anything usable: still a drawing.
    expect(plot.layoutPlot([digital], 0, 1000).width).toBe(plot.MIN_PLOT_WIDTH);
  });

  it("SIL14: no traces, one point and a constant trace are laid out without dividing by zero", () => {
    const empty = plot.layoutPlot([], 400, 0);
    expect(empty).toMatchObject({ lanes: [], t0: 0, t1: 1, height: 4 + plot.AXIS_H });
    expect(plot.valueRange([])).toEqual({ min: 0, max: 0 });
    expect(plot.stepPath([], (t) => t, (v) => v, 100)).toBe("");
    expect(plot.valueAt([], 5)).toBeNull();

    const one: sil.Trace = { ...numeric, points: [{ t: 0, v: 7 }] };
    const l = plot.layoutPlot([one], 400, 0);
    expect(l.t1).toBe(1);
    const lane = l.lanes[0];
    const y = plot.valueToY(lane, 7);
    expect(y).toBe(lane.bandTop + lane.bandHeight / 2);
    const d = plot.stepPath(one.points, (t) => plot.timeToX(l, t), (v) => plot.valueToY(lane, v), l.x1);
    expect(d).toBe(`M8 ${y}H392`);
    expect(d).not.toContain("NaN");
    // A point at the very end still leaves a mark.
    expect(plot.stepPath([{ t: 10, v: 1 }], (t) => t, (v) => v, 10)).toBe("M10 1H11");
    // The axis reaches the last point when that is past the run's end.
    expect(plot.layoutPlot([digital], 400, 500).t1).toBe(1000);
  });

  it("SIL15: a step path holds each value to the next point, then jumps", () => {
    const x = (t: number) => t / 10;
    const y = (v: number) => 20 - v * 10;
    expect(plot.stepPath(digital.points, x, y, 120)).toBe("M0 20H10V10H30V20H100H120");
    // Coordinates are rounded to a tenth.
    expect(plot.stepPath([{ t: 1, v: 0.123 }, { t: 2.26, v: 1 }], x, y, 3.333)).toBe("M0.1 18.8H0.2V10H3.3");
  });

  it("SIL16: the value at a time is found by binary search: the last sample at or before it", () => {
    const p = digital.points;
    expect([-1, 0, 50, 99.9, 100, 299, 300, 999, 1000, 5000].map((t) => plot.sampleIndex(p, t))).toEqual([-1, 0, 0, 0, 1, 1, 2, 2, 3, 3]);
    expect(plot.valueAt(p, -1)).toBeNull();
    expect(plot.valueAt(p, 150)).toBe(1);
    expect(plot.valueAt(numeric.points, 499)).toBe(1000);
    // The same as a linear scan, on a long trace with repeated times.
    const long: sil.TracePoint[] = Array.from({ length: 5000 }, (_, i) => ({ t: Math.floor(i / 2) * 3, v: i }));
    for (const t of [0, 1, 2, 3, 4, 2999, 3000, 7497, 7498, 9999]) {
      let expected = -1;
      long.forEach((q, i) => {
        if (q.t <= t) expected = i;
      });
      expect(plot.sampleIndex(long, t)).toBe(expected);
    }
  });

  it("SIL17: ticks are round times, and times, values and the status line read naturally", () => {
    expect(plot.timeTicks(0, 2000, 5)).toEqual([0, 500, 1000, 1500, 2000]);
    expect(plot.timeTicks(0, 60_000, 4)).toEqual([0, 20_000, 40_000, 60_000]);
    expect(plot.timeTicks(0, 1, 4)).toEqual([0, 0.5, 1]);
    expect(plot.timeTicks(0, 0, 4)).toEqual([0]);
    expect(plot.timeTicks(0, 950, 3).every((t) => t >= 0 && t <= 950)).toBe(true);
    expect(plot.timeTicks(0, 2000, 3).length).toBeLessThanOrEqual(4);

    expect(plot.formatTime(1250, 2000)).toBe("1.25 s");
    expect(plot.formatTime(60_000, 60_000)).toBe("60 s");
    expect(plot.formatTime(250, 900)).toBe("250 ms");
    expect(plot.formatTime(0.5, 1)).toBe("0.5 ms");
    expect(plot.formatValue(4095)).toBe("4095");
    expect(plot.formatValue(0.48851)).toBe("0.489");
    expect(plot.formatValue(-2.5)).toBe("-2.5");
    expect(plot.fitLabel("gpio15 → button", 40)).toBe("gpio15 → button");
    expect(plot.fitLabel("gpio15 → button", 8)).toBe("gpio15 …");

    expect(plot.statusText({ simulatedMs: 60_000, wallMs: 410, speedup: 60_000 / 410 })).toBe("Simulated 60.0 s in 0.41 s (146× real time)");
    expect(plot.statusText({ simulatedMs: 1500, wallMs: 1500, speedup: 1 }, "running")).toBe("Simulating: 1.5 s in 1.50 s (1× real time)");
    expect(plot.statusText({ simulatedMs: 300, wallMs: 120, speedup: 2.5 }, "stopped")).toBe("Stopped. Simulated 0.3 s in 0.12 s (2.5× real time)");
    expect(plot.statusText({ simulatedMs: 0, wallMs: 0, speedup: 0 })).toBe("Simulated 0.0 s in 0.00 s");

    expect(plot.readout([digital, numeric], 150, 1000)).toEqual({
      time: "0.15 s",
      values: [
        { label: "gpio15 → button", value: "high" },
        { label: "adc26 → raw", value: "1000" },
      ],
    });
    expect(plot.readout([numeric], -5, 1000).values).toEqual([{ label: "adc26 → raw", value: "–" }]);
  });
});
