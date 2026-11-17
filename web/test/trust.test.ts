// U13/U14: trust wire formats, the golden signing vector shared with the
// Rust device side, the encrypted key backup and the key store.

import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";
import {
  BackupError,
  PBKDF2_MIN_ITERATIONS,
  decompressP256,
  createBackup,
  createSigningKey,
  exportPublicKey,
  fingerprint,
  generateKeyPair,
  groupFingerprint,
  importSeed,
  parseBackupText,
  restoreBackup,
  sha256,
  signImage,
  verifySignature,
} from "../src/trust/crypto";
import { KeyStorageError, indexedDbStorage, memoryStorage, storedKey } from "../src/trust/keyStore";
import {
  TrustFormatError,
  canClaim,
  chooseVersion,
  claimBody,
  commitBody,
  encodeTrustInfo,
  fromHex,
  keyKind,
  parseCommitBody,
  parseTrustInfo,
  planUpload,
  relateKeys,
  signedMessage,
  toHex,
} from "../src/trust/wire";

interface Vector {
  seed: string;
  public_key: string;
  image_len: number;
  version: number;
  sha256: string;
  message: string;
  signature: string;
  commit_body: string;
}

const vector = JSON.parse(readFileSync(new URL("./fixtures/trust-vector.json", import.meta.url), "utf8")) as Vector;
const vectorImage = Uint8Array.from({ length: vector.image_len }, (_, i) => (i * 7 + 3) & 0xff);
const PASS = "correct horse battery";

describe("trust golden vector", () => {
  it("reproduces the message, signature and commit body of the shared vector", async () => {
    const key = await importSeed(fromHex(vector.seed));
    expect(key.extractable).toBe(false);
    expect(toHex(await sha256(vectorImage))).toBe(vector.sha256);
    const signed = await signImage(key, vectorImage, vector.version);
    expect(toHex(signed.message)).toBe(vector.message);
    expect(signed.message.length).toBe(54);
    expect(toHex(signed.signature)).toBe(vector.signature);
    expect(toHex(signed.body)).toBe(vector.commit_body);
    expect(await verifySignature(fromHex(vector.public_key), signed.message, signed.signature)).toBe(true);
  });

  it("builds the message from its parts", () => {
    const m = signedMessage(vector.image_len, vector.version, fromHex(vector.sha256));
    expect(toHex(m)).toBe(vector.message);
    expect(new TextDecoder().decode(m.slice(0, 14))).toBe("mw-payload-v1\0");
    expect(() => signedMessage(-1, 1, new Uint8Array(32))).toThrow(TrustFormatError);
    expect(() => signedMessage(1, 2 ** 32, new Uint8Array(32))).toThrow(TrustFormatError);
    expect(() => signedMessage(1, 1, new Uint8Array(31))).toThrow(TrustFormatError);
  });

  it("encodes and splits the commit body", () => {
    const sig = fromHex(vector.signature);
    const body = commitBody(vector.version, sig);
    expect(toHex(body)).toBe(vector.commit_body);
    expect(parseCommitBody(body)).toEqual({ version: vector.version, signature: sig });
    expect(parseCommitBody(new Uint8Array())).toBeNull();
    expect(() => commitBody(1, new Uint8Array(63))).toThrow(TrustFormatError);
    expect(() => parseCommitBody(new Uint8Array(67))).toThrow(TrustFormatError);
  });

  it("encodes the claim body as the raw public key", () => {
    const pk = fromHex(vector.public_key);
    expect(claimBody(pk)).toEqual(pk);
    expect(() => claimBody(new Uint8Array(33))).toThrow(TrustFormatError);
  });

  it("fingerprints a key as the first 8 bytes of its SHA-256", async () => {
    const pk = fromHex(vector.public_key);
    const fp = await fingerprint(pk);
    expect(fp).toBe(toHex((await sha256(pk)).slice(0, 8)));
    expect(fp).toMatch(/^[0-9a-f]{16}$/);
    expect(groupFingerprint("0123456789abcdef")).toBe("0123 4567 89ab cdef");
  });
});

