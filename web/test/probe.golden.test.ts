// U47/U48: the probe of a KD240, from the wire fixtures posix-node records
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
  shellQuote,
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
    private readonly opts: { noManifest?: boolean } = {},
  ) {
    super(proto);
  }
  async request(key: string, body: Uint8Array = new Uint8Array(), _opts?: RequestOptions): Promise<Uint8Array> {
    this.asked.push(`${key}${body.length ? `#${[...body].join(",")}` : ""}`);
    const r = this.f.responses;
    if (key === "sys/info") return unhex(r["sys/info"]);
    if (key === "sys/manifest" && !this.opts.noManifest) return unhex(r["sys/manifest"]);
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

  it("writes the mwctl commands that deploy over the mesh, every argument single-quoted", () => {
    const sender = "5e110de02a9c0102";
    const plan = deployCommands(r, { site: "example.org", sender, wasm: "lathe.wasm" });
    const z = `mwctl '--zenoh' 'tls/mesh.updates.example.org:443' '--node' '${sender}'`;
    const specs = r.ioSpecs.map((x) => `'${x}'`).join(" ");
    expect(plan).toEqual({
      ok: true,
      commands: [
        `${z} 'upload' 'lathe.wasm' '--key' '<KEYFILE>'`,
        `${z} 'bind' 'lathe.wasm' ${specs} '--key' '<KEYFILE>'`,
        `${z} 'start' '--key' '<KEYFILE>'`,
      ],
    });
    // Golden: the first command exactly.
    expect(plan.ok && plan.commands[0]).toBe(
      "mwctl '--zenoh' 'tls/mesh.updates.example.org:443' '--node' '5e110de02a9c0102' 'upload' 'lathe.wasm' '--key' '<KEYFILE>'",
    );
  });

  it("writes no command when a value came from the mesh in a shape it may not have", () => {
    const ok = { site: "example.org", sender: "5e110de02a9c0102", wasm: "lathe.wasm" };
    const hostileSpecs = [
      "x_pos=driver:$(rm -rf ~)",
      "x_pos=driver:a;reboot",
      "x_pos=driver:`id`",
      "x_pos=driver:a b",
      "x_pos=driver:a'b",
      "$(id)=driver:x_pos",
      "x_pos=key:app/x",
      "x_pos=driver:" + "a".repeat(25),
    ];
    for (const spec of hostileSpecs) {
      const plan = deployCommands({ ...r, ioSpecs: [spec] }, ok);
      expect(plan, spec).toEqual({ ok: false, refused: expect.stringMatching(/^refused: untrusted value/) });
    }
    for (const sender of ["$(id)", "5e110de02a9c010", "5E110DE02A9C0102", "5e110de02a9c0102;x", ""]) {
      expect(deployCommands(r, { ...ok, sender }).ok, sender).toBe(false);
    }
    for (const site of [
      "evil.org;reboot",
      "$(id).org",
      "example",
      "-x.org",
      "a..org",
      "ex ample.org",
      "x.org'",
      null,
    ]) {
      const plan = deployCommands(r, { ...ok, site });
      expect(plan.ok, String(site)).toBe(false);
    }
    expect(deployCommands(r, { ...ok, wasm: "a'b.wasm" }).ok).toBe(false);
    const refused = deployCommands(r, { ...ok, sender: "$(id)" });
    expect(refused.ok === false && refused.refused).toContain("sender");
  });

  it("single-quotes for the shell, closing and reopening around a quote", () => {
    expect(shellQuote("a")).toBe("'a'");
    expect(shellQuote("it's")).toBe("'it'\\''s'");
    expect(shellQuote("$(x)")).toBe("'$(x)'");
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

  it("reports a page whose page count is not the manifest's", () => {
    const f = fixture("kd240.wire.json");
    const bad = input(f);
    const page = bad.pages[0]!.slice();
    expect(page[1]).toBe(1); // the header's page count
    page[1] = 2;
    bad.pages = [page];
    expect(decodeProbe(proto, bad).problems).toContain("sys/manifest/cm page 0 says 2 pages, the manifest 1");
  });

  it("rejects a node that answers sys/manifest with NotFound", async () => {
    const f = fixture("kd240.wire.json");
    const c = new FixtureClient(proto, f, null, { noManifest: true });
    await expect(probeNode(c, proto)).rejects.toThrow(/no sys\/manifest/);
    expect(c.asked).toEqual(["sys/info", "sys/manifest"]);
  });

  it("reports a malformed manifest and one without control points", () => {
    const f = fixture("kd240.wire.json");
    const malformed = decodeProbe(proto, { ...input(f), manifest: new Uint8Array([1, 2]), pages: [] });
    expect(malformed.problems).toContain("sys/manifest is malformed");
    expect(malformed.points).toEqual([]);
    const plain = proto.simulatorManifest(f.facts.node)!;
    expect(proto.decodeManifest(plain)?.controlModules).toBeUndefined();
    const none = decodeProbe(proto, { ...input(f), manifest: plain, pages: input(f).pages });
    expect(none.problems).toContain("the manifest lists no control points (sys/manifest/cm)");
    expect(none.points).toEqual([]);
  });

  it("refuses a node without a manifest or control points", async () => {
    const f = fixture("kd240.wire.json");
    const noPoints = { ...input(f), pages: [] };
    expect(decodeProbe(proto, noPoints).problems).toContain(
      "sys/manifest/cm page 0 is missing (the manifest lists 1 page)",
    );
  });
});
