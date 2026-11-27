// An in-memory world for the browser mesh's tests: a PeerJS-compatible
// signalling server (the message relay, `ID-TAKEN`, `EXPIRE` for an
// unknown destination, the `/peers` listing with or without
// `allow_discovery`), RTCPeerConnections whose data channels are paired in
// memory once offer and answer have been exchanged, and one manual clock.
//
// `World.env(seed)` is a page's `MeshEnv`; `World.run(ms)` advances the
// clock, running due timers and letting every promise settle in between.

import type { DataChannelLike, IceCandidateLike, MeshEnv, PeerConnectionLike, SessionDescriptionLike, SocketLike } from "../src/mesh/env";

/** Lets queued microtasks and stream callbacks run. */
export function settle(): Promise<void> {
  return new Promise((resolve) => setImmediate(resolve));
}

class FakeSocket implements SocketLike {
  onopen: (() => void) | null = null;
  onmessage: ((e: { data: unknown }) => void) | null = null;
  onclose: (() => void) | null = null;
  onerror: (() => void) | null = null;
  readonly id: string;
  private live = true;
  private registered = false;

  constructor(
    private readonly world: World,
    readonly url: string,
  ) {
    const u = new URL(url);
    this.id = u.searchParams.get("id") ?? "";
    world.urls.push(url);
    queueMicrotask(() => {
      if (!this.live) return;
      if (world.down) {
        this.onerror?.();
        this.drop();
        return;
      }
      if (world.clients.has(this.id)) {
        this.deliver({ type: "ID-TAKEN", payload: { msg: "ID is taken" } });
        this.drop();
        return;
      }
      world.clients.set(this.id, this);
      this.registered = true;
      this.onopen?.();
      this.deliver({ type: "OPEN" });
    });
  }

  deliver(message: unknown): void {
    // A message sent before the socket closes still arrives.
    if (!this.live) return;
    const text = JSON.stringify(message);
    queueMicrotask(() => this.onmessage?.({ data: text }));
  }

  send(data: string): void {
    if (!this.live || !this.registered) throw new Error("socket is not open");
    const m = JSON.parse(data) as { type: string; dst?: string; payload?: unknown };
    if (m.type === "HEARTBEAT") {
      this.world.heartbeats++;
      return;
    }
    this.world.relayed.push({ type: m.type, src: this.id, dst: m.dst ?? "" });
    const to = m.dst ? this.world.clients.get(m.dst) : undefined;
    if (to) to.deliver({ type: m.type, src: this.id, dst: m.dst, payload: m.payload });
    else if (m.type !== "LEAVE" && m.type !== "EXPIRE") this.deliver({ type: "EXPIRE", src: m.dst, dst: this.id });
  }

  /** The server's side closes (or the page's: `close`). */
  drop(): void {
    if (!this.live) return;
    this.live = false;
    if (this.registered && this.world.clients.get(this.id) === this) this.world.clients.delete(this.id);
    queueMicrotask(() => this.onclose?.());
  }

  close(): void {
    this.drop();
  }
}

class FakeChannel implements DataChannelLike {
  binaryType = "blob";
  readyState = "connecting";
  onopen: (() => void) | null = null;
  onmessage: ((e: { data: unknown }) => void) | null = null;
  onclose: (() => void) | null = null;
  other: FakeChannel | null = null;

  constructor(readonly label: string) {}

  open(): void {
    if (this.readyState !== "connecting") return;
    this.readyState = "open";
    queueMicrotask(() => this.onopen?.());
  }

  send(data: string | Uint8Array<ArrayBuffer>): void {
    if (this.readyState !== "open") throw new Error("channel is not open");
    const other = this.other;
    // A copy, as an ArrayBuffer: what a browser hands the receiver.
    const copy = typeof data === "string" ? data : data.slice().buffer;
    queueMicrotask(() => {
      if (other?.readyState === "open") other.onmessage?.({ data: copy });
    });
  }

  close(): void {
    if (this.readyState === "closed") return;
    this.readyState = "closed";
    queueMicrotask(() => this.onclose?.());
    this.other?.close();
  }
}

class FakePc implements PeerConnectionLike {
  onicecandidate: ((e: { candidate: IceCandidateLike | null }) => void) | null = null;
  ondatachannel: ((e: { channel: DataChannelLike }) => void) | null = null;
  onconnectionstatechange: (() => void) | null = null;
  connectionState = "new";
  readonly tag: string;
  private channel: FakeChannel | null = null;
  private offerFrom: FakePc | null = null;
  candidates = 0;

