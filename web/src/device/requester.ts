// A real board as a unit of a recipe run (design.md Q23): the Web Serial
// requester. It has the request shape of the simulator's SimBus
// (`request(key, body, meta) -> RequestResult`, here as a Promise) over the
// serial protocol's `DeviceClient`, and feeds the board's publications
// (the phases' `/state` reports, which the firmware forwards over USB) to
// the run. DOM-free; the client is anything with DeviceClient's
// `request`, so the tests use the in-memory fake device.
//
// * A request goes out once (no retries: the engine retries by level)
//   with the command's acknowledgement deadline as the response timeout.
//   The device's `HandlerError` comes back by name (`RequestError.status`),
//   no answer within the deadline is `DeadlineExceeded`, and a failure to
//   send (the port is gone) is lost (null).
// * Publications are handed on in a later task (setTimeout 0), after the
//   promise chain of the response that came before them on the wire, so
//   the run takes a command's reply before the reports it caused, as with
//   the simulator.
// * `closed()` (the Device tab calls it when the port closes: cable
//   pulled, Disconnect) marks the board gone and tells the run, which
//   treats it as a lost unit; `reopened(...)` brings it back.
// * `canServe(info)`: a board serves phases only when its `sys/info`
//   carries the capability bit "requests are routed to the payload"
//   (`serves`) and a node id. `serveProblem` says in plain words why not.

import type { DeviceInfo, RequestResult } from "../protocol/wasm";
import type { NetReply, UnitLink } from "../sim/net";
import type { RequestMetaIn } from "../sim/session";
import { type RequestOptions, RequestError, RequestTimeout } from "./client";

/** The part of DeviceClient a requester needs. */
export interface RequestClient {
  request(key: string, body: Uint8Array, opts?: RequestOptions): Promise<Uint8Array>;
}

/** Response timeout when a request carries no deadline. */
export const DEFAULT_DEADLINE_MS = 1000;

/** Whether a board whose `sys/info` is `info` can serve S88 phases. */
export function canServe(info: Pick<DeviceInfo, "serves" | "node">): boolean {
  return info.serves && !!info.node;
}

/** Why a board cannot serve phases, in plain words; null when it can. */
export function serveProblem(info: Pick<DeviceInfo, "serves" | "node" | "chip" | "firmware">): string | null {
  if (canServe(info)) return null;
  const board = `${info.chip} ${info.firmware}`;
  if (!info.node) return `This board (${board}) does not report a node id, so phase keys cannot name it. Update its firmware on the Flash tab.`;
  return `This board (${board}) cannot serve phases: its firmware does not pass requests to the payload. Update its firmware on the Flash tab, or run this unit in the simulator.`;
}

export class SerialRequester implements UnitLink {
  private alive = true;
  private readonly publishers: ((key: string, data: Uint8Array) => void)[] = [];
  private readonly presence: ((connected: boolean) => void)[] = [];

  readonly node: string;

  /** `info` is the board's `sys/info` (its node id names the phase keys). */
  constructor(
    private client: RequestClient,
    private latest: DeviceInfo,
  ) {
    if (!latest.node) throw new Error(serveProblem(latest) ?? "no node id");
    this.node = latest.node.toLowerCase();
  }

  /** The board's latest `sys/info`. */
  get info(): DeviceInfo {
    return this.latest;
  }

  get connected(): boolean {
    return this.alive;
  }

  async request(key: string, body: Uint8Array, meta: RequestMetaIn): Promise<NetReply> {
    if (!this.alive) return null;
    const timeoutMs = Math.max(1, meta.deadlineMs ?? DEFAULT_DEADLINE_MS);
    try {
      const reply = await this.client.request(key, body, { timeoutMs, retries: 0 });
      return { ok: true, reply } satisfies RequestResult;
    } catch (e) {
      if (e instanceof RequestError) return { ok: false, error: e.status };
      if (e instanceof RequestTimeout) return { ok: false, error: "DeadlineExceeded" };
      return null;
    }
  }

  /** A publication from the board (the owner of the client's `onPublish`
   * calls this for every one). */
  publication(key: string, data: Uint8Array): void {
    if (!this.alive) return;
    setTimeout(() => {
      for (const f of [...this.publishers]) f(key, data);
    }, 0);
  }

  onPublish(f: (key: string, data: Uint8Array) => void): () => void {
    this.publishers.push(f);
    return () => {
      const i = this.publishers.indexOf(f);
      if (i >= 0) this.publishers.splice(i, 1);
    };
  }

  onPresence(f: (connected: boolean) => void): () => void {
    this.presence.push(f);
    return () => {
      const i = this.presence.indexOf(f);
      if (i >= 0) this.presence.splice(i, 1);
    };
  }

  /** The port closed: the board is gone (once). */
  closed(): void {
    if (!this.alive) return;
    this.alive = false;
    for (const f of [...this.presence]) f(false);
  }

  /** The same board (same node) is connected again over `client`. */
  reopened(client: RequestClient, info: DeviceInfo): boolean {
    if (this.alive || info.node?.toLowerCase() !== this.node) return false;
    this.client = client;
    this.latest = info;
    this.alive = true;
    for (const f of [...this.presence]) f(true);
    return true;
  }
}
