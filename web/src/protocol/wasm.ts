// Typed facade over the protocol-wasm package (Rust compiled with wasm-pack).
//
// Every frame, CRC, loader body and ABI constant used by the site comes from
// the Rust middleware crate through this module (requirement U4); nothing
// here re-implements the protocol. The raw wasm-bindgen exports return plain
// objects typed `object`/`any`; this file gives them TypeScript shapes.

import init, * as pkg from "../pkg/protocol_wasm.js";

/** A frame decoded from the serial line. */
export interface FrameEvent {
  kind: "request" | "response" | "publish";
  id: number;
  key?: string;
  payload: Uint8Array;
  /** Responses only: "ok" or a handler error name. */
  status?: string;
  /** Responses only: 0 for success, otherwise the wire error code. */
  statusCode?: number;
}

/** Bytes on the line that are not frames (boot banners, prints). */
export interface TextEvent {
  kind: "text";
  text: string;
}

/** A delimited segment that looked like a frame but failed to decode. */
export interface InvalidEvent {
  kind: "invalid";
  reason: string;
  len: number;
}

export type LineEvent = FrameEvent | TextEvent | InvalidEvent;

export interface UploadStep {
  key: string;
  body: Uint8Array;
  /** Image bytes delivered once this step succeeds. */
  done: number;
}

export interface DeviceInfo {
  abiVersion: number;
  capacity: number;
  status: number;
  statusName: string;
  chip: string;
  firmware: string;
}

export interface LoaderStepResult {
  statusCode: number;
  status: string;
  action?: "app" | "info" | "start" | "stop" | "erase" | "write" | "activate";
  offset?: number;
  data?: Uint8Array;
  len?: number;
  crc?: number;
}

export interface ProtocolConstants {
  chunkMax: number;
  maxRawFrame: number;
  maxWireFrame: number;
  keys: {
    info: string;
    begin: string;
    chunk: string;
    commit: string;
    start: string;
    stop: string;
    log: string;
    status: string;
  };
}

export interface AbiImport {
  name: string;
  params: number;
  returns: boolean;
}

export interface AbiInfo {
  version: number;
  importModule: string;
  maxSubscriptions: number;
  maxMessage: number;
  maxLog: number;
  fuelPerCall: number;
  minTickMs: number;
  /** Every import of every ABI revision (ABI 2 included). */
  imports: AbiImport[];
  exports: { memory: string; init: string; tick: string; alloc: string; onMessage: string; onIrq: string };
  errors: Record<string, number>;
  /** `on_irq` `kind` codes. */
  eventKinds: { Gpio: number; Timer: number; DmaDone: number; UartRx: number };
  /** `DmaDone` `source` / `dma_read` channel codes. */
  dmaChannels: { Spi0: number; Uart1Tx: number };
  /** `gpio_irq` edge codes. */
  edges: { Off: number; Rising: number; Falling: number; Any: number };
  maxEvents: number;
  maxTimers: number;
  minTimerUs: number;
  maxDma: number;
  spiBuses: number;
  maxSpiKhz: number;
  uartPort: number;
  uartRxBuffer: number;
}

/** An interrupt event popped from the queue for `on_irq`. */
export interface IrqEvent {
  kind: "irq";
  irq: "Gpio" | "Timer" | "DmaDone" | "UartRx";
  code: number;
  source: number;
  value: number;
  timeUs: number;
  /** Whether the payload exports `on_irq` (otherwise the event was discarded). */
  handled: boolean;
}

/** A started hardware timer. */
export interface TimerState {
  id: number;
  periodUs: number;
  periodic: boolean;
  active: boolean;
  /** Periods elapsed since load. */
  fires: number;
}

export type SimEvent =
  | { kind: "log"; line: string }
  | { kind: "publish"; key: string; data: Uint8Array }
  | { kind: "output"; pin: number }
  | { kind: "input"; pin: number; pull: "none" | "up" | "down" }
  | { kind: "write"; pin: number; high: boolean }
  | { kind: "spi"; tx: Uint8Array; rx: Uint8Array }
  | { kind: "uart"; data: Uint8Array }
  | IrqEvent;

export interface PinState {
  mode: "unused" | "output" | "input";
  high?: boolean;
  pull?: "none" | "up" | "down";
}