describe("trust info", () => {
  const pk = fromHex(vector.public_key);

  it("parses the three states, with the key present iff claimed", () => {
    expect(parseTrustInfo(Uint8Array.of(0, 0, 0, 0, 0))).toEqual({ state: "unclaimed", minVersion: 0, key: null });
    const claimed = Uint8Array.from([1, 0x60, 0x5b, 0xfe, 0x68, ...pk]);
    expect(parseTrustInfo(claimed)).toEqual({ state: "claimed", minVersion: vector.version, key: pk });
    expect(parseTrustInfo(Uint8Array.from([2, 1, 0, 0, 0, ...pk]))).toEqual({ state: "rekey-window", minVersion: 1, key: pk });
  });

  it("round-trips through the encoder", () => {
    for (const info of [
      { state: "unclaimed" as const, minVersion: 7, key: null },
      { state: "claimed" as const, minVersion: 0xffffffff, key: pk },
      { state: "rekey-window" as const, minVersion: 3, key: pk },
    ]) {
      expect(parseTrustInfo(encodeTrustInfo(info))).toEqual(info);
    }
  });

  it("rejects short bodies, unknown states and a key that disagrees with the state", () => {
    expect(() => parseTrustInfo(new Uint8Array(4))).toThrow(TrustFormatError);
    expect(() => parseTrustInfo(Uint8Array.of(3, 0, 0, 0, 0))).toThrow(/unknown state/);
    expect(() => parseTrustInfo(Uint8Array.from([0, 0, 0, 0, 0, ...pk]))).toThrow(TrustFormatError);
    expect(() => parseTrustInfo(Uint8Array.of(1, 0, 0, 0, 0))).toThrow(TrustFormatError);
    // 39 bytes is neither kind (38 would be a P-256 key: the length decides).
    expect(() => parseTrustInfo(Uint8Array.from([1, 0, 0, 0, 0, ...pk, 0, 0]))).toThrow(TrustFormatError);
  });

  it("relates the device key to this browser's key and decides the upload", () => {
    const other = new Uint8Array(32).fill(9);
    const claimed = { state: "claimed" as const, minVersion: 100, key: pk };
    expect(relateKeys(null, pk)).toBe("no-trust");
    expect(relateKeys({ state: "unclaimed", minVersion: 0, key: null }, pk)).toBe("unclaimed");
    expect(relateKeys(claimed, pk)).toBe("match");
    expect(relateKeys(claimed, other)).toBe("different");
    expect(relateKeys(claimed, null)).toBe("no-local-key");
    expect(canClaim(null)).toBe(false);
    expect(canClaim({ state: "unclaimed", minVersion: 0, key: null })).toBe(true);
    expect(canClaim(claimed)).toBe(false);
    expect(canClaim({ ...claimed, state: "rekey-window" })).toBe(true);

    expect(planUpload(null, pk, 0)).toEqual({ kind: "unsigned", reason: "legacy-firmware" });
    expect(planUpload({ state: "unclaimed", minVersion: 0, key: null }, null, 0)).toEqual({ kind: "unsigned", reason: "unclaimed" });
    expect(planUpload(claimed, other, 0)).toEqual({ kind: "refuse", reason: "different-key" });
    expect(planUpload(claimed, null, 0)).toEqual({ kind: "refuse", reason: "no-local-key" });
    expect(planUpload(claimed, pk, 200_500)).toEqual({ kind: "signed", version: 200, bumped: false });
  });

  it("uses wall-clock seconds as the version, bumping past min_version when behind", () => {
    expect(chooseVersion(1_761_500_000_999, 0)).toEqual({ version: 1_761_500_000, bumped: false });
    expect(chooseVersion(1_761_500_000_000, 1_761_500_000)).toEqual({ version: 1_761_500_001, bumped: true });
    expect(chooseVersion(5_000, 4_000_000_000)).toEqual({ version: 4_000_000_001, bumped: true });
    expect(() => chooseVersion(0, 0xffffffff)).toThrow(TrustFormatError);
  });
});

