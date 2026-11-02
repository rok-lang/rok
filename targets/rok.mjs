// The JS host of a rok program built for wasm32-unknown-unknown:
// a browser or node gives it the few WASI preview 1 imports its
// runtime uses. stdout / stderr go to `write` (default: the console, a
// line at a time), clocks to `Date.now` / `performance.now`, randomness
// to `crypto.getRandomValues`; every other import fails with ENOSYS, so
// files, sockets and the rest are `IoErrorKind::Unsupported`.
//
// The module is a reactor: `main` runs the program, and the runtime
// stays up after it for the module's other exports, its
// `#[export_name("x")] extern "C" fn`s (numbers in and out, 64-bit
// integers as BigInt; other values through linear memory).
//
//   import { run, load } from "./rok.mjs";
//   const code = await run(fetch("app.wasm"), { args: ["app"] });
//
//   const app = await load(fetch("app.wasm"));
//   const code = app.main();
//   app.exports.add(2, 3);
//
//   node rok.mjs app.wasm [args..]     (the environment is node's)

const ENOSYS = 52;
const EBADF = 8;

class Exit {
  constructor(code) {
    this.code = code;
  }
}

function consoleWriter() {
  const dec = [new TextDecoder(), new TextDecoder()];
  const line = ["", ""];
  return (fd, bytes) => {
    const i = fd === 1 ? 0 : 1;
    line[i] += dec[i].decode(bytes, { stream: true });
    const parts = line[i].split("\n");
    line[i] = parts.pop();
    for (const p of parts) (i === 0 ? console.log : console.error)(p);
  };
}

// Block for `ms`: `Atomics.wait` where the thread may block (node, a
// worker), else spin (a browser's main thread cannot block).
function sleep(ms) {
  if (ms <= 0) return;
  try {
    Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, ms);
  } catch {
    const end = performance.now() + ms;
    while (performance.now() < end);
  }
}

/// Instantiate `wasm` (bytes, a `WebAssembly.Module`, or a `Response` /
/// promise of one) and run its `main`: the exit code.
export async function run(wasm, opts) {
  return (await load(wasm, opts)).main();
}

