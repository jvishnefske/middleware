// D10 (browser part): ABI 2 payloads — interrupts, timers, SPI and UART
// DMA — run in the protocol-wasm simulator in virtual time. The WAT
// fixtures in test/fixtures are shared with the protocol-wasm Rust tests.

import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";
import { checkAbi } from "../src/editor/abiCheck";
import { compile, compileWat, type Language } from "../src/editor/compile";
import { template, templateFrom, templateNames, templateNamesFrom } from "../src/editor/templates";
import type { SimEvent } from "../src/protocol/wasm";
import { SimSession } from "../src/sim/session";
import { ManualClock } from "./clock";
import { protocol } from "./fakeDevice";

const proto = protocol();

function fixture(name: string): string {
  return readFileSync(new URL(`./fixtures/${name}.wat`, import.meta.url), "utf8");
}

async function wat(source: string): Promise<Uint8Array> {
  const r = await compileWat(source);
  if (!r.ok) throw new Error(r.errors);
  return r.wasm;
}

function run(wasm: Uint8Array) {
  const clock = new ManualClock();
  const events: SimEvent[] = [];
  const errors: string[] = [];
  const s = new SimSession(proto, wasm, { onEvent: (e) => events.push(e), onError: (m) => errors.push(m) }, clock);
  s.start();
  return { s, clock, events, errors };
}

const published = (events: SimEvent[], key: string) =>
  events.flatMap((e) => (e.kind === "publish" && e.key === key ? [e.data] : []));
const lastI32 = (events: SimEvent[], key: string) => {
  const d = published(events, key).at(-1);
  return d ? new DataView(d.buffer, d.byteOffset, d.byteLength).getInt32(0, true) : undefined;
};

describe("ABI 2 constants and checks", () => {
  it("abi() exposes the ABI 2 imports, on_irq and event constants", () => {
    const names = proto.abi.imports.map((i) => i.name);
    for (const n of ["now_us", "irq_overflow", "gpio_irq", "timer_start", "timer_stop", "spi_open", "spi_transfer", "dma_read", "uart_open", "uart_write", "uart_read"]) {
      expect(names).toContain(n);
    }
    expect(proto.abi.version).toBe(2);
    expect(proto.abi.exports.onIrq).toBe("on_irq");
    expect(proto.abi.eventKinds).toEqual({ Gpio: 1, Timer: 2, DmaDone: 3, UartRx: 4 });
    expect(proto.abi.dmaChannels).toEqual({ Spi0: 0, Uart1Tx: 1 });
    expect(proto.abi.edges).toEqual({ Off: 0, Rising: 1, Falling: 2, Any: 3 });
    expect(proto.abi.maxDma).toBe(4092);
    expect(proto.abi.maxTimers).toBe(4);
    expect(proto.abi.maxEvents).toBe(32);
    expect(proto.abi.errors.Busy).toBe(-7);
  });

  it("abiCheck accepts the ABI 2 imports and the on_irq export", async () => {
    for (const name of ["timer_count", "button_irq", "spi_loopback", "uart_echo", "overflow"]) {
      const report = checkAbi(await wat(fixture(name)), proto.abi);
      expect(report.problems, name).toEqual([]);
      expect(report.warnings, name).toEqual([]);
      expect(report.exports).toContain("on_irq");
    }
  });

  it("abiCheck warns about event sources without on_irq and rejects a non-function on_irq", async () => {
    const noIrq = await wat(`(module (import "mw" "timer_start" (func (param i32 i32 i32) (result i32)))
      (memory (export "memory") 1))`);
    const r = checkAbi(noIrq, proto.abi);
    expect(r.problems).toEqual([]);
    expect(r.warnings).toEqual(['imports timer_start but does not export "on_irq"; its events will be discarded']);
    const bad = await wat(`(module (memory (export "memory") 1) (global (export "on_irq") i32 (i32.const 0)))`);
    expect(checkAbi(bad, proto.abi).problems).toEqual(['export "on_irq" must be a function']);
  });

  it("template selector offers new templates per language and keeps the base ones", () => {
    const files = {
      "t/blink.wat": "",
      "t/button.wat": "",
      "t/timer.wat": "",
      "t/button.ts": "",
      "t/spi.ts": "",
      "t/README.md": "",
    };
    expect(templateNamesFrom(files, "wat")).toEqual(["blink", "echo", "button", "timer"]);
    expect(templateNamesFrom(files, "as")).toEqual(["blink", "echo", "button", "spi"]);
    expect(templateNamesFrom({}, "wat")).toEqual(["blink", "echo"]);
    expect(templateFrom(files, "timer", "as").builtin).toBe(true);
  });
});

