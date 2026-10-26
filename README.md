<p align="center">
  <img src="assets/rok.svg" alt="rok" width="96" height="96">
</p>

# Rok

The implementation of the Rok language: the compiler `rokc`, the package / build
front end `rok`, the language server `rokls`, the runtime `librokr.a`, and the
standard library (`library/{core,std,bare}`).

- Site: https://rok-lang.org
- Docs (guide, reference, standard library API): https://docs.rok-lang.org
- Language specification: https://github.com/rok-lang/spec

## Install

```sh
curl -fsSL https://rok-lang.org/install.sh | sh
```

```powershell
powershell -ExecutionPolicy Bypass -c "irm https://rok-lang.org/install.ps1 | iex"
```

The [Download page](https://rok-lang.org/download.html) has the release archives,
their checksums, and the cross-compilation targets. No LLVM install is needed: a
toolchain links its own LLVM and ships every target's runtime, so
`--target <triple>` cross-compiles from any host.

## Quick start

```sh
rok new hello        # hello/{src/main.rok, rok.rokon, .gitignore} + git init
cd hello
rok run              # Hello, world!
rok repl .           # a REPL session with the package's bindings
rok test
rok jupyter install  # Rok kernel for Jupyter / VS Code
```

## Building from source

Building needs Rust, plus a C compiler and linker for the runtime step below.

```sh
cargo build --release                                     # the compiler: target/release/{rok,rokc,rokls}
targets/build.sh "$(rustc -vV | sed -n 's/^host: //p')"   # the host runtime, so rokc can build programs
```

`cargo build` produces only the compiler binaries. Before `rokc` can compile or
run a program it needs that target's runtime (`librokr.a` and the target's C
library) under `target/lib/rok/targets/<triple>/`, which `targets/build.sh
<triple>` builds; without it `rokc` reports `no runtime for <triple>`. Build the
host triple to run programs locally, and any other shipped triple to
cross-compile to it (no triple builds every one the host can).

For anything above `-O0`, `rokc` also needs the pinned LLVM: `llvm/build.sh`
builds it once into `~/.cache/rok` (see `llvm/README.md`); without it `rokc` uses
its native `-O0` backend.

Checks:

```sh
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
ROK_REQUIRE_LLVM=1 cargo test --no-fail-fast
./target/debug/rokc fmt --check library
```

Packaging (`dist-*.sh`), the cross-target and wasm corpus, and the CI tiers are
documented in [docs/ci.md](docs/ci.md).

## License

`MIT OR Apache-2.0` ([LICENSE-MIT](LICENSE-MIT), [LICENSE-APACHE](LICENSE-APACHE)).
The specification repository is CC-BY-4.0.
