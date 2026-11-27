// Browser mesh (design.md BM): pages in one room find each other through
// a PeerJS-compatible server, link over data channels, and their payloads'
// channels connect by name. The server and the data channels are the
// in-memory ones of fakeMesh.ts; the frames, the gate and every rule are
// protocol-wasm's.

import { readFileSync } from "node:fs";
import { afterEach, describe, expect, it } from "vitest";
import { DeviceClient } from "../src/device/client";
import { SerialRequester } from "../src/device/requester";
import { compileWat } from "../src/editor/compile";
import { token } from "../src/mesh/env";
import { Mesh, type MeshOptions, pageHello, peerUnit } from "../src/mesh/mesh";
import { NODE_KEY, nodeId } from "../src/mesh/nodeId";
import { RtcLink } from "../src/mesh/rtc";
import { SignalSocket } from "../src/mesh/signal";
import type { ChannelDecl, DeviceInfo, FrameEvent, MeshServer } from "../src/protocol/wasm";
import { SimBus } from "../src/sim/bus";
import { MixedNet } from "../src/sim/net";
import { FakeDevice, protocol, serialPair } from "./fakeDevice";
import { World, settle } from "./fakeMesh";

const proto = protocol();
const ROOM = "lab";
const SERVER: MeshServer = { host: "signal.test", port: 9000, path: "/", key: "peerjs", secure: false };
const node = (n: number) => n.toString(16).padStart(16, "0");
const bytesDecl = (name: string, dir: "in" | "out", required = false): ChannelDecl => ({
  name,
  dir,
  kind: "bytes",
  sensor: null,
  formats: 0,
  channels: 0,
  rateMin: 0,
  rateMax: 0xffffffff,
  required,
});

const WAT = readFileSync(new URL("./fixtures/level_channels.wat", import.meta.url), "utf8");

/** The fixture: republishes input `input` on output `output` (five
 * letters each, as the fixture's `audio` and `level`). */
async function repeater(input: string, output: string): Promise<Uint8Array> {
  const src = WAT.replace('(data (i32.const 32) "audio")', `(data (i32.const 32) "${input}")`).replace('(data (i32.const 48) "level")', `(data (i32.const 48) "${output}")`);
  const r = await compileWat(src);
  if (!r.ok) throw new Error(r.errors);
  return proto.withChannels(r.wasm, proto.encodeChannels([bytesDecl(input, "in", true), bytesDecl(output, "out")]));
}

interface Page {
  mesh: Mesh;
  bus: SimBus;
  net: MixedNet;
  /** What the bus's units published: "unit key bytes". */
  published: string[];
  /** What the mesh took from peers: "node key bytes". */
  taken: string[];
}

const open: (() => void | Promise<void>)[] = [];
afterEach(async () => {
  for (const f of open.splice(0).reverse()) await f();
});

/** A page with node id `n` in the room, running `wasm` (if any) with its
 * unbound channels on their room keys. */
function page(world: World, n: number, wasm: Uint8Array | null = null, opts: Partial<MeshOptions> = {}): Page {
  const bus = new SimBus(proto);
  const net = new MixedNet(bus);
  const published: string[] = [];
  const taken: string[] = [];
  bus.onPublish((unit, key, data) => published.push(`${unit} ${key} ${[...data]}`));
  const mesh = new Mesh({ proto, room: ROOM, node: node(n), server: SERVER, env: world.env(n), hello: () => pageHello(proto, node(n), wasm), ...opts });
  mesh.onPublish((from, key, data) => taken.push(`${from} ${key} ${[...data]}`));
  mesh.bind(net);
  if (wasm) bus.add("unit", wasm, { node: node(n), binding: proto.mesh.binding(wasm, undefined, ROOM) });
  open.push(() => {
    mesh.leave();
    net.dispose();
    bus.dispose();
  });
  return { mesh, bus, net, published, taken };
}

const linked = (p: Page) =>
  p.mesh.peers
    .filter((v) => v.state === "open")
    .map((v) => v.node)
    .sort();

/** A peer driven by hand: it registers, answers the dial of a lower id
 * and the hello, and sends whatever frames the test gives it. */
