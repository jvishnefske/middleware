// The host side of a device: `DeviceClient`, what the pages and the upload,
// binding and trust flows use, whatever carries the requests.
//
// * `RequestClient` is the shared base: every typed helper (`info`,
//   `manifest`, `controlPage`, `bind`, `trustInfo`, ...) over one abstract
//   `request(key, body, opts)`.
// * `SerialClient` speaks the serial protocol over a pair of byte streams.
//   It owns a reader loop that feeds the protocol-wasm decoder, matches
//   responses to requests by correlation id, and reports console text,
//   publications and rejected segments through callbacks. It knows nothing
//   about Web Serial or the DOM: it takes a ReadableStream/WritableStream
//   pair, which is a SerialPort's `readable`/`writable` in the browser and
//   an in-memory pipe in the tests.
// * `MeshClient` (mesh.ts) asks one node over Zenoh through the cloud
//   router; it is `remote`.
//
// Trust (U13): `trustInfo` and `trustClaim` speak `sys/trust/*`; a response
// with wire status 5 rejects with `DeniedError`, whose message says how to
// recover (the device pins a different key; hold BOOT to re-key).

import type { BoardInfo, DeviceInfo, LineEvent, Manifest, Protocol } from "../protocol/wasm";
import { DENIED_CODE, TRUST_KEYS, type TrustInfo, claimBody, parseTrustInfo } from "../trust/wire";

export interface ByteStreams {
  readable: ReadableStream<Uint8Array>;
  writable: WritableStream<Uint8Array>;
}

export interface ClientEvents {
  /** Non-frame bytes from the line. */
  onText?(text: string): void;
  /** A publication from the device (including `sys/log`, `sys/payload/status`). */
  onPublish?(key: string, payload: Uint8Array): void;
  /** A segment that failed to decode. */
  onInvalid?(reason: string, len: number): void;
  /** The reader loop ended (port closed, cable pulled, `close()` called). */
  onClosed?(error?: unknown): void;
}

export interface RequestOptions {
  /** Per-attempt response timeout. */
  timeoutMs?: number;
  /** Additional attempts after a timeout. Error responses are never retried. */
  retries?: number;
}

/** The device answered with a non-zero status. */
export class RequestError extends Error {
  constructor(
    readonly key: string,
    readonly status: string,
    readonly statusCode: number,
  ) {
    super(`${key}: device returned ${status}`);
    this.name = "RequestError";
  }
}

/** Wire status code of `HandlerError::NotFound`. */
export const NOT_FOUND_CODE = 1;

/** The device refused with `Denied` (wire code 5): a trust check failed. */
export class DeniedError extends RequestError {
  constructor(key: string) {
    super(key, "Denied", DENIED_CODE);
    this.name = "DeniedError";
    this.message = `${key}: Denied by the device. ${deniedHint(key)}`;
  }
}

/** Recovery advice for a `Denied` answer to `key`. */
export function deniedHint(key: string): string {
  const rekey = "hold BOOT ≥3 s after boot to open the 60 s re-key window";
  if (key === TRUST_KEYS.claim) return `The device is already claimed by another key — ${rekey}, then claim again.`;
  return `The device trusts a different key, or this version is not newer than the last one it accepted — ${rekey}.`;
}

/** No response arrived within the timeout, after all retries. */
export class RequestTimeout extends Error {
  constructor(
    readonly key: string,
    readonly attempts: number,
  ) {
    super(`${key}: no response after ${attempts} attempt(s)`);
    this.name = "RequestTimeout";
  }
}

interface Pending {
  resolve(payload: Uint8Array): void;
  reject(e: Error): void;
  key: string;
}

export const DEFAULT_TIMEOUT_MS = 1000;

/** A device the page talks to, over whichever link. */
export interface DeviceClient {
  /** What carries the requests. */
  readonly transport: "serial" | "mesh";
  /** True when the device is reached through the cloud, not a cable. */
  readonly remote: boolean;
  /** Sends a request and resolves with the response body; a non-zero status
   * rejects with `RequestError` (`DeniedError` for Denied), no answer with
   * `RequestTimeout`. */
  request(key: string, body?: Uint8Array, opts?: RequestOptions): Promise<Uint8Array>;
  info(opts?: RequestOptions): Promise<DeviceInfo>;
  board(opts?: RequestOptions): Promise<BoardInfo | null>;
  manifest(opts?: RequestOptions): Promise<Manifest | null>;
  manifestBody(opts?: RequestOptions): Promise<Uint8Array | null>;
  /** Page `page` of `sys/manifest/cm` (ABI 8), raw; null for NotFound. */
  controlPage(page: number, opts?: RequestOptions): Promise<Uint8Array | null>;
  peers(opts?: RequestOptions): Promise<string[]>;
  peerManifestBody(node: string, opts?: RequestOptions): Promise<Uint8Array | null>;
  bind(body: Uint8Array, opts?: RequestOptions): Promise<void>;
  binding(opts?: RequestOptions): Promise<Uint8Array | null>;
  trustInfo(opts?: RequestOptions): Promise<TrustInfo | null>;
  trustClaim(publicKey: Uint8Array, opts?: RequestOptions): Promise<void>;
  startPayload(opts?: RequestOptions): Promise<void>;
  stopPayload(opts?: RequestOptions): Promise<void>;
  close(): Promise<void>;
}

