// Virtual clock for simulator tests: time only moves when a test advances
// it, and every registered poll runs at each step.

import type { Clock } from "../src/sim/session";

export class ManualClock implements Clock {
  t = 0;
  private fns: (() => void)[] = [];
  now() {
    return this.t;
  }
  every(_ms: number, f: () => void) {
    this.fns.push(f);
    return () => {
      this.fns = this.fns.filter((g) => g !== f);
    };
  }
  advance(ms: number, stepMs = 10) {
    for (let i = 0; i < ms; i += stepMs) {
      this.t += stepMs;
      for (const f of this.fns) f();
    }
  }
}
