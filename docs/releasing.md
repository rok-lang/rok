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
There is no long-lived release branch: a maintenance branch (for example
`0.1`) is made from a release tag only if a patch for an older minor is
needed after `main` has moved on. A version may first go out as a release
candidate (`v<version>-rc.N`) to exercise the pipeline before the final
tag.

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

1. `ci-green`: the tagged commit must have a `ci` run on `main` (a push
   event) whose `ci-gate` job succeeded. A run still in progress is waited
   for; no run, or a gate that failed, fails the release before anything is
   built. So the release commit is pushed to `main` first, and the tag goes
   on only once that run is green.
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
     `.tar.xz`; Windows a `.zip`), the `rok-mlir-dev-<version>-<triple>`
     bundle (none for static musl), a `.sha256` beside each, and a
     `rok-version` file.
4. `publish`, once every build passed: gathers every archive, runs
   `release.sh --dir dist` to verify that every target's archives and
   checksums are present and match, records the build provenance
   attestation, and runs `release.sh --dir dist --publish` to create the
   `v<version>` GitHub release and upload every asset with the `gh` CLI.
   `install.sh` / `install.ps1` download `v<version>` by default;
   `rok-version` is served from `releases/latest/`.

A manual `workflow_dispatch` run skips `ci-green`, then runs `plan`,
`build` and the verify in `publish` exactly as a tag does. It stops there:
no attestation, no release, nothing uploaded. Use it to check the pipeline
on a commit before tagging it.

## Series and update pointers

A series is the releases one breaking slot covers: `0.MINOR` while the
major version is 0 (`0.1` is 0.1.0, 0.1.1, ...), then `MAJOR`. Clients find
the newest release in two ways:

- `latest/download/rok-version`: one line naming the newest release, of
  any series. A bare `rok toolchain update` installs what it names and
  makes it the default, as rustup does, also when that moves to a new
  series. The previous toolchain stays installed, and when the series
  changed `rok` says so once, with how to stay on the old one: pin it
  (`toolchain: "0.1"` in `rok.rokon`, or in a script's `//! rokon`
  block) or run `rok +0.1 ..`. `rok new` writes that pin from 0.1.6 on.
- `download/channels/rok-version-<series>`, one channel file per series,
  each one line naming the newest release of that series. They are assets
  of a `channels` GitHub release (a prerelease, never the latest one, so
  it does not move `latest/`).

rok 0.1.6 and later read the channel files for a series:

- `rok toolchain update <series>` and `rok toolchain install <series>`
  (or a requirement such as `^0.1.5`) read the series' channel file;
  a full version (`0.1.6`) is fetched directly and needs no pointer.
- `install.sh --version 0.1` and `install.ps1 -Version 0.1` read the same
  channel file.

When a channel file is missing (HTTP 404), these clients fall back to
`rok-version` and use it only if it names a release of the series they
want; otherwise they stop and ask for a full version. So 0.1.6 works
before any channel file is published, for the series `rok-version` is in.

To publish the channel files, every release would write
`rok-version-<its series>` to the `channels` release, replacing the old
one (`gh release upload channels rok-version-<series> --clobber`); the
`channels` release is created once by hand, as a prerelease. Nothing does
this yet: `release.sh` and `release.yml` upload only `rok-version` with
each release, and clients rely on the fallback above.

## Before the tag

- The prefixes for this `<version>` must already be on S3: the native
  prefixes and the container `dist-llvm` prefixes (`prefixes.yml`; the
  Windows LLVM via `prefixes-windows.yml`). They are rebuilt only when the
  LLVM version or a build script changes, not for every release.
- The version must agree across `Cargo.toml` (the source of truth, baked
  into `rokc --version`), `install.sh` and `install.ps1`; `dist-native.sh`
  fails the build if they disagree.
- `CHANGELOG.md` has a dated `## <version>` section.
- The release commit (version bump and changelog) goes straight to `main`,
  and its `ci` run there has a green `ci-gate`. `ci-green` refuses the tag
  otherwise; a green `release.yml` manual run is not a substitute.
- The stones, the sites and CI version and deploy on their own terms; this
  file covers only the toolchain release.

## After the release

Once `release.yml` has published and the assets are downloadable:

- Nix: run `scripts/nix-sources.sh <version>`, which rewrites
  `nix/sources.json` from the release's `.sha256` assets, and land it as
  `chore(nix): install <version>` through the merge queue. It cannot be in
  the release commit: the checksums exist only once the archives are
  published.
- Roadmap: close the version's milestone, and move what shipped on the
  rok-lang.org roadmap. That page lives in its own repository and deploys
  on its own; the toolchain release does not touch it.

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
