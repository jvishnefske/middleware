// WebCrypto side of payload signing (requirement U13): Ed25519 keys,
// image signatures, key fingerprints, and the passphrase-encrypted backup
// file. No storage and no DOM here; `keyStore.ts` persists what this
// module produces.
//
// Backup file (JSON, `format: "middleware-signing-key"`, `version: 1`): the
// PKCS#8 private key encrypted with AES-256-GCM under a key derived from
// the passphrase with PBKDF2-HMAC-SHA-256. Salt (16 random bytes), IV (12
// random bytes) and the iteration count (>= 600 000) are recorded in the
// file. The raw public key is the GCM additional data, so a file whose
// `public_key` was edited fails to decrypt instead of restoring under a
// wrong identity.

import { MESSAGE_LEN, PUBLIC_KEY_LEN, TrustFormatError, commitBody, fromHex, signedMessage, toHex } from "./wire";

export const ED25519 = { name: "Ed25519" } as const;
/** PKCS#8 DER header for a raw 32-byte Ed25519 seed (RFC 8410). */
export const PKCS8_ED25519_PREFIX = fromHex("302e020100300506032b657004220420");
export const PBKDF2_MIN_ITERATIONS = 600_000;
/** Refuse files that would hang the tab deriving the key. */
export const PBKDF2_MAX_ITERATIONS = 10_000_000;
export const MIN_PASSPHRASE = 8;
export const BACKUP_FORMAT = "middleware-signing-key";

/** Copies into a fresh ArrayBuffer (WebCrypto's BufferSource). */
function ab(u: Uint8Array): ArrayBuffer {
  const out = new ArrayBuffer(u.length);
  new Uint8Array(out).set(u);
  return out;
}

function subtle(): SubtleCrypto {
  const s = globalThis.crypto?.subtle;
  if (!s) throw new Error("WebCrypto is unavailable (the page must be served over https or localhost)");
  return s;
}

export async function sha256(data: Uint8Array): Promise<Uint8Array> {
  return new Uint8Array(await subtle().digest("SHA-256", ab(data)));
}

/** First 8 bytes of SHA-256 of the raw public key, as 16 hex digits. */
export async function fingerprint(publicKey: Uint8Array): Promise<string> {
  return toHex((await sha256(publicKey)).slice(0, 8));
}

/** "0123456789abcdef" -> "0123 4567 89ab cdef" for display. */
export function groupFingerprint(fp: string): string {
  return fp.replace(/(.{4})(?=.)/g, "$1 ");
}

export interface SigningKeyPair {
  privateKey: CryptoKey;
  publicKey: CryptoKey;
}

export async function generateKeyPair(extractable: boolean): Promise<SigningKeyPair> {
  try {
    return (await subtle().generateKey(ED25519, extractable, ["sign", "verify"])) as SigningKeyPair;
  } catch (e) {
    throw new Error(`this browser cannot generate Ed25519 keys (Chrome 137+ needed): ${e instanceof Error ? e.message : e}`);
  }
}

/** Raw 32-byte public key (public keys are always exportable). */
export async function exportPublicKey(publicKey: CryptoKey): Promise<Uint8Array> {
  return new Uint8Array(await subtle().exportKey("raw", publicKey));
}

export async function importPrivateKey(pkcs8: Uint8Array, extractable: boolean): Promise<CryptoKey> {
  return subtle().importKey("pkcs8", ab(pkcs8), ED25519, extractable, ["sign"]);
}

/** Imports a raw 32-byte seed (golden-vector tests). */
export function importSeed(seed: Uint8Array, extractable = false): Promise<CryptoKey> {
  if (seed.length !== 32) throw new TrustFormatError("Ed25519 seed must be 32 bytes");
  const der = new Uint8Array(PKCS8_ED25519_PREFIX.length + 32);
  der.set(PKCS8_ED25519_PREFIX);
  der.set(seed, PKCS8_ED25519_PREFIX.length);
  return importPrivateKey(der, extractable);
}

/** The raw public key of an extractable private key (JWK `x`). */
async function publicOfPrivate(privateKey: CryptoKey): Promise<Uint8Array> {
  const jwk = await subtle().exportKey("jwk", privateKey);
  if (!jwk.x) throw new Error("private key has no public part");
  const pub = fromBase64Url(jwk.x);
  if (pub.length !== PUBLIC_KEY_LEN) throw new Error("unexpected Ed25519 public key length");
  return pub;
}