/** A payload running in wasm-host against a virtual device. */
export interface Simulator {
  tick(nowMs: number): void;
  /** One main-loop pass at `nowUs`; returns the number of events dispatched. */
  step(nowUs: number): number;
  setNowUs(nowUs: number): void;
  /** Bytes arriving on UART 1; returns how many were buffered. */
  uartInject(data: Uint8Array): number;
  /** Empty selects loopback; otherwise the pattern repeats. */
  setSpiPattern(pattern: Uint8Array): void;
  timers(): TimerState[];
  /** Events dropped from the full queue since load. */
  readonly dropped: number;
  readonly pending: number;
  readonly handlesIrq: boolean;
  deliver(key: string, data: Uint8Array): boolean;
  setNow(nowMs: number): void;
  nextTick(): number | undefined;
  subscriptions(): string[];
  setInput(pin: number, high: boolean): boolean;
  pin(pin: number): PinState | undefined;
  readonly pinCount: number;
  readonly trapped: boolean;
  free(): void;
}

/** A streaming decoder for the serial line. */
export interface Decoder {
  push(bytes: Uint8Array): LineEvent[];
}

/** A device-side loader running the Rust state machine (for fakes). */
export interface DeviceLoader {
  readonly receiving: boolean;
  step(key: string, body: Uint8Array): LoaderStepResult;
}

export interface Protocol {
  newDecoder(): Decoder;
  encodeRequest(id: number, key: string, payload: Uint8Array): Uint8Array;
  encodePublish(key: string, payload: Uint8Array): Uint8Array;
  encodeResponse(id: number, status: number, payload: Uint8Array): Uint8Array;
  statusName(code: number): string;
  crc32(data: Uint8Array): number;
  uploadPlan(image: Uint8Array, chunk?: number): UploadStep[];
  decodeDeviceInfo(data: Uint8Array): DeviceInfo | undefined;
  encodeDeviceInfo(info: Omit<DeviceInfo, "statusName">): Uint8Array;
  payloadStatusName(b: number): string | undefined;
  newDeviceLoader(capacity: number): DeviceLoader;
  readonly constants: ProtocolConstants;
  readonly abi: AbiInfo;
  readonly hasSimulator: boolean;
  /** Loads a payload into the simulator; throws on rejection or init trap. */
  newSimulator(wasm: Uint8Array, onEvent: (e: SimEvent) => void): Simulator;
}

function wrap(): Protocol {
  const constants = pkg.protocol() as ProtocolConstants;
  const abi = pkg.abi() as AbiInfo;
  const hasSimulator = pkg.hasSimulator();
  return {
    newDecoder: () => {
      const d = new pkg.Decoder();
      return { push: (bytes) => d.push(bytes) as LineEvent[] };
    },
    encodeRequest: pkg.encodeRequest,
    encodePublish: pkg.encodePublish,
    encodeResponse: pkg.encodeResponse,
    statusName: pkg.statusName,
    crc32: pkg.crc32,
    uploadPlan: (image, chunk) => pkg.uploadPlan(image, chunk ?? constants.chunkMax) as UploadStep[],
    decodeDeviceInfo: (data) => pkg.decodeDeviceInfo(data) as DeviceInfo | undefined,
    encodeDeviceInfo: (i) => pkg.encodeDeviceInfo(i.abiVersion, i.capacity, i.status, i.chip, i.firmware),
    payloadStatusName: pkg.payloadStatusName,
    newDeviceLoader: (capacity) => {
      const l = new pkg.DeviceLoader(capacity);
      return {
        get receiving() {
          return l.receiving;
        },
        step: (key, body) => l.step(key, body) as LoaderStepResult,
      };
    },
    constants,
    abi,
    hasSimulator,
    newSimulator: (wasm, onEvent) => {
      // `Simulator` exists only when protocol-wasm was built with the
      // `simulator` feature (the default).
      const Ctor = (pkg as unknown as { Simulator?: new (w: Uint8Array, f: (e: unknown) => void) => Simulator })
        .Simulator;
      if (!hasSimulator || !Ctor) throw new Error("protocol-wasm was built without the simulator feature");
      return new Ctor(wasm, (e) => onEvent(e as SimEvent));
    },
  };
}

let loaded: Protocol | null = null;

/** Fetches and instantiates the WASM module (browser). */
export async function loadProtocol(): Promise<Protocol> {
  if (!loaded) {
    await init();
    loaded = wrap();
  }
  return loaded;
}

/** Instantiates from bytes already in memory (tests, workers). */
export function loadProtocolSync(wasm: BufferSource): Protocol {
  if (!loaded) {
    pkg.initSync({ module: wasm });
    loaded = wrap();
  }
  return loaded;
}
