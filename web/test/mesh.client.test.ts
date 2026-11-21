// U44: MeshTransport (the page's WebSocket and timer around a MeshSession),
// discovery, and MeshClient, which only probes: it reads sys/info, the
// manifest, its control pages and trust/info, and refuses everything that
// would change the node without sending a query.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { DeniedError, ProbeOnlyError, RequestError, RequestTimeout } from "../src/device/client";
import {
  MESH_PROBE_KEYS,
  MeshClient,
  MeshDownError,
  MeshTransport,
  type SocketLike,
  defaultMeshUrl,
  scan,
  siteOfMeshUrl,
} from "../src/device/mesh";
import { installBinding } from "../src/device/binding";
import { trustedUpload, uploadPayload } from "../src/device/upload";
import type { MeshCompletion, MeshSessionLike } from "../src/protocol/wasm";
import { protocol } from "./fakeDevice";

const proto = protocol();
const keys = proto.constants.keys;
const NODE = "0011223344556677";
const SENDER = "5e110de02a9c0102";

type Sent = { id: number; sender?: string; key: string; body: number[]; timeoutMs: number; max?: number };

/** A scripted MeshSession: records commands; completions come out at the
 * next poll once `finish` queued them. */
class FakeSession implements MeshSessionLike {
  readonly senderId = "00000000000000aa";
  readonly sent: Sent[] = [];
  readonly fed: number[][] = [];
  readonly polls: number[] = [];
  output: Uint8Array[] = [];
  wake: number | undefined = undefined;
  linkClosed = false;
  down = false;
  private next = 1;
  private done: MeshCompletion[] = [];
  feed(msg: Uint8Array) {
    this.fed.push([...msg]);
  }
  closed() {
    this.linkClosed = true;
  }
  takeOutput() {
    const o = this.output;
    this.output = [];
    return o;
  }
  open() {
    return this.cmd({ key: "<open>", body: [], timeoutMs: 0 });
  }
  close() {
    return this.cmd({ key: "<close>", body: [], timeoutMs: 0 });
  }
  request(sender: string | undefined, key: string, body: Uint8Array, timeoutMs: number) {
    return this.cmd({ sender, key, body: [...body], timeoutMs });
  }
  discover(key: string, body: Uint8Array, timeoutMs: number, max: number) {
    return this.cmd({ key, body: [...body], timeoutMs, max });
  }
  poll(now: number) {
    this.polls.push(now);
    const d = this.done;
    this.done = [];
    return d;
  }
  nextWakeMs() {
    return this.wake;
  }
  isDown() {
    return this.down;
  }
  finish(c: MeshCompletion) {
    this.done.push(c);
  }
  private cmd(s: Omit<Sent, "id">) {
    const id = this.next++;
    this.sent.push({ id, ...s });
    return id;
  }
}

class FakeSocket implements SocketLike {
  static last: FakeSocket | null = null;
  binaryType = "blob";
  readonly sent: number[][] = [];
  closedByUs = false;
  onopen: ((ev: unknown) => void) | null = null;
  onmessage: ((ev: { data: unknown }) => void) | null = null;
  onclose: ((ev: unknown) => void) | null = null;
  onerror: ((ev: unknown) => void) | null = null;
  constructor(readonly url: string) {
    FakeSocket.last = this;
  }
  send(data: Uint8Array) {
    this.sent.push([...data]);
  }
  close() {
    this.closedByUs = true;
    this.onclose?.({});
  }
  /** The router side. */
  opened() {
    this.onopen?.({});
  }
  message(bytes: number[]) {
    this.onmessage?.({ data: new Uint8Array(bytes).buffer });
  }
  dropped() {
    this.onclose?.({});
  }
}

let session: FakeSession;
let now = 0;

/** Lets awaited promises run (connect submits the handshake after open). */
async function settle() {
  for (let i = 0; i < 5; i++) await Promise.resolve();
}

beforeEach(() => {
  vi.useFakeTimers();
  now = 0;
  session = new FakeSession();
});
afterEach(() => {
  vi.useRealTimers();
});

/** A connected transport on the fake socket and session. */
async function connected(): Promise<{ t: MeshTransport; sock: FakeSocket }> {
  const p = MeshTransport.connect("wss://updates.example.org/mesh", proto, {
    WebSocketCtor: FakeSocket,
    newSession: () => session,
    now: () => now,
  });
  const sock = FakeSocket.last!;
  expect(sock.url).toBe("wss://updates.example.org/mesh");
  sock.opened();
  await settle();
  expect(sock.binaryType).toBe("arraybuffer");
  const open = session.sent.find((s) => s.key === "<open>")!;
  session.finish({ id: open.id, kind: "opened", sender: session.senderId });
  sock.message([0x01]);
  return { t: await p, sock };
}

/** The node's answer to the last command, arriving as a ws message. */
function answer(sock: FakeSocket, c: Omit<MeshCompletion, "id"> & Record<string, unknown>) {
  const last = session.sent.at(-1)!;
  session.finish({ id: last.id, ...c } as MeshCompletion);
  sock.message([0x02]);
}

