// ABI 8 (MC, CM) in the browser: machine channel kinds, driver binding
// targets, the manifest's control-module record and sys/manifest/cm pages.

import { describe, expect, it } from "vitest";
import { describeTarget } from "../src/device/binding";
import type { ChannelDecl } from "../src/protocol/wasm";
import { protocol } from "./fakeDevice";

const proto = protocol();
const SHA = new Uint8Array(32).fill(3);

/** One control-module point on the wire (loader::control). */
function point(name: string, kind: number, flow: number, unit: number, min: number, max: number, scale: number): number[] {
  const b = new DataView(new ArrayBuffer(12));
  b.setInt32(0, min, true);
  b.setInt32(4, max, true);
  b.setUint32(8, scale, true);
  return [name.length, ...new TextEncoder().encode(name), kind, flow, unit, ...new Uint8Array(b.buffer)];
}

describe("ABI 8 machine channels", () => {
  it("declares machine kinds in a section and reads them back", () => {
    const decls: ChannelDecl[] = (["digital", "position", "setpoint", "segments"] as const).map((kind, i) => ({
      name: `m${i}`,
      dir: kind === "setpoint" ? "out" : "in",
      kind,
      sensor: null,
      formats: 0,
      channels: 0,
      rateMin: 0,
      rateMax: 0xffffffff,
      required: true,
    }));
    const section = proto.encodeChannels(decls);
    expect(section[2 + 1 + 2 + 1]).toBe(3); // m0's kind: digital
    expect(() => proto.encodeChannels([{ ...decls[0], kind: "torque" as never }])).toThrow();
  });

  it("encodes and decodes driver targets", () => {
    const record = proto.encodeBinding({
      imageSha256: SHA,
      version: 4,
      entries: [{ name: "x_limit", target: { kind: "driver", point: "x_limit_min" } }],
    });
    const back = proto.decodeBinding(record)!;
    expect(back.entries[0].target).toEqual({ kind: "driver", point: "x_limit_min" });
    expect(describeTarget(back.entries[0].target)).toBe("driver point x_limit_min");
    expect(() =>
      proto.encodeBinding({ imageSha256: SHA, version: 4, entries: [{ name: "x", target: { kind: "driver", point: "X-Pos" } }] }),
    ).toThrow();
  });

  it("reads the manifest's control modules and a page of points", () => {
    const base = proto.simulatorManifest("0011223344556677")!;
    const m = proto.decodeManifest(new Uint8Array([...base, 12, 2, 2, 1]))!;
    expect(m.controlModules).toEqual({ points: 2, pages: 1 });
    const page = new Uint8Array([
      1, 1, 0, 2,
      ...point("z_pos", 4, 0, 3, -230000, 4500, 1000),
      ...point("motion", 6, 2, 0, 0, 32, 0),
    ]);
    expect(proto.decodeControlPage(page)).toEqual({
      pages: 1,
      page: 0,
      points: [
        { name: "z_pos", kind: "position", flow: "in", unit: "count", min: -230000, max: 4500, scale: 1000 },
        { name: "motion", kind: "segments", flow: "both", unit: "none", min: 0, max: 32, scale: 0 },
      ],
    });
    expect(proto.decodeControlPage(page.slice(0, page.length - 1))).toBeUndefined();
    expect(Array.from(proto.controlPageRequest(3))).toEqual([3]);
  });
});
