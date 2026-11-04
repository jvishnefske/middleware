// A control recipe run in the browser (design.md Q19): recipe-core's
// engine (protocol-wasm `RecipeRun`) driving the plant's units as
// simulated payloads on a SimBus (bus.ts). DOM-free; the clock is
// injected so tests run in virtual time.
//
//   operator / lose / find / tick ──► RecipeRun.on ──► effects
//        ▲                                  │ send {key, body, timeoutMs}
//        │ reply                             ▼
//        └──────────── SimBus.request ──► the unit's SimSession (on_request)
//   SimBus publications on ".../state" ──► RecipeRun.on(report)
//
// Inputs are queued and handled one at a time (a request's reply and the
// reports it publishes arrive while the engine's effects are being
// carried out); one drain handles at most MAX_INPUTS, then traces that
// the inputs did not settle. A request to a unit that is offline (or that
// no unit serves) is lost: the engine's acknowledgement deadline notices,
// as on a real network. `start` runs every unit's main loop and the
// engine's clock every `pollMs` (10 ms: the payload-sim tests' step).
//
// `operatorButtons` is the pure part the page's Start/Hold/... buttons
// use: a command is enabled exactly when the phase model accepts it in the
// procedure's state.

import { buildPayload } from "../blocks/pipeline";
import type { AbiInfo, Protocol, RecipeRunHandle, RunConfig, RunEffect, RunInput, RunUnit, StepStatus } from "../protocol/wasm";
import { flowsOf, profileOfModel, workspaceOf } from "../model/payload";
import { type PlantModel, allocationUnits, phaseSpecs, unitPayload } from "../model/plant";
import type { MasterRecipe } from "../model/recipe";
import { PHASE_COMMANDS } from "../model/procedural";
import { type SimBus, defaultNode } from "./bus";
import type { Clock } from "./session";

/** Most inputs one drain handles. */
export const MAX_INPUTS = 256;

export interface TraceEntry {
  t: number;
  type: string;
  text: string;
}

/** The phase model as protocol-wasm `phaseModel()` gives it. */
export interface PhaseModelTable {
  states: string[];
  commands: string[];
  next: (string | null)[][];
}

/** Each operator command and whether the phase model accepts it while
 * the procedure is in `state` (an unknown state enables nothing). */
export function operatorButtons(model: PhaseModelTable, state: string): { command: string; enabled: boolean }[] {
  const row = model.next[model.states.indexOf(state)];
  return model.commands.map((command, i) => ({ command, enabled: row !== undefined && row[i] !== null && row[i] !== undefined }));
}

/** Compiles one unit of the plant to its payload (the generator's phase
 * server when it has phases). */
export async function buildUnit(plant: PlantModel, unit: string, abi: AbiInfo): Promise<Uint8Array> {
  const m = unitPayload(plant, unit);
  if (!m) throw new Error(`no unit ${unit}`);
  const r = await buildPayload(workspaceOf(m), profileOfModel(m), abi, {
    pins: m.pins,
    issues: [],
    machines: m.machines,
    machineIssues: [],
    flows: flowsOf(m),
    flowIssues: [],
    phases: phaseSpecs(plant, unit),
  });
  if (!r.ok) throw new Error(`${unit}: ${r.stage}: ${r.errors.join("; ")}`);
  return r.wasm;
}

export interface RunnerOptions {
  config?: RunConfig;
  /** After every drain (state, steps or trace may have changed). */
  onChange?(): void;
}

export class RecipeRunner {
  readonly trace: TraceEntry[] = [];
  /** Commands sent, in order: unit, command, and what came back
   * ("lost" when nothing took the request). */
  readonly sent: { unit: string; command: string; outcome: string }[] = [];
  private readonly run: RecipeRunHandle;
  private readonly t0: number;
  private queue: RunInput[] = [];
  private draining = false;
  private stopTimer: (() => void) | null = null;
  private readonly unlisten: () => void;

  /** Allocates `recipe` on the plant's units as they are on `bus` (online,
   * node ids) and builds an Idle run. Throws with allocation's message. */
  constructor(
    proto: Protocol,
    plant: PlantModel,
    recipe: MasterRecipe,
    private readonly bus: SimBus,
    private readonly clock: Clock,
    private readonly opts: RunnerOptions = {},
  ) {
    this.t0 = clock.now();
    const units: RunUnit[] = allocationUnits(plant, (u) => bus.online(u.id)).map((u, i) => ({
      ...u,
      node: bus.unit(u.id)?.session.nodeId ?? plant.physical.units[i]?.node ?? defaultNode(i),
    }));
    this.run = proto.newRecipeRun(recipe, plant.procedural.classes, units, opts.config);
    this.unlisten = bus.onPublish((_unit, key, data) => {
      if (key.endsWith("/state")) this.input({ kind: "report", key, data });
    });
  }

  /** Milliseconds since the run was built. */
  nowMs(): number {
    return Math.max(0, Math.floor(this.clock.now() - this.t0));
  }

  get state(): string {
    return this.run.state;
  }

  steps(): { id: string; status: StepStatus }[] {
    return this.run.steps();
  }

  phases(): ReturnType<RecipeRunHandle["phases"]> {
    return this.run.phases();
  }

  bindings(): ReturnType<RecipeRunHandle["bindings"]> {
    return this.run.bindings();
  }

  /** Queues one input and handles everything it leads to. */
  input(i: RunInput): void {
    this.queue.push(i);
    this.drain();
  }

  /** An operator command (`"Start"`, ... any case). */
  operator(command: string): void {
    this.input({ kind: "operator", command: command.toLowerCase() });
  }

  /** Takes a unit off the network and tells the engine. */
  lose(unit: string): void {
    this.bus.setOnline(unit, false);
    this.input({ kind: "lost", unit });
  }

  /** Brings a unit back and tells the engine. */
  find(unit: string): void {
    this.bus.setOnline(unit, true);
    this.input({ kind: "found", unit });
  }

  tick(): void {
    this.input({ kind: "tick" });
  }

  private carryOut(e: RunEffect): void {
    if (e.kind === "trace") {
      this.trace.push({ t: e.t, type: e.type, text: e.text });
      return;
    }
    const r = this.bus.online(e.unit) ? this.bus.request(e.key, e.body, { deadlineMs: e.timeoutMs }, true) : null;
    this.sent.push({ unit: e.unit, command: e.command, outcome: r === null ? "lost" : r.result.ok ? "ok" : r.result.error });
    // The reply first, then the reports the request caused.
    if (r) this.queue.push({ kind: "reply", id: e.id, ...r.result });
    this.bus.flush();
  }

  private drain(): void {
    if (this.draining) return;
    this.draining = true;
    try {
      for (let n = 0; this.queue.length > 0; n++) {
        if (n >= MAX_INPUTS) {
          this.trace.push({ t: this.nowMs(), type: "failed", text: `inputs did not settle; ${this.queue.length} dropped` });
          this.queue = [];
          break;
        }
        const i = this.queue.shift()!;
        for (const e of this.run.on(this.nowMs(), i)) this.carryOut(e);
      }
    } finally {
      this.draining = false;
    }
    this.opts.onChange?.();
  }

  /** Every `pollMs`: each unit's main loop, then the engine's clock. */
  start(pollMs = 10): void {
    if (this.stopTimer) return;
    this.stopTimer = this.clock.every(pollMs, () => {
      this.bus.step();
      this.tick();
    });
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
    this.unlisten();
    this.run.free();
  }
}

/** The commands in the phase model's order (for tests and the page). */
export const OPERATOR_COMMANDS = PHASE_COMMANDS;
