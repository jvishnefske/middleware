// Runs `mwctl` (tools/mwctl built for wasm32-wasip1) in the page.
//
// The module is the real command-line tool: it gets its arguments, an
// in-memory file tree, stdout and stderr through the WASI imports below,
// and its serial port through the five `mw_port` imports
// (tools/mwctl/src/webport.rs). Nothing here knows a mwctl command.
//
// mwctl is synchronous: it writes a request and waits for the reply. A
// browser cannot block, so the imports that wait (`mw_port.*` and the
// sleep in `poll_oneoff`) are wrapped with `WebAssembly.Suspending` and
// `_start` with `WebAssembly.promising` (JavaScript Promise Integration,
// in every browser that has Web Serial). Without JSPI the module still
// runs to completion for commands that never wait, such as `wire --emit
// report`; a command that needs the port then fails with "no such port".
//
// Only the WASI calls the module imports are implemented, over one
// preopened directory `/` holding `files`.

/** The serial port mwctl sees as `--port web`. Results follow
 * webport.rs: 0 or a byte count, -1 no such port, -2 the port went away,
 * -3 another failure. */
export interface PortHost {
  open(path: string, baud: number): Promise<number>;
  write(bytes: Uint8Array): Promise<number>;
  /** At most `cap` bytes, or an empty array when nothing came in time. */
  read(cap: number, timeoutMs: number): Promise<Uint8Array | number>;
  signals(dtr: boolean, rts: boolean): Promise<number>;
  close(): Promise<void>;
}

export interface RunOptions {
  /** Files under `/`, by path without the leading slash. Files the
   * command creates or changes are added to this map. */
  files?: Map<string, Uint8Array>;
  stdin?: Uint8Array;
  port?: PortHost;
  /** Called with each chunk as it is written. */
  onStdout?(text: string): void;
  onStderr?(text: string): void;
  /** Milliseconds, monotonic (default `performance.now`). */
  now?(): number;
  /** Waits (default `setTimeout`). */
  sleep?(ms: number): Promise<void>;
}

export interface RunResult {
  /** Exit status: 0 success, 1 device or port failure, 2 usage error. */
  code: number;
  stdout: string;
  stderr: string;
}

interface Jspi {
  Suspending: new (f: (...args: never[]) => unknown) => WebAssembly.ImportValue;
  promising(f: (...args: unknown[]) => unknown): (...args: unknown[]) => Promise<unknown>;
}

/** JavaScript Promise Integration, when this engine has it. */
export function jspi(): Jspi | null {
  const w = WebAssembly as unknown as Partial<Jspi>;
  return typeof w.Suspending === "function" && typeof w.promising === "function" ? (w as Jspi) : null;
}

// WASI errno.
const OK = 0;
const EBADF = 8;
const EEXIST = 20;
const EINVAL = 28;
const ENOENT = 44;
// path_open oflags.
const O_CREAT = 1;
const O_EXCL = 4;
const O_TRUNC = 8;
// Descriptors: 0..2 are the standard streams, 3 is the preopened `/`.
const ROOT_FD = 3;

class Exit extends Error {
  constructor(readonly code: number) {
    super(`exit ${code}`);
  }
}

interface OpenFile {
  path: string;
  pos: number;
}

function normalise(path: string): string {
  return path
    .split("/")
    .filter((s) => s !== "" && s !== ".")
    .join("/");
}

/** Splits a command line the way a shell would for mwctl's needs: spaces
 * separate, single and double quotes group, a backslash keeps the next
 * character. */
export function splitCommandLine(line: string): string[] {
  const out: string[] = [];
  let word = "";
  let open = false;
  let quote: string | null = null;
  for (let i = 0; i < line.length; i++) {
    const c = line[i];
    if (quote) {
      if (c === quote) quote = null;
      else word += c;
    } else if (c === "'" || c === '"') {
      quote = c;
      open = true;
    } else if (c === "\\" && i + 1 < line.length) {
      word += line[++i];
      open = true;
    } else if (/\s/.test(c)) {
      if (open || word) out.push(word);
      word = "";
      open = false;
    } else {
      word += c;
    }
  }
  if (open || word) out.push(word);
  return out;
}

export class Mwctl {
  constructor(private readonly module: WebAssembly.Module) {}

  /** Compiles the module served at `url`. */
  static async load(url: string): Promise<Mwctl> {
    const response = await fetch(url);
    if (!response.ok) throw new Error(`mwctl.wasm: HTTP ${response.status}`);
    return new Mwctl(await WebAssembly.compile(await response.arrayBuffer()));
  }

  /** Whether commands that wait for a board can run here. */
  get canWait(): boolean {
    return jspi() !== null;
  }

