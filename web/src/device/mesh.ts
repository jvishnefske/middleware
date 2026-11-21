// The mesh: nodes reached through the cloud Zenoh router over a WebSocket
// (`wss://updates.<site>/mesh`), for probing a lathe from the browser.
//
// * `MeshTransport` owns the WebSocket and one timer around a protocol-wasm
//   `MeshSession` (the middleware's Zenoh client, polled by hand: see
//   protocol-wasm `mesh`). Each ws message is fed to the session and the
//   session is polled; what it wrote is sent; finished commands settle
//   their promises; the timer is re-armed for the session's next deadline
//   (keep-alives, timeouts). The session runs commands one at a time in
//   order. The link going down fails everything pending with
//   `MeshDownError` and calls `onClosed`; a new connection is a new
//   transport.
// * `scan` asks every node for `sys/info` (`@mw/node/*/sys/info`) and lists
//   each replier once, by its sender id (the key the node answers at; a
//   reply whose sender is not 16 lowercase hex digits is left out).
//   Sender ids are not stored: always rescan before probing.
// * `MeshClient` is a `DeviceClient` for one node, by sender id. It is
//   probe-only: the cloud router lets a browser ask `sys/info`,
//   `sys/manifest`, `sys/manifest/cm` and `sys/trust/info` and drops any
//   other query, so `request` refuses other keys with `ProbeOnlyError`
//   before sending, `board`, `peers` and `binding` answer as for firmware
//   without them, and deploys go through `mwctl` with the lathe's key.
//
// No DOM here beyond the `WebSocket` constructor, which tests replace.

import type { DeviceInfo, MeshCompletion, MeshSessionLike, Protocol } from "../protocol/wasm";
import { DENIED_CODE, TRUST_KEYS } from "../trust/wire";
import {
  DeniedError,
  type DeviceClient,
  ProbeOnlyError,
  RequestClient,
  RequestError,
  type RequestOptions,
  RequestTimeout,
} from "./client";

/** Request timeout over the mesh unless a caller sets one (mwctl's). */
export const MESH_TIMEOUT_MS = 8000;
/** How long a scan gathers replies. */
export const SCAN_TIMEOUT_MS = 3000;
/** At most this many replies to a scan. */
export const SCAN_MAX = 64;

/** The keys a browser may ask over the mesh (the cloud router's ACL). */
export const MESH_PROBE_KEYS: ReadonlySet<string> = new Set([
  "sys/info",
  "sys/manifest",
  "sys/manifest/cm",
  TRUST_KEYS.info,
]);

/** The WebSocket the transport needs (the browser's, or a test's). */
export interface SocketLike {
  binaryType: string;
  send(data: Uint8Array): void;
  close(): void;
  onopen: ((ev: unknown) => void) | null;
  onmessage: ((ev: { data: unknown }) => void) | null;
  onclose: ((ev: unknown) => void) | null;
  onerror: ((ev: unknown) => void) | null;
}

export interface MeshOptions {
  /** Defaults to the global `WebSocket`. */
  WebSocketCtor?: new (url: string) => SocketLike;
  /** Defaults to `proto.newMeshSession` with 16 random bytes. */
  newSession?: () => MeshSessionLike;
  /** Milliseconds, never decreasing; defaults to `performance.now`. */
  now?: () => number;
}

/** The link to the router is gone. */
export class MeshDownError extends Error {
  constructor(what = "mesh") {
    super(`${what}: the mesh link is down; reconnect`);
    this.name = "MeshDownError";
  }
}

/** One reply to a discovery. */
export interface Discovered {
  /** The replier's sender id, 16 hex, when its reply carried one. */
  sender?: string;
  body?: Uint8Array;
  /** The `HandlerError` code of an error reply. */
  status?: number;
}

/** What a `MeshClient` and `scan` need of a transport (a `MeshTransport`,
 * or a test's fake). */
