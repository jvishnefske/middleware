// Blocks editor, build side: every example workspace compiles with asc and
// passes checkAbi; the badge profile's 1-byte-page rewrite; generated
// payloads running in the protocol-wasm simulator (wasm-host compiled to
// WASM) in virtual time.

import { describe, expect, it } from "vitest";
import { initialMemoryBytes, toBytePages } from "../src/blocks/bytePages";
import { EXAMPLES, exampleWorkspace } from "../src/blocks/examples";
import { type BuildResult, buildPayload, usesMemorySize } from "../src/blocks/pipeline";
import { BADGE_MEMORY_BYTES, type ProfileId } from "../src/blocks/profiles";
import type { BlockJson, WorkspaceJson } from "../src/blocks/workspace";
import { compileAssemblyScript, compileWat } from "../src/editor/compile";
import type { SimEvent } from "../src/protocol/wasm";
import { SimSession } from "../src/sim/session";
import { ManualClock } from "./clock";
import { protocol } from "./fakeDevice";

const proto = protocol();

const ws = (...blocks: BlockJson[]): WorkspaceJson => ({ blocks: { languageVersion: 0, blocks } });
const num = (n: number) => ({ shadow: { type: "math_number", fields: { NUM: n } } });

async function build(w: WorkspaceJson, profile: ProfileId): Promise<Extract<BuildResult, { ok: true }>> {
  const r = await buildPayload(w, profile, proto.abi);
  if (!r.ok) throw new Error(`${r.stage}: ${r.errors.join("; ")}\n${r.source}`);
  return r;
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
const text = (b: Uint8Array) => new TextDecoder().decode(b);

describe("Blocks examples", () => {
  for (const e of EXAMPLES) {
    it(`example "${e.id}" compiles and passes checkAbi (${e.profile})`, async () => {
      const r = await build(e.workspace, e.profile);
      expect(r.report.problems).toEqual([]);
      expect(r.report.warnings).toEqual([]);
      expect(r.warnings).toEqual([]);
      expect(r.report.imports.every((i) => i.startsWith("mw."))).toBe(true);
      expect(r.report.exports).toContain("memory");
      expect(initialMemoryBytes(r.wasm)).toBe(e.profile === "badge" ? BADGE_MEMORY_BYTES : 65536);
    });
  }

  it("loads examples as copies", () => {
    const a = exampleWorkspace("blink");
    a?.workspace.blocks?.blocks?.pop();
    expect(exampleWorkspace("blink")?.workspace.blocks?.blocks).toHaveLength(2);
    expect(exampleWorkspace("nope")).toBeUndefined();
  });
});

describe("badge memory: 1-byte pages", () => {
  it("rewrites the memory section to min = max = 16384 bytes with page size 2^0", async () => {
    const r = await compileAssemblyScript("export function init(): void {}");
    if (!r.ok) throw new Error(r.errors);
    expect(initialMemoryBytes(r.wasm)).toBe(65536);
    const bp = toBytePages(r.wasm, 16384);
    if (!bp.ok) throw new Error(bp.error);
    expect(initialMemoryBytes(bp.wasm)).toBe(16384);
    // flags 0x09 (max + page size), 16384 = LEB 0x80 0x80 0x01 twice, log2 0.
    const at = [...bp.wasm].findIndex((b, i) => b === 5 && bp.wasm[i + 2] === 1 && bp.wasm[i + 3] === 9);
    expect([...bp.wasm.subarray(at, at + 11)]).toEqual([5, 9, 1, 9, 0x80, 0x80, 0x01, 0x80, 0x80, 0x01, 0]);
  });

  it("refuses static data beyond the memory and modules without one memory", async () => {
    const big = await compileWat('(module (memory (export "memory") 1 1) (data (i32.const 16380) "abcdefgh"))');
    if (!big.ok) throw new Error(big.errors);
    expect(toBytePages(big.wasm, 16384)).toEqual({ ok: false, error: "static data ends at byte 16388, beyond the 16384-byte memory" });
    expect(toBytePages(big.wasm, 16388).ok).toBe(true);
    const none = await compileWat("(module)");
    if (!none.ok) throw new Error(none.errors);
    expect(toBytePages(none.wasm, 16384)).toEqual({ ok: false, error: "the module has no memory section" });
    const imported = await compileWat('(module (import "mw" "m" (memory 1)))');
    if (!imported.ok) throw new Error(imported.errors);
    expect(toBytePages(imported.wasm, 16384)).toEqual({ ok: false, error: "the module imports its memory" });
  });

  it("detects memory.size and memory.grow, which assume 64 KiB pages", async () => {
    const grows = await compileAssemblyScript("export function init(): void { memory.grow(1); }");
    if (!grows.ok) throw new Error(grows.errors);
    expect(await usesMemorySize(grows.wasm)).toBe(true);
    const plain = await compileAssemblyScript("export function init(): void {}");
    if (!plain.ok) throw new Error(plain.errors);
    expect(await usesMemorySize(plain.wasm)).toBe(false);
  });
});

describe.runIf(proto.hasSimulator)("Blocks payloads in the simulator", () => {
  it("every 500 ms toggle pin toggles GPIO", async () => {
    const w = ws(
      { type: "mw_on_init", inputs: { DO: { block: { type: "mw_gpio_output", fields: { PIN: 8 } } } } },
      { type: "mw_every", fields: { MS: 500 }, inputs: { DO: { block: { type: "mw_gpio_toggle", fields: { PIN: 8 } } } } },
    );
    const { s, clock, events, errors } = run((await build(w, "esp-base")).wasm);
    expect(s.pins.get(8)).toEqual({ mode: "output", high: false });
    clock.advance(1600);
    expect(errors).toEqual([]);
    const writes = events.flatMap((e) => (e.kind === "write" && e.pin === 8 ? [e.high] : []));
    expect(writes).toEqual([true, false, true]);
    expect(s.pins.get(8)).toEqual({ mode: "output", high: true });
    s.dispose();
  });

  it("on message -> publish echoes text and numbers", async () => {
    const w = ws(
      {
        type: "mw_on_message",
        fields: { KEY: "app/echo/in" },
        inputs: {
          DO: {
            block: {
              type: "mw_publish",
              fields: { KEY: "app/echo/out" },
              inputs: { DATA: { block: { type: "mw_message_text" } } },
              next: {
                block: {
                  type: "mw_publish",
                  fields: { KEY: "app/echo/double" },
                  inputs: {
                    DATA: {
                      block: {
                        type: "math_arithmetic",
                        fields: { OP: "MULTIPLY" },
                        inputs: { A: { block: { type: "mw_message_number" } }, B: num(2) },
                      },
                    },
                  },
                },
              },
            },
          },
        },
      },
    );
    const { s, events, errors } = run((await build(w, "esp-base")).wasm);
    expect(s.subscriptions).toEqual(["app/echo/in"]);
    expect(s.deliver("app/echo/in", new TextEncoder().encode("-21"))).toBe(true);
    expect(s.deliver("app/echo/in", new TextEncoder().encode("ping"))).toBe(true);
    expect(errors).toEqual([]);
    expect(published(events, "app/echo/out").map(text)).toEqual(["-21", "ping"]);
    expect(published(events, "app/echo/double").map(text)).toEqual(["-42", "0"]);
    s.dispose();
  });

  it("the echo example echoes", async () => {
    const e = exampleWorkspace("echo");
    if (!e) throw new Error("no echo example");
    const { s, events } = run((await build(e.workspace, e.profile)).wasm);
    s.deliver("app/echo/in", new TextEncoder().encode("hello"));
    expect(published(events, "app/echo/out").map(text)).toEqual(["hello"]);
    expect(events.some((x) => x.kind === "log" && x.line === "echo: message")).toBe(true);
    s.dispose();
  });

  it("on pin edge runs on the configured edge and timers fire on_irq", async () => {
    const w = ws(
      {
        type: "mw_on_pin_edge",
        fields: { PIN: 4, PULL: "1", EDGE: "2" },
        inputs: {
          DO: {
            block: {
              type: "mw_publish",
              fields: { KEY: "app/edge" },
              inputs: { DATA: { block: { type: "mw_pin_level" } } },
            },
          },
        },
      },
      { type: "mw_on_init", inputs: { DO: { block: { type: "mw_timer_start", fields: { ID: 1, MS: 50, MODE: "1" } } } } },
      {
        type: "mw_on_timer",
        fields: { ID: 1 },
        inputs: { DO: { block: { type: "mw_log", inputs: { TEXT: { shadow: { type: "text", fields: { TEXT: "alarm" } } } } } } },
      },
    );
    const { s, clock, events, errors } = run((await build(w, "esp-base")).wasm);
    expect(s.pins.get(4)).toEqual({ mode: "input", pull: "up", high: true });
    clock.advance(10);
    s.setInput(4, false);
    clock.advance(10);
    s.setInput(4, true);
    clock.advance(200);
    expect(errors).toEqual([]);
    expect(published(events, "app/edge").map(text)).toEqual(["0"]);
    expect(events.filter((e) => e.kind === "log" && e.line === "alarm").length).toBeGreaterThanOrEqual(4);
    s.dispose();
  });

  it("the badge profile publishes on badge/leds from a byte-paged module", async () => {
    const e = exampleWorkspace("chase");
    if (!e) throw new Error("no chase example");
    const r = await build(e.workspace, "badge");
    expect(r.memoryBytes).toBe(BADGE_MEMORY_BYTES);
    expect(initialMemoryBytes(r.wasm)).toBe(BADGE_MEMORY_BYTES);
    const { s, clock, events, errors } = run(r.wasm);
    expect(published(events, "badge/text").map(text)).toEqual(["LED chase"]);
    expect(published(events, "badge/pet").map((d) => [...d])).toEqual([[4]]);
    clock.advance(300);
    expect(errors).toEqual([]);
    const frames = published(events, "badge/leds");
    expect(frames).toHaveLength(3);
    for (const f of frames) expect(f.length).toBe(24);
    // The lit LED moves one place per frame.
    frames.forEach((f, i) => {
      const lit = [...Array(8).keys()].filter((led) => f[led * 3] !== 0);
      expect(lit).toEqual([i]);
      expect([...f.subarray(i * 3, i * 3 + 3)]).toEqual([255, 0, 96]);
    });
    expect(s.subscriptions).toEqual(["badge/buttons"]);
    s.deliver("badge/buttons", Uint8Array.of(3, 0));
    s.deliver("badge/buttons", Uint8Array.of(3, 1));
    expect(published(events, "badge/text").map(text)).toEqual(["LED chase", "button 3"]);
    s.dispose();
  });

  it("show text cuts badge text to 256 bytes on a character boundary", async () => {
    // 401 bytes; byte 256 is the second byte of an "é".
    const long = `a${"é".repeat(200)}`;
    const w = ws({
      type: "mw_on_init",
      inputs: { DO: { block: { type: "badge_show_text", inputs: { TEXT: { block: { type: "text", fields: { TEXT: long } } } } } } },
    });
    const { s, events } = run((await build(w, "badge")).wasm);
    const shown = published(events, "badge/text");
    expect(shown.map((d) => d.length)).toEqual([255]);
    expect(text(shown[0] ?? new Uint8Array())).toBe(`a${"é".repeat(127)}`);
    s.dispose();
  });
});
