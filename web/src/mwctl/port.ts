// mwctl's serial port in the page: a Web Serial port (or anything shaped
// like one) as the `PortHost` behind `--port web`.
//
// A pump reads the port all the time into a queue, as an operating
// system's serial driver would: mwctl reads with a timeout, a stream read
// cannot be abandoned without losing bytes, and a Web Serial port that
// nobody reads overruns its small buffer. A chunk larger than the
// caller's buffer is handed out in pieces.

import type { PortHost } from "./host";

/** The part of `SerialPort` mwctl needs. */
export interface SerialLike {
  open(options: { baudRate: number }): Promise<void>;
  close(): Promise<void>;
  setSignals?(signals: { dataTerminalReady?: boolean; requestToSend?: boolean }): Promise<void>;
  readonly readable: ReadableStream<Uint8Array> | null;
  readonly writable: WritableStream<Uint8Array> | null;
}

/** The path mwctl names the page's port by. */
export const WEB_PORT = "web";

const OK = 0;
const NO_PORT = -1;
const LOST = -2;
const FAILED = -3;

export class WebPort implements PortHost {
  private reader: ReadableStreamDefaultReader<Uint8Array> | null = null;
  private writer: WritableStreamDefaultWriter<Uint8Array> | null = null;
  private pump: Promise<void> | null = null;
  private queue: Uint8Array[] = [];
  /** Wakes the read that is waiting for the pump. */
  private wake: (() => void) | null = null;
  private ended = false;
  private isOpen = false;

  constructor(
    private readonly target: SerialLike,
    private readonly sleep: (ms: number) => Promise<void> = (ms) => new Promise((r) => setTimeout(r, ms)),
  ) {}

  async open(path: string, baud: number): Promise<number> {
    if (path !== WEB_PORT) return NO_PORT;
    try {
      await this.target.open({ baudRate: baud });
    } catch {
      return FAILED;
    }
    this.isOpen = true;
    // Both lines released, as mwctl's own port does: on boards with the
    // DTR/RTS auto-reset circuit the chip keeps running.
    await this.target.setSignals?.({ dataTerminalReady: false, requestToSend: false }).catch(() => {});
    if (!this.target.readable || !this.target.writable) return FAILED;
    const reader = this.target.readable.getReader();
    this.reader = reader;
    this.writer = this.target.writable.getWriter();
    this.queue = [];
    this.ended = false;
    this.pump = (async () => {
      try {
        for (;;) {
          const { value, done } = await reader.read();
          if (done) break;
          if (value.length > 0) this.queue.push(value);
          this.wake?.();
        }
      } catch {
        /* the port went away */
      }
      this.ended = true;
      this.wake?.();
    })();
    return OK;
  }

  async write(bytes: Uint8Array): Promise<number> {
    if (!this.writer) return LOST;
    try {
      await this.writer.write(bytes);
      return OK;
    } catch {
      return LOST;
    }
  }

  async read(cap: number, timeoutMs: number): Promise<Uint8Array | number> {
    if (!this.reader) return LOST;
    if (this.queue.length === 0 && !this.ended) {
      await Promise.race([new Promise<void>((r) => (this.wake = r)), this.sleep(timeoutMs)]);
      this.wake = null;
    }
    const head = this.queue[0];
    if (!head) return this.ended ? LOST : new Uint8Array(0);
    if (head.length <= cap) {
      this.queue.shift();
      return head;
    }
    this.queue[0] = head.subarray(cap);
    return head.subarray(0, cap);
  }

  async signals(dtr: boolean, rts: boolean): Promise<number> {
    if (!this.isOpen) return LOST;
    if (!this.target.setSignals) return OK;
    try {
      await this.target.setSignals({ dataTerminalReady: dtr, requestToSend: rts });
      return OK;
    } catch {
      return FAILED;
    }
  }

  async close(): Promise<void> {
    if (!this.isOpen) return;
    this.isOpen = false;
    const { reader, writer, pump } = this;
    this.reader = null;
    this.writer = null;
    this.pump = null;
    await reader?.cancel().catch(() => {});
    await pump;
    reader?.releaseLock();
    // Every write was awaited, so the lock can go without closing the
    // stream (closing it would end a stream the port may hand out again).
    writer?.releaseLock();
    await this.target.close().catch(() => {});
  }
}