  constructor(private readonly world: World) {
    this.tag = `pc${world.pcs.size + 1}`;
    world.pcs.set(this.tag, this);
  }

  createDataChannel(label: string): DataChannelLike {
    this.channel = new FakeChannel(label);
    return this.channel;
  }

  async createOffer(): Promise<{ sdp?: string }> {
    return { sdp: `v=0\r\na=fake-offer:${this.tag}\r\n` };
  }

  async createAnswer(): Promise<{ sdp?: string }> {
    return { sdp: `v=0\r\na=fake-answer:${this.tag}\r\n` };
  }

  async setLocalDescription(_d: SessionDescriptionLike): Promise<void> {
    queueMicrotask(() => {
      this.onicecandidate?.({ candidate: { candidate: `candidate:${this.tag} 1 udp 1 192.0.2.1 9 typ host`, sdpMid: "0", sdpMLineIndex: 0 } });
      this.onicecandidate?.({ candidate: null });
    });
  }

  async setRemoteDescription(d: SessionDescriptionLike): Promise<void> {
    const m = /a=fake-(offer|answer):(pc\d+)/.exec(d.sdp);
    const remote = m ? this.world.pcs.get(m[2]!) : undefined;
    if (!m || m[1] !== d.type || !remote) throw new Error("bad description");
    if (d.type === "offer") {
      this.offerFrom = remote;
      return;
    }
    // The answer reached the dialler: the two are connected.
    if (remote.offerFrom !== this || !this.channel || this.connectionState === "closed" || remote.connectionState === "closed") throw new Error("no such offer");
    const far = new FakeChannel(this.channel.label);
    far.other = this.channel;
    this.channel.other = far;
    remote.channel = far;
    this.connectionState = remote.connectionState = "connected";
    remote.ondatachannel?.({ channel: far });
    this.channel.open();
    far.open();
  }

  async addIceCandidate(c: IceCandidateLike): Promise<void> {
    if (!c.candidate.startsWith("candidate:")) throw new Error("bad candidate");
    this.candidates++;
  }

  close(): void {
    if (this.connectionState === "closed") return;
    this.connectionState = "closed";
    this.channel?.close();
  }
}

export class World {
  readonly clients = new Map<string, FakeSocket>();
  readonly pcs = new Map<string, FakePc>();
  /** Every message the server was asked to relay. */
  readonly relayed: { type: string; src: string; dst: string }[] = [];
  /** Every socket URL opened and every URL fetched. */
  readonly urls: string[] = [];
  readonly fetched: string[] = [];
  heartbeats = 0;
  /** Whether the server lists its peers (`--allow_discovery`). */
  allowDiscovery = true;
  /** The server is unreachable. */
  down = false;
  now = 0;
  private timers: { at: number; seq: number; f: () => void }[] = [];
  private seq = 0;

  /** A page's environment; `seed` makes its random numbers its own. */
  env(seed: number): MeshEnv {
    const world = this;
    let x = (seed * 2654435761) >>> 0 || 1;
    return {
      WebSocket: class extends FakeSocket {
        constructor(url: string) {
          super(world, url);
        }
      },
      RTCPeerConnection: class extends FakePc {
        constructor() {
          super(world);
        }
      },
      fetch: async (url) => {
        world.fetched.push(url);
        if (world.down) throw new Error("network error");
        if (!world.allowDiscovery) return { ok: false, status: 401, text: async () => "Unauthorized" };
        return { ok: true, status: 200, text: async () => JSON.stringify([...world.clients.keys()]) };
      },
      after: (ms, f) => {
        const t = { at: world.now + ms, seq: world.seq++, f };
        world.timers.push(t);
        return () => {
          world.timers = world.timers.filter((u) => u !== t);
        };
      },
      random: () => {
        // xorshift32
        x ^= x << 13;
        x >>>= 0;
        x ^= x >>> 17;
        x ^= x << 5;
        x >>>= 0;
        return x / 0x1_0000_0000;
      },
    };
  }

  /** Advances the clock by `ms`, running the timers that come due. */
  async run(ms: number): Promise<void> {
    const end = this.now + ms;
    await settle();
    for (;;) {
      const next = this.timers.filter((t) => t.at <= end).sort((a, b) => a.at - b.at || a.seq - b.seq)[0];
      if (!next) break;
      this.timers = this.timers.filter((t) => t !== next);
      this.now = Math.max(this.now, next.at);
      next.f();
      await settle();
    }
    this.now = end;
    await settle();
  }

  /** The server closes `id`'s socket. */
  drop(id: string): void {
    this.clients.get(id)?.drop();
  }
}
