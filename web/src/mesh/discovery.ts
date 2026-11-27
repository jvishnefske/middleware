// Who else is in the room (design.md BM8). DOM-free.
//
// * Listing first: the server's peer list (`GET .../<key>/peers`, a server
//   started with `allow_discovery`), every `LIST_POLL_MS`. The ids with
//   this room's prefix are the room.
// * When the server refuses (401, 404, no answer), a rendezvous: the page
//   that holds the id `mw-<room>-hub` on a second signalling socket is the
//   hub. Every other page opens a data channel to the hub, says its id,
//   and gets the list of ids on every change. A member whose id is not the
//   one its offer came from is not listed.
// * When the hub goes away its members each wait a random part of
//   `TAKEOVER_MS` and try to register the hub id; the first is the new
//   hub, the others join it.
//
// `onPeers` hears the room's other ids (sorted) whenever they are learnt
// again; `onMode` the way they are being learnt.

import type { MeshServer, MeshSignal, MeshSignalOut, Protocol } from "../protocol/wasm";
import { type IceServer, Listeners, type MeshEnv, token } from "./env";
import { RtcLink } from "./rtc";
import { SignalSocket } from "./signal";

/** How often the server's list is read. */
export const LIST_POLL_MS = 3000;
/** The longest a member waits before claiming a lost hub's id. */
export const TAKEOVER_MS = 2000;
/** The wait after a signalling error before trying again. */
export const RETRY_MS = 5000;
/** The label of a hub's data channels. */
export const HUB_LABEL = "mw-hub";

export type DiscoveryMode = "starting" | "listing" | "hub" | "member" | "searching";

export interface DiscoveryDeps {
  proto: Protocol;
  env: MeshEnv;
  server: MeshServer;
  ice: IceServer[];
  room: string;
  /** This page's id on the server. */
  id: string;
  /** Sends on the page's own signalling socket. */
  send(s: MeshSignalOut): void;
}

export class Discovery {
  private readonly peersHeard = new Listeners<[string[]]>();
  private readonly modes = new Listeners<[DiscoveryMode]>();
  private readonly hubId: string;
  private current: DiscoveryMode = "starting";
  private stopped = true;
  private cancel: (() => void) | null = null;
  // As the hub.
  private hubSocket: SignalSocket | null = null;
  private readonly offers = new Map<string, RtcLink>();
  private readonly members = new Map<string, RtcLink>();
  // As a member.
  private hubLink: RtcLink | null = null;

  constructor(private readonly d: DiscoveryDeps) {
    this.hubId = d.proto.mesh.hubId(d.room);
  }

  get mode(): DiscoveryMode {
    return this.current;
  }

  onPeers(f: (ids: string[]) => void): () => void {
    return this.peersHeard.add(f);
  }

  onMode(f: (m: DiscoveryMode) => void): () => void {
    return this.modes.add(f);
  }

  private setMode(m: DiscoveryMode): void {
    if (this.current === m) return;
    this.current = m;
    this.modes.emit(m);
  }

  private later(ms: number, f: () => void): void {
    this.cancel?.();
    this.cancel = this.d.env.after(ms, () => {
      this.cancel = null;
      if (!this.stopped) f();
    });
  }

  private heard(ids: readonly string[]): void {
    this.peersHeard.emit(this.d.proto.mesh.roomPeers(this.d.room, this.d.id, ids));
  }

  start(): void {
    if (!this.stopped) return;
    this.stopped = false;
    void this.poll();
  }

  private async list(): Promise<string[] | null> {
    try {
      const r = await this.d.env.fetch(this.d.proto.mesh.peersUrl(this.d.server));
      if (!r.ok) return null;
      return this.d.proto.mesh.parseListing(await r.text()) ?? null;
    } catch {
      return null;
    }
  }

  private async poll(): Promise<void> {
    const ids = await this.list();
    if (this.stopped) return;
    if (!ids) {
      // No listing here (or no longer): find each other through a hub.
      this.setMode("searching");
      void this.claim();
      return;
    }
    this.setMode("listing");
    this.heard(ids);
    this.later(LIST_POLL_MS, () => void this.poll());
  }

