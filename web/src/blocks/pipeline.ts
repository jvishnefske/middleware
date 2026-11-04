// Blocks build pipeline: the pin table's check (protocol-wasm checkPins,
// done by the caller and passed in; any issue stops the build at stage
// "pins") -> the state machines' check (protocol-wasm checkMachines, also
// done by the caller; an error stops the build at stage "states", a
// warning never does) -> the flows' check (protocol-wasm checkFlows, the
// same way, stage "flows") -> workspace + pin table + machines + flows ->
// AssemblyScript (generator.ts) ->
// wasm (the editor's `compileAssemblyScript`, same asc options as the
// templates) -> ABI pre-check (`checkAbi`) -> for byte-sized profiles, the
// 1-byte-page memory rewrite (bytePages.ts).
//
// `checkAbi` runs on the 64 KiB-page module: the JS engine that does the
// reflection may not implement custom page sizes, and the rewrite changes
// only the memory section, not the imports or exports. wasm-host (the
// simulator and the device) validates the final module itself.

import { checkAbi, type AbiReport } from "../editor/abiCheck";
import { compileAssemblyScript, loadWabt } from "../editor/compile";
import type { AbiInfo } from "../protocol/wasm";
import { toBytePages } from "./bytePages";
import { type GenIssue, generate } from "./generator";
import type { Flow, FlowIssue } from "../model/flows";
import type { Machine, MachineIssue } from "../model/machines";
import { EMPTY_PINS, type PinIssue, type PinTable } from "../model/pins";
import { PROFILES, type ProfileId } from "./profiles";
import type { WorkspaceJson } from "./workspace";

export type BuildResult =
  | {
      ok: true;
      source: string;
      wasm: Uint8Array;
      report: AbiReport;
      warnings: GenIssue[];
      /** Linear memory in bytes as declared by the final module. */
      memoryBytes: number;
      log: string;
    }
  | { ok: false; source: string; stage: "pins" | "states" | "flows" | "generate" | "compile" | "abi" | "memory"; errors: string[]; issues: GenIssue[] };

/** Whether wasm code uses `memory.size` or `memory.grow`, whose results
 * depend on the page size. */
export async function usesMemorySize(wasm: Uint8Array): Promise<boolean> {
  const wabt = await loadWabt();
  const mod = wabt.readWasm(wasm, { readDebugNames: false });
  try {
    return /\bmemory\.(size|grow)\b/.test(mod.toText({ foldExprs: false, inlineExport: false }));
  } finally {
    mod.destroy();
  }
}

/** The pin table and what checkPins said about it; the state machines
 * and what checkMachines said about them; the flows and what checkFlows
 * said about them (none when absent). */
export interface PinInput {
  pins: PinTable;
  issues: readonly PinIssue[];
  machines?: readonly Machine[];
  machineIssues?: readonly MachineIssue[];
  flows?: readonly Flow[];
  flowIssues?: readonly FlowIssue[];
}

/** A checkFlows issue as one line: `Flows Press node n2: message`
 * (`wire <from>.<port> -> <to>` for a wire, nothing for the flow). */
export function flowIssueText(i: FlowIssue, flows: readonly Flow[]): string {
  const f = flows[i.flow];
  const id = f?.id ? f.id : `flow ${i.flow + 1}`;
  let where = "";
  if (i.node !== null) where = ` node ${f?.nodes[i.node]?.id ?? i.node + 1}`;
  else if (i.wire !== null) {
    const w = f?.wires[i.wire];
    where = w ? ` wire ${w.from.node}.${w.from.port} -> ${w.to.node}` : ` wire ${i.wire + 1}`;
  }
  return `Flows ${id}${where}: ${i.message}`;
}

/** A checkMachines issue as one line: `States Mode row 2: message`
 * (`state <name>` / `event <name>` for those lists, nothing for the
 * machine as a whole). */
export function machineIssueText(i: MachineIssue, machines: readonly Machine[]): string {
  const m = machines[i.machine];
  const id = m?.id ? m.id : `machine ${i.machine + 1}`;
  let where = "";
  if (i.row !== null) {
    if (i.list === "rows") where = ` row ${i.row + 1}`;
    else if (i.list === "states") where = ` state ${m?.states[i.row]?.name || i.row + 1}`;
    else if (i.list === "events") where = ` event ${m?.events[i.row]?.name || i.row + 1}`;
  }
  return `States ${id}${where}: ${i.message}`;
}

export async function buildPayload(
  ws: WorkspaceJson,
  profile: ProfileId,
  abi: AbiInfo,
  pinInput: PinInput = { pins: EMPTY_PINS, issues: [] },
): Promise<BuildResult> {
  const machines = pinInput.machines ?? [];
  const flows = pinInput.flows ?? [];
  const gen = generate(ws, profile, pinInput.pins, machines, flows);
  if (pinInput.issues.length > 0) {
    const where = (i: PinIssue) => (i.row === null ? "" : `row ${i.row + 1}${pinInput.pins.rows[i.row]?.label ? ` (${pinInput.pins.rows[i.row]?.label})` : ""}: `);
    return { ok: false, source: gen.source, stage: "pins", errors: pinInput.issues.map((i) => `Pins ${where(i)}${i.message}`), issues: [] };
  }
  const machineErrors = (pinInput.machineIssues ?? []).filter((i) => i.severity === "error");
  if (machineErrors.length > 0) {
    return { ok: false, source: gen.source, stage: "states", errors: machineErrors.map((i) => machineIssueText(i, machines)), issues: [] };
  }
  const flowErrors = (pinInput.flowIssues ?? []).filter((i) => i.severity === "error");
  if (flowErrors.length > 0) {
    return { ok: false, source: gen.source, stage: "flows", errors: flowErrors.map((i) => flowIssueText(i, flows)), issues: [] };
  }
  if (gen.errors.length > 0) {
    return { ok: false, source: gen.source, stage: "generate", errors: gen.errors.map((e) => e.message), issues: gen.errors };
  }
  const r = await compileAssemblyScript(gen.source);
  if (!r.ok) return { ok: false, source: gen.source, stage: "compile", errors: [r.errors], issues: [] };
  const report = checkAbi(r.wasm, abi);
  if (report.problems.length > 0) return { ok: false, source: gen.source, stage: "abi", errors: report.problems, issues: [] };
  const layout = PROFILES[profile].memory;
  if (layout.kind === "page64k") {
    return { ok: true, source: gen.source, wasm: r.wasm, report, warnings: gen.warnings, memoryBytes: 65536, log: r.log };
  }
  if (await usesMemorySize(r.wasm)) {
    return {
      ok: false,
      source: gen.source,
      stage: "memory",
      errors: ["the module uses memory.size or memory.grow, which would change meaning with 1-byte pages"],
      issues: [],
    };
  }
  const bp = toBytePages(r.wasm, layout.bytes);
  if (!bp.ok) return { ok: false, source: gen.source, stage: "memory", errors: [bp.error], issues: [] };
  return { ok: true, source: gen.source, wasm: bp.wasm, report, warnings: gen.warnings, memoryBytes: layout.bytes, log: r.log };
}
