// The browser simulator's network: one SimSession per unit and a bus
// between them (like payload-sim's --peer boards, for any number of
// units). DOM-free; the clock is injected so tests run in virtual time.
//
// * Publications: every publish of an online unit's payload reaches the
//   other online units' subscriptions (`deliver`) and the bus's
//   listeners (`onPublish`: a recipe run reads the phases' `/state`
//   reports there). Deliveries can publish again; the bus queues them and
//   handles at most MAX_HOPS per drain, counting the rest in `dropped`.
//   Publications are queued while the publishing simulator is inside a
//   call and handled after it returns, so a listener may call into any
//   session (a recipe run's request to the unit that just reported).
// * Requests: `request(key, body, meta)` goes to the online unit whose
//   node id the key names (`unit/<node>/...`), else to the online unit
//   serving the key exactly; `null` when no online unit takes it (the
//   request is lost, as on a network: the caller's deadline notices).
// * Online: `setOnline(unit, false)` takes a unit off the network (its
//   payload keeps running; its publications and requests to it are lost)
//   for "lose unit" in the recipe run panel; `true` brings it back.
// * Node ids: a unit's node is the one given (a bound board's), else
//   `defaultNode(i)`: 0000000000000001, ...02, ... in the order units
//   were added, so every session's phase keys are distinct.

import type { Protocol, RequestResult, SimEvent } from "../protocol/wasm";
import { type Clock, type RequestMetaIn, type SessionEvents, SimSession, realClock } from "./session";

/** Most queued deliveries one publication may cause. */
export const MAX_HOPS = 64;

/** The default node id of the `i`th unit (0-based). */
export function defaultNode(i: number): string {
  return (i + 1).toString(16).padStart(16, "0");
}

/** The node id a phase key names (`unit/<16 hex>/...`), lower case. */
export function keyNode(key: string): string | null {
  const m = /^unit\/([0-9a-fA-F]{16})\//.exec(key);
  return m ? m[1]!.toLowerCase() : null;
}

export interface BusUnit {
  readonly id: string;
  readonly session: SimSession;
  online: boolean;
}

export type PublishListener = (unit: string, key: string, data: Uint8Array) => void;

export class SimBus {
  private readonly units: BusUnit[] = [];
  private readonly listeners: PublishListener[] = [];
  private queue: { from: string; key: string; data: Uint8Array }[] = [];
  private draining = false;
  private stopTimer: (() => void) | null = null;
  /** Deliveries dropped past MAX_HOPS. */
  dropped = 0;

  constructor(
    private readonly proto: Protocol,
    private readonly clock: Clock = realClock,
  ) {}

  /** Loads a unit's payload on a new session (node `node` or the next
   * default) and joins it to the bus. Throws if the id is taken or
   * wasm-host rejects the payload. */
  add(id: string, wasm: Uint8Array, opts: { node?: string | null; events?: SessionEvents } = {}): SimSession {
    if (this.unit(id)) throw new Error(`unit ${id} is already on the bus`);
    const node = (opts.node ?? defaultNode(this.units.length)).toLowerCase();
    if (this.units.some((u) => u.session.nodeId === node)) throw new Error(`node ${node} is already on the bus`);
    const events = opts.events ?? {};
    // What `init` publishes arrives before the unit is on the list.
    const early: { from: string; key: string; data: Uint8Array }[] = [];
    const session = new SimSession(
      this.proto,
      wasm,
      {
        ...events,
        onEvent: (e: SimEvent) => {
          if (e.kind === "publish") {
            if (this.unit(id)) this.publish(id, e.key, e.data);
            else early.push({ from: id, key: e.key, data: e.data });
          }
          events.onEvent?.(e);
        },
      },
      this.clock,
      { node },
    );
    this.units.push({ id, session, online: true });
    this.queue.push(...early);
    this.drain();
    return session;
  }

  unit(id: string): BusUnit | undefined {
    return this.units.find((u) => u.id === id);
  }

  get all(): readonly BusUnit[] {
    return this.units;
  }

  onPublish(f: PublishListener): () => void {
    this.listeners.push(f);
    return () => {
      const i = this.listeners.indexOf(f);
      if (i >= 0) this.listeners.splice(i, 1);
    };
  }

  setOnline(id: string, online: boolean): boolean {
    const u = this.unit(id);
    if (!u) return false;
    u.online = online;
    return true;
  }

  online(id: string): boolean {
    return this.unit(id)?.online ?? false;
  }

  /** Called from a session's event callback, i.e. while that session's
   * simulator is inside a call: only queue (calling back into it now
   * would be a re-entrant use of the wasm object). `step`, `request` and
   * `add` drain once the call has returned. */
  private publish(from: string, key: string, data: Uint8Array): void {
    if (!this.online(from)) return;
    this.queue.push({ from, key, data });
  }

  private drain(): void {
    if (this.draining) return;
    this.draining = true;
    try {
      let hops = 0;
      while (this.queue.length > 0) {
        const m = this.queue.shift()!;
        if (hops++ >= MAX_HOPS) {
          this.dropped += 1 + this.queue.length;
          this.queue = [];
          break;
        }
        for (const f of [...this.listeners]) f(m.from, m.key, m.data);
        for (const u of this.units) {
          if (u.id !== m.from && u.online && !u.session.trapped) u.session.deliver(m.key, m.data);
        }
      }
    } finally {
      this.draining = false;
    }
  }

  /** The online unit a request for `key` goes to, if any. */
  route(key: string): BusUnit | undefined {
    const node = keyNode(key);
    const live = this.units.filter((u) => u.online);
    return (node ? live.find((u) => u.session.nodeId === node) : undefined) ?? live.find((u) => u.session.serves.includes(key));
  }

  /** Hands a request to the unit `route` picks; `null` when none (lost).
   * What the unit publishes meanwhile is handled before this returns,
   * unless `deferPublications` (then at the caller's `flush`, so a caller
   * can take the reply in before the reports it caused). */
  request(key: string, body: Uint8Array, meta: RequestMetaIn = {}, deferPublications = false): { unit: string; result: RequestResult } | null {
    const u = this.route(key);
    if (!u) return null;
    const result = u.session.request(key, body, meta);
    if (!deferPublications) this.drain();
    return { unit: u.id, result };
  }

  /** Handles queued publications now. */
  flush(): void {
    this.drain();
  }

  /** One main-loop pass on every session (offline ones too: a unit off
   * the network still runs its payload). */
  step(): void {
    for (const u of this.units) if (!u.session.trapped) u.session.step();
    this.drain();
  }

  start(pollMs = 10): void {
    if (this.stopTimer) return;
    this.stopTimer = this.clock.every(pollMs, () => this.step());
  }

  stop(): void {
    this.stopTimer?.();
    this.stopTimer = null;
  }

  get running(): boolean {
    return this.stopTimer !== null;
  }

  dispose(): void {
    this.stop();
    for (const u of this.units) u.session.dispose();
    this.units.length = 0;
    this.listeners.length = 0;
  }
}
