// State shared by the pages: the protocol bindings, the one serial port, and
// the hand-off of compiled payloads from the Editor to the Device tab.

import type { Protocol } from "../protocol/wasm";
import type { PortManager } from "../serial/ports";

export type TabId = "flash" | "device" | "editor";

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
  /** Editor -> Device: offer a compiled payload for upload. */
  offerPayload(p: PayloadHandoff): void;
  onPayloadOffered(f: (p: PayloadHandoff) => void): void;
}
