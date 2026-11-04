// The "Button toggles LED mode" example's state machine: BUTTON presses
// step the LED through Off, On and Blink; in Blink each TICK (raised every
// 250 ms by the example's blocks, examples/mode.json) toggles the LED.

import { type Machine, event, state, transition } from "../../model/machines";

export const MODE_MACHINE: Machine = {
  id: "Mode",
  name: "LED mode",
  initial: "Off",
  states: [
    state("Off", [{ kind: "write", label: "LED", level: 0 }], "LED dark"),
    state("On", [{ kind: "write", label: "LED", level: 1 }], "LED lit"),
    state("Blink", [], "LED toggles on every TICK"),
  ],
  events: [event("PRESS", "BUTTON pressed"), event("TICK", "every 250 ms")],
  rows: [
    transition("Off", "PRESS", "On"),
    transition("On", "PRESS", "Blink"),
    transition("Blink", "TICK", "Blink", [{ kind: "toggle", label: "LED" }]),
    transition("Blink", "PRESS", "Off"),
  ],
};
