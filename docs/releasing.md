# Releasing

How a `rok` release is cut. 0.1.0 ships six host archives. No CI job
builds or uploads them, so they are built by hand on each host, gathered,
verified with `release.sh`, and uploaded together. Nothing here runs on
its own; a release happens only when it is meant to.

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
needed after `main` has moved on.

A new version may first go out as a release candidate (`v<version>-rc.N`)
to exercise the pipeline before the final tag; when the product and the
pipeline are already verified, the final tag can be cut directly. The
stones, the sites and CI version and deploy on their own terms; this file
covers only cutting the toolchain release.

## Host archives

| Host | Machine | Command |
|---|---|---|
| `aarch64-apple-darwin` | an Apple-silicon Mac | `./dist-native.sh --out dist` |
| `x86_64-unknown-linux-gnu` | x86_64 Linux | `./dist-linux-gnu.sh --out dist` |
| `aarch64-unknown-linux-gnu` | arm64 Linux | `./dist-linux-gnu.sh --out dist` |
| `x86_64-pc-windows-msvc` | Windows x86_64 | `./dist-native.sh --out dist` (Git Bash, MSVC environment) |
| `x86_64-unknown-linux-musl` | x86_64 Linux | `./dist-linux-musl.sh --out dist` (Alpine container; static, no libclang / dev bundle) |
| `aarch64-unknown-linux-musl` | arm64 Linux | `./dist-linux-musl.sh --out dist` |

`dist-linux-gnu.sh` (glibc 2.34) and `dist-linux-musl.sh` run in a container, so one
x86_64 and one arm64 Linux box build four of the six. Intel Macs and
Windows on arm64 get no archive; they build from source.

Each `dist*.sh` writes, per triple: `rok-<version>-<triple>` as `.tar.gz`
and `.tar.xz` (Windows: one `.zip`), the `rok-mlir-dev-<version>-<triple>`
bundle the same way (none for the static musl builds), a `.sha256` beside
every archive, and a `rok-version` file. Gather all of them, from every
host, into one `dist/` directory.

## Verify

```sh
./release.sh --dir dist
```

It checks that all six triples' archives and their checksums are present
and that every `.sha256` verifies, and exits non-zero if any is missing,
so the release cannot go out with a dead download link. It changes
nothing without `--publish`.

## Cut the release

1. In `CHANGELOG.md`, change `## 0.1.0 (unreleased)` to `## 0.1.0
   (<date>)`; commit on `main`.
2. Confirm the version matches across `rok`/`rokc` (`--version`),
   `install.sh`, `install.ps1` and `rok-lang.org` `site.rokon`. `dist-native.sh`
   already fails if the install scripts disagree.
3. Publish (this is the release itself):

   ```sh
   ./release.sh --dir dist --publish
   ```

   It tags the release commit `v<version>` and uploads every archive,
   checksum and `rok-version` with the `gh` CLI. `install.sh` /
   `install.ps1` download `v<version>` by default, and `rok-version` is
   served from `releases/latest/`.

## Stones

Publish the libraries under `../stones` to the index, so their
`"<name>": "0.1"` dependency resolves. For each of `json`, `toml`, `http`,
`markdown`, `rok-syntax`:

1. Push the repository to `github.com/rok-lang/<name>`.
2. In a [rokstones.io-index](https://github.com/rok-lang/rokstones.io-index)
   checkout: `rok publish` to preview the index line, then `rok publish
   --index <checkout> --commit`, and open a pull request.
3. Once merged, switch the sites (`rok-lang.org`, `playground`,
   `rokstones.io`) from `Dep::path("../stones/<name>")` to `"<name>":
   "0.1"`.

## Sites

Deploy `rok-lang.org` (home + docs) and `playground.rok-lang.org` from
their own repositories after the release and the stones are up.
