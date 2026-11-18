// MF4: the node manifest in the browser: decodeManifest, DeviceClient.manifest()
// over the fake device, and the Device page's capability rows.

import { afterEach, describe, expect, it } from "vitest";
import { DeviceClient } from "../src/device/client";
import { capabilityRows } from "../src/device/manifest";
import { FakeDevice, protocol, serialPair } from "./fakeDevice";

const proto = protocol();
const NODE = "0011223344556677";
const open: { client: DeviceClient; device: FakeDevice }[] = [];

function rig(manifest?: Uint8Array) {
  const { host, device: devSide } = serialPair();
  const client = new DeviceClient(host, proto, {});
  const device = new FakeDevice(proto, devSide, manifest ? { manifest } : {});
  client.start();
  open.push({ client, device });
  return client;
}

afterEach(async () => {
  for (const r of open.splice(0)) {
    await r.client.close();
    await r.device.close();
  }
});

describe("node manifest", () => {
  it("decodes the simulator's manifest with its sources, rates and node", () => {
    const body = proto.simulatorManifest(NODE)!;
    const m = proto.decodeManifest(body)!;
    expect(m.node).toBe(NODE);
    expect(m.groupNames).toEqual(["gpio", "timers", "irq", "spi", "uart", "dma", "pwm", "adc", "routes", "serve"]);
    expect(m.imu).toEqual({ ratesHz: [50, 100, 200, 400, 800], rangesG: [2, 4, 8], magRatesHz: [50, 100, 200, 400, 800] });
    expect(m.mic).toEqual({ ratesHz: [8000, 16000, 32000, 48000], modes: ["levels", "mulaw", "pcm"], maxGainDb: 30 });
    expect(m.links).toEqual([{ id: 0, kind: 0, kindName: "serial" }]);
    expect(m.capNames).toEqual(["serve", "source_imu", "source_microphone"]);
    expect(m.i2c).toBeUndefined();
    expect(proto.decodeManifest(body.slice(0, 2))).toBeUndefined();
    expect(proto.simulatorManifest("nothex")).toBeUndefined();
  });

  it("reads sys/manifest, or null from firmware without one", async () => {
    const m = await rig(proto.simulatorManifest(NODE)).manifest();
    expect(m?.mic?.ratesHz).toEqual([8000, 16000, 32000, 48000]);
    expect(await rig().manifest()).toBeNull();
  });

  it("lists capabilities, the manifest's records and the board", () => {
    const info = proto.decodeDeviceInfo(
      proto.encodeDeviceInfo({ abiVersion: 6, capacity: 1, status: 1, chip: "rt1050", firmware: "0.1.0", node: NODE, caps: 13 }),
    )!;
    const m = proto.decodeManifest(proto.simulatorManifest(NODE)!)!;
    const rows = Object.fromEntries(capabilityRows(info, m, null));
    expect(rows["Capabilities"]).toBe("serve, source_imu, source_microphone");
    expect(rows["IMU"]).toBe("50, 100, 200, 400, 800 Hz, ±2/4/8 g; with magnetometer 50, 100, 200, 400, 800 Hz");
    expect(rows["Microphone"]).toBe("8000, 16000, 32000, 48000 Hz; levels, mulaw, pcm; gain 0–30 dB");
    expect(rows["Links"]).toBe("0 serial");
    expect(rows["Limits"]).toBe("4 routes, 8 subscriptions, 0 channels, 512-byte messages");
    expect(rows["Board"]).toBeUndefined();
    const old = Object.fromEntries(capabilityRows(info, null, null));
    expect(old["Manifest"]).toContain("not reported");
  });
});
