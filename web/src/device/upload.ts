// Payload upload over the loader protocol (requirement U3).
//
// The request sequence (begin with length + CRC-32, in-order chunks of at
// most CHUNK_MAX bytes, commit) comes from protocol-wasm's `uploadPlan`,
// which uses the Rust loader encoders. This module only sequences the
// requests, reports progress and turns failures into readable errors.
//
// Retry semantics: each request is retried once on timeout. That recovers a
// request lost on the way to the device, and also a lost response: the
// device's loader acknowledges a resend of the chunk it last wrote without
// writing it again, and the CRC-32 check at commit covers the stored bytes.
//
// Signing (U13): with `signing` set, the commit body is
// `[version u32 LE][Ed25519 signature 64]` over the trust message built from
// the image (see `trust/wire.ts`). `trustedUpload` first reads
// `sys/trust/info` and decides: sign when the device pins this browser's
// key, upload unsigned when it is unclaimed or its firmware predates trust
// (after asking `confirmUnsigned`), and refuse before sending anything when
// the device pins a key this browser does not hold.

import type { Protocol } from "../protocol/wasm";
import { signImage } from "../trust/crypto";
import { type UploadPlan, planUpload } from "../trust/wire";
import { type DeviceClient, type RequestOptions, RequestError } from "./client";

export interface UploadOptions extends RequestOptions {
  /** Chunk size (clamped by the plan to 1..CHUNK_MAX). */
  chunk?: number;
  /** Called after each acknowledged step with bytes delivered and total. */
  onProgress?(done: number, total: number): void;
  /** Aborts between steps. */
  signal?: AbortSignal;
  /** Send `sys/payload/start` after a successful commit. */
  start?: boolean;
  /** Sign the commit with this key and version. */
  signing?: { privateKey: CryptoKey; version: number };
}

/** Suffix of the firmware version string of builds without the WASM
 * payload runtime (ESP32-C3, ESP8266). They store payloads but answer
 * `sys/payload/start` with NotFound. */
export const NO_WASM_SUFFIX = "+no-wasm";

/** Whether firmware reporting `version` in `sys/info` can run payloads. */
export function firmwareRunsPayloads(version: string): boolean {
  return !version.endsWith(NO_WASM_SUFFIX);
}

/** A step of the upload failed. */
export class UploadError extends Error {
  constructor(
    readonly step: number,
    readonly key: string,
    readonly cause: unknown,
  ) {
    super(`upload failed at step ${step} (${key}): ${cause instanceof Error ? cause.message : String(cause)}`);
    this.name = "UploadError";
  }

  /** The device's status name when it refused the step. */
  get status(): string | undefined {
    return this.cause instanceof RequestError ? this.cause.status : undefined;
  }
}

/**
 * Floor for the `begin` timeout: the device erases the payload flash region
 * (up to ~21 sectors) before answering, which can take over a second.
 */
export const BEGIN_TIMEOUT_MS = 5000;

/** Installs `image` on the device; resolves once committed (and started). */
export async function uploadPayload(
  client: DeviceClient,
  proto: Protocol,
  image: Uint8Array,
  opts: UploadOptions = {},
): Promise<void> {
  const steps = proto.uploadPlan(image, opts.chunk);
  if (opts.signing) {
    const commit = steps.findIndex((s) => s.key === proto.constants.keys.commit);
    if (commit < 0) throw new UploadError(steps.length, proto.constants.keys.commit, new Error("upload plan has no commit"));
    const signed = await signImage(opts.signing.privateKey, image, opts.signing.version);
    steps[commit] = { ...steps[commit], body: signed.body };
  }
  const total = image.length;
  const reqOpts: RequestOptions = { timeoutMs: opts.timeoutMs, retries: opts.retries ?? 1 };
  opts.onProgress?.(0, total);
  for (const [i, s] of steps.entries()) {
    if (opts.signal?.aborted) throw new UploadError(i, s.key, new Error("aborted"));
    try {
      const stepOpts =
        s.key === proto.constants.keys.begin
          ? { ...reqOpts, timeoutMs: Math.max(reqOpts.timeoutMs ?? BEGIN_TIMEOUT_MS, BEGIN_TIMEOUT_MS) }
          : reqOpts;
      await client.request(s.key, s.body, stepOpts);
    } catch (e) {
      throw new UploadError(i, s.key, e);
    }
    opts.onProgress?.(s.done, total);
  }
  if (opts.start) {
    try {
      await client.startPayload(reqOpts);
    } catch (e) {
      throw new UploadError(steps.length, proto.constants.keys.start, e);
    }
  }
}

/** The device pins a key this browser cannot sign with; nothing was sent. */
export class TrustRefusal extends Error {
  constructor(readonly reason: "different-key" | "no-local-key") {
    super(
      reason === "different-key"
        ? "the device trusts a different key — restore that key from its backup, or hold BOOT ≥3 s after boot to open the re-key window and claim the device again"
        : "the device is claimed, and this browser has no signing key — restore the key from its backup, or re-key the device (hold BOOT ≥3 s after boot, then claim)",
    );
    this.name = "TrustRefusal";
  }
}

export interface SigningKey {
  privateKey: CryptoKey;
  publicKey: Uint8Array;
}

export interface TrustedUploadOptions extends UploadOptions {
  /** Wall clock for the version, ms; defaults to `Date.now`. */
  now?: () => number;
  /** Asked before an unsigned upload; resolve false to cancel. */
  confirmUnsigned?(reason: "unclaimed" | "legacy-firmware"): Promise<boolean> | boolean;
}

/** What `trustedUpload` did. */
export type TrustedUploadResult = Exclude<UploadPlan, { kind: "refuse" }>;

/** Reads the device's trust state, then uploads signed or unsigned
 * accordingly (see the module comment). */
export async function trustedUpload(
  client: DeviceClient,
  proto: Protocol,
  image: Uint8Array,
  key: SigningKey | null,
  opts: TrustedUploadOptions = {},
): Promise<TrustedUploadResult> {
  const reqOpts: RequestOptions = { timeoutMs: opts.timeoutMs, retries: opts.retries ?? 1 };
  const info = await client.trustInfo(reqOpts);
  const plan = planUpload(info, key?.publicKey ?? null, (opts.now ?? Date.now)());
  if (plan.kind === "refuse") throw new TrustRefusal(plan.reason);
  if (plan.kind === "unsigned") {
    if (opts.confirmUnsigned && !(await opts.confirmUnsigned(plan.reason))) {
      throw new UploadError(0, proto.constants.keys.begin, new Error("cancelled"));
    }
    await uploadPayload(client, proto, image, { ...opts, signing: undefined });
    return plan;
  }
  if (!key) throw new TrustRefusal("no-local-key");
  await uploadPayload(client, proto, image, { ...opts, signing: { privateKey: key.privateKey, version: plan.version } });
  return plan;
}
