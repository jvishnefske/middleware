// The browser mesh (design.md BM): pages that open the same room find
// each other through a PeerJS-compatible server (signal.ts, discovery.ts),
// open WebRTC data channels (rtc.ts) and forward their payloads'
// publications to each other. Every format and rule is protocol-wasm's
// (`proto.mesh`); this file holds the connections. DOM-free.
//
// * One message is one raw frame. A local publication goes to every
//   linked peer unless the gate keeps it (`sys/...`, `ch/...`, `drv/...`,
//   more than 512 bytes); a peer's publication passes the same gate and is
//   handed on with the peer's node id as sender. The frame carries no
//   sender: it is the node the peer's id names, which its `sys/info` must
//   repeat before anything it publishes is taken.
// * A peer may ask three things: `sys/info`, `sys/manifest` and
//   `sys/payload/channels`. Every other request is answered NotFound.
//   Nothing a peer sends is passed to a board or to a request handler.
// * Peers are not authenticated. A node id is what the peer says it is.
// * `bind(net)` joins the mesh to a simulator network: the bus's
//   publications go out, and each peer is a `UnitLink` attached to the
//   `MixedNet`, which delivers its publications to the simulated units as
//   it does a board's. `addBoard(link)` sends a board's publications out
//   too; nothing comes back to it.
// * The lower id dials; a node holds at most 16 links, the rest of the
//   room is listed only (`listed`).

import type { ChannelDecl, DeviceInfo, Manifest, MeshPairing, MeshServer, MeshSignal, Protocol } from "../protocol/wasm";
import type { MixedNet, NetReply, UnitLink } from "../sim/net";
import { Discovery, type DiscoveryMode } from "./discovery";
import { type IceServer, Listeners, type MeshEnv, browserEnv, token } from "./env";
import { RtcLink } from "./rtc";
import { SignalSocket } from "./signal";

/** PeerJS's public server and STUN: the defaults. */
export const DEFAULT_SERVER: MeshServer = { host: "0.peerjs.com", port: 443, path: "/", key: "peerjs", secure: true };
export const DEFAULT_ICE: IceServer[] = [{ urls: "stun:stun.l.google.com:19302" }];

/** How long a hello request may take. */
export const HELLO_TIMEOUT_MS = 5000;
/** How often the dial plan is run again and peers' channels re-read. */
export const TICK_MS = 5000;
/** The label of a mesh data channel. */
export const MESH_LABEL = "mw";

/** What this page answers a peer's hello with. */
export interface Hello {
  /** `sys/info` body. */
  info: Uint8Array;
  /** `sys/manifest` body. */
  manifest: Uint8Array;
  /** The payload's `mw.channels` section bytes (empty: none). */
  channels: Uint8Array;
}

/** The hello of a page with node id `node` running `wasm` (or nothing). */
export function pageHello(proto: Protocol, node: string, wasm: Uint8Array | null): Hello {
  const manifest = proto.simulatorManifest(node, true) ?? new Uint8Array();
  const caps = proto.decodeManifest(manifest)?.caps ?? 0;
  return {
    info: proto.encodeDeviceInfo({ abiVersion: proto.abi.version, capacity: 0, status: wasm ? 2 : 0, chip: "browser", firmware: "web", node, caps }),
    manifest,
    channels: wasm ? proto.channelsSection(wasm) : new Uint8Array(),
  };
}

export interface MeshOptions {
  proto: Protocol;
  room: string;
  /** This page's node id (16 hex digits). */
  node: string;
  hello(): Hello;
  server?: MeshServer;
  iceServers?: IceServer[];
  env?: MeshEnv;
}

export type MeshState = "idle" | "joining" | "joined" | "reconnecting" | "failed";

export interface MeshCounters {
  /** Publications sent (once per publication, however many peers). */
  sent: number;
  /** Publications taken from peers. */
  received: number;
  /** Messages from peers that were refused. */
  refused: number;
  /** Local publications the gate kept in the page. */
  kept: number;
}

export interface PeerView {
  /** The peer's id on the server. */
  id: string;
  /** Its node id. */
  node: string;
  /** "connecting" until the channel opens, "hello" until its `sys/info`
   * names the node of its id, then "open". */
  state: "connecting" | "hello" | "open";
  info?: DeviceInfo;
  manifest?: Manifest;
  channels: ChannelDecl[];
  /** Its channels next to this page's. */
  pairs: MeshPairing[];
  received: number;
  refused: number;
}

