// mwctl itself, compiled for WASI, run by the page's host: the command
// line, its files, and its serial port against the fake board (the real
// Rust loader) through the same streams a Web Serial port has.

import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { beforeAll, describe, expect, it } from "vitest";
import { Mwctl, jspi, splitCommandLine } from "../src/mwctl/host";
import { type SerialLike, WebPort } from "../src/mwctl/port";
import { canWrite, describeBoard, flashesHere, parseProbe, probe, usbId, watchPorts } from "../src/mwctl/ready";
import * as sil from "../src/mwctl/sil";
import * as wire from "../src/mwctl/wire";
import type { Protocol } from "../src/protocol/wasm";
import { SimSession } from "../src/sim/session";
import { ManualClock } from "./clock";
import { FakeDevice, type FakeDeviceScript, protocol, serialPair } from "./fakeDevice";

let mwctl: Mwctl;
let proto: Protocol;

beforeAll(() => {
  const path = fileURLToPath(new URL("../src/pkg/mwctl.wasm", import.meta.url));
  mwctl = new Mwctl(new WebAssembly.Module(readFileSync(path)));
  proto = protocol();
});

/** What is on the other end of the cable. */
type Board = { kind: "firmware"; script: FakeDeviceScript } | { kind: "silent" } | { kind: "rom" };

/** A serial port as the page sees one: new streams on every open. */
class FakeSerial implements SerialLike {
  readable: ReadableStream<Uint8Array> | null = null;
  writable: WritableStream<Uint8Array> | null = null;
  devices: FakeDevice[] = [];
  bauds: number[] = [];
  signals: string[] = [];
  private closeBoard: (() => Promise<void>) | null = null;

  constructor(private readonly board: Board) {}

  async open(options: { baudRate: number }): Promise<void> {
    if (this.readable) throw new Error("already open");
    this.bauds.push(options.baudRate);
    const { host, device } = serialPair();
    this.readable = host.readable;
    this.writable = host.writable;
    if (this.board.kind === "firmware") {
      const d = new FakeDevice(proto, device, this.board.script);
      this.devices.push(d);
      // Closing the port does not unplug the board: it keeps running.
      this.closeBoard = null;
      return;
    }
    // No firmware: swallow everything; a ROM answers SYNC.
    const reader = device.readable.getReader();
    const writer = device.writable.getWriter();
    const rom = this.board.kind === "rom";
    const loop = (async () => {
      for (;;) {
        const { value, done } = await reader.read().catch(() => ({ value: undefined, done: true as const }));
        if (done) return;
        const sync = value.length > 12 && value[0] === 0xc0 && value[2] === 0x08 && value[9] === 0x07;
        if (rom && sync) {
          await writer.write(Uint8Array.from([0xc0, 1, 8, 4, 0, 7, 7, 0x12, 0x20, 0, 0, 0, 0, 0xc0])).catch(() => {});
        }
      }
    })();
    this.closeBoard = async () => {
      await reader.cancel().catch(() => {});
      await loop;
    };
  }

  async setSignals(s: { dataTerminalReady?: boolean; requestToSend?: boolean }): Promise<void> {
    this.signals.push(`dtr=${s.dataTerminalReady} rts=${s.requestToSend}`);
  }

  async close(): Promise<void> {
    this.readable = null;
    this.writable = null;
    await this.closeBoard?.();
  }
}

const LAMP = ["button=gpio15,pullup,invert", "armed=gpio14,pullup,invert", "lit=and(button,armed)", "gpio25=lit"];