  /** Runs `mwctl <args>` to completion. */
  async run(args: string[], opts: RunOptions = {}): Promise<RunResult> {
    const files = opts.files ?? new Map<string, Uint8Array>();
    const now = opts.now ?? (() => performance.now());
    const sleep = opts.sleep ?? ((ms: number) => new Promise<void>((r) => setTimeout(r, ms)));
    const port = opts.port;
    const async = jspi();
    const enc = new TextEncoder();
    const out = { 1: new TextDecoder(), 2: new TextDecoder() };
    const result: RunResult = { code: 0, stdout: "", stderr: "" };
    const open = new Map<number, OpenFile>();
    let nextFd = ROOT_FD + 1;
    let stdinPos = 0;
    let memory!: WebAssembly.Memory;
    // The buffer is replaced when memory grows: never keep a view.
    const bytes = () => new Uint8Array(memory.buffer);
    const view = () => new DataView(memory.buffer);
    const argv = ["mwctl", ...args].map((a) => enc.encode(`${a}\0`));

    const emit = (fd: 1 | 2, chunk: Uint8Array) => {
      const text = out[fd].decode(chunk, { stream: true });
      if (fd === 1) {
        result.stdout += text;
        opts.onStdout?.(text);
      } else {
        result.stderr += text;
        opts.onStderr?.(text);
      }
    };

    /** Runs `f` over each buffer of an iovec array; stops when `f` moves
     * fewer bytes than the buffer holds. Returns the total. */
    const iovs = (ptr: number, n: number, f: (at: number, len: number) => number): number => {
      let total = 0;
      for (let i = 0; i < n; i++) {
        const at = view().getUint32(ptr + 8 * i, true);
        const len = view().getUint32(ptr + 8 * i + 4, true);
        const moved = f(at, len);
        total += moved;
        if (moved < len) break;
      }
      return total;
    };

    const wasi: Record<string, (...a: never[]) => unknown> = {
      args_sizes_get(argc: number, size: number) {
        view().setUint32(argc, argv.length, true);
        view().setUint32(size, argv.reduce((n, a) => n + a.length, 0), true);
        return OK;
      },
      args_get(table: number, buf: number) {
        for (const a of argv) {
          view().setUint32(table, buf, true);
          bytes().set(a, buf);
          table += 4;
          buf += a.length;
        }
        return OK;
      },
      environ_sizes_get(count: number, size: number) {
        view().setUint32(count, 0, true);
        view().setUint32(size, 0, true);
        return OK;
      },
      environ_get: () => OK,
      clock_time_get(id: number, _precision: bigint, at: number) {
        const ms = id === 0 ? Date.now() : now();
        view().setBigUint64(at, BigInt(Math.round(ms * 1e6)), true);
        return OK;
      },
      fd_prestat_get(fd: number, at: number) {
        if (fd !== ROOT_FD) return EBADF;
        view().setUint8(at, 0);
        view().setUint32(at + 4, 1, true);
        return OK;
      },
      fd_prestat_dir_name(fd: number, at: number, len: number) {
        if (fd !== ROOT_FD || len < 1) return EBADF;
        bytes()[at] = 0x2f;
        return OK;
      },
      path_open(
        dir: number,
        _dirflags: number,
        pathPtr: number,
        pathLen: number,
        oflags: number,
        _rights: bigint,
        _inheriting: bigint,
        _fdflags: number,
        fdOut: number,
      ) {
        if (dir !== ROOT_FD) return EBADF;
        const path = normalise(new TextDecoder().decode(bytes().slice(pathPtr, pathPtr + pathLen)));
        if (!path) return EINVAL;
        const exists = files.has(path);
        if (exists && oflags & O_CREAT && oflags & O_EXCL) return EEXIST;
        if (!exists && !(oflags & O_CREAT)) return ENOENT;
        if (!exists || oflags & O_TRUNC) files.set(path, new Uint8Array(0));
        open.set(nextFd, { path, pos: 0 });
        view().setUint32(fdOut, nextFd++, true);
        return OK;
      },
      fd_filestat_get(fd: number, at: number) {
        const f = open.get(fd);
        if (!f && fd > ROOT_FD) return EBADF;
        bytes().fill(0, at, at + 64);
        // Regular file, directory, or character device.
        view().setUint8(at + 16, f ? 4 : fd === ROOT_FD ? 3 : 2);
        view().setBigUint64(at + 24, 1n, true);
        view().setBigUint64(at + 32, BigInt(f ? (files.get(f.path)?.length ?? 0) : 0), true);
        return OK;
      },
      fd_read(fd: number, iov: number, n: number, nread: number) {
        const f = open.get(fd);
        if (fd !== 0 && !f) return EBADF;
        const data = f ? (files.get(f.path) ?? new Uint8Array(0)) : (opts.stdin ?? new Uint8Array(0));
        const total = iovs(iov, n, (at, len) => {
          const pos = f ? f.pos : stdinPos;
          const chunk = data.subarray(pos, pos + len);
          bytes().set(chunk, at);
          if (f) f.pos += chunk.length;
          else stdinPos += chunk.length;
          return chunk.length;
        });
        view().setUint32(nread, total, true);
        return OK;
      },
      fd_write(fd: number, iov: number, n: number, nwritten: number) {
        const f = open.get(fd);
        if (fd !== 1 && fd !== 2 && !f) return EBADF;
        const total = iovs(iov, n, (at, len) => {
          const chunk = bytes().slice(at, at + len);
          if (!f) {
            emit(fd as 1 | 2, chunk);
            return len;
          }
          const old = files.get(f.path) ?? new Uint8Array(0);
          const grown = new Uint8Array(Math.max(old.length, f.pos + len));
          grown.set(old);
          grown.set(chunk, f.pos);
          files.set(f.path, grown);
          f.pos += len;
          return len;
        });
        view().setUint32(nwritten, total, true);
        return OK;
      },
      fd_sync: () => OK,
      fd_close(fd: number) {
        open.delete(fd);
        return OK;
      },
      proc_exit(code: number) {
        throw new Exit(code);
      },
    };

    // A sleep: one clock subscription in, one event out.
    const pollOneoff = (subs: number, events: number, n: number, nevents: number): number | Promise<number> => {
      if (n < 1) return EINVAL;
      const userdata = view().getBigUint64(subs, true);
      const clock = view().getUint8(subs + 8) === 0;
      const timeout = clock ? Number(view().getBigUint64(subs + 24, true)) / 1e6 : 0;
      const absolute = clock && (view().getUint16(subs + 40, true) & 1) === 1;
      const ms = Math.max(0, absolute ? timeout - now() : timeout);
      const done = () => {
        bytes().fill(0, events, events + 32);
        view().setBigUint64(events, userdata, true);
        view().setUint32(nevents, 1, true);
        return OK;
      };
      return async ? sleep(ms).then(done) : done();
    };

    const noPort = -1;
    const lost = -2;
    const guard = async (f: () => Promise<number>): Promise<number> => {
      try {
        return await f();
      } catch {
        return lost;
      }
    };
    const portImports = {
      open: (ptr: number, len: number, baud: number) => {
        const path = new TextDecoder().decode(bytes().slice(ptr, ptr + len));
        return port ? guard(() => port.open(path, baud)) : Promise.resolve(noPort);
      },
      write: (ptr: number, len: number) => {
        const data = bytes().slice(ptr, ptr + len);
        return port ? guard(() => port.write(data)) : Promise.resolve(noPort);
      },
      read: (ptr: number, cap: number, timeoutMs: number) =>
        port
          ? guard(async () => {
              const got = await port.read(cap, timeoutMs);
              if (typeof got === "number") return got;
              const chunk = got.subarray(0, cap);
              bytes().set(chunk, ptr);
              return chunk.length;
            })
          : Promise.resolve(noPort),
      signals: (dtr: number, rts: number) =>
        port ? guard(() => port.signals(dtr !== 0, rts !== 0)) : Promise.resolve(noPort),
      close: () => (port ? port.close().catch(() => {}) : Promise.resolve()),
    };

    const waiting = (f: (...a: never[]) => unknown, without: unknown): WebAssembly.ImportValue =>
      async ? new async.Suspending(f) : (((): unknown => without) as WebAssembly.ImportValue);
    const imports: WebAssembly.Imports = {
      wasi_snapshot_preview1: {
        ...(wasi as Record<string, WebAssembly.ImportValue>),
        poll_oneoff: async ? new async.Suspending(pollOneoff as never) : (pollOneoff as WebAssembly.ImportValue),
      },
      mw_port: {
        open: waiting(portImports.open as never, noPort),
        write: waiting(portImports.write as never, noPort),
        read: waiting(portImports.read as never, noPort),
        signals: waiting(portImports.signals as never, noPort),
        close: waiting(portImports.close as never, undefined),
      },
    };

    const instance = await WebAssembly.instantiate(this.module, imports);
    memory = instance.exports.memory as WebAssembly.Memory;
    const start = instance.exports._start as () => unknown;
    try {
      if (async) await async.promising(start)();
      else start();
    } catch (e) {
      if (e instanceof Exit) result.code = e.code;
      else throw e;
    } finally {
      // mwctl closes its port when it drops it; an exit skips that.
      await port?.close().catch(() => {});
    }
    emit(1, new Uint8Array(0));
    emit(2, new Uint8Array(0));
    return result;
  }
}
