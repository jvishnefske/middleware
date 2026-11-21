// Typed facade over the protocol-wasm package (Rust compiled with wasm-pack).
//
// Every frame, CRC, loader body and ABI constant used by the site comes from
// the Rust middleware crate through this module (requirement U4); nothing
// here re-implements the protocol. The raw wasm-bindgen exports return plain
// objects typed `object`/`any`; this file gives them TypeScript shapes.

import type { Flow, FlowIssue, PhaseDecl } from "../model/flows";
import type { Machine, MachineIssue } from "../model/machines";
import type { BusSetting, PinFunc, PinIssue, PinRow } from "../model/pins";
import type { PhaseClass, RecipeIssue } from "../model/procedural";
import type { Allocation, MasterRecipe, ScriptEvent, SimulatedRun } from "../model/recipe";
import type { AllocUnit } from "../model/plant";
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
  /** Node id as 16 hex digits (middleware 0.5 firmware), if reported. */
  node?: string;
  /** Capability bits after the node id (middleware 0.6 firmware); 0 when absent. */
  caps: number;
  /** Names of the set bits that have one, lowest first: "serve", "bridge" (bit 1),
   * "source_imu" (bit 2), "source_microphone" (bit 3). */
  capNames: string[];
  /** Bit 0: requests on the payload's served keys reach the payload (S88 phases can run on it). */
  serves: boolean;
}

/** A decoded `sys/board` body: pin classes as ascending GPIO numbers,
 * `pwmPins[2·timer + channel]`. */
export interface BoardInfo {
  abiVersion: number;
  maxImage: number;
  maxMemory: number;
  gpio: number[];
  reserved: number[];
  inputOnly: number[];
  strapping: number[];
  adc: number[];
  pwmMaxBits: number;
  runtime: boolean;
  usbJtag: boolean;
  pwmRefTick: boolean;
  uartTx: number;
  uartRx: number;
  pwmPins: number[];
}

/** A decoded `sys/manifest` body (MF4): what a node offers payloads. */
export interface Manifest {
  version: number;
  abi: number;
  /** Node id as 16 hex digits, when the node sent one. */
  node?: string;
  /** Import groups served (bitmask) and their names: "gpio", "timers", "irq",
   * "spi", "uart", "dma", "pwm", "adc", "i2c", "routes", "serve", "bridge",
   * "channels". */
  groups: number;
  groupNames: string[];
  i2c?: { buses: number; minHz: number; maxHz: number };
  spi?: { buses: number };
  /** IMU source: rates for 3-channel samples, ranges in ±g, rates with the
   * magnetometer (empty: none). */
  imu?: { ratesHz: number[]; rangesG: number[]; magRatesHz: number[] };
  /** Microphone source: rates, modes ("levels", "mulaw", "pcm"), highest gain. */
  mic?: { ratesHz: number[]; modes: string[]; maxGainDb: number };
  /** Frame sources: kinds ("testPattern", "camera"), formats ("rgb565", "jpeg"). */
  frames?: { kinds: string[]; formats: string[]; maxWidth: number; maxHeight: number };
  sinkSpi?: { maxFrame: number };
  links: { id: number; kind: number; kindName: string }[];
  limits?: { routes: number; subscriptions: number; channels: number; maxMessage: number };
  /** The board's model (a Linux node's device tree `model`), when sent. */
  board?: string;
  /** ABI 8: the node lists control-module points at `sys/manifest/cm`. */
  controlModules?: { points: number; pages: number };
  /** The `sys/info` capability bits the manifest implies, and their names. */
  caps: number;
  capNames: string[];
}

/** What a channel carries: ABI 7's kinds, and ABI 8's machine kinds. */
export type ChannelKind = "bytes" | "samples" | "frames" | MachineKind;
/** ABI 8: a switch, a measured position, a commanded value, a segment queue. */
export type MachineKind = "digital" | "position" | "setpoint" | "segments";

/** One control-module point a node's driver exposes (ABI 8, `sys/manifest/cm`). */
export interface ControlPoint {
  name: string;
  kind: MachineKind;
  /** Seen from the payload: it reads ("in"), writes ("out"), or both (a segment queue). */
  flow: "in" | "out" | "both";
  unit: "none" | "bool" | "um" | "count" | "rpm" | "station" | "um/s";
  min: number;
  max: number;
  /** Counts per mm for a position point, 0 otherwise. */
  scale: number;
}

/** One page of `sys/manifest/cm`. */
export interface ControlPage {
  pages: number;
  page: number;
  points: ControlPoint[];
}

