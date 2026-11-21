// U44, against a real router (skipped unless MW_MESH_URL is set): the
// browser's session over zenohd's WebSocket listener finds a posix-node and
// probes it. This is what shows the ws link carries Zenoh batches with no
// length prefix. Run:
//
//   zenohd -l ws/127.0.0.1:8000 -l tcp/127.0.0.1:7447 --no-multicast-scouting
//   MW_ZENOH_ROUTER=tcp/127.0.0.1:7447 MW_POSIX_NODE_DRIVER=sim \
//     MW_POSIX_NODE_STATE=<dir> MW_POSIX_NODE_NODE_ID=<16 hex> mw-posix-node
//   MW_MESH_URL=ws://127.0.0.1:8000 npx vitest run test/mesh.live.test.ts
//
// Node 22 or later has the global WebSocket.

import { describe, expect, it } from "vitest";
import { MeshClient, MeshTransport, scan } from "../src/device/mesh";
import { probeNode } from "../src/device/probe";
import { protocol } from "./fakeDevice";

const url = process.env.MW_MESH_URL;

describe.skipIf(!url)("mesh against a live router", () => {
  it("scans the posix-node simulator and probes its ten points", async () => {
    const proto = protocol();
    const t = await MeshTransport.connect(url!, proto);
    try {
      const nodes = await scan(t, proto);
      const want = process.env.MW_MESH_NODE;
      const dev = nodes.find((n) => n.info && (!want || n.info.node === want));
      expect(dev, JSON.stringify(nodes.map((n) => [n.sender, n.info?.node, n.problem]))).toBeDefined();
      const r = await probeNode(new MeshClient(t, dev!, proto), proto);
      expect(r.points).toHaveLength(10);
      expect(r.problems.filter((p) => !p.startsWith("no known board"))).toEqual([]);
      expect(r.ioSpecs).toHaveLength(11);
      // A key the cloud would drop is refused here without a query.
      await expect(new MeshClient(t, dev!, proto).startPayload()).rejects.toThrow(/probe-only/);
      // The session is still up after the probe.
      expect(t.up).toBe(true);
    } finally {
      await t.close();
    }
  });
});