export interface SignedCommit {
  version: number;
  sha256: Uint8Array;
  message: Uint8Array;
  signature: Uint8Array;
  /** `sys/loader/commit` body. */
  body: Uint8Array;
}

export async function signImage(privateKey: CryptoKey, image: Uint8Array, version: number): Promise<SignedCommit> {
  const digest = await sha256(image);
  const message = signedMessage(image.length, version, digest);
  if (message.length !== MESSAGE_LEN) throw new Error("bad message length");
  const signature = new Uint8Array(await subtle().sign(ED25519, privateKey, ab(message)));
  return { version, sha256: digest, message, signature, body: commitBody(version, signature) };
}

export async function verifySignature(publicKey: Uint8Array, message: Uint8Array, signature: Uint8Array): Promise<boolean> {
  try {
    const k = await subtle().importKey("raw", ab(publicKey), ED25519, true, ["verify"]);
    return await subtle().verify(ED25519, k, ab(signature), ab(message));
  } catch {
    return false;
  }
}

// ---- backup file

export interface BackupFile {
  format: typeof BACKUP_FORMAT;
  version: 1;
  algorithm: "Ed25519";
  /** Raw public key, hex. */
  public_key: string;
  fingerprint: string;
  created: string;
  kdf: { name: "PBKDF2"; hash: "SHA-256"; iterations: number; salt: string };
  cipher: { name: "AES-GCM"; iv: string; ciphertext: string };
}

export class BackupError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "BackupError";
  }
}

function toBase64(b: Uint8Array): string {
  let s = "";
  for (const x of b) s += String.fromCharCode(x);
  return btoa(s);
}

function fromBase64(s: string): Uint8Array {
  return Uint8Array.from(atob(s), (c) => c.charCodeAt(0));
}

function fromBase64Url(s: string): Uint8Array {
  const b = s.replace(/-/g, "+").replace(/_/g, "/");
  return fromBase64(b + "=".repeat((4 - (b.length % 4)) % 4));
}

async function wrappingKey(passphrase: string, salt: Uint8Array, iterations: number): Promise<CryptoKey> {
  const base = await subtle().importKey("raw", ab(new TextEncoder().encode(passphrase)), "PBKDF2", false, ["deriveKey"]);
  return subtle().deriveKey(
    { name: "PBKDF2", hash: "SHA-256", salt: ab(salt), iterations },
    base,
    { name: "AES-GCM", length: 256 },
    false,
    ["encrypt", "decrypt"],
  );
}

export interface BackupOptions {
  iterations?: number;
  /** Creation time recorded in the file; defaults to now. */
  now?: Date;
}

/** Encrypts an extractable private key into a backup file. */
export async function createBackup(
  privateKey: CryptoKey,
  passphrase: string,
  opts: BackupOptions = {},
): Promise<BackupFile> {
  if (passphrase.length < MIN_PASSPHRASE) throw new BackupError(`passphrase must be at least ${MIN_PASSPHRASE} characters`);
  const iterations = opts.iterations ?? PBKDF2_MIN_ITERATIONS;
  if (iterations < PBKDF2_MIN_ITERATIONS) throw new BackupError(`PBKDF2 needs at least ${PBKDF2_MIN_ITERATIONS} iterations`);
  if (!privateKey.extractable) throw new BackupError("only a freshly generated key can be backed up");
  const pkcs8 = new Uint8Array(await subtle().exportKey("pkcs8", privateKey));
  const pub = await publicOfPrivate(privateKey);
  const salt = crypto.getRandomValues(new Uint8Array(16));
  const iv = crypto.getRandomValues(new Uint8Array(12));
  const k = await wrappingKey(passphrase, salt, iterations);
  const ct = new Uint8Array(await subtle().encrypt({ name: "AES-GCM", iv: ab(iv), additionalData: ab(pub) }, k, ab(pkcs8)));
  pkcs8.fill(0);
  return {
    format: BACKUP_FORMAT,
    version: 1,
    algorithm: "Ed25519",
    public_key: toHex(pub),
    fingerprint: await fingerprint(pub),
    created: (opts.now ?? new Date()).toISOString(),
    kdf: { name: "PBKDF2", hash: "SHA-256", iterations, salt: toBase64(salt) },
    cipher: { name: "AES-GCM", iv: toBase64(iv), ciphertext: toBase64(ct) },
  };
}

