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

import type { Protocol } from "../protocol/wasm";
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