export interface MeshLink {
  readonly senderId: string;
  readonly up: boolean;
  request(sender: string | null, key: string, body: Uint8Array, timeoutMs: number): Promise<Uint8Array>;
  discover(key: string, body: Uint8Array, timeoutMs: number, max?: number): Promise<Discovered[]>;
  close(): Promise<void>;
  onClosed(f: (e?: unknown) => void): void;
}

interface Waiter {
  key: string;
  resolve(c: MeshCompletion): void;
  reject(e: Error): void;
}

/** A Zenoh session over one WebSocket (see the module comment). */
export class MeshTransport implements MeshLink {
  private readonly waiting = new Map<number, Waiter>();
  private readonly closedListeners: ((e?: unknown) => void)[] = [];
  private timer: ReturnType<typeof setTimeout> | null = null;
  private isUp = true;

  private constructor(
    private readonly ws: SocketLike,
    private readonly session: MeshSessionLike,
    private readonly proto: Protocol,
    private readonly now: () => number,
  ) {}

  /** Opens the WebSocket to `url` and runs the Zenoh handshake on it. */
  static async connect(url: string, proto: Protocol, opts: MeshOptions = {}): Promise<MeshTransport> {
    const Ctor = opts.WebSocketCtor ?? (globalThis.WebSocket as unknown as new (url: string) => SocketLike);
    if (!Ctor) throw new Error("this browser has no WebSocket");
    const now = opts.now ?? (() => performance.now());
    const session = opts.newSession?.() ?? proto.newMeshSession(crypto.getRandomValues(new Uint8Array(16)));
    const ws = new Ctor(url);
    ws.binaryType = "arraybuffer";
    await new Promise<void>((resolve, reject) => {
      ws.onopen = () => resolve();
      ws.onerror = () => reject(new Error(`${url}: could not connect`));
      ws.onclose = () => reject(new Error(`${url}: closed before it opened`));
    });
    const t = new MeshTransport(ws, session, proto, now);
    ws.onmessage = (ev) => t.message(ev.data);
    ws.onclose = (ev) => t.linkClosed(ev);
    ws.onerror = () => {};
    const done = await t.submit("handshake", session.open());
    if (done.kind !== "opened") {
      t.isUp = false;
      ws.close();
      throw new Error(`${url}: the Zenoh handshake failed (${errorText(done)})`);
    }
    return t;
  }

  /** Our sender id, as nodes see it. */
  get senderId(): string {
    return this.session.senderId;
  }

  /** False once the link is down. */
  get up(): boolean {
    return this.isUp;
  }

  /** Calls `f` once when the link goes down. */
  onClosed(f: (e?: unknown) => void): void {
    this.closedListeners.push(f);
  }

  /** Asks `key` of the node with sender id `sender` (or whoever serves it,
   * for null). Rejects as `SerialClient` does: `RequestError` (`DeniedError`
   * for Denied), `RequestTimeout`, or `MeshDownError`. */
  async request(sender: string | null, key: string, body: Uint8Array, timeoutMs: number): Promise<Uint8Array> {
    if (!this.isUp) throw new MeshDownError(key);
    const done = await this.submit(key, this.session.request(sender ?? undefined, key, body, timeoutMs));
    if (done.kind === "reply") return done.body;
    throw this.failure(key, done);
  }

  /** Asks `key` of every node and gathers the replies until the router's
   * final response, `max` of them or `timeoutMs`. */
  async discover(key: string, body: Uint8Array, timeoutMs: number, max = SCAN_MAX): Promise<Discovered[]> {
    if (!this.isUp) throw new MeshDownError(key);
    const done = await this.submit(key, this.session.discover(key, body, timeoutMs, max));
    if (done.kind === "replies") return done.replies;
    throw this.failure(key, done);
  }

  /** Sends Close and drops the link. */
  async close(): Promise<void> {
    if (!this.isUp) return;
    await this.submit("close", this.session.close()).catch(() => {});
    this.ws.close();
  }

  private submit(key: string, id: number): Promise<MeshCompletion> {
    const p = new Promise<MeshCompletion>((resolve, reject) => this.waiting.set(id, { key, resolve, reject }));
    this.pump();
    return p;
  }

