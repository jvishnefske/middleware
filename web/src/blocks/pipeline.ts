// Blocks build pipeline: workspace -> AssemblyScript (generator.ts) ->
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
  | { ok: false; source: string; stage: "generate" | "compile" | "abi" | "memory"; errors: string[]; issues: GenIssue[] };

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

export async function buildPayload(ws: WorkspaceJson, profile: ProfileId, abi: AbiInfo): Promise<BuildResult> {
  const gen = generate(ws, profile);
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