  /** Tries to become the hub; joins the one there is otherwise. */
  private async claim(): Promise<void> {
    const { proto, env, server } = this.d;
    const socket = new SignalSocket(proto, env, server, this.hubId, token(env));
    const r = await socket.opened;
    if (this.stopped) {
      socket.close();
      return;
    }
    if (r === "open") this.serve(socket);
    else if (r === "taken") this.joinHub();
    else this.later(RETRY_MS, () => void this.claim());
  }

  // ---- hub

  private broadcast(): void {
    const ids = [this.d.id, ...this.members.keys()].sort();
    const text = this.d.proto.mesh.hubPeers(ids);
    for (const link of this.members.values()) link.send(text);
    this.heard(ids);
  }

  private serve(socket: SignalSocket): void {
    const { proto, env } = this.d;
    this.hubSocket = socket;
    this.setMode("hub");
    this.heard([]);
    socket.onSignal((s) => {
      if (!("peer" in s)) return;
      if (s.type !== "offer") {
        this.offers.get(`${s.peer} ${"connection" in s ? s.connection : ""}`)?.signal(s);
        return;
      }
      const from = s.peer;
      // Only this room's nodes, and no more than the hub lists.
      if (!proto.mesh.parsePeerId(this.d.room, from) || this.offers.size >= proto.mesh.constants.maxMembers) return;
      const slot = `${from} ${s.connection}`;
      const link = new RtcLink(env, this.d.ice, (m) => socket.send(m), from, s.connection, HUB_LABEL);
      this.offers.set(slot, link);
      link.onMessage = (data) => {
        const m = typeof data === "string" ? proto.mesh.hubDecode(data) : undefined;
        // A member lists itself: the id its offer came from.
        if (m?.type !== "join" || m.id !== from) return;
        const old = this.members.get(from);
        this.members.set(from, link);
        if (old && old !== link) old.close();
        this.broadcast();
      };
      link.onClose = () => {
        this.offers.delete(slot);
        if (this.members.get(from) === link) {
          this.members.delete(from);
          if (!this.stopped && this.hubSocket === socket) this.broadcast();
        }
      };
      void link.accept(s.sdp);
    });
    socket.onClose(() => {
      // The server dropped the hub id: the room has no hub until someone
      // claims it again (the members notice their channel only when this
      // page goes; they stay linked to each other meanwhile).
      if (this.stopped || this.hubSocket !== socket) return;
      this.dropHub();
      this.setMode("searching");
      this.later(this.d.env.random() * TAKEOVER_MS, () => void this.claim());
    });
  }

  private dropHub(): void {
    const socket = this.hubSocket;
    this.hubSocket = null;
    for (const link of [...this.offers.values()]) link.close();
    this.offers.clear();
    this.members.clear();
    socket?.close();
  }

  // ---- member

  private joinHub(): void {
    const { proto, env } = this.d;
    const link = new RtcLink(env, this.d.ice, this.d.send, this.hubId, `hub_${token(env)}`, HUB_LABEL);
    this.hubLink = link;
    this.setMode("member");
    link.onOpen = () => link.send(proto.mesh.hubJoin(this.d.id));
    link.onMessage = (data) => {
      const m = typeof data === "string" ? proto.mesh.hubDecode(data) : undefined;
      if (m?.type === "peers") this.heard(m.ids);
    };
    link.onClose = () => {
      if (this.hubLink !== link) return;
      this.hubLink = null;
      if (this.stopped) return;
      // The hub left (or never answered): whoever claims its id first is
      // the next one.
      this.setMode("searching");
      this.later(this.d.env.random() * TAKEOVER_MS, () => void this.claim());
    };
    void link.dial();
  }

  /** A signalling message from the hub's id on the page's own socket. */
  signal(s: MeshSignal): void {
    if ("peer" in s && s.type === "expire") {
      // The hub id is registered but nobody answers there.
      this.hubLink?.close();
      return;
    }
    this.hubLink?.signal(s);
  }

  /** The id whose messages `signal` takes. */
  get hub(): string {
    return this.hubId;
  }

  stop(): void {
    this.stopped = true;
    this.cancel?.();
    this.cancel = null;
    this.dropHub();
    const link = this.hubLink;
    this.hubLink = null;
    link?.close();
    this.current = "starting";
  }
}
