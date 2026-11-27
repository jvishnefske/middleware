// What the browser mesh needs from the browser (design.md BM): a
// WebSocket, an RTCPeerConnection, fetch and timers, as the smallest
// shapes the mesh uses, so tests run it against an in-memory signalling
// server and paired fake data channels. DOM-free.

export interface SocketLike {
  onopen: (() => void) | null;
  onmessage: ((e: { data: unknown }) => void) | null;
  onclose: (() => void) | null;
  onerror: (() => void) | null;
  send(data: string): void;
  close(): void;
}

export interface DataChannelLike {
  binaryType: string;
  readonly readyState: string;
  onopen: (() => void) | null;
  onmessage: ((e: { data: unknown }) => void) | null;
  onclose: (() => void) | null;
  send(data: string | Uint8Array<ArrayBuffer>): void;
  close(): void;
}

export interface IceCandidateLike {
  candidate: string;
  sdpMid?: string | null;
  sdpMLineIndex?: number | null;
}

export interface SessionDescriptionLike {
  type: "offer" | "answer";
  sdp: string;
}

export interface PeerConnectionLike {
  onicecandidate: ((e: { candidate: IceCandidateLike | null }) => void) | null;
  ondatachannel: ((e: { channel: DataChannelLike }) => void) | null;
  onconnectionstatechange: (() => void) | null;
  readonly connectionState: string;
  createDataChannel(label: string, opts?: { ordered?: boolean }): DataChannelLike;
  createOffer(): Promise<{ sdp?: string }>;
  createAnswer(): Promise<{ sdp?: string }>;
  setLocalDescription(d: SessionDescriptionLike): Promise<void>;
  setRemoteDescription(d: SessionDescriptionLike): Promise<void>;
  addIceCandidate(c: IceCandidateLike): Promise<void>;
  close(): void;
}

export interface IceServer {
  urls: string | string[];
  username?: string;
  credential?: string;
}

export interface MeshEnv {
  WebSocket: new (url: string) => SocketLike;
  RTCPeerConnection: new (config: { iceServers: IceServer[] }) => PeerConnectionLike;
  /** GET `url`. */
  fetch(url: string): Promise<{ ok: boolean; status: number; text(): Promise<string> }>;
  /** Calls `f` after `ms`; returns the cancel. */
  after(ms: number, f: () => void): () => void;
  /** A number in [0, 1). */
  random(): number;
}

/** The browser's own. */
export function browserEnv(): MeshEnv {
  return {
    WebSocket: WebSocket as unknown as MeshEnv["WebSocket"],
    RTCPeerConnection: RTCPeerConnection as unknown as MeshEnv["RTCPeerConnection"],
    fetch: (url) => fetch(url),
    after: (ms, f) => {
      const h = setTimeout(f, ms);
      return () => clearTimeout(h);
    },
    random: Math.random,
  };
}

/** A random token of `a-z0-9` (a signalling session, a connection id). */
export function token(env: Pick<MeshEnv, "random">, len = 12): string {
  let s = "";
  for (let i = 0; i < len; i++) s += Math.floor(env.random() * 36).toString(36);
  return s;
}

/** Listeners of one event. */
export class Listeners<A extends unknown[]> {
  private readonly fs: ((...a: A) => void)[] = [];

  add(f: (...a: A) => void): () => void {
    this.fs.push(f);
    return () => {
      const i = this.fs.indexOf(f);
      if (i >= 0) this.fs.splice(i, 1);
    };
  }

  emit(...a: A): void {
    for (const f of [...this.fs]) f(...a);
  }

  clear(): void {
    this.fs.length = 0;
  }
}
