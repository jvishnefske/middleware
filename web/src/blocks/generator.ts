// AssemblyScript generator for Blocks workspaces (pure: workspace JSON in,
// source text out; no Blockly, no I/O).
//
// The output is one module for the "mw" payload ABI (src/wasm_abi.rs,
// restated for AssemblyScript in payloads/templates/README.md):
//
// * `@external("mw", ...)` declarations for only the host functions the
//   blocks use, in ABI order;
// * `memory`, which asc exports by itself;
// * `init` (subscriptions, pin interrupts and `set_tick` first, then the
//   `on init` stacks), and `tick`, `mw_alloc` + `on_message` and `on_irq`
//   only when an event block needs them.
//
// Nothing allocates at run time. Constant text is UTF-8 in data segments
// (`memory.data<u8>([...])`), and messages land in a fixed 512-byte
// receive buffer; text being built for `publish`, `log` and `show text`
// goes into a fixed 512-byte transmit buffer. Every value is an `i32` (no
// floating point; booleans are 0 and 1, division by zero gives 0), and
// loops have bounds known when the code is generated: `repeat N` and `for`
// with constant numbers. Each call into a payload also runs on a fuel
// budget, so even a long bounded loop traps instead of hanging the device.
//
// Several `every` blocks share one tick of their greatest common divisor
// and each keeps its own countdown. Several handlers on one pin share one
// interrupt (the union of their edges) and filter on the pin level.
//
// Problems are reported per block (`GenIssue.blockId`) so the UI can point
// at the block. Disabled blocks are skipped; a block the profile does not
// support is an error rather than being dropped silently.

import { EVENT_BLOCKS, blockSupported } from "./defs";
import { PROFILES, type ProfileId } from "./profiles";
import {
  type BlockJson,
  type WorkspaceJson,
  fieldNumber,
  fieldText,
  input,
  isDisabled,
  nextBlock,
  topBlocks,
  variableName,
} from "./workspace";

export interface GenIssue {
  blockId?: string;
  message: string;
}

export interface GenResult {
  /** The AssemblyScript module; present even with errors, for display. */
  source: string;
  errors: GenIssue[];
  warnings: GenIssue[];
  /** Host functions imported, in ABI order. */
  imports: string[];
  /** Exports besides `memory`. */
  exports: string[];
}

/** Largest message or publish (wasm_abi::MAX_MESSAGE). */
export const MAX_MESSAGE = 512;
/** Largest `badge/text` payload. */
export const MAX_BADGE_TEXT = 256;
/** Shortest tick (wasm_abi::MIN_TICK_MS). */
export const MIN_TICK_MS = 10;
/** Most iterations one generated loop may run. */
export const MAX_LOOP = 10_000;
/** Highest pin number the GPIO blocks accept. */
export const MAX_PIN = 48;

/** Host imports: AssemblyScript signature after the name, in ABI order. */
const IMPORTS: readonly [string, string][] = [
  ["log", "(ptr: usize, len: i32): void"],
  ["publish", "(keyPtr: usize, keyLen: i32, dataPtr: usize, dataLen: i32): i32"],
  ["subscribe", "(keyPtr: usize, keyLen: i32): i32"],
  ["now_ms", "(): i32"],
  ["set_tick", "(periodMs: i32): i32"],
  ["gpio_output", "(pin: i32): i32"],
  ["gpio_input", "(pin: i32, pull: i32): i32"],
  ["gpio_write", "(pin: i32, level: i32): i32"],
  ["gpio_read", "(pin: i32): i32"],
  ["gpio_irq", "(pin: i32, edge: i32): i32"],
  ["timer_start", "(id: i32, periodUs: i32, periodic: i32): i32"],
  ["timer_stop", "(id: i32): i32"],
];