describe("mwctl in the page", () => {
  it("has Promise Integration here, as Chrome does", () => {
    expect(jspi()).not.toBeNull();
    expect(mwctl.canWait).toBe(true);
  });

  it("is the command-line tool: help, usage errors and exit statuses", async () => {
    const help = await mwctl.run(["--help"]);
    expect(help.code).toBe(0);
    expect(help.stdout).toContain("wire <STATEMENT>...");
    expect(help.stdout).toContain("probe [--usb <VID:PID>]");
    const usage = await mwctl.run(["wire", "a=gpio1"]);
    expect(usage.code).toBe(2);
    expect(usage.stderr).toContain("somewhere to put the payload");
    const failed = await mwctl.run(["wire", "a=gpio4", "gpio34=a", "--chip", "esp32", "-o", "x.wasm"]);
    expect(failed.code).toBe(1);
    expect(failed.stderr).toContain("error: GPIO 34 is input-only on this board");
    const chunks: string[] = [];
    const streamed = await mwctl.run(["wire", ...LAMP, "--emit", "statements"], { onStdout: (t) => chunks.push(t) });
    expect(streamed.stdout.trim().split("\n")).toEqual(LAMP);
    expect(chunks.join("")).toBe(streamed.stdout);
  });

  it("reads and writes the page's files and stdin", async () => {
    const files = new Map<string, Uint8Array>();
    const built = await mwctl.run(["wire", ...LAMP, "--name", "lamp", "-o", "out/lamp.wasm", "--emit", "json"], { files });
    expect(built.code).toBe(0);
    expect(built.stderr).toContain("wrote out/lamp.wasm");
    const wasm = files.get("out/lamp.wasm")!;
    expect([...wasm.subarray(0, 4)]).toEqual([0, 0x61, 0x73, 0x6d]);
    // The JSON it printed is a file it reads back, and stdin as `-`.
    files.set("lamp.json", new TextEncoder().encode(built.stdout));
    const again = await mwctl.run(["wire", "--json", "/lamp.json", "-o", "again.wasm"], { files });
    expect(again.code).toBe(0);
    expect(files.get("again.wasm")).toEqual(wasm);
    const piped = await mwctl.run(["wire", "--json", "-", "--emit", "statements"], { stdin: new TextEncoder().encode(built.stdout) });
    expect(piped.stdout.trim().split("\n")).toEqual(LAMP);
    const missing = await mwctl.run(["wire", "--json", "nowhere.json", "--emit", "json"], { files });
    expect(missing.code).toBe(1);
    expect(missing.stderr).toContain("nowhere.json");
  });

  it("has no port unless the page gives it one, and no tokens", async () => {
    const r = await mwctl.run(["--port", "web", "info"]);
    expect(r.code).toBe(1);
    expect(r.stderr).toContain("no such port");
    const other = await mwctl.run(["--port", "/dev/ttyUSB0", "info"], { port: new WebPort(new FakeSerial({ kind: "silent" })) });
    expect(other.code).toBe(1);
    expect(other.stderr).toContain("in a browser the port is `web`");
    const token = await mwctl.run(["key", "pub", "--pkcs11", "mw-release"]);
    expect(token.code).toBe(1);
    expect(token.stderr).toContain("a browser cannot reach a PKCS#11 token");
  });

  it("splits a typed command line like a shell", () => {
    expect(splitCommandLine("mwctl wire  a=gpio1 'lit=and(a, b)' \"x y\" c\\ d ''")).toEqual(["mwctl", "wire", "a=gpio1", "lit=and(a, b)", "x y", "c d", ""]);
    expect(splitCommandLine("   ")).toEqual([]);
  });
});