/** A channel a payload declares in its `mw.channels` section (ABI 7). */
export interface ChannelDecl {
  name: string;
  dir: "in" | "out";
  kind: ChannelKind;
  sensor: "imu" | "mic" | null;
  /** Mask over sample format codes (i16 0, mulaw 1, levels 2) or pixel
   * formats; 0 = any. */
  formats: number;
  /** Channels per unit, 0 = any. */
  channels: number;
  rateMin: number;
  rateMax: number;
  required: boolean;
}

/** A local source a channel binds to. */
export type LocalSource =
  | { kind: "local"; source: "mic"; rateHz: number; gainDb: number; mode: "levels" | "mulaw" | "pcm"; units: number; block: number }
  | { kind: "local"; source: "imu"; rateHz: number; rangeG: number; mag: boolean; units: number }
  | { kind: "local"; source: "testPattern" | "camera"; cfg: Uint8Array };

/** A stream transformation applied before `on_message`. */
export type Adapter =
  | { kind: "pcmToLevels"; block: number }
  | { kind: "pcmToMuLaw" }
  | { kind: "decimate"; factor: number }
  | { kind: "imuToMagnitude" };

/** Where a channel's data comes from or goes to. */
export type Endpoint =
  | LocalSource
  | { kind: "key"; key: string }
  | { kind: "remote"; node: string; key: string }
  /** ABI 8: a control-module point of the node's driver. */
  | { kind: "driver"; point: string };
export type BindingTarget = Endpoint | { kind: "adapted"; adapter: Adapter; inner: Endpoint };

/** A binding record: which image it is for, its version, each channel's target. */
export interface BindingRecord {
  imageSha256: Uint8Array;
  version: number;
  entries: { name: string; target: BindingTarget }[];
}

/** One problem `checkBinding` found, with its one-click fix when there is one. */
export interface BindingIssue {
  channel: string;
  message: string;
  fix?: LocalSource | { kind: "keyNamed" } | { kind: "remove" };
}

export interface LoaderStepResult {
  statusCode: number;
  status: string;
  action?: "app" | "info" | "board" | "manifest" | "bind" | "binding" | "start" | "stop" | "erase" | "write" | "activate";
  /** For "bind", the record to store. */
  body?: Uint8Array;
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
    board: string;
    manifest: string;
    bind: string;
    binding: string;
    peers: string;
    begin: string;
    chunk: string;
    commit: string;
    start: string;
    stop: string;
    log: string;
    status: string;
    trustInfo: string;
    trustClaim: string;
    /** `sys/manifest/cm`: the node's control-module points, paged (ABI 8). */
    controlModules: string;
  };
}

/** A finished `MeshSession` command (protocol-wasm `mesh`). */
export type MeshCompletion =
  | { id: number; kind: "opened"; sender: string }
  | { id: number; kind: "closed" }
  | { id: number; kind: "reply"; body: Uint8Array }
  | { id: number; kind: "replies"; replies: { sender?: string; body?: Uint8Array; status?: number }[] }
  | {
      id: number;
      kind: "error";
      failure: "handler" | "noReply" | "down" | "tooLarge" | "other";
      /** The `HandlerError` wire code, for "handler". */
      status?: number;
      message?: string;
    };

/** The browser's Zenoh session over one WebSocket (protocol-wasm
 * `MeshSession`): the page feeds it messages and commands and polls it with
 * the time; every method is synchronous. */
