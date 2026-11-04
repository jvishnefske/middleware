// The S88 phase server a unit payload gets for its equipment phases
// (design.md "S88 recipe model", Q17): generated AssemblyScript that
// answers recipe-core's wire contract (recipe-core/src/wire.rs) over the
// payload ABI 5 request export. Pure: phase specs and lowered body flows
// in, source fragments out; generator.ts places them.
//
// Per phase k (in unit order) the module keeps, in fixed `memory.data`
// arrays indexed by k: its state code, the last applied command id, how
// often its body ran in the current state, the time since the body last
// ran (for `every` bodies), a pending Done flag, the Start arguments and
// the report values. Nothing is allocated at run time.
//
// * init: `node_id` fills the 16 hex digits of every key
//   (`unit/<node>/phase/<em>.<phase>` and `.../state`), `serve` registers
//   each request key, and each phase reports Idle. Without a node id the
//   phases are not served (logged).
// * on_request -> phRequest(k, ...): the command body is decoded exactly
//   as recipe-core `wire::decode_command` does (version 1, code 1..6, id,
//   n <= 4 values, length 7 + 4n, values only for Start) and a Start's
//   arguments are checked against the phase's ranges, each failure
//   InvalidRequest (-2); a reply buffer under 7 bytes is BufferTooSmall
//   (-3); then the phase table (`phNext`, a bounded switch generated from
//   procedural.ts PHASE_NEXT) decides: rejected -> the acknowledgement
//   `[1, 1, state, id]` with the unchanged state; accepted -> `[1, 0, new
//   state, id]`, the id becomes the last applied one, a Start stores its
//   arguments and clears the report values, and the phase enters the new
//   state (phEnter). A slot no phase serves is NotFound (-1).
// * phEnter: sets the state, clears the run count and the wait, logs
//   `phase <em>.<phase>: <State>`, publishes the report and, for an acting
//   state, runs the body (phBody). A body is the flow whose entry is
//   `{on: "phase", phase, state}`; it runs once on entering and, with
//   `every`, again every that many ms while the phase stays in the state
//   (from the shared tick). An acting state without a body is done at
//   once.
// * `phase-done` sets the pending Done flag (with `after`, only once the
//   body has run that often); `phase-report` stores up to 4 values and
//   publishes a report. phSettle, run at the end of every export, applies
//   pending Dones through the table (Running -> Complete, Holding ->
//   Held, Restarting -> Running, Stopping -> Stopped, Aborting ->
//   Aborted, Resetting -> Idle; a Done in a quiescent state is ignored),
//   at most 4 per phase per pass and 2 passes, so it is bounded and never
//   recursive: bodies only set flags.
// * reports: `[1, state, last id, n, n values]` (wire::encode_report) on
//   the state key, on every state change, on phase-report and as a 1 s
//   heartbeat from the tick.

import type { PhaseAction, PhaseValue } from "../model/flows";
import { ACTING_CODE, type ActingState, PHASE_NEXT, PHASE_STATES } from "../model/procedural";
import { type LowerHost, type Stmt, line, printStmts } from "./stmt";

/** Most phases one unit serves (wasm_abi::serve::MAX_SERVES). */
export const MAX_SERVED_PHASES = 8;
/** Most class parameters and report values (recipe-core MAX_PARAMS). */
export const MAX_PHASE_VALUES = 4;
/** The heartbeat period of the state reports. */
export const HEARTBEAT_MS = 1000;
/** The receive buffer of a serving payload (wasm_abi::serve::MAX_REQUEST_BUFFER). */
export const REQUEST_BUFFER = 16 + 512 + 512;

/** One equipment phase of the unit, its ranges in class parameter order. */
export interface PhaseSpec {
  em: string;
  id: string;
  params: readonly { id: string; lo: number; hi: number }[];
}

/** A lowered body flow: which phase, state, repeat period and code. */
export interface PhaseBody {
  phase: string;
  state: ActingState;
  every: number | undefined;
  body: Stmt[];
  /** Reports a problem with this body. */
  fail(msg: string): void;
}

/** What the phase code needs besides the shared lowering host. */
export interface PhaseHost extends LowerHost {
  error(message: string): void;
  /** Code reading the labelled pin (a GPIO input's level or an analog
   * input's raw value), or the problem. */
  pinRead(label: string): { code: string } | { problem: string };
}

/** The planned phases: index by `<em>.<phase>`. */
export interface PhasePlan {
  specs: readonly PhaseSpec[];
  index: ReadonlyMap<string, number>;
}

