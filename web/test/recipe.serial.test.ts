// Real boards as units (design.md Q23), against the fake serial device
// running what it is sent and routing served keys like rp-base (M7): the
// Web Serial requester (device/requester.ts) has SimBus's request shape
// over DeviceClient; the Device tab's unit upload (device/unitUpload.ts)
// installs the LED unit's generated phase server on the "board"; and a
// mixed plant (pico-led on the board, pico-sense simulated on a SimBus)
// reproduces payload-sim p3-p5 through RecipeRunner: Complete; Hold,
// Held, Restart, Complete; the board's cable pulled mid-run holds the
// batch, Restart is refused, Abort ends it Aborted.

import { beforeAll, describe, expect, it } from "vitest";
import { DeviceClient } from "../src/device/client";
import { SerialRequester, canServe, serveProblem } from "../src/device/requester";
import { boardUnitsNote, unitsOnBoard, uploadUnit } from "../src/device/unitUpload";
import { bindNode } from "../src/model/plantEdit";
import { BLINK_THEN_MEASURE, EXAMPLE_PLANT } from "../src/model/plantExample";
import type { DeviceInfo } from "../src/protocol/wasm";
import { SimBus } from "../src/sim/bus";
import { MixedNet } from "../src/sim/net";
import { RecipeRunner, buildUnit } from "../src/sim/recipeRun";
import { ManualClock } from "./clock";
import { FakeDevice, type FakeDeviceScript, protocol, serialPair } from "./fakeDevice";

const proto = protocol();
const LED_NODE = "e6613852831f2a0b";
const LED_KEY = `unit/${LED_NODE}/phase/LED.Blink`;
let senseWasm: Uint8Array;

beforeAll(async () => {
  senseWasm = await buildUnit(EXAMPLE_PLANT, "pico-sense", proto.abi);
}, 60_000);

/** Lets stream I/O, the fake's answers and the requester's deferred
 * publications run (real tasks; the clocks are virtual). */
async function settle(turns = 3): Promise<void> {
  for (let i = 0; i < turns; i++) await new Promise((r) => setTimeout(r, 0));
}

const bound = bindNode(EXAMPLE_PLANT, "pico-led", LED_NODE);

/** A board on a serial pair with a client whose publications and close
 * reach `req` once it exists. */
function board(script: FakeDeviceScript) {
  const { host, device } = serialPair();
  const fake = new FakeDevice(proto, device, script);
  const hooks: { req: SerialRequester | null } = { req: null };
  const client = new DeviceClient(host, proto, {
    onPublish: (k, d) => hooks.req?.publication(k, d),
    onClosed: () => hooks.req?.closed(),
  });
  client.start();
  return { fake, client, hooks };
}

/** The mixed plant: pico-led on the fake board (its payload uploaded
 * through the Device tab's path), pico-sense in the simulator. */
async function mixedPlant() {
  const clock = new ManualClock();
  const { fake, client, hooks } = board({ runs: { clock, node: LED_NODE } });
  await uploadUnit(client, proto, bound, "pico-led", null, { confirmUnsigned: async () => true });
  const info: DeviceInfo = await client.info();
  const req = new SerialRequester(client, info);
  hooks.req = req;
  const bus = new SimBus(proto, clock);
  bus.add("pico-sense", senseWasm).setAdc(26, 2048);
  const net = new MixedNet(bus);
  net.attach("pico-led", req);
  const run = new RecipeRunner(proto, bound, BLINK_THEN_MEASURE, net, clock);
  run.start(10);
  const to = async (t: number) => {
    while (clock.t < t) {
      clock.advance(10);
      await settle();
    }
  };
  const recipeStates = () => run.trace.filter((l) => l.type === "recipe").map((l) => l.text.split(" -> ")[1]);
  return { clock, fake, client, req, bus, net, run, to, recipeStates, info };
}

