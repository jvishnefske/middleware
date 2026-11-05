// Test doubles: an in-memory serial port and a scripted device built from
// the protocol-wasm encoders and the Rust loader state machine, plus
// `FakeTrust`, a TypeScript model of the device's trust contract
// (`sys/trust/*` and the signed commit) for the U13 tests.
//
// With `runs` the fake is a board that runs what it was sent and routes
// requests on served keys to it, as rp-base and esp-base do since M7
// (`esp_base::serve`): `sys/payload/start` loads the uploaded image into a
// browser-simulator session (on the script's clock, with the script's
// node id), `sys/info` reports that node and the capability bits (1:
// requests reach the payload), a request on a key the running payload
// serves goes to its `on_request` and the payload's publications go out as
// publish frames after the response; a stopped payload's keys answer
// Unavailable, other application keys NotFound, `sys/` keys the loader.

import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import type { ByteStreams } from "../src/device/client";
import { loadProtocolSync, type Protocol } from "../src/protocol/wasm";
import { type Clock, SimSession } from "../src/sim/session";
import { sha256, verifySignature } from "../src/trust/crypto";
import {
  DENIED_CODE,
  PUBLIC_KEY_LEN,
  TRUST_KEYS,
  type TrustState,
  TrustFormatError,
  encodeTrustInfo,
  parseCommitBody,
  signedMessage,
} from "../src/trust/wire";

const NOT_FOUND = 1;
const INVALID_REQUEST = 2;
/** `HandlerError::code` by name (middleware transport/reqrep.rs). */
const HANDLER_CODES: Record<string, number> = {
  NotFound: 1,
  InvalidRequest: 2,
  BufferTooSmall: 3,
  Internal: 4,
  Denied: 5,
  Unavailable: 6,
  DeadlineExceeded: 7,
};
export const BOOT_HOLD_MS = 3000;
export const REKEY_WINDOW_MS = 60_000;

/** The device's root of trust as the contract describes it. */
export class FakeTrust {
  key: Uint8Array | null = null;
  minVersion = 0;
  private windowUntil = Number.NEGATIVE_INFINITY;

  constructor(private readonly now: () => number = Date.now) {}

  get state(): TrustState {
    if (!this.key) return "unclaimed";
    return this.now() < this.windowUntil ? "rekey-window" : "claimed";
  }

  /** BOOT held for `ms` after boot: >= 3 s opens the 60 s re-key window. */
  holdBoot(ms: number): void {
    if (ms >= BOOT_HOLD_MS) this.windowUntil = this.now() + REKEY_WINDOW_MS;
  }

  info(): Uint8Array {
    return encodeTrustInfo({ state: this.state, minVersion: this.minVersion, key: this.key });
  }

  /** Status code for `sys/trust/claim`. */
  claim(body: Uint8Array): number {
    if (body.length !== PUBLIC_KEY_LEN) return INVALID_REQUEST;
    const s = this.state;
    if (s === "claimed") return DENIED_CODE;
    this.key = body.slice();
    // As the device does (loader::trust): a new key starts over at 0.
    this.minVersion = 0;
    this.windowUntil = Number.NEGATIVE_INFINITY;
    return 0;
  }

  /** Trust check for `sys/loader/commit` of `image`: the status code and,
   * for an accepted signed commit, the version to record once the loader
   * has activated the image. */
  async checkCommit(body: Uint8Array, image: Uint8Array): Promise<{ status: number; version?: number }> {
    if (!this.key) return { status: 0 }; // unclaimed: anything goes
    let parsed;
    try {
      parsed = parseCommitBody(body);
    } catch (e) {
      if (e instanceof TrustFormatError) return { status: DENIED_CODE };
      throw e;
    }
    if (!parsed || parsed.version <= this.minVersion) return { status: DENIED_CODE };
    const m = signedMessage(image.length, parsed.version, await sha256(image));
    if (!(await verifySignature(this.key, m, parsed.signature))) return { status: DENIED_CODE };
    return { status: 0, version: parsed.version };
  }

  /** The image committed with `version` is active: refuse it from now on. */
  accept(version: number | undefined): void {
    if (version !== undefined) this.minVersion = version;
  }
}

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
  /** Trust support; without it the device predates `sys/trust` and
   * answers those keys NotFound. */
  trust?: FakeTrust;
  /** `sys/board` body (e.g. `bsp_export --wire`); without it the device
   * predates 0.6 and answers NotFound. */
  board?: Uint8Array;
  /** Runs uploaded payloads and routes served keys to them (see above). */
  runs?: FakeRuntime;
}

export interface FakeRuntime {
  /** The session's clock (steps every 10 ms). */
  clock: Clock;
  /** Node id (16 hex digits) for `sys/info` and the payload's `node_id`. */
  node: string;
  /** Capability bits in `sys/info` (default 1: requests are routed). */
  caps?: number;
  /** Called with each session the board starts (to set analog levels). */
  onSession?(s: SimSession): void;
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
  /** The running payload (with `runs`). */
  session: SimSession | null = null;
  /** The keys the last run served. */
  private lastServes: string[] = [];
  /** Publications held while a request is being answered. */
  private holding: Uint8Array[] | null = null;
  private closed = false;

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