/** One peer, as a unit of a `MixedNet`. */
export class PeerLink implements UnitLink {
  private readonly publishers = new Listeners<[string, Uint8Array]>();
  private readonly presence = new Listeners<[boolean]>();
  private readonly pending = new Map<number, { resolve(body: Uint8Array | null): void; cancel(): void }>();
  private nextId = 0;
  state: PeerView["state"] = "connecting";
  info?: DeviceInfo;
  manifest?: Manifest;
  section: Uint8Array = new Uint8Array();
  channels: ChannelDecl[] = [];
  received = 0;
  refused = 0;

  constructor(
    readonly id: string,
    readonly node: string,
    readonly rtc: RtcLink,
    private readonly proto: Protocol,
    private readonly env: MeshEnv,
  ) {}

  get connected(): boolean {
    return this.state === "open";
  }

  /** The room carries data only: a request to a peer is lost. */
  request(): Promise<NetReply> {
    return Promise.resolve(null);
  }

  onPublish(f: (key: string, data: Uint8Array) => void): () => void {
    return this.publishers.add(f);
  }

  onPresence(f: (connected: boolean) => void): () => void {
    return this.presence.add(f);
  }

  /** A publication of the peer that passed the gate. */
  deliver(key: string, data: Uint8Array): void {
    this.received++;
    this.publishers.emit(key, data);
  }

  /** Asks the peer for a hello key; null when refused or unanswered. */
  ask(key: string): Promise<Uint8Array | null> {
    // Ids not waiting for an answer (at most three are).
    let id = this.nextId;
    while (this.pending.has(id)) id = (id + 1) & 0xff;
    this.nextId = (id + 1) & 0xff;
    const msg = this.proto.mesh.helloRequest(id, key);
    if (!msg || !this.rtc.send(msg)) return Promise.resolve(null);
    return new Promise((resolve) => {
      const cancel = this.env.after(HELLO_TIMEOUT_MS, () => {
        this.pending.delete(id);
        resolve(null);
      });
      this.pending.set(id, { resolve, cancel });
    });
  }

  /** An answer from the peer; false when nothing was asked under `id`. */
  answered(id: number, body: Uint8Array | null): boolean {
    const p = this.pending.get(id);
    if (!p) return false;
    this.pending.delete(id);
    p.cancel();
    p.resolve(body);
    return true;
  }

  opened(): void {
    this.state = "open";
    this.presence.emit(true);
  }

  gone(): void {
    const was = this.state === "open";
    this.state = "connecting";
    for (const p of this.pending.values()) {
      p.cancel();
      p.resolve(null);
    }
    this.pending.clear();
    if (was) this.presence.emit(false);
    this.publishers.clear();
    this.presence.clear();
  }
}

/** The unit name a peer has on a bound `MixedNet`. */
export function peerUnit(node: string): string {
  return `room:${node}`;
}

export class Mesh {
  private readonly proto: Protocol;
  private readonly env: MeshEnv;
  private readonly server: MeshServer;
  private readonly ice: IceServer[];
  private readonly changes = new Listeners<[]>();
  private readonly heard = new Listeners<[string, string, Uint8Array]>();
  private readonly links = new Map<string, PeerLink>();
  private socket: SignalSocket | null = null;
  private discovery: Discovery | null = null;
  private net: MixedNet | null = null;
  private cancelTick: (() => void) | null = null;
  private cancelRetry: (() => void) | null = null;
  private attempts = 0;
  private roomIds: string[] = [];
  private current: MeshState = "idle";
  /** This page's id on the server. */
  readonly id: string;
  readonly room: string;
  readonly node: string;
  readonly counters: MeshCounters = { sent: 0, received: 0, refused: 0, kept: 0 };
  /** Why the state is "failed" or "reconnecting". */
  error = "";

  constructor(private readonly opts: MeshOptions) {
    this.proto = opts.proto;
    this.env = opts.env ?? browserEnv();
    this.server = opts.server ?? DEFAULT_SERVER;
    this.ice = opts.iceServers ?? DEFAULT_ICE;
    this.room = opts.room;
    this.node = opts.node.toLowerCase();
    this.id = this.proto.mesh.peerId(this.room, this.node);
  }

  get state(): MeshState {
    return this.current;
  }

  /** How the room's members are being learnt. */
  get discoveryMode(): DiscoveryMode {
    return this.discovery?.mode ?? "starting";
  }

  /** The room's other ids, linked or not. */
  get listed(): readonly string[] {
    return this.roomIds;
  }

  get peers(): PeerView[] {
    const own = this.opts.hello().channels;
    return [...this.links.values()].map((l) => {
      let pairs: MeshPairing[] = [];
      try {
        pairs = this.proto.mesh.compare(own, l.section);
      } catch {
        // A section that does not parse pairs with nothing.
      }
      return { id: l.id, node: l.node, state: l.state, info: l.info, manifest: l.manifest, channels: l.channels, pairs, received: l.received, refused: l.refused };
    });
  }