async function rawPeer(world: World, n: number, says: { node?: string; silentUntil?: Promise<void>; onOpen?(link: RtcLink): void } = {}) {
  const env = world.env(n);
  const id = proto.mesh.peerId(ROOM, node(n));
  const socket = new SignalSocket(proto, env, SERVER, id, token(env));
  expect(await socket.opened).toBe("open");
  const got: FrameEvent[] = [];
  let link: RtcLink | null = null;
  const hello = pageHello(proto, says.node ?? node(n), null);
  socket.onSignal((s) => {
    if (s.type === "offer") {
      const l = new RtcLink(env, [], (m) => void socket.send(m), s.peer, s.connection, s.label);
      link = l;
      l.onOpen = () => says.onOpen?.(l);
      l.onMessage = async (data) => {
        if (typeof data === "string") return;
        const f = proto.decodeDatagram(data);
        if (!f) return;
        if (f.kind === "request") {
          await says.silentUntil;
          l.send(proto.mesh.helloReply(f.id, f.key ?? "", hello.info, hello.manifest, hello.channels));
        } else got.push(f);
      };
      void l.accept(s.sdp);
    } else link?.signal(s);
  });
  open.push(() => {
    link?.close();
    socket.close();
  });
  return {
    id,
    got,
    get link(): RtcLink {
      if (!link) throw new Error("not dialled");
      return link;
    },
  };
}

describe("browser mesh: channels by name (BM2, BM3)", () => {
  it("a payload's output reaches the same-named input in another browser, with the sender's node, and no binding made by hand", async () => {
    const world = new World();
    // Browser 1 turns `audio` into `level`; browser 2 reads `level`.
    const a = page(world, 1, await repeater("audio", "level"));
    const b = page(world, 2, await repeater("level", "heard"));
    await a.mesh.join();
    await b.mesh.join();
    expect(a.mesh.state).toBe("joined");
    await world.run(4000);
    expect(a.mesh.discoveryMode).toBe("listing");
    expect(linked(a)).toEqual([node(2)]);
    expect(linked(b)).toEqual([node(1)]);
    // The lower id dialled; nobody else offered.
    expect(world.relayed.filter((m) => m.type === "OFFER")).toEqual([{ type: "OFFER", src: a.mesh.id, dst: b.mesh.id }]);

    // In browser 2, two more units bound by hand to browser 1's node and
    // to some other node: the sender decides which of them hears it.
    const wasm = await repeater("level", "heard");
    const sha = new Uint8Array(32);
    const strict = (from: string, out: string) =>
      proto.mesh.binding(
        wasm,
        proto.encodeBinding({
          imageSha256: sha,
          version: 1,
          entries: [
            { name: "level", target: { kind: "remote", node: from, key: "room/lab/level" } },
            { name: "heard", target: { kind: "key", key: out } },
          ],
        }),
        ROOM,
      );
    b.bus.add("from1", wasm, { node: node(0x21), binding: strict(node(1), "t/from1") });
    b.bus.add("from9", wasm, { node: node(0x22), binding: strict(node(9), "t/from9") });

    // Something in browser 1 feeds `audio`.
    a.bus.inject("feeder", node(0x77), "room/lab/audio", Uint8Array.of(1, 2, 3));
    await settle();
    expect(a.published).toEqual(["unit room/lab/level 1,2,3"]);
    expect(b.taken).toEqual([`${node(1)} room/lab/level 1,2,3`]);
    expect(b.published.sort()).toEqual(["from1 t/from1 1,2,3", "unit room/lab/heard 1,2,3"]);
    expect(a.mesh.counters).toMatchObject({ sent: 1, received: 2, refused: 0 });
    expect(b.mesh.counters).toMatchObject({ sent: 2, received: 1, refused: 0 });

    // Each sees the other's node, link and channels.
    const [peer] = b.mesh.peers;
    expect(peer.info).toMatchObject({ chip: "browser", node: node(1) });
    expect(peer.manifest?.links.map((l) => l.kindName)).toEqual(["serial", "webrtc"]);
    expect(peer.channels.map((c) => `${c.name}:${c.dir}`)).toEqual(["audio:in", "level:out"]);
    expect(peer.pairs).toEqual([{ name: "level", state: "connected", message: "level comes from this peer" }]);
    expect(a.mesh.peers[0].pairs).toEqual([{ name: "level", state: "connected", message: "level goes to this peer" }]);
    expect(b.net.node(peerUnit(node(1)))).toBe(node(1));

    // Browser 1 leaves: browser 2 notices and the unit is gone.
    a.mesh.leave();
    await settle();
    expect(linked(b)).toEqual([]);
    expect(b.net.boards).toEqual([]);
    await world.run(4000);
    expect(b.mesh.listed).toEqual([]);
  });

  it("a second tab with the same node id is told so", async () => {
    const world = new World();
    const a = page(world, 1);
    const again = page(world, 1);
    await a.mesh.join();
    await again.mesh.join();
    expect(again.mesh.state).toBe("failed");
    expect(again.mesh.error).toMatch(/already in room lab/);
  });

  it("the simulator bus keeps node-internal keys to the unit that published them", async () => {
    const bus = new SimBus(proto);
    open.push(() => bus.dispose());
    const heard: string[] = [];
    bus.onPublish((unit, key, data) => heard.push(`${unit} ${key} ${[...data]}`));
    // A unit whose `audio` is its own microphone listens on `ch/audio`.
    const wasm = await repeater("audio", "level");
    const record = proto.encodeBinding({
      imageSha256: new Uint8Array(32),
      version: 1,
      entries: [
        { name: "audio", target: { kind: "local", source: "mic", rateHz: 16000, gainDb: 20, mode: "levels", units: 10, block: 160 } },
        { name: "level", target: { kind: "key", key: "lab/level" } },
      ],
    });
    const unit = bus.add("unit", wasm, { binding: record });
    expect(unit.subscriptions).toEqual(["ch/audio"]);
    // Another node's `ch/audio` is not this unit's microphone.
    bus.inject("other", node(9), "ch/audio", Uint8Array.of(1, 2, 3));
    bus.inject("other", node(9), "drv/spindle", Uint8Array.of(1));
    expect(heard).toEqual([]);
    // Delivered by hand it would have been taken: the bus is what kept it out.
    unit.deliver("ch/audio", Uint8Array.of(1, 2, 3));
    bus.flush();
    expect(heard).toEqual(["unit lab/level 1,2,3"]);
    expect(proto.isInternalKey("ch/audio")).toBe(true);
    expect(proto.isInternalKey("drv/spindle")).toBe(true);
    expect(proto.isInternalKey("channel/x")).toBe(false);
    expect(proto.mesh.outbound("room/lab/level")).toBe(true);
    for (const key of ["sys/info", "ch/audio", "drv/spindle"]) expect(proto.mesh.outbound(key)).toBe(false);
  });
});

