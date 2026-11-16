// X15 (browser part): ABI 3 PWM and ADC payloads run in the protocol-wasm
// simulator. The pwm_adc.wat fixture is shared with the protocol-wasm Rust
// tests (`x15_*`).

import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";
import { checkAbi } from "../src/editor/abiCheck";
import { compileWat } from "../src/editor/compile";
import type { SimEvent } from "../src/protocol/wasm";
import { protocol } from "./fakeDevice";

const proto = protocol();

async function fixture(name: string): Promise<Uint8Array> {
  const r = await compileWat(readFileSync(new URL(`./fixtures/${name}.wat`, import.meta.url), "utf8"));
  if (!r.ok) throw new Error(r.errors);
  return r.wasm;
}

const lastI32 = (events: SimEvent[], key: string) => {
  const d = events.flatMap((e) => (e.kind === "publish" && e.key === key ? [e.data] : [])).at(-1);
  return d ? new DataView(d.buffer, d.byteOffset, d.byteLength).getInt32(0, true) : undefined;
};

describe("ABI 3 constants and checks", () => {
  it("abi() lists the PWM and ADC imports and abiCheck accepts the fixture", async () => {
    const imports = new Map(proto.abi.imports.map((i) => [i.name, i]));
    expect(imports.get("pwm_open")).toEqual({ name: "pwm_open", params: 2, returns: true });
    expect(imports.get("pwm_set")).toEqual({ name: "pwm_set", params: 3, returns: true });
    expect(imports.get("pwm_stop")).toEqual({ name: "pwm_stop", params: 1, returns: true });
    expect(imports.get("adc_read")).toEqual({ name: "adc_read", params: 1, returns: true });
    expect(proto.abi.version).toBeGreaterThanOrEqual(3);
    const report = checkAbi(await fixture("pwm_adc"), proto.abi);
    expect(report.problems).toEqual([]);
  });
});

describe.runIf(proto.hasSimulator)("ABI 3 simulator: PWM and ADC", () => {
  it("reads set ADC values, drives PWM, and reports Busy after stop", async () => {
    const events: SimEvent[] = [];
    const sim = proto.newSimulator(await fixture("pwm_adc"), (e) => events.push(e));
    expect(events).toEqual([{ kind: "pwmOpen", timer: 0, freqHz: 1000 }]);
    expect(sim.pwm()).toEqual([{ timer: 0, freqHz: 1000, duty: [0, 0, 0, 0] }]);

    sim.setAdc(4, 1000);
    sim.tick(10);
    expect(lastI32(events, "app/adc")).toBe(1000);
    expect(events).toContainEqual({ kind: "pwmSet", timer: 0, channel: 1, duty: 16_000 });

    sim.setAdc(4, 4095);
    sim.tick(20);
    expect(lastI32(events, "app/adc")).toBe(4095);
    expect(sim.pwm()[0].duty).toEqual([0, 65_520, 0, 0]);

    sim.tick(30);
    expect(events).toContainEqual({ kind: "pwmStop", timer: 0 });
    expect(lastI32(events, "app/err")).toBe(proto.abi.errors.Busy);
    expect(sim.pwm()).toEqual([]);
    sim.free();
  });

  it("setAdc rejects values above 12 bits and pins beyond the board", async () => {
    const sim = proto.newSimulator(await fixture("pwm_adc"), () => {});
    expect(() => sim.setAdc(4, 4096)).toThrow(/above 4095/);
    expect(() => sim.setAdc(sim.pinCount, 1)).toThrow(/no pin/);
    sim.setAdc(sim.pinCount - 1, 4095);
    sim.free();
  });
});
