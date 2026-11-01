# Pinned LLVM

rokc links LLVM, MLIR and lld statically (`compiler/rokc_llvm`) and runs
the whole native back end in process: MLIR passes, translation to LLVM IR,
the LLVM optimization pipeline, object emission (split across cores for a
big module), the link (lld) and, for `-g` on macOS, dsymutil. Users do not
install LLVM; macOS needs the SDK of Xcode's command line tools.

The pin is **LLVM 22.1.8** (`VERSION` in `build.sh`, `LLVM_VERSION` in
`compiler/rokc_llvm`). The C++ shim uses the MLIR / LLVM / lld C++ APIs,
which change between majors, so exactly one version is supported; 22.1.8
is the last release of the newest major the emitted MLIR is tested with.

`build.sh` applies `patches/*.patch` to the source:

- `arm64e-x1.patch`: upstream b8007a8e4020 (in LLVM 23.1), library
  sources only. The macOS 27 SDK's `.tbd` files list `arm64e.x1-macos`,
  which 22.1.8's TextAPI rejects ("malformed file ... unknown
  architecture"), so ld64.lld found no libSystem symbols and every
  macOS link failed. On Apple hosts rokc_llvm only takes a prefix with
  the patch (`build.sh` rebuilds an older one; `build.ps1` does not apply
  it, as Windows hosts don't link Apple targets).

## Building it

```sh
llvm/build.sh            # -> ~/.cache/rok/llvm-22.1.8 (ROK_LLVM_CACHE)
llvm/build.sh <prefix>   # anywhere else
```

Hosts:

- macOS (arm64, x86_64): Xcode's command line tools, cmake, ninja.
- Linux (x86_64, aarch64; glibc or musl): cmake, ninja, curl, xz, patch
  and a C++17 compiler (`apt install cmake ninja-build g++ curl xz-utils
  patch`; Alpine: `apk add cmake samurai g++ curl xz patch
  linux-headers`; NixOS or
  any nix host: `nix-shell` at the repository root, whose `shell.nix`
  has these with clang, which reads the PGO profile). rokc_llvm
  links the C++ runtime the archives were built against: libstdc++, or
  libc++ when they name `std::__1`.
- Windows (MSVC ABI; x86_64 verified, arm64 not yet run): `llvm\build.ps1
  [prefix]` from a Developer PowerShell for VS (the MSVC C++ build tools,
  cmake, ninja, Python 3) installs into
  `%LOCALAPPDATA%\rok\llvm-22.1.8`. The same libraries as `.lib` with
  the dynamic CRT (`/MD`, as rustc links), no DIA SDK. rokc_llvm compiles
  its shim with `cl` (or `CXX`, e.g. `clang-cl`) and archives it with
  `lib`. Only building rok needs these: the toolchain links Windows
  programs with its own lld and import libraries.

`cargo check --target <another triple>` builds the stub (below) when the
prefix is this machine's (`LLVM_HOST_TRIPLE` names another architecture
or OS).

