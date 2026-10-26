// Board profiles for the Blocks editor: which block groups a board
// supports and how its payload's linear memory is sized.
//
// * `esp-base`: the middleware ESP base firmware. GPIO, timers and GPIO
//   interrupts; memory is one standard 64 KiB page, like the editor
//   templates.
// * `badge`: the conagotchi ESP32-S3 badge. Its firmware refuses every
//   `gpio_*` call and keeps wasm-host's `Unsupported` defaults for the
//   ABI 2 timers and interrupts, so those blocks are disabled; instead it
//   routes publishes on `badge/leds`, `badge/text` and `badge/pet` to the
//   badge and delivers button edges on `badge/buttons`. It caps linear
//   memory at 16 KiB (`board::PAYLOAD_MAX_MEMORY`), less than one 64 KiB
//   page, so the payload is rewritten to 1-byte pages (custom-page-sizes)
//   of exactly that size; see `bytePages.ts`.

/** A group of blocks a board may or may not support. */
export type Feature = "core" | "gpio" | "timers" | "badge";

export type ProfileId = "esp-base" | "badge";

/** How the compiled module's memory is declared. */
export type MemoryLayout =
  | { kind: "page64k" }
  /** 1-byte pages, initial = maximum = `bytes`. */
  | { kind: "bytes"; bytes: number };

export interface Profile {
  id: ProfileId;
  label: string;
  features: readonly Feature[];
  memory: MemoryLayout;
  /** One line for the UI. */
  note: string;
}

/** The badge's linear-memory cap (conagotchi `board::PAYLOAD_MAX_MEMORY`,
 * also the default of `payloads/scripts/build-byte-pages.sh`). */
export const BADGE_MEMORY_BYTES = 16 * 1024;

export const PROFILES: Readonly<Record<ProfileId, Profile>> = {
  "esp-base": {
    id: "esp-base",
    label: "ESP base firmware",
    features: ["core", "gpio", "timers"],
    memory: { kind: "page64k" },
    note: "GPIO, timers and pin interrupts. One 64 KiB memory page.",
  },
  badge: {
    id: "badge",
    label: "Conagotchi badge",
    features: ["core", "badge"],
    memory: { kind: "bytes", bytes: BADGE_MEMORY_BYTES },
    note: "LEDs, text, pet mode and buttons; no payload GPIO or timers. 16 KiB memory in 1-byte pages.",
  },
};

export const PROFILE_IDS = Object.keys(PROFILES) as ProfileId[];

export function isProfileId(s: unknown): s is ProfileId {
  return typeof s === "string" && s in PROFILES;
}

export function supports(profile: ProfileId, feature: Feature): boolean {
  return PROFILES[profile].features.includes(feature);
}