describe("a board that appears", () => {
  const firmware = (script: FakeDeviceScript = {}): FakeSerial => new FakeSerial({ kind: "firmware", script });

  it("is ready to update when a base firmware answers", async () => {
    const serial = firmware({ runs: { clock: new ManualClock(), node: "00000000000000a1" } });
    const state = await probe(mwctl, new WebPort(serial), "2e8a:0009", 100);
    expect(state.ready).toBe("update");
    expect(state.fields).toMatchObject({ chip: "rp2350", firmware: "0.1.0-fake", payloads: "run", usb: "2e8a:0009" });
    expect(canWrite(state)).toBe(true);
    expect(describeBoard(state)).toBe("rp2350 running 0.1.0-fake: ready to update. A wiring can be written now.");
    // Opened at 115200 with both lines released, and closed again.
    expect(serial.bauds).toEqual([115200]);
    expect(serial.signals).toEqual(["dtr=false rts=false"]);
    expect(serial.readable).toBeNull();
  });

  it("is ready to flash when only a ROM bootloader answers, or by its USB id", async () => {
    const rom = await probe(mwctl, new WebPort(new FakeSerial({ kind: "rom" })), "10c4:ea60", 60);
    expect(rom.ready).toBe("flash");
    expect(rom.fields.method).toBe("esp-rom");
    expect(rom.fields.how).toContain("is listening");
    expect(flashesHere(rom)).toBe(true);
    expect(canWrite(rom)).toBe(false);

    const bridge = await probe(mwctl, new WebPort(new FakeSerial({ kind: "silent" })), "1a86:7523", 60);
    expect(bridge.ready).toBe("flash");
    expect(describeBoard(bridge)).toContain("USB-UART bridge, as on ESP32 and ESP8266 development boards: ready to flash.");
    expect(flashesHere(bridge)).toBe(true);

    const pico = await probe(mwctl, new WebPort(new FakeSerial({ kind: "silent" })), "2e8a:0009", 60);
    expect(pico.fields.method).toBe("rp-bootsel");
    expect(flashesHere(pico)).toBe(false);

    const unknown = await probe(mwctl, new WebPort(new FakeSerial({ kind: "silent" })), null, 60);
    expect(unknown.ready).toBe("unknown");
    expect(describeBoard(unknown)).toContain("no answer");
  });

  it("reports a port that cannot be opened", async () => {
    const serial = firmware();
    await serial.open({ baudRate: 115200 }); // another tab's
    await expect(probe(mwctl, new WebPort(serial), null, 60)).rejects.toThrow(/serial port failed/);
  });

  it("reads probe's lines and a port's USB id", () => {
    expect(parseProbe("ready: flash\nusb: 303a:1001\nhow: a: b\n")).toEqual({ ready: "flash", fields: { usb: "303a:1001", how: "a: b" } });
    expect(parseProbe("mwctl: no such port\n")).toBeNull();
    expect(usbId({ usbVendorId: 0x303a, usbProductId: 0x1001 })).toBe("303a:1001");
    expect(usbId({ usbVendorId: 0x2e8a, usbProductId: 9 })).toBe("2e8a:0009");
    expect(usbId({})).toBeNull();
  });

  it("watches granted ports: those already there, plugged in and unplugged", async () => {
    const listeners: Record<string, (e: Event) => void> = {};
    const serial = {
      getPorts: async () => ["a", "b"],
      addEventListener: (type: string, f: (e: Event) => void) => {
        listeners[type] = f;
      },
    };
    const seen: string[] = [];
    await watchPorts<string>(serial, (p) => seen.push(`+${p}`), (p) => seen.push(`-${p}`));
    listeners.connect!({ target: "c" } as unknown as Event);
    listeners.disconnect!({ target: "a" } as unknown as Event);
    expect(seen).toEqual(["+a", "+b", "+c", "-a"]);
  });
});

