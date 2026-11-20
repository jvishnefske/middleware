// The autoprobe: what a node's own description says about the machine it
// drives, drafted into a plant unit, the lathe's IoMap and a binding.
//
// * `probeNode` reads `sys/info`, `sys/manifest` and every page of
//   `sys/manifest/cm` the manifest announces (in order, from page 0), and
//   nothing else: over the mesh those are the only keys a browser may ask.
//   `decodeProbe` is the pure part, run on what was read (the golden test
//   feeds it posix-node's recorded fixtures).
// * The board is the manifest's BOARD record (a Linux node's device tree
//   `model`), never `sys/board`; it picks the Start-tab board (the KD240).
// * The unit is drafted with no pin table and the node's points
//   (model/points.ts), grouped into equipment modules by name, in this
//   order: `x_`/`z_` or a segment queue is `motion`, `spindle_` is
//   `spindle`, `turret_` is `turret`, `estop` is `estop`. A point matching
//   none stays ungrouped; empty modules are left out.
// * The lathe's channels (cnc/model `IoMap::pairs`, `LATHE_CHANNELS`) are
//   matched to points: the point of the same name first, else an unused
//   point with the channel's prefix, kind and direction; `queue` takes the
//   `motion` point, as `IoMap::pairs` does. An unmatched channel is null
//   in the IoMap draft and a problem.
// * `io` is the IoMap's JSON (camelCase, its field order); `ioSpecs` the
//   `channel=driver:point` specs `mwctl bind` takes (`IoMap::binding_specs`);
//   `binding` the same as a draft that `bindingForImage` turns into a
//   binding record once a payload image is chosen.
// * Deploying is mwctl's (the mesh is probe-only): `deployCommands` writes
//   the upload, bind and start commands for the probed node.

import { BOARDS, type Board } from "../onboarding/boards";
import { makeModel } from "../model/payload";
import { EMPTY_PINS } from "../model/pins";
import { type EquipmentModule, type PlantModel, type Unit, makePlant, unitFromPayload } from "../model/plant";
import { type PointRow, pointFromControl } from "../model/points";
import type { BindingRecord, ChannelDecl, DeviceInfo, MachineKind, Protocol } from "../protocol/wasm";
import type { DeviceClient } from "./client";

/** The IoMap's fields (cnc/model `IoMap`, camelCase, in its order). */
export type IoKey =
  | "xPos"
  | "zPos"
  | "xHome"
  | "zHome"
  | "spindleRpm"
  | "spindleActual"
  | "turretStation"
  | "turretReady"
  | "estop"
  | "motion";

/** The lathe's IoMap, drafted: each field the point it names, or null. */
export type IoMapDraft = Record<IoKey, string | null>;

/** One channel of the lathe's physical model. */
export interface LatheChannel {
  channel: string;
  io: IoKey;
  kind: MachineKind;
  /** Seen from the payload. */
  dir: "in" | "out";
  /** The name prefix a point for it has. */
  prefix: string;
}

/** The lathe's channels in `IoMap::pairs` order. */
export const LATHE_CHANNELS: readonly LatheChannel[] = [
  { channel: "x_pos", io: "xPos", kind: "position", dir: "in", prefix: "x_" },
  { channel: "z_pos", io: "zPos", kind: "position", dir: "in", prefix: "z_" },
  { channel: "x_home", io: "xHome", kind: "digital", dir: "in", prefix: "x_" },
  { channel: "z_home", io: "zHome", kind: "digital", dir: "in", prefix: "z_" },
  { channel: "spindle_rpm", io: "spindleRpm", kind: "setpoint", dir: "out", prefix: "spindle_" },
  { channel: "spindle_actual", io: "spindleActual", kind: "setpoint", dir: "in", prefix: "spindle_" },
  { channel: "turret_station", io: "turretStation", kind: "setpoint", dir: "out", prefix: "turret_" },
  { channel: "turret_ready", io: "turretReady", kind: "digital", dir: "in", prefix: "turret_" },
  { channel: "estop", io: "estop", kind: "digital", dir: "in", prefix: "estop" },
  { channel: "motion", io: "motion", kind: "segments", dir: "out", prefix: "" },
  { channel: "queue", io: "motion", kind: "segments", dir: "in", prefix: "" },
];

