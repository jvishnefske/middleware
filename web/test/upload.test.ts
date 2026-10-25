// U2/U3/U7: DeviceClient and the upload flow over a mocked serial port,
// against a scripted fake device that runs the Rust loader state machine.

import { afterEach, describe, expect, it } from "vitest";
import { DeviceClient, RequestError, RequestTimeout } from "../src/device/client";
import { UploadError, firmwareRunsPayloads, uploadPayload } from "../src/device/upload";
import { FakeDevice, type FakeDeviceScript, protocol, serialPair } from "./fakeDevice";

const proto = protocol();
const image = Uint8Array.from({ length: 1500 }, (_, i) => (i * 31 + 7) & 0xff);

interface Rig {
  client: DeviceClient;
  device: FakeDevice;
  texts: string[];
  pubs: { key: string; payload: Uint8Array }[];
}

let rigs: Rig[] = [];

function rig(script: FakeDeviceScript = {}): Rig {
  const { host, device: devSide } = serialPair();
  const texts: string[] = [];
  const pubs: { key: string; payload: Uint8Array }[] = [];
  const client = new DeviceClient(host, proto, {
    onText: (t) => texts.push(t),
    onPublish: (key, payload) => pubs.push({ key, payload }),
  });
  const device = new FakeDevice(proto, devSide, script);
  client.start();
  const r = { client, device, texts, pubs };
  rigs.push(r);
  return r;
}

afterEach(async () => {
  for (const r of rigs) {
    await r.client.close();
    await r.device.close();
  }
  rigs = [];
});

const offsetOf = (body: Uint8Array) => new DataView(body.buffer, body.byteOffset).getUint32(0, true);

describe("device client", () => {
  it("reads sys/info after a boot banner and shows the banner as console text", async () => {
    const { client, texts } = rig({ banner: "ESP-ROM:esp32c3-api1-20210207\r\n" });
    const info = await client.info();
    expect(info).toMatchObject({ chip: "esp32c3", firmware: "0.1.0-fake", capacity: 65536, statusName: "empty" });
    expect(texts.join("")).toContain("ESP-ROM:esp32c3");
  });

  it("delivers device publications and sends host publications", async () => {
    const { client, device, pubs } = rig();
    await device.publish("sys/log", new TextEncoder().encode("hello"));
    await client.publish("app/led", new Uint8Array([1]));
    await client.info(); // round trip: everything before it has been processed
    expect(pubs).toEqual([{ key: "sys/log", payload: new TextEncoder().encode("hello") }]);
    expect(device.published).toEqual([{ key: "app/led", payload: new Uint8Array([1]) }]);
  });

  it("times out after the retry and reports the attempts", async () => {
    const { client, device } = rig({ drop: () => true });
    await expect(client.request("sys/info", new Uint8Array(), { timeoutMs: 30, retries: 1 })).rejects.toBeInstanceOf(
      RequestTimeout,
    );
    expect(device.received).toHaveLength(2);
    expect(device.received[0].id).not.toBe(device.received[1].id);
  });

  it("does not retry error responses", async () => {
    const { client, device } = rig();
    const err = await client.request("sys/nope").catch((e) => e);
    expect(err).toBeInstanceOf(RequestError);
    expect(err.status).toBe("NotFound");
    expect(device.received).toHaveLength(1);
  });

  it("releases both stream locks on close", async () => {
    const { host, device: devSide } = serialPair();
    const device = new FakeDevice(proto, devSide);
    const client = new DeviceClient(host, proto);
    client.start();
    await client.info();
    await client.close();
    expect(host.readable.locked).toBe(false);
    expect(host.writable.locked).toBe(false);
    await device.close();
  });
});

