// In-browser payload compilers (requirement U5).
//
// * WAT -> wasm with wabt (wabt.js, the reference toolkit compiled to wasm).
// * AssemblyScript -> wasm with the `asc` compiler API. Options match the
//   payload templates (payloads/templates/README.md): stub runtime, no
//   `env.abort` import (the device does not provide one), optimised, and
//   exactly one non-growing 64 KiB memory page.
//
// Both compilers are loaded lazily on first use; AssemblyScript pulls in
// binaryen, which is several megabytes.

export type Language = "wat" | "as";

export type CompileResult = { ok: true; wasm: Uint8Array; log: string } | { ok: false; errors: string };

type WabtModule = Awaited<ReturnType<typeof import("wabt")>>;
let wabtPromise: Promise<WabtModule> | null = null;

function loadWabt(): Promise<WabtModule> {
  wabtPromise ??= import("wabt").then((m) => (m.default as unknown as () => Promise<WabtModule>)());
  return wabtPromise;
}

/** Compiles WebAssembly text. */
export async function compileWat(source: string): Promise<CompileResult> {
  const wabt = await loadWabt();
  let mod: ReturnType<WabtModule["parseWat"]> | null = null;
  try {
    mod = wabt.parseWat("payload.wat", source);
    mod.resolveNames();
    mod.validate();
    const { buffer, log } = mod.toBinary({ log: false, write_debug_names: false });
    return { ok: true, wasm: new Uint8Array(buffer), log };
  } catch (e) {
    return { ok: false, errors: e instanceof Error ? e.message : String(e) };
  } finally {
    mod?.destroy();
  }
}

/** asc options equivalent to
 * `--runtime stub --use abort= -O --initialMemory 1 --maximumMemory 1`. */
export const ASC_OPTIONS = {
  runtime: "stub",
  use: ["abort="],
  optimizeLevel: 3,
  shrinkLevel: 0,
  initialMemory: 1,
  maximumMemory: 1,
} as const;

/** Compiles AssemblyScript source. */
export async function compileAssemblyScript(source: string): Promise<CompileResult> {
  const asc = (await import("assemblyscript/asc")).default;
  const r = await asc.compileString({ "payload.ts": source }, { ...ASC_OPTIONS, use: [...ASC_OPTIONS.use] });
  const stderr = r.stderr.toString();
  if (r.error || !r.binary) {
    return { ok: false, errors: stderr || (r.error ? r.error.message : "compilation produced no binary") };
  }
  return { ok: true, wasm: r.binary, log: stderr };
}

export function compile(lang: Language, source: string): Promise<CompileResult> {
  return lang === "wat" ? compileWat(source) : compileAssemblyScript(source);
}
