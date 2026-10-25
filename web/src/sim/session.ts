// Simulation session (requirements U6, D10): drives a protocol-wasm
// Simulator (wasm-host, the device runtime, compiled to WASM) on a clock and
// keeps what the UI renders: the virtual pin table and the ABI 2 peripheral
// view (timers, last SPI transfer, UART transmit log, queue overflow).
// DOM-free; the clock is injected so tests run in virtual time.
//
// Each poll calls `Simulator.step(nowUs)`, which completes DMA started
// earlier, raises due timers, runs `tick` and dispatches queued interrupt
// events to `on_irq`. Input edges and UART bytes are stamped with the
// current clock and dispatched on the next step.

import type { PinState, Protocol, SimEvent, Simulator, TimerState } from "../protocol/wasm";

export interface Clock {
  now(): number;
  every(ms: number, f: () => void): () => void;
}

export const realClock: Clock = {
  now: () => performance.now(),
  every: (ms, f) => {
    const h = setInterval(f, ms);
    return () => clearInterval(h);
  },
};

/** The ABI 2 peripherals as the UI shows them. */
export interface PeripheralView {
  timers: TimerState[];
  /** Events dropped from the full queue since load. */
  dropped: number;
  /** Events waiting for the next step. */
  pending: number;
  /** The most recent SPI transfer. */
  lastSpi: { tx: Uint8Array; rx: Uint8Array } | null;
  /** Interrupt events dispatched since load. */
  irqs: number;
}

export interface SessionEvents {
  onEvent?(e: SimEvent): void;
  onPins?(pins: ReadonlyMap<number, PinState>): void;
  /** After every step and peripheral change. */
  onPeripherals?(view: PeripheralView): void;
  onError?(message: string): void;
}

export class SimSession {
  private readonly sim: Simulator;
  private readonly pinTable = new Map<number, PinState>();
  private readonly t0: number;
  private stopTimer: (() => void) | null = null;
  private lastSpi: PeripheralView["lastSpi"] = null;
  private irqs = 0;

  /** Loads the payload (runs `init`). Throws if wasm-host rejects it. */
  constructor(
    proto: Protocol,
    wasm: Uint8Array,
    private readonly events: SessionEvents = {},
    private readonly clock: Clock = realClock,
  ) {
    this.t0 = clock.now();
    this.sim = proto.newSimulator(wasm, (e) => this.apply(e));
    this.events.onPins?.(this.pinTable);
  }

  private apply(e: SimEvent): void {
    switch (e.kind) {
      case "output":
        this.pinTable.set(e.pin, { mode: "output", high: false });
        break;
      case "input":
        this.pinTable.set(e.pin, { mode: "input", pull: e.pull, high: e.pull === "up" });
        break;
      case "write":
        this.pinTable.set(e.pin, { mode: "output", high: e.high });
        break;
      case "spi":
        this.lastSpi = { tx: e.tx, rx: e.rx };
        break;
      case "irq":
        this.irqs++;
        break;
      default:
        break;
    }
    this.events.onEvent?.(e);
    if (e.kind === "output" || e.kind === "input" || e.kind === "write") this.events.onPins?.(this.pinTable);
  }

  /** Current peripheral state. */
  peripherals(): PeripheralView {
    return {
      timers: this.sim.timers(),
      dropped: this.sim.dropped,
      pending: this.sim.pending,
      lastSpi: this.lastSpi,
      irqs: this.irqs,
    };
  }

  private notifyPeripherals(): void {
    if (this.events.onPeripherals) this.events.onPeripherals(this.peripherals());
  }

  /** Whether the payload exports `on_irq`. */
  get handlesIrq(): boolean {
    return this.sim.handlesIrq;
  }

  get pins(): ReadonlyMap<number, PinState> {
    return this.pinTable;
  }

  get subscriptions(): string[] {
    return this.sim.subscriptions();
  }

  get running(): boolean {
    return this.stopTimer !== null;
  }

  /** Milliseconds since load, wrapping like the device's `now_ms`. */
  nowMs(): number {
    return Math.floor(this.clock.now() - this.t0) >>> 0;
  }

  /** Microseconds since load (not wrapped; the device wraps `now_us`). */
  nowUs(): number {
    return Math.max(0, Math.floor((this.clock.now() - this.t0) * 1000));
  }

  private guard(f: () => void): void {
    try {
      f();
    } catch (e) {
      this.stop();
      this.events.onError?.(e instanceof Error ? e.message : String(e));
    }
  }

  /** One main-loop pass at the current time: DMA completions, due
   * timers, a due tick and up to one queue of `on_irq` dispatches. */
  step(): void {
    this.guard(() => this.sim.step(this.nowUs()));
    this.notifyPeripherals();
  }

  start(pollMs = 10): void {
    if (this.stopTimer || this.sim.trapped) return;
    this.stopTimer = this.clock.every(pollMs, () => this.step());
  }

  stop(): void {
    this.stopTimer?.();
    this.stopTimer = null;
  }

  /** Delivers a message; returns whether a subscription matched. */
  deliver(key: string, data: Uint8Array): boolean {
    let hit = false;
    this.guard(() => {
      this.sim.setNowUs(this.nowUs());
      hit = this.sim.deliver(key, data);
    });
    return hit;
  }

  /** Drives an input; a matching armed edge queues a GPIO event for the
   * next step. */
  setInput(pin: number, high: boolean): boolean {
    this.sim.setNowUs(this.nowUs());
    const ok = this.sim.setInput(pin, high);
    const p = this.pinTable.get(pin);
    if (ok && p) {
      this.pinTable.set(pin, { ...p, high });
      this.events.onPins?.(this.pinTable);
    }
    this.notifyPeripherals();
    return ok;
  }

  /** Bytes arriving on UART 1; returns how many the device buffered (0 if
   * the payload has not opened the port with an RX pin). */
  uartInject(data: Uint8Array): number {
    this.sim.setNowUs(this.nowUs());
    const n = this.sim.uartInject(data);
    this.notifyPeripherals();
    return n;
  }

  /** What the virtual SPI device answers: `null` or empty for loopback,
   * otherwise a repeating pattern. Applies to transfers started later. */
  setSpiResponder(pattern: Uint8Array | null): void {
    this.sim.setSpiPattern(pattern ?? new Uint8Array());
  }

  dispose(): void {
    this.stop();
    this.sim.free();
  }
}