/** `null` for a NotFound answer, the error otherwise. */
async function orNull<T>(p: Promise<T>): Promise<T | null> {
  try {
    return await p;
  } catch (e) {
    if (e instanceof RequestError && e.statusCode === NOT_FOUND_CODE) return null;
    throw e;
  }
}

/** The typed helpers of a `DeviceClient`, over an abstract `request`. */
export abstract class RequestClient implements DeviceClient {
  abstract readonly transport: "serial" | "mesh";
  abstract readonly remote: boolean;

  constructor(protected readonly proto: Protocol) {}

  abstract request(key: string, body?: Uint8Array, opts?: RequestOptions): Promise<Uint8Array>;
  abstract close(): Promise<void>;

  /** Requests and decodes `sys/info`. */
  async info(opts?: RequestOptions): Promise<DeviceInfo> {
    const body = await this.request(this.proto.constants.keys.info, new Uint8Array(), opts);
    const info = this.proto.decodeDeviceInfo(body);
    if (!info) throw new Error("malformed sys/info response");
    return info;
  }

  /** Reads `sys/board`; null when the firmware predates 0.6 and answers
   * NotFound. */
  async board(opts?: RequestOptions): Promise<BoardInfo | null> {
    const body = await orNull(this.request(this.proto.constants.keys.board, new Uint8Array(), opts));
    if (!body) return null;
    const board = this.proto.decodeBoardInfo(body);
    if (!board) throw new Error("malformed sys/board response");
    return board;
  }

  /** Reads `sys/manifest` (MF4); null when the firmware has none and
   * answers NotFound. */
  async manifest(opts?: RequestOptions): Promise<Manifest | null> {
    const body = await this.manifestBody(opts);
    if (!body) return null;
    const m = this.proto.decodeManifest(body);
    if (!m) throw new Error("malformed sys/manifest response");
    return m;
  }

  /** The raw `sys/manifest` body (what `checkBinding` takes); null for
   * NotFound. */
  manifestBody(opts?: RequestOptions): Promise<Uint8Array | null> {
    return orNull(this.request(this.proto.constants.keys.manifest, new Uint8Array(), opts));
  }

  /** The raw page `page` of `sys/manifest/cm` (decode it with
   * `decodeControlPage`); null for NotFound (no such page, or a node
   * without control points). */
  controlPage(page: number, opts?: RequestOptions): Promise<Uint8Array | null> {
    return orNull(this.request(this.proto.constants.keys.controlModules, this.proto.controlPageRequest(page), opts));
  }

  /** The node ids the board has heard (`sys/peers`, PR2); empty from
   * firmware that does not forward (NotFound). */
  async peers(opts?: RequestOptions): Promise<string[]> {
    const body = await orNull(this.request(this.proto.constants.keys.peers, new Uint8Array(), opts));
    if (!body) return [];
    const ids = this.proto.decodePeers(body);
    if (!ids) throw new Error("malformed sys/peers response");
    return ids;
  }

  /** `node`'s raw `sys/manifest`, asked through the board (PR1,
   * `sys/peer/<node>/manifest`); null when it has none. */
  peerManifestBody(node: string, opts?: RequestOptions): Promise<Uint8Array | null> {
    return orNull(this.request(this.proto.peerKey(node, "manifest"), new Uint8Array(), opts));
  }

  /** Installs a channel binding (`sys/payload/bind`, ABI 7): `body` is
   * `encodeBindBody(record, signature?)`. It applies at the payload's
   * next start. */
  async bind(body: Uint8Array, opts?: RequestOptions): Promise<void> {
    await this.request(this.proto.constants.keys.bind, body, opts);
  }

  /** Reads the stored binding record; null without one (NotFound). */
  binding(opts?: RequestOptions): Promise<Uint8Array | null> {
    return orNull(this.request(this.proto.constants.keys.binding, new Uint8Array(), opts));
  }

  /** Reads `sys/trust/info`; null when the firmware predates trust and
   * answers NotFound (it accepts unsigned payloads only). */
  async trustInfo(opts?: RequestOptions): Promise<TrustInfo | null> {
    const body = await orNull(this.request(TRUST_KEYS.info, new Uint8Array(), opts));
    return body ? parseTrustInfo(body) : null;
  }

  /** `sys/trust/claim`: pins `publicKey` (raw 32 bytes). Rejects with
   * `DeniedError` unless the device is unclaimed or its re-key window is
   * open. */
  async trustClaim(publicKey: Uint8Array, opts?: RequestOptions): Promise<void> {
    await this.request(TRUST_KEYS.claim, claimBody(publicKey), opts);
  }

  /** `sys/payload/start`. */
  async startPayload(opts?: RequestOptions): Promise<void> {
    await this.request(this.proto.constants.keys.start, new Uint8Array(), opts);
  }

  /** `sys/payload/stop`. */
  async stopPayload(opts?: RequestOptions): Promise<void> {
    await this.request(this.proto.constants.keys.stop, new Uint8Array(), opts);
  }
}

