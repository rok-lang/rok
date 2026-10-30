# Contributing to Rok

Thanks for your interest in improving Rok. This repository holds the compiler
(`rokc`), the toolchain front end (`rok`), the runtime, and the standard
library.

## Building

You need the Rust toolchain pinned in `rust-toolchain.toml` and a C compiler.
Run `cargo build`, then `targets/build.sh <host triple>` for the runtime, as
the README's "Building from source" shows. Builds above `-O0` also need the
pinned LLVM (`llvm/README.md`).

## Checks

Run the gates CI runs. `just` lists every task.

```sh
just fmt-check     # cargo fmt + shfmt
just lint          # clippy -D warnings, shellcheck, PSScriptAnalyzer
cargo test         # compiler and runtime tests
"${CARGO_TARGET_DIR:-target}/debug/rokc" fmt --check library   # the stdlib
```

Format before sending a change (`just fmt`).

## Changes that affect behavior

The language and standard library are defined by the spec (`rok-lang/spec`,
English and Korean editions). A change in observable behavior updates the spec
in both editions in the same change.

## Comments and style

Comments explain *why*, in the terse style of the surrounding code: no banners
or decorative headers, ASCII only. Match the conventions of the file you edit.

## Submitting

Open a pull request against `main` with the checks above passing. Leave
`CHANGELOG.md` alone: it is written at release time from the merged pull
requests. By contributing you agree that your work is licensed under both MIT
and Apache-2.0, the project's dual license.
