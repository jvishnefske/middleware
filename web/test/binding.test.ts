// BD5/BD6 (browser part): channel declarations read from a payload,
// binding records encoded and checked against a node's manifest with
// one-click fixes, a binding installed on the (fake) board, and the
// simulator running a bound payload.

import { readFileSync } from "node:fs";
import { afterEach, describe, expect, it } from "vitest";
import { compileWat } from "../src/editor/compile";
import { DeviceClient } from "../src/device/client";
import { applyFix, bindingIssues, describeTarget, installBinding, setTarget, suggestBinding } from "../src/device/binding";
import type { ChannelDecl, SimEvent } from "../src/protocol/wasm";
import { FakeDevice, protocol, serialPair } from "./fakeDevice";

const proto = protocol();
const NODE = "0011223344556677";
const SHA = new Uint8Array(32).fill(7);

const DECLS: ChannelDecl[] = [
  { name: "audio", dir: "in", kind: "samples", sensor: "mic", formats: 1 << 2, channels: 0, rateMin: 8000, rateMax: 48000, required: true },
  { name: "level", dir: "out", kind: "bytes", sensor: null, formats: 0, channels: 0, rateMin: 0, rateMax: 0xffffffff, required: false },
];

async function payload(): Promise<Uint8Array> {
  const r = await compileWat(readFileSync(new URL("./fixtures/level_channels.wat", import.meta.url), "utf8"));
  if (!r.ok) throw new Error(r.errors);
  return proto.withChannels(r.wasm, proto.encodeChannels(DECLS));
}

const open: { client: DeviceClient; device: FakeDevice }[] = [];
afterEach(async () => {
  for (const r of open.splice(0)) {
    await r.client.close();
    await r.device.close();
  }
});

describe("channel bindings", () => {
  it("reads the declarations back and round-trips a binding", async () => {
    const wasm = await payload();
    expect(proto.readChannels(wasm)).toEqual(DECLS);
    const b = setTarget(
      setTarget({ imageSha256: SHA, version: 3, entries: [] }, "level", { kind: "key", key: "lab/level" }),
      "audio",
      { kind: "local", source: "mic", rateHz: 16000, gainDb: 20, mode: "levels", units: 10, block: 160 },
    );
    const bytes = proto.encodeBinding(b);
    expect(proto.decodeBinding(bytes)).toEqual(b);
    expect(proto.bindingMessage(bytes, NODE).length).toBe(14 + 8 + 32 + 4 + 32);
    expect(proto.encodeBindBody(bytes, new Uint8Array(64)).length).toBe(bytes.length + 68);
    expect(() => proto.encodeBinding(setTarget(b, "audio", { kind: "key", key: "sys/info" }))).toThrow();
    expect(describeTarget(b.entries[0].target)).toBe("microphone 16000 Hz levels, gain 20 dB");
  });

  it("checks a binding against the manifest and fixes it in one click", async () => {
    const wasm = await payload();
    const manifest = proto.simulatorManifest(NODE)!;
    const first = suggestBinding(proto, manifest, wasm, SHA);
    expect(first.entries.map((e) => e.name)).toEqual(["audio"]);
    expect(bindingIssues(proto, manifest, wasm, first)).toEqual([]);
    // A PCM source does not fit a levels channel: the fix restores levels.
    const pcm = setTarget(first, "audio", { kind: "local", source: "mic", rateHz: 16000, gainDb: 0, mode: "pcm", units: 160, block: 0 });
    const [issue] = bindingIssues(proto, manifest, wasm, pcm);
    expect(issue.channel).toBe("audio");
    expect(issue.message).toMatch(/does not take/);
    expect(bindingIssues(proto, manifest, wasm, applyFix(pcm, issue))).toEqual([]);
    const extra = setTarget(first, "lvl", { kind: "key", key: "x" });
    const [unknown] = bindingIssues(proto, manifest, wasm, extra);
    expect(unknown.fix).toEqual({ kind: "remove" });
    expect(applyFix(extra, unknown).entries.map((e) => e.name)).toEqual(["audio"]);
  });

  it("installs a binding on an unclaimed board and reads it back", async () => {
    const { host, device: devSide } = serialPair();
    const client = new DeviceClient(host, proto, {});
    const device = new FakeDevice(proto, devSide, {});
    client.start();
    open.push({ client, device });
    expect(await client.binding()).toBeNull();
    const record = setTarget({ imageSha256: SHA, version: 0, entries: [] }, "level", { kind: "key", key: "lab/level" });
    const r = await installBinding(client, proto, record, null, { now: () => 5_000_000 });
    expect(r.kind).toBe("unsigned");
    const stored = proto.decodeBinding((await client.binding())!)!;
    expect(stored.version).toBe(5000);
    expect(stored.entries).toEqual(record.entries);
  });

  it("runs a bound payload in the simulator: the microphone's levels reach the output key", async () => {
    const wasm = await payload();
    const record = setTarget(
      setTarget({ imageSha256: SHA, version: 1, entries: [] }, "level", { kind: "key", key: "lab/level" }),
      "audio",
      { kind: "local", source: "mic", rateHz: 16000, gainDb: 20, mode: "levels", units: 10, block: 160 },
    );
    const events: SimEvent[] = [];
    const sim = proto.newSimulator(wasm, (e) => events.push(e), NODE, proto.encodeBinding(record));
    for (let ms = 10; ms <= 300; ms += 10) sim.step(ms * 1000);
    const level = events.filter((e) => e.kind === "publish" && e.key === "lab/level");
    expect(level.length).toBeGreaterThanOrEqual(2);
    expect(() => proto.newSimulator(wasm, () => {}, NODE)).toThrow(/audio is required/);
  });
});

