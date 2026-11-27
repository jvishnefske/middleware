// Reacting to a board: when a serial port the page may use appears, ask
// mwctl whether the board on it is ready to update (a base firmware
// answers, so a wiring can be written) or ready to flash (and how).
//
// `mwctl --port web probe --usb VID:PID` decides (tools/mwctl/src/ready.rs);
// this file only starts it and reads its `name: value` lines.

import type { Mwctl, PortHost } from "./host";
import { WEB_PORT } from "./port";

export interface BoardState {
  /** mwctl's verdict. */
  ready: "update" | "flash" | "unknown";
  /** The other lines of the report: `chip`, `firmware`, `payloads`,
   * `method`, `board`, `how`, `usb`, ... */
  fields: Record<string, string>;
}

/** Parses probe's report. Null when the first line is not a verdict. */
export function parseProbe(stdout: string): BoardState | null {
  const fields: Record<string, string> = {};
  for (const line of stdout.split("\n")) {
    const at = line.indexOf(": ");
    if (at > 0) fields[line.slice(0, at)] = line.slice(at + 2).trim();
  }
  const ready = fields.ready;
  if (ready !== "update" && ready !== "flash" && ready !== "unknown") return null;
  delete fields.ready;
  return { ready, fields };
}

/** `VVVV:PPPP` for a port's USB ids, or null for a port that is not USB. */
export function usbId(info: { usbVendorId?: number; usbProductId?: number }): string | null {
  if (info.usbVendorId === undefined || info.usbProductId === undefined) return null;
  const hex = (n: number) => n.toString(16).padStart(4, "0");
  return `${hex(info.usbVendorId)}:${hex(info.usbProductId)}`;
}

/** Probes the board on `port`. Throws with mwctl's message when the port
 * itself fails (in use, unplugged). */
export async function probe(mwctl: Mwctl, port: PortHost, usb: string | null, timeoutMs?: number): Promise<BoardState> {
  const args = ["--port", WEB_PORT, "probe"];
  if (usb) args.push("--usb", usb);
  if (timeoutMs) args.push("--timeout", String(timeoutMs));
  const r = await mwctl.run(args, { port });
  const state = r.code === 0 ? parseProbe(r.stdout) : null;
  if (!state) throw new Error(r.stderr.replace(/^mwctl: /, "").trim() || `mwctl probe exited with status ${r.code}`);
  return state;
}

/** One sentence for the board bar. */
export function describeBoard(s: BoardState): string {
  const f = s.fields;
  switch (s.ready) {
    case "update":
      return f.payloads === "run"
        ? `${f.chip} running ${f.firmware}: ready to update. A wiring can be written now.`
        : `${f.chip} running ${f.firmware}: this firmware stores payloads but cannot run them.`;
    case "flash":
      return `${f.board ?? "Board"}: ready to flash. ${capitalise(f.how ?? "")}`;
    default:
      return `${f.board ?? "Board"}: no answer. ${capitalise(f.how ?? "")}`;
  }
}

function capitalise(s: string): string {
  return s ? `${s[0].toUpperCase()}${s.slice(1)}${s.endsWith(".") ? "" : "."}` : "";
}

/** Whether a wiring can be written and run on the board. */
export function canWrite(s: BoardState | null): boolean {
  return s?.ready === "update" && s.fields.payloads === "run";
}

/** Whether the page's Flash tab installs this board's base firmware. */
export function flashesHere(s: BoardState | null): boolean {
  return s?.ready === "flash" && s.fields.method === "esp-rom";
}

/** The part of `navigator.serial` the watcher uses. */
export interface SerialEvents<P> {
  getPorts(): Promise<P[]>;
  addEventListener(type: "connect" | "disconnect", listener: (e: Event) => void): void;
}

/** Calls `appeared` for every port the page was already granted, now and
 * whenever one is plugged in, and `gone` when one is unplugged. */
export async function watchPorts<P>(serial: SerialEvents<P>, appeared: (port: P) => void, gone: (port: P) => void): Promise<void> {
  serial.addEventListener("connect", (e) => appeared(e.target as P));
  serial.addEventListener("disconnect", (e) => gone(e.target as P));
  for (const port of await serial.getPorts()) appeared(port);
}
