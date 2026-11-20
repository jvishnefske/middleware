// U49/U50: the probe of a KD240, from the wire fixtures posix-node records
// (posix-node controller/tests/web_fixtures.rs): the points, the drafted
// unit and plant, the lathe's IoMap and binding draft, compared with the
// golden kd240.plant.json (bless with MW_BLESS_FIXTURES=1 after a deliberate
// change), and probeNode reading pages through a DeviceClient.

import { readFileSync, writeFileSync } from "node:fs";
import { describe, expect, it } from "vitest";
import {
  type DeviceClient,
  NOT_FOUND_CODE,
  RequestClient,
  RequestError,
  type RequestOptions,
} from "../src/device/client";
import {
  LATHE_CHANNELS,
  type ProbeResult,
  bindingForImage,
  decodeProbe,
  deployCommands,
  latheChannelDecls,
  probeNode,
} from "../src/device/probe";
import { plantProblem, serializePlant } from "../src/model/plant";
import { sha256 } from "../src/trust/crypto";
import type { Protocol } from "../src/protocol/wasm";
import { protocol } from "./fakeDevice";

const proto = protocol();

interface WireFixture {
  facts: { chip: string; firmware: string; node: string; board: string };
  responses: { "sys/info": string; "sys/manifest": string; "sys/manifest/cm": string[] };
}

const unhex = (s: string) => new Uint8Array(s.match(/../g)?.map((b) => parseInt(b, 16)) ?? []);
const fixture = (name: string): WireFixture =>
  JSON.parse(readFileSync(new URL(`./fixtures/${name}`, import.meta.url), "utf8")) as WireFixture;

function input(f: WireFixture) {
  return {
    info: proto.decodeDeviceInfo(unhex(f.responses["sys/info"]))!,
    manifest: unhex(f.responses["sys/manifest"]),
    pages: f.responses["sys/manifest/cm"].map(unhex),
  };
}

/** What the golden file holds: everything the probe decided. */
function golden(r: ProbeResult): string {
  const { node, chip, boardName, io, ioSpecs, binding, problems } = r;
  const plant = JSON.parse(serializePlant(r.plant)) as unknown;
  return `${JSON.stringify({ node, chip, boardName, io, ioSpecs, binding, problems, plant }, null, 2)}\n`;
}