A Release build of static libraries only: targets AArch64, X86, ARM,
RISCV and WebAssembly; the MLIR and lld (Mach-O, ELF, COFF, MinGW, wasm)
libraries; no tests, tools (but lld's own binary), examples, docs, bindings, zlib, zstd or
libxml2. dsymutil is a tool, not a library: its sources and generated
option table are kept in `share/rok/dsymutil` and compiled into rokc. The
source tarball is checked against a pinned SHA-256; the source and build
trees are deleted after the install.

On a 10-core Apple Silicon machine: about 50 minutes; 700 MB installed.

### PGO

`build.sh` optimizes LLVM with `rok.profdata.xz`, an IR profile
(sparse, xz) of rokc's own `-O2` builds, whenever the C++ compiler can
read it: it tries (`-fprofile-instr-use` on an empty unit) and says when
it builds without. It merges a macOS (arm64) and a Linux (x86_64)
training run: the two compile LLVM differently (libc++ / libstdc++,
host `#if`s), so a third of the functions profiled on one host have
another control-flow hash, or do not exist, on the other. Records are
keyed by name and hash, so each host's clang uses its own run's; a
function both share gets the sum. Names are Itanium-mangled: the one
file serves macOS (Xcode's clang) and Linux (a clang at least as new as
the profile's indexed format: version 12 now, written by Xcode's
llvm-profdata). gcc reads no clang profile: a Linux build with gcc
(the default `c++` on most distributions) has no PGO; `CXX=clang++
CC=clang` gives it. Nothing extra is fetched either way.

An MSVC-ABI build names functions by MSVC mangling, which that profile
does not have. `build.ps1` reads `rok-msvc.profdata.tar.xz` (2 MB, made
by `train-pgo.ps1` on a Windows x86_64 host) when built with clang-cl
(`CXX=clang-cl`, 22 or newer); `cl` reads no clang profile and builds
without PGO. On an 8-core host, rokc's `-O2` builds of `bench/` as
objects take 6.97 s with the profile, 7.83 s with clang-cl alone (-11%)
and 8.92 s with cl (-22%); the cases training left out, built and linked
in turn, 66.1 s, 68.7 s and 71.1 s.

`llvm/train-pgo.sh [profile...]` trains on the host (macOS, Xcode's
clang; or Linux, clang and its llvm-profdata): an instrumented LLVM
(`ROK_LLVM_INSTRUMENT=1 build.sh`), a release rokc against it, linked
with the profile runtime, then three quarters of
`compiler/rokc/tests/cases` built with it at `-O2` (a third of them as
Linux objects of the other architecture, so both main back ends are
profiled). It keeps the host's profile as
`$ROK_LLVM_CACHE/rok-<os>.profdata` and writes `rok.profdata.xz` from
it merged with the profiles given. Regenerating takes both hosts, Linux
first, so that the merge runs with Xcode's llvm-profdata, whose format
every reader of the profile accepts:

```sh
# Linux x86_64 (NixOS: in nix-shell), from the same commit:
targets/build.sh x86_64-unknown-linux-gnu
llvm/train-pgo.sh        # -> ~/.cache/rok/rok-linux.profdata
# macOS arm64, with that file copied over:
llvm/train-pgo.sh rok-linux.profdata
```

Against a profile of the host alone (bench/ at `-O2`, instructions),
the merged one costs 0.1% on Linux and 0.5% on macOS, where the other
host's alone costs 2.8% and 0.9%; the merged file is 4.1 MB.
`train-pgo.sh` peaks at ~5 GB of scratch space and takes about 20
minutes on a 12-core Apple Silicon machine (55 on a 16-thread x86_64
laptop with `ROK_LLVM_JOBS=12`).

A new Xcode clang changes the hashes of many functions: Apple clang 21
(macOS 27) found 2,742 mismatches in the profile before it. The current
one merges a macOS 27 run and an Ubuntu 26.04 x86_64 run (clang 21),
with `xcrun llvm-profdata merge --sparse`. Building LLVM with it, Apple
clang still reports 430 hash mismatches and Ubuntu's clang 189:
functions only the other host's run reached, whose control flow differs
on this one. On a 12-core Apple Silicon machine rokc's `-O2` builds of
`bench/` as objects take 3.49 s with the profile and 3.75 s without PGO
(-7%); the 298 cases training left out, as objects, 56.5 s and 59.6 s
(-5%). On an 8-core x86_64 VM (Ubuntu 26.04, both prefixes built by
clang 21), 6.43 s and 7.15 s (-10%), 103.0 s and 114.2 s (-10%).

On Ubuntu, training needs clang, `llvm-profdata` (package `llvm-21`,
whose binary is `llvm-profdata-21`: put it on PATH as `llvm-profdata`)
and the profile runtime (`libclang-rt-21-dev`); `CC=clang CXX=clang++`.

`llvm\train-pgo.ps1` does the same on Windows with clang-cl and the
llvm-profdata of its installation (the third as objects for the other
Windows architecture) into `rok-msvc.profdata.tar.xz`: 26 minutes for
the instrumented LLVM and 5.5 for the rest on 8 cores. Regenerate after a change of the pinned LLVM or
a large change of what rokc asks LLVM to do; a stale profile still
builds, with less benefit.

## Using it

`compiler/rokc_llvm/build.rs` takes `ROK_LLVM_PREFIX`, else the default
prefix of `build.sh`. Without either, rokc_llvm builds a stub whose entry
points return an error: that rokc builds `-O0` with the native backend
only and links with the system's `cc`, reports `llvm: none` in
`rokc version -v`, and the tests that need LLVM skip (fail under
`ROK_REQUIRE_LLVM=1`). A `ROK_LLVM_PREFIX` that does
not hold this LLVM with MLIR, lld and the dsymutil sources fails the
build. `dist-native.sh` runs `build.sh` when no prefix is given.

rokc registers only the MLIR dialects and passes it lowers with
(`register_dialects` / `register_passes` in `src/shim.cpp`), so the
linker leaves the rest of MLIR out. rokc exports no symbols; everything
it does not reach is dead-stripped.

Homebrew's `llvm` (22.1.8) has static LLVM and MLIR archives but ships lld
only as shared libraries, so it cannot serve as the prefix; release and
development builds both use `build.sh`.

## libclang (`rokc bindgen`)

`rokc bindgen` reads C headers through libclang's C API, which it loads at
run time (`dlopen` / `LoadLibraryW`): rokc does not link it, and no other
command needs it. It comes from the same source tarball, built apart from
the prefix above so that prefix (and its CI cache key) stays as it is:

```sh
llvm/build-libclang.sh            # -> ~/.cache/rok/libclang-22.1.8 (ROK_LLVM_CACHE)
llvm\build-libclang.ps1           # Windows (MSVC) -> %LOCALAPPDATA%\rok\libclang-22.1.8
```

It builds only the `libclang` shared library and `clang-resource-headers`:
Release, no LLVM back end (`LLVM_TARGETS_TO_BUILD` empty: clang's target
info for `-target` is its own), no tools, tests or docs. The prefix holds
`lib/libclang.{dylib,so,dll}` and `lib/clang/22/include` (clang's built-in
headers, passed as `-resource-dir`); the build tree is deleted at the end.
On a 10-core Apple Silicon machine: 5.5 minutes, a build tree of ~2 GB at
its peak, 36 MB installed (libclang.dylib 29 MB, headers 7.7 MB; 12 MB
compressed). On an 8-core Windows host: 16 minutes, 30 MB.

rokc looks for it in its toolchain's `lib/` (`dist-native.sh` copies it there,
from `ROK_LIBCLANG_PREFIX` or this script's prefix), else, for a build in
the repository, in this script's prefix. Without one, `rokc bindgen` says
how to get it and the bindgen tests skip (fail under
`ROK_REQUIRE_LIBCLANG=1`).
