// @vitest-environment jsdom
// U44: over the mesh the trust panel only reads: it shows the node's trust
// state from sys/trust/info, and Claim stays disabled (lathe nodes are
// claimed with mwctl and the lathe's key).

import { readFileSync } from "node:fs";
import { join } from "node:path";
import { describe, expect, it } from "vitest";
import { RequestClient, type RequestOptions } from "../src/device/client";
import { loadProtocolSync } from "../src/protocol/wasm";
import { memoryStorage } from "../src/trust/keyStore";
import { encodeTrustInfo } from "../src/trust/wire";
import { trustPanel } from "../src/ui/trustPanel";

const proto = loadProtocolSync(readFileSync(join(process.cwd(), "src/pkg/protocol_wasm_bg.wasm")));

class Remote extends RequestClient {
  readonly transport = "mesh" as const;
  readonly remote = true;
  claims = 0;
  constructor(private readonly state: "unclaimed" | "claimed") {
    super(proto);
  }
  async request(key: string, _body?: Uint8Array, _opts?: RequestOptions): Promise<Uint8Array> {
    if (key === "sys/trust/info") {
      return encodeTrustInfo({
        state: this.state,
        minVersion: 3,
        key: this.state === "claimed" ? new Uint8Array(32).fill(9) : null,
      });
    }
    throw new Error(`unexpected ${key}`);
  }
  async trustClaim(): Promise<void> {
    this.claims++;
  }
  async close(): Promise<void> {}
}

const until = async (f: () => boolean) => {
  for (let i = 0; i < 50 && !f(); i++) await new Promise((r) => setTimeout(r, 5));
};

describe("trust panel over the mesh", () => {
  it("shows the node's trust state and keeps Claim disabled", async () => {
    for (const state of ["unclaimed", "claimed"] as const) {
      const panel = trustPanel(memoryStorage());
      document.body.append(panel.el);
      const c = new Remote(state);
      panel.setClient(c);
      const text = () => panel.el.querySelector("dl")?.textContent ?? "";
      await until(() => text().includes(state === "claimed" ? "claimed" : "unclaimed") && !text().includes("reading"));
      expect(text()).toContain(state);
      expect(text()).toContain("3");
      const claim = [...panel.el.querySelectorAll("button")].find((b) => b.textContent === "Claim device")!;
      expect(claim.disabled).toBe(true);
      expect(claim.title).toMatch(/mwctl/);
      claim.click();
      expect(c.claims).toBe(0);
      expect(panel.uploadNote()).toEqual({ text: expect.stringMatching(/probe-only.*mwctl/), warn: true });
      panel.el.remove();
    }
  });
});
