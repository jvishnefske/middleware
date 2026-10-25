// Editor templates.
//
// The canonical templates live in ../payloads/templates: blink and echo in
// WAT and AssemblyScript, plus ABI 2 examples (button, timer, spi, ...) in
// whichever languages they are written in. `import.meta.glob` picks up
// whichever exist at build time and yields nothing for missing files, so
// the site still builds without them; a minimal built-in blink fills any
// gap. The selector always offers blink and echo, then every other template
// found for the current language.

import type { Language } from "./compile";

/** A template's base name, e.g. "blink" for blink.wat / blink.ts. */
export type TemplateName = string;

/** Always offered, in this order, falling back to the built-in blink. */
export const BASE_TEMPLATES: readonly TemplateName[] = ["blink", "echo"];

const found = import.meta.glob("../../../payloads/templates/*.{wat,ts}", {
  query: "?raw",
  import: "default",
  eager: true,
}) as Record<string, string>;

const FALLBACK_WAT = `;; Minimal blink (built-in fallback template).
(module
  (import "mw" "log" (func $log (param i32 i32)))
  (import "mw" "set_tick" (func $set_tick (param i32) (result i32)))
  (import "mw" "gpio_output" (func $gpio_output (param i32) (result i32)))
  (import "mw" "gpio_write" (func $gpio_write (param i32 i32) (result i32)))
  (memory (export "memory") 1 1)
  (data (i32.const 16) "blink")
  (global $level (mut i32) (i32.const 0))
  (func (export "init")
    (drop (call $gpio_output (i32.const 8)))
    (drop (call $set_tick (i32.const 500)))
    (call $log (i32.const 16) (i32.const 5)))
  (func (export "tick") (param i32)
    (global.set $level (i32.xor (global.get $level) (i32.const 1)))
    (drop (call $gpio_write (i32.const 8) (global.get $level)))))
`;

const FALLBACK_AS = `// Minimal blink (built-in fallback template).
@external("mw", "set_tick")
declare function mw_set_tick(periodMs: i32): i32;
@external("mw", "gpio_output")
declare function mw_gpio_output(pin: i32): i32;
@external("mw", "gpio_write")
declare function mw_gpio_write(pin: i32, level: i32): i32;

let level: i32 = 0;

export function init(): void {
  mw_gpio_output(8);
  mw_set_tick(500);
}

export function tick(nowMs: i32): void {
  level ^= 1;
  mw_gpio_write(8, level);
}
`;

/** Looks up a template in `files` (path -> source), falling back to the
 * built-in blink. Exposed with an explicit map for tests. */
export function templateFrom(
  files: Record<string, string>,
  name: TemplateName,
  lang: Language,
): { source: string; builtin: boolean } {
  const hit = Object.entries(files).find(([path]) => path.endsWith(`/${name}.${extension(lang)}`));
  if (hit) return { source: hit[1], builtin: false };
  return { source: lang === "wat" ? FALLBACK_WAT : FALLBACK_AS, builtin: true };
}

export function template(name: TemplateName, lang: Language): { source: string; builtin: boolean } {
  return templateFrom(found, name, lang);
}

function extension(lang: Language): string {
  return lang === "wat" ? "wat" : "ts";
}

/** Template names for `lang`: the base templates, then every other file
 * in `files` with that language's extension, sorted. */
export function templateNamesFrom(files: Record<string, string>, lang: Language): TemplateName[] {
  const ext = `.${extension(lang)}`;
  const extra = Object.keys(files)
    .map((path) => path.slice(path.lastIndexOf("/") + 1))
    .filter((file) => file.endsWith(ext))
    .map((file) => file.slice(0, -ext.length))
    .filter((name) => !BASE_TEMPLATES.includes(name));
  return [...BASE_TEMPLATES, ...[...new Set(extra)].sort()];
}

export function templateNames(lang: Language): TemplateName[] {
  return templateNamesFrom(found, lang);
}
