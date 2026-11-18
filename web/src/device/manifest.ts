// What the Device page says about a node beyond `sys/info` (MF4): its
// capabilities, its `sys/manifest` (what it offers payloads: import
// groups, buses, sources with their rates, links, limits) and, from an
// ESP32, `sys/board`. Pure: `devicePage.ts` renders the rows.

import type { BoardInfo, DeviceInfo, Manifest } from "../protocol/wasm";

/** A `[label, value]` row of the Device page's info list. */
export type Row = [string, string];

const list = (v: readonly (string | number)[]) => (v.length ? v.join(", ") : "none");
const buses = (n: number) => `${n} ${n === 1 ? "bus" : "buses"}`;

/** The rows for `info`, the node's manifest (null: it answered NotFound) and
 * its board description (null: none). */
export function capabilityRows(info: DeviceInfo, manifest: Manifest | null, board: BoardInfo | null): Row[] {
  const rows: Row[] = [["Capabilities", list(info.capNames)]];
  if (!manifest) {
    rows.push(["Manifest", "not reported (firmware without sys/manifest)"]);
  } else {
    const m = manifest;
    rows.push(["Imports", list(m.groupNames)]);
    if (m.i2c) rows.push(["I2C", `${buses(m.i2c.buses)}, ${m.i2c.minHz}–${m.i2c.maxHz} Hz`]);
    if (m.spi) rows.push(["SPI", buses(m.spi.buses)]);
    if (m.imu) {
      const mag = m.imu.magRatesHz.length ? `with magnetometer ${list(m.imu.magRatesHz)} Hz` : "no magnetometer";
      rows.push(["IMU", `${list(m.imu.ratesHz)} Hz, ±${m.imu.rangesG.join("/")} g; ${mag}`]);
    }
    if (m.mic) rows.push(["Microphone", `${list(m.mic.ratesHz)} Hz; ${list(m.mic.modes)}; gain 0–${m.mic.maxGainDb} dB`]);
    if (m.frames) {
      const f = m.frames;
      rows.push(["Frames", `${list(f.kinds)}; ${list(f.formats)}; up to ${f.maxWidth}×${f.maxHeight}`]);
    }
    if (m.sinkSpi) rows.push(["SPI sink", `frames up to ${m.sinkSpi.maxFrame} bytes`]);
    rows.push(["Links", list(m.links.map((l) => `${l.id} ${l.kindName}`))]);
    if (m.limits) {
      const l = m.limits;
      rows.push(["Limits", `${l.routes} routes, ${l.subscriptions} subscriptions, ${l.channels} channels, ${l.maxMessage}-byte messages`]);
    }
  }
  if (board) {
    rows.push([
      "Board",
      `${board.gpio.length} GPIO, ${board.adc.length} ADC, PWM ${board.pwmMaxBits ? `${board.pwmMaxBits} bits` : "none"}, max image ${board.maxImage} bytes`,
    ]);
  }
  return rows;
}
