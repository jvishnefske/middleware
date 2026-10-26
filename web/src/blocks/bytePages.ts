// Rewrites a compiled payload's memory to 1-byte pages (the WebAssembly
// custom-page-sizes proposal) with a fixed size in bytes, for boards whose
// wasm-host `Limits::bytes` cap is below one 64 KiB page (the conagotchi
// badge: 16 KiB).
//
// Why a rewrite: the Rust payloads get this from the linker
// (`payloads/scripts/build-byte-pages.sh` passes `--page-size=1` and
// `--initial-memory`/`--max-memory` in bytes), but AssemblyScript 0.28 has
// no equivalent. Its `--initialMemory`/`--maximumMemory` count 64 KiB pages,
// and it hands binaryen's `setMemory` no page size (binaryen knows the
// `CustomPageSizes` feature, but asc does not expose it). An imported
// memory with byte limits is no way out either, because wasm-host rejects
// every non-function import. So the module is compiled with one 64 KiB
// page and its memory section is replaced afterwards by
// `flags 0x09 (maximum + page size), min = max = bytes, log2(page) = 0`,
// the encoding wasm-host's `abi::initial_memory_bytes` reads.
//
// That is sound only if the code never depends on the page size, so the
// module must:
//
// * define exactly one memory and import none;
// * contain no `memory.size` or `memory.grow` (the stub runtime's
//   allocator scales `memory.size` by 65536; the Blocks generator never
//   allocates, so asc leaves the allocator out);
// * keep every active data segment, i.e. all static data and buffers,
//   inside the new size.
//
// Pure functions over bytes; the `memory.size`/`memory.grow` check is
// done by the caller on wabt's disassembly (see `pipeline.ts`), because
// spotting opcodes needs a full instruction decoder.

export type BytePagesResult = { ok: true; wasm: Uint8Array; dataEnd: number } | { ok: false; error: string };

const SECTION_IMPORT = 2;
const SECTION_MEMORY = 5;
const SECTION_DATA = 11;
const IMPORT_KIND_MEMORY = 2;
/** Memory flags: has maximum (1) | custom page size (8). */
const FLAGS_MAX_AND_PAGE_SIZE = 0x09;

class Reader {
  constructor(
    readonly bytes: Uint8Array,
    public at = 0,
  ) {}
  byte(): number {
    const b = this.bytes[this.at];
    if (b === undefined) throw new Error("unexpected end of module");
    this.at++;
    return b;
  }
  /** Unsigned LEB128, at most 5 bytes (32 bits). */
  u32(): number {
    let v = 0;
    for (let shift = 0; shift < 35; shift += 7) {
      const b = this.byte();
      v += (b & 0x7f) * 2 ** shift;
      if ((b & 0x80) === 0) return v;
    }
    throw new Error("LEB128 value too long");
  }
  /** Signed LEB128, at most 5 bytes. */
  s32(): number {
    let v = 0;
    let shift = 0;
    let b = 0;
    for (; shift < 35; shift += 7) {
      b = this.byte();
      v += (b & 0x7f) * 2 ** shift;
      if ((b & 0x80) === 0) break;
    }
    shift += 7;
    if (b & 0x40) v -= 2 ** shift;
    return v;
  }
  skip(n: number): void {
    if (this.at + n > this.bytes.length) throw new Error("unexpected end of module");
    this.at += n;
  }
}

function leb(n: number): number[] {
  const out: number[] = [];
  for (let i = 0; i < 5; i++) {
    const b = n & 0x7f;
    n = Math.floor(n / 128);
    if (n === 0) {
      out.push(b);
      return out;
    }
    out.push(b | 0x80);
  }
  return out;
}

interface Section {
  id: number;
  start: number;
  bodyStart: number;
  end: number;
}

function sections(wasm: Uint8Array): Section[] {
  const r = new Reader(wasm, 8);
  const out: Section[] = [];
  while (r.at < wasm.length) {
    const start = r.at;
    const id = r.byte();
    const len = r.u32();
    const bodyStart = r.at;
    r.skip(len);
    out.push({ id, start, bodyStart, end: r.at });
  }
  return out;
}