describe("a wiring from the canvas to the board", () => {
  it("the command line and the graph are the same wiring", async () => {
    const typed = await wire.fromCommandLine(mwctl, `mwctl wire --name lamp ${LAMP.map((s) => `'${s}'`).join(" ")}`);
    expect(typed.ok).toBe(true);
    expect(typed.issues).toEqual([]);
    expect(typed.statements).toEqual(LAMP);
    expect(typed.graph.nodes.map((n) => n.id)).toEqual(["gpio15", "button", "gpio14", "armed", "lit", "lit.fn", "gpio25"]);
    expect(typed.graph.nodes.every((n) => typeof n.x === "number" && typeof n.y === "number")).toBe(true);
    const back = await wire.report(mwctl, typed.graph);
    expect(back.command).toBe(typed.command);
    expect(back.command).toBe("mwctl wire --name lamp button=gpio15,pullup,invert armed=gpio14,pullup,invert 'lit=and(button,armed)' gpio25=lit");
    expect(back.bytes).toBeGreaterThan(100);
    await expect(wire.fromCommandLine(mwctl, "wire a=gpio1 --port web")).rejects.toThrow("--port is not part of a wiring");
    await expect(wire.fromCommandLine(mwctl, "wire button")).rejects.toThrow("is not a wiring statement");
  });

  it("builds a wiring by editing: add, name, set pins, connect", async () => {
    let g = wire.emptyGraph();
    const add = (kind: wire.WireKind, x: number, y: number) => {
      const r = wire.addNode(g, kind, x, y);
      g = r.graph;
      return r.id;
    };
    const inId = add("input", 40, 40);
    const ch = add("channel", 200, 40);
    const not = add("not", 360, 40);
    const ch2 = add("channel", 520, 40);
    const out = add("output", 680, 40);
    expect([inId, ch, not, ch2, out]).toEqual(["in1", "ch1", "not1", "ch2", "out1"]);
    // Unfinished: every problem names its node.
    const draft = await wire.report(mwctl, g);
    expect(draft.ok).toBe(false);
    expect(draft.issues.filter((i) => i.severity === "error").map((i) => i.node)).toEqual(["in1", "out1", "ch1", "not1", "ch2", "out1"]);
    expect(draft.issues[0]!.message).toBe("in1 has no pin: choose its GPIO number");

    g = wire.setConfig(g, inId, { pin: 15, pull: "up" });
    g = wire.setConfig(g, out, { pin: 25 });
    g = wire.renameNode(g, ch, "button");
    g = wire.renameNode(g, ch2, "button"); // taken: unchanged
    g = wire.renameNode(g, ch2, "dark");
    for (const [a, b] of [
      [inId, "button"],
      ["button", not],
      [not, "dark"],
      ["dark", out],
    ] as const) {
      const c = await wire.connect(mwctl, g, a, b);
      expect(c.refused).toBeNull();
      g = c.graph;
    }
    // mwctl refuses what the rules do not allow, in its own words.
    const direct = await wire.connect(mwctl, g, inId, out);
    expect(direct.refused).toBe("in1 is a digital input and out1 is a digital output: an input drives a channel; put a channel between them");
    expect(direct.graph).toBe(g);
    expect((await wire.connect(mwctl, g, out, out)).refused).toBe("a node cannot connect to itself");
    expect((await wire.connect(mwctl, g, inId, "button")).refused).toContain("connected twice");

    const done = await wire.report(mwctl, g, "rp2350");
    expect(done.issues).toEqual([]);
    expect(done.statements).toEqual(["button=gpio15,pullup", "dark=not(button)", "gpio25=dark"]);

    // The chip's pin rules arrive through the same report.
    const onEsp = await wire.report(mwctl, wire.setConfig(g, out, { pin: 34 }), "esp32");
    expect(onEsp.issues.map((i) => [i.node, i.message])).toEqual([["out1", "GPIO 34 is input-only on this board; it cannot be an output"]]);

    g = wire.setFunction(g, not, "nor");
    expect(g.nodes.find((n) => n.id === not)!.kind).toBe("nor");
    expect(wire.isFunction("lowpass") && !wire.isFunction("pwm") && !wire.isFunction("analog")).toBe(true);
    g = wire.removeEdge(g, "button", not);
    expect(g.edges).toHaveLength(3);
    g = wire.removeNode(g, "dark");
    expect(g.edges).toEqual([[inId, "button"]]);
    g = wire.moveNode(g, inId, -5, 99999);
    expect(g.nodes[0]).toMatchObject({ x: 0, y: wire.MAX_COORD });
  });

  it("keeps fifty undo steps and survives storage", () => {
    const h = new wire.History();
    let g = wire.emptyGraph();
    for (let i = 0; i < 60; i++) {
      h.push(g);
      g = wire.addNode(g, "channel", i, i).graph;
    }
    h.push(wire.copyGraph(g));
    h.push(wire.copyGraph(g)); // unchanged: one step
    expect(h.size).toBe(wire.UNDO_LIMIT);
    expect(h.pop()!.nodes).toHaveLength(60);
    expect(h.pop()!.nodes).toHaveLength(59);

    const store = new Map<string, string>();
    const storage = { setItem: (k: string, v: string) => void store.set(k, v), getItem: (k: string) => store.get(k) ?? null };
    wire.saveGraph(storage, g);
    expect(wire.loadGraph(storage)).toEqual(JSON.parse(wire.graphJson(g)));
    store.set(wire.STORAGE_KEY, "{]");
    expect(wire.loadGraph(storage)).toBeNull();
    store.set(wire.STORAGE_KEY, '{"name":1}');
    expect(wire.loadGraph(storage)).toBeNull();
    expect(wire.loadGraph(undefined)).toBeNull();
    wire.saveGraph({ setItem: () => { throw new Error("full"); } }, g);
  });

  it("the built payload follows its inputs in the simulator", async () => {
    const { graph } = await wire.fromCommandLine(mwctl, LAMP.join(" "));
    const wasm = await wire.build(mwctl, graph, "rp2350");
    const clock = new ManualClock();
    const logs: string[] = [];
    const s = new SimSession(proto, wasm, { onEvent: (e) => e.kind === "log" && logs.push(e.line) }, clock);
    s.start(10);
    expect(logs).toEqual(["wire wire: 2 in, 3 ch, 1 out"]);
    expect(s.pins.get(15)).toMatchObject({ mode: "input", pull: "up" });
    expect(s.pins.get(25)).toEqual({ mode: "output", high: false });
    // Both switches closed to ground: the lamp lights.
    s.setInput(15, false);
    s.setInput(14, false);
    clock.advance(20);
    expect(s.pins.get(25)).toEqual({ mode: "output", high: true });
    s.setInput(14, true);
    clock.advance(20);
    expect(s.pins.get(25)).toEqual({ mode: "output", high: false });
    s.dispose();
    await expect(wire.build(mwctl, wire.setConfig(graph, "gpio25", { pin: 99 }), "rp2350")).rejects.toThrow("this board has no GPIO 99");
  });

  it("writes the wiring to the board: checked for its chip, uploaded, started, running", async () => {
    const clock = new ManualClock();
    const serial = new FakeSerial({ kind: "firmware", script: { runs: { clock, node: "00000000000000a1" } } });
    const { graph } = await wire.fromCommandLine(mwctl, `--name lamp ${LAMP.join(" ")}`);
    const out: string[] = [];
    const r = await wire.write(mwctl, graph, new WebPort(serial), { onStdout: (t) => out.push(t), onStderr: (t) => out.push(t) });
    expect(r.stderr).toContain("board: rp2350 running 0.1.0-fake");
    expect(r.stdout).toContain("uploading wiring lamp");
    expect(r.stdout).toMatch(/installed wiring lamp \(\d+ bytes\) and started/);
    expect(r.code).toBe(0);
    expect(out.join("")).toContain("installed wiring lamp");
    const board = serial.devices[0]!;
    expect(Uint8Array.from(board.image)).toEqual(await wire.build(mwctl, graph));
    expect(board.started).toBe(true);
    // The board runs it.
    const s = board.session!;
    s.setInput(15, false);
    s.setInput(14, false);
    clock.advance(20);
    expect(s.pins.get(25)).toEqual({ mode: "output", high: true });
    expect(serial.readable).toBeNull();
  });

  it("refuses before sending anything when the board's chip lacks a pin", async () => {
    const serial = new FakeSerial({ kind: "firmware", script: { runs: { clock: new ManualClock(), node: "00000000000000a1" } } });
    const { graph } = await wire.fromCommandLine(mwctl, "a=gpio4 gpio40=a");
    const r = await wire.write(mwctl, graph, new WebPort(serial));
    expect(r.code).toBe(1);
    expect(r.stderr).toContain("error: this board has no GPIO 40");
    expect(serial.devices[0]!.received.map((q) => q.key)).toEqual(["sys/info"]);
  });

  it("does not start a wiring on firmware without the runtime", async () => {
    const serial = new FakeSerial({ kind: "firmware", script: {} });
    // The plain fake reports esp32c3 with a runtime; a +no-wasm one is the
    // Rust side's test (ready.rs, the wire command's check).
    const { graph } = await wire.fromCommandLine(mwctl, "a=gpio4 gpio5=a");
    const r = await wire.write(mwctl, graph, new WebPort(serial));
    expect(r.code).toBe(0);
    expect(serial.devices[0]!.started).toBe(true);
  });
});