describe("browser mesh: finding the room (BM8)", () => {
  it("a server that lists nothing: the first page is the hub, a newcomer learns the room, and a member takes over when the hub leaves", async () => {
    const world = new World();
    world.allowDiscovery = false;
    const a = page(world, 1);
    const b = page(world, 2);
    const c = page(world, 3);
    await a.mesh.join();
    await world.run(100);
    expect(a.mesh.discoveryMode).toBe("hub");
    expect(world.clients.has("mw-lab-hub")).toBe(true);
    await b.mesh.join();
    await c.mesh.join();
    await world.run(100);
    expect([b.mesh.discoveryMode, c.mesh.discoveryMode]).toEqual(["member", "member"]);
    expect(linked(a)).toEqual([node(2), node(3)]);
    expect(linked(b)).toEqual([node(1), node(3)]);
    expect(linked(c)).toEqual([node(1), node(2)]);
    // Listing was tried first, once per page, on the server's own URL.
    expect(world.fetched).toEqual(Array(3).fill("http://signal.test:9000/peerjs/peers"));

    // The hub leaves: one of the two members is the next hub.
    a.mesh.leave();
    await world.run(3000);
    expect([b.mesh.discoveryMode, c.mesh.discoveryMode].sort()).toEqual(["hub", "member"]);
    expect(linked(b)).toEqual([node(3)]);
    expect(b.mesh.listed).toEqual([c.mesh.id]);
    expect(c.mesh.listed).toEqual([b.mesh.id]);

    // A newcomer finds both through the new hub, though it dials neither
    // (its id is the highest).
    const d = page(world, 4);
    await d.mesh.join();
    await world.run(200);
    expect(d.mesh.discoveryMode).toBe("member");
    expect(linked(d)).toEqual([node(2), node(3)]);
    expect(world.relayed.filter((m) => m.type === "OFFER" && m.src === d.mesh.id).map((m) => m.dst)).toEqual(["mw-lab-hub"]);
  });

  it("a hub lists a member only under the id its offer came from", async () => {
    const world = new World();
    world.allowDiscovery = false;
    const a = page(world, 1);
    await a.mesh.join();
    await world.run(100);
    // A page that says it is someone else.
    const env = world.env(50);
    const liar = new SignalSocket(proto, env, SERVER, proto.mesh.peerId(ROOM, node(5)), token(env));
    expect(await liar.opened).toBe("open");
    const link = new RtcLink(env, [], (m) => void liar.send(m), "mw-lab-hub", "hub_x", "mw-hub");
    const lists: string[][] = [];
    liar.onSignal((s) => link.signal(s));
    link.onOpen = () => link.send(proto.mesh.hubJoin(proto.mesh.peerId(ROOM, node(6))));
    link.onMessage = (data) => {
      const m = typeof data === "string" ? proto.mesh.hubDecode(data) : undefined;
      if (m?.type === "peers") lists.push(m.ids);
    };
    open.push(() => {
      link.close();
      liar.close();
    });
    await link.dial();
    await world.run(100);
    expect(link.open).toBe(true);
    expect(lists).toEqual([]);
    expect(a.mesh.listed).toEqual([]);
    link.send(proto.mesh.hubJoin(proto.mesh.peerId(ROOM, node(5))));
    await world.run(100);
    expect(lists).toEqual([[a.mesh.id, proto.mesh.peerId(ROOM, node(5))]]);
  });

  it("an unreachable server is a failure to join, and a dropped socket is reconnected with the links kept", async () => {
    const world = new World();
    world.down = true;
    const a = page(world, 1);
    await a.mesh.join();
    expect(a.mesh.state).toBe("failed");
    expect(a.mesh.error).toMatch(/signalling server/);
    world.down = false;
    const b = page(world, 2);
    await a.mesh.join();
    await b.mesh.join();
    await world.run(4000);
    expect(linked(a)).toEqual([node(2)]);
    const beats = world.heartbeats;
    await world.run(10_000);
    expect(world.heartbeats).toBeGreaterThanOrEqual(beats + 4);
    world.drop(a.mesh.id);
    await settle();
    expect(a.mesh.state).toBe("reconnecting");
    expect(linked(a)).toEqual([node(2)]);
    await world.run(5000);
    expect(a.mesh.state).toBe("joined");
    expect(linked(a)).toEqual([node(2)]);
    expect(a.mesh.publish("room/lab/x", Uint8Array.of(9))).toBe(true);
    await settle();
    expect(b.taken).toEqual([`${node(1)} room/lab/x 9`]);
  });
});

