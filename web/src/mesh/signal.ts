// One socket to a PeerJS-compatible signalling server (design.md BM7):
// it registers an id, relays offers, answers and candidates to other ids
// and is kept open by a heartbeat. The URL and every message's JSON come
// from protocol-wasm (`mesh`); this file owns the WebSocket. DOM-free:
// the constructor is injected (`MeshEnv`).
//
// * `opened` settles once: "open" when the server accepted the id,
//   "taken" when the id is in use, "error" otherwise (`error` says why).
// * `onSignal` hears the messages relayed from other ids, `onClose` the
//   socket closing after it was open (the caller reconnects).
// * A message that is not one of the mesh's shapes is ignored.

import type { MeshServer, MeshSignal, MeshSignalOut, Protocol } from "../protocol/wasm";
import { Listeners, type MeshEnv, type SocketLike } from "./env";

export type OpenResult = "open" | "taken" | "error";

export class SignalSocket {
  private readonly socket: SocketLike | null = null;
  private readonly signals = new Listeners<[MeshSignal]>();
  private readonly closes = new Listeners<[]>();
  private stopHeartbeat: (() => void) | null = null;
  private settle: (r: OpenResult) => void = () => {};
  private isOpen = false;
  private done = false;
  /** Why `opened` was "error", or why the socket closed. */
  error = "";
  readonly opened: Promise<OpenResult>;

  constructor(
    private readonly proto: Protocol,
    private readonly env: MeshEnv,
    server: MeshServer,
    readonly id: string,
    sessionToken: string,
  ) {
    this.opened = new Promise((resolve) => {
      this.settle = resolve;
    });
    try {
      this.socket = new env.WebSocket(proto.mesh.socketUrl(server, id, sessionToken));
    } catch (e) {
      this.fail(e instanceof Error ? e.message : String(e));
      return;
    }
    this.socket.onmessage = (e) => {
      if (typeof e.data === "string") this.message(e.data);
    };
    this.socket.onerror = () => {
      this.error ||= "the signalling server did not answer";
    };
    this.socket.onclose = () => this.closed();
  }

  get open(): boolean {
    return this.isOpen;
  }

  private fail(why: string): void {
    this.error = why;
    this.done = true;
    this.settle("error");
  }

  private message(text: string): void {
    const s = this.proto.mesh.signalDecode(text);
    if (!s) return;
    if (s.type === "open") {
      this.isOpen = true;
      this.heartbeat();
      this.settle("open");
    } else if (s.type === "idTaken") {
      this.settle("taken");
      this.close();
    } else if (s.type === "error") {
      this.error = s.message;
      this.settle("error");
      this.close();
    } else if (s.type !== "heartbeat") {
      this.signals.emit(s);
    }
  }

  private heartbeat(): void {
    this.stopHeartbeat = this.env.after(this.proto.mesh.constants.heartbeatMs, () => {
      if (!this.isOpen) return;
      this.send({ type: "heartbeat" });
      this.heartbeat();
    });
  }

  private closed(): void {
    if (this.done) return;
    this.done = true;
    const was = this.isOpen;
    this.isOpen = false;
    this.stopHeartbeat?.();
    this.error ||= "the signalling server closed the connection";
    this.settle("error");
    if (was) this.closes.emit();
  }

  /** Sends a message; false when the socket is not open. */
  send(s: MeshSignalOut): boolean {
    if (!this.isOpen || !this.socket) return false;
    try {
      this.socket.send(this.proto.mesh.signalEncode(s));
      return true;
    } catch {
      return false;
    }
  }

  onSignal(f: (s: MeshSignal) => void): () => void {
    return this.signals.add(f);
  }

  /** The socket closed by itself after it was open. */
  onClose(f: () => void): () => void {
    return this.closes.add(f);
  }

  /** Closes the socket (no `onClose`). */
  close(): void {
    this.done = true;
    this.isOpen = false;
    this.stopHeartbeat?.();
    this.settle("error");
    try {
      this.socket?.close();
    } catch {
      // Already closed.
    }
  }
}
