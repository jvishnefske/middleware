// State shared by the pages: the protocol bindings, the one serial port, the
// hand-off of compiled payloads from the Editor and Blocks tabs to the
// Device tab, the Start tab's request to open an example in the Blocks
// simulator, the Start tab's board pick (the Blocks tab follows it) and
// the connected device's sys/info (the Units view binds to its node id),
// the connected board as a recipe unit (the Recipe view runs a unit bound
// to it on the board) and the Blocks tab's plant (the Device tab uploads
// the units bound to the board). Also which tab opens first: the one last
// shown, or Start on a first visit.

import type { SerialRequester } from "../device/requester";
import type { PlantModel } from "../model/plant";
import type { BoardId } from "../onboarding/boards";
import type { DeviceInfo, Protocol } from "../protocol/wasm";
import type { PortManager } from "../serial/ports";

export const TAB_IDS = ["start", "flash", "device", "editor", "blocks"] as const;
export type TabId = (typeof TAB_IDS)[number];

export function isTabId(s: unknown): s is TabId {
  return typeof s === "string" && (TAB_IDS as readonly string[]).includes(s);
}

/** The tab to open: the saved one when valid, else Start (first visit, or
 * storage cleared or unavailable). */
export function initialTab(saved: string | null | undefined): TabId {
  return isTabId(saved) ? saved : "start";
}

export interface PayloadHandoff {
  name: string;
  wasm: Uint8Array;
}

export interface App {
  proto: Protocol;
  ports: PortManager;
  /** Problem with Web Serial in this browser, or null. */
  serialProblem: string | null;
  showTab(id: TabId): void;
  /** Editor or Blocks -> Device: offer a compiled payload for upload. */
  offerPayload(p: PayloadHandoff): void;
  onPayloadOffered(f: (p: PayloadHandoff) => void): void;
  /** Start -> Blocks: show an example, compile it and run it in the
   * simulator. */
  tryExample(id: string): void;
  onTryExample(f: (id: string) => void): void;
  /** Start -> Blocks: the visitor picked a board (Blocks re-seeds a pin
   * table that is still the old board's defaults). */
  chooseBoard(id: BoardId): void;
  onBoardChosen(f: (id: BoardId) => void): void;
  /** Device -> Blocks: what the connected device's sys/info said (null
   * once it disconnects); the Units view binds a unit to its node id. */
  reportDeviceInfo(i: DeviceInfo | null): void;
  onDeviceInfo(f: (i: DeviceInfo | null) => void): void;
  /** Device -> Blocks: the connected board as a unit link (null once it
   * disconnects, or when it reports no node id). */
  reportBoard(b: ConnectedBoard | null): void;
  onBoard(f: (b: ConnectedBoard | null) => void): void;
  /** Blocks -> Device: the plant being edited (its units and bindings). */
  reportPlant(p: PlantModel): void;
  onPlant(f: (p: PlantModel) => void): void;
}

/** The board the Device tab is connected to, as a recipe unit. */
export interface ConnectedBoard {
  info: DeviceInfo;
  /** The Web Serial requester over the Device tab's client. */
  requester: SerialRequester;
}
