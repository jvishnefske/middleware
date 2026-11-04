// The board a payload is written for: one of the Start tab's boards, or
// the conagotchi badge. The target decides the Blocks profile (which block
// groups exist, how memory is laid out) and the chip whose pin rules the
// pin table is checked against (protocol-wasm `checkPins`, the rules the
// firmwares apply). Pure.
//
// * Every Start-tab board runs the base firmware's payload ABI (esp-base
//   or rp-base), so its profile is `esp-base`.
// * The badge is an ESP32-S3 whose firmware refuses payload GPIO, so it
//   has the `badge` profile and no pin table (`chipOf` is null).

import type { ProfileId } from "../blocks/profiles";
import { BOARDS, type Board, type BoardId, boardById } from "../onboarding/boards";

export type TargetId = BoardId | "badge";

export const TARGET_IDS: readonly TargetId[] = [...BOARDS.map((b) => b.id), "badge"];

export function isTargetId(s: unknown): s is TargetId {
  return typeof s === "string" && (TARGET_IDS as readonly string[]).includes(s);
}

/** The Blocks profile of a target. */
export function profileOf(t: TargetId): ProfileId {
  return t === "badge" ? "badge" : "esp-base";
}

/** The board (Start tab data) behind a target; none for the badge. */
export function boardOf(t: TargetId): Board | undefined {
  return t === "badge" ? undefined : boardById(t);
}

/** The `sys/info` chip name whose pin rules apply, or null when the
 * target gives payloads no pins (the badge). */
export function chipOf(t: TargetId): string | null {
  return boardOf(t)?.chip ?? null;
}

export function targetLabel(t: TargetId): string {
  return t === "badge" ? "Conagotchi badge" : (boardOf(t)?.label ?? t);
}

/** The target a saved profile implies when nothing else says (a file
 * from before targets existed): the badge for the badge profile, else
 * `fallback`. */
export function targetForProfile(profile: ProfileId, fallback: BoardId): TargetId {
  return profile === "badge" ? "badge" : fallback;
}
