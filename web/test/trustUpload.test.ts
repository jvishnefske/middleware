// U13/U15: claim and signed-upload flows through DeviceClient against a
// fake device that runs the Rust loader behind a TypeScript model of the
// trust contract (`FakeTrust`).

import { afterEach, describe, expect, it } from "vitest";
import { DeniedError, DeviceClient } from "../src/device/client";
import { type SigningKey, TrustRefusal, UploadError, trustedUpload, uploadPayload } from "../src/device/upload";
import { exportPublicKey, generateKeyPair } from "../src/trust/crypto";
import { parseCommitBody } from "../src/trust/wire";
import { FakeDevice, FakeTrust, REKEY_WINDOW_MS, protocol, serialPair } from "./fakeDevice";

const proto = protocol();
const image = Uint8Array.from({ length: 1500 }, (_, i) => (i * 31 + 7) & 0xff);
const image2 = Uint8Array.from({ length: 700 }, (_, i) => (i * 13 + 1) & 0xff);
const NOW_MS = 1_761_500_000_000;
const NOW_S = NOW_MS / 1000;

let open: { client: DeviceClient; device: FakeDevice }[] = [];

function rig(trust?: FakeTrust) {
  const { host, device: devSide } = serialPair();
  const client = new DeviceClient(host, proto);
  const device = new FakeDevice(proto, devSide, { trust });
  client.start();
  open.push({ client, device });
  return { client, device };
}

afterEach(async () => {
  for (const r of open) {
    await r.client.close();
    await r.device.close();
  }
  open = [];
});

async function newKey(): Promise<SigningKey> {
  const pair = await generateKeyPair(false);
  return { privateKey: pair.privateKey, publicKey: await exportPublicKey(pair.publicKey) };
}

const commits = (d: FakeDevice) => d.received.filter((r) => r.key === proto.constants.keys.commit);
const begins = (d: FakeDevice) => d.received.filter((r) => r.key === proto.constants.keys.begin);

