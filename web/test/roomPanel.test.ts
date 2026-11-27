// @vitest-environment jsdom
//
// The Room panel in a DOM (jsdom): two pages join a room from their
// panels, each runs a payload on the panel's network, and each panel
// shows the other node, its channels and how they pair; the counters
// follow the traffic. The server and the data channels are fakeMesh.ts's.

import { readFileSync } from "node:fs";
import { join } from "node:path";
import { afterEach, describe, expect, it } from "vitest";
import { compileWat } from "../src/editor/compile";
import { loadProtocolSync } from "../src/protocol/wasm";
import { ROOM_SETTINGS_KEY, type RoomPanel, parseDeclared, roomPanel } from "../src/ui/roomPanel";
import { ManualClock } from "./clock";
import { World, settle } from "./fakeMesh";

// jsdom gives modules an http URL, so the files are found from the package root.
const proto = loadProtocolSync(readFileSync(join(process.cwd(), "src/pkg/protocol_wasm_bg.wasm")));
const WAT = readFileSync(join(process.cwd(), "test/fixtures/level_channels.wat"), "utf8");
const node = (n: number) => n.toString(16).padStart(16, "0");

const panels: RoomPanel[] = [];
afterEach(() => {
  for (const p of panels.splice(0)) p.dispose();
  document.body.replaceChildren();
});

/** The fixture without a channels section, as the Editor compiles it. */
async function repeater(input: string, output: string): Promise<Uint8Array> {
  const r = await compileWat(WAT.replace('(i32.const 32) "audio")', `(i32.const 32) "${input}")`).replace('(i32.const 48) "level")', `(i32.const 48) "${output}")`));
  if (!r.ok) throw new Error(r.errors);
  return r.wasm;
}

function storage(initial: Record<string, string> = {}) {
  const m = new Map(Object.entries(initial));
  return { getItem: (k: string) => m.get(k) ?? null, setItem: (k: string, v: string) => void m.set(k, v), m };
}

function mount(world: World, n: number, store = storage()) {
  const clock = new ManualClock();
  const panel = roomPanel({ proto, node: node(n), storage: store, env: world.env(n), clock, refreshMs: 0 });
  panels.push(panel);
  document.body.append(panel.el);
  const inputs = [...panel.el.querySelectorAll("input")];
  const labelled = (text: string) => inputs.find((i) => i.closest("label")?.textContent?.startsWith(text))!;
  const button = (text: string) => [...panel.el.querySelectorAll("button")].find((b) => b.textContent === text)!;
  const type = (el: HTMLInputElement, value: string) => {
    el.value = value;
    el.dispatchEvent(new Event("change", { bubbles: true }));
  };
  return { panel, clock, store, labelled, button, type, text: () => panel.el.textContent ?? "", status: () => panel.el.querySelector('[role="status"]')!.textContent };
}