/** Run-time helpers, emitted when used. Order is emission order. */
const HELPERS: readonly [string, string][] = [
  [
    "txBytes",
    `// Appends len bytes at ptr to TX, up to its capacity.
function txBytes(ptr: usize, len: i32): void {
  const room = TX_CAP - txLen;
  const n = len < room ? len : room;
  for (let i = 0; i < n; i++) store<u8>(TX + <usize>(txLen + i), load<u8>(ptr + <usize>i));
  txLen += n > 0 ? n : 0;
}`,
  ],
  [
    "txNum",
    `// Appends n in decimal to TX.
function txNum(n: i32): void {
  let u: u32 = n < 0 ? <u32>(0 - n) : <u32>n;
  let p: i32 = 11;
  do {
    p--;
    store<u8>(DIGITS + <usize>p, <u8>(0x30 + u % 10));
    u /= 10;
  } while (u != 0);
  if (n < 0) {
    p--;
    store<u8>(DIGITS + <usize>p, 0x2d);
  }
  txBytes(DIGITS + <usize>p, 11 - p);
}`,
  ],
  [
    "parseNum",
    `// Reads a decimal number (optional leading '-') at the start of ptr..ptr+len.
function parseNum(ptr: usize, len: i32): i32 {
  let i: i32 = 0;
  let neg = false;
  if (len > 0 && load<u8>(ptr) == 0x2d) {
    neg = true;
    i = 1;
  }
  let v: i32 = 0;
  for (; i < len; i++) {
    const c = <i32>load<u8>(ptr + <usize>i) - 0x30;
    if (c < 0 || c > 9) break;
    v = v * 10 + c;
  }
  return neg ? 0 - v : v;
}`,
  ],
  [
    "div",
    `// Integer division; dividing by zero gives 0 instead of trapping.
function div(a: i32, b: i32): i32 {
  if (b == 0) return 0;
  if (b == -1) return 0 - a;
  return a / b;
}`,
  ],
  [
    "rem",
    `// Remainder with the sign of the divisor (Blockly's modulo); 0 for a zero divisor.
function rem(a: i32, b: i32): i32 {
  if (b == 0 || b == -1) return 0;
  const r = a % b;
  return r != 0 && (r < 0) != (b < 0) ? r + b : r;
}`,
  ],
  [
    "ipow",
    `// Integer power by squaring (at most 32 rounds); negative exponents give 0.
function ipow(b: i32, e: i32): i32 {
  if (e < 0) return b == 1 ? 1 : b == -1 ? ((e & 1) != 0 ? -1 : 1) : 0;
  let r: i32 = 1;
  for (let k = 0; k < 32 && e != 0; k++) {
    if ((e & 1) != 0) r *= b;
    b *= b;
    e >>>= 1;
  }
  return r;
}`,
  ],
  [
    "clamp",
    `function clamp(v: i32, lo: i32, hi: i32): i32 {
  return v < lo ? lo : v > hi ? hi : v;
}`,
  ],
  [
    "random",
    `// xorshift32 with a fixed seed: the same sequence on every run.
let seed: u32 = 2463534242;
function random(lo: i32, hi: i32): i32 {
  seed ^= seed << 13;
  seed ^= seed >> 17;
  seed ^= seed << 5;
  if (hi < lo) {
    const t = lo;
    lo = hi;
    hi = t;
  }
  const span = <u32>(hi - lo) + 1;
  return span == 0 ? <i32>seed : lo + <i32>(seed % span);
}`,
  ],
  [
    "pinWrite",
    `// Drives an output and remembers the level for toggle.
function pinWrite(pin: i32, level: i32): void {
  const v: u8 = level != 0 ? 1 : 0;
  store<u8>(PINS + <usize>pin, v);
  mw_gpio_write(pin, v);
}`,
  ],
  [
    "pinToggle",
    `function pinToggle(pin: i32): void {
  pinWrite(pin, load<u8>(PINS + <usize>pin) ^ 1);
}`,
  ],
  [
    "ledSet",
    `// Sets LED i (taken modulo 8) in the LED buffer, channels clamped to 0..255.
function ledSet(i: i32, r: i32, g: i32, b: i32): void {
  const at = LEDS + <usize>((((i % 8) + 8) % 8) * 3);
  store<u8>(at, <u8>clamp(r, 0, 255));
  store<u8>(at + 1, <u8>clamp(g, 0, 255));
  store<u8>(at + 2, <u8>clamp(b, 0, 255));
}`,
  ],
  [
    "ledFill",
    `function ledFill(r: i32, g: i32, b: i32): void {
  for (let i = 0; i < 8; i++) ledSet(i, r, g, b);
}`,
  ],
  [
    "textCap",
    `// TX length cut to at most max bytes on a UTF-8 character boundary.
function textCap(max: i32): i32 {
  if (txLen <= max) return txLen;
  let n = max;
  while (n > 0 && (load<u8>(TX + <usize>n) & 0xc0) == 0x80) n--;
  return n;
}`,
  ],
];

/** Helpers that need other helpers or buffers. */
const HELPER_NEEDS: Record<string, { helpers?: string[]; buffers?: string[]; imports?: string[] }> = {
  txBytes: { buffers: ["TX"] },
  txNum: { helpers: ["txBytes"], buffers: ["DIGITS"] },
  pinWrite: { buffers: ["PINS"], imports: ["gpio_write"] },
  pinToggle: { helpers: ["pinWrite"] },
  ledSet: { helpers: ["clamp"], buffers: ["LEDS"] },
  ledFill: { helpers: ["ledSet"] },
  textCap: { buffers: ["TX"] },
};

const BUFFERS: readonly [string, string][] = [
  ["RX", `// Receive buffer handed to the host by mw_alloc.\nconst RX: usize = memory.data(${MAX_MESSAGE});`],
  ["TX", `// Text being built for publish, log and show text.\nconst TX_CAP: i32 = ${MAX_MESSAGE};\nconst TX: usize = memory.data(${MAX_MESSAGE});\nlet txLen: i32 = 0;`],
  ["DIGITS", "const DIGITS: usize = memory.data(11);"],
  ["PINS", `// Last level written to each pin, for toggle.\nconst PINS: usize = memory.data(${MAX_PIN + 1});`],
  ["LEDS", "// 8 LEDs, R G B each, as published on badge/leds.\nconst LEDS: usize = memory.data(24);"],
];

