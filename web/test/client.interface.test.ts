// U40: DeviceClient is an interface; SerialClient (the serial protocol) and
// any RequestClient subclass get the same typed helpers over `request`.

import { afterEach, describe, expect, it } from "vitest";
import {
  type DeviceClient,
  NOT_FOUND_CODE,
  RequestClient,
  RequestError,
  type RequestOptions,
  SerialClient,
} from "../src/device/client";
import type { Protocol } from "../src/protocol/wasm";
import { FakeDevice, protocol, serialPair } from "./fakeDevice";

const proto = protocol();
const NODE = "0011223344556677";

/** A RequestClient over a table of answers; records every request. */
class TableClient extends RequestClient {
  readonly transport = "mesh" as const;
  readonly remote = true;
  readonly asked: { key: string; body: number[] }[] = [];
  constructor(
    proto: Protocol,
    private readonly answers: Record<string, Uint8Array | number>,
  ) {
    super(proto);
  }
  async request(key: string, body: Uint8Array = new Uint8Array(), _opts?: RequestOptions): Promise<Uint8Array> {
    this.asked.push({ key, body: [...body] });
    const a = this.answers[`${key}#${[...body].join(",")}`] ?? this.answers[key];
    if (a === undefined) throw new RequestError(key, "NotFound", NOT_FOUND_CODE);
    if (typeof a === "number") throw new RequestError(key, proto.statusName(a), a);
    return a;
  }
  async close(): Promise<void> {}
}

const open: { client: SerialClient; device: FakeDevice }[] = [];
afterEach(async () => {
  for (const r of open.splice(0)) {
    await r.client.close();
    await r.device.close();
  }
});

function serial(): SerialClient {
  const { host, device: devSide } = serialPair();
  const client = new SerialClient(host, proto, {});
  const device = new FakeDevice(proto, devSide, { manifest: proto.simulatorManifest(NODE) });
  client.start();
  open.push({ client, device });
  return client;
}

async function sameHelpers(c: DeviceClient) {
  const info = await c.info();
  expect(info.chip).toBeTruthy();
  const m = await c.manifest();
  expect(m?.node).toBe(NODE);
  expect(await c.manifestBody()).toEqual(proto.simulatorManifest(NODE));
}

describe("DeviceClient", () => {
  it("is what SerialClient and a RequestClient subclass both are", async () => {
    const s: DeviceClient = serial();
    expect([s.transport, s.remote]).toEqual(["serial", false]);
    await sameHelpers(s);
    const info = await s.info();
    const t: DeviceClient = new TableClient(proto, {
      [proto.constants.keys.info]: proto.encodeDeviceInfo(info),
      [proto.constants.keys.manifest]: proto.simulatorManifest(NODE)!,
    });
    expect([t.transport, t.remote]).toEqual(["mesh", true]);
    await sameHelpers(t);
  });

  it("reads a control page by number, null when the node has none", async () => {
    const page = new Uint8Array([9, 9]);
    const t = new TableClient(proto, { [`${proto.constants.keys.controlModules}#1`]: page });
    expect(proto.constants.keys.controlModules).toBe("sys/manifest/cm");
    expect(await t.controlPage(1)).toEqual(page);
    expect(t.asked.at(-1)).toEqual({ key: "sys/manifest/cm", body: [...proto.controlPageRequest(1)] });
    expect(await t.controlPage(0)).toBeNull();
    expect(await serial().controlPage(0)).toBeNull();
  });

  it("keeps NotFound as null and other errors as errors", async () => {
    const t = new TableClient(proto, { [proto.constants.keys.binding]: 6 });
    expect(await t.board()).toBeNull();
    expect(await t.peers()).toEqual([]);
    expect(await t.trustInfo()).toBeNull();
    await expect(t.binding()).rejects.toMatchObject({ statusCode: 6 });
  });
});
