// Example workspaces offered in the Blocks tab's Examples menu. The JSON
// files in examples/ are Blockly workspace serializations, so they can
// also be imported with "Import JSON". Loading one over different blocks
// asks first (see replace.ts). An example names the pins it uses by label
// (`needs`); loading it adds any missing one to the pin table from the
// board's defaults (model/pins.ts `withNeeds`), so Blink drives GPIO 25 on
// a Pico 2 and GPIO 4 on the ESP32-CAM without editing the blocks. An
// example may carry state-machine tables (`machines`); loading it replaces
// the payload's machines with them (replace.ts `exampleModel`).

import type { Machine } from "../model/machines";
import type { PinNeed } from "../model/pins";
import type { ProfileId } from "./profiles";
import type { WorkspaceJson } from "./workspace";
import blink from "./examples/blink.json";
import chase from "./examples/chase.json";
import echo from "./examples/echo.json";
import mode from "./examples/mode.json";
import { MODE_MACHINE } from "./examples/mode.machine";

export interface Example {
  id: string;
  label: string;
  /** The profile the example is written for. */
  profile: ProfileId;
  workspace: WorkspaceJson;
  /** Pins the blocks name by label. */
  needs: readonly PinNeed[];
  /** State machines the blocks refer to (none when absent). */
  machines?: readonly Machine[];
}

export const EXAMPLES: readonly Example[] = [
  {
    id: "blink",
    label: "Blink (the LED pin every 500 ms)",
    profile: "esp-base",
    workspace: blink as unknown as WorkspaceJson,
    needs: [{ label: "LED", func: "gpio-out" }],
  },
  { id: "echo", label: "Echo (app/echo/in to app/echo/out)", profile: "esp-base", workspace: echo as unknown as WorkspaceJson, needs: [] },
  {
    id: "mode",
    label: "Button toggles LED mode",
    profile: "esp-base",
    workspace: mode as unknown as WorkspaceJson,
    needs: [
      { label: "LED", func: "gpio-out" },
      { label: "BUTTON", func: "gpio-in" },
    ],
    machines: [MODE_MACHINE],
  },
  { id: "chase", label: "Badge LED chase", profile: "badge", workspace: chase as unknown as WorkspaceJson, needs: [] },
];

/** A deep copy (blocks and machines), so editing a loaded example never changes the original. */
export function exampleWorkspace(id: string): Example | undefined {
  const e = EXAMPLES.find((x) => x.id === id);
  return e && { ...e, workspace: structuredClone(e.workspace), ...(e.machines ? { machines: structuredClone(e.machines) } : {}) };
}