export interface MeshSessionLike {
  feed(msg: Uint8Array): void;
  /** The WebSocket closed. */
  closed(): void;
  /** Messages to send, each one Zenoh batch with no length prefix. */
  takeOutput(): Uint8Array[];
  open(): number;
  close(): number;
  /** To the node with this sender id (16 hex), or to whoever serves `key`. */
  request(sender: string | undefined, key: string, body: Uint8Array, timeoutMs: number): number;
  /** `key` on every node (`@mw/node/<any>/<key>`). */
  discover(key: string, body: Uint8Array, timeoutMs: number, max: number): number;
  poll(nowMs: number): MeshCompletion[];
  nextWakeMs(): number | undefined;
  /** True once the opened session has ended (lease, the router's Close,
   * the link); it then ignores input. */
  isDown(): boolean;
  readonly senderId: string;
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
  exports: { memory: string; init: string; tick: string; alloc: string; onMessage: string; onIrq: string; onRequest?: string };
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

/** An open PWM timer (ABI 3): its frequency and each channel's last duty. */
export interface PwmState {
  timer: number;
  freqHz: number;
  /** Channels 0–3, as fractions of 65535. */
  duty: number[];
}

export type SimEvent =
  | { kind: "log"; line: string }
  | { kind: "publish"; key: string; data: Uint8Array }
  | { kind: "output"; pin: number }
  | { kind: "input"; pin: number; pull: "none" | "up" | "down" }
  | { kind: "write"; pin: number; high: boolean }
  | { kind: "spi"; tx: Uint8Array; rx: Uint8Array }
  | { kind: "uart"; data: Uint8Array }
  | { kind: "pwmOpen"; timer: number; freqHz: number }
  | { kind: "pwmSet"; timer: number; channel: number; duty: number }
  | { kind: "pwmStop"; timer: number }
  | { kind: "serve"; key: string }
  | IrqEvent;

/** What a served request came back with (ABI 5): the reply, or the
 * `HandlerError` name a device answers instead. */
export type RequestResult = { ok: true; reply: Uint8Array } | { ok: false; error: string };

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
  /** Sets what `adc_read(pin)` returns (0–4095, 0 until set); throws when out of range. */
  setAdc(pin: number, value: number): void;
  /** Open PWM timers. */
  pwm(): PwmState[];
  /** Events dropped from the full queue since load. */
  readonly dropped: number;
  readonly pending: number;
  readonly handlesIrq: boolean;
  /** Delivers a message from `sender` (16 hex digits; unknown when omitted). */
  deliver(key: string, data: Uint8Array, sender?: string): boolean;
  setNow(nowMs: number): void;
  nextTick(): number | undefined;
  subscriptions(): string[];
  setInput(pin: number, high: boolean): boolean;
  pin(pin: number): PinState | undefined;
  readonly pinCount: number;
  readonly trapped: boolean;
  /** Hands a request to the payload serving `key` (ABI 5); throws on a
   * trap (the payload stops) or a malformed key. */
  request(key: string, body: Uint8Array, deadlineMs?: number | null, sender?: string | null): RequestResult;
  /** Keys the payload serves, in slot order. */
  serves(): string[];
  readonly handlesRequests: boolean;
  /** The device's node id (16 hex digits). */
  readonly nodeId: string;
  free(): void;
}

/** A unit as a recipe run reads it: allocation's view plus its node id. */
export type RunUnit = AllocUnit & { node: string };

/** An input to a `RecipeRun`. */
export type RunInput =
  | { kind: "operator"; command: string }
  | ({ kind: "reply"; id: number } & RequestResult)
  | { kind: "report"; key: string; data: Uint8Array }
  | { kind: "lost" | "found"; unit: string }
  | { kind: "tick" };

/** What a `RecipeRun` asks for or says. */
export type RunEffect =
  | { kind: "send"; id: number; unit: string; key: string; body: Uint8Array; timeoutMs: number; command: string; step: string }
  | { kind: "trace"; t: number; type: string; text: string };

export type StepStatus = "inactive" | "active" | "started" | "done";

/** recipe-core's engine over keyed requests (protocol-wasm `RecipeRun`). */
export interface RecipeRunHandle {
  on(nowMs: number, input: RunInput): RunEffect[];
  readonly state: string;
  steps(): { id: string; status: StepStatus }[];
  phases(): { unit: string; em: string; phase: string; state: string | null; values: number[] }[];
  bindings(): { step: string; unit: string; em: string; phase: string }[];
  nextDeadline(): number | undefined;
  free(): void;
}

export interface RunConfig {
  ackTimeoutMs?: number;
  settleTimeoutMs?: number;
  maxCommands?: number;
}

/** One GPIO of a board, from protocol-wasm `boardPins` (board-pins). */
export interface GpioInfo {
  pin: number;
  /** Why the board keeps it from payloads, when it does. */
  reserved?: string;
  inputOnly: boolean;
  /** ADC channel. */
  adc?: number;
  /** The PWM output (ABI timer and channel) that drives the pin. */
  pwm?: { timer: number; channel: number };
  /** The board-wired I2C line. */
  i2c?: { bus: number; line: "sda" | "scl" };
  /** RP2040/RP2350 F1 SPI signal. */
  spi?: { block: number; signal: "sck" | "mosi" | "miso" };
}