const BADGE_LEDS = "badge/leds";
const BADGE_TEXT = "badge/text";
const BADGE_PET = "badge/pet";
const BADGE_BUTTONS = "badge/buttons";

type Ty = "number" | "text";

/** What the current handler provides to value blocks. */
type EventCtx = "init" | "tick" | "message" | "button" | "pin" | "timer";

interface Ctx {
  event: EventCtx;
  loops: number;
}

function gcd(a: number, b: number): number {
  for (let i = 0; i < 64 && b !== 0; i++) [a, b] = [b, a % b];
  return a;
}

/** A valid AssemblyScript identifier for a Blockly variable name. */
export function variableIdent(name: string): string {
  const base = name.replace(/[^A-Za-z0-9_]/g, "_");
  return `v_${base}`;
}

/** Checks a key the way the host would before the payload runs. */
export function keyProblem(key: string): string | null {
  if (key.length === 0) return "key is empty";
  if (new TextEncoder().encode(key).length > 128) return "key is longer than 128 bytes";
  if (/\s/.test(key)) return "key contains whitespace";
  if (key.startsWith("/") || key.endsWith("/") || key.includes("//")) return "key has an empty segment";
  if (key === "sys" || key.startsWith("sys/")) return "keys under sys/ belong to the firmware";
  return null;
}

class Gen {
  readonly errors: GenIssue[] = [];
  readonly warnings: GenIssue[] = [];
  private readonly imports = new Set<string>();
  private readonly helpers = new Set<string>();
  private readonly buffers = new Set<string>();
  /** Constant UTF-8 byte strings by content -> identifier. */
  private readonly strings = new Map<string, string>();
  private readonly variables = new Map<string, string>();
  /** Subscription keys -> slot global. */
  private readonly slots = new Map<string, string>();
  private loopVars = 0;

  constructor(
    private readonly ws: WorkspaceJson,
    private readonly profile: ProfileId,
  ) {}

  error(b: BlockJson | undefined, message: string): void {
    this.errors.push({ blockId: b?.id, message });
  }

  warn(b: BlockJson | undefined, message: string): void {
    this.warnings.push({ blockId: b?.id, message });
  }

  use(name: string): void {
    this.imports.add(name);
  }

  helper(name: string): void {
    if (this.helpers.has(name)) return;
    this.helpers.add(name);
    const needs = HELPER_NEEDS[name];
    for (const h of needs?.helpers ?? []) this.helper(h);
    for (const b of needs?.buffers ?? []) this.buffers.add(b);
    for (const i of needs?.imports ?? []) this.use(i);
  }

  buffer(name: string): void {
    this.buffers.add(name);
  }

  /** Identifier of a constant byte string; `${id}_LEN` is its length. */
  str(text: string): string {
    let id = this.strings.get(text);
    if (!id) {
      id = `S${this.strings.size}`;
      this.strings.set(text, id);
    }
    return id;
  }

  slot(key: string): string {
    let id = this.slots.get(key);
    if (!id) {
      id = `slot${this.slots.size}`;
      this.slots.set(key, id);
    }
    return id;
  }

  variable(b: BlockJson): string | null {
    const name = variableName(this.ws, b);
    if (name === null) {
      this.error(b, "block has no variable");
      return null;
    }
    let id = this.variables.get(name);
    if (!id) {
      id = variableIdent(name);
      // Different Blockly names may sanitise to one identifier.
      const taken = new Set(this.variables.values());
      for (let i = 2; taken.has(id); i++) id = `${variableIdent(name)}_${i}`;
      this.variables.set(name, id);
    }
    return id;
  }

  /** Checks that the profile supports a block; returns false (with an
   * error) if not. */
  supported(b: BlockJson): boolean {
    if (blockSupported(b.type, this.profile)) return true;
    if (b.type === "controls_whileUntil") this.error(b, "while/until loops are unbounded; use repeat or for with constant bounds");
    else this.error(b, `block "${b.type}" is not available on the ${this.profile} profile`);
    return false;
  }

  /** An integer constant from a number field. */
  intField(b: BlockJson, name: string, lo: number, hi: number, what: string): number {
    const n = fieldNumber(b, name);
    if (!Number.isInteger(n) || n < lo || n > hi) {
      this.error(b, `${what} must be a whole number from ${lo} to ${hi}`);
      return lo;
    }
    return n;
  }

  /** A compile-time constant: a number block, possibly negated. */
  constant(b: BlockJson | undefined): number | null {
    if (!b || isDisabled(b)) return null;
    if (b.type === "math_number") {
      const n = fieldNumber(b, "NUM");
      return Number.isInteger(n) ? n : null;
    }
    if (b.type === "math_single" && fieldText(b, "OP") === "NEG") {
      const c = this.constant(input(b, "NUM"));
      return c === null ? null : -c;
    }
    return null;
  }

