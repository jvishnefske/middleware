// Channel bindings in the browser (BD5): a payload's declared channels, a
// node's manifest and the binding the user edits; issues with one-click
// fixes from protocol-wasm's `checkBinding`; and installing the binding
// on the connected board, signed like an upload (`trust/wire.ts`
// `planUpload`: unsigned only on an unclaimed or pre-trust device).
//
// Pure except `installBinding`; `ui/bindingPanel.ts` renders it.

import type { BindingIssue, BindingRecord, BindingTarget, ChannelDecl, Protocol } from "../protocol/wasm";
import { type DeviceClient, type RequestOptions, refuseRemote } from "./client";
import { TrustRefusal, type SigningKey } from "./upload";
import { planUpload } from "../trust/wire";

/** A binding with no targets for the image with SHA-256 `imageSha256`. */
export function emptyBinding(imageSha256: Uint8Array): BindingRecord {
  return { imageSha256, version: 0, entries: [] };
}

/** `record` with `name` bound to `target` (or unbound with `null`). */
export function setTarget(record: BindingRecord, name: string, target: BindingTarget | null): BindingRecord {
  const entries = record.entries.filter((e) => e.name !== name);
  if (target) entries.push({ name, target });
  // Keep the declaration order stable for display: by name.
  entries.sort((a, b) => a.name.localeCompare(b.name));
  return { ...record, entries };
}

/** `record` with `issue`'s fix applied (unchanged without one). */
export function applyFix(record: BindingRecord, issue: BindingIssue): BindingRecord {
  const f = issue.fix;
  if (!f) return record;
  switch (f.kind) {
    case "remove":
      return setTarget(record, issue.channel, null);
    case "keyNamed":
      return setTarget(record, issue.channel, { kind: "key", key: issue.channel });
    default:
      return setTarget(record, issue.channel, f);
  }
}

/** The issues of `record` for `wasm` on the node with `manifest`; with
 * `peers` (the board's `sys/peers`) remote targets must name one of them. */
export function bindingIssues(
  proto: Protocol,
  manifest: Uint8Array,
  wasm: Uint8Array,
  record: BindingRecord,
  peers?: readonly string[],
): BindingIssue[] {
  return proto.checkBinding(manifest, wasm, proto.encodeBinding(record), peers);
}

/** A first binding: every fix `checkBinding` offers applied once (the
 * lowest offered source for each required input, a key named after each
 * required output). */
export function suggestBinding(proto: Protocol, manifest: Uint8Array, wasm: Uint8Array, imageSha256: Uint8Array): BindingRecord {
  let record = emptyBinding(imageSha256);
  for (const issue of bindingIssues(proto, manifest, wasm, record)) record = applyFix(record, issue);
  return record;
}

/** A target in a few words, for the panel. */
export function describeTarget(t: BindingTarget | undefined): string {
  if (!t) return "unbound";
  switch (t.kind) {
    case "key":
      return `key ${t.key}`;
    case "remote":
      return `key ${t.key} from ${t.node}`;
    case "driver":
      return `driver point ${t.point}`;
    case "adapted":
      return `${describeTarget(t.inner)} → ${t.adapter.kind}`;
    case "local":
      if (t.source === "mic") return `microphone ${t.rateHz} Hz ${t.mode}, gain ${t.gainDb} dB`;
      if (t.source === "imu") return `IMU ${t.rateHz} Hz ±${t.rangeG} g${t.mag ? " with magnetometer" : ""}`;
      return t.source;
  }
}

/** A channel declaration in a few words. */
export function describeChannel(c: ChannelDecl): string {
  const what = c.kind === "samples" ? `${c.sensor ?? "any"} samples ${c.rateMin}–${c.rateMax} Hz` : c.kind;
  return `${c.dir} ${what}${c.required ? "" : " (optional)"}`;
}

/** What `installBinding` did. */
export type BindResult = { kind: "signed"; version: number } | { kind: "unsigned"; reason: "unclaimed" | "legacy-firmware" };

/** Installs `record` on the connected board: version from the clock (above
 * the device's `min_version` when signing), signed with `key` when the
 * device is claimed by it. Throws `TrustRefusal` when it is claimed by
 * another key or this browser has none. */
export async function installBinding(
  client: DeviceClient,
  proto: Protocol,
  record: BindingRecord,
  key: SigningKey | null,
  opts: RequestOptions & { now?: () => number } = {},
): Promise<BindResult> {
  refuseRemote(client, "bind");
  const now = (opts.now ?? Date.now)();
  const plan = planUpload(await client.trustInfo(opts), key?.publicKey ?? null, now);
  if (plan.kind === "refuse") throw new TrustRefusal(plan.reason);
  if (plan.kind === "unsigned") {
    const bytes = proto.encodeBinding({ ...record, version: Math.floor(now / 1000) });
    await client.bind(proto.encodeBindBody(bytes), opts);
    return plan;
  }
  if (!key) throw new TrustRefusal("no-local-key");
  const node = (await client.info(opts)).node;
  if (!node) throw new Error("the device reports no node id to sign the binding for");
  const bytes = proto.encodeBinding({ ...record, version: plan.version });
  const message = proto.bindingMessage(bytes, node);
  const signature = new Uint8Array(
    await crypto.subtle.sign({ name: "Ed25519" }, key.privateKey, message.slice().buffer as ArrayBuffer),
  );
  await client.bind(proto.encodeBindBody(bytes, signature), opts);
  return { kind: "signed", version: plan.version };
}