describe("the Web Serial requester (a board as a unit)", () => {
  it("installs a unit's payload through the Device tab's upload; sys/info says the board serves phases", async () => {
    const clock = new ManualClock();
    const { fake, client } = board({ runs: { clock, node: LED_NODE } });
    expect(unitsOnBoard(bound, LED_NODE.toUpperCase()).map((u) => u.id)).toEqual(["pico-led"]);
    expect(unitsOnBoard(bound, "00000000000000aa")).toEqual([]);
    const sent = await uploadUnit(client, proto, bound, "pico-led", null, { confirmUnsigned: async () => true });
    expect(sent).toMatchObject({ kind: "unsigned", reason: "legacy-firmware" });
    expect(fake.started).toBe(true);
    expect(fake.session?.serves).toEqual([LED_KEY]);
    const info = await client.info();
    expect(info).toMatchObject({ node: LED_NODE, caps: 1, serves: true, statusName: "running" });
    expect(canServe(info)).toBe(true);
    expect(boardUnitsNote(bound, info)).toBe("Bound here: pico-led. Upload a unit's payload, then run the recipe from Blocks → Recipe.");
    await client.close();
    await fake.close();
  });

  it("answers like SimBus: the payload's reply or error by name, DeadlineExceeded, NotFound, Unavailable, lost once closed", async () => {
    const clock = new ManualClock();
    let dropNext = false;
    const { fake, client, hooks } = board({ runs: { clock, node: LED_NODE }, drop: () => (dropNext ? ((dropNext = false), true) : false) });
    await uploadUnit(client, proto, bound, "pico-led", null, { confirmUnsigned: async () => true });
    const req = new SerialRequester(client, await client.info());
    hooks.req = req;
    const reports: string[] = [];
    req.onPublish((k) => reports.push(k));
    // A malformed body is the payload's own InvalidRequest.
    expect(await req.request(LED_KEY, new Uint8Array([9]), { deadlineMs: 200 })).toEqual({ ok: false, error: "InvalidRequest" });
    // An unserved application key: NotFound from the firmware.
    expect(await req.request(`unit/${LED_NODE}/phase/LED.Other`, new Uint8Array(), { deadlineMs: 200 })).toEqual({ ok: false, error: "NotFound" });
    // No answer within the deadline.
    dropNext = true;
    expect(await req.request(LED_KEY, new Uint8Array([9]), { deadlineMs: 30 })).toEqual({ ok: false, error: "DeadlineExceeded" });
    // A stopped payload's key is Unavailable (resumable), not NotFound.
    await client.stopPayload();
    expect(await req.request(LED_KEY, new Uint8Array([9]), { deadlineMs: 200 })).toEqual({ ok: false, error: "Unavailable" });
    // Pulling the cable: the requester says so once; requests are lost.
    const presence: boolean[] = [];
    req.onPresence((c) => presence.push(c));
    await fake.close();
    await settle();
    expect(presence).toEqual([false]);
    expect(req.connected).toBe(false);
    expect(await req.request(LED_KEY, new Uint8Array(), { deadlineMs: 50 })).toBeNull();
    await client.close();
  });

  it("a board without the capability bit cannot serve phases, in plain words", async () => {
    const clock = new ManualClock();
    const { fake, client } = board({ runs: { clock, node: LED_NODE, caps: 0 } });
    const info = await client.info();
    expect(info).toMatchObject({ node: LED_NODE, caps: 0, serves: false });
    expect(canServe(info)).toBe(false);
    expect(serveProblem(info)).toBe(
      "This board (rp2350 0.1.0-fake) cannot serve phases: its firmware does not pass requests to the payload. Update its firmware on the Flash tab, or run this unit in the simulator.",
    );
    expect(boardUnitsNote(bound, info)).toMatch(/^Bound here: pico-led\. This board .* cannot serve phases/);
    expect(serveProblem({ ...info, node: undefined })).toMatch(/does not report a node id/);
    expect(boardUnitsNote(null, info)).toMatch(/Open a plant/);
    expect(boardUnitsNote(bound, null)).toMatch(/Connect a board/);
    await client.close();
    await fake.close();
  });
});

