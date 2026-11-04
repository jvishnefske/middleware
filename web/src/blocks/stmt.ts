// The statement IR every logic source lowers to before it becomes text.
// Blocks (generator.ts), state-machine rows and entries (machineGen.ts)
// and flows (flowGen.ts) all build `Stmt` trees and print them with one
// printer, so an `if`, a bounded loop, a pin write or a raise reads the
// same whichever editor made it.
//
// A `Stmt` is already lowered as far as expressions go: conditions and
// statement lines are AssemblyScript text, built by the source's own
// expression lowering (Blockly value blocks in generator.ts, the model's
// closed `Expr` grammar here in `lowerExpr`). What the IR fixes is the
// structure: a straight line, an `if` / `else if` / `else` chain on i32
// conditions (`cond != 0`), and a `for` loop with a constant count known
// when the code is generated. There is no other control flow, so every
// lowered handler is bounded.
//
// Shared lowering for the payload model's closed grammars lives here too:
// `lowerExpr` (the guard / branch `Expr`, depth at most `MAX_EXPR_DEPTH`;
// its recursion is bounded by that constant) and `lowerActions` (the
// `ActionRef` set: write / toggle a pin by label, publish, log, raise an
// event of a machine). A problem is reported through the caller's `fail`
// and that piece is left out (a bad expression reads as 0), so the output
// is always valid source.
//
// `printStmts` is iterative (an explicit work stack), so printing depth
// does not depend on how deeply a workspace nests its blocks.

import { type ActionRef, type Expr, MAX_ACTIONS, MAX_EXPR_DEPTH } from "../model/machines";
import type { PinFunc } from "../model/pins";

export type Stmt =
  /** One statement (or comment) line, without indentation. */
  | { kind: "line"; code: string }
  /** `if (c0 != 0) { ... } else if (c1 != 0) { ... } else { ... }`. */
  | { kind: "if"; arms: readonly { cond: string; body: readonly Stmt[] }[]; else: readonly Stmt[] | null }
  /** `for (let v: i32 = 0; v < count; v++) { ... }`, count constant. */
  | { kind: "for"; v: string; count: number; body: readonly Stmt[] };

export function line(code: string): Stmt {
  return { kind: "line", code };
}

/** Appends the statements' text to `out`, each line indented by `pad`
 * plus two spaces per nesting level. Iterative. */
export function printStmts(stmts: readonly Stmt[], pad: string, out: string[]): void {
  type Task = { text: string } | { list: readonly Stmt[]; pad: string };
  const stack: Task[] = [{ list: stmts, pad }];
  for (let t = stack.pop(); t; t = stack.pop()) {
    if ("text" in t) {
      out.push(t.text);
      continue;
    }
    const p = t.pad;
    const inner = `${p}  `;
    // Expand this list's statements in order: push their tasks reversed.
    const tasks: Task[] = [];
    for (const s of t.list) {
      switch (s.kind) {
        case "line":
          tasks.push({ text: `${p}${s.code}` });
          break;
        case "if":
          if (s.arms.length === 0) {
            if (s.else) tasks.push({ list: s.else, pad: p });
            break;
          }
          s.arms.forEach((a, i) => {
            tasks.push({ text: `${p}${i === 0 ? "if" : "} else if"} (${a.cond} != 0) {` }, { list: a.body, pad: inner });
          });
          if (s.else) tasks.push({ text: `${p}} else {` }, { list: s.else, pad: inner });
          tasks.push({ text: `${p}}` });
          break;
        case "for":
          tasks.push({ text: `${p}for (let ${s.v}: i32 = 0; ${s.v} < ${s.count}; ${s.v}++) {` }, { list: s.body, pad: inner }, { text: `${p}}` });
          break;
      }
    }
    for (let i = tasks.length - 1; i >= 0; i--) stack.push(tasks[i] as Task);
  }
}

// ---- statements every source emits the same way

/** Drives a labelled or numbered output, remembering the level. */
export function writeStmt(pin: string, level: string): Stmt {
  return line(`pinWrite(${pin}, ${level});`);
}
export function toggleStmt(pin: string): Stmt {
  return line(`pinToggle(${pin});`);
}

// ---- the model's closed grammars

/** What lowering the model's expressions and actions needs from the
 * module being generated (generator.ts implements it). */
export interface LowerHost {
  /** Identifier of a constant byte string (`${id}_LEN` its length). */
  str(text: string): string;
  /** Imports host function `mw_<name>`. */
  use(name: string): void;
  /** Emits a run-time helper (pinWrite, pinToggle). */
  helper(name: string): void;
  /** The `PIN_<label>` constant for a pin-table row of one of `funcs`, or
   * the problem. */
  pin(label: string, funcs: readonly PinFunc[], what: string): { ident: string } | { problem: string };
  /** Checks a publish key; the problem, or null. */
  keyProblem(key: string): string | null;
  /** `<i32>(sm{k} == SM{k}_<state>)` for machine `machine`'s state, or
   * the problem. */
  inState(machine: string, state: string): { code: string } | { problem: string };
  /** The bounded raise call for machine `machine`'s event, or the
   * problem. */
  raise(machine: string, event: string): { code: string } | { problem: string };
}