/// Instantiate `wasm` and initialize it: `{ exports, main }`. `main()`
/// runs the program: its exit code (an exit or a failure included). An
/// export that exits or traps throws.
export async function load(wasm, { args = ["main"], env = {}, write = consoleWriter() } = {}) {
  const enc = new TextEncoder();
  const argv = args.map((a) => enc.encode(a + "\0"));
  const envv = Object.entries(env).map(([k, v]) => enc.encode(`${k}=${v}\0`));
  let memory;
  const view = () => new DataView(memory.buffer);
  const bytes = (p, n) => new Uint8Array(memory.buffer, p, n);
  const strings = (list) => ({
    sizes(countPtr, sizePtr) {
      view().setUint32(countPtr, list.length, true);
      view().setUint32(sizePtr, list.reduce((n, s) => n + s.length, 0), true);
      return 0;
    },
    get(ptrs, buf) {
      for (const s of list) {
        view().setUint32(ptrs, buf, true);
        bytes(buf, s.length).set(s);
        ptrs += 4;
        buf += s.length;
      }
      return 0;
    },
  });
  const a = strings(argv);
  const e = strings(envv);
  const now = (id) =>
    id === 0 ? BigInt(Date.now()) * 1000000n : BigInt(Math.round(performance.now() * 1e6));
  const wasi = {
    args_sizes_get: a.sizes,
    args_get: a.get,
    environ_sizes_get: e.sizes,
    environ_get: e.get,
    clock_res_get(_id, out) {
      view().setBigUint64(out, 1000n, true);
      return 0;
    },
    clock_time_get(id, _precision, out) {
      view().setBigUint64(out, now(id), true);
      return 0;
    },
    random_get(buf, len) {
      for (let off = 0; off < len; off += 65536) {
        crypto.getRandomValues(bytes(buf + off, Math.min(65536, len - off)));
      }
      return 0;
    },
    fd_write(fd, iovs, n, written) {
      if (fd !== 1 && fd !== 2) return EBADF;
      let total = 0;
      for (let i = 0; i < n; i++) {
        const p = view().getUint32(iovs + 8 * i, true);
        const len = view().getUint32(iovs + 8 * i + 4, true);
        write(fd, bytes(p, len).slice());
        total += len;
      }
      view().setUint32(written, total, true);
      return 0;
    },
    // No stdin: at its end at once.
    fd_read(fd, _iovs, _n, read) {
      if (fd !== 0) return EBADF;
      view().setUint32(read, 0, true);
      return 0;
    },
    // stdin / stdout / stderr are terminals (a character device without
    // seek or tell), so wasi-libc's isatty holds and the runtime flushes
    // stdout at each newline: a page shows a line as it is printed, and a
    // program stopped from outside keeps what it printed.
    fd_fdstat_get(fd, buf) {
      if (fd > 2) return ENOSYS;
      const v = view();
      v.setUint8(buf, 2);
      v.setUint16(buf + 2, 0, true);
      v.setBigUint64(buf + 8, 0n, true);
      v.setBigUint64(buf + 16, 0n, true);
      return 0;
    },
    // One pre-opened directory, `/`, whose every call fails with ENOSYS:
    // file system calls report Unsupported rather than "no such file".
    fd_prestat_get(fd, buf) {
      if (fd !== 3) return EBADF;
      view().setUint8(buf, 0);
      view().setUint32(buf + 4, 1, true);
      return 0;
    },
    fd_prestat_dir_name(fd, path, len) {
      if (fd !== 3 || len < 1) return EBADF;
      view().setUint8(path, 0x2f);
      return 0;
    },
    proc_exit(code) {
      throw new Exit(code);
    },
    sched_yield: () => 0,
    // Clock subscriptions only: sleep to the earliest, report it.
    poll_oneoff(subs, events, n, nevents) {
      let best = -1;
      let wake = Infinity;
      const t = Number(now(1)) / 1e6;
      for (let i = 0; i < n; i++) {
        const s = subs + 48 * i;
        if (view().getUint8(s + 8) !== 0) return ENOSYS;
        const id = view().getUint32(s + 16, true);
        const timeout = Number(view().getBigUint64(s + 24, true)) / 1e6;
        const abs = view().getUint16(s + 40, true) & 1;
        const at = abs ? timeout - (Number(now(id)) / 1e6 - t) : t + timeout;
        if (at < wake) [best, wake] = [i, at];
      }
      if (best < 0) return ENOSYS;
      sleep(wake - t);
      const ev = new Uint8Array(memory.buffer, events, 32);
      ev.fill(0);
      view().setBigUint64(events, view().getBigUint64(subs + 48 * best, true), true);
      view().setUint32(nevents, 1, true);
      return 0;
    },
  };
  const imports = {
    wasi_snapshot_preview1: new Proxy(wasi, {
      get: (target, name) => target[name] ?? (() => ENOSYS),
    }),
  };
  const source = await wasm;
  const module =
    source instanceof WebAssembly.Module
      ? source
      : typeof Response !== "undefined" && source instanceof Response
        ? await WebAssembly.compileStreaming(source)
        : await WebAssembly.compile(source);
  const instance = await WebAssembly.instantiate(module, imports);
  memory = instance.exports.memory;
  const fail = (msg) => {
    write(2, enc.encode(msg));
    return 134;
  };
  // stdout is the runtime's buffer (and C's, when the program uses
  // it), flushed after each call into the module.
  const flushed =
    (f) =>
    (...args) => {
      try {
        return f(...args);
      } finally {
        instance.exports.rok_stdio_flush?.();
        instance.exports.fflush?.(0);
      }
    };
  const exports = {};
  for (const [name, v] of Object.entries(instance.exports)) {
    exports[name] = typeof v === "function" ? flushed(v) : v;
  }
  const main = () => {
    try {
      return exports.main();
    } catch (err) {
      if (err instanceof Exit) return err.code;
      // The engine's call stack (runtime error: stack overflow).
      if (err instanceof RangeError) {
        return fail("\nthread 'main' has overflowed its stack\nfatal runtime error: stack overflow, aborting\n");
      }
      if (err instanceof WebAssembly.RuntimeError) return fail(`wasm trap: ${err.message}\n`);
      throw err;
    }
  };
  instance.exports._initialize();
  return { exports, main };
}

if (typeof process !== "undefined" && process.argv?.[1]) {
  const { fileURLToPath } = await import("node:url");
  const { resolve } = await import("node:path");
  if (fileURLToPath(import.meta.url) === resolve(process.argv[1])) {
    const { readFileSync, writeSync } = await import("node:fs");
    const [file, ...rest] = process.argv.slice(2);
    const env = Object.fromEntries(Object.entries(process.env));
    const write = (fd, b) => {
      for (let off = 0; off < b.length; ) off += writeSync(fd, b, off);
    };
    process.exitCode = await run(readFileSync(file), { args: [file, ...rest], env, write });
  }
}