  /** After any change of state, peers or what is known of them (not per
   * message: read `counters` when rendering). */
  onChange(f: () => void): () => void {
    return this.changes.add(f);
  }

  /** Every publication taken from a peer: its node, the key, the data. */
  onPublish(f: (node: string, key: string, data: Uint8Array) => void): () => void {
    return this.heard.add(f);
  }

  private set(state: MeshState, error = ""): void {
    this.current = state;
    this.error = error;
    this.changes.emit();
  }

  // ---- signalling

  /** Registers on the server and starts looking for the room. Resolves
   * when joined; a failure is in `state` and `error`. */
  async join(): Promise<void> {
    if (this.current !== "idle" && this.current !== "failed") return;
    this.attempts = 0;
    this.set("joining");
    await this.connect();
  }

  private async connect(): Promise<void> {
    const socket = new SignalSocket(this.proto, this.env, this.server, this.id, token(this.env));
    this.socket = socket;
    const r = await socket.opened;
    if (this.socket !== socket) {
      socket.close();
      return;
    }
    if (r === "taken") {
      this.socket = null;
      // Another tab of this browser, most likely: same stored node id.
      this.fail(`node ${this.node} is already in room ${this.room} (another tab of this browser?)`);
      return;
    }
    if (r === "error") {
      this.socket = null;
      if (this.current === "reconnecting") this.retry(socket.error);
      else this.fail(socket.error);
      return;
    }
    this.attempts = 0;
    socket.onSignal((s) => this.signal(s));
    socket.onClose(() => {
      if (this.socket !== socket) return;
      this.socket = null;
      // The data channels stay up; only finding new peers needs the server.
      this.discovery?.stop();
      this.retry(socket.error);
    });
    if (!this.discovery) {
      this.discovery = new Discovery({
        proto: this.proto,
        env: this.env,
        server: this.server,
        ice: this.ice,
        room: this.room,
        id: this.id,
        send: (m) => void this.socket?.send(m),
      });
      this.discovery.onPeers((ids) => {
        this.roomIds = ids;
        this.plan();
        this.changes.emit();
      });
      this.discovery.onMode(() => this.changes.emit());
    }
    this.discovery.start();
    this.cancelTick?.();
    this.tick();
    this.set("joined");
  }

  private fail(why: string): void {
    this.teardown();
    this.set("failed", why);
  }

  private retry(why: string): void {
    this.attempts++;
    this.set("reconnecting", why);
    this.cancelRetry?.();
    this.cancelRetry = this.env.after(Math.min(30_000, 1000 * 2 ** Math.min(this.attempts, 5)), () => {
      this.cancelRetry = null;
      if (this.current === "reconnecting") void this.connect();
    });
  }

  private tick(): void {
    this.cancelTick = this.env.after(TICK_MS, () => {
      if (this.current !== "joined" && this.current !== "reconnecting") return;
      this.plan();
      for (const l of this.links.values()) if (l.state === "open") void this.readChannels(l);
      this.tick();
    });
  }

  private signal(s: MeshSignal): void {
    if (!("peer" in s)) return;
    if (s.peer === this.discovery?.hub) {
      this.discovery.signal(s);
      return;
    }
    const link = this.links.get(s.peer);
    if (s.type === "offer") {
      // The lower id dials: an offer comes from a lower id of this room,
      // to a node with a link to spare and none to that peer.
      const node = this.proto.mesh.parsePeerId(this.room, s.peer);
      if (!node || link || this.proto.mesh.dialPlan(s.peer, [this.id], []).length === 0 || !this.proto.mesh.hasRoom(this.links.size)) {
        this.counters.refused++;
        return;
      }
      void this.link(s.peer, node, s.connection).rtc.accept(s.sdp);
    } else if (s.type === "answer" || s.type === "candidate") {
      link?.rtc.signal(s);
    } else if (s.type === "expire" || s.type === "leave") {
      // An offer that reached nobody, or the peer saying goodbye.
      if (link && (s.type === "leave" || link.state === "connecting")) link.rtc.close();
    }
  }

  // ---- links

  private plan(): void {
    if (this.current !== "joined") return;
    for (const id of this.proto.mesh.dialPlan(this.id, this.roomIds, [...this.links.keys()])) {
      const node = this.proto.mesh.parsePeerId(this.room, id);
      if (node) void this.link(id, node, `dc_${token(this.env)}`).rtc.dial();
    }
  }

  private link(id: string, node: string, connection: string): PeerLink {
    const rtc = new RtcLink(this.env, this.ice, (m) => void this.socket?.send(m), id, connection, MESH_LABEL);
    const link = new PeerLink(id, node, rtc, this.proto, this.env);
    this.links.set(id, link);
    rtc.onOpen = () => void this.hello(link);
    rtc.onMessage = (data) => this.message(link, data);
    rtc.onClose = () => {
      if (this.links.get(id) !== link) return;
      this.links.delete(id);
      this.net?.detach(peerUnit(link.node));
      link.gone();
      this.changes.emit();
    };
    this.changes.emit();
    return link;
  }