  // ---- expressions

  typeOf(b: BlockJson | undefined): Ty {
    if (!b || isDisabled(b)) return "number";
    return b.type === "text" || b.type === "text_join" || b.type === "mw_message_text" ? "text" : "number";
  }

  /** An i32 expression. A missing or disabled block is 0. */
  num(b: BlockJson | undefined, ctx: Ctx): string {
    if (!b || isDisabled(b)) return "0";
    if (!this.supported(b)) return "0";
    const A = (name: string) => this.num(input(b, name), ctx);
    switch (b.type) {
      case "math_number": {
        const n = fieldNumber(b, "NUM");
        if (!Number.isInteger(n)) {
          this.error(b, "numbers must be whole (there is no floating point on the device)");
          return "0";
        }
        if (n < -2147483648 || n > 2147483647) {
          this.error(b, "number does not fit in 32 bits");
          return "0";
        }
        return n < 0 ? `(${n})` : String(n);
      }
      case "logic_boolean":
        return fieldText(b, "BOOL") === "TRUE" ? "1" : "0";
      case "logic_compare": {
        const ops: Record<string, string> = { EQ: "==", NEQ: "!=", LT: "<", LTE: "<=", GT: ">", GTE: ">=" };
        if (this.typeOf(input(b, "A")) === "text" || this.typeOf(input(b, "B")) === "text") {
          this.error(b, "comparing text is not supported; compare numbers");
          return "0";
        }
        return `<i32>(${A("A")} ${ops[fieldText(b, "OP")] ?? "=="} ${A("B")})`;
      }
      case "logic_operation":
        return `<i32>(${A("A")} != 0 ${fieldText(b, "OP") === "OR" ? "||" : "&&"} ${A("B")} != 0)`;
      case "logic_negate":
        return `<i32>(${A("BOOL")} == 0)`;
      case "logic_ternary":
        if (this.typeOf(input(b, "THEN")) === "text" || this.typeOf(input(b, "ELSE")) === "text") {
          this.error(b, "the ternary block works on numbers only");
          return "0";
        }
        return `(${A("IF")} != 0 ? ${A("THEN")} : ${A("ELSE")})`;
      case "math_arithmetic": {
        const a = A("A");
        const c = A("B");
        switch (fieldText(b, "OP")) {
          case "ADD":
            return `(${a} + ${c})`;
          case "MINUS":
            return `(${a} - ${c})`;
          case "MULTIPLY":
            return `(${a} * ${c})`;
          case "DIVIDE":
            this.helper("div");
            return `div(${a}, ${c})`;
          case "POWER":
            this.helper("ipow");
            return `ipow(${a}, ${c})`;
          default:
            this.error(b, `unknown operator ${fieldText(b, "OP")}`);
            return "0";
        }
      }
      case "math_single": {
        const x = A("NUM");
        switch (fieldText(b, "OP")) {
          case "NEG":
            return `(0 - ${x})`;
          case "ABS":
            return `abs<i32>(${x})`;
          case "POW10":
            this.helper("ipow");
            return `ipow(10, ${x})`;
          default:
            this.error(b, `${fieldText(b, "OP")} needs floating point; use abs, negate or 10^ instead`);
            return "0";
        }
      }
      case "math_modulo":
        this.helper("rem");
        return `rem(${A("DIVIDEND")}, ${A("DIVISOR")})`;
      case "math_constrain":
        this.helper("clamp");
        return `clamp(${A("VALUE")}, ${A("LOW")}, ${A("HIGH")})`;
      case "math_random_int":
        this.helper("random");
        return `random(${A("FROM")}, ${A("TO")})`;
      case "math_number_property": {
        const x = A("NUMBER_TO_CHECK");
        switch (fieldText(b, "PROPERTY")) {
          case "EVEN":
            return `<i32>((${x} & 1) == 0)`;
          case "ODD":
            return `<i32>((${x} & 1) != 0)`;
          case "POSITIVE":
            return `<i32>(${x} > 0)`;
          case "NEGATIVE":
            return `<i32>(${x} < 0)`;
          case "WHOLE":
            return "1";
          case "DIVISIBLE_BY":
            this.helper("rem");
            return `<i32>(rem(${x}, ${A("DIVISOR")}) == 0)`;
          default:
            this.error(b, `${fieldText(b, "PROPERTY")} is not supported`);
            return "0";
        }
      }
      case "variables_get":
        return this.variable(b) ?? "0";
      case "mw_now_ms":
        this.use("now_ms");
        return "mw_now_ms()";
      case "mw_message_number":
        if (ctx.event !== "message") {
          this.error(b, "message number is only available inside on message");
          return "0";
        }
        this.helper("parseNum");
        return "parseNum(msgPtr, msgLen)";
      case "mw_gpio_read": {
        const p = this.intField(b, "PIN", 0, MAX_PIN, "pin");
        this.use("gpio_read");
        return `mw_gpio_read(${p})`;
      }
      case "mw_pin_level":
        if (ctx.event !== "pin") {
          this.error(b, "pin level is only available inside on pin edge");
          return "0";
        }
        return "irqValue";
      case "badge_button":
      case "badge_pressed":
        if (ctx.event !== "button") {
          this.error(b, `${b.type === "badge_button" ? "button number" : "button pressed"} is only available inside on button`);
          return "0";
        }
        return b.type === "badge_button" ? "<i32>load<u8>(msgPtr)" : "<i32>load<u8>(msgPtr + 1)";
      case "text":
      case "text_join":
      case "mw_message_text":
        this.error(b, "text cannot be used as a number");
        return "0";
      default:
        this.error(b, `block "${b.type}" is not supported by the generator`);
        return "0";
    }
  }

