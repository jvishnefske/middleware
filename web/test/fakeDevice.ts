// Test doubles: an in-memory serial port and a scripted device built from
// the protocol-wasm encoders and the Rust loader state machine.

import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import type { ByteStreams } from "../src/device/client";
import { loadProtocolSync, type Protocol } from "../src/protocol/wasm";

export function protocol(): Protocol {
  const path = fileURLToPath(new URL("../src/pkg/protocol_wasm_bg.wasm", import.meta.url));
  return loadProtocolSync(readFileSync(path));
}

/** Two cross-connected stream pairs, like the two ends of a serial cable. */
export function serialPair(): { host: ByteStreams; device: ByteStreams } {
  const toDevice = new TransformStream<Uint8Array, Uint8Array>();
  const toHost = new TransformStream<Uint8Array, Uint8Array>();
  return {
    host: { readable: toHost.readable, writable: toDevice.writable },
    device: { readable: toDevice.readable, writable: toHost.writable },
  };
}

export interface ReceivedRequest {
  id: number;
  key: string;
  body: Uint8Array;
}

export interface FakeDeviceScript {
  /** Bytes written before anything else (a ROM boot banner). */
  banner?: string;
  capacity?: number;
  /** Return true to drop a request silently (simulates loss on the line). */
  drop?(req: ReceivedRequest, index: number): boolean;
  /** Return true to process a request but drop its response (loss on the way back). */
  dropResponse?(req: ReceivedRequest, index: number): boolean;
  /** Return a status code to force an error answer instead of running the loader. */
  forceStatus?(req: ReceivedRequest): number | undefined;
  /** Delay before answering, ms. */
  delayMs?: number;
  /** Per-request delay before answering, ms; overrides `delayMs`. */
  delayFor?(req: ReceivedRequest): number | undefined;
}

/** A device speaking the serial protocol with the real loader rules. */
export class FakeDevice {
  readonly received: ReceivedRequest[] = [];
  readonly image: number[] = [];
  readonly published: { key: string; payload: Uint8Array }[] = [];
  activated: { len: number; crc: number } | null = null;
  started = false;
  status = 0;
  private readonly loader;
  private readonly writer: WritableStreamDefaultWriter<Uint8Array>;
  private readonly done: Promise<void>;
  private readonly reader: ReadableStreamDefaultReader<Uint8Array>;
  private index = 0;

  constructor(
    private readonly proto: Protocol,
    streams: ByteStreams,
    private readonly script: FakeDeviceScript = {},
  ) {
    this.loader = proto.newDeviceLoader(script.capacity ?? 65536);
    this.writer = streams.writable.getWriter();
    if (script.banner) void this.writer.write(new TextEncoder().encode(script.banner));
    this.reader = streams.readable.getReader();
    this.done = this.run(this.reader);
  }

  /** Writes raw bytes to the host (noise, log lines). */
  send(bytes: Uint8Array): Promise<void> {
    return this.writer.write(bytes);
  }

  publish(key: string, payload: Uint8Array): Promise<void> {
    return this.send(this.proto.encodePublish(key, payload));
  }

  private async run(reader: ReadableStreamDefaultReader<Uint8Array>): Promise<void> {
    const decoder = this.proto.newDecoder();
    for (;;) {
      const { value, done } = await reader.read().catch(() => ({ value: undefined, done: true as const }));
      if (done) return;
      for (const e of decoder.push(value)) {
        if (e.kind === "publish") this.published.push({ key: e.key ?? "", payload: e.payload });
        if (e.kind !== "request") continue;
        const req = { id: e.id, key: e.key ?? "", body: e.payload };
        this.received.push(req);
        const i = this.index++;
        if (this.script.drop?.(req, i)) continue;
        await this.answer(req, this.script.dropResponse?.(req, i) ?? false);
      }
    }
  }

  private async answer(req: ReceivedRequest, silent: boolean): Promise<void> {
    const delay = this.script.delayFor?.(req) ?? this.script.delayMs;
    if (delay) await new Promise((r) => setTimeout(r, delay));
    const forced = this.script.forceStatus?.(req);
    if (forced) return this.send(this.proto.encodeResponse(req.id, forced, new Uint8Array()));
    const r = this.loader.step(req.key, req.body);
    let body: Uint8Array = new Uint8Array();
    if (r.statusCode === 0) {
      switch (r.action) {
        case "info":
          body = this.proto.encodeDeviceInfo({
            abiVersion: this.proto.abi.version,
            capacity: this.script.capacity ?? 65536,
            status: this.status,
            chip: "esp32c3",
            firmware: "0.1.0-fake",
          });
          break;
        case "erase":
          this.image.length = 0;
          this.status = 4;
          break;
        case "write":
          this.image.push(...(r.data ?? []));
          break;
        case "activate":
          this.activated = { len: r.len ?? 0, crc: r.crc ?? 0 };
          this.status = 1;
          break;
        case "start":
          this.started = true;
          this.status = 2;
          break;
        case "stop":
          this.status = 1;
          break;
        default:
          break;
      }
    }
    if (silent) return;
    await this.send(this.proto.encodeResponse(req.id, r.statusCode, body));
    if (r.statusCode === 0 && (r.action === "activate" || r.action === "start" || r.action === "stop")) {
      await this.publish(this.proto.constants.keys.status, new Uint8Array([this.status]));
    }
  }

  async close(): Promise<void> {
    await this.reader.cancel().catch(() => {});
    await this.writer.close().catch(() => {});
    await this.done;
  }
}
