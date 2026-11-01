// V6 (browser part): an ABI 5 payload drives an LED strip in the
// protocol-wasm simulator. The WAT fixture is shared with the
// protocol-wasm Rust tests.

import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";
import { checkAbi } from "../src/editor/abiCheck";
import { compileWat } from "../src/editor/compile";
import type { SimEvent } from "../src/protocol/wasm";
import { SimSession, ledColours } from "../src/sim/session";
import { ManualClock } from "./clock";
import { protocol } from "./fakeDevice";

const proto = protocol();

async function fixture(): Promise<Uint8Array> {
  const r = await compileWat(readFileSync(new URL("./fixtures/ledstrip.wat", import.meta.url), "utf8"));
  if (!r.ok) throw new Error(r.errors);
  return r.wasm;
}

const i32 = (events: SimEvent[], key: string) => {
  const d = events.flatMap((e) => (e.kind === "publish" && e.key === key ? [e.data] : [])).at(-1);
  return d ? new DataView(d.buffer, d.byteOffset, d.byteLength).getInt32(0, true) : undefined;
};

describe("LED strips", () => {
  it("ledColours turns G, R, B wire bytes into #rrggbb per whole LED", () => {
    expect(ledColours(new Uint8Array([0, 0xff, 0, 0xff, 0, 0, 0, 0, 0x80, 1]))).toEqual(["#ff0000", "#00ff00", "#000080"]);
    expect(ledColours(new Uint8Array())).toEqual([]);
  });

  it("abiCheck accepts the strip imports and does not treat them as event sources", async () => {
    const report = checkAbi(await fixture(), proto.abi);
    expect(report.problems).toEqual([]);
    expect(report.warnings).toEqual([]);
    expect(report.imports).toContain("mw.ledstrip_write");
  });

  it.runIf(proto.hasSimulator)("records frames, holds the pin and closes", async () => {
    const events: SimEvent[] = [];
    const pins: number[][] = [];
    const s = new SimSession(
      proto,
      await fixture(),
      { onEvent: (e) => events.push(e), onPins: (p) => pins.push([...p.keys()]) },
      new ManualClock(),
    );
    expect(events.filter((e) => e.kind.startsWith("ledstrip"))).toEqual([
      { kind: "ledstripOpen", handle: 0, pin: 5, count: 3 },
      { kind: "ledstrip", handle: 0, pin: 5, grb: new Uint8Array([0, 0xff, 0, 0xff, 0, 0, 0, 0, 0xff]) },
    ]);
    // gpio_write on the strip's pin and a frame one LED too long.
    expect(i32(events, "app/gpio")).toBe(proto.abi.errors.InvalidArgument);
    expect(i32(events, "app/long")).toBe(proto.abi.errors.InvalidArgument);
    expect(pins.at(-1)).toEqual([]);
    expect(s.pins.has(5)).toBe(false);

    expect(s.deliver("app/in", new Uint8Array([0x10, 0x20, 0x30]))).toBe(true);
    const [strip] = s.ledstrips();
    expect({ handle: strip.handle, pin: strip.pin, count: strip.count }).toEqual({ handle: 0, pin: 5, count: 3 });
    expect(ledColours(strip.grb)).toEqual(["#201030", "#00ff00", "#0000ff"]);

    expect(s.deliver("app/off", new Uint8Array())).toBe(true);
    expect(events.at(-1)).toEqual({ kind: "ledstripClose", handle: 0, pin: 5 });
    expect(s.ledstrips()).toEqual([]);
    s.dispose();
  });
});