const info = (node = NODE) =>
  proto.encodeDeviceInfo({ abiVersion: 8, capacity: 65536, status: 1, chip: "linux-aarch64", firmware: "0.5.0", node });

describe("MeshTransport", () => {
  it("feeds messages, sends what the session wrote, and re-arms one timer", async () => {
    const { t, sock } = await connected();
    expect(session.fed).toEqual([[0x01]]);
    session.output = [new Uint8Array([9, 9])];
    session.wake = 2500;
    sock.message([0x03]);
    expect(sock.sent).toEqual([[9, 9]]);
    expect(vi.getTimerCount()).toBe(1);
    // The timer polls at its time, and only one is ever armed.
    now = 2500;
    session.wake = 5000;
    vi.advanceTimersByTime(2500);
    expect(session.polls.at(-1)).toBe(2500);
    expect(vi.getTimerCount()).toBe(1);
    expect(t.senderId).toBe(session.senderId);
  });

  it("maps completions to replies and the shared request errors", async () => {
    const { t, sock } = await connected();
    const ok = t.request(SENDER, keys.info, new Uint8Array([1]), 8000);
    expect(session.sent.at(-1)).toMatchObject({ sender: SENDER, key: keys.info, body: [1], timeoutMs: 8000 });
    answer(sock, { kind: "reply", body: new Uint8Array([7]) });
    expect(await ok).toEqual(new Uint8Array([7]));

    const denied = t.request(SENDER, keys.manifest, new Uint8Array(), 100);
    answer(sock, { kind: "error", failure: "handler", status: 5 });
    await expect(denied).rejects.toBeInstanceOf(DeniedError);

    const notFound = t.request(SENDER, keys.manifest, new Uint8Array(), 100);
    answer(sock, { kind: "error", failure: "handler", status: 1 });
    await expect(notFound).rejects.toMatchObject({ statusCode: 1, status: "NotFound" });
    await expect(notFound).rejects.toBeInstanceOf(RequestError);

    const silent = t.request(SENDER, keys.info, new Uint8Array(), 100);
    answer(sock, { kind: "error", failure: "noReply" });
    await expect(silent).rejects.toBeInstanceOf(RequestTimeout);
  });

  it("fails everything pending when the link drops, and tells the page", async () => {
    const { t, sock } = await connected();
    const closed = vi.fn();
    t.onClosed(closed);
    const a = t.request(SENDER, keys.info, new Uint8Array(), 8000);
    const b = t.discover(keys.info, new Uint8Array(), 3000);
    sock.dropped();
    expect(session.linkClosed).toBe(true);
    await expect(a).rejects.toBeInstanceOf(MeshDownError);
    await expect(b).rejects.toBeInstanceOf(MeshDownError);
    expect(closed).toHaveBeenCalledTimes(1);
    expect(t.up).toBe(false);
    await expect(t.request(SENDER, keys.info, new Uint8Array(), 10)).rejects.toBeInstanceOf(MeshDownError);
  });

  it("notices a session that died while idle: fails what waits, tells the page, feeds nothing more", async () => {
    const { t, sock } = await connected();
    const closed = vi.fn();
    t.onClosed(closed);
    const a = t.request(SENDER, keys.info, new Uint8Array(), 8000);
    // The lease runs out at the next timer: the session reports it is down.
    session.wake = 10_000;
    sock.message([0x04]);
    session.down = true;
    now = 10_000;
    vi.advanceTimersByTime(10_000);
    await expect(a).rejects.toBeInstanceOf(MeshDownError);
    expect(closed).toHaveBeenCalledTimes(1);
    expect(t.up).toBe(false);
    expect(sock.closedByUs).toBe(true);
    expect(vi.getTimerCount()).toBe(0);
    const fed = session.fed.length;
    sock.message([0x05]);
    expect(session.fed.length).toBe(fed);
  });

  it("refuses to connect when the handshake fails", async () => {
    const p = MeshTransport.connect("ws://127.0.0.1:8000", proto, {
      WebSocketCtor: FakeSocket,
      newSession: () => session,
      now: () => now,
    });
    const sock = FakeSocket.last!;
    sock.opened();
    await settle();
    session.finish({ id: 1, kind: "error", failure: "down" });
    sock.message([0]);
    await expect(p).rejects.toThrow(/handshake/);
    expect(sock.closedByUs).toBe(true);
  });
});

describe("scan", () => {
  it("asks every node for sys/info once and lists each sender once", async () => {
    const { t, sock } = await connected();
    const rows = scan(t, proto);
    expect(session.sent.at(-1)).toMatchObject({ key: keys.info, body: [], timeoutMs: 3000 });
    answer(sock, {
      kind: "replies",
      replies: [
        { sender: SENDER, body: info() },
        { sender: SENDER, body: info() },
        { sender: "00000000000000bb", status: 5 },
        { body: info("ffffffffffffffff") },
        { sender: "00000000000000cc", body: new Uint8Array([1]) },
        // A sender id that is not 16 lowercase hex digits is not listed.
        { sender: "$(id)", body: info() },
        { sender: "5E110DE02A9C0102", body: info() },
      ],
    });
    const got = await rows;
    expect(got.map((r) => [r.sender, r.info?.node ?? null, r.problem ?? null])).toEqual([
      [SENDER, NODE, null],
      ["00000000000000bb", null, "Denied"],
      ["00000000000000cc", null, "malformed sys/info"],
    ]);
  });
});

