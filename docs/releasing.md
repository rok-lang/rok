# Releasing

How a `rok` release is cut. A release ships one host archive set for every
triple in `release-targets.json`, built on GitHub-hosted runners. Nothing
runs on its own: a release starts only when someone dispatches
`release.yml`, and only `release.yml`, through the `rok-lang-release` App,
creates `v<version>` tags.

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

1. Prepare the release commit: Actions, release-prep, Run workflow,
   `version` `X.Y.Z` (below). Write the hand-written blocks on
   `release/X.Y.Z` until `changelog-lint` passes.
2. Rehearse it: Actions, release, Run workflow, `version` `X.Y.Z`, `mode`
   `rehearse` (the default). It runs every stage below on a copy of the
   release commit (`rehearse/X.Y.Z`) and makes nothing public.
3. Release it: the same with `mode` `release`, then approve the one
   deployment review (environment `release`) when the run stops for it.
   Its summary shows the version, the commit, every asset with size and
   SHA-256, and the notes.

`release.yml`, `release-prep.yml` and nothing else share the concurrency
group `rok-release`: one runs at a time, the next waits.

A pushed tag is never moved or deleted, and a release is never re-cut. If
something is wrong after the tag, the fix ships as the next patch version.
The `immutable release tags` ruleset enforces it, and `tag-guard.yml`
opens an `alarm` issue for any `v*` tag the App did not create.

Branches: `main` only, changed through the merge queue (squash) and, for
the release commit, by the App's fast-forward. `release/X.Y.Z` and
`rehearse/X.Y.Z` live for one release; the `release branches` ruleset lets
only the App (and an org admin, to break glass) create or delete them,
while anyone with write access pushes the hand-written blocks to
`release/X.Y.Z`. There are no maintenance branches: only the latest series
gets fixes.

## What release.yml does

Each job first looks at what is already done and skips it, so a run that
failed or was cancelled resumes when the same version and mode are
dispatched again. The record lives in the issue `Release X.Y.Z`
(`Rehearsal X.Y.Z`, labels `release`, `rehearsal`): a table and a JSON
block with the state reached, the evidence of each stage and, after a
failure, `blocked` with the failed jobs.

| Job | State | What it does | Rehearsal |
|---|---|---|---|
| `plan` | PLANNED | checks the version, opens or reuses the record | same |
| `prepare` | PREPARED | `release-prep.sh --push` (a no-op when up to date), or finds `release: X.Y.Z` already on `main`; `changelog-lint` must pass | on `rehearse/X.Y.Z`, open milestone issues and a draft changelog only warn |
| `land` | LANDED | fast-forwards `main` to the release commit (the App bypasses the `main` ruleset for this push); a `main` that moved fails, and the next dispatch rebases | skipped |
| `ready`, `ci-green` | READY | `release-ready.yml` on the commit (full ci and a `release-build.yml` dry run, passing runs reused), then `main`'s own `ci-gate` | no `ci-green` |
| `build-find`, `build`, `collect` | BUILT | reuses the dry run's `dist-*` artifacts (`scripts/release-dist.sh find`), else builds; checks every build is of the commit, `release.sh --dir dist` | same |
| `tag` | TAGGED | the one approval (environment `release`), then the App creates the annotated tag; a tag already on the commit is kept, one on another commit stops the run | environment `rehearsal`: no reviewer, checks the tag is free and the App token mints |
| `publish` | PUBLISHED | attestation, then `release.sh --publish`: draft, check, tag check, public; a public release with every asset is left alone | `release.sh --rehearse`: a draft without a tag, checked, deleted |
| `smoke` | SMOKED | installs `vX.Y.Z` with `install-rok` on Linux, macOS and Windows; `latest/download/rok-version` names it | installs the build run's archives |
| `finish`, `cleanup` | | writes the furthest state, or `blocked`, and comments | closes the record, deletes `rehearse/X.Y.Z` |

`mode` `abort` drops an attempt with no tag: it deletes `release/X.Y.Z`
and `rehearse/X.Y.Z` and closes the record as ABANDONED. A commit that
already landed stays on `main` (it is never rewritten); the next
release-prep makes a new `release: X.Y.Z` commit over it. Once tagged, the
version is burned: ship the next one.

## Recovering

Read the record's `blocked` and the failed job, fix the cause, then
dispatch `release` again with the same version and mode.

- `prepare` failed on changelog-lint: write the Summary and Highlights on
  `release/X.Y.Z`. On open issues or a bad `changelog:` block: close or
  move the issues, fix the PR description.
- `land` failed because `main` moved: dispatch again; prepare rebases.
- `ready` or `ci-green` failed: fix `main` (a red `ci-gate`) or rerun the
  failed ci run, then dispatch again; passing runs are reused.
- `build` failed: dispatch again; only a run that holds every `dist-*`
  artifact is reused, so it builds again.