const IO_KEYS: readonly IoKey[] = [
  "xPos",
  "zPos",
  "xHome",
  "zHome",
  "spindleRpm",
  "spindleActual",
  "turretStation",
  "turretReady",
  "estop",
  "motion",
];

/** The lathe's channels as `mw.channels` declarations (what its payload
 * declares). */
export function latheChannelDecls(): ChannelDecl[] {
  return LATHE_CHANNELS.map((c) => ({
    name: c.channel,
    dir: c.dir,
    kind: c.kind,
    sensor: null,
    formats: 0,
    channels: 0,
    rateMin: 0,
    rateMax: 0xffffffff,
    required: true,
  }));
}

/** A binding before its image is known: each channel and its point. */
export interface BindingDraft {
  entries: { name: string; point: string }[];
}

export interface ProbeInput {
  info: DeviceInfo;
  /** The raw `sys/manifest`. */
  manifest: Uint8Array;
  /** The raw `sys/manifest/cm` pages by number; a missing one is absent. */
  pages: readonly (Uint8Array | null | undefined)[];
}

export interface ProbeResult {
  /** The node id (16 hex), from `sys/info`. */
  node: string | null;
  chip: string;
  /** The manifest's BOARD record. */
  boardName: string | null;
  points: PointRow[];
  /** The drafted unit (plantProblem-clean). */
  unit: Unit;
  /** `makePlant([unit])`. */
  plant: PlantModel;
  io: IoMapDraft;
  ioSpecs: string[];
  binding: BindingDraft;
  /** What the probe could not settle, in words. */
  problems: string[];
}

const EM_NAMES: Record<string, string> = { motion: "Motion", spindle: "Spindle", turret: "Turret", estop: "E-stop" };

/** The equipment module a point belongs to, or null. */
export function emOf(p: Pick<PointRow, "name" | "kind">): string | null {
  if (p.name.startsWith("x_") || p.name.startsWith("z_") || p.kind === "segments") return "motion";
  if (p.name.startsWith("spindle_")) return "spindle";
  if (p.name.startsWith("turret_")) return "turret";
  if (p.name === "estop") return "estop";
  return null;
}

function groups(points: readonly PointRow[]): EquipmentModule[] {
  return Object.keys(EM_NAMES)
    .map((id) => ({ id, name: EM_NAMES[id], cms: points.filter((p) => emOf(p) === id).map((p) => p.name) }))
    .filter((em) => em.cms.length > 0);
}

const admits = (p: PointRow, dir: "in" | "out") => p.flow === "both" || p.flow === dir;

function matchIo(points: readonly PointRow[]): IoMapDraft {
  const io = Object.fromEntries(IO_KEYS.map((k) => [k, null])) as IoMapDraft;
  const used = new Set<string>();
  const fits = (p: PointRow, c: LatheChannel) => p.kind === c.kind && admits(p, c.dir);
  for (const c of LATHE_CHANNELS) {
    if (c.channel === "queue") continue;
    const p =
      points.find((x) => x.name === c.channel && fits(x, c) && !used.has(x.name)) ??
      points.find((x) => x.name.startsWith(c.prefix) && fits(x, c) && !used.has(x.name));
    if (p) {
      io[c.io] = p.name;
      used.add(p.name);
    }
  }
  return io;
}

function boardFor(chip: string, boardName: string | null): Board | undefined {
  return BOARDS.find((b) => b.chip === chip && (!b.model || (boardName ?? "").includes(b.model)));
}

const plural = (n: number, one: string) => `${n} ${one}${n === 1 ? "" : "s"}`;