interface P256Vector {
  public_key: string;
  message: string;
  signature: string;
  commit_body: string;
  control_message: string;
  control_signature: string;
}

const p256 = JSON.parse(readFileSync(new URL("./fixtures/trust-vector-p256.json", import.meta.url), "utf8")) as P256Vector;

describe("P-256 pinned keys (hardware tokens)", () => {
  const pk = fromHex(p256.public_key);

  it("tells the kinds apart by length and SEC1 prefix", () => {
    expect(keyKind(fromHex(vector.public_key))).toBe("ed25519");
    expect(keyKind(pk)).toBe("p256");
    expect(keyKind(Uint8Array.from([4, ...pk.subarray(1)]))).toBeNull();
    expect(keyKind(new Uint8Array(65))).toBeNull();
    expect(claimBody(pk)).toEqual(pk);
  });

  it("parses and encodes a 38-byte trust info", () => {
    const body = Uint8Array.from([1, 0x60, 0x5b, 0xfe, 0x68, ...pk]);
    const info = parseTrustInfo(body);
    expect(info).toEqual({ state: "claimed", minVersion: vector.version, key: pk });
    expect(encodeTrustInfo(info)).toEqual(body);
    expect(() => parseTrustInfo(Uint8Array.from([1, 0, 0, 0, 0, 4, ...pk.subarray(1)]))).toThrow(TrustFormatError);
  });

  it("verifies the OpenSSL vector, and nothing else", async () => {
    const message = fromHex(p256.message);
    const sig = fromHex(p256.signature);
    expect(toHex(signedMessage(vector.image_len, vector.version, fromHex(vector.sha256)))).toBe(p256.message);
    expect(await verifySignature(pk, message, sig)).toBe(true);
    expect(await verifySignature(pk, fromHex(p256.control_message), fromHex(p256.control_signature))).toBe(true);
    const bad = sig.slice();
    bad[5] ^= 1;
    expect(await verifySignature(pk, message, bad)).toBe(false);
    expect(await verifySignature(pk, message.subarray(1), sig)).toBe(false);
    // Kinds never cross: the Ed25519 vector's signature under the P-256 key
    // and the other way round.
    expect(await verifySignature(pk, message, fromHex(vector.signature))).toBe(false);
    expect(await verifySignature(fromHex(vector.public_key), message, sig)).toBe(false);
    expect(parseCommitBody(fromHex(p256.commit_body))?.signature).toEqual(sig);
  });

  it("decompresses points and refuses x off the curve", () => {
    const point = decompressP256(pk);
    expect(point?.length).toBe(65);
    expect(point?.[0]).toBe(4);
    expect(toHex(point!.subarray(1, 33))).toBe(p256.public_key.slice(2));
    const flipped = pk.slice();
    flipped[0] = flipped[0] === 2 ? 3 : 2;
    const other = decompressP256(flipped);
    expect(toHex(other!.subarray(33))).not.toBe(toHex(point!.subarray(33)));
    let offCurve = 0;
    for (let x = 0; x < 16; x++) {
      const k = new Uint8Array(33);
      k[0] = 2;
      k[32] = x;
      if (decompressP256(k) === null) offCurve++;
    }
    expect(offCurve).toBeGreaterThan(0);
    expect(decompressP256(fromHex(vector.public_key))).toBeNull();
  });

  it("a device pinned to a P-256 key refuses browser uploads (different key)", () => {
    const info = parseTrustInfo(Uint8Array.from([1, 0, 0, 0, 0, ...pk]));
    expect(relateKeys(info, fromHex(vector.public_key))).toBe("different");
    expect(planUpload(info, fromHex(vector.public_key), 0)).toEqual({ kind: "refuse", reason: "different-key" });
  });
});

