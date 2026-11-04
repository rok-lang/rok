# Releasing

How a `rok` release is cut. A release ships one host archive set for every
triple in `release-targets.json`; `release.yml` builds them all on
GitHub-hosted runners and cuts the GitHub release on a tag push. Nothing
runs on its own: a release happens only when a `v<version>` tag is pushed.
A manual `workflow_dispatch` run builds and verifies the archives and never
publishes.

## Versioning and cadence

Rok shifts semantic versioning down one level while the major version is 0,
the way Cargo treats 0.x crates: the minor number is the breaking slot and
the patch number carries features and fixes. A minor bump (`0.2.0`) is for
breaking changes; a patch bump (`0.1.1`, `0.1.2`, ...) adds
backward-compatible features and fixes. 1.0 waits until the language and
standard library are meant to be stable.

Releases are cut on demand, when a version's work is ready; there is no
fixed schedule. A release is cut from `main` and tagged `v<version>`.
There is no release or maintenance branch: only the latest series gets
fixes, so a fix always ships as the next release from `main`. Once a new
series is out, the one before it is no longer supported (no fixes, no
security releases); its archives stay downloadable. The 0.1 series is
unsupported since 0.2.0 (`SECURITY.md`). A version may first go out as a release
candidate (`v<version>-rc.N`) to exercise the pipeline before the final
tag.

A release candidate is always a GitHub prerelease: `release.sh --publish`
passes `--prerelease` for a `-rc.N` version. GitHub's `latest/` skips
prereleases, so `latest/download/rok-version` keeps naming the last final
release, and a bare `rok toolchain update` (or `install.sh` / `install.ps1`
without a version) never installs an rc. An rc is installed only by name:
`install.sh --version 0.2.0-rc.1`, `install.ps1 -Version 0.2.0-rc.1` or
`rok toolchain update 0.2.0-rc.1`. `scripts/test-release.sh` checks this
against a stubbed `gh` (CI `lint`).

## Flow