/** A board's pin map: what the pin table offers and checks against. */
export interface BoardPins {
  chip: string;
  spiRouting: "matrix" | "rp-function-table" | "fixed" | "none";
  gpios: GpioInfo[];
  i2c: { bus: number; sda: number; scl: number }[];
  pwm: { timer: number; channel: number; pin: number }[];
  adc: number[];
  i2cBuses: number;
  spiBuses: number;
  i2cHz: [number, number];
  maxSpiKhz: number;
  maxLabel: number;
  funcs: string[];
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
  encodeDeviceInfo(info: Omit<DeviceInfo, "statusName" | "caps" | "capNames" | "serves"> & { caps?: number }): Uint8Array;
  decodeBoardInfo(data: Uint8Array): BoardInfo | undefined;
  decodeManifest(data: Uint8Array): Manifest | undefined;
  /** One page of `sys/manifest/cm` (ABI 8); undefined when malformed. */
  decodeControlPage(data: Uint8Array): ControlPage | undefined;
  /** The request body for page `page` of `sys/manifest/cm`. */
  controlPageRequest(page: number): Uint8Array;
  /** The browser simulator's `sys/manifest` body with `node` (16 hex digits)
   * as its node id; undefined without the simulator or for a bad node. */
  simulatorManifest(node: string): Uint8Array | undefined;
  /** The channels a payload declares (empty without a section); throws for a malformed one. */
  readChannels(wasm: Uint8Array): ChannelDecl[];
  /** A `mw.channels` section body for these declarations. */
  encodeChannels(decls: readonly ChannelDecl[]): Uint8Array;
  /** `wasm` with a `mw.channels` section appended. */
  withChannels(wasm: Uint8Array, section: Uint8Array): Uint8Array;
  /** Encodes a binding record; throws for what a device would refuse. */
  encodeBinding(b: BindingRecord): Uint8Array;
  decodeBinding(record: Uint8Array): BindingRecord | undefined;
  /** What a binding signature covers, for the device with node id `node`. */
  bindingMessage(record: Uint8Array, node: string): Uint8Array;
  /** A `sys/payload/bind` body, signed when a signature is given. */
  encodeBindBody(record: Uint8Array, signature?: Uint8Array): Uint8Array;
  /** Issues of a binding for a payload on a node (its `sys/manifest`). */
  /** With `peers` (the board's `sys/peers`), a remote target naming
   * another node is an issue too. */
  checkBinding(manifest: Uint8Array, wasm: Uint8Array, record: Uint8Array, peers?: readonly string[]): BindingIssue[];
  /** The node ids of a `sys/peers` reply; undefined when malformed. */
  decodePeers(data: Uint8Array): string[] | undefined;
  /** The key that reaches `node`'s `sys/<what>` through the board. */
  peerKey(node: string, what: "info" | "manifest" | "board"): string;
  payloadStatusName(b: number): string | undefined;
  newDeviceLoader(capacity: number): DeviceLoader;
  readonly constants: ProtocolConstants;
  readonly abi: AbiInfo;
  readonly hasSimulator: boolean;
  /** The pin map of a `sys/info` chip name, or of the board `board` names
   * (its manifest's BOARD record, for a Linux board); throws for an unknown
   * pair. */
  boardPins(chip: string, board?: string): BoardPins;
  /** Checks a pin table on a chip (board-pins rules + the ABI's bus
   * argument rules); `board` as for `boardPins`; throws for an unknown
   * chip. */
  checkPins(chip: string, rows: readonly PinRow[], buses: readonly BusSetting[], board?: string): PinIssue[];
  /** A Zenoh session over a WebSocket with Zenoh id `zid` (1–16 bytes,
   * not all zero); see `device/mesh.ts`. */
  newMeshSession(zid: Uint8Array): MeshSessionLike;
  /** Checks state machines against the pin table's labels and functions
   * (the `state-table` crate's rules); a `MachineChecker`. */
  checkMachines(machines: readonly Machine[], pins: readonly { label: string; func: PinFunc }[]): MachineIssue[];
  /** Checks flows against the machines and the pin table (the
   * `state-table` crate's `flow` rules); a `FlowChecker`. */
  checkFlows(flows: readonly Flow[], machines: readonly Machine[], pins: readonly { label: string; func: PinFunc }[], phases?: readonly PhaseDecl[]): FlowIssue[];
  /** Checks phase classes and a master recipe (recipe-core's rules). */
  checkRecipe(recipe: MasterRecipe, classes: readonly PhaseClass[]): RecipeIssue[];
  /** Binds a recipe's steps to units' equipment phases (plant.ts
   * `allocationUnits`), or says per unit why not. */
  allocateRecipe(recipe: MasterRecipe, classes: readonly PhaseClass[], units: readonly AllocUnit[]): Allocation;
  /** Allocates and runs a recipe against simulated units until `untilMs`. */
  simulateRecipe(recipe: MasterRecipe, classes: readonly PhaseClass[], units: readonly AllocUnit[], script: readonly ScriptEvent[], untilMs: number): SimulatedRun;
  /** The S88 phase state model: states, commands, and next[state][command..., Done]. */
  phaseModel(): { states: string[]; commands: string[]; next: (string | null)[][] };
  /** Loads a payload into the simulator on a device with node id `node`
   * (16 hex digits; default 0000000000000001), its channels resolved
   * against the encoded `binding` when given; throws on rejection, an
   * unresolved binding or an init trap. */
  newSimulator(wasm: Uint8Array, onEvent: (e: SimEvent) => void, node?: string, binding?: Uint8Array): Simulator;
  /** Compiles, allocates and builds a control recipe run; throws with the
   * allocation failure's message. */
  newRecipeRun(recipe: MasterRecipe, classes: readonly PhaseClass[], units: readonly RunUnit[], config?: RunConfig): RecipeRunHandle;
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
    decodeBoardInfo: (data) => pkg.decodeBoardInfo(data) as BoardInfo | undefined,
    decodeManifest: (data) => pkg.decodeManifest(data) as Manifest | undefined,
    decodeControlPage: (data) => pkg.decodeControlPage(data) as ControlPage | undefined,
    controlPageRequest: (page) => pkg.controlPageRequest(page),
    simulatorManifest: (node) => pkg.simulatorManifest?.(node) ?? undefined,
    readChannels: (wasm) => pkg.readChannels(wasm) as ChannelDecl[],
    encodeChannels: (decls) => pkg.encodeChannels([...decls]),
    withChannels: (wasm, section) => pkg.withChannels(wasm, section),
    encodeBinding: (b) => pkg.encodeBinding(b),
    decodeBinding: (record) => pkg.decodeBinding(record) as BindingRecord | undefined,
    bindingMessage: (record, node) => pkg.bindingMessage(record, node),
    encodeBindBody: (record, signature) => pkg.encodeBindBody(record, signature),
    checkBinding: (manifest, wasm, record, peers) =>
      pkg.checkBinding(manifest, wasm, record, peers ? [...peers] : undefined) as BindingIssue[],
    decodePeers: (data) => pkg.decodePeers(data) ?? undefined,
    peerKey: (node, what) => pkg.peerKey(node, what),
    encodeDeviceInfo: (i) =>
      pkg.encodeDeviceInfo(i.abiVersion, i.capacity, i.status, i.chip, i.firmware, i.node, i.caps),
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
    boardPins: (chip, board) => pkg.boardPins(chip, board) as BoardPins,
    checkPins: (chip, rows, buses, board) => pkg.checkPins(chip, [...rows], [...buses], board) as PinIssue[],
    newMeshSession: (zid) => new pkg.MeshSession(zid) as unknown as MeshSessionLike,
    checkMachines: (machines, pins) => pkg.checkMachines([...machines], [...pins]) as MachineIssue[],
    checkFlows: (flows, machines, pins, phases) => pkg.checkFlows([...flows], [...machines], [...pins], phases ? [...phases] : undefined) as FlowIssue[],
    checkRecipe: (recipe, classes) => pkg.checkRecipe(recipe, [...classes]) as RecipeIssue[],
    allocateRecipe: (recipe, classes, units) => pkg.allocateRecipe(recipe, [...classes], [...units]) as Allocation,
    simulateRecipe: (recipe, classes, units, script, untilMs) => pkg.simulateRecipe(recipe, [...classes], [...units], [...script], untilMs) as SimulatedRun,
    phaseModel: () => pkg.phaseModel() as { states: string[]; commands: string[]; next: (string | null)[][] },
    newSimulator: (wasm, onEvent, node, binding) => {
      // `Simulator` exists only when protocol-wasm was built with the
      // `simulator` feature (the default).
      const Ctor = (
        pkg as unknown as { Simulator?: new (w: Uint8Array, f: (e: unknown) => void, node?: string, binding?: Uint8Array) => Simulator }
      ).Simulator;
      if (!hasSimulator || !Ctor) throw new Error("protocol-wasm was built without the simulator feature");
      return new Ctor(wasm, (e) => onEvent(e as SimEvent), node, binding);
    },
    newRecipeRun: (recipe, classes, units, config) =>
      new pkg.RecipeRun(recipe, [...classes], [...units], config ?? {}) as unknown as RecipeRunHandle,
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