describe.runIf(proto.hasSimulator)("ABI 2 simulator (virtual time)", () => {
  it("counts periodic timer ticks", async () => {
    const { s, clock, events, errors } = run(await wat(fixture("timer_count")));
    expect(s.handlesIrq).toBe(true);
    clock.advance(100);
    expect(errors).toEqual([]);
    expect(lastI32(events, "app/ticks")).toBe(100);
    expect(s.peripherals().timers).toEqual([{ id: 0, periodUs: 1000, periodic: true, active: true, fires: 100 }]);
    // 1 ms alarms coalesce into one event per 10 ms poll.
    expect(events.filter((e) => e.kind === "irq" && e.irq === "Timer")).toHaveLength(10);
    s.dispose();
  });

  it("reacts to a simulated button edge", async () => {
    const { s, clock, events } = run(await wat(fixture("button_irq")));
    expect(s.pins.get(4)).toEqual({ mode: "input", pull: "up", high: true });
    clock.advance(10);
    expect(s.setInput(4, false)).toBe(true);
    expect(s.peripherals().pending).toBe(1);
    clock.advance(10);
    expect(s.pins.get(8)).toEqual({ mode: "output", high: true });
    expect(events.some((e) => e.kind === "log" && e.line === "press")).toBe(true);
    const irq = events.find((e) => e.kind === "irq");
    expect(irq).toMatchObject({ kind: "irq", irq: "Gpio", source: 4, value: 0, timeUs: 10_000, handled: true });
    s.setInput(4, true);
    clock.advance(10);
    expect(s.pins.get(8)).toEqual({ mode: "output", high: true });
    s.dispose();
  });

  it("round-trips SPI loopback through dma_read", async () => {
    const { s, clock, events } = run(await wat(fixture("spi_loopback")));
    const tx = [0xde, 0xad, 0xbe, 0xef];
    expect(published(events, "app/spi")).toEqual([]);
    clock.advance(10);
    expect(published(events, "app/spi").map((d) => [...d])).toEqual([tx]);
    const v = s.peripherals();
    expect([...(v.lastSpi?.tx ?? [])]).toEqual(tx);
    expect([...(v.lastSpi?.rx ?? [])]).toEqual(tx);
    s.dispose();
  });

  it("echoes UART bytes", async () => {
    const { s, clock, events } = run(await wat(fixture("uart_echo")));
    expect(s.uartInject(new TextEncoder().encode("hello"))).toBe(5);
    clock.advance(10);
    const tx = events.filter((e) => e.kind === "uart");
    expect(tx.map((e) => e.kind === "uart" && new TextDecoder().decode(e.data))).toEqual(["hello"]);
    clock.advance(10);
    expect(events.some((e) => e.kind === "log" && e.line === "sent")).toBe(true);
    s.dispose();
  });

  it("reports overflow when GPIO edges flood the queue", async () => {
    const { s, clock, events } = run(await wat(fixture("overflow")));
    const extra = 8;
    for (let i = 0; i < proto.abi.maxEvents + extra; i++) s.setInput(4, i % 2 === 1);
    expect(s.peripherals()).toMatchObject({ pending: proto.abi.maxEvents, dropped: extra });
    clock.advance(10);
    expect(published(events, "app/overflow")).toHaveLength(1);
    expect(lastI32(events, "app/overflow")).toBe(extra);
    expect(s.peripherals()).toMatchObject({ pending: 0, dropped: extra, irqs: proto.abi.maxEvents });
    s.dispose();
  });
});

// The ABI 2 templates in payloads/templates are written concurrently; each
// case runs once its file exists and is skipped (with the reason in its
// name) until then.
const ABI2_TEMPLATES: [string, Language][] = [
  ["button", "wat"],
  ["timer", "wat"],
  ["button", "as"],
  ["spi", "as"],
];

describe.runIf(proto.hasSimulator)("ABI 2 templates from payloads/templates", () => {
  for (const [name, lang] of ABI2_TEMPLATES) {
    const file = `${name}.${lang === "wat" ? "wat" : "ts"}`;
    const present = templateNames(lang).includes(name);
    it.skipIf(!present)(`${file} compiles, loads and handles interrupts${present ? "" : ` (skipped: payloads/templates/${file} not found)`}`, async () => {
      const t = template(name, lang);
      expect(t.builtin).toBe(false);
      const r = await compile(lang, t.source);
      if (!r.ok) throw new Error(r.errors);
      const report = checkAbi(r.wasm, proto.abi);
      expect(report.problems).toEqual([]);
      const { s, clock, events, errors } = run(r.wasm);
      expect(s.handlesIrq).toBe(true);
      // Stimulate every source a template might use: input edges, UART RX
      // and time for timers and DMA completions.
      for (let i = 0; i < 4; i++) {
        for (const [pin, p] of s.pins) if (p.mode === "input") s.setInput(pin, !p.high);
        s.uartInject(new TextEncoder().encode("x"));
        clock.advance(250);
      }
      expect(errors).toEqual([]);
      expect(events.some((e) => e.kind === "irq" && e.handled)).toBe(true);
      s.dispose();
    });
  }
});