  private async hello(link: PeerLink): Promise<void> {
    link.state = "hello";
    this.changes.emit();
    const { helloKeys } = this.proto.mesh.constants;
    const [info, manifest] = await Promise.all([link.ask(helloKeys[0]!), link.ask(helloKeys[1]!)]);
    if (this.links.get(link.id) !== link) return;
    const decoded = info ? this.proto.decodeDeviceInfo(info) : undefined;
    // The node its id names, said again by the peer itself.
    if (decoded?.node?.toLowerCase() !== link.node) {
      this.counters.refused++;
      link.rtc.close();
      return;
    }
    link.info = decoded;
    link.manifest = manifest ? this.proto.decodeManifest(manifest) : undefined;
    link.opened();
    if (this.net) this.attach(this.net, link);
    this.changes.emit();
    await this.readChannels(link);
  }

  private async readChannels(link: PeerLink): Promise<void> {
    const section = await link.ask(this.proto.mesh.constants.channelsKey);
    if (!section || this.links.get(link.id) !== link) return;
    const same = section.length === link.section.length && section.every((b, i) => b === link.section[i]);
    if (same) return;
    try {
      link.channels = this.proto.decodeChannels(section);
      link.section = section;
    } catch {
      link.refused++;
      this.counters.refused++;
      return;
    }
    this.changes.emit();
  }

  private message(link: PeerLink, data: Uint8Array | string): void {
    const refuse = () => {
      link.refused++;
      this.counters.refused++;
    };
    if (typeof data === "string") {
      refuse();
      return;
    }
    const m = this.proto.mesh.inbound(data);
    if (m.kind === "publish") {
      // Nothing is taken from a peer whose node is not yet known.
      if (link.state !== "open") {
        refuse();
        return;
      }
      this.counters.received++;
      this.heard.emit(link.node, m.key, m.data);
      link.deliver(m.key, m.data);
    } else if (m.kind === "request") {
      const h = this.opts.hello();
      if (!this.proto.mesh.constants.helloKeys.includes(m.key)) refuse();
      link.rtc.send(this.proto.mesh.helloReply(m.id, m.key, h.info, h.manifest, h.channels));
    } else if (m.kind === "reply") {
      if (!link.answered(m.id, m.ok ? m.body : null)) refuse();
    } else {
      refuse();
    }
  }

  /** Sends a local publication to the room; false when the gate keeps it
   * in the page. */
  publish(key: string, data: Uint8Array): boolean {
    const msg = this.proto.mesh.publish(key, data);
    if (!msg) {
      this.counters.kept++;
      return false;
    }
    let any = false;
    for (const l of this.links.values()) any = l.rtc.send(msg) || any;
    if (any) this.counters.sent++;
    return true;
  }

  // ---- the simulator and boards

  private attach(net: MixedNet, link: PeerLink): void {
    try {
      net.attach(peerUnit(link.node), link);
    } catch {
      // A unit of that name is there already: its publications still
      // reach `onPublish` listeners.
    }
  }

  /** Joins the mesh to a simulator network: what its bus publishes goes
   * to the room, and the peers' publications reach its units. Returns the
   * undo. One network at a time. */
  bind(net: MixedNet): () => void {
    this.unbind();
    this.net = net;
    const off = net.bus.onPublish((_unit, key, data) => void this.publish(key, data));
    for (const l of this.links.values()) if (l.state === "open") this.attach(net, l);
    this.unbind = () => {
      off();
      for (const l of this.links.values()) net.detach(peerUnit(l.node));
      this.net = null;
      this.unbind = () => {};
    };
    return () => this.unbind();
  }

  private unbind: () => void = () => {};

  /** Sends a board's publications to the room (through the gate, like the
   * page's own). Nothing from the room goes to the board: the mesh never
   * calls its `request`. Returns the undo. */
  addBoard(link: Pick<UnitLink, "onPublish">): () => void {
    return link.onPublish((key, data) => void this.publish(key, data));
  }

  private teardown(): void {
    this.cancelTick?.();
    this.cancelRetry?.();
    this.cancelTick = this.cancelRetry = null;
    this.discovery?.stop();
    this.discovery = null;
    for (const l of [...this.links.values()]) l.rtc.close();
    this.links.clear();
    this.roomIds = [];
    const socket = this.socket;
    this.socket = null;
    socket?.close();
  }

  /** Leaves the room: every link and the signalling socket close. */
  leave(): void {
    this.teardown();
    this.set("idle");
  }
}
