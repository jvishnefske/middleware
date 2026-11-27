// This page's node id (design.md BM5): eight random bytes made once and
// kept in localStorage, so two browsers in a room are two nodes (every
// simulated device used to default to 0000000000000001) and a page keeps
// its id across reloads. DOM-free: the storage and the random source are
// passed in.

export const NODE_KEY = "mw.node";

type Store = Pick<Storage, "getItem" | "setItem">;

type Fill = (b: Uint8Array<ArrayBuffer>) => void;

function randomNode(fill: Fill): string {
  const b = new Uint8Array(8);
  fill(b);
  // The all-zero id means "no sender".
  if (b.every((x) => x === 0)) b[7] = 1;
  return [...b].map((x) => x.toString(16).padStart(2, "0")).join("");
}

/** The stored node id, or a new one (stored when there is a storage that
 * takes it; a page without storage has a new id on every load). */
export function nodeId(
  storage: Store | null,
  fill: Fill = (b) => {
    crypto.getRandomValues(b);
  },
): string {
  try {
    const saved = storage?.getItem(NODE_KEY);
    if (saved && /^[0-9a-f]{16}$/.test(saved) && !/^0{16}$/.test(saved)) return saved;
  } catch {
    // Storage refused: fall through to a fresh id.
  }
  const id = randomNode(fill);
  try {
    storage?.setItem(NODE_KEY, id);
  } catch {
    // Not kept.
  }
  return id;
}

/** The page's localStorage, or null when it has none (or it is blocked). */
export function pageStorage(): Storage | null {
  try {
    return globalThis.localStorage ?? null;
  } catch {
    return null;
  }
}

/** `nodeId` on the page's localStorage, when it has one. */
export function pageNodeId(): string {
  return nodeId(pageStorage());
}