- `tag` stopped on a tag on another commit: someone tagged by hand; the
  version is burned (see `tag-guard`'s alarm), ship the next one.
- `publish` failed after the tag: dispatch again; the leftover draft is
  made again. A public release with other assets is not touched.
- `smoke` failed: the release is public. Find why it does not install;
  the fix ships as the next patch.

## Preparing the release commit

`release-prep.yml` (manual, from `main`, in the environment `release-prep`)
runs `scripts/release-prep.sh --push X.Y.Z` and pushes as the
`rok-lang-release` App.
It fails, listing each, when the milestone `X.Y.Z` has open issues or a PR
merged since the previous tag has no valid `changelog:` block (fix the PR
description, then run it again). Otherwise it pushes `release/X.Y.Z`: one
commit `release: X.Y.Z` on `main` that sets the version in `Cargo.toml`,
`Cargo.lock` (`cargo update --workspace --offline`), `install.sh` and
`install.ps1`, and puts on top of `CHANGELOG.md`:

```
## X.Y.Z (YYYY-MM-DD)

<!-- begin summary -->
Summary: ...
<!-- end summary -->

<!-- begin highlights -->
### Highlights
...
<!-- end highlights -->

<!-- begin migration -->     (X.Y.0 only)
### Migration
...
<!-- end migration -->

### Added / ### Changed / ### Fixed / ### Removed
- <the PR's line> (#N)

<!-- silent: #N ... -->
```

The kinds and their lines come from each PR's `changelog:` block (the pull
request template; `pr-lint` checks it on every PR). Silent lines and
Dependabot PRs go only into the `silent` comment, so every PR stays
accounted for. To change a generated line, edit the PR description and run
release-prep again.

The blocks between the `begin` / `end` markers are written by hand on the
branch (the web editor is enough): the one `Summary:` line (the roadmap and
rok-lang.org show it), the highlights, and for X.Y.0 the migration notes.
Delete a block that has nothing to say. Every push to `release/*` runs
`changelog-lint.yml` (`scripts/changelog-lint.sh X.Y.Z`): the first
section is `## X.Y.Z (YYYY-MM-DD)` and the only one for X.Y.Z, `Cargo.toml`
sets X.Y.Z, no heading is empty, every PR since the previous tag is cited
as `(#N)` or listed silent, the Summary line is written and no `TODO` is
left in a block.

Running release-prep again refreshes the branch: when `main` has moved it
rebases onto it, regenerates the PR lines and keeps the hand-written blocks
(and their deletion). The branch may hold extra commits that touch only the
release files; they are squashed into the one release commit. A branch that
changes anything else is refused. When nothing changed it writes nothing.
`scripts/release-prep.sh X.Y.Z` without `--push` does the same into the
local branch only.

## Targets

`release-targets.json` is the single list of release targets:
`release-build.yml` builds its matrix from it, `scripts/release.sh` verifies exactly its
archives, and `scripts/check-targets.sh` checks the install scripts against
it. Each entry names the triple, the runner image (`runs-on`), the build
script (`kind`: `native` is `dist-native.sh`, `gnu` / `musl` the container
scripts) and the archive set (`archive`: `unix`, `win` or `musl`). Adding or
dropping a host is an edit there, not here.

## What a build does

`release-build.yml` builds one commit; `release-ready` dispatches it as the
dry run and `release.yml` reuses its artifacts:

1. `plan`: reads the targets from `release-targets.json`.
2. `build`, one independent job per target, all in parallel (`fail-fast`
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
3. `verify`: gathers every archive, checks every build ran on the commit
   (`scripts/release-dist.sh check`) and runs `release.sh --dir dist`: every
   target's archives and checksums are present and match, and every
   toolchain archive keeps the installer contract (below).

`release.yml`'s `publish` then records the build provenance attestation,
writes `dist/notes.md` from the version's `CHANGELOG.md` section
(`scripts/release-notes.sh`) and runs `release.sh --dir dist --publish
--commit <sha>`. That checks the tag on origin still names `<sha>`,
creates the `v<version>` release as a draft with those notes as its body,
uploads every planned asset, checks the draft holds exactly them (every
archive, its `.sha256`, and `rok-version`, each at its local size), checks
the tag again and only then makes the release public. A draft that fails a
check stays unpublished; `latest/` never sees it. `install.sh` /
`install.ps1` download `v<version>` by default; `rok-version` is served
from `releases/latest/`.

`release-legacy.yml` keeps the old path (a pushed tag builds and
publishes) for one release, off unless the repository variable
`ROK_RELEASE_LEGACY` is `on`.

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
- The release commit (version bump and changelog, from release-prep) goes straight to `main`,
  its `ci` run there has a green `ci-gate`, and a `release-ready` run passed
  on it; `release.yml` does all three and stops on the first that fails. The
  `corpus` jobs build and run every integration case: each shard reports
  its share of the corpus, and one that selects no case, or runs fewer than
  its share of the floor (docs/ci.md), fails (an empty `ROK_TEST_FILTER` is
  the whole corpus).
- The stones, the sites and CI version and deploy on their own terms; this
  file covers only the toolchain release.

## After the release

Once `release.yml` reached SMOKED (its record says so):

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