describe("a mixed plant: the LED unit on a board, the sensor simulated (payload-sim p3-p5)", () => {
  it("p3: Blink then Measure runs to Complete", async () => {
    const p = await mixedPlant();
    expect(p.net.simulated("pico-led")).toBe(false);
    expect(p.net.simulated("pico-sense")).toBe(true);
    expect(p.net.node("pico-led")).toBe(LED_NODE);
    p.run.operator("Start");
    await p.to(3_000);
    expect(p.run.state, p.run.trace.map((l) => `${l.t} ${l.text}`).join("\n")).toBe("Complete");
    expect(p.recipeStates()).toEqual(["Running", "Complete"]);
    expect(p.run.sent).toEqual([
      { unit: "pico-led", command: "Start", outcome: "ok" },
      { unit: "pico-led", command: "Reset", outcome: "ok" },
      { unit: "pico-sense", command: "Start", outcome: "ok" },
      { unit: "pico-sense", command: "Reset", outcome: "ok" },
    ]);
    // The board's request went out under its node id.
    expect(p.fake.received.filter((r) => r.key === LED_KEY).length).toBe(2);
    expect(p.run.phases().map((ph) => [ph.unit, ph.state])).toEqual([
      ["pico-led", "Idle"],
      ["pico-sense", "Idle"],
    ]);
    // The board's reply is taken before the reports it caused.
    expect(p.run.trace.filter((l) => l.type === "ignored")).toEqual([]);
    p.run.dispose();
    await p.client.close();
    await p.fake.close();
  });

  it("p4: a Hold mid-Blink holds the batch and Restart finishes it", async () => {
    const p = await mixedPlant();
    p.run.operator("Start");
    await p.to(300);
    p.run.operator("Hold");
    await p.to(1_000);
    expect(p.run.state).toBe("Held");
    expect(p.run.sent.some((s) => s.unit === "pico-sense")).toBe(false);
    p.run.operator("Restart");
    await p.to(4_000);
    expect(p.run.state).toBe("Complete");
    expect(p.recipeStates()).toEqual(["Running", "Holding", "Held", "Restarting", "Running", "Complete"]);
    expect(p.run.sent.filter((s) => s.unit === "pico-led").map((s) => s.command)).toEqual(["Start", "Hold", "Restart", "Reset"]);
    p.run.dispose();
    await p.client.close();
    await p.fake.close();
  });

  it("p5: pulling the board's cable holds, Restart is refused, Abort ends Aborted", async () => {
    const p = await mixedPlant();
    p.run.operator("Start");
    await p.to(100);
    await p.fake.close();
    await p.to(200);
    p.run.operator("Restart");
    await p.to(300);
    p.run.operator("Abort");
    await p.to(2_000);
    const texts = p.run.trace.map((l) => l.text);
    expect(texts).toContain("unit pico-led left the network (resumable: hold)");
    expect(texts).toContain("Restart refused: unit pico-led is offline");
    expect(p.run.state).toBe("Aborted");
    expect(p.run.trace.some((l) => l.t >= 110 && l.type === "send" && l.text.includes("-> pico-led/"))).toBe(false);
    p.run.dispose();
    await p.client.close();
  });
});

describe("a board that comes back", () => {
  it("reopened on the same node tells the run it is found; another node is refused", async () => {
    const clock = new ManualClock();
    const a = board({ runs: { clock, node: LED_NODE } });
    const info = await a.client.info();
    const req = new SerialRequester(a.client, info);
    a.hooks.req = req;
    const seen: boolean[] = [];
    req.onPresence((c) => seen.push(c));
    expect(req.reopened(a.client, info)).toBe(false); // still connected
    await a.fake.close();
    await settle();
    const b = board({ runs: { clock, node: "00000000000000aa" } });
    expect(req.reopened(b.client, await b.client.info())).toBe(false);
    const c = board({ runs: { clock, node: LED_NODE } });
    const again = await c.client.info();
    expect(req.reopened(c.client, again)).toBe(true);
    expect(seen).toEqual([false, true]);
    expect(req.info).toBe(again);
    expect(await req.request("sys/info", new Uint8Array(), {})).toMatchObject({ ok: true });
    for (const x of [a, b, c]) await x.client.close();
    await b.fake.close();
    await c.fake.close();
  });
});
