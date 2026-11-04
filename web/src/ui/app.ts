// State shared by the pages: the protocol bindings, the one serial port, the
// hand-off of compiled payloads from the Editor and Blocks tabs to the
// Device tab, and the Start tab's request to open an example in the Blocks
// simulator. Also which tab opens first: the one last shown, or Start on a
// first visit.

import type { Protocol } from "../protocol/wasm";
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
}