const IDENT = /^[A-Za-z][A-Za-z0-9_]{0,23}$/;

/** The phases that get code; problems are reported and the phase left
 * out. */
export function planPhases(specs: readonly PhaseSpec[], host: PhaseHost): PhasePlan {
  const kept: PhaseSpec[] = [];
  const index = new Map<string, number>();
  for (const s of specs) {
    const name = `${s.em}.${s.id}`;
    const fail = (m: string) => host.error(`phase ${name}: ${m}`);
    if (!IDENT.test(s.em) || !IDENT.test(s.id)) {
      fail("module and phase names must start with a letter and hold only letters, digits and _ (at most 24)");
      continue;
    }
    if (index.has(name)) {
      fail("is listed twice");
      continue;
    }
    if (kept.length >= MAX_SERVED_PHASES) {
      fail(`a payload serves at most ${MAX_SERVED_PHASES} phases`);
      continue;
    }
    if (s.params.length > MAX_PHASE_VALUES) {
      fail(`a phase class has at most ${MAX_PHASE_VALUES} parameters`);
      continue;
    }
    const bad = s.params.find((p) => !Number.isInteger(p.lo) || !Number.isInteger(p.hi) || p.lo > p.hi || p.lo < -(2 ** 31) || p.hi >= 2 ** 31);
    if (bad) {
      fail(`the range of ${bad.id} must be whole 32-bit numbers, minimum first`);
      continue;
    }
    index.set(name, kept.length);
    kept.push(s);
  }
  return { specs: kept, index };
}

const lit = (v: number) => (v < 0 ? `(${v})` : String(v));

/** A phase action (flowGen.ts calls this for action nodes); `own` is the
 * phase the flow is the body of, if any. */
export function lowerPhaseAction(a: PhaseAction, plan: PhasePlan, own: string | null, host: PhaseHost, fail: (m: string) => void): Stmt[] {
  const k = plan.index.get(a.phase);
  if (k === undefined) {
    fail(`"${a.phase}" is not a phase of this unit that gets code`);
    return [];
  }
  const spec = plan.specs[k] as PhaseSpec;
  const param = (id: string): number | null => {
    const i = spec.params.findIndex((p) => p.id === id);
    if (i < 0) fail(`phase ${a.phase} has no parameter ${id}`);
    return i < 0 ? null : i;
  };
  if (a.kind === "phase-done") {
    const done = `phSet(PH_DONE, ${k}, 1);`;
    if (a.after === undefined) return [line(done)];
    if (own !== a.phase) {
      fail("counting runs only works in the phase's own body flow");
      return [];
    }
    let n: string;
    if (typeof a.after === "number") {
      if (!Number.isInteger(a.after) || a.after < 1 || a.after > 10_000) {
        fail("done after 1 to 10000 runs");
        return [];
      }
      n = String(a.after);
    } else {
      const i = param(a.after);
      if (i === null) return [];
      n = `phArg(${k}, ${i})`;
    }
    return [{ kind: "if", arms: [{ cond: `<i32>(phGet(PH_RUNS, ${k}) >= ${n})`, body: [line(done)] }], else: null }];
  }
  if (a.values.length > MAX_PHASE_VALUES) fail(`a report carries at most ${MAX_PHASE_VALUES} values`);
  const out: Stmt[] = [];
  const values = a.values.slice(0, MAX_PHASE_VALUES);
  values.forEach((v: PhaseValue, i) => {
    let code = "0";
    switch (v.kind) {
      case "const":
        if (!Number.isInteger(v.value) || v.value < -(2 ** 31) || v.value >= 2 ** 31) fail(`${v.value} is not a whole 32-bit number`);
        else code = lit(v.value);
        break;
      case "param": {
        const p = param(v.param);
        if (p !== null) code = `phArg(${k}, ${p})`;
        break;
      }
      case "runs":
        if (own !== a.phase) fail("the run count only exists in the phase's own body flow");
        else code = `phGet(PH_RUNS, ${k})`;
        break;
      case "pin": {
        const r = host.pinRead(v.label);
        if ("problem" in r) fail(r.problem);
        else code = r.code;
        break;
      }
    }
    out.push(line(`phSetVal(${k}, ${i}, ${code});`));
  });
  out.push(line(`phSet(PH_NV, ${k}, ${values.length});`), line(`phReport(${k});`));
  return out;
}

/** What the generator places: constants, globals, functions, lines for
 * `init` and for `tick`, the tick periods the phases need, and the
 * exports' code. */