describe("upload flow", () => {
  it("sends begin, in-order chunks and commit; the device verifies the CRC and activates", async () => {
    const { client, device, pubs } = rig();
    const progress: number[] = [];
    await uploadPayload(client, proto, image, { onProgress: (d) => progress.push(d), start: true });

    const keys = device.received.map((r) => r.key);
    expect(keys).toEqual([
      "sys/loader/begin",
      "sys/loader/chunk",
      "sys/loader/chunk",
      "sys/loader/chunk",
      "sys/loader/commit",
      "sys/payload/start",
    ]);
    const chunks = device.received.filter((r) => r.key === "sys/loader/chunk");
    expect(chunks.map((c) => offsetOf(c.body))).toEqual([0, 512, 1024]);
    expect(chunks.every((c) => c.body.length - 4 <= proto.constants.chunkMax)).toBe(true);
    const begin = new DataView(device.received[0].body.buffer, device.received[0].body.byteOffset);
    expect(begin.getUint32(0, true)).toBe(image.length);
    expect(begin.getUint32(4, true)).toBe(proto.crc32(image));

    expect(Uint8Array.from(device.image)).toEqual(image);
    expect(device.activated).toEqual({ len: image.length, crc: proto.crc32(image) });
    expect(device.started).toBe(true);
    expect(progress).toEqual([0, 0, 512, 1024, 1500, 1500]);
    await client.info();
    expect(pubs.filter((p) => p.key === "sys/payload/status").map((p) => p.payload[0])).toEqual([1, 2]);
  });

  it("retries a chunk whose request was lost on the line", async () => {
    let dropped = false;
    const { client, device } = rig({
      drop: (req) => {
        if (!dropped && req.key === "sys/loader/chunk" && offsetOf(req.body) === 512) {
          dropped = true;
          return true;
        }
        return false;
      },
    });
    await uploadPayload(client, proto, image, { timeoutMs: 50 });
    const chunkOffsets = device.received.filter((r) => r.key === "sys/loader/chunk").map((r) => offsetOf(r.body));
    expect(chunkOffsets).toEqual([0, 512, 512, 1024]);
    expect(Uint8Array.from(device.image)).toEqual(image);
    expect(device.activated?.crc).toBe(proto.crc32(image));
  });

  it("recovers when a chunk's response was lost and the chunk is resent", async () => {
    let dropped = false;
    const { client, device } = rig({
      dropResponse: (req) => {
        if (!dropped && req.key === "sys/loader/chunk" && offsetOf(req.body) === 512) {
          dropped = true;
          return true;
        }
        return false;
      },
    });
    await uploadPayload(client, proto, image, { timeoutMs: 50 });
    const chunkOffsets = device.received.filter((r) => r.key === "sys/loader/chunk").map((r) => offsetOf(r.body));
    expect(chunkOffsets).toEqual([0, 512, 512, 1024]);
    expect(Uint8Array.from(device.image)).toEqual(image);
    expect(device.activated?.crc).toBe(proto.crc32(image));
  });

  it("waits longer for begin while the device erases flash", async () => {
    const { client, device } = rig({ delayFor: (r) => (r.key === "sys/loader/begin" ? 300 : undefined) });
    await uploadPayload(client, proto, image, { timeoutMs: 50 });
    expect(device.received.filter((r) => r.key === "sys/loader/begin")).toHaveLength(1);
    expect(device.activated?.crc).toBe(proto.crc32(image));
  });

  it("fails with the step and status when the device refuses a step", async () => {
    const { client, device } = rig({ forceStatus: (r) => (r.key === "sys/loader/commit" ? 2 : undefined) });
    const err = await uploadPayload(client, proto, image).catch((e) => e);
    expect(err).toBeInstanceOf(UploadError);
    expect(err.key).toBe("sys/loader/commit");
    expect(err.step).toBe(4);
    expect(err.status).toBe("InvalidRequest");
    expect(device.activated).toBeNull();
  });

  it("fails when the image exceeds the device capacity", async () => {
    const { client } = rig({ capacity: 1000 });
    const err = await uploadPayload(client, proto, image).catch((e) => e);
    expect(err).toBeInstanceOf(UploadError);
    expect(err.key).toBe("sys/loader/begin");
    expect(err.status).toBe("BufferTooSmall");
  });

  it("gives up after one retry when the device stays silent", async () => {
    const { client, device } = rig({ drop: (r) => r.key === "sys/loader/chunk" });
    const err = await uploadPayload(client, proto, image, { timeoutMs: 30 }).catch((e) => e);
    expect(err).toBeInstanceOf(UploadError);
    expect(err.cause).toBeInstanceOf(RequestTimeout);
    expect(device.received.filter((r) => r.key === "sys/loader/chunk")).toHaveLength(2);
  });

  it("a corrupted chunk makes the device's CRC check fail at commit", async () => {
    // Corrupt the bytes in flight by wrapping the plan: flip one image byte
    // after the begin request (which carries the CRC of the original).
    const { client, device } = rig();
    const corruptProto = {
      ...proto,
      uploadPlan: (img: Uint8Array, chunk?: number) => {
        const plan = proto.uploadPlan(img, chunk);
        plan[2].body[10] ^= 0xff;
        return plan;
      },
    };
    const err = await uploadPayload(client, corruptProto, image).catch((e) => e);
    expect(err).toBeInstanceOf(UploadError);
    expect(err.key).toBe("sys/loader/commit");
    expect(err.status).toBe("InvalidRequest");
    expect(device.activated).toBeNull();
  });
});

describe("firmware capabilities", () => {
  it("recognises protocol-only firmware by its +no-wasm version suffix", () => {
    expect(firmwareRunsPayloads("0.1.0")).toBe(true);
    expect(firmwareRunsPayloads("0.1.0+no-wasm")).toBe(false);
    expect(firmwareRunsPayloads("0.2.0-rc1+no-wasm")).toBe(false);
    expect(firmwareRunsPayloads("0.1.0+no-wasm-later")).toBe(true);
  });
});
