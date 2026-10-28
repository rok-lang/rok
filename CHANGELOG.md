# Changelog

## 0.1.3 (2026-10-29)

- `rok::core::encoding`: `hex_encode`, `hex_decode`, `base64_encode`, `base64_decode`;
  standard or url-safe, padded or not; a decode error names the byte and the fix.
- `str` gains `splitn`, `rsplit`, `rsplitn`, `match_indices`, `trim_matches`,
  `trim_start_matches` and `trim_end_matches`, all sharing the source's bytes.
- Iterator adapters `take_while`, `skip_while`, `scan`, `inspect`, `cycle`, `chunks`,
  `windows`; consumers `try_fold`, `try_for_each`, `partition`, `unzip`.
- `List` gains `contains`, `retain`, `dedup`, `fill`, `swap`, `split_at`, `binary_search`,
  `windows`, `chunks`; `Slice` gains `contains`, `split_at`, `windows`, `chunks`; `retain` and
  `extend` on `Dict`, `Set`, `OrderedDict`, `OrderedSet`; arrays gain `iter`, `first`, `last`,
  `contains`, `map`, `fill`.
- Type parameters inside an associated-type binding are inferred, also from a `where`:
  `Iterator<Item = (A, B)>` binds `A` and `B`.
- `Writer::write_display` writes a value through `Display` in chunks, with no intermediate
  `str`.
- `Inspect` gains `fields`, `variant_names`, `variant_name`, `field_names_of`, with `FieldInfo`
  and `fields_of`; `get_field` reads an enum value's payload; a missing derive says to add
  `#[derive(Inspect)]`.
- `int` gains `count_ones`, `trailing_zeros`, `bit`, `set_bit`, `is_power_of_two`,
  `next_power_of_two`, `ilog2`, `checked_ilog2` and little- and big-endian bytes; `f64` and
  `f32` gain `lerp`, `next_after`, `next_up`, `next_down`.
- `i128` and `u128` are an error that names `int` or the 64-bit type to write.
- `#[doc(hidden)]` keeps an item out of completion and `rokc doc`; the compiler hooks carry it.
- A closure that captures a `ref` parameter writes back to the caller; `[ref self]` captures
  in a `ref self` method.
- `Self::f()` in a trait default works for a private type.
- A binding in a meta template renames only within its own scope.

## 0.1.2 (2026-10-28)

- Dict spread: a defaulted parameter the dict's value type cannot fill takes its default;
  a key naming it panics.
- Clearer errors: an `if` in a typed slot names the branch that does not fit; a bare `break`
  mixed with `break <value>`, misordered fn modifiers and `mod x;` without a folder say what
  to write.
- One item imported through a re-export and directly is no longer a conflict; a real clash
  suggests an alias.
- An unimported type in an annotation is one error, without its cascade.
- A static initialized from itself through other statics is a compile error.
- A meta attribute named like a built-in (`bench`, `path`, ...) warns; it becomes an error
  in 0.2.0.
- Collections: `BinaryHeap` gains `IntoIterator`, `Debug`, `Clone`, `Default`, `FromIterator`
  and an in-place `retain`; `clear` on `Dict`, `Set`, `OrderedSet`; `+=` on `Set`,
  `OrderedSet`, `Deque`, `OrderedDict`; `Deque` gains `Clone`, `PartialEq`, `Eq`,
  `FromIterator`; `OrderedDict` and `OrderedSet` gain `Eq` and `Hash`; `Dict::from` and
  `Set::from` through `From`.
- `Iterator::exact_len`; `count()` on exact-size adapters no longer runs the pipeline.
- `ManyPtr::cast`; `ClientConfig::try_new` throws when the system roots cannot be read.
- Package metadata in `rok.rokon`: `authors`, `homepage`, `documentation`, `readme`,
  `keywords`, `publish`; `publish: false` refuses `rok publish`.
- An unreadable `config.rokon` (permissions, not UTF-8) is an error, no longer read as empty
  or overwritten by `rok toolchain default`.
- The install scripts and `rok toolchain update` print rok's status lines; `-q` and `-v`.
- The language server refreshes documents after a dependency's buffer changes.
- The build cache rebuilds when an `include_str!` input disappears.
- Install through Nix: `nix profile install github:rok-lang/rok`.
- A read-only toolchain (Nix, system packages) keeps its build cache in the user's cache
  directory.

## 0.1.1 (2026-10-27)

- `print` and `println` accept any `Display` value, and default to printing a blank line.
- The REPL prints a result through `Display`; a trailing `?` prints it through `Debug`.
- Refreshing an LLVM prefix no longer rewrites the MLIR plugin's lib directory.

## 0.1.0 (2026-10-25)

First release.