describe("browser mesh: the gate (BM3, BM4, BM6)", () => {
  it("refuses and counts what a peer may not send, and none of it reaches a board; the board's publications reach the room", async () => {
    const world = new World();
    const a = page(world, 1, await repeater("audio", "level"));
    // A board on the page's serial port.
    const { host, device: devSide } = serialPair();
    const BOARD = node(0xb0);
    let requester: SerialRequester | null = null;
    const client = new DeviceClient(host, proto, { onPublish: (key, payload) => requester?.publication(key, payload) });
    const device = new FakeDevice(proto, devSide, {});
    client.start();
    open.push(async () => {
      await client.close();
      await device.close();
    });
    requester = new SerialRequester(client, { node: BOARD, serves: true } as DeviceInfo);
    a.net.attach("board", requester);
    a.mesh.addBoard(requester);
    await a.mesh.join();

    // A peer that publishes before it has said who it is.
    let answer = () => {};
    const silentUntil = new Promise<void>((resolve) => {
      answer = resolve;
    });
    const early = proto.mesh.publish("room/lab/audio", Uint8Array.of(7))!;
    const peer = await rawPeer(world, 2, { silentUntil, onOpen: (l) => l.send(early) });
    await world.run(4000);
    expect(a.mesh.peers.map((p) => p.state)).toEqual(["hello"]);
    expect(a.mesh.counters).toMatchObject({ received: 0, refused: 1 });
    answer();
    await settle();
    expect(linked(a)).toEqual([node(2)]);

    const send = (frame: Parameters<typeof proto.encodeDatagram>[0]) => peer.link.send(proto.encodeDatagram(frame));
    const refused = () => a.mesh.counters.refused;
    const before = refused();
    // System and node-internal keys, too long, not a frame, not binary.
    send({ kind: "publish", key: "sys/payload/start", payload: new Uint8Array() });
    send({ kind: "publish", key: "sys/loader/begin", payload: Uint8Array.of(1) });
    send({ kind: "publish", key: "ch/audio", payload: Uint8Array.of(1) });
    send({ kind: "publish", key: "drv/spindle", payload: Uint8Array.of(1) });
    send({ kind: "publish", key: "room/lab/audio", payload: new Uint8Array(proto.mesh.constants.maxMessage + 1) });
    peer.link.send(Uint8Array.of(3, 0, 1, 2, 3));
    peer.link.send("sys/payload/start");
    // A response nobody asked for.
    send({ kind: "response", id: 200, statusCode: 0, payload: Uint8Array.of(1) });
    await settle();
    expect(refused() - before).toBe(8);
    // Requests: answered NotFound and counted, whatever they ask for.
    for (const [i, key] of ["sys/payload/start", "sys/payload/stop", "sys/loader/begin", "sys/board", "room/lab/audio"].entries()) send({ kind: "request", id: i, key, payload: Uint8Array.of(1) });
    await settle();
    expect(refused() - before).toBe(13);
    expect(peer.got.map((f) => `${f.kind} ${f.id} ${f.status}`)).toEqual([0, 1, 2, 3, 4].map((i) => `response ${i} NotFound`));
    peer.got.length = 0;
    expect(a.mesh.counters.received).toBe(0);
    expect(a.published).toEqual([]);

    // What it may send is delivered, as the node it said it is.
    send({ kind: "publish", key: "room/lab/audio", payload: Uint8Array.of(4, 5) });
    send({ kind: "publish", key: "room/lab/audio", payload: new Uint8Array(proto.mesh.constants.maxMessage) });
    await settle();
    expect(a.mesh.counters.received).toBe(2);
    expect(a.taken[0]).toBe(`${node(2)} room/lab/audio 4,5`);
    expect(a.published[0]).toBe("unit room/lab/level 4,5");
    expect(a.mesh.peers[0]).toMatchObject({ received: 2, refused: 14 });
    // The room got the payload's answer (twice), and nothing else.
    expect(peer.got.map((f) => `${f.kind} ${f.key} ${f.payload.length}`)).toEqual(["publish room/lab/level 2", `publish room/lab/level ${proto.mesh.constants.maxMessage}`]);
    peer.got.length = 0;

    // Nothing of all that was written to the board.
    await settle();
    expect(device.received).toEqual([]);
    expect(device.published).toEqual([]);

    // The board's publications go out (not its system ones); the local
    // units hear them from the board's node.
    await device.publish("mic/level", Uint8Array.of(42));
    await device.publish("sys/log", new TextEncoder().encode("boot"));
    await device.publish("sys/payload/status", Uint8Array.of(2));
    await new Promise((resolve) => setTimeout(resolve, 20));
    await settle();
    expect(peer.got.map((f) => `${f.kind} ${f.key} ${[...f.payload]}`)).toEqual(["publish mic/level 42"]);
    expect(a.mesh.counters.kept).toBe(2);
    expect(device.received).toEqual([]);
    expect(device.published).toEqual([]);
  });

  it("a peer whose sys/info names another node than its id is dropped", async () => {
    const world = new World();
    const a = page(world, 1);
    await a.mesh.join();
    const peer = await rawPeer(world, 2, { node: node(3) });
    await world.run(4000);
    expect(a.mesh.peers.filter((p) => p.state === "open")).toEqual([]);
    expect(a.mesh.counters.refused).toBeGreaterThanOrEqual(1);
    expect(peer.link.open).toBe(false);
    expect(a.net.boards).toEqual([]);
  });

  it("an offer from the wrong side or from outside the room is not answered", async () => {
    const world = new World();
    const a = page(world, 5);
    await a.mesh.join();
    const offer = async (id: string) => {
      const env = world.env(id.length);
      const socket = new SignalSocket(proto, env, SERVER, id, token(env));
      expect(await socket.opened).toBe("open");
      const link = new RtcLink(env, [], (m) => void socket.send(m), a.mesh.id, "dc_x", "mw");
      open.push(() => {
        link.close();
        socket.close();
      });
      await link.dial();
      await world.run(100);
      return link.open;
    };
    // A higher id must wait to be dialled; a stranger is not of the room.
    expect(await offer(proto.mesh.peerId(ROOM, node(9)))).toBe(false);
    expect(await offer("stranger")).toBe(false);
    expect(await offer(proto.mesh.peerId("other", node(1)))).toBe(false);
    expect(world.relayed.filter((m) => m.type === "ANSWER")).toEqual([]);
    expect(a.mesh.counters.refused).toBe(3);
  });
});

