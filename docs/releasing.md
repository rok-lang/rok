# Releasing

How a `rok` release is cut. A release ships six host archives; `release.yml`
builds them all on GitHub-hosted runners and cuts the GitHub release on a
tag push. Nothing runs on its own: a release happens only when a
`v<version>` tag is pushed. A manual `workflow_dispatch` run builds and
verifies the archives without publishing.

## Versioning and cadence

rok shifts semantic versioning down one level while the major version is 0,
the way Cargo treats 0.x crates: the minor number is the breaking slot and
the patch number carries features and fixes. A minor bump (`0.2.0`) is for
breaking changes; a patch bump (`0.1.1`, `0.1.2`, ...) adds
backward-compatible features and fixes. 1.0 waits until the language and
standard library are meant to be stable.

Releases are cut on demand, when a version's work is ready; there is no
fixed schedule. A release is cut from `main` and tagged `v<version>`.
There is no long-lived release branch: a maintenance branch (for example
`0.1`) is made from a release tag only if a patch for an older minor is
needed after `main` has moved on. A version may first go out as a release
candidate (`v<version>-rc.N`) to exercise the pipeline before the final
tag.

## What a tag push does

Pushing a `v<version>` tag runs `release.yml`:

1. `build`, a matrix of six in parallel:
   - macOS and Windows build natively with `dist-native.sh`, taking the
     pinned LLVM and libclang prefixes from the `rok-lang-artifacts` S3
     bucket (the `llvm-prefix` / `libclang-prefix` actions).
   - the four Linux archives build in a container: `dist-linux-gnu.sh`
     (glibc 2.34) and `dist-linux-musl.sh` (static musl), one x86_64 and
     one arm64 runner building two each. The container's LLVM is
     downloaded from S3 (`dist-llvm/<version>/<triple>/`), not built from
     source.
   - each uploads its `rok-<version>-<triple>` archives (`.tar.gz` +
     `.tar.xz`; Windows a `.zip`), the `rok-mlir-dev-<version>-<triple>`
     bundle (none for static musl), a `.sha256` beside each, and a
     `rok-version` file.
2. `publish`: gathers every archive, runs `release.sh --dir dist` to
   verify the full six-triple set and its checksums, and on a tag runs
   `release.sh --dir dist --publish` to create the `v<version>` GitHub
   release and upload every asset with the `gh` CLI. `install.sh` /
   `install.ps1` download `v<version>` by default; `rok-version` is served
   from `releases/latest/`.

A `workflow_dispatch` run stops after the verify; it never publishes.

## Before the tag

- The prefixes for this `<version>` must already be on S3: the native
  prefixes and the container `dist-llvm` prefixes (`prefixes.yml`; the
  Windows LLVM via `prefixes-windows.yml`). They are rebuilt only when the
  LLVM version or a build script changes, not for every release.
- The version must agree across `Cargo.toml` (the source of truth, baked
  into `rokc --version`), `install.sh` and `install.ps1`; `dist-native.sh`
  fails the build if they disagree.
- `CHANGELOG.md` has a dated `## <version>` section.
- The stones, the sites and CI version and deploy on their own terms; this
  file covers only the toolchain release.

## Building an archive by hand

The same scripts build one host's archives locally, for a spot check or a
host CI cannot build:

| Host | Machine | Command |
|---|---|---|
| `aarch64-apple-darwin` | an Apple-silicon Mac | `./dist-native.sh --out dist` |
| `x86_64-unknown-linux-gnu` | x86_64 Linux | `./dist-linux-gnu.sh --out dist` |
| `aarch64-unknown-linux-gnu` | arm64 Linux | `./dist-linux-gnu.sh --out dist` |
| `x86_64-pc-windows-msvc` | Windows x86_64 | `./dist-native.sh --out dist` (Git Bash, MSVC environment) |
| `x86_64-unknown-linux-musl` | x86_64 Linux | `./dist-linux-musl.sh --out dist` (Alpine container; static, no libclang / dev bundle) |
| `aarch64-unknown-linux-musl` | arm64 Linux | `./dist-linux-musl.sh --out dist` |

`dist-linux-gnu.sh` and `dist-linux-musl.sh` run in a container, so one
x86_64 and one arm64 Linux box build four of the six. Intel Macs and
Windows on arm64 get no archive; they build from source. Gather the output
of every host into one `dist/` directory and check it with:

```sh
./release.sh --dir dist
```

It verifies that all six triples' archives and checksums are present and
that every `.sha256` matches, and changes nothing without `--publish`.
