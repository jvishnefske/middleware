// One WebRTC peer connection with one ordered, reliable data channel
// (design.md BM7), negotiated through a signalling socket in PeerJS's
// message shapes. DOM-free: the RTCPeerConnection constructor is injected.
//
// * The dialler creates the channel and sends the offer (`dial`); the
//   other side answers it (`accept`). Candidates trickle both ways and
//   wait for the remote description when they arrive first.
// * `onOpen`, `onMessage` (bytes or text) and `onClose` (once: the channel
//   or the connection closed, the negotiation failed, or it did not open
//   within `OPEN_TIMEOUT_MS`).

import type { MeshSignal, MeshSignalOut } from "../protocol/wasm";
import type { DataChannelLike, IceCandidateLike, IceServer, MeshEnv, PeerConnectionLike } from "./env";

/** How long a connection may take to open. */
export const OPEN_TIMEOUT_MS = 20_000;

export class RtcLink {
  private readonly pc: PeerConnectionLike;
  private channel: DataChannelLike | null = null;
  private remoteSet = false;
  private early: IceCandidateLike[] = [];
  private closed = false;
  private cancelTimeout: () => void;
  onOpen: () => void = () => {};
  onMessage: (data: Uint8Array | string) => void = () => {};
  onClose: () => void = () => {};

  constructor(
    env: MeshEnv,
    ice: IceServer[],
    /** Sends a signalling message (through the socket this link uses). */
    private readonly sendSignal: (s: MeshSignalOut) => void,
    readonly remote: string,
    readonly connection: string,
    readonly label: string,
  ) {
    this.pc = new env.RTCPeerConnection({ iceServers: ice });
    this.pc.onicecandidate = (e) => {
      if (!e.candidate || this.closed) return;
      this.sendSignal({
        type: "candidate",
        peer: this.remote,
        connection: this.connection,
        candidate: e.candidate.candidate,
        mid: e.candidate.sdpMid ?? undefined,
        mline: e.candidate.sdpMLineIndex ?? undefined,
      });
    };
    this.pc.onconnectionstatechange = () => {
      const s = this.pc.connectionState;
      if (s === "failed" || s === "closed" || s === "disconnected") this.close();
    };
    this.pc.ondatachannel = (e) => this.adopt(e.channel);
    this.cancelTimeout = env.after(OPEN_TIMEOUT_MS, () => {
      if (!this.open) this.close();
    });
  }

  get open(): boolean {
    return !this.closed && this.channel?.readyState === "open";
  }

  private adopt(ch: DataChannelLike): void {
    if (this.channel || this.closed) return;
    this.channel = ch;
    ch.binaryType = "arraybuffer";
    const opened = () => {
      this.cancelTimeout();
      if (!this.closed) this.onOpen();
    };
    ch.onopen = opened;
    ch.onclose = () => this.close();
    ch.onmessage = (e) => {
      if (this.closed) return;
      const d = e.data;
      if (typeof d === "string") this.onMessage(d);
      else if (d instanceof ArrayBuffer) this.onMessage(new Uint8Array(d));
      else if (ArrayBuffer.isView(d)) this.onMessage(new Uint8Array(d.buffer, d.byteOffset, d.byteLength));
    };
    if (ch.readyState === "open") opened();
  }

  private async guard(f: () => Promise<void>): Promise<void> {
    try {
      await f();
    } catch {
      this.close();
    }
  }

  /** Creates the channel and sends the offer. */
  dial(): Promise<void> {
    return this.guard(async () => {
      this.adopt(this.pc.createDataChannel(this.label, { ordered: true }));
      const sdp = (await this.pc.createOffer()).sdp ?? "";
      await this.pc.setLocalDescription({ type: "offer", sdp });
      this.sendSignal({ type: "offer", peer: this.remote, connection: this.connection, label: this.label, sdp });
    });
  }

  private async remoteDescription(type: "offer" | "answer", sdp: string): Promise<void> {
    await this.pc.setRemoteDescription({ type, sdp });
    this.remoteSet = true;
    for (const c of this.early.splice(0)) await this.pc.addIceCandidate(c);
  }

  /** Answers the offer `sdp`. */
  accept(sdp: string): Promise<void> {
    return this.guard(async () => {
      await this.remoteDescription("offer", sdp);
      const answer = (await this.pc.createAnswer()).sdp ?? "";
      await this.pc.setLocalDescription({ type: "answer", sdp: answer });
      this.sendSignal({ type: "answer", peer: this.remote, connection: this.connection, sdp: answer });
    });
  }

  /** An answer or a candidate for this connection. */
  signal(s: MeshSignal): void {
    if (this.closed || !("connection" in s) || s.connection !== this.connection) return;
    if (s.type === "answer") {
      void this.guard(() => this.remoteDescription("answer", s.sdp));
    } else if (s.type === "candidate" && s.candidate) {
      const c: IceCandidateLike = { candidate: s.candidate, sdpMid: s.mid ?? null, sdpMLineIndex: s.mline ?? null };
      if (this.remoteSet) void this.pc.addIceCandidate(c).catch(() => {});
      else this.early.push(c);
    }
  }

  /** Sends one message; false when the channel is not open. */
  send(data: Uint8Array | string): boolean {
    if (!this.open || !this.channel) return false;
    try {
      this.channel.send(typeof data === "string" ? data : (new Uint8Array(data) as Uint8Array<ArrayBuffer>));
      return true;
    } catch {
      return false;
    }
  }

  close(): void {
    if (this.closed) return;
    this.closed = true;
    this.cancelTimeout();
    try {
      this.channel?.close();
      this.pc.close();
    } catch {
      // Already closed.
    }
    this.onClose();
  }
}