  /** Statements appending a value to TX (numbers as decimal). */
  text(b: BlockJson | undefined, ctx: Ctx, pad: string, out: string[]): void {
    if (!b || isDisabled(b)) return;
    if (!this.supported(b)) return;
    if (this.typeOf(b) === "number") {
      this.helper("txNum");
      out.push(`${pad}txNum(${this.num(b, ctx)});`);
      return;
    }
    switch (b.type) {
      case "text": {
        const s = fieldText(b, "TEXT");
        if (s === "") return;
        const id = this.str(s);
        this.helper("txBytes");
        out.push(`${pad}txBytes(${id}, ${id}_LEN);`);
        return;
      }
      case "text_join": {
        const count = Number((b.extraState as { itemCount?: unknown } | undefined)?.itemCount ?? 2);
        for (let i = 0; i < Math.min(count, 64); i++) this.text(input(b, `ADD${i}`), ctx, pad, out);
        return;
      }
      case "mw_message_text":
        if (ctx.event !== "message") {
          this.error(b, "message text is only available inside on message");
          return;
        }
        this.helper("txBytes");
        out.push(`${pad}txBytes(msgPtr, msgLen);`);
        return;
      default:
        this.error(b, `block "${b.type}" is not supported by the generator`);
    }
  }

  /** Builds a value into TX: resets it, then appends. */
  buildText(b: BlockJson | undefined, ctx: Ctx, pad: string, out: string[]): void {
    this.buffer("TX");
    out.push(`${pad}txLen = 0;`);
    this.text(b, ctx, pad, out);
  }

  publishConst(key: string, b: BlockJson): string {
    const p = keyProblem(key);
    if (p) this.error(b, p);
    this.use("publish");
    return this.str(key);
  }

  // ---- statements

  stmts(first: BlockJson | undefined, ctx: Ctx, pad: string, out: string[]): void {
    for (let b = first; b; b = nextBlock(b)) {
      if (isDisabled(b)) continue;
      if (!this.supported(b)) continue;
      this.stmt(b, ctx, pad, out);
    }
  }

