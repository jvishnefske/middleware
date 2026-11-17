// SC6 (browser part): the protocol-wasm simulator offers a fake IMU and a
// fake microphone as `route_source` kinds 2 and 3. The sensor_stream.wat
// fixture is shared with the payload-sim and protocol-wasm Rust tests
// (`sc6_*`); the batches decode with the SC1 format (src/protocol/samples.ts).

import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";
import { compileWat } from "../src/editor/compile";
import {
  SampleBatchError,
  decodeSampleBatch,
  lostBetween,
  sampleLevels,
  sampleValues,
} from "../src/protocol/samples";
import type { SimEvent } from "../src/protocol/wasm";
import { protocol } from "./fakeDevice";

async function fixture(name: string): Promise<Uint8Array> {
  const r = await compileWat(readFileSync(new URL(`./fixtures/${name}.wat`, import.meta.url), "utf8"));
  if (!r.ok) throw new Error(r.errors);
  return r.wasm;
}

const on = (events: SimEvent[], key: string) =>
  events.flatMap((e) => (e.kind === "publish" && e.key === key ? [e.data] : []));

describe("sample batch decoder", () => {
  it("refuses truncated, unknown-version and short batches", () => {
    const header = new Uint8Array(32);
    header.set([1, 0, 0, 3]);
    const v = new DataView(header.buffer);
    v.setUint32(4, 100, true);
    v.setUint16(24, 1, true);
    v.setUint16(26, 2, true);
    v.setUint32(28, 61, true);
    const ok = new Uint8Array([...header, 1, 0, 2, 0, 3, 0]);
    expect(Array.from(sampleValues(decodeSampleBatch(ok)))).toEqual([1, 2, 3]);
    expect(() => decodeSampleBatch(ok.subarray(0, 31))).toThrow(SampleBatchError);
    expect(() => decodeSampleBatch(ok.subarray(0, 37))).toThrow(/count past/);
    const v2 = ok.slice();
    v2[0] = 2;
    expect(() => decodeSampleBatch(v2)).toThrow(/version/);
    const mic = ok.slice();
    mic[2] = 1; // µ-law on the IMU
    expect(() => decodeSampleBatch(mic)).toThrow(/does not fit/);
  });
});

const proto = protocol();

describe.runIf(proto.hasSimulator)("SC6 simulator: fake IMU and microphone", () => {
  it("streams both, decodes the batches, and stops the IMU route", async () => {
    const events: SimEvent[] = [];
    const sim = proto.newSimulator(await fixture("sensor_stream"), (e) => events.push(e));
    for (let ms = 10; ms <= 1000; ms += 10) sim.step(ms * 1000);
    const started = on(events, "app/started")[0];
    expect(Array.from(new Int32Array(started.slice().buffer))).toEqual([0, 1]);
    const imu = on(events, "app/imu").map(decodeSampleBatch);
    expect(imu).toHaveLength(3);
    expect(imu[0].header).toMatchObject({ kind: "imu", channels: 3, count: 5, rateHz: 50, range: 2 });
    expect(lostBetween(imu[0].header, imu[1].header)).toBe(0n);
    // Flat on the bench at index 0: +1 g (16384 LSB at ±2 g) on Z.
    expect(Array.from(sampleValues(imu[0]).subarray(0, 3))).toEqual([0, 0, 16_384]);
    const mic = on(events, "app/mic").map(decodeSampleBatch);
    expect(mic).toHaveLength(10);
    expect(mic[9].header).toMatchObject({ kind: "microphone", format: "levels", scale: 400, firstIndex: 18n });
    for (const { rms, peak } of sampleLevels(mic[9])) {
      expect(rms).toBeGreaterThan(10_000);
      expect(peak).toBeGreaterThan(16_000);
    }
    sim.free();
  });
});
