// Signed-payload wire formats (requirement U13): pure functions with no I/O
// and no WebCrypto, so every byte layout is unit tested on its own.
//
// Contract with the device (`sys/trust/*`, `sys/loader/commit`):
//
// - Signed message M = "mw-payload-v1\0" (14 bytes) || len u32 LE ||
//   version u32 LE || sha256(image) (32 bytes), 54 bytes, signed with
//   Ed25519.
// - `sys/trust/info` answers `[state u8][min_version u32 LE][key 32]`, the
//   key present iff state != 0 (0 Unclaimed, 1 Claimed, 2 Claimed with the
//   re-key window open).
// - `sys/trust/claim` takes the 32-byte raw public key.
// - `sys/loader/commit` takes an empty body (unsigned) or
//   `[version u32 LE][signature 64]`.
// - Wire status 5 is `Denied`.

export const TRUST_KEYS = {
  info: "sys/trust/info",
  claim: "sys/trust/claim",
} as const;

/** Wire status code of `HandlerError::Denied`. */
export const DENIED_CODE = 5;

export const MESSAGE_TAG = "mw-payload-v1\0";
export const PUBLIC_KEY_LEN = 32;
export const SIGNATURE_LEN = 64;
export const SHA256_LEN = 32;
export const MESSAGE_LEN = MESSAGE_TAG.length + 4 + 4 + SHA256_LEN;
export const COMMIT_BODY_LEN = 4 + SIGNATURE_LEN;
export const U32_MAX = 0xffff_ffff;

/** The device's trust state, as a closed set. */
export type TrustState = "unclaimed" | "claimed" | "rekey-window";

export interface TrustInfo {
  state: TrustState;
  /** Commits must carry a version strictly greater than this. */
  minVersion: number;
  /** The pinned public key; null exactly when unclaimed. */
  key: Uint8Array | null;
}

/** A body that does not match the contract. */
export class TrustFormatError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "TrustFormatError";
  }
}

function checkU32(name: string, v: number): void {
  if (!Number.isInteger(v) || v < 0 || v > U32_MAX) throw new TrustFormatError(`${name} ${v} is not a u32`);
}

function checkLen(name: string, b: Uint8Array, len: number): void {
  if (b.length !== len) throw new TrustFormatError(`${name} must be ${len} bytes, got ${b.length}`);
}

/** Builds the 54-byte message the device verifies. */
export function signedMessage(imageLen: number, version: number, sha256: Uint8Array): Uint8Array {
  checkU32("image length", imageLen);
  checkU32("version", version);
  checkLen("sha256", sha256, SHA256_LEN);
  const m = new Uint8Array(MESSAGE_LEN);
  const tag = MESSAGE_TAG.length;
  for (let i = 0; i < tag; i++) m[i] = MESSAGE_TAG.charCodeAt(i);
  const dv = new DataView(m.buffer);
  dv.setUint32(tag, imageLen, true);
  dv.setUint32(tag + 4, version, true);
  m.set(sha256, tag + 8);
  return m;
}

/** `sys/loader/commit` body for a signed image: `[version u32 LE][sig 64]`. */
export function commitBody(version: number, signature: Uint8Array): Uint8Array {
  checkU32("version", version);
  checkLen("signature", signature, SIGNATURE_LEN);
  const b = new Uint8Array(COMMIT_BODY_LEN);
  new DataView(b.buffer).setUint32(0, version, true);
  b.set(signature, 4);
  return b;
}

/** Splits a commit body; null for the empty (unsigned) body. */
export function parseCommitBody(body: Uint8Array): { version: number; signature: Uint8Array } | null {
  if (body.length === 0) return null;
  checkLen("commit body", body, COMMIT_BODY_LEN);
  const version = new DataView(body.buffer, body.byteOffset, body.byteLength).getUint32(0, true);
  return { version, signature: body.slice(4) };
}

/** `sys/trust/claim` body: the raw 32-byte Ed25519 public key. */
export function claimBody(publicKey: Uint8Array): Uint8Array {
  checkLen("public key", publicKey, PUBLIC_KEY_LEN);
  return publicKey.slice();
}

const STATES: readonly TrustState[] = ["unclaimed", "claimed", "rekey-window"];