/** A DeviceClient serving the fixture's answers; records what was asked. */
class FixtureClient extends RequestClient {
  readonly transport = "mesh" as const;
  readonly remote = true;
  readonly asked: string[] = [];
  constructor(
    proto: Protocol,
    private readonly f: WireFixture,
    private readonly drop: number | null = null,
  ) {
    super(proto);
  }
  async request(key: string, body: Uint8Array = new Uint8Array(), _opts?: RequestOptions): Promise<Uint8Array> {
    this.asked.push(`${key}${body.length ? `#${[...body].join(",")}` : ""}`);
    const r = this.f.responses;
    if (key === "sys/info") return unhex(r["sys/info"]);
    if (key === "sys/manifest") return unhex(r["sys/manifest"]);
    if (key === "sys/manifest/cm") {
      const page = body[0] ?? 0;
      const hex = page === this.drop ? undefined : r["sys/manifest/cm"][page];
      if (hex) return unhex(hex);
    }
    throw new RequestError(key, "NotFound", NOT_FOUND_CODE);
  }
  async close(): Promise<void> {}
  override board(): never {
    throw new Error("the probe must not ask sys/board");
  }
  override peers(): never {
    throw new Error("the probe must not ask sys/peers");
  }
  override trustInfo(): never {
    throw new Error("the probe must not ask sys/trust/info");
  }
}

describe("probe (KD240 fixture)", () => {
  const f = fixture("kd240.wire.json");
  const r = decodeProbe(proto, input(f));

  it("matches the golden plant and drafts", () => {
    const path = new URL("./fixtures/kd240.plant.json", import.meta.url);
    const text = golden(r);
    if (process.env.MW_BLESS_FIXTURES === "1") writeFileSync(path, text);
    expect(text).toBe(readFileSync(path, "utf8"));
  });

  it("lists the ten points and groups them into the lathe's equipment modules", () => {
    expect(r.points.map((p) => p.name)).toEqual([
      "x_pos",
      "z_pos",
      "x_home",
      "z_home",
      "spindle_rpm",
      "spindle_actual",
      "turret_station",
      "turret_ready",
      "estop",
      "motion",
    ]);
    expect(r.unit.ems.map((e) => [e.id, [...e.cms]])).toEqual([
      ["motion", ["x_pos", "z_pos", "x_home", "z_home", "motion"]],
      ["spindle", ["spindle_rpm", "spindle_actual"]],
      ["turret", ["turret_station", "turret_ready"]],
      ["estop", ["estop"]],
    ]);
    expect(r.unit.node).toBe(f.facts.node);
    expect(r.node).toBe(f.facts.node);
    expect(r.chip).toBe("linux-aarch64");
    expect(r.boardName).toBe("ZynqMP KD240 revA/B/1");
    expect(r.unit.board).toBe("kd240");
    expect(r.unit.cms).toEqual({ rows: [], buses: [] });
    expect(r.problems).toEqual([]);
    expect(plantProblem(JSON.parse(serializePlant(r.plant)))).toBeNull();
  });

  it("drafts the lathe's IoMap (its defaults) and the binding specs mwctl takes", () => {
    // cnc/model IoMap::default(), as its camelCase JSON.
    expect(r.io).toEqual({
      xPos: "x_pos",
      zPos: "z_pos",
      xHome: "x_home",
      zHome: "z_home",
      spindleRpm: "spindle_rpm",
      spindleActual: "spindle_actual",
      turretStation: "turret_station",
      turretReady: "turret_ready",
      estop: "estop",
      motion: "motion",
    });
    expect(Object.keys(r.io)).toEqual(
      Object.keys(r.io)
        .slice()
        .sort((a, b) => keyOrder(a) - keyOrder(b)),
    );
    expect(r.ioSpecs).toHaveLength(11);
    expect(r.ioSpecs[0]).toBe("x_pos=driver:x_pos");
    expect(r.ioSpecs.at(-1)).toBe("queue=driver:motion");
    expect(r.binding.entries).toHaveLength(11);
  });

  it("drafts a binding that checks clean against the node's real manifest", async () => {
    // A minimal payload declaring the lathe's eleven machine channels.
    const empty = new Uint8Array([0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00]);
    const wasm = proto.withChannels(empty, proto.encodeChannels(latheChannelDecls()));
    expect(proto.readChannels(wasm).map((c) => c.name)).toEqual(LATHE_CHANNELS.map((c) => c.channel));
    const sha = await sha256(wasm);
    const record = bindingForImage(r.binding, sha, 7);
    expect(record.version).toBe(7);
    expect(record.entries[0]).toEqual({ name: "x_pos", target: { kind: "driver", point: "x_pos" } });
    expect(proto.checkBinding(unhex(f.responses["sys/manifest"]), wasm, proto.encodeBinding(record))).toEqual([]);
  });

  it("writes the mwctl commands that deploy over the mesh", () => {
    const sender = "5e110de02a9c0102";
    const cmds = deployCommands(r, { site: "example.org", sender, wasm: "lathe.wasm" });
    const z = `mwctl --zenoh tls/mesh.updates.example.org:443 --node ${sender}`;
    expect(cmds).toEqual([
      `${z} upload lathe.wasm --key <KEYFILE>`,
      `${z} bind lathe.wasm ${r.ioSpecs.join(" ")} --key <KEYFILE>`,
      `${z} start --key <KEYFILE>`,
    ]);
  });
});

const IO_ORDER = [
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
const keyOrder = (k: string) => IO_ORDER.indexOf(k);

describe("probeNode", () => {
  it("reads info, the manifest and every page in order, and nothing else", async () => {
    const f = fixture("kd240-paged.wire.json");
    const c: DeviceClient = new FixtureClient(proto, f);
    const r = await probeNode(c, proto);
    const pages = f.responses["sys/manifest/cm"].length;
    expect(pages).toBeGreaterThan(1);
    expect((c as FixtureClient).asked).toEqual([
      "sys/info",
      "sys/manifest",
      ...Array.from({ length: pages }, (_, p) => `sys/manifest/cm#${p}`),
    ]);
    expect(r.points).toHaveLength(64);
    expect(r.points[63].name).toBe("point_with_long_name_63");
    // None of these points is the lathe's: every IoMap channel is unmatched.
    expect(r.unit.ems).toEqual([]);
    expect(r.io.xPos).toBeNull();
    expect(r.ioSpecs).toEqual([]);
    expect(r.problems.filter((p) => p.startsWith("no point for the lathe's channel"))).toHaveLength(11);
    expect(plantProblem(JSON.parse(serializePlant(r.plant)))).toBeNull();
  });

  it("reports a missing page and a page count that changes", async () => {
    const f = fixture("kd240-paged.wire.json");
    const r = await probeNode(new FixtureClient(proto, f, 2), proto);
    expect(r.problems).toContain("sys/manifest/cm page 2 is missing (the manifest lists 6 pages)");
    expect(r.points.length).toBeLessThan(64);
    const bad = input(f);
    bad.pages = [bad.pages[0], bad.pages[0]];
    expect(decodeProbe(proto, bad).problems).toContain("sys/manifest/cm page 1 came back as page 0");
  });

  it("refuses a node without a manifest or control points", async () => {
    const f = fixture("kd240.wire.json");
    const noPoints = { ...input(f), pages: [] };
    expect(decodeProbe(proto, noPoints).problems).toContain(
      "sys/manifest/cm page 0 is missing (the manifest lists 1 page)",
    );
  });
});