  private message(data: unknown): void {
    const bytes = data instanceof ArrayBuffer ? new Uint8Array(data) : data instanceof Uint8Array ? data : null;
    if (!bytes) return;
    this.session.feed(bytes);
    this.pump();
  }

  private linkClosed(e: unknown): void {
    this.session.closed();
    this.pump();
    this.down(e);
  }

  private down(e?: unknown): void {
    if (!this.isUp) return;
    this.isUp = false;
    if (this.timer !== null) clearTimeout(this.timer);
    this.timer = null;
    for (const w of this.waiting.values()) w.reject(new MeshDownError(w.key));
    this.waiting.clear();
    for (const f of this.closedListeners.splice(0)) f(e);
  }

  /** Polls the session, sends its output, settles what finished and
   * re-arms the timer. */
  private pump(): void {
    const now = this.now();
    const done = this.session.poll(now);
    for (const m of this.session.takeOutput()) {
      try {
        this.ws.send(m);
      } catch {
        /* the close event follows */
      }
    }
    let lost = false;
    for (const c of done) {
      const w = this.waiting.get(c.id);
      if (!w) continue;
      this.waiting.delete(c.id);
      if (c.kind === "error" && c.failure === "down") lost = true;
      w.resolve(c);
    }
    if (this.timer !== null) clearTimeout(this.timer);
    this.timer = null;
    if (lost) {
      this.down();
      this.ws.close();
      return;
    }
    const wake = this.session.nextWakeMs();
    if (wake !== undefined && this.isUp) {
      this.timer = setTimeout(
        () => {
          this.timer = null;
          this.pump();
        },
        Math.max(0, wake - now),
      );
    }
  }

  private failure(key: string, c: MeshCompletion): Error {
    if (c.kind !== "error") return new Error(`${key}: unexpected ${c.kind}`);
    switch (c.failure) {
      case "handler":
        return c.status === DENIED_CODE
          ? new DeniedError(key)
          : new RequestError(key, this.proto.statusName(c.status ?? 0), c.status ?? 0);
      case "noReply":
        return new RequestTimeout(key, 1);
      case "down":
        return new MeshDownError(key);
      default:
        return new Error(`${key}: ${errorText(c)}`);
    }
  }
}

function errorText(c: MeshCompletion): string {
  if (c.kind !== "error") return c.kind;
  if (c.failure === "other") return c.message ?? "failed";
  return c.failure === "tooLarge" ? "too large for one batch" : c.failure;
}

/** A node a scan found: its sender id and `sys/info`, or why it gave none. */
export interface MeshDevice {
  sender: string;
  info: DeviceInfo | null;
  /** Why there is no `info` (an error reply's status name, a bad body). */
  problem?: string;
}

/** Every node that answers `sys/info` on the mesh, each sender once, in
 * reply order. Replies without a sender id cannot be addressed and are
 * left out. */
export async function scan(t: MeshLink, proto: Protocol, timeoutMs = SCAN_TIMEOUT_MS): Promise<MeshDevice[]> {
  const replies = await t.discover(proto.constants.keys.info, new Uint8Array(), timeoutMs);
  const rows = new Map<string, MeshDevice>();
  for (const r of replies) {
    // A sender id is addressed and shown: only the shape a node has.
    if (!r.sender || !SENDER_ID.test(r.sender) || rows.has(r.sender)) continue;
    if (r.body) {
      const info = proto.decodeDeviceInfo(r.body) ?? null;
      rows.set(
        r.sender,
        info ? { sender: r.sender, info } : { sender: r.sender, info: null, problem: "malformed sys/info" },
      );
    } else {
      rows.set(r.sender, { sender: r.sender, info: null, problem: proto.statusName(r.status ?? 0) });
    }
  }
  return [...rows.values()];
}

/** One node over the mesh, probe-only (see the module comment). */
export class MeshClient extends RequestClient implements DeviceClient {
  readonly transport = "mesh" as const;
  readonly remote = true;