/** A device on a serial line (Web Serial, or an in-memory pipe in tests). */
export class SerialClient extends RequestClient {
  readonly transport = "serial" as const;
  readonly remote = false;
  private readonly decoder;
  private readonly pending = new Map<number, Pending>();
  private reader: ReadableStreamDefaultReader<Uint8Array> | null = null;
  private writer: WritableStreamDefaultWriter<Uint8Array> | null = null;
  private loop: Promise<void> | null = null;
  private writes: Promise<void> = Promise.resolve();
  private nextId = 1;
  private closed = false;

  constructor(
    private readonly streams: ByteStreams,
    proto: Protocol,
    private readonly events: ClientEvents = {},
  ) {
    super(proto);
    this.decoder = proto.newDecoder();
  }

  /** Locks the streams and starts reading. Sends one `0x00` so the device
   * drops any partial frame left on the line. */
  start(): void {
    if (this.loop) return;
    this.reader = this.streams.readable.getReader();
    this.writer = this.streams.writable.getWriter();
    this.loop = this.readLoop(this.reader);
    void this.write(new Uint8Array([0])).catch(() => {});
  }

  private async readLoop(reader: ReadableStreamDefaultReader<Uint8Array>): Promise<void> {
    let error: unknown;
    try {
      for (;;) {
        const { value, done } = await reader.read();
        if (done) break;
        if (value) this.dispatch(this.decoder.push(value));
      }
    } catch (e) {
      if (!this.closed) error = e;
    }
    this.failPending(new Error("serial port closed"));
    this.events.onClosed?.(error);
  }

  private dispatch(events: LineEvent[]): void {
    for (const e of events) {
      switch (e.kind) {
        case "text":
          this.events.onText?.(e.text);
          break;
        case "invalid":
          this.events.onInvalid?.(e.reason, e.len);
          break;
        case "publish":
          this.events.onPublish?.(e.key ?? "", e.payload);
          break;
        case "response": {
          const p = this.pending.get(e.id);
          if (!p) break; // stale (answer to a timed-out attempt)
          this.pending.delete(e.id);
          if (e.statusCode === DENIED_CODE) p.reject(new DeniedError(p.key));
          else if (e.statusCode) p.reject(new RequestError(p.key, e.status ?? "error", e.statusCode));
          else p.resolve(e.payload);
          break;
        }
        case "request":
          // Devices do not send requests to the host; ignore.
          break;
      }
    }
  }

  private failPending(err: Error): void {
    for (const p of this.pending.values()) p.reject(err);
    this.pending.clear();
  }

  private write(bytes: Uint8Array): Promise<void> {
    const w = this.writer;
    if (!w) return Promise.reject(new Error("client not started"));
    const next = this.writes.then(() => w.write(bytes));
    this.writes = next.catch(() => {});
    return next;
  }

  private allocId(): number {
    for (let i = 0; i < 255; i++) {
      const id = this.nextId;
      this.nextId = id >= 255 ? 1 : id + 1;
      if (!this.pending.has(id)) return id;
    }
    throw new Error("too many requests in flight");
  }

  private attempt(key: string, body: Uint8Array, timeoutMs: number): Promise<Uint8Array | null> {
    const id = this.allocId();
    const frame = this.proto.encodeRequest(id, key, body);
    return new Promise<Uint8Array | null>((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        resolve(null);
      }, timeoutMs);
      this.pending.set(id, {
        key,
        resolve: (p) => {
          clearTimeout(timer);
          resolve(p);
        },
        reject: (e) => {
          clearTimeout(timer);
          reject(e);
        },
      });
      this.write(frame).catch((e) => {
        clearTimeout(timer);
        this.pending.delete(id);
        reject(e);
      });
    });
  }

  /** Sends a request and resolves with the response body. Each timed-out
   * attempt is resent with a fresh correlation id, so a late answer to an
   * earlier attempt is discarded rather than mistaken for the retry's. */
  async request(key: string, body: Uint8Array = new Uint8Array(), opts: RequestOptions = {}): Promise<Uint8Array> {
    const timeoutMs = opts.timeoutMs ?? DEFAULT_TIMEOUT_MS;
    const attempts = 1 + Math.max(0, opts.retries ?? 1);
    for (let i = 0; i < attempts; i++) {
      const r = await this.attempt(key, body, timeoutMs);
      if (r) return r;
    }
    throw new RequestTimeout(key, attempts);
  }

  /** Publishes on `key` (fire and forget). */
  publish(key: string, payload: Uint8Array): Promise<void> {
    return this.write(this.proto.encodePublish(key, payload));
  }

  /** Stops reading, fails pending requests and releases both stream locks,
   * leaving the streams usable by the next owner (or the port closable). */
  async close(): Promise<void> {
    if (this.closed) return;
    this.closed = true;
    await this.writes;
    try {
      await this.reader?.cancel();
    } catch {
      /* already errored */
    }
    await this.loop;
    this.reader?.releaseLock();
    this.writer?.releaseLock();
    this.reader = null;
    this.writer = null;
  }
}