/** Decodes what a probe read (see the module comment). */
export function decodeProbe(proto: Protocol, input: ProbeInput): ProbeResult {
  const problems: string[] = [];
  const { info } = input;
  const m = proto.decodeManifest(input.manifest);
  if (!m) problems.push("sys/manifest is malformed");
  const boardName = m?.board ?? null;
  const want = m?.controlModules?.pages ?? 0;
  if (m && !m.controlModules) problems.push("the manifest lists no control points (sys/manifest/cm)");
  const points: PointRow[] = [];
  for (let p = 0; p < want; p++) {
    const raw = input.pages[p];
    if (!raw) {
      problems.push(`sys/manifest/cm page ${p} is missing (the manifest lists ${plural(want, "page")})`);
      continue;
    }
    const page = proto.decodeControlPage(raw);
    if (!page) {
      problems.push(`sys/manifest/cm page ${p} is malformed`);
      continue;
    }
    if (page.page !== p) {
      problems.push(`sys/manifest/cm page ${p} came back as page ${page.page}`);
      continue;
    }
    if (page.pages !== want)
      problems.push(`sys/manifest/cm page ${p} says ${plural(page.pages, "page")}, the manifest ${want}`);
    points.push(...page.points.map((x) => pointFromControl(x)));
  }
  const announced = m?.controlModules?.points;
  if (announced !== undefined && problems.length === 0 && points.length !== announced) {
    problems.push(`sys/manifest/cm lists ${plural(points.length, "point")}, the manifest ${announced}`);
  }
  const node = info.node?.toLowerCase() ?? null;
  if (!node) problems.push("sys/info reports no node id, so the unit is not bound");
  const board = boardFor(info.chip, boardName);
  if (!board)
    problems.push(`no known board runs chip ${info.chip} on board ${JSON.stringify(boardName)}; drafted as a KD240`);
  const boardId = board?.id ?? "kd240";
  const unitId = board ? board.id : `node${(node ?? "").slice(0, 8)}`;
  const base = unitFromPayload(makeModel(boardId, {}, EMPTY_PINS), unitId);
  const unit: Unit = {
    ...base,
    name: boardName ?? `${info.chip} node`,
    node,
    ems: groups(points),
    ...(points.length ? { points } : {}),
  };
  const io = matchIo(points);
  const ioSpecs: string[] = [];
  const entries: BindingDraft["entries"] = [];
  for (const c of LATHE_CHANNELS) {
    const point = io[c.io];
    if (point === null) {
      problems.push(`no point for the lathe's channel ${c.channel} (${c.kind}, ${c.dir})`);
      continue;
    }
    ioSpecs.push(`${c.channel}=driver:${point}`);
    entries.push({ name: c.channel, point });
  }
  return {
    node,
    chip: info.chip,
    boardName,
    points,
    unit,
    plant: makePlant([unit]),
    io,
    ioSpecs,
    binding: { entries },
    problems,
  };
}

/** Probes the node behind `client` (see the module comment). */
export async function probeNode(client: DeviceClient, proto: Protocol): Promise<ProbeResult> {
  const info = await client.info();
  const manifest = await client.manifestBody();
  if (!manifest) throw new Error("the node has no sys/manifest, so it lists no control points");
  const pages = proto.decodeManifest(manifest)?.controlModules?.pages ?? 0;
  const read: (Uint8Array | null)[] = [];
  for (let p = 0; p < pages; p++) read.push(await client.controlPage(p));
  return decodeProbe(proto, { info, manifest, pages: read });
}

/** The binding record of `draft` for the payload image with this
 * SHA-256, at `version`. */
export function bindingForImage(draft: BindingDraft, imageSha256: Uint8Array, version: number): BindingRecord {
  return {
    imageSha256,
    version,
    entries: draft.entries.map((e) => ({ name: e.name, target: { kind: "driver", point: e.point } })),
  };
}

/** The mwctl commands that deploy `wasm` to the probed node over the mesh:
 * upload, bind with the drafted specs, start. `sender` is the node's
 * sender id (what the scan listed), `site` the domain whose
 * `mesh.updates.<site>` is the router; `<KEYFILE>` is the lathe's key. */
export function deployCommands(r: ProbeResult, opts: { site: string; sender: string; wasm: string }): string[] {
  const z = `mwctl --zenoh tls/mesh.updates.${opts.site}:443 --node ${opts.sender}`;
  return [
    `${z} upload ${opts.wasm} --key <KEYFILE>`,
    `${z} bind ${opts.wasm} ${r.ioSpecs.join(" ")} --key <KEYFILE>`,
    `${z} start --key <KEYFILE>`,
  ];
}
