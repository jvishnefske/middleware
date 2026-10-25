// Quick ABI check of a compiled payload before simulating or uploading.
//
// Uses only the JS WebAssembly reflection API (names and kinds; it does not
// expose function signatures) against the import table and export names
// from protocol-wasm's `abi()`, so ABI 2 imports (timers, GPIO interrupts,
// SPI and UART DMA) are accepted without a list kept here. The full check,
// including signatures, runs in wasm-host when the module is loaded into
// the simulator or onto the device.
//
// Problems are ABI violations the device would reject. Warnings flag a
// payload that starts interrupt sources but exports no `on_irq`, whose
// events the device would discard.

import type { AbiInfo } from "../protocol/wasm";

export interface AbiReport {
  problems: string[];
  warnings: string[];
  imports: string[];
  exports: string[];
}

/** Imports that make the device raise `on_irq` events. */
const EVENT_SOURCES = ["gpio_irq", "timer_start", "spi_transfer", "uart_open", "uart_write"];

export function checkAbi(wasm: Uint8Array, abi: AbiInfo): AbiReport {
  let mod: WebAssembly.Module;
  try {
    mod = new WebAssembly.Module(wasm as BufferSource);
  } catch (e) {
    return {
      problems: [`not a valid module: ${e instanceof Error ? e.message : String(e)}`],
      warnings: [],
      imports: [],
      exports: [],
    };
  }
  const problems: string[] = [];
  const warnings: string[] = [];
  const known = new Set(abi.imports.map((i) => i.name));
  const imports = WebAssembly.Module.imports(mod);
  for (const i of imports) {
    if (i.module !== abi.importModule || i.kind !== "function" || !known.has(i.name)) {
      problems.push(`unsupported import ${i.module}.${i.name} (${i.kind})`);
    }
  }
  const exports = WebAssembly.Module.exports(mod);
  if (!exports.some((e) => e.name === abi.exports.memory && e.kind === "memory")) {
    problems.push(`missing export "${abi.exports.memory}"`);
  }
  for (const name of [abi.exports.init, abi.exports.tick, abi.exports.alloc, abi.exports.onMessage, abi.exports.onIrq]) {
    const e = exports.find((x) => x.name === name);
    if (e && e.kind !== "function") problems.push(`export "${name}" must be a function`);
  }
  const names = new Set(exports.map((e) => e.name));
  if (names.has(abi.exports.onMessage) !== names.has(abi.exports.alloc)) {
    problems.push(`"${abi.exports.onMessage}" and "${abi.exports.alloc}" must be exported together`);
  }
  const sources = imports.filter((i) => i.module === abi.importModule && EVENT_SOURCES.includes(i.name));
  if (sources.length > 0 && !names.has(abi.exports.onIrq)) {
    warnings.push(
      `imports ${sources.map((i) => i.name).join(", ")} but does not export "${abi.exports.onIrq}"; its events will be discarded`,
    );
  }
  return {
    problems,
    warnings,
    imports: imports.map((i) => `${i.module}.${i.name}`),
    exports: exports.map((e) => e.name),
  };
}