describe("the Room panel (BM10)", () => {
  it("joins from the form, shows the peer with its channels, and counts the traffic", async () => {
    const world = new World();
    const a = mount(world, 1);
    const b = mount(world, 2);
    // Every control has a label; the server is the public one until changed.
    for (const el of a.panel.el.querySelectorAll("input")) expect(el.closest("label")?.textContent?.trim()).toBeTruthy();
    expect(a.labelled("Signalling server").value).toBe("0.peerjs.com");
    expect(a.text()).toContain(`This page is node ${node(1)}`);
    expect(a.text()).toMatch(/Peers are not checked/);
    expect(a.status()).toBe("Not in a room.");

    // A name that is not a room's is said so, and nothing is joined.
    a.type(a.labelled("Room"), "My Room");
    a.button("Join").click();
    await settle();
    expect(a.status()).toMatch(/A room name is 1 to 24 of a-z, 0-9 and _/);
    expect(a.panel.mesh).toBeNull();

    for (const p of [a, b]) {
      p.type(p.labelled("Signalling server"), "signal.test");
      p.type(p.labelled("Port"), "9000");
      (p.panel.el.querySelector('input[type="checkbox"]') as HTMLInputElement).checked = false;
      p.type(p.labelled("Room"), "lab");
    }
    // The payloads declare nothing themselves: the panel's field does.
    a.type(a.labelled("Channels of a payload"), "audio:in level:out");
    b.type(b.labelled("Channels of a payload"), "level:in, heard:out");
    const published: string[] = [];
    a.panel.run(await repeater("audio", "level"));
    const sb = b.panel.run(await repeater("level", "heard"), { onEvent: (e) => e.kind === "publish" && published.push(`${e.key} ${[...e.data]}`) });
    expect(sb.nodeId).toBe(node(2));
    expect(sb.subscriptions).toEqual(["room/lab/level"]);

    a.button("Join").click();
    b.button("Join").click();
    await world.run(4000);
    expect(world.urls[0]).toBe(`ws://signal.test:9000/peerjs?key=peerjs&id=mw-lab-${node(1)}&token=${new URL(world.urls[0]!).searchParams.get("token")}`);
    expect(a.status()).toBe("In room lab (peers from the server's list).");
    expect(a.button("Join").disabled).toBe(true);
    expect(a.button("Leave").disabled).toBe(false);
    expect(a.labelled("Room").disabled).toBe(true);
    const row = () => [...b.panel.el.querySelectorAll("tbody tr")].map((tr) => [...tr.querySelectorAll("td")].map((td) => td.textContent));
    expect(row()).toEqual([[node(1), "linked (browser)", "audio (reads), level (writes)", "level comes from this peer", "0 received"]]);
    expect(a.text()).toContain("level goes to this peer");
    expect(a.text()).not.toMatch(/Start the simulation again/);

    // Page 1's payload gets `audio` from its own page and answers on
    // `level`, which page 2's payload reads.
    const sa = a.panel.mesh!;
    expect(sa.publish("room/lab/unrelated", Uint8Array.of(0))).toBe(true);
    a.panel.run(await repeater("audio", "level"), {}).deliver("room/lab/audio", Uint8Array.of(5, 6));
    a.clock.advance(10);
    await settle();
    b.clock.advance(10);
    expect(published).toEqual(["room/lab/heard 5,6"]);
    a.button("Leave").click();
    await settle();
    expect(a.status()).toBe("Not in a room.");
    expect(b.text()).toMatch(/Sent 1, received 2, refused 0/);
    expect(b.text()).toMatch(/Nobody else is linked yet/);

    // The settings are kept for the next visit.
    const again = mount(world, 1, a.store);
    expect(again.labelled("Room").value).toBe("lab");
    expect(again.labelled("Port").value).toBe("9000");
    expect(again.labelled("Channels of a payload").value).toBe("audio:in level:out");
    expect(JSON.parse(a.store.m.get(ROOM_SETTINGS_KEY)!).server.host).toBe("signal.test");
  });

  it("says when the running payload is not in the room, and why a join failed", async () => {
    const world = new World();
    const a = mount(world, 1);
    a.type(a.labelled("Signalling server"), "signal.test");
    // Started with no room named: its channels are not in any.
    a.type(a.labelled("Channels of a payload"), "audio:in level:out");
    a.panel.run(await repeater("audio", "level"));
    a.type(a.labelled("Room"), "lab");
    a.button("Join").click();
    await world.run(100);
    expect(a.text()).toMatch(/Start the simulation again to put them in the room/);
    a.panel.run(await repeater("audio", "level"));
    expect(a.text()).not.toMatch(/Start the simulation again/);
    a.button("Leave").click();

    world.down = true;
    a.button("Join").click();
    await world.run(100);
    expect(a.status()).toMatch(/^Could not join: .*signalling server/);
    expect(a.button("Join").disabled).toBe(false);
    a.type(a.labelled("Signalling server"), "bad host/");
    a.button("Join").click();
    await settle();
    expect(a.status()).toMatch(/bad server address/);
  });

  it("reads the declared channels and says what is wrong with them", () => {
    expect(parseDeclared(" audio:in, level:out ").map((c) => `${c.name} ${c.dir} ${c.kind} ${c.required}`)).toEqual(["audio in bytes false", "level out bytes false"]);
    expect(parseDeclared("")).toEqual([]);
    expect(() => parseDeclared("Audio:in")).toThrow(/"Audio:in" is not name:in or name:out/);
    expect(() => parseDeclared("audio")).toThrow();
  });
});