/** Largest message text (wasm_abi::MAX_MESSAGE). */
export const MAX_TEXT_BYTES = 512;

const CMP_AS: Readonly<Record<string, string>> = { eq: "==", ne: "!=", lt: "<", le: "<=", gt: ">", ge: ">=" };

/** Pointer and length arguments for constant text. */
export function textArgs(host: LowerHost, text: string): string {
  if (text === "") return "0, 0";
  const id = host.str(text);
  return `${id}, ${id}_LEN`;
}

/** `e` as an i32 expression; problems go to `fail` and read as 0. The
 * recursion is bounded by `MAX_EXPR_DEPTH`. */
export function lowerExpr(e: Expr, host: LowerHost, fail: (msg: string) => void, depth = 1): string {
  if (depth > MAX_EXPR_DEPTH) {
    fail(`the guard is nested deeper than ${MAX_EXPR_DEPTH} levels`);
    return "0";
  }
  const sub = (x: Expr) => lowerExpr(x, host, fail, depth + 1);
  switch (e.kind) {
    case "pin": {
      const p = host.pin(e.label, ["gpio-in", "gpio-out"], "a GPIO input or output");
      if ("problem" in p) {
        fail(p.problem);
        return "0";
      }
      host.use("gpio_read");
      return `mw_gpio_read(${p.ident})`;
    }
    case "const":
      if (!Number.isInteger(e.value) || e.value < -2147483648 || e.value > 2147483647) {
        fail(`${e.value} is not a whole 32-bit number`);
        return "0";
      }
      return e.value < 0 ? `(${e.value})` : String(e.value);
    case "state": {
      const s = host.inState(e.machine, e.state);
      if ("problem" in s) {
        fail(s.problem);
        return "0";
      }
      return s.code;
    }
    case "cmp": {
      const op = CMP_AS[e.op];
      if (!op) {
        fail(`unknown comparison "${String(e.op)}"`);
        return "0";
      }
      return `<i32>(${sub(e.a)} ${op} ${sub(e.b)})`;
    }
    case "and":
    case "or":
      return `<i32>(${sub(e.a)} != 0 ${e.kind === "and" ? "&&" : "||"} ${sub(e.b)} != 0)`;
    case "not":
      return `<i32>(${sub(e.a)} == 0)`;
    default:
      fail("unknown guard");
      return "0";
  }
}

/** One action as statements; problems go to `fail` and the action is left
 * out. `ownMachine` is the machine a raise without `machine` names (a
 * table's own); null where a raise must name its machine (flows). */
export function lowerAction(a: ActionRef, host: LowerHost, fail: (msg: string) => void, ownMachine: string | null): Stmt[] {
  switch (a.kind) {
    case "write":
    case "toggle": {
      const p = host.pin(a.label, ["gpio-out"], "a GPIO output");
      if ("problem" in p) {
        fail(p.problem);
        return [];
      }
      host.helper(a.kind === "write" ? "pinWrite" : "pinToggle");
      return [a.kind === "write" ? writeStmt(p.ident, a.level ? "1" : "0") : toggleStmt(p.ident)];
    }
    case "publish": {
      const kp = host.keyProblem(a.key);
      if (kp) {
        fail(kp);
        return [];
      }
      if (new TextEncoder().encode(a.text).length > MAX_TEXT_BYTES) {
        fail(`the text is longer than ${MAX_TEXT_BYTES} bytes`);
        return [];
      }
      host.use("publish");
      const k = host.str(a.key);
      return [line(`mw_publish(${k}, ${k}_LEN, ${textArgs(host, a.text)});`)];
    }
    case "log":
      if (new TextEncoder().encode(a.text).length > MAX_TEXT_BYTES) {
        fail(`the text is longer than ${MAX_TEXT_BYTES} bytes`);
        return [];
      }
      host.use("log");
      return [line(`mw_log(${textArgs(host, a.text)});`)];
    case "raise": {
      const machine = a.machine ?? ownMachine;
      if (machine === null || machine === "") {
        fail(`raise ${a.event} needs a machine`);
        return [];
      }
      const r = host.raise(machine, a.event);
      if ("problem" in r) {
        fail(r.problem);
        return [];
      }
      return [line(r.code)];
    }
    default:
      fail("unknown action");
      return [];
  }
}

/** Actions in order (at most `MAX_ACTIONS`; more is reported and cut). */
export function lowerActions(as: readonly ActionRef[], host: LowerHost, fail: (msg: string) => void, ownMachine: string | null): Stmt[] {
  if (as.length > MAX_ACTIONS) fail(`at most ${MAX_ACTIONS} actions`);
  return as.slice(0, MAX_ACTIONS).flatMap((a) => lowerAction(a, host, fail, ownMachine));
}
