// Sample batches (SC1): the messages of an IMU or microphone source route,
// `middleware::stream::samples` in the Rust core. Each message stands
// alone: a 32-byte little-endian header, then exactly `count` units.
// `firstIndex` counts units since the route started, so the gap between
// two batches (`lostBetween`) says exactly how many were lost. Mirrors the
// Rust decoder's refusals; the browser simulator's fake IMU and microphone
// publish these.

export const SAMPLES_VERSION = 1;
export const SAMPLES_HEADER_LEN = 32;
/** One ABI message (`wasm_abi::MAX_MESSAGE`). */
export const SAMPLES_MAX_BATCH = 512;

export type SensorKind = "imu" | "microphone";
export type SampleFormat = "i16" | "mulaw" | "levels";

const KINDS: SensorKind[] = ["imu", "microphone"];
const FORMATS: SampleFormat[] = ["i16", "mulaw", "levels"];
const CHANNEL_BYTES: Record<SampleFormat, number> = { i16: 2, mulaw: 1, levels: 4 };

export interface SampleHeader {
  kind: SensorKind;
  format: SampleFormat;
  channels: number;
  rateHz: number;
  /** Units since the route started (a bigint: the wire field is u64). */
  firstIndex: bigint;
  /** When the first unit was taken, µs on the board clock. */
  timestampUs: bigint;
  count: number;
  /** IMU: full scale in ±g. Microphone: gain in dB. */
  range: number;
  /** IMU: µg per accelerometer LSB. Levels: audio samples per block. */
  scale: number;
}

export interface SampleBatch {
  header: SampleHeader;
  /** The units, still encoded. */
  data: Uint8Array;
}

export class SampleBatchError extends Error {}

function check(h: SampleHeader): void {
  const fail = (why: string) => {
    throw new SampleBatchError(why);
  };
  if (h.rateHz === 0) fail("zero rate");
  const monoOrStereo = h.channels === 1 || h.channels === 2;
  const ok =
    h.kind === "imu"
      ? h.format === "i16" && (h.channels === 3 || h.channels === 6) && h.range !== 0 && h.scale !== 0
      : monoOrStereo && (h.format === "levels" ? h.scale !== 0 : h.scale === 0);
  if (!ok) fail(`${h.format} with ${h.channels} channels does not fit ${h.kind}`);
  if (h.count === 0) fail("count past the message");
  if (SAMPLES_HEADER_LEN + h.count * h.channels * CHANNEL_BYTES[h.format] > SAMPLES_MAX_BATCH) {
    fail("batch above one message");
  }
}

/** Parses one batch; throws `SampleBatchError` when the Rust decoder would refuse it. */
export function decodeSampleBatch(msg: Uint8Array): SampleBatch {
  if (msg.length < SAMPLES_HEADER_LEN) throw new SampleBatchError("truncated");
  const v = new DataView(msg.buffer, msg.byteOffset, msg.byteLength);
  if (msg[0] !== SAMPLES_VERSION) throw new SampleBatchError(`unknown version ${msg[0]}`);
  const kind = KINDS[msg[1]];
  const format = FORMATS[msg[2]];
  if (!kind || !format) throw new SampleBatchError("unknown kind or format");
  const header: SampleHeader = {
    kind,
    format,
    channels: msg[3],
    rateHz: v.getUint32(4, true),
    firstIndex: v.getBigUint64(8, true),
    timestampUs: v.getBigUint64(16, true),
    count: v.getUint16(24, true),
    range: v.getUint16(26, true),
    scale: v.getUint32(28, true),
  };
  check(header);
  const need = header.count * header.channels * CHANNEL_BYTES[header.format];
  const data = msg.subarray(SAMPLES_HEADER_LEN);
  if (data.length < need) throw new SampleBatchError("count past the message");
  if (data.length > need) throw new SampleBatchError("bytes after the last unit");
  return { header, data };
}

/** The `i16` values of an `i16` batch, unit by unit, channel by channel. */
export function sampleValues(b: SampleBatch): Int16Array {
  if (b.header.format !== "i16") return new Int16Array(0);
  const v = new DataView(b.data.buffer, b.data.byteOffset, b.data.byteLength);
  return Int16Array.from({ length: b.data.length / 2 }, (_, i) => v.getInt16(2 * i, true));
}

/** The `(rms, peak)` pairs of a levels batch, in `i16` full scale. */
export function sampleLevels(b: SampleBatch): Array<{ rms: number; peak: number }> {
  if (b.header.format !== "levels") return [];
  const v = new DataView(b.data.buffer, b.data.byteOffset, b.data.byteLength);
  return Array.from({ length: b.data.length / 4 }, (_, i) => ({
    rms: v.getUint16(4 * i, true),
    peak: v.getUint16(4 * i + 2, true),
  }));
}

/** Units lost between consecutive batches `prev` and `next`: 0 when
 * contiguous, `null` when `next` does not start after `prev` (a duplicate,
 * reordering or restarted route). */
export function lostBetween(prev: SampleHeader, next: SampleHeader): bigint | null {
  const expected = prev.firstIndex + BigInt(prev.count);
  return next.firstIndex >= expected ? next.firstIndex - expected : null;
}