describe("MeshClient", () => {
  async function client() {
    const { t, sock } = await connected();
    const dev = { sender: SENDER, info: proto.decodeDeviceInfo(info())! };
    return { c: new MeshClient(t, dev, proto), sock };
  }

  it("asks its node, by sender id, with the mesh timeout", async () => {
    const { c, sock } = await client();
    expect([c.transport, c.remote]).toEqual(["mesh", true]);
    const p = c.info();
    expect(session.sent.at(-1)).toMatchObject({ sender: SENDER, key: keys.info, timeoutMs: 8000 });
    answer(sock, { kind: "reply", body: info() });
    expect((await p).node).toBe(NODE);
    const page = c.controlPage(2, { timeoutMs: 500 });
    expect(session.sent.at(-1)).toMatchObject({ key: keys.controlModules, body: [2], timeoutMs: 500 });
    answer(sock, { kind: "error", failure: "handler", status: 1 });
    expect(await page).toBeNull();
    const trust = c.trustInfo();
    expect(session.sent.at(-1)).toMatchObject({ key: "sys/trust/info" });
    answer(sock, { kind: "error", failure: "handler", status: 1 });
    expect(await trust).toBeNull();
  });

  it("only ever sends the keys the cloud lets a browser ask", () => {
    expect([...MESH_PROBE_KEYS].sort()).toEqual(
      ["sys/info", "sys/manifest", "sys/manifest/cm", "sys/trust/info"].sort(),
    );
  });

  it("answers board, peers and binding without asking", async () => {
    const { c } = await client();
    const before = session.sent.length;
    expect(await c.board()).toBeNull();
    expect(await c.peers()).toEqual([]);
    expect(await c.binding()).toBeNull();
    expect(await c.peerManifestBody(NODE)).toBeNull();
    expect(session.sent.length).toBe(before);
  });

  it("refuses every change at once, sending nothing", async () => {
    const { c } = await client();
    const before = session.sent.length;
    const refusals = [
      c.bind(new Uint8Array([1])),
      c.startPayload(),
      c.stopPayload(),
      c.trustClaim(new Uint8Array(32)),
      c.request(keys.begin, new Uint8Array([1])),
      c.request("app/anything"),
      uploadPayload(c, proto, new Uint8Array(64)),
      trustedUpload(c, proto, new Uint8Array(64), null),
      installBinding(c, proto, { version: 1, imageSha256: new Uint8Array(32), entries: [] }, null),
    ];
    for (const r of refusals) {
      await expect(r).rejects.toBeInstanceOf(ProbeOnlyError);
      await expect(r).rejects.toThrow(/probe-only: deploy with mwctl/);
    }
    expect(session.sent.length).toBe(before);
  });
});

describe("defaultMeshUrl", () => {
  it("is wss://updates.<site>/mesh for the page's site, unless overridden", () => {
    expect(defaultMeshUrl({ hostname: "lathe.example.org" })).toBe("wss://updates.example.org/mesh");
    expect(defaultMeshUrl({ hostname: "example.org" })).toBe("wss://updates.example.org/mesh");
    expect(defaultMeshUrl({ hostname: "a.b.example.org" })).toBe("wss://updates.b.example.org/mesh");
    expect(defaultMeshUrl({ hostname: "localhost" })).toBe("ws://localhost:8000");
    expect(defaultMeshUrl({ hostname: "127.0.0.1" })).toBe("ws://127.0.0.1:8000");
    expect(defaultMeshUrl({ hostname: "x.example.org", search: "?mesh=ws%3A%2F%2Fh%3A1" })).toBe("ws://h:1");
    expect(defaultMeshUrl({ hostname: "x.example.org", stored: "wss://other/mesh" })).toBe("wss://other/mesh");
  });
});

describe("siteOfMeshUrl", () => {
  it("is the site whose mesh.updates.<site> router mwctl connects to", () => {
    expect(siteOfMeshUrl("wss://updates.example.org/mesh")).toBe("example.org");
    expect(siteOfMeshUrl("wss://mesh.example.org/")).toBe("mesh.example.org");
    expect(siteOfMeshUrl("ws://127.0.0.1:8000")).toBeNull();
    expect(siteOfMeshUrl("not a url")).toBeNull();
    expect(siteOfMeshUrl("wss://UPDATES.Example.ORG/mesh")).toBe("example.org");
    // Only a strict DNS name is a site.
    for (const bad of [
      "ws://localhost:8000",
      "wss://updates/mesh",
      "wss://[::1]:8000/",
      "wss://xn--.-a.org/",
      "wss://-bad.org/",
      "wss://bad-.org/",
      `wss://${"a".repeat(64)}.org/`,
      "wss://a_b.org/",
    ]) {
      expect(siteOfMeshUrl(bad), bad).toBeNull();
    }
    expect(siteOfMeshUrl("wss://updates.example.org.")).toBeNull();
  });
});