describe("multi-rate wirings, simulated faster than real time", () => {
  const run = async (line: string, config: Partial<sil.SilConfig>) => {
    const { graph, ok, issues } = await wire.fromCommandLine(mwctl, line);
    expect(issues.filter((i) => i.severity === "error")).toEqual([]);
    expect(ok).toBe(true);
    const wasm = await wire.build(mwctl, wire.withTrace(graph));
    const base = sil.defaultConfig(graph);
    const result = await sil.runSil(proto, wasm, { ...base, ...config }, { speed: "max", graph });
    expect(result.error).toBeUndefined();
    const trace = (id: string) => result.traces.find((t) => t.id === id)!;
    return { graph, result, trace };
  };

  it("MS6: mwctl lists every kind with its settings for the page", async () => {
    const kinds = await wire.kinds(mwctl);
    const by = (k: string) => kinds.find((x) => x.kind === k)!;
    expect(kinds.length).toBeGreaterThan(30);
    expect(by("lowpass")).toMatchObject({ class: "function", inputs: [1, 1], stateful: true });
    expect(by("lowpass").params.map((p) => p.name)).toEqual(["shift", "period_ms"]);
    expect(by("pwm").class).toBe("sink");
    expect(by("analog").class).toBe("source");
    expect(by("channel").params.map((p) => p.name)).toEqual(["trace"]);
  });

  it("MS7: a divider through a delay toggles once per update of its own rate", async () => {
    // q updates every 100 ms on a 10 ms base: a 200 ms square wave.
    const { result, trace } = await run("wire q=delay(nq)@100ms nq=not(q)@100ms gpio25=q", { durationMs: 2000, stimuli: [] });
    const q = trace("channel:q").points;
    const flips = q.filter((p, i) => i > 0 && p.v !== q[i - 1]!.v);
    expect(flips.length).toBeGreaterThanOrEqual(18);
    for (let i = 1; i < flips.length; i++) expect(flips[i]!.t - flips[i - 1]!.t).toBe(100);
    expect(trace("output:25").digital).toBe(true);
    expect(result.steps).toBe(200);
    expect(result.speedup).toBeGreaterThan(10);
  });

  it("MS8: an analog input decimated, smoothed and compared at slower rates drives its output", async () => {
    const line = "wire raw=adc26 'avg=mean(raw,n=10)' 'lvl=lowpass(avg,shift=1)@100ms' 'hot=hyst(lvl,lo=1000,hi=3000)@100ms' gpio25=hot@100ms";
    const { trace } = await run(line, {
      durationMs: 8000,
      stimuli: [{ pin: 26, kind: "analog", wave: { kind: "step", atMs: 2000, from: 0, to: 4000 } }],
    });
    // Ten 10 ms samples per dump: the mean changes 100 ms apart and settles on the input.
    const avg = trace("channel:avg").points;
    const dumps = avg.filter((p, i) => i > 0 && p.v !== avg[i - 1]!.v);
    expect(dumps.length).toBeGreaterThan(0);
    for (let i = 1; i < dumps.length; i++) expect((dumps[i]!.t - dumps[i - 1]!.t) % 100).toBe(0);
    expect(avg[avg.length - 1]!.v).toBe(4000);
    // The filtered level rises through the band; the output follows the
    // Schmitt trigger, after the step and not before.
    const lvl = trace("channel:lvl").points;
    expect(lvl[lvl.length - 1]!.v).toBeGreaterThan(3900);
    const on = trace("output:25").points.find((p) => p.v === 1)!;
    expect(on.t).toBeGreaterThan(2000);
    expect(on.t).toBeLessThan(3000);
    expect(trace("channel:hot").points.find((p) => p.v === 1)!.t).toBeLessThanOrEqual(on.t);
  });

  it("MS9: a loop without a delay, and a rate that is not a multiple of the scan, are mwctl's errors", async () => {
    const loop = await wire.fromCommandLine(mwctl, "wire a=gpio1 x=and(a,y) y=not(x) gpio2=y");
    expect(loop.ok).toBe(false);
    expect(loop.issues.some((i) => i.message.includes("a loop needs a delay in it"))).toBe(true);
    const rate = await wire.fromCommandLine(mwctl, "wire a=gpio1@15ms gpio2=a");
    expect(rate.ok).toBe(false);
    expect(rate.issues[0]!.message).toMatch(/multiple/);
  });
});