describe("remote channels through the board (PR4)", () => {
  it("lists the board's peers, reads a peer's manifest and flags an unheard node", async () => {
    const PEER = "00000000000000aa";
    const { host, device: devSide } = serialPair();
    const client = new DeviceClient(host, proto, {});
    const device = new FakeDevice(proto, devSide, { peers: [{ node: PEER, manifest: proto.simulatorManifest(PEER)! }] });
    client.start();
    open.push({ client, device });
    expect(await client.peers()).toEqual([PEER]);
    const m = proto.decodeManifest((await client.peerManifestBody(PEER))!)!;
    expect(m.node).toBe(PEER);
    const wasm = await payload();
    const manifest = proto.simulatorManifest(NODE)!;
    const remote = (node: string) =>
      setTarget({ imageSha256: SHA, version: 1, entries: [] }, "audio", { kind: "remote", node, key: "mic/level" });
    expect(bindingIssues(proto, manifest, wasm, remote(PEER), await client.peers())).toEqual([]);
    const [unheard] = bindingIssues(proto, manifest, wasm, remote("00000000000000bb"), [PEER]);
    expect(unheard.message).toMatch(/has not heard node 00000000000000bb/);
  });

  it("delivers a board's publications to a simulated unit bound to it, and only its", async () => {
    const { MixedNet } = await import("../src/sim/net");
    const { SimBus } = await import("../src/sim/bus");
    const wasm = await payload();
    const bus = new SimBus(proto);
    const net = new MixedNet(bus);
    const BOARD = "00000000000000aa";
    let publish: ((key: string, data: Uint8Array) => void) | null = null;
    net.attach("board", {
      connected: true,
      node: BOARD,
      request: async () => null,
      onPublish: (f) => {
        publish = f;
        return () => {};
      },
      onPresence: () => () => {},
    });
    const record = setTarget(
      setTarget({ imageSha256: SHA, version: 1, entries: [] }, "audio", { kind: "remote", node: BOARD, key: "mic/level" }),
      "level",
      { kind: "key", key: "lab/level" },
    );
    const heard: string[] = [];
    bus.onPublish((unit, key) => heard.push(`${unit} ${key}`));
    bus.add("unit", wasm, { binding: proto.encodeBinding(record) });
    publish!("mic/level", Uint8Array.of(1, 2, 3));
    expect(heard).toEqual(["unit lab/level"]);
  });
});