1. Land the release commit (version bump and `CHANGELOG.md`) on `main`.
2. Run `release-ready` on it: Actions, release-ready, Run workflow, `ref`
   `main` (or the commit's sha). It runs, on that exact commit:
   - `scripts/check-version.sh` and `scripts/release-notes.sh <version>`;
   - full `ci` (`hosts: all`, `jobs: all`: every host at `-O0` and `-O2`,
     cross, wasm, bindgen, dist), reusing a passing manual or nightly run of
     the commit, else dispatching one on the branch and waiting;
   - a `release.yml` dry run (build and verify every archive, no
     publish), reused or dispatched the same way.

   When all pass it sets the `release-ready` status on the commit. A later
   push to `main` does not matter: the status belongs to the commit.
3. Tag that commit `v<version>` and push the tag. `release.yml` refuses a
   commit without a passing `release-ready` run.

A pushed tag is never moved or deleted, and a release is never re-cut. If
something is wrong after the tag, the fix ships as the next patch version.
`release.yml` checks right before it publishes that the tag still names
the commit the archives were built from, and stops otherwise.

## Targets

`release-targets.json` is the single list of release targets: `release.yml`
builds its matrix from it, `scripts/release.sh` verifies exactly its
archives, and `scripts/check-targets.sh` checks the install scripts against
it. Each entry names the triple, the runner image (`runs-on`), the build
script (`kind`: `native` is `dist-native.sh`, `gnu` / `musl` the container
scripts) and the archive set (`archive`: `unix`, `win` or `musl`). Adding or
dropping a host is an edit there, not here.

## What a tag push does

Pushing a `v<version>` tag runs `release.yml`:

1. `ready` calls `release-ready.yml` with `verify_only`: the version and
   notes checks again, the tag is `v` plus the version, and the commit has a
   `release-ready` status set by a passing release-ready run. Then
   `ci-green`: the tagged commit must have a `ci` run on `main` (a push
   event, the newest not cancelled) whose `ci-gate` job succeeded. A run
   still in progress is waited for; no run, or a gate that failed, fails the
   release before anything is built.
2. `plan`: reads the targets from `release-targets.json`.
3. `build`, one independent job per target, all in parallel (`fail-fast`
   off, so one failure does not cancel the rest):
   - `native` targets (macOS, Windows) build with `dist-native.sh`, taking
     the pinned LLVM and libclang prefixes from the `rok-lang-artifacts` S3
     bucket (the `llvm-prefix` / `libclang-prefix` actions).
   - `gnu` and `musl` targets build in a container: `dist-linux-gnu.sh`
     (glibc 2.34) and `dist-linux-musl.sh` (static musl), each on a runner
     of its own architecture. The container's LLVM is downloaded from S3
     (`dist-llvm/<version>/<triple>/`), not built from source.
   - each uploads its `rok-<version>-<triple>` archives (`.tar.gz` +
     `.tar.xz`; Windows a `.zip`), a `.sha256` beside each, a
     `rok-version` file, and the commit it built.
4. `publish`, once every build passed: gathers every archive, checks every
   build ran on the run's commit, runs `release.sh --dir dist` to verify
   that every target's archives and checksums are present and match and
   that every toolchain archive keeps the installer contract (below),
   records the build provenance attestation, writes `dist/notes.md` from
   the version's `CHANGELOG.md` section (`scripts/release-notes.sh`), and
   runs `release.sh --dir dist --publish --commit <sha>`. That checks the
   tag on origin still names `<sha>`, creates the `v<version>` release as a
   draft with those notes as its body, uploads every planned asset, checks
   the draft holds exactly them (every archive, its `.sha256`, and
   `rok-version`, each at its local size), checks the tag again and only
   then makes the release public. A draft that fails a check stays
   unpublished; `latest/` never sees it. `release.sh` refuses a tag that is
   not `v` plus the archives' `rok-version`, and `--publish` refuses a
   missing `notes.md`.
   `install.sh` / `install.ps1` download `v<version>` by default;
   `rok-version` is served from `releases/latest/`.

A manual `workflow_dispatch` run skips `ready` and `ci-green`, then runs
`plan`, `build` and the verify in `publish` exactly as a tag does. It stops
there, also on a tag ref: no attestation, no release, nothing uploaded.
`release-ready` uses it as the dry run.

## Update pointer

A series is the releases one breaking slot covers: `0.MINOR` while the
major version is 0 (`0.1` is 0.1.0, 0.1.1, ...), then `MAJOR`.

`latest/download/rok-version` is one line naming the newest release, of
any series; each release uploads it. A bare `rok toolchain update`
installs what it names and makes it the default, as rustup does, also
when that moves to a new series. The previous toolchain
stays installed, and when the series changed `rok` says so once: the old
series gets no more fixes, so move projects to the new one; until then a
project can pin it (`toolchain: "0.1"` in `rok.rokon`, or in a script's
`//! rokon` block) or run `rok +0.1 ..`. Every 0.1.x client
reads this file the same way, so it must keep its name and format.

Any other release is installed by its full version:
`rok toolchain update 0.2.0`, `rok toolchain install 0.2.0`,
`install.sh --version 0.2.0`, `install.ps1 -Version 0.2.0`. There is no
series lookup and no channel file: an argument that is not a full version
is an error. (rok 0.1.6 still looks for
`download/channels/rok-version-<series>` when given a series; none is
published, so it falls back to `rok-version` and otherwise asks for a full
version.)

## Installer contract

Every 0.1.x `rok toolchain update` and install script must be able to
install a new release, so the archive keeps what they rely on:

- the name `rok-<version>-<triple>.tar.gz` / `.tar.xz` (Windows `.zip`),
  holding one top directory `rok-<version>-<triple>/`;
- in it `bin/rok`, `bin/rokc` (`.exe` on Windows), `lib/librokr.a`
  (Windows `lib/rokr.lib`) and `lib/rok/core/src/lib.rok`;
- `rokc --version --verbose` printing `rokc <version>` first and a
  `host: <triple>` line.

`dist-native.sh` checks the `rokc --version --verbose` output of what it
builds; `scripts/check-archive.sh`, run by `dist-native.sh` and by
`release.sh` on every toolchain archive, checks the name and layout. The nightly `dist`
job (linux-x64 and windows-x64, docs/ci.md) installs the last 0.1.x
release and updates it to the archive it built, with `rok toolchain
update`, the old install script and the new one (`scripts/test-update.sh`,
`scripts/test-update.ps1`); then the new toolchain builds a project the
old one made and locked. Run it (`jobs: dist`) before tagging.

## Before the tag

- The prefixes for this `<version>` must already be on S3: the native
  prefixes and the container `dist-llvm` prefixes (`prefixes.yml`; the
  Windows LLVM via `prefixes-windows.yml`). They are rebuilt only when the
  LLVM version or a build script changes, not for every release.
- The version must agree across `Cargo.toml` (the source of truth, baked
  into `rokc --version`), `install.sh` and `install.ps1`; `dist-native.sh`
  fails the build if they disagree.
- `CHANGELOG.md` has a dated, non-empty `## <version> (<date>)` section;
  it becomes the release notes, and the release fails without it. An rc
  uses its own `## <version>-rc.N` section if there is one, else the
  section of the final version (`## 0.2.0` for `0.2.0-rc.1`). Check with
  `scripts/release-notes.sh <version>`.
- The release commit (version bump and changelog) goes straight to `main`,
  its `ci` run there has a green `ci-gate`, and a `release-ready` run passed
  on it (Flow, above). `release.yml` refuses the tag otherwise. The
  `corpus` jobs build and run every integration case: each shard reports
  its share of the corpus, and one that selects no case, or runs fewer than
  its share of the floor (docs/ci.md), fails (an empty `ROK_TEST_FILTER` is
  the whole corpus).
- The stones, the sites and CI version and deploy on their own terms; this
  file covers only the toolchain release.

## After the release

Once `release.yml` has published and the assets are downloadable:

- Nix: run `scripts/nix-sources.sh <version>`, which rewrites
  `nix/sources.json` from the release's `.sha256` assets, and land it as
  `chore(nix): install <version>` through the merge queue. It cannot be in
  the release commit: the checksums exist only once the archives are
  published.
- Roadmap: close the version's milestone, and move what shipped in
  `roadmap.md` in rok-lang/www (the rok-lang.org roadmap). That repository
  deploys on its own; the toolchain release does not touch it.

## Building an archive by hand

The same scripts build one host's archives locally, for a spot check or a
host CI cannot build:

| Host | Machine | Command |
|---|---|---|
| `aarch64-apple-darwin` | an Apple-silicon Mac | `scripts/dist-native.sh --out dist` |
| `x86_64-unknown-linux-gnu` | x86_64 Linux | `scripts/dist-linux-gnu.sh --out dist` |
| `aarch64-unknown-linux-gnu` | arm64 Linux | `scripts/dist-linux-gnu.sh --out dist` |
| `x86_64-pc-windows-msvc` | Windows x86_64 | `scripts/dist-native.sh --out dist` (Git Bash, MSVC environment) |
| `x86_64-unknown-linux-musl` | x86_64 Linux | `scripts/dist-linux-musl.sh --out dist` (Alpine container; static, no libclang / dev bundle) |
| `aarch64-unknown-linux-musl` | arm64 Linux | `scripts/dist-linux-musl.sh --out dist` |

The table follows `release-targets.json`; the file wins if they differ.
`dist-linux-gnu.sh` and `dist-linux-musl.sh` run in a container, so one
x86_64 and one arm64 Linux box build every Linux target. Intel Macs and
Windows on arm64 get no archive; they build from source. Gather the output
of every host into one `dist/` directory and check it with:

```sh
scripts/release.sh --dir dist
```

It verifies that the archives and checksums of every triple in
`release-targets.json` are present and that every `.sha256` matches, and
changes nothing without `--publish`.
