// U43: the protocol-wasm MeshSession as JS sees it (the real wasm): the
// handshake goes out as one batch per message with no length prefix, the
// session asks to be polled again, and junk input never throws.

import { describe, expect, it } from "vitest";
import { protocol } from "./fakeDevice";

const proto = protocol();

/** Zenoh transport message ids (low 5 bits of the header byte). */
const INIT = 0x01;

describe("MeshSession", () => {
  it("sends InitSyn unprefixed on open and asks for a later poll", () => {
    const s = proto.newMeshSession(new Uint8Array(16).fill(7));
    expect(s.senderId).toMatch(/^[0-9a-f]{16}$/);
    const id = s.open();
    expect(s.poll(0)).toEqual([]);
    const out = s.takeOutput();
    expect(out).toHaveLength(1);
    // The first byte is the InitSyn header itself, not a length.
    expect(out[0][0] & 0x1f).toBe(INIT);
    expect(s.nextWakeMs()).toBeGreaterThan(0);
    expect(id).toBeGreaterThan(0);
    // No more output until the router answers.
    expect(s.takeOutput()).toEqual([]);
  });

  it("fails a request before the session is open, and survives junk", () => {
    const s = proto.newMeshSession(new Uint8Array([1, 2, 3]));
    const id = s.request(undefined, "sys/info", new Uint8Array(), 1000);
    expect(s.poll(0)).toEqual([{ id, kind: "error", failure: "down" }]);
    const open = s.open();
    const done = s.poll(1);
    for (const junk of [new Uint8Array(), new Uint8Array([0xff, 0xff, 0xff]), new Uint8Array(9000)]) {
      s.feed(junk);
      done.push(...s.poll(2));
    }
    s.closed();
    done.push(...s.poll(3));
    // The handshake failed (on the junk or the closed link), exactly once.
    expect(done).toHaveLength(1);
    expect(done[0]).toMatchObject({ id: open, kind: "error" });
    expect(() => s.request("not hex", "sys/info", new Uint8Array(), 1000)).toThrow(/16 hex/);
    expect(() => proto.newMeshSession(new Uint8Array(16))).toThrow(/not all zero/);
  });
});