  private stmt(b: BlockJson, ctx: Ctx, pad: string, out: string[]): void {
    const inner = `${pad}  `;
    switch (b.type) {
      case "controls_if": {
        const st = (b.extraState ?? {}) as { elseIfCount?: number; hasElse?: boolean };
        const branches = 1 + Math.min(Number(st.elseIfCount ?? 0), 64);
        for (let i = 0; i < branches; i++) {
          out.push(`${pad}${i === 0 ? "if" : "} else if"} (${this.num(input(b, `IF${i}`), ctx)} != 0) {`);
          this.stmts(input(b, `DO${i}`), ctx, inner, out);
        }
        if (st.hasElse || b.inputs?.ELSE) {
          out.push(`${pad}} else {`);
          this.stmts(input(b, "ELSE"), ctx, inner, out);
        }
        out.push(`${pad}}`);
        return;
      }
      case "controls_repeat":
      case "controls_repeat_ext": {
        const n = b.type === "controls_repeat" ? fieldNumber(b, "TIMES") : this.constant(input(b, "TIMES"));
        if (n === null || !Number.isInteger(n)) {
          this.error(b, "repeat needs a constant whole number of times");
          return;
        }
        if (n > MAX_LOOP) this.error(b, `repeat at most ${MAX_LOOP} times`);
        const i = `i${this.loopVars++}`;
        out.push(`${pad}for (let ${i}: i32 = 0; ${i} < ${Math.max(0, Math.min(n, MAX_LOOP))}; ${i}++) {`);
        this.stmts(input(b, "DO"), { ...ctx, loops: ctx.loops + 1 }, inner, out);
        out.push(`${pad}}`);
        return;
      }
      case "controls_for": {
        const v = this.variable(b);
        const from = this.constant(input(b, "FROM"));
        const to = this.constant(input(b, "TO"));
        const by = this.constant(input(b, "BY")) ?? 1;
        if (v === null) return;
        if (from === null || to === null) {
          this.error(b, "for needs constant whole numbers for from and to");
          return;
        }
        // Blockly counts toward `to` by |by| whatever the sign of `by`.
        const step = Math.abs(by) || 1;
        const count = Math.floor(Math.abs(to - from) / step) + 1;
        if (count > MAX_LOOP) {
          this.error(b, `for would run ${count} times; at most ${MAX_LOOP}`);
          return;
        }
        const signed = from <= to ? step : -step;
        const i = `i${this.loopVars++}`;
        out.push(`${pad}for (let ${i}: i32 = 0; ${i} < ${count}; ${i}++) {`);
        out.push(`${inner}${v} = ${from < 0 ? `(${from})` : from} + ${i} * ${signed < 0 ? `(${signed})` : signed};`);
        this.stmts(input(b, "DO"), { ...ctx, loops: ctx.loops + 1 }, inner, out);
        out.push(`${pad}}`);
        return;
      }
      case "controls_flow_statements":
        if (ctx.loops === 0) {
          this.error(b, "break and continue must be inside a loop");
          return;
        }
        out.push(`${pad}${fieldText(b, "FLOW") === "CONTINUE" ? "continue" : "break"};`);
        return;
      case "variables_set": {
        const v = this.variable(b);
        if (this.typeOf(input(b, "VALUE")) === "text") {
          this.error(b, "variables hold numbers; text can only be published, logged or shown");
          return;
        }
        if (v) out.push(`${pad}${v} = ${this.num(input(b, "VALUE"), ctx)};`);
        return;
      }
      case "math_change": {
        const v = this.variable(b);
        if (v) out.push(`${pad}${v} += ${this.num(input(b, "DELTA"), ctx)};`);
        return;
      }
      case "text_print":
      case "mw_log":
        this.buildText(input(b, "TEXT"), ctx, pad, out);
        this.use("log");
        out.push(`${pad}mw_log(TX, txLen);`);
        return;
      case "mw_publish": {
        const key = fieldText(b, "KEY");
        const k = this.publishConst(key, b);
        if (this.profile === "badge" && key.startsWith("badge/")) {
          this.warn(b, `use the badge blocks for ${key}; a malformed payload there is refused`);
        }
        this.buildText(input(b, "DATA"), ctx, pad, out);
        out.push(`${pad}mw_publish(${k}, ${k}_LEN, TX, txLen);`);
        return;
      }
      case "mw_gpio_output": {
        const p = this.intField(b, "PIN", 0, MAX_PIN, "pin");
        this.use("gpio_output");
        out.push(`${pad}mw_gpio_output(${p});`);
        return;
      }
      case "mw_gpio_input": {
        const p = this.intField(b, "PIN", 0, MAX_PIN, "pin");
        const pl = this.intField(b, "PULL", 0, 2, "pull");
        this.use("gpio_input");
        out.push(`${pad}mw_gpio_input(${p}, ${pl});`);
        return;
      }
      case "mw_gpio_write": {
        const p = this.intField(b, "PIN", 0, MAX_PIN, "pin");
        this.helper("pinWrite");
        out.push(`${pad}pinWrite(${p}, ${this.num(input(b, "LEVEL"), ctx)});`);
        return;
      }
      case "mw_gpio_toggle": {
        const p = this.intField(b, "PIN", 0, MAX_PIN, "pin");
        this.helper("pinToggle");
        out.push(`${pad}pinToggle(${p});`);
        return;
      }
      case "mw_timer_start": {
        const id = this.intField(b, "ID", 0, 3, "timer");
        const ms = this.intField(b, "MS", 1, 2_000_000, "period");
        const periodic = this.intField(b, "MODE", 0, 1, "mode");
        this.use("timer_start");
        out.push(`${pad}mw_timer_start(${id}, ${ms * 1000}, ${periodic});`);
        return;
      }
      case "mw_timer_stop": {
        const id = this.intField(b, "ID", 0, 3, "timer");
        this.use("timer_stop");
        out.push(`${pad}mw_timer_stop(${id});`);
        return;
      }
      case "badge_led":
        this.helper("ledSet");
        out.push(
          `${pad}ledSet(${this.num(input(b, "INDEX"), ctx)}, ${this.num(input(b, "R"), ctx)}, ${this.num(input(b, "G"), ctx)}, ${this.num(input(b, "B"), ctx)});`,
        );
        return;
      case "badge_leds_fill":
        this.helper("ledFill");
        out.push(`${pad}ledFill(${this.num(input(b, "R"), ctx)}, ${this.num(input(b, "G"), ctx)}, ${this.num(input(b, "B"), ctx)});`);
        return;
      case "badge_set_leds": {
        const k = this.publishConst(BADGE_LEDS, b);
        this.buffer("LEDS");
        out.push(`${pad}mw_publish(${k}, ${k}_LEN, LEDS, 24);`);
        return;
      }
      case "badge_show_text": {
        const k = this.publishConst(BADGE_TEXT, b);
        this.buildText(input(b, "TEXT"), ctx, pad, out);
        this.helper("textCap");
        out.push(`${pad}mw_publish(${k}, ${k}_LEN, TX, textCap(${MAX_BADGE_TEXT}));`);
        return;
      }
      case "badge_pet_mode": {
        const mode = this.intField(b, "MODE", 0, 5, "pet mode");
        const k = this.publishConst(BADGE_PET, b);
        const m = this.str(String.fromCharCode(mode));
        out.push(`${pad}mw_publish(${k}, ${k}_LEN, ${m}, 1);`);
        return;
      }
      default:
        if (EVENT_BLOCKS.includes(b.type)) this.error(b, "event blocks cannot be nested");
        else this.error(b, `block "${b.type}" is not a statement the generator supports`);
    }
  }

