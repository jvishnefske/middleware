// U4/U7: the protocol-wasm bindings as seen from JS (wasm-pack output).

import { describe, expect, it } from "vitest";
import type { FrameEvent, LineEvent } from "../src/protocol/wasm";
import { protocol } from "./fakeDevice";

const proto = protocol();
const enc = new TextEncoder();

const frames = (ev: LineEvent[]) => ev.filter((e): e is FrameEvent => e.kind !== "text" && e.kind !== "invalid");
const text = (ev: LineEvent[]) => ev.map((e) => (e.kind === "text" ? e.text : "")).join("");

function concat(...parts: Uint8Array[]): Uint8Array {
  const out = new Uint8Array(parts.reduce((n, p) => n + p.length, 0));
  let at = 0;
  for (const p of parts) {
    out.set(p, at);
    at += p.length;
  }
  return out;
}

describe("decoder", () => {
  it("finds frames amid ROM boot-banner noise", () => {
    const banner = "ESP-ROM:esp32s3-20210327\r\nrst:0x1 (POWERON),boot:0x8 (SPI_FAST_FLASH_BOOT)\r\n";
    const stream = concat(
      enc.encode(banner),
      proto.encodePublish("sys/log", enc.encode("booted")),
      enc.encode("I (42) main: hello\n"),
      proto.encodeResponse(7, 0, new Uint8Array([1, 2])),
    );
    const ev = proto.newDecoder().push(stream);
    expect(text(ev)).toBe(banner + "I (42) main: hello\n");
    const f = frames(ev);
    expect(f).toHaveLength(2);
    expect(f[0]).toMatchObject({ kind: "publish", key: "sys/log" });
    expect(new TextDecoder().decode(f[0].payload)).toBe("booted");
    expect(f[1]).toMatchObject({ kind: "response", id: 7, status: "ok", statusCode: 0 });
    expect([...f[1].payload]).toEqual([1, 2]);
  });

  it("decodes identically when fed one byte at a time", () => {
    const stream = concat(enc.encode("noise\n"), proto.encodeRequest(3, "app/led", new Uint8Array([0, 1, 0])));
    const d = proto.newDecoder();
    const ev = [...stream].flatMap((b) => d.push(new Uint8Array([b])));
    expect(text(ev)).toBe("noise\n");
    expect(frames(ev)).toEqual([{ kind: "request", id: 3, key: "app/led", payload: new Uint8Array([0, 1, 0]) }]);
  });

  it("reports corrupt frames as invalid and error responses with their status", () => {
    const bad = proto.encodePublish("a/b", enc.encode("xyz"));
    bad[4] ^= 0x40;
    const ev = proto.newDecoder().push(concat(bad, proto.encodeResponse(9, 1, new Uint8Array())));
    expect(ev[0]).toMatchObject({ kind: "invalid", reason: "CRC mismatch" });
    expect(ev[1]).toMatchObject({ kind: "response", id: 9, status: "NotFound", statusCode: 1 });
  });
});

describe("encoders", () => {
  it("terminate frames with a single 0x00 and reject bad keys", () => {
    const w = proto.encodeRequest(1, "sys/info", new Uint8Array());
    expect(w[w.length - 1]).toBe(0);
    expect(w.slice(0, -1).includes(0)).toBe(false);
    expect(() => proto.encodePublish("bad//key", new Uint8Array())).toThrow(/invalid key/);
    expect(() => proto.encodePublish("a", new Uint8Array(2000))).toThrow(/too large/);
  });

  it("names statuses", () => {
    expect([0, 1, 2, 3, 4].map(proto.statusName)).toEqual(["ok", "NotFound", "InvalidRequest", "BufferTooSmall", "Internal"]);
    expect([0, 1, 2, 3, 4, 5].map(proto.payloadStatusName)).toEqual([
      "empty",
      "stopped",
      "running",
      "trapped",
      "loading",
      undefined,
    ]);
  });
});

describe("loader helpers", () => {
  it("plans begin / ordered chunks / commit with the image CRC-32", () => {
    const image = Uint8Array.from({ length: 1300 }, (_, i) => (i * 7) & 0xff);
    const plan = proto.uploadPlan(image);
    expect(proto.constants.chunkMax).toBe(512);
    expect(plan.map((s) => s.key)).toEqual([
      "sys/loader/begin",
      "sys/loader/chunk",
      "sys/loader/chunk",
      "sys/loader/chunk",
      "sys/loader/commit",
    ]);
    const begin = new DataView(plan[0].body.buffer, plan[0].body.byteOffset);
    expect(begin.getUint32(0, true)).toBe(1300);
    expect(begin.getUint32(4, true)).toBe(proto.crc32(image));
    const offsets = plan.slice(1, 4).map((s) => new DataView(s.body.buffer, s.body.byteOffset).getUint32(0, true));
    expect(offsets).toEqual([0, 512, 1024]);
    expect(plan.map((s) => s.done)).toEqual([0, 512, 1024, 1300, 1300]);
    expect(() => proto.uploadPlan(new Uint8Array())).toThrow(/empty/);
  });

  it("computes the reference CRC-32", () => {
    expect(proto.crc32(enc.encode("123456789"))).toBe(0xcbf43926);
  });

  it("round-trips sys/info", () => {
    const body = proto.encodeDeviceInfo({ abiVersion: 1, capacity: 65536, status: 2, chip: "esp32s3", firmware: "0.1.0" });
    expect(proto.decodeDeviceInfo(body)).toEqual({
      abiVersion: 1,
      capacity: 65536,
      status: 2,
      statusName: "running",
      chip: "esp32s3",
      firmware: "0.1.0",
    });
    expect(proto.decodeDeviceInfo(body.slice(0, 3))).toBeUndefined();
  });
});

describe("ABI constants", () => {
  it("mirror middleware::wasm_abi", () => {
    const abi = proto.abi;
    expect(abi.importModule).toBe("mw");
    expect(abi.maxMessage).toBe(512);
    expect(abi.imports.map((i) => i.name)).toEqual([
      "log",
      "publish",
      "subscribe",
      "now_ms",
      "set_tick",
      "gpio_output",
      "gpio_input",
      "gpio_write",
      "gpio_read",
      "now_us",
      "irq_overflow",
      "gpio_irq",
      "timer_start",
      "timer_stop",
      "spi_open",
      "spi_transfer",
      "dma_read",
      "uart_open",
      "uart_write",
      "uart_read",
    ]);
    expect(abi.imports.find((i) => i.name === "publish")).toEqual({ name: "publish", params: 4, returns: true });
    expect(abi.exports).toEqual({ memory: "memory", init: "init", tick: "tick", alloc: "mw_alloc", onMessage: "on_message", onIrq: "on_irq" });
    expect(abi.errors.InvalidKey).toBe(-6);
    expect(proto.constants.keys.status).toBe("sys/payload/status");
  });
});