/** Parses a `sys/trust/info` response. */
export function parseTrustInfo(body: Uint8Array): TrustInfo {
  if (body.length < 5) throw new TrustFormatError(`sys/trust/info: ${body.length} bytes is too short`);
  const state = STATES[body[0]];
  if (!state) throw new TrustFormatError(`sys/trust/info: unknown state ${body[0]}`);
  const minVersion = new DataView(body.buffer, body.byteOffset, body.byteLength).getUint32(1, true);
  const want = state === "unclaimed" ? 5 : 5 + PUBLIC_KEY_LEN;
  if (body.length !== want) {
    throw new TrustFormatError(`sys/trust/info: ${state} must be ${want} bytes, got ${body.length}`);
  }
  return { state, minVersion, key: state === "unclaimed" ? null : body.slice(5) };
}

/** Encodes a `sys/trust/info` response (the fake device in tests). */
export function encodeTrustInfo(info: TrustInfo): Uint8Array {
  checkU32("min_version", info.minVersion);
  const claimed = info.state !== "unclaimed";
  if (claimed !== (info.key !== null)) throw new TrustFormatError("key must be present iff claimed");
  const b = new Uint8Array(claimed ? 5 + PUBLIC_KEY_LEN : 5);
  b[0] = STATES.indexOf(info.state);
  new DataView(b.buffer).setUint32(1, info.minVersion, true);
  if (info.key) {
    checkLen("public key", info.key, PUBLIC_KEY_LEN);
    b.set(info.key, 5);
  }
  return b;
}

/**
 * The commit version: wall-clock seconds as a u32, so the browser keeps no
 * counter. When that is not above the device's `min_version` (clock behind,
 * or a key used from several machines), `min_version + 1` with `bumped` set
 * so the caller can warn.
 */
export function chooseVersion(nowMs: number, minVersion: number): { version: number; bumped: boolean } {
  checkU32("min_version", minVersion);
  const now = Math.min(Math.max(Math.floor(nowMs / 1000), 0), U32_MAX);
  if (now > minVersion) return { version: now, bumped: false };
  if (minVersion === U32_MAX) throw new TrustFormatError("device min_version is at the u32 maximum; re-key it");
  return { version: minVersion + 1, bumped: true };
}

export function bytesEqual(a: Uint8Array, b: Uint8Array): boolean {
  if (a.length !== b.length) return false;
  for (let i = 0; i < a.length; i++) if (a[i] !== b[i]) return false;
  return true;
}

/** How this browser's key relates to the device's pinned key. */
export type KeyRelation = "no-trust" | "unclaimed" | "match" | "different" | "no-local-key";

/** `info` null means the firmware predates `sys/trust` (NotFound). */
export function relateKeys(info: TrustInfo | null, mine: Uint8Array | null): KeyRelation {
  if (!info) return "no-trust";
  if (!info.key) return "unclaimed";
  if (!mine) return "no-local-key";
  return bytesEqual(info.key, mine) ? "match" : "different";
}

/** Whether `sys/trust/claim` will be accepted in this state. */
export function canClaim(info: TrustInfo | null): boolean {
  return info !== null && (info.state === "unclaimed" || info.state === "rekey-window");
}

/** What an upload should do, decided from the device's trust info. */
export type UploadPlan =
  | { kind: "signed"; version: number; bumped: boolean }
  | { kind: "unsigned"; reason: "unclaimed" | "legacy-firmware" }
  | { kind: "refuse"; reason: "different-key" | "no-local-key" };

export function planUpload(info: TrustInfo | null, mine: Uint8Array | null, nowMs: number): UploadPlan {
  switch (relateKeys(info, mine)) {
    case "no-trust":
      return { kind: "unsigned", reason: "legacy-firmware" };
    case "unclaimed":
      return { kind: "unsigned", reason: "unclaimed" };
    case "no-local-key":
      return { kind: "refuse", reason: "no-local-key" };
    case "different":
      return { kind: "refuse", reason: "different-key" };
    case "match":
      return { kind: "signed", ...chooseVersion(nowMs, info?.minVersion ?? 0) };
  }
}

export function toHex(b: Uint8Array): string {
  return Array.from(b, (x) => x.toString(16).padStart(2, "0")).join("");
}

export function fromHex(s: string): Uint8Array {
  if (s.length % 2 !== 0 || /[^0-9a-f]/i.test(s)) throw new TrustFormatError("invalid hex");
  return Uint8Array.from(s.match(/../g) ?? [], (x) => parseInt(x, 16));
}