  /** A payload publication: after the response being sent, if any. */
  private payloadPublish(key: string, data: Uint8Array): void {
    if (this.closed) return;
    const frame = this.proto.encodePublish(key, data);
    if (this.holding) this.holding.push(frame);
    else void this.send(frame).catch(() => {});
  }

  private startSession(rt: FakeRuntime): void {
    this.stopSession();
    const s = new SimSession(this.proto, Uint8Array.from(this.image), { onEvent: (e) => e.kind === "publish" && this.payloadPublish(e.key, e.data) }, rt.clock, { node: rt.node });
    rt.onSession?.(s);
    s.start(10);
    this.session = s;
  }

  private stopSession(): void {
    if (!this.session) return;
    this.lastServes = this.session.serves;
    this.session.dispose();
    this.session = null;
  }

  /** The firmware's routing (`esp_base::serve::route_request`) for an
   * application key: the payload's answer, or the error code. */
  private serve(req: ReceivedRequest): { code: number; body: Uint8Array } {
    const errors = HANDLER_CODES;
    const s = this.session;
    if (s && !s.trapped && s.serves.includes(req.key)) {
      const r = s.request(req.key, req.body);
      return r.ok ? { code: 0, body: r.reply } : { code: errors[r.error] ?? errors.Internal!, body: new Uint8Array() };
    }
    const known = (s?.serves ?? []).includes(req.key) || this.lastServes.includes(req.key);
    return { code: known ? errors.Unavailable! : NOT_FOUND, body: new Uint8Array() };
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
    if (this.script.runs && !req.key.startsWith("sys/")) {
      this.holding = [];
      const r = this.serve(req);
      const held = this.holding;
      this.holding = null;
      if (!silent) await this.send(this.proto.encodeResponse(req.id, r.code, r.body));
      for (const f of held) await this.send(f).catch(() => {});
      return;
    }
    let loaderBody = req.body;
    let accepted: number | undefined;
    const trust = this.script.trust;
    if (!trust && req.key.startsWith("sys/trust/")) {
      // Firmware that predates trust: the key is unknown.
      return silent ? undefined : this.send(this.proto.encodeResponse(req.id, NOT_FOUND, new Uint8Array()));
    }
    if (trust) {
      const fail = (code: number) => this.send(this.proto.encodeResponse(req.id, code, new Uint8Array()));
      if (req.key === TRUST_KEYS.info) {
        return silent ? undefined : this.send(this.proto.encodeResponse(req.id, 0, trust.info()));
      }
      if (req.key === TRUST_KEYS.claim) {
        const code = trust.claim(req.body);
        return silent ? undefined : this.send(this.proto.encodeResponse(req.id, code, new Uint8Array()));
      }
      if (req.key === this.proto.constants.keys.commit) {
        // Trust check first; the Rust loader then sees the unsigned commit.
        const check = await trust.checkCommit(req.body, Uint8Array.from(this.image));
        if (check.status) return silent ? undefined : fail(check.status);
        accepted = check.version;
        loaderBody = new Uint8Array();
      }
    }
    const r = this.loader.step(req.key, loaderBody);
    let body: Uint8Array = new Uint8Array();
    if (r.statusCode === 0) {
      switch (r.action) {
        case "info":
          body = this.proto.encodeDeviceInfo({
            abiVersion: this.proto.abi.version,
            capacity: this.script.capacity ?? 65536,
            status: this.status,
            chip: this.script.runs ? "rp2350" : "esp32c3",
            firmware: "0.1.0-fake",
            ...(this.script.runs ? { node: this.script.runs.node, caps: this.script.runs.caps ?? 1 } : {}),
          });
          break;
        case "board":
          if (!this.script.board) {
            return silent ? undefined : this.send(this.proto.encodeResponse(req.id, NOT_FOUND, new Uint8Array()));
          }
          body = this.script.board;
          break;
        case "erase":
          this.stopSession();
          this.lastServes = [];
          this.image.length = 0;
          this.status = 4;
          break;
        case "write":
          this.image.push(...(r.data ?? []));
          break;
        case "activate":
          this.stopSession();
          this.lastServes = [];
          trust?.accept(accepted);
          this.activated = { len: r.len ?? 0, crc: r.crc ?? 0 };
          this.status = 1;
          break;
        case "start":
          this.started = true;
          this.status = 2;
          if (this.script.runs) this.startSession(this.script.runs);
          break;
        case "stop":
          this.stopSession();
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

  /** The cable is pulled: the payload stops with the board, both
   * directions end. */
  async close(): Promise<void> {
    this.closed = true;
    this.stopSession();
    await this.reader.cancel().catch(() => {});
    await this.writer.close().catch(() => {});
    await this.done;
  }
}