  constructor(
    private readonly t: MeshLink,
    readonly device: MeshDevice,
    proto: Protocol,
  ) {
    super(proto);
  }

  /** Asks the node `key`; a key the cloud does not let a browser ask is
   * refused with `ProbeOnlyError` and nothing is sent. `retries` is
   * ignored (the link is reliable; a dead one needs a reconnect). */
  request(key: string, body: Uint8Array = new Uint8Array(), opts: RequestOptions = {}): Promise<Uint8Array> {
    if (!MESH_PROBE_KEYS.has(key)) return Promise.reject(new ProbeOnlyError(key));
    return this.t.request(this.device.sender, key, body, opts.timeoutMs ?? MESH_TIMEOUT_MS);
  }

  /** Not asked over the mesh (the cloud drops it): as firmware without
   * `sys/board`. */
  async board(_opts?: RequestOptions): Promise<null> {
    return null;
  }

  /** Not asked over the mesh: no peers. */
  async peers(_opts?: RequestOptions): Promise<string[]> {
    return [];
  }

  /** Not asked over the mesh: no stored binding is known. */
  async binding(_opts?: RequestOptions): Promise<null> {
    return null;
  }

  /** Not asked over the mesh. */
  async peerManifestBody(_node: string, _opts?: RequestOptions): Promise<null> {
    return null;
  }

  bind(_body: Uint8Array, _opts?: RequestOptions): Promise<void> {
    return Promise.reject(new ProbeOnlyError("bind"));
  }

  trustClaim(_publicKey: Uint8Array, _opts?: RequestOptions): Promise<void> {
    return Promise.reject(new ProbeOnlyError("claim"));
  }

  startPayload(_opts?: RequestOptions): Promise<void> {
    return Promise.reject(new ProbeOnlyError("start"));
  }

  stopPayload(_opts?: RequestOptions): Promise<void> {
    return Promise.reject(new ProbeOnlyError("stop"));
  }

  /** The transport is shared by every node of a scan; closing one client
   * leaves it open. */
  async close(): Promise<void> {}
}

/** Where the mesh is: `?mesh=<url>` on the page, else a stored choice, else
 * `wss://updates.<site>/mesh` for the page's site (its host name without
 * the first label), or a local router's `ws://<host>:8000` on localhost or
 * an IP address. */
export function defaultMeshUrl(where: { hostname: string; search?: string; stored?: string | null }): string {
  const fromQuery = where.search ? new URLSearchParams(where.search).get("mesh") : null;
  if (fromQuery) return fromQuery;
  if (where.stored) return where.stored;
  const host = where.hostname;
  if (host === "localhost" || /^[0-9.]+$/.test(host) || host.includes(":")) return `ws://${host}:8000`;
  const labels = host.split(".");
  const site = labels.length > 2 ? labels.slice(1).join(".") : host;
  return `wss://updates.${site}/mesh`;
}

/** A strict DNS name: dot-separated labels of letters, digits and inner
 * hyphens, at least two labels, at most 253 characters. */
export const DNS_NAME = /^(?=.{1,253}$)[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?(\.[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?)+$/i;

/** A sender id: 16 lowercase hex digits. */
export const SENDER_ID = /^[0-9a-f]{16}$/;

/** The site of a mesh URL `wss://updates.<site>/mesh`, whose mTLS router
 * `mesh.updates.<site>` is where mwctl connects, lowercased; null unless
 * the host is a strict DNS name (`isDnsName`), so no IP address, local name
 * or anything a shell would read. */
export function siteOfMeshUrl(url: string): string | null {
  let host: string;
  try {
    host = new URL(url).hostname.toLowerCase();
  } catch {
    return null;
  }
  const site = host.startsWith("updates.") ? host.slice("updates.".length) : host;
  return isDnsName(host) && isDnsName(site) ? site : null;
}

/** `DNS_NAME`, and not an IPv4 address (its last label has a letter). */
export function isDnsName(s: string): boolean {
  return DNS_NAME.test(s) && /[a-z]/i.test(s.slice(s.lastIndexOf(".") + 1));
}