- Compiler `rokc` (AOT via MLIR / LLVM 22, REPL, Jupyter kernel, language server, `fmt`, `lint`, `doc`, `test`,
  `bench`, `bindgen`), package / build front-end `rok` (`rok.rokon` / `rok.lock`, path / git dependencies, stones,
  environments, toolchains), runtime `librokr.a`, standard library `core` / `std` / `bare`.
- Install: rustup-style layout (`~/.rok/bin` proxies, `~/.rok/toolchains/<version>-<triple>/`), `install.sh` /
  `install.ps1`, and `rok-<version>-<triple>.tar.gz` / `.tar.xz` (Windows `.zip`) archives built by `dist.sh`
  (Linux: `dist-linux.sh`, in a Debian 11 container; they need glibc 2.34 or later).
- `rok` / `rokc` / `rokls --version` print the version, commit, and date.
- License `MIT OR Apache-2.0`.

### Language

- Using a value after it was handed off (`Mutex::new`, `send`, `task::spawn`, `Handoff::new`, through calls)
  is a compile error; the check follows conditions, call outcomes, a collection's elements, copies and aliased
  arguments, and each error says what to write instead.
- A hand-off gives the receiver its own copy of every collection the value reaches (a list and its views keep
  one buffer; values built at compile time are not copied), so a route the check misses cannot race. A
  coroutine future gets a frame of its own; one that has started cannot be handed over.
- A lock guard and its value stay inside their `using` block: returning, storing, yielding or capturing them
  (closures and coroutines included) is an error.
- `Frozen<T>` shares a value read-only between threads without `unsafe`; `get` gives each reader a copy and
  keeps `Sync` parts shared.
- `par` runs items that reach one collection or future one at a time, with a warning naming the call.
- `Weak<T>` is not `Send`; `any Tr` crosses threads only when its bound has `Send + Sync`.
- Every `await` of a `Task` that was handed on gets its own copy of the result.
- Duplicate items point at both definitions; a tuple or unit struct's constructor shares the value namespace
  with functions, consts and statics. A named `use` and a local item of the same name are an error; a glob
  import gives way.
- Names from another module need a `use`; "cannot find" errors suggest the `use` to add or the missing `pub`.
- Generic argument counts are checked; a bare generic type says it needs arguments.
- Generic unions; impls on generic type aliases; an alias's `where` is checked in declaration types.
- Block-local items (`struct`, `enum`, `impl`, `use`, `const` in a block), including module glob `use`.
- Statics and consts of `fn` type are callable. `Sync` rules for statics, `RwLock` and `[ref]` cells.
- `#[cold]` keeps a function out of line.
- A `try!` panic shows the error and where it was raised.

### Tools

- Scripts: `rok <file>`, shebang lines, dependencies from the file's `//! rokon` block (`rok add <lib> <file>`),
  `mod helper;` across script files; `import` gets an error naming `mod` / `use`.
- Packages with both `src/lib.rok` and `src/main.rok`; `rok install <stone>` installs executables, keeps one
  installed at the same version and rebuilds with `--force`; `rok install <lib>` serves the REPL and Jupyter.
- The package name `rok` is reserved for the standard library; names code can't write as a path (`pkg`,
  `sup`, keywords) are refused with a suggestion. Every `rok new` template is tested end to end.
- Temporary files and directories are removed on exit, error and panic; leftovers of killed processes are swept,
  and a REPL host ends with its `rokc`.
- Build profiles: debug info by default; release folds identical code and strips.
- Lints in every compiling command; hints for habits from other languages.

### Platforms

- Verified natively: macOS arm64 (macOS 26 and 27 SDKs), Linux x86_64 and aarch64 (Ubuntu 26.04), Windows
  x86_64 (Windows 11). Windows arm64 links but has not been run.
- macOS 27: rokc's linker reads the new SDK's stubs (`arm64e.x1`).
- Windows: libclang (`rokc bindgen`), a PGO profile for the clang-cl LLVM build, `dist.sh` and
  `install.ps1 -NoModifyPath`; CI builds the prefix with clang-cl, runs `bindgen`, and a nightly job installs
  the distribution and builds a package with it.
- `riscv64gc-unknown-linux-gnu`, verified on a riscv64 kernel.

### Performance

- Bounds checks of counted loops over lists move in front of the loop; short loops and loop nests run without
  safepoint polls; a fixed-index element stays in a register.
- Calls between rok functions pass the thread block as an argument; the GC frame links only on paths that reach
  a safepoint; locals that only a loop's poll sees are rooted on its yield path only.
- `task::spawn` of a future made at the call, or kept in a local nothing else names, skips the hand-off.
- PGO profiles retrained for macOS 27 and Linux: rokc's own `-O2` builds are 5-10% faster than without.
- rok / C instructions on the bench suite: nbody 1.46x, fannkuch 1.64x, spectral_norm 1.11x, throw_parse 1.85x;
  json_tokenizer 0.69x, word_count 0.89x, binary_trees 0.55x.