/** End offset of the highest active data segment (0 if none). */
function dataEnd(wasm: Uint8Array, s: Section): number {
  const r = new Reader(wasm.subarray(s.bodyStart, s.end));
  const count = r.u32();
  let end = 0;
  for (let i = 0; i < count; i++) {
    const flags = r.u32();
    if (flags === 1) {
      // Passive: no address.
      r.skip(r.u32());
      continue;
    }
    if (flags === 2) r.u32(); // memory index
    if (flags !== 0 && flags !== 2) throw new Error(`unsupported data segment flags ${flags}`);
    if (r.byte() !== 0x41) throw new Error("data segment offset is not an i32.const");
    const offset = r.s32();
    if (r.byte() !== 0x0b) throw new Error("data segment offset expression is not a single constant");
    const len = r.u32();
    r.skip(len);
    end = Math.max(end, offset + len);
  }
  return end;
}

function importsMemory(wasm: Uint8Array, s: Section): boolean {
  const r = new Reader(wasm.subarray(s.bodyStart, s.end));
  const count = r.u32();
  for (let i = 0; i < count; i++) {
    r.skip(r.u32());
    r.skip(r.u32());
    const kind = r.byte();
    if (kind === IMPORT_KIND_MEMORY) return true;
    if (kind === 0) r.u32();
    else if (kind === 1) {
      r.byte();
      const f = r.u32();
      r.u32();
      if (f & 1) r.u32();
    } else if (kind === 3) r.skip(2);
    else return false;
  }
  return false;
}

/** Replaces the module's memory with `bytes` bytes of 1-byte pages. */
export function toBytePages(wasm: Uint8Array, bytes: number): BytePagesResult {
  try {
    if (wasm.length < 8 || wasm[0] !== 0 || wasm[1] !== 0x61 || wasm[2] !== 0x73 || wasm[3] !== 0x6d) {
      return { ok: false, error: "not a wasm module" };
    }
    const all = sections(wasm);
    const imp = all.find((s) => s.id === SECTION_IMPORT);
    if (imp && importsMemory(wasm, imp)) return { ok: false, error: "the module imports its memory" };
    const mem = all.filter((s) => s.id === SECTION_MEMORY);
    const memSection = mem[0];
    if (mem.length !== 1 || !memSection) return { ok: false, error: "the module has no memory section" };
    const count = new Reader(wasm.subarray(memSection.bodyStart, memSection.end)).u32();
    if (count !== 1) return { ok: false, error: `the module defines ${count} memories, not 1` };
    const data = all.find((s) => s.id === SECTION_DATA);
    const end = data ? dataEnd(wasm, data) : 0;
    if (end > bytes) {
      return { ok: false, error: `static data ends at byte ${end}, beyond the ${bytes}-byte memory` };
    }
    const body = [1, FLAGS_MAX_AND_PAGE_SIZE, ...leb(bytes), ...leb(bytes), 0];
    const section = Uint8Array.from([SECTION_MEMORY, ...leb(body.length), ...body]);
    const out = new Uint8Array(wasm.length - (memSection.end - memSection.start) + section.length);
    out.set(wasm.subarray(0, memSection.start), 0);
    out.set(section, memSection.start);
    out.set(wasm.subarray(memSection.end), memSection.start + section.length);
    return { ok: true, wasm: out, dataEnd: end };
  } catch (e) {
    return { ok: false, error: e instanceof Error ? e.message : String(e) };
  }
}

/** Initial memory in bytes as wasm-host computes it (for tests and the
 * build report): one memory's minimum times its page size. */
export function initialMemoryBytes(wasm: Uint8Array): number | null {
  try {
    const mem = sections(wasm).find((s) => s.id === SECTION_MEMORY);
    if (!mem) return 0;
    const r = new Reader(wasm.subarray(mem.bodyStart, mem.end));
    if (r.u32() < 1) return 0;
    const flags = r.u32();
    const min = r.u32();
    if (flags & 1) r.u32();
    const log2 = flags & 8 ? r.u32() : 16;
    return min * 2 ** log2;
  } catch {
    return null;
  }
}