export interface PhaseCode {
  consts: string[];
  fns: string[];
  init: string[];
  /** Tick lines given the shared tick period. */
  tick(tickMs: number): string[];
  /** Periods the tick must divide (bodies with `every`, the heartbeat). */
  periods: number[];
  onRequest: string;
}

function bytes(text: string): string {
  return [...new TextEncoder().encode(text)].join(", ");
}

/** The phase server for `plan` with `bodies` (each already lowered).
 * Imports and helpers go through `host`. */
export function emitPhases(plan: PhasePlan, bodies: readonly PhaseBody[], host: PhaseHost): PhaseCode {
  const n = plan.specs.length;
  for (const name of ["log", "publish", "serve", "node_id"]) host.use(name);
  host.helper("txBytes");
  const consts: string[] = [
    "// ---- S88 phase server (blocks/phaseGen.ts): recipe-core's wire contract",
    `const PH_N: i32 = ${n};`,
    "// Per phase: state code, last applied id, body runs, ms since the body ran,",
    "// Done pending, serve slot, report value count (i32 each).",
    ...["PH_STATE", "PH_LAST", "PH_RUNS", "PH_WAIT", "PH_DONE", "PH_SLOT", "PH_NV"].map((a) => `const ${a}: usize = memory.data(${4 * n});`),
    `// Start arguments and report values, ${MAX_PHASE_VALUES} per phase.`,
    `const PH_ARGS: usize = memory.data(${4 * MAX_PHASE_VALUES * n});`,
    `const PH_VALS: usize = memory.data(${4 * MAX_PHASE_VALUES * n});`,
    "// Parameter count and accepted range per Start argument.",
    `const PH_NPAR: usize = memory.data<i32>([${plan.specs.map((s) => s.params.length).join(", ")}]);`,
    `const PH_LO: usize = memory.data<i32>([${plan.specs.flatMap((s) => [0, 1, 2, 3].map((i) => s.params[i]?.lo ?? 0)).join(", ")}]);`,
    `const PH_HI: usize = memory.data<i32>([${plan.specs.flatMap((s) => [0, 1, 2, 3].map((i) => s.params[i]?.hi ?? 0)).join(", ")}]);`,
    "const PH_NODE: usize = memory.data(8);",
    `const PH_HEX: usize = memory.data<u8>([${bytes("0123456789abcdef")}]);`,
    `const PH_BODY: usize = memory.data(${7 + 4 * MAX_PHASE_VALUES});`,
    "let phBeat: i32 = 0;",
  ];
  plan.specs.forEach((s, k) => {
    const key = `unit/0000000000000000/phase/${s.em}.${s.id}`;
    consts.push(
      `// ${key} (the node's hex digits are written at init)`,
      `const PH${k}_KEY: usize = memory.data<u8>([${bytes(key)}]);`,
      `const PH${k}_KEY_LEN: i32 = ${key.length};`,
      `const PH${k}_SKEY: usize = memory.data<u8>([${bytes(`${key}/state`)}]);`,
      `const PH${k}_SKEY_LEN: i32 = ${key.length + 6};`,
      `const PH${k}_NAME: usize = memory.data<u8>([${bytes(`phase ${s.em}.${s.id}: `)}]);`,
      `const PH${k}_NAME_LEN: i32 = ${`phase ${s.em}.${s.id}: `.length};`,
    );
  });
  PHASE_STATES.forEach((st, c) => {
    consts.push(`const PH_S${c}: usize = memory.data<u8>([${bytes(st)}]);`, `const PH_S${c}_LEN: i32 = ${st.length};`);
  });

  const switchK = (fn: (k: number) => string, fallback: string) =>
    [...plan.specs.map((_, k) => `  if (k == ${k}) ${fn(k)}`), `  ${fallback}`].join("\n");

  const nextCases = PHASE_NEXT.map((row, s) => {
    const arms = row.flatMap((to, c) => (to < 0 ? [] : [`c == ${c === 6 ? 0 : c + 1} ? ${to}`]));
    return `    case ${s}: return ${[...arms, "-1"].join(" : ")};`;
  });

  // Bodies grouped by phase; the generator reports a second body itself.
  const byPhase = new Map<number, PhaseBody[]>();
  for (const b of bodies) {
    const k = plan.index.get(b.phase);
    if (k === undefined) {
      b.fail(`"${b.phase}" is not a phase of this unit that gets code`);
      continue;
    }
    const list = byPhase.get(k) ?? [];
    if (list.some((x) => x.state === b.state)) {
      b.fail(`phase ${b.phase} already has a ${b.state} body`);
      continue;
    }
    if (b.every !== undefined && (!Number.isInteger(b.every) || b.every < 10 || b.every > 2_147_483_647)) {
      b.fail("the period must be a whole number from 10 to 2147483647 ms");
      continue;
    }
    list.push(b);
    byPhase.set(k, list);
  }
  const bodyLines: string[] = [];
  for (const [k, list] of byPhase) {
    for (const b of list) {
      const out: string[] = [];
      printStmts(b.body, "    ", out);
      bodyLines.push(`  if (k == ${k} && s == ${ACTING_CODE[b.state]}) {`, ...out, "    return;", "  }");
    }
  }

  const fns: string[] = [
    `function phGet(base: usize, k: i32): i32 {
  return load<i32>(base + (<usize>k << 2));
}`,
    `function phSet(base: usize, k: i32, v: i32): void {
  store<i32>(base + (<usize>k << 2), v);
}`,
    `function phArg(k: i32, i: i32): i32 {
  return load<i32>(PH_ARGS + (<usize>(k * ${MAX_PHASE_VALUES} + i) << 2));
}`,
    `function phSetVal(k: i32, i: i32, v: i32): void {
  store<i32>(PH_VALS + (<usize>(k * ${MAX_PHASE_VALUES} + i) << 2), v);
}`,
    `// The phase state model (recipe-core phase::next): the next state for
// state s and input c (1..6 Start, Hold, Restart, Stop, Abort, Reset; 0
// Done), or -1 when the table rejects it.
function phNext(s: i32, c: i32): i32 {
  switch (s) {
${nextCases.join("\n")}
  }
  return -1;
}`,
    `function phActing(s: i32): bool {
  return s == 1 || s == 3 || s == 5 || s == 6 || s == 8 || s == 10;
}`,
    `// Writes the node id as 16 hex digits at dst.
function phHex(dst: usize): void {
  for (let i = 0; i < 8; i++) {
    const b = <i32>load<u8>(PH_NODE + <usize>i);
    store<u8>(dst + <usize>(2 * i), load<u8>(PH_HEX + <usize>(b >> 4)));
    store<u8>(dst + <usize>(2 * i + 1), load<u8>(PH_HEX + <usize>(b & 15)));
  }
}`,
    `// Publishes phase k's state report (recipe-core wire::encode_report).
function phReport(k: i32): void {
  const nv = phGet(PH_NV, k);
  store<u8>(PH_BODY, 1);
  store<u8>(PH_BODY + 1, <u8>phGet(PH_STATE, k));
  store<i32>(PH_BODY + 2, phGet(PH_LAST, k));
  store<u8>(PH_BODY + 6, <u8>nv);
  for (let i = 0; i < nv && i < ${MAX_PHASE_VALUES}; i++) store<i32>(PH_BODY + 7 + (<usize>i << 2), load<i32>(PH_VALS + (<usize>(k * ${MAX_PHASE_VALUES} + i) << 2)));
${switchK((k) => `mw_publish(PH${k}_SKEY, PH${k}_SKEY_LEN, PH_BODY, 7 + 4 * nv);`, "")}
}`,
    `function phStateName(s: i32): void {
${PHASE_STATES.map((_, c) => `  if (s == ${c}) txBytes(PH_S${c}, PH_S${c}_LEN);`).join("\n")}
}`,
    `// Logs "phase <em>.<phase>: <State>".
function phLog(k: i32, s: i32): void {
  txLen = 0;
${switchK((k) => `txBytes(PH${k}_NAME, PH${k}_NAME_LEN);`, "")}
  phStateName(s);
  mw_log(TX, txLen);
}`,
    `// Runs phase k's body for its current state (an acting state); without
// a body the state is done at once.
function phBody(k: i32): void {
  phSet(PH_RUNS, k, phGet(PH_RUNS, k) + 1);
  const s = phGet(PH_STATE, k);
${bodyLines.join("\n")}
  phSet(PH_DONE, k, 1);
}`,
    `function phEnter(k: i32, s: i32): void {
  phSet(PH_STATE, k, s);
  phSet(PH_RUNS, k, 0);
  phSet(PH_WAIT, k, 0);
  phSet(PH_DONE, k, 0);
  phLog(k, s);
  phReport(k);
  if (phActing(s)) phBody(k);
}`,
    `// Applies pending Dones through the table; bounded: 2 passes, at most 4
// steps per phase each.
function phSettle(): void {
  for (let pass = 0; pass < 2; pass++) {
    for (let k = 0; k < PH_N; k++) {
      for (let i = 0; i < 4; i++) {
        if (phGet(PH_DONE, k) == 0) break;
        phSet(PH_DONE, k, 0);
        const to = phNext(phGet(PH_STATE, k), 0);
        if (to >= 0) phEnter(k, to);
      }
    }
  }
}`,
    `// One command for phase k (recipe-core wire::serve): the reply length or
// a negated HandlerError code.
function phRequest(k: i32, req: usize, len: i32, reply: usize, cap: i32): i32 {
  if (len < 7 || load<u8>(req) != 1) return -2;
  const c = <i32>load<u8>(req + 1);
  if (c < 1 || c > 6) return -2;
  const id = load<i32>(req + 2);
  const n = <i32>load<u8>(req + 6);
  if (n > ${MAX_PHASE_VALUES} || len != 7 + 4 * n) return -2;
  if (c != 1 && n != 0) return -2;
  if (c == 1) {
    if (n != load<i32>(PH_NPAR + (<usize>k << 2))) return -2;
    for (let i = 0; i < n; i++) {
      const v = load<i32>(req + 7 + (<usize>i << 2));
      const at = <usize>(k * ${MAX_PHASE_VALUES} + i) << 2;
      if (v < load<i32>(PH_LO + at) || v > load<i32>(PH_HI + at)) return -2;
    }
  }
  if (cap < 7) return -3;
  const s = phGet(PH_STATE, k);
  const to = phNext(s, c);
  store<u8>(reply, 1);
  store<u8>(reply + 1, to < 0 ? 1 : 0);
  store<u8>(reply + 2, <u8>(to < 0 ? s : to));
  store<i32>(reply + 3, id);
  if (to >= 0) {
    phSet(PH_LAST, k, id);
    if (c == 1) {
      for (let i = 0; i < n; i++) store<i32>(PH_ARGS + (<usize>(k * ${MAX_PHASE_VALUES} + i) << 2), load<i32>(req + 7 + (<usize>i << 2)));
      phSet(PH_NV, k, 0);
    }
    phEnter(k, to);
  }
  return 7;
}`,
  ];
  host.use("log");
  const noNode = host.str("phase: no node id; the phases are not served");
  const init = [
    "  for (let k = 0; k < PH_N; k++) phSet(PH_SLOT, k, -1);",
    "  if (mw_node_id(PH_NODE) != 0) {",
    `    mw_log(${noNode}, ${noNode}_LEN);`,
    "  } else {",
    ...plan.specs.flatMap((_, k) => [`    phHex(PH${k}_KEY + 5);`, `    phHex(PH${k}_SKEY + 5);`, `    phSet(PH_SLOT, ${k}, mw_serve(PH${k}_KEY, PH${k}_KEY_LEN));`, `    phReport(${k});`]),
    "  }",
  ];
  const everyBodies = [...byPhase].flatMap(([k, list]) => list.filter((b) => b.every !== undefined).map((b) => ({ k, code: ACTING_CODE[b.state], ms: b.every as number })));
  const tick = (tickMs: number): string[] => [
    ...everyBodies.flatMap((e) => [
      `  if (phGet(PH_STATE, ${e.k}) == ${e.code}) {`,
      `    phSet(PH_WAIT, ${e.k}, phGet(PH_WAIT, ${e.k}) + ${tickMs});`,
      `    if (phGet(PH_WAIT, ${e.k}) >= ${e.ms}) {`,
      `      phSet(PH_WAIT, ${e.k}, 0);`,
      `      phBody(${e.k});`,
      "    }",
      "  }",
    ]),
    `  phBeat += ${tickMs};`,
    `  if (phBeat >= ${HEARTBEAT_MS}) {`,
    "    phBeat = 0;",
    "    for (let k = 0; k < PH_N; k++) phReport(k);",
    "  }",
  ];
  const onRequest = [
    "export function on_request(slot: i32, reqPtr: usize, reqLen: i32, metaPtr: usize, replyPtr: usize, replyCap: i32): i32 {",
    "  let r: i32 = -1;",
    "  for (let k = 0; k < PH_N; k++) {",
    "    if (slot >= 0 && phGet(PH_SLOT, k) == slot) {",
    "      r = phRequest(k, reqPtr, reqLen, replyPtr, replyCap);",
    "      break;",
    "    }",
    "  }",
    "  phSettle();",
    "  return r;",
    "}",
  ].join("\n");
  return { consts, fns, init, tick, periods: [HEARTBEAT_MS, ...everyBodies.map((e) => e.ms)], onRequest };
}
