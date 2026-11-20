// @vitest-environment jsdom
// U49: the Device tab's Mesh section over a fake link serving the KD240
// fixture: Scan lists the node, Probe shows its points, the IoMap and the
// mwctl commands, and "Use as plant" hands the drafted plant on.

import { readFileSync } from "node:fs";
import { join } from "node:path";
import { describe, expect, it, vi } from "vitest";
import { NOT_FOUND_CODE, RequestError } from "../src/device/client";
import type { Discovered, MeshLink } from "../src/device/mesh";
import type { PlantModel } from "../src/model/plant";
import { loadProtocolSync } from "../src/protocol/wasm";
import { meshPanel } from "../src/ui/meshPanel";

const proto = loadProtocolSync(readFileSync(join(process.cwd(), "src/pkg/protocol_wasm_bg.wasm")));
const f = JSON.parse(readFileSync(join(process.cwd(), "test/fixtures/kd240.wire.json"), "utf8")) as {
  responses: { "sys/info": string; "sys/manifest": string; "sys/manifest/cm": string[] };
};
const unhex = (s: string) => new Uint8Array(s.match(/../g)!.map((b) => parseInt(b, 16)));
const SENDER = "5e110de02a9c0102";

class FakeLink implements MeshLink {
  readonly senderId = "00000000000000aa";
  up = true;
  readonly asked: string[] = [];
  async request(sender: string | null, key: string, body: Uint8Array): Promise<Uint8Array> {
    this.asked.push(`${sender} ${key}`);
    if (key === "sys/info") return unhex(f.responses["sys/info"]);
    if (key === "sys/manifest") return unhex(f.responses["sys/manifest"]);
    const page = f.responses["sys/manifest/cm"][body[0] ?? 0];
    if (key === "sys/manifest/cm" && page) return unhex(page);
    throw new RequestError(key, "NotFound", NOT_FOUND_CODE);
  }
  async discover(): Promise<Discovered[]> {
    return [{ sender: SENDER, body: unhex(f.responses["sys/info"]) }];
  }
  async close(): Promise<void> {
    this.up = false;
  }
  onClosed(): void {}
}

const settle = async () => {
  for (let i = 0; i < 20; i++) await new Promise((r) => setTimeout(r, 1));
};
const button = (el: HTMLElement, text: string) =>
  [...el.querySelectorAll("button")].find((b) => b.textContent === text)!;

describe("mesh panel", () => {
  it("connects, scans, probes and offers the plant and the mwctl commands", async () => {
    const link = new FakeLink();
    const connect = vi.fn(async () => link);
    const used: PlantModel[] = [];
    const selected: unknown[] = [];
    const panel = meshPanel(proto, {
      connect,
      defaultUrl: "wss://updates.example.org/mesh",
      usePlant: (p) => used.push(p),
      onSelect: (c) => selected.push(c),
    });
    document.body.append(panel.el);
    const url = panel.el.querySelector("input") as HTMLInputElement;
    expect(url.value).toBe("wss://updates.example.org/mesh");
    expect(button(panel.el, "Scan").disabled).toBe(true);
    button(panel.el, "Connect").click();
    await settle();
    expect(connect).toHaveBeenCalledWith("wss://updates.example.org/mesh");
    button(panel.el, "Scan").click();
    await settle();
    const rows = [...panel.el.querySelectorAll(".mesh-nodes tbody tr")].map((tr) => tr.textContent);
    expect(rows).toHaveLength(1);
    expect(rows[0]).toContain(SENDER);
    expect(rows[0]).toContain("4b44323430000001");
    expect(rows[0]).toContain("linux-aarch64");
    button(panel.el, "Probe").click();
    await settle();
    expect(link.asked.every((a) => a.startsWith(`${SENDER} `))).toBe(true);
    const text = panel.el.textContent ?? "";
    expect(text).toContain("ZynqMP KD240 revA/B/1");
    expect(panel.el.querySelectorAll(".mesh-points tbody tr")).toHaveLength(10);
    expect(text).toContain("queue=driver:motion");
    expect(text).toContain('"xPos": "x_pos"');
    expect(text).toContain(`mwctl --zenoh tls/mesh.updates.example.org:443 --node ${SENDER} upload`);
    expect(text).toContain("probe-only");
    expect(selected.at(-1)).toMatchObject({ remote: true });
    button(panel.el, "Use as plant").click();
    expect(used).toHaveLength(1);
    expect(used[0].physical.units[0].node).toBe("4b44323430000001");
    // Disconnect drops the selection.
    button(panel.el, "Disconnect").click();
    await settle();
    expect(link.up).toBe(false);
    expect(selected.at(-1)).toBeNull();
    panel.el.remove();
  });
});
