// Port sharing between the Flash and Device tabs, and payload formatting.

import { describe, expect, it } from "vitest";
import { PortManager } from "../src/serial/ports";
import { formatBytes, parseHex } from "../src/ui/dom";

const fakePort = { getInfo: () => ({ usbVendorId: 0x303a, usbProductId: 0x1001 }) };

describe("PortManager", () => {
  it("asks the current owner to release before handing the port to another tab", async () => {
    const ports = new PortManager(async () => fakePort);
    const log: string[] = [];
    await ports.acquire("device", async () => {
      log.push("device released");
    });
    expect(ports.currentOwner).toBe("device");
    await ports.acquire("flash", async () => {
      log.push("flash released");
    });
    expect(log).toEqual(["device released"]);
    expect(ports.currentOwner).toBe("flash");
    ports.released("flash");
    expect(ports.currentOwner).toBeNull();
    // Re-acquiring by the same owner does not release itself.
    await ports.acquire("device", async () => {
      log.push("x");
    });
    await ports.acquire("device", async () => {
      log.push("y");
    });
    expect(log).toEqual(["device released"]);
  });

  it("prompts for a port only when none is selected", async () => {
    let prompts = 0;
    const ports = new PortManager(async () => {
      prompts++;
      return fakePort;
    });
    await ports.acquire("a", async () => {});
    await ports.acquire("b", async () => {});
    expect(prompts).toBe(1);
  });
});

describe("payload formatting", () => {
  it("shows text as a string and binary as hex", () => {
    expect(formatBytes(new TextEncoder().encode("hi"))).toBe('"hi"');
    expect(formatBytes(new Uint8Array([0, 255, 16]))).toBe("00 ff 10");
  });

  it("parses hex input", () => {
    expect([...parseHex("01 ff 0x0a")]).toEqual([1, 255, 10]);
    expect(() => parseHex("abc")).toThrow();
  });
});
