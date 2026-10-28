// Persistence of this browser's signing key (requirement U13).
//
// The key lives in IndexedDB (database `middleware-trust`, object store
// `keys`, record `signing`) as a structured-cloned, non-extractable
// CryptoKey: page script can sign with it but cannot read the private
// bytes. Next to it the record keeps the raw public key and the
// passphrase-encrypted backup file made at creation, so "Back up key" can
// download that file again later (the stored CryptoKey itself can never be
// exported).
//
// Every IndexedDB access is wrapped so that a missing or blocked IndexedDB
// (private windows, disabled site data, non-browser runtimes) surfaces as a
// `KeyStorageError` with a readable message instead of an exception from
// deep inside a callback.

import { type BackupFile, fingerprint } from "./crypto";

export interface StoredKey {
  /** Non-extractable Ed25519 private key. */
  privateKey: CryptoKey;
  /** Raw 32-byte public key. */
  publicKey: Uint8Array;
  /** The encrypted backup made when the key was created or restored. */
  backup: BackupFile;
  fingerprint: string;
}

export interface KeyStorage {
  load(): Promise<StoredKey | null>;
  save(key: StoredKey): Promise<void>;
  clear(): Promise<void>;
}

export class KeyStorageError extends Error {
  constructor(message: string, readonly cause?: unknown) {
    super(message);
    this.name = "KeyStorageError";
  }
}

const DB_NAME = "middleware-trust";
const STORE = "keys";
const RECORD = "signing";

function reason(e: unknown): string {
  return e instanceof Error ? `${e.name}: ${e.message}` : String(e);
}

function req<T>(r: IDBRequest<T>): Promise<T> {
  return new Promise((resolve, reject) => {
    r.onsuccess = () => resolve(r.result);
    r.onerror = () => reject(r.error);
  });
}

/** IndexedDB-backed storage. `factory` defaults to `globalThis.indexedDB`;
 * null means IndexedDB is unavailable. */
export function indexedDbStorage(factory: IDBFactory | null | undefined = globalThis.indexedDB): KeyStorage {
  async function open(): Promise<IDBDatabase> {
    if (!factory) throw new KeyStorageError("IndexedDB is not available in this browser, so no key can be stored");
    try {
      const r = factory.open(DB_NAME, 1);
      r.onupgradeneeded = () => {
        if (!r.result.objectStoreNames.contains(STORE)) r.result.createObjectStore(STORE);
      };
      return await new Promise<IDBDatabase>((resolve, reject) => {
        r.onsuccess = () => resolve(r.result);
        r.onerror = () => reject(r.error);
        r.onblocked = () => reject(new Error("database upgrade blocked by another tab"));
      });
    } catch (e) {
      throw new KeyStorageError(`cannot open key storage (IndexedDB): ${reason(e)}`, e);
    }
  }

  async function withStore<T>(mode: IDBTransactionMode, what: string, f: (s: IDBObjectStore) => IDBRequest<T>): Promise<T> {
    const db = await open();
    try {
      const tx = db.transaction(STORE, mode);
      const done = new Promise<void>((resolve, reject) => {
        tx.oncomplete = () => resolve();
        tx.onerror = () => reject(tx.error);
        tx.onabort = () => reject(tx.error ?? new Error("transaction aborted"));
      });
      const result = await req(f(tx.objectStore(STORE)));
      await done;
      return result;
    } catch (e) {
      throw new KeyStorageError(`cannot ${what} the signing key in IndexedDB: ${reason(e)}`, e);
    } finally {
      db.close();
    }
  }

  return {
    async load() {
      const v = await withStore("readonly", "read", (s) => s.get(RECORD) as IDBRequest<unknown>);
      return isStoredKey(v) ? v : null;
    },
    async save(key) {
      await withStore("readwrite", "save", (s) => s.put(key, RECORD));
      await requestPersistence();
    },
    async clear() {
      await withStore("readwrite", "delete", (s) => s.delete(RECORD));
    },
  };
}

function isStoredKey(v: unknown): v is StoredKey {
  const k = v as Partial<StoredKey> | null;
  return (
    !!k &&
    typeof k === "object" &&
    typeof CryptoKey !== "undefined" &&
    k.privateKey instanceof CryptoKey &&
    k.publicKey instanceof Uint8Array &&
    typeof k.fingerprint === "string" &&
    !!k.backup
  );
}

/** Asks the browser not to evict site storage under pressure. Best effort. */
async function requestPersistence(): Promise<void> {
  try {
    await globalThis.navigator?.storage?.persist?.();
  } catch {
    /* not supported or refused; the backup file is the real safety net */
  }
}

/** In-memory storage (tests; also a fallback a caller may choose). */
export function memoryStorage(): KeyStorage & { readonly current: StoredKey | null } {
  let current: StoredKey | null = null;
  return {
    get current() {
      return current;
    },
    load: async () => current,
    save: async (k) => {
      current = k;
    },
    clear: async () => {
      current = null;
    },
  };
}

/** Builds the stored record for a key that was just created or restored. */
export async function storedKey(privateKey: CryptoKey, publicKey: Uint8Array, backup: BackupFile): Promise<StoredKey> {
  if (privateKey.extractable) throw new KeyStorageError("refusing to store an extractable private key");
  return { privateKey, publicKey, backup, fingerprint: await fingerprint(publicKey) };
}