describe("browser mesh: the size of a node's mesh (BM9)", () => {
  it("a node links sixteen peers; the seventeenth is listed, not dialled", async () => {
    const world = new World();
    const pages = Array.from({ length: 18 }, (_, i) => page(world, i + 1));
    for (const p of pages) await p.mesh.join();
    await world.run(8000);
    const [first] = pages;
    const last = pages[17]!;
    expect(first.mesh.listed.length).toBe(17);
    expect(first.mesh.peers.length).toBe(proto.mesh.constants.maxPeers);
    expect(linked(first)).toEqual(pages.slice(1, 17).map((p) => p.mesh.node));
    expect(first.mesh.listed).toContain(last.mesh.id);
    expect(world.relayed.filter((m) => m.type === "OFFER" && m.src === first.mesh.id).map((m) => m.dst)).not.toContain(last.mesh.id);
    for (const p of pages) expect(p.mesh.peers.length).toBeLessThanOrEqual(16);
    // A publication reaches the sixteen.
    first.mesh.publish("room/lab/x", Uint8Array.of(1));
    await settle();
    expect(pages.filter((p) => p.taken.length === 1).length).toBe(16);
    expect(last.taken).toEqual([]);
  });
});

describe("browser mesh: this page's node id (BM5)", () => {
  it("is random, kept, and replaced when what is stored is not an id", () => {
    const store = new Map<string, string>();
    const storage = { getItem: (k: string) => store.get(k) ?? null, setItem: (k: string, v: string) => void store.set(k, v) };
    let n = 0;
    const fill = (b: Uint8Array) => b.fill(++n);
    const id = nodeId(storage, fill);
    expect(id).toBe("0101010101010101");
    expect(store.get(NODE_KEY)).toBe(id);
    expect(nodeId(storage, fill)).toBe(id);
    for (const bad of ["", "xyz", "0000000000000000", "0101010101010101ff", "0101010101010A01"]) {
      store.set(NODE_KEY, bad);
      const fresh = nodeId(storage, fill);
      expect(fresh).toMatch(/^[0-9a-f]{16}$/);
      expect(store.get(NODE_KEY)).toBe(fresh);
    }
    // All zero means "no sender": never an id.
    expect(nodeId(null, (b) => b.fill(0))).toBe("0000000000000001");
    // No storage, or one that throws: an id all the same.
    const broken = {
      getItem: () => {
        throw new Error("blocked");
      },
      setItem: () => {
        throw new Error("blocked");
      },
    };
    expect(nodeId(broken, fill)).toMatch(/^[0-9a-f]{16}$/);
    expect(proto.mesh.peerId(ROOM, id)).toBe("mw-lab-0101010101010101");
    expect(proto.mesh.parsePeerId(ROOM, "mw-lab-0101010101010101")).toBe(id);
    expect(() => proto.mesh.peerId("No Room", id)).toThrow();
  });

  it("the room's key and URLs come from the Rust side", () => {
    expect(proto.mesh.roomKey("lab", "level")).toBe("room/lab/level");
    expect(proto.mesh.roomKey("Lab", "level")).toBeUndefined();
    expect(proto.mesh.validRoom("lab_2")).toBe(true);
    expect(proto.mesh.socketUrl({ host: "0.peerjs.com", port: 443, path: "/", key: "peerjs", secure: true }, "mw-lab-hub", "t1")).toBe("wss://0.peerjs.com:443/peerjs?key=peerjs&id=mw-lab-hub&token=t1");
    expect(() => proto.mesh.peersUrl({ host: "a/b", port: 1, path: "/", key: "k", secure: true })).toThrow();
    const m = proto.decodeManifest(proto.simulatorManifest(node(1), true)!)!;
    expect(m.links).toEqual([
      { id: 0, kind: 0, kindName: "serial" },
      { id: 1, kind: 4, kindName: "webrtc" },
    ]);
    expect(proto.decodeManifest(proto.simulatorManifest(node(1))!)!.links.length).toBe(1);
  });
});
