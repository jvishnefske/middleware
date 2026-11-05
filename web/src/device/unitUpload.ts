// Uploading a plant unit's payload to its bound board (design.md Q23):
// the Device tab lists the units bound to the connected board's node id
// (Blocks → Units → Bind) and sends each one's compiled phase server
// through the same signed upload as any payload (`trustedUpload`), then
// starts it. DOM-free.

import type { PlantModel, Unit } from "../model/plant";
import type { DeviceInfo, Protocol } from "../protocol/wasm";
import { buildUnit } from "../sim/recipeRun";
import type { DeviceClient } from "./client";
import { canServe, serveProblem } from "./requester";
import { type SigningKey, type TrustedUploadOptions, type TrustedUploadResult, trustedUpload } from "./upload";

/** The plant's units bound to the board with node id `node`. */
export function unitsOnBoard(plant: PlantModel | null, node: string | null | undefined): Unit[] {
  if (!plant || !node) return [];
  const n = node.toLowerCase();
  return plant.physical.units.filter((u) => u.node?.toLowerCase() === n);
}

/** What the Device tab says about the plant and the connected board. */
export function boardUnitsNote(plant: PlantModel | null, info: DeviceInfo | null): string {
  if (!info) return "Connect a board to upload the plant's unit payloads to it.";
  if (!plant) return "Open a plant in the Blocks tab (Examples: Plant: Blink then Measure) to upload its units here.";
  const units = unitsOnBoard(plant, info.node);
  if (!info.node) return serveProblem(info) ?? "";
  if (units.length === 0) return `No unit of the plant is bound to this board (node ${info.node}). Bind one in Blocks → Units.`;
  const names = units.map((u) => u.id).join(", ");
  const problem = canServe(info) ? null : serveProblem(info);
  return problem ? `Bound here: ${names}. ${problem}` : `Bound here: ${names}. Upload a unit's payload, then run the recipe from Blocks → Recipe.`;
}

/** Compiles `unit` of `plant` and installs it on the board behind
 * `client` with the signed upload, starting it afterwards. */
export async function uploadUnit(
  client: DeviceClient,
  proto: Protocol,
  plant: PlantModel,
  unit: string,
  key: SigningKey | null,
  opts: TrustedUploadOptions = {},
): Promise<TrustedUploadResult> {
  const wasm = await buildUnit(plant, unit, proto.abi);
  return trustedUpload(client, proto, wasm, key, { ...opts, start: true });
}
