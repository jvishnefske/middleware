// The "Button toggles LED mode" example drawn as flows instead of blocks:
// the same machine (mode.machine.ts) driven by three flows, one per hat
// block of examples/mode.json. BUTTON's falling edge raises Mode.PRESS,
// every 250 ms raises Mode.TICK, and entering Mode.Blink logs
// "mode: blinking". It behaves exactly like the blocks version in the
// simulator (web/test/flows.gen.test.ts), apart from the `flow <id>:
// <node>` lines the flows log for the canvas.

import type { Flow } from "../../model/flows";

export const MODE_FLOWS: readonly Flow[] = [
  {
    kind: "flow",
    id: "Press",
    name: "BUTTON press raises PRESS",
    nodes: [
      { id: "n1", kind: "entry", x: 40, y: 40, props: { on: "pin-edge", label: "BUTTON", edge: "fall" } },
      { id: "n2", kind: "action", x: 40, y: 160, props: { kind: "raise", machine: "Mode", event: "PRESS" } },
    ],
    wires: [{ from: { node: "n1", port: "next" }, to: { node: "n2", port: "in" } }],
  },
  {
    kind: "flow",
    id: "Tick",
    name: "TICK every 250 ms",
    nodes: [
      { id: "n1", kind: "entry", x: 40, y: 40, props: { on: "every", ms: 250 } },
      { id: "n2", kind: "action", x: 40, y: 160, props: { kind: "raise", machine: "Mode", event: "TICK" } },
    ],
    wires: [{ from: { node: "n1", port: "next" }, to: { node: "n2", port: "in" } }],
  },
  {
    kind: "flow",
    id: "Blinking",
    name: "say when blinking starts",
    nodes: [
      { id: "n1", kind: "entry", x: 40, y: 40, props: { on: "entered", state: "Mode.Blink" } },
      { id: "n2", kind: "action", x: 40, y: 160, props: { kind: "log", text: "mode: blinking" } },
    ],
    wires: [{ from: { node: "n1", port: "next" }, to: { node: "n2", port: "in" } }],
  },
];
