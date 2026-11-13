// The plant's network as a recipe run sees it (design.md Q19, Q23): every
// unit is either a simulated session on a SimBus (bus.ts) or a real board
// behind a `UnitLink` (device/requester.ts: Web Serial), and the run talks
// to both through one `PlantNet`. DOM-free.
//
// * `request(unit, key, body, meta)` answers synchronously for a
//   simulated unit (SimBus.request, publications deferred to `flush`) and
//   with a Promise for a board; `null` (or a Promise of null) when the
//   request is lost: the unit is offline or nothing takes the key, and the
//   engine's acknowledgement deadline notices, as on a real network.
// * `onPublish` hears every unit's publications: the SimBus's and each
//   board's (the firmware forwards a payload's publications over USB),
//   which a run filters for the phases' `/state` reports.
// * `onPresence` hears a board leaving or coming back by itself (the
//   cable pulled: the port closed); `setOnline` is the page's Lose / Find,
//   which for a board only stops or resumes talking to it.
// * A board's publications are not delivered to the simulated units'
//   subscriptions (the units of the worked example talk only to the
//   engine); the SimBus still delivers between simulated units.

import type { RequestResult } from "../protocol/wasm";
import { type PublishListener, SimBus } from "./bus";
import type { RequestMetaIn } from "./session";

/** What a request comes back with: the result, or null when lost. */
export type NetReply = RequestResult | null;

/** One real unit: a board the page talks to. */
export interface UnitLink {
  /** Whether the board is connected and answers. */
  readonly connected: boolean;
  /** Its node id (16 lower-case hex digits), from `sys/info`. */
  readonly node: string;
  request(key: string, body: Uint8Array, meta: RequestMetaIn): Promise<NetReply>;
  /** Its publications (key, body); returns the unsubscribe. */
  onPublish(f: (key: string, data: Uint8Array) => void): () => void;
  /** Called when the board leaves (false) or returns (true). */
  onPresence(f: (connected: boolean) => void): () => void;
}

export type PresenceListener = (unit: string, online: boolean) => void;

export interface PlantNet {
  /** Whether requests to `unit` go out now. */
  online(unit: string): boolean;
  /** The node id `unit` answers to, if the network knows it. */
  node(unit: string): string | null;
  /** Whether `unit` is simulated (false: a real board). */
  simulated(unit: string): boolean;
  request(unit: string, key: string, body: Uint8Array, meta: RequestMetaIn): NetReply | Promise<NetReply>;
  onPublish(f: PublishListener): () => void;
  onPresence(f: PresenceListener): () => void;
  /** Lose (false) or find (true) a unit from the page. */
  setOnline(unit: string, online: boolean): void;
  /** One main-loop pass of the simulated units. */
  step(): void;
  /** Publications the simulated units queued during a request. */
  flush(): void;
}

/** Simulated units on a SimBus, plus real boards attached by unit id. */
export class MixedNet implements PlantNet {
  private readonly links = new Map<string, { link: UnitLink; online: boolean; off: (() => void)[] }>();
  private readonly publishers: PublishListener[] = [];
  private readonly presence: PresenceListener[] = [];

  constructor(readonly bus: SimBus) {}

  /** Makes `unit` the board behind `link` (instead of a bus session). */
  attach(unit: string, link: UnitLink): void {
    if (this.links.has(unit)) throw new Error(`unit ${unit} already has a board`);
    if (this.bus.unit(unit)) throw new Error(`unit ${unit} is already simulated`);
    const entry = { link, online: true, off: [] as (() => void)[] };
    entry.off.push(
      link.onPublish((key, data) => {
        for (const f of [...this.publishers]) f(unit, key, data);
      }),
      link.onPresence((connected) => {
        for (const f of [...this.presence]) f(unit, connected);
      }),
    );
    this.links.set(unit, entry);
  }

  /** The units with a board. */
  get boards(): string[] {
    return [...this.links.keys()];
  }

  online(unit: string): boolean {
    const l = this.links.get(unit);
    return l ? l.online && l.link.connected : this.bus.online(unit);
  }

  node(unit: string): string | null {
    return this.links.get(unit)?.link.node ?? this.bus.unit(unit)?.session.nodeId ?? null;
  }

  simulated(unit: string): boolean {
    return !this.links.has(unit);
  }

  request(unit: string, key: string, body: Uint8Array, meta: RequestMetaIn): NetReply | Promise<NetReply> {
    const l = this.links.get(unit);
    if (l) return this.online(unit) ? l.link.request(key, body, meta) : null;
    if (!this.bus.online(unit)) return null;
    return this.bus.request(key, body, meta, true)?.result ?? null;
  }

  onPublish(f: PublishListener): () => void {
    const offBus = this.bus.onPublish(f);
    this.publishers.push(f);
    return () => {
      offBus();
      const i = this.publishers.indexOf(f);
      if (i >= 0) this.publishers.splice(i, 1);
    };
  }

  onPresence(f: PresenceListener): () => void {
    this.presence.push(f);
    return () => {
      const i = this.presence.indexOf(f);
      if (i >= 0) this.presence.splice(i, 1);
    };
  }

  setOnline(unit: string, online: boolean): void {
    const l = this.links.get(unit);
    if (l) l.online = online;
    else this.bus.setOnline(unit, online);
  }

  step(): void {
    this.bus.step();
  }

  flush(): void {
    this.bus.flush();
  }

  /** Detaches the boards (the bus is the caller's). */
  dispose(): void {
    for (const l of this.links.values()) for (const off of l.off) off();
    this.links.clear();
    this.publishers.length = 0;
    this.presence.length = 0;
  }
}

/** `net` as a PlantNet: a SimBus alone becomes a MixedNet without boards. */
export function plantNet(net: SimBus | PlantNet): PlantNet {
  return net instanceof SimBus ? new MixedNet(net) : net;
}