describe("signing key backup", () => {
  it("backs up a new key and restores it with the passphrase", async () => {
    const created = await createSigningKey(PASS);
    expect(created.privateKey.extractable).toBe(false);
    const file = created.backup;
    expect(file).toMatchObject({ format: "middleware-signing-key", version: 1, algorithm: "Ed25519" });
    expect(file.kdf).toMatchObject({ name: "PBKDF2", hash: "SHA-256", iterations: PBKDF2_MIN_ITERATIONS });
    expect(atob(file.kdf.salt)).toHaveLength(16);
    expect(atob(file.cipher.iv)).toHaveLength(12);
    expect(file.public_key).toBe(toHex(created.publicKey));
    expect(file.fingerprint).toBe(await fingerprint(created.publicKey));

    const restored = await restoreBackup(parseBackupText(JSON.stringify(file)), PASS);
    expect(restored.privateKey.extractable).toBe(false);
    expect(restored.publicKey).toEqual(created.publicKey);
    // The restored key signs for the same identity.
    const s = await signImage(restored.privateKey, vectorImage, 42);
    expect(await verifySignature(created.publicKey, s.message, s.signature)).toBe(true);
  });

  it("rejects the wrong passphrase and a tampered file", async () => {
    const pair = await generateKeyPair(true);
    const file = await createBackup(pair.privateKey, PASS);
    await expect(restoreBackup(file, "not the passphrase")).rejects.toThrow(/wrong passphrase/);
    const otherPub = toHex(await exportPublicKey((await generateKeyPair(false)).publicKey));
    await expect(restoreBackup({ ...file, public_key: otherPub }, PASS)).rejects.toBeInstanceOf(BackupError);
  });

  it("enforces passphrase length, the iteration floor and the file format", async () => {
    const pair = await generateKeyPair(true);
    await expect(createBackup(pair.privateKey, "short")).rejects.toThrow(/at least 8/);
    await expect(createBackup(pair.privateKey, PASS, { iterations: 1000 })).rejects.toThrow(/600000/);
    await expect(createBackup((await generateKeyPair(false)).privateKey, PASS)).rejects.toThrow(/freshly generated/);
    const file = await createBackup(pair.privateKey, PASS);
    expect(() => parseBackupText("{")).toThrow(/not JSON/);
    expect(() => parseBackupText(JSON.stringify({ ...file, format: "x" }))).toThrow(/format tag/);
    expect(() => parseBackupText(JSON.stringify({ ...file, kdf: { ...file.kdf, iterations: 1000 } }))).toThrow(/iterations/);
  });
});

describe("key store", () => {
  it("keeps the non-extractable key, its public key and the backup", async () => {
    const created = await createSigningKey(PASS);
    const store = memoryStorage();
    const rec = await storedKey(created.privateKey, created.publicKey, created.backup);
    await store.save(rec);
    expect(await store.load()).toBe(rec);
    expect(rec.fingerprint).toBe(created.backup.fingerprint);
    await store.clear();
    expect(await store.load()).toBeNull();
    const extractable = await generateKeyPair(true);
    await expect(storedKey(extractable.privateKey, created.publicKey, created.backup)).rejects.toThrow(/extractable/);
  });

  it("reports unavailable or failing IndexedDB as a KeyStorageError", async () => {
    await expect(indexedDbStorage(null).load()).rejects.toThrow(/IndexedDB is not available/);
    const throwing = {
      open() {
        throw new DOMException("The operation is insecure.", "SecurityError");
      },
    } as unknown as IDBFactory;
    const err = await indexedDbStorage(throwing).load().catch((e) => e);
    expect(err).toBeInstanceOf(KeyStorageError);
    expect(err.message).toMatch(/SecurityError/);
  });
});