/** Validates the shape of a parsed backup file. */
export function parseBackup(json: unknown): BackupFile {
  const f = json as Partial<BackupFile> | null;
  const bad = (what: string) => new BackupError(`not a middleware signing-key backup: ${what}`);
  if (!f || typeof f !== "object" || f.format !== BACKUP_FORMAT) throw bad("wrong format tag");
  if (f.version !== 1) throw bad(`unsupported version ${String(f.version)}`);
  if (f.algorithm !== "Ed25519") throw bad("algorithm is not Ed25519");
  if (typeof f.public_key !== "string" || f.public_key.length !== 64) throw bad("public_key");
  const kdf = f.kdf;
  if (!kdf || kdf.name !== "PBKDF2" || kdf.hash !== "SHA-256" || typeof kdf.salt !== "string") throw bad("kdf");
  if (!Number.isInteger(kdf.iterations) || kdf.iterations < PBKDF2_MIN_ITERATIONS || kdf.iterations > PBKDF2_MAX_ITERATIONS) {
    throw bad(`kdf iterations ${String(kdf.iterations)}`);
  }
  const c = f.cipher;
  if (!c || c.name !== "AES-GCM" || typeof c.iv !== "string" || typeof c.ciphertext !== "string") throw bad("cipher");
  return f as BackupFile;
}

export function parseBackupText(text: string): BackupFile {
  let json: unknown;
  try {
    json = JSON.parse(text);
  } catch {
    throw new BackupError("not a middleware signing-key backup: not JSON");
  }
  return parseBackup(json);
}

export interface RestoredKey {
  /** Non-extractable. */
  privateKey: CryptoKey;
  publicKey: Uint8Array;
}

/** Decrypts a backup and returns a non-extractable re-import of the key. */
export async function restoreBackup(file: BackupFile, passphrase: string): Promise<RestoredKey> {
  const f = parseBackup(file);
  let pub: Uint8Array;
  let salt: Uint8Array;
  let iv: Uint8Array;
  let ct: Uint8Array;
  try {
    pub = fromHex(f.public_key);
    salt = fromBase64(f.kdf.salt);
    iv = fromBase64(f.cipher.iv);
    ct = fromBase64(f.cipher.ciphertext);
  } catch {
    throw new BackupError("backup file is corrupt (bad hex or base64)");
  }
  const k = await wrappingKey(passphrase, salt, f.kdf.iterations);
  let pkcs8: Uint8Array;
  try {
    pkcs8 = new Uint8Array(await subtle().decrypt({ name: "AES-GCM", iv: ab(iv), additionalData: ab(pub) }, k, ab(ct)));
  } catch {
    throw new BackupError("wrong passphrase, or the backup file was modified");
  }
  try {
    // Extractable only long enough to confirm the public half.
    const check = await importPrivateKey(pkcs8, true);
    const derived = await publicOfPrivate(check);
    if (toHex(derived) !== toHex(pub)) throw new BackupError("backup private key does not match its public key");
    return { privateKey: await importPrivateKey(pkcs8, false), publicKey: pub };
  } finally {
    pkcs8.fill(0);
  }
}

export interface NewKey {
  /** Non-extractable. */
  privateKey: CryptoKey;
  publicKey: Uint8Array;
  backup: BackupFile;
}

/**
 * Generates a key pair, encrypts the private key into a backup under
 * `passphrase`, and returns a non-extractable re-import. The extractable
 * copy exists only inside this call.
 */
export async function createSigningKey(passphrase: string, opts: BackupOptions = {}): Promise<NewKey> {
  const pair = await generateKeyPair(true);
  const backup = await createBackup(pair.privateKey, passphrase, opts);
  const pkcs8 = new Uint8Array(await subtle().exportKey("pkcs8", pair.privateKey));
  try {
    return {
      privateKey: await importPrivateKey(pkcs8, false),
      publicKey: await exportPublicKey(pair.publicKey),
      backup,
    };
  } finally {
    pkcs8.fill(0);
  }
}