  // ---- module

  run(): GenResult {
    const tops = topBlocks(this.ws).filter((b) => !isDisabled(b));
    const initBody: string[] = [];
    const setup: string[] = [];
    const every: { b: BlockJson; ms: number; body: string[] }[] = [];
    const messages: { key: string; body: string[] }[] = [];
    const buttons: { button: number; action: number; body: string[] }[] = [];
    const pins = new Map<number, { pull: number; edges: number; b: BlockJson }>();
    const pinBodies: { pin: number; edge: number; body: string[] }[] = [];
    const timers: { id: number; body: string[] }[] = [];

    for (const b of tops) {
      if (!EVENT_BLOCKS.includes(b.type)) {
        this.warn(b, "not inside an event block; ignored");
        continue;
      }
      if (!this.supported(b)) continue;
      const body: string[] = [];
      const ctx = (event: EventCtx): Ctx => ({ event, loops: 0 });
      switch (b.type) {
        case "mw_on_init":
          this.stmts(input(b, "DO"), ctx("init"), "  ", body);
          initBody.push(...body);
          break;
        case "mw_every": {
          const ms = this.intField(b, "MS", MIN_TICK_MS, 2_147_483_647, "period");
          this.stmts(input(b, "DO"), ctx("tick"), "    ", body);
          every.push({ b, ms, body });
          break;
        }
        case "mw_on_message": {
          const key = fieldText(b, "KEY");
          const p = keyProblem(key);
          if (p) this.error(b, p);
          this.slot(key);
          this.stmts(input(b, "DO"), ctx("message"), "    ", body);
          messages.push({ key, body });
          break;
        }
        case "badge_on_button":
          this.slot(BADGE_BUTTONS);
          this.stmts(input(b, "DO"), ctx("button"), "      ", body);
          buttons.push({
            button: this.intField(b, "BUTTON", 0, 5, "button"),
            action: this.intField(b, "ACTION", 0, 2, "action"),
            body,
          });
          break;
        case "mw_on_pin_edge": {
          const pin = this.intField(b, "PIN", 0, MAX_PIN, "pin");
          const pull = this.intField(b, "PULL", 0, 2, "pull");
          const edge = this.intField(b, "EDGE", 1, 3, "edge");
          const prev = pins.get(pin);
          if (prev && prev.pull !== pull) this.error(b, `pin ${pin} already has a handler with a different pull`);
          pins.set(pin, { pull, edges: (prev?.edges ?? 0) | edge, b });
          this.stmts(input(b, "DO"), ctx("pin"), "    ", body);
          pinBodies.push({ pin, edge, body });
          break;
        }
        case "mw_on_timer":
          this.stmts(input(b, "DO"), ctx("timer"), "    ", body);
          timers.push({ id: this.intField(b, "ID", 0, 3, "timer"), body });
          break;
      }
    }

    // init: subscriptions, pin interrupts, tick, then the on-init stacks.
    for (const [key, slot] of this.slots) {
      const k = this.str(key);
      this.use("subscribe");
      setup.push(`  ${slot} = mw_subscribe(${k}, ${k}_LEN);`);
    }
    for (const [pin, p] of pins) {
      this.use("gpio_input");
      this.use("gpio_irq");
      setup.push(`  mw_gpio_input(${pin}, ${p.pull});`, `  mw_gpio_irq(${pin}, ${p.edges});`);
    }
    const tickMs = every.reduce((g, e) => gcd(g, e.ms), 0);
    if (every.length > 0) {
      if (tickMs < MIN_TICK_MS) {
        this.error(every[0]?.b, `the every periods share a ${tickMs} ms tick; periods must have a common divisor of at least ${MIN_TICK_MS} ms`);
      }
      this.use("set_tick");
      setup.push(`  mw_set_tick(${tickMs});`);
    }

    const globals: string[] = [];
    const fns: string[] = [];
    const exportsOut: string[] = [];

    if (setup.length + initBody.length > 0) {
      fns.push(["export function init(): void {", ...setup, ...initBody, "}"].join("\n"));
      exportsOut.push("init");
    }

    if (every.length > 0) {
      const lines = ["export function tick(nowMs: i32): void {"];
      every.forEach((e, i) => {
        if (e.ms === tickMs) {
          lines.push("  {", ...e.body, "  }");
        } else {
          globals.push(`let every${i}: i32 = 0;`);
          lines.push(`  every${i} += ${tickMs};`, `  if (every${i} >= ${e.ms}) {`, `    every${i} = 0;`, ...e.body, "  }");
        }
      });
      lines.push("}");
      fns.push(lines.join("\n"));
      exportsOut.push("tick");
    }

    if (this.slots.size > 0) {
      this.buffer("RX");
      for (const slot of this.slots.values()) globals.push(`let ${slot}: i32 = -1;`);
      globals.push("// The message being handled.", "let msgPtr: usize = 0;", "let msgLen: i32 = 0;");
      fns.push(
        [
          "// Returns the receive buffer, or 0 to decline a message that does not fit.",
          "export function mw_alloc(len: i32): usize {",
          `  return len < 0 || len > ${MAX_MESSAGE} ? 0 : RX;`,
          "}",
        ].join("\n"),
      );
      const lines = ["export function on_message(slot: i32, ptr: usize, len: i32): void {", "  msgPtr = ptr;", "  msgLen = len;"];
      for (const m of messages) {
        lines.push(`  if (slot == ${this.slot(m.key)}) {`, ...m.body, "  }");
      }
      if (buttons.length > 0) {
        lines.push(`  if (slot == ${this.slot(BADGE_BUTTONS)} && len >= 2) {`);
        lines.push("    const button = <i32>load<u8>(ptr);", "    const pressed = <i32>load<u8>(ptr + 1);");
        for (const bt of buttons) {
          const conds = [bt.button === 0 ? "" : `button == ${bt.button}`, bt.action === 2 ? "" : `pressed == ${bt.action}`].filter(Boolean);
          lines.push(`    if (${conds.join(" && ") || "true"}) {`, ...bt.body, "    }");
        }
        lines.push("  }");
      }
      lines.push("}");
      fns.push(lines.join("\n"));
      exportsOut.push("mw_alloc", "on_message");
    }

    const needsIrq = pinBodies.length > 0 || timers.length > 0 || this.imports.has("timer_start") || this.imports.has("gpio_irq");
    if (needsIrq) {
      const lines = ["export function on_irq(kind: i32, source: i32, value: i32, timeUs: i32): void {"];
      if (pinBodies.length > 0) {
        globals.push("// The level after the edge being handled.", "let irqValue: i32 = 0;");
        lines.push("  irqValue = value;");
      }
      for (const p of pinBodies) {
        const level = p.edge === 1 ? " && value == 1" : p.edge === 2 ? " && value == 0" : "";
        lines.push(`  if (kind == 1 && source == ${p.pin}${level}) {`, ...p.body, "  }");
      }
      for (const t of timers) lines.push(`  if (kind == 2 && source == ${t.id}) {`, ...t.body, "  }");
      lines.push("}");
      fns.push(lines.join("\n"));
      exportsOut.push("on_irq");
      if (timers.length > 0 && !this.imports.has("timer_start")) {
        this.warn(undefined, "an on timer block has no start timer block, so it never runs");
      }
    }

    for (const id of this.variables.values()) globals.push(`let ${id}: i32 = 0;`);

    // Assemble: header, imports, buffers, constants, globals, helpers, exports.
    const layout = PROFILES[this.profile].memory;
    const parts: string[] = [
      [
        `// Generated by the middleware Blocks editor (profile ${this.profile}).`,
        "// Payload ABI: middleware src/wasm_abi.rs. Compile with",
        "// asc --runtime stub --use abort= -O --initialMemory 1 --maximumMemory 1",
        ...(layout.kind === "bytes" ? [`// then rewritten to ${layout.bytes} bytes of 1-byte pages (bytePages.ts).`] : []),
      ].join("\n"),
    ];
    const imports = IMPORTS.filter(([n]) => this.imports.has(n));
    if (imports.length > 0) {
      parts.push(imports.map(([n, sig]) => `@external("mw", "${n}")\ndeclare function mw_${n}${sig};`).join("\n"));
    }
    const buffers = BUFFERS.filter(([n]) => this.buffers.has(n)).map(([, src]) => src);
    if (buffers.length > 0) parts.push(buffers.join("\n"));
    if (this.strings.size > 0) {
      parts.push(
        [...this.strings]
          .map(([text, id]) => {
            const bytes = [...new TextEncoder().encode(text)];
            return `// ${JSON.stringify(text)}\nconst ${id}: usize = memory.data<u8>([${bytes.join(", ")}]);\nconst ${id}_LEN: i32 = ${bytes.length};`;
          })
          .join("\n"),
      );
    }
    if (globals.length > 0) parts.push(globals.join("\n"));
    const helpers = HELPERS.filter(([n]) => this.helpers.has(n)).map(([, src]) => src);
    parts.push(...helpers, ...fns);

    return {
      source: `${parts.join("\n\n")}\n`,
      errors: this.errors,
      warnings: this.warnings,
      imports: imports.map(([n]) => n),
      exports: exportsOut,
    };
  }
}

/** Generates the AssemblyScript module for a workspace. */
export function generate(ws: WorkspaceJson, profile: ProfileId): GenResult {
  return new Gen(ws, profile).run();
}
