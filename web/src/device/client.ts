// Host side of the serial protocol over a pair of byte streams.
//
// `DeviceClient` owns a reader loop that feeds the protocol-wasm decoder,
// matches responses to requests by correlation id, and reports console text,
// publications and rejected segments through callbacks. It knows nothing
// about Web Serial or the DOM: it takes a ReadableStream/WritableStream pair,
// which is a SerialPort's `readable`/`writable` in the browser and an
// in-memory pipe in the tests.

import type { DeviceInfo, LineEvent, Protocol } from "../protocol/wasm";

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

export class DeviceClient {
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
    private readonly proto: Protocol,
    private readonly events: ClientEvents = {},
  ) {
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
          if (e.statusCode) p.reject(new RequestError(p.key, e.status ?? "error", e.statusCode));
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

  /** Requests and decodes `sys/info`. */
  async info(opts?: RequestOptions): Promise<DeviceInfo> {
    const body = await this.request(this.proto.constants.keys.info, new Uint8Array(), opts);
    const info = this.proto.decodeDeviceInfo(body);
    if (!info) throw new Error("malformed sys/info response");
    return info;
  }

  /** `sys/payload/start`. */
  async startPayload(opts?: RequestOptions): Promise<void> {
    await this.request(this.proto.constants.keys.start, new Uint8Array(), opts);
  }

  /** `sys/payload/stop`. */
  async stopPayload(opts?: RequestOptions): Promise<void> {
    await this.request(this.proto.constants.keys.stop, new Uint8Array(), opts);
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
