// Example workspaces offered in the Blocks tab's Examples menu. The JSON
// files in examples/ are Blockly workspace serializations, so they can
// also be imported with "Import JSON".

import type { ProfileId } from "./profiles";
import type { WorkspaceJson } from "./workspace";
import blink from "./examples/blink.json";
import chase from "./examples/chase.json";
import echo from "./examples/echo.json";

export interface Example {
  id: string;
  label: string;
  /** The profile the example is written for. */
  profile: ProfileId;
  workspace: WorkspaceJson;
}

export const EXAMPLES: readonly Example[] = [
  { id: "blink", label: "Blink (GPIO 8 every 500 ms)", profile: "esp-base", workspace: blink as unknown as WorkspaceJson },
  { id: "echo", label: "Echo (app/echo/in to app/echo/out)", profile: "esp-base", workspace: echo as unknown as WorkspaceJson },
  { id: "chase", label: "Badge LED chase", profile: "badge", workspace: chase as unknown as WorkspaceJson },
];

/** A deep copy, so editing a loaded example never changes the original. */
export function exampleWorkspace(id: string): Example | undefined {
  const e = EXAMPLES.find((x) => x.id === id);
  return e && { ...e, workspace: structuredClone(e.workspace) };
}