describe("trust flows", () => {
  it("uploads unsigned to an unclaimed device after asking", async () => {
    const trust = new FakeTrust();
    const { client, device } = rig(trust);
    const asked: string[] = [];
    const r = await trustedUpload(client, proto, image, await newKey(), {
      now: () => NOW_MS,
      confirmUnsigned: (why) => (asked.push(why), true),
    });
    expect(r).toEqual({ kind: "unsigned", reason: "unclaimed" });
    expect(asked).toEqual(["unclaimed"]);
    expect(commits(device)[0].body).toHaveLength(0);
    expect(device.activated).toEqual({ len: image.length, crc: proto.crc32(image) });
  });

  it("sends nothing when the unsigned upload is declined", async () => {
    const { client, device } = rig(new FakeTrust());
    const err = await trustedUpload(client, proto, image, null, { confirmUnsigned: () => false }).catch((e) => e);
    expect(err).toBeInstanceOf(UploadError);
    expect(err.message).toMatch(/cancelled/);
    expect(begins(device)).toHaveLength(0);
  });

  it("uploads unsigned to firmware without sys/trust and says why", async () => {
    const { client, device } = rig();
    expect(await client.trustInfo()).toBeNull();
    const asked: string[] = [];
    const r = await trustedUpload(client, proto, image, await newKey(), { confirmUnsigned: (why) => (asked.push(why), true) });
    expect(r).toEqual({ kind: "unsigned", reason: "legacy-firmware" });
    expect(asked).toEqual(["legacy-firmware"]);
    expect(commits(device)[0].body).toHaveLength(0);
    expect(device.activated?.crc).toBe(proto.crc32(image));
  });

  it("claims an unclaimed device and denies a second claim", async () => {
    const trust = new FakeTrust();
    const { client } = rig(trust);
    const a = await newKey();
    const b = await newKey();
    expect(await client.trustInfo()).toEqual({ state: "unclaimed", minVersion: 0, key: null });
    await client.trustClaim(a.publicKey);
    expect(await client.trustInfo()).toEqual({ state: "claimed", minVersion: 0, key: a.publicKey });
    const err = await client.trustClaim(b.publicKey).catch((e) => e);
    expect(err).toBeInstanceOf(DeniedError);
    expect(err.status).toBe("Denied");
    expect(err.statusCode).toBe(5);
    expect(err.message).toMatch(/already claimed.*hold BOOT ≥3 s/);
    expect(trust.key).toEqual(a.publicKey);
  });

  it("signs the commit for a device that pins this key, and the device accepts it", async () => {
    const trust = new FakeTrust();
    const { client, device } = rig(trust);
    const k = await newKey();
    await client.trustClaim(k.publicKey);
    const r = await trustedUpload(client, proto, image, k, { now: () => NOW_MS, start: true });
    expect(r).toEqual({ kind: "signed", version: NOW_S, bumped: false });
    const body = parseCommitBody(commits(device)[0].body);
    expect(body?.version).toBe(NOW_S);
    expect(body?.signature).toHaveLength(64);
    expect(device.activated).toEqual({ len: image.length, crc: proto.crc32(image) });
    expect(device.started).toBe(true);
    expect(trust.minVersion).toBe(NOW_S);
  });

  it("a commit signed by another key is Denied and the old image stays active", async () => {
    const trust = new FakeTrust();
    const { client, device } = rig(trust);
    const mine = await newKey();
    const other = await newKey();
    await client.trustClaim(mine.publicKey);
    await trustedUpload(client, proto, image, mine, { now: () => NOW_MS });

    const err = await uploadPayload(client, proto, image2, {
      signing: { privateKey: other.privateKey, version: NOW_S + 10 },
    }).catch((e) => e);
    expect(err).toBeInstanceOf(UploadError);
    expect(err.key).toBe("sys/loader/commit");
    expect(err.status).toBe("Denied");
    expect(err.cause).toBeInstanceOf(DeniedError);
    expect(err.message).toMatch(/trusts a different key.*hold BOOT ≥3 s after boot/);
    expect(device.activated).toEqual({ len: image.length, crc: proto.crc32(image) });

    // Unsigned is Denied too once claimed.
    const unsigned = await uploadPayload(client, proto, image2).catch((e) => e);
    expect(unsigned.status).toBe("Denied");

    // trustedUpload with the wrong key refuses before sending anything.
    const before = device.received.length;
    const refused = await trustedUpload(client, proto, image2, other).catch((e) => e);
    expect(refused).toBeInstanceOf(TrustRefusal);
    expect(refused.reason).toBe("different-key");
    expect(device.received.slice(before).map((r) => r.key)).toEqual(["sys/trust/info"]);
    const keyless = await trustedUpload(client, proto, image2, null).catch((e) => e);
    expect(keyless.reason).toBe("no-local-key");
  });

  it("a replayed version is Denied; a clock behind the device bumps past min_version", async () => {
    const trust = new FakeTrust();
    const { client, device } = rig(trust);
    const k = await newKey();
    await client.trustClaim(k.publicKey);
    await trustedUpload(client, proto, image, k, { now: () => NOW_MS });

    const replay = await uploadPayload(client, proto, image2, { signing: { privateKey: k.privateKey, version: NOW_S } }).catch(
      (e) => e,
    );
    expect(replay.status).toBe("Denied");
    expect(device.activated?.crc).toBe(proto.crc32(image));

    // Same second (or a clock behind): min_version + 1, flagged.
    const r = await trustedUpload(client, proto, image2, k, { now: () => NOW_MS - 3_600_000 });
    expect(r).toEqual({ kind: "signed", version: NOW_S + 1, bumped: true });
    expect(device.activated).toEqual({ len: image2.length, crc: proto.crc32(image2) });
    expect(trust.minVersion).toBe(NOW_S + 1);
  });

  it("re-keys only while the window opened by holding BOOT is open", async () => {
    let t = 0;
    const trust = new FakeTrust(() => t);
    const { client, device } = rig(trust);
    const oldKey = await newKey();
    const newer = await newKey();
    await client.trustClaim(oldKey.publicKey);
    await trustedUpload(client, proto, image, oldKey, { now: () => NOW_MS });

    trust.holdBoot(2000); // too short
    expect((await client.trustInfo())?.state).toBe("claimed");
    expect(await client.trustClaim(newer.publicKey).catch((e) => e)).toBeInstanceOf(DeniedError);

    trust.holdBoot(3000);
    expect(await client.trustInfo()).toMatchObject({ state: "rekey-window", key: oldKey.publicKey });
    t += REKEY_WINDOW_MS; // the window lapses unused
    expect((await client.trustInfo())?.state).toBe("claimed");
    expect(await client.trustClaim(newer.publicKey).catch((e) => e)).toBeInstanceOf(DeniedError);

    trust.holdBoot(3500);
    t += REKEY_WINDOW_MS - 1;
    await client.trustClaim(newer.publicKey);
    expect(await client.trustInfo()).toEqual({ state: "claimed", minVersion: 0, key: newer.publicKey });

    // The old key no longer signs for this device; the new one does.
    const stale = await uploadPayload(client, proto, image2, { signing: { privateKey: oldKey.privateKey, version: NOW_S + 5 } }).catch(
      (e) => e,
    );
    expect(stale.status).toBe("Denied");
    const r = await trustedUpload(client, proto, image2, newer, { now: () => NOW_MS + 60_000 });
    expect(r.kind).toBe("signed");
    expect(device.activated?.crc).toBe(proto.crc32(image2));
  });
});
