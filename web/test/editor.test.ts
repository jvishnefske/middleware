// U5/U6: compile the payload templates in-process (wabt, asc) and run them
// in the protocol-wasm simulator (wasm-host compiled to WASM).

import { describe, expect, it } from "vitest";
import { checkAbi } from "../src/editor/abiCheck";
import { compileAssemblyScript, compileWat } from "../src/editor/compile";
import { template, templateFrom } from "../src/editor/templates";
import type { SimEvent } from "../src/protocol/wasm";
import { SimSession } from "../src/sim/session";
import { ManualClock } from "./clock";
import { protocol } from "./fakeDevice";

const proto = protocol();

async function wasmOf(lang: "wat" | "as", name: "blink" | "echo"): Promise<Uint8Array> {
  const src = template(name, lang).source;
  const r = lang === "wat" ? await compileWat(src) : await compileAssemblyScript(src);
  if (!r.ok) throw new Error(r.errors);
  return r.wasm;
}

describe("templates", () => {
  it("fall back to a built-in template when a file is missing", () => {
    expect(templateFrom({}, "echo", "wat").builtin).toBe(true);
    expect(templateFrom({ "x/templates/echo.wat": "(module)" }, "echo", "wat")).toEqual({
      source: "(module)",
      builtin: false,
    });
  });

  it("built-in fallbacks compile and pass the ABI check", async () => {
    for (const lang of ["wat", "as"] as const) {
      const src = templateFrom({}, "blink", lang).source;
      const r = lang === "wat" ? await compileWat(src) : await compileAssemblyScript(src);
      expect(r.ok, r.ok ? "" : r.errors).toBe(true);
      if (r.ok) expect(checkAbi(r.wasm, proto.abi).problems).toEqual([]);
    }
  });
});

describe("compilers", () => {
  it("report WAT errors with a location", async () => {
    const r = await compileWat("(module (func (export \"init\") (call $nope)))");
    expect(r.ok).toBe(false);
    if (!r.ok) expect(r.errors).toMatch(/payload\.wat:\d+/);
  });

  it("report AssemblyScript errors", async () => {
    const r = await compileAssemblyScript("export function init(): void { let x: i32 = 'no'; }");
    expect(r.ok).toBe(false);
    if (!r.ok) expect(r.errors).toMatch(/TS\d+|ERROR/);
  });

  it("flag modules that import outside the ABI", async () => {
    const r = await compileWat('(module (import "env" "abort" (func)) (memory (export "memory") 1))');
    expect(r.ok).toBe(true);
    if (r.ok) expect(checkAbi(r.wasm, proto.abi).problems).toEqual(["unsupported import env.abort (function)"]);
  });

  it("compile AssemblyScript without an env.abort import", async () => {
    const wasm = await wasmOf("as", "echo");
    const report = checkAbi(wasm, proto.abi);
    expect(report.problems).toEqual([]);
    expect(report.imports.every((i) => i.startsWith("mw."))).toBe(true);
  });
});

describe.runIf(proto.hasSimulator)("simulator", () => {
  for (const lang of ["wat", "as"] as const) {
    it(`runs the ${lang} blink template with a virtual LED`, async () => {
      const clock = new ManualClock();
      const events: SimEvent[] = [];
      const s = new SimSession(proto, await wasmOf(lang, "blink"), { onEvent: (e) => events.push(e) }, clock);
      s.start();
      clock.advance(1600);
      s.dispose();
      const writes = events.filter((e) => e.kind === "write");
      expect(writes.length).toBe(3);
      expect(writes.map((e) => e.kind === "write" && e.high)).toEqual([true, false, true]);
      expect(events.some((e) => e.kind === "log")).toBe(true);
      const pub = events.filter((e) => e.kind === "publish");
      expect(pub.map((e) => e.kind === "publish" && new TextDecoder().decode(e.data))).toEqual(["1", "0", "1"]);
    });

    it(`runs the ${lang} echo template with the message log`, async () => {
      const events: SimEvent[] = [];
      const s = new SimSession(proto, await wasmOf(lang, "echo"), { onEvent: (e) => events.push(e) }, new ManualClock());
      expect(s.subscriptions).toEqual(["app/echo/in"]);
      expect(s.deliver("app/echo/in", new TextEncoder().encode("ping"))).toBe(true);
      expect(s.deliver("app/other", new Uint8Array([1]))).toBe(false);
      s.dispose();
      const pub = events.find((e) => e.kind === "publish");
      expect(pub).toMatchObject({ kind: "publish", key: "app/echo/out" });
      expect(pub?.kind === "publish" && new TextDecoder().decode(pub.data)).toBe("ping");
    });
  }

  it("drives virtual input pins and reports traps", async () => {
    const wat = `(module
      (import "mw" "gpio_input" (func $in (param i32 i32) (result i32)))
      (import "mw" "gpio_read" (func $read (param i32) (result i32)))
      (import "mw" "gpio_output" (func $out (param i32) (result i32)))
      (import "mw" "gpio_write" (func $write (param i32 i32) (result i32)))
      (import "mw" "set_tick" (func $tick (param i32) (result i32)))
      (memory (export "memory") 1 1)
      (func (export "init") (drop (call $in (i32.const 4) (i32.const 1))) (drop (call $out (i32.const 8)))
        (drop (call $tick (i32.const 10))))
      (func (export "tick") (param $now i32)
        (drop (call $write (i32.const 8) (call $read (i32.const 4))))
        (if (i32.gt_u (local.get $now) (i32.const 100)) (then (loop $l (br $l))))))`;
    const r = await compileWat(wat);
    if (!r.ok) throw new Error(r.errors);
    const clock = new ManualClock();
    const errors: string[] = [];
    const s = new SimSession(proto, r.wasm, { onError: (m) => errors.push(m) }, clock);
    expect(s.pins.get(4)).toEqual({ mode: "input", pull: "up", high: true });
    s.start();
    clock.advance(20);
    expect(s.pins.get(8)).toEqual({ mode: "output", high: true });
    expect(s.setInput(4, false)).toBe(true);
    clock.advance(20);
    expect(s.pins.get(8)).toEqual({ mode: "output", high: false });
    clock.advance(100);
    expect(errors).toHaveLength(1);
    expect(errors[0]).toMatch(/fuel/);
    expect(s.running).toBe(false);
    s.dispose();
  });

  it("rejects modules that break the ABI at load", async () => {
    const r = await compileWat('(module (import "env" "abort" (func)) (memory (export "memory") 1))');
    if (!r.ok) throw new Error(r.errors);
    expect(() => new SimSession(proto, r.wasm)).toThrow(/env\.abort/);
  });
});
