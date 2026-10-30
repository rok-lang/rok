# CI

`.github/workflows/ci.yml` (GitHub Actions). Changes to docs or Markdown alone run `lint` only; a newer run of
the same branch and event cancels the older one.

## Tiers

| Event | Hosts | Jobs |
|---|---|---|
| pull request | linux-x64, windows-x64 | `check`, `test`, `corpus` at `-O0` |
| merge queue (`merge_group`), push to `main` | linux-x64, linux-arm64, macos-arm64, windows-x64 | `check`, `test`, `corpus` at `-O0` |
| nightly (`schedule`, 21:00 UTC), manual run | every host | `check`, `test`, `corpus` at `-O0` and `-O2`, `cross-link`, `cross`, `wasm`, `bindgen`, `dist` |

A push to `main` that the merge queue already passed runs `lint` only.

A manual run (`workflow_dispatch`) takes `hosts` (all, or some of `linux-x64 linux-arm64 macos-arm64
windows-x64 windows-arm64`), `jobs` (all, or some of `gate cross wasm bindgen dist`; `none`: prefixes only) and `filter`
(`ROK_TEST_FILTER`: the integration cases to run; the differential suites always run). Hosts: `ubuntu-24.04` x86_64, `ubuntu-24.04-arm` aarch64, `macos-26`
arm64, `windows-2022` x86_64, `windows-11-arm` aarch64 (cross only). x86_64-apple-darwin is a cross target only. There is no native Intel macOS host, as GitHub is retiring its Intel runners.

## Jobs

- `llvm-prefix`, per host: the pinned LLVM prefix (`.github/actions/llvm-prefix`), downloaded from the public
  `rok-lang-artifacts` S3 bucket (`llvm/<version>/<triple>/llvm.tar.gz`, SHA-256 verified) and no longer
  built here. A GitHub-hosted runner keeps it in the actions cache as a fast layer over the download (120-175
  MB per host; a cache entry made on a branch serves that branch only, so the first run on a new branch falls
  through to the download). A self-hosted runner keeps it in `ROK_CI_HOME` and downloads only when none is
  there. The prefixes are built and published out of band by `prefixes.yml` (manual: once per version, or
  when a build script or PGO profile changes); that build is the slow part (roughly 0.5 h on macOS arm64 up
  to 6.0 h on Windows, at the 6 h job limit), paid once, not on every CI run.
- `check`, per host: fmt, clippy, every test binary built and archived (`cargo nextest archive --workspace`,
  uploaded as `tests-<host>`), doctests, `rokc fmt --check library`, and the compiler / runtime crate
  isolation check; every step runs even after one fails. On Linux after `targets/build.sh <host>`.
- `test`, per host and part: every test but the two corpora from that archive, `nextest --partition
  hash:i/3`, with no build but `librokr.a` (`ROK_TEST_PREBUILT=1`).
- `corpus`, per host, level and shard: the `integration` and `differential` binaries from the same archive
  with `ROK_TEST_SHARD=i/4`. A shard takes every fourth integration case of the sorted list (and every fourth
  differential suite); unset, the tests run everything.
- rustc runs through sccache (`.github/actions/sccache`), cached in the `rok-lang-artifacts` bucket under
  `sccache/<os>-<arch>/` (expired after 14 days). Pull requests read it; other events write it through the
  `rok-ci-sccache` role. A cache error builds without it.
- Each build and run of an `integration` case, and each run of a `differential` suite, has its own limit:
  600 s to build and 300 s to run, `ROK_TEST_CASE_TIMEOUT=<seconds>` for both, or a case's `// timeout: N`.
  Past it the process is killed and that case fails as `<case>: timed out after Ns`; the other cases go on.
  `targets/test-wasm.sh` sets 900 s.
- nextest (`.config/nextest.toml`) kills a test that runs past 8 minutes. The archive's baked paths assume
  every job checks out at the same path, true on GitHub-hosted runners.
- `cross-link` (linux-x64): `targets/build.sh` for the Linux targets and `i686-pc-windows-msvc`, then the
  sysroot link test.
- `cross`, per target and level: the corpora for `ROK_TEST_TARGET` on a host that runs it:
  `i686-unknown-linux-gnu` (the i386 loader), `armv7-unknown-linux-gnueabihf` (qemu-user) and
  `x86_64-unknown-linux-musl` (static) on linux-x64, `aarch64-unknown-linux-musl` (static) on
  linux-arm64, `i686-pc-windows-msvc` (WOW64) on windows-x64,
  `aarch64-pc-windows-msvc` on windows-arm64, whose rokc is the x64 build under emulation with the x64
  prefix.
- `wasm` (linux-x64), per wasm32 target: `targets/test-wasm.sh <target>`, with the image's node and clang.
- `bindgen` (linux-x64 and windows-x64, nightly and manual runs only): libclang and clang's resource
  headers (~40 MB, `.github/actions/libclang-prefix`: `llvm/build-libclang.sh`, Windows
  `llvm/build-libclang.ps1`) under their own cache key (`libclang-<os>-<arch>-<hash of the script>`), apart
  from the LLVM prefix, whose key and contents they do not touch; built only when the script changes (on a
  self-hosted runner kept in the default cache directory). Then the `rokc bindgen` tests (`cargo test --test
  cli -- bindgen`) with `ROK_REQUIRE_LIBCLANG=1`. Elsewhere those tests skip with a notice when no libclang
  is built.
- `dist` (windows-x64, nightly and manual runs only): `scripts/dist-native.sh` (Git Bash with the MSVC environment of
  `llvm-prefix`, the LLVM and libclang prefixes above; `targets/build.sh` falls back to the image's `python`
  / `mingw32-make`), then `install.ps1 -NoModifyPath` into a scratch `ROK_HOME` and, with only
  its `bin\` on PATH, `rok run` of a file and a `rok new --lib --bin` package's build / run / `--release`
  build / test.
- `ci-gate`: the one required status check. It passes when `lint`, `plan`, `llvm-prefix`, `check`, `test` and
  `corpus` passed or were skipped, and fails if any failed or was cancelled. A release tag needs it green
  on `main` (docs/releasing.md).

## Self-hosted runners

linux-x64 and windows-x64 run on a self-hosted runner when the repository variable `ROK_LINUX_RUNNER` /
`ROK_WINDOWS_RUNNER` is `self-hosted` (Settings -> Secrets and variables -> Actions -> Variables); otherwise on
GitHub's.

- Labels: `self-hosted, linux, x64` / `self-hosted, windows, x64` (the runner's defaults on that OS and
  architecture).
- Service: configure with `./config.sh` (`config.cmd`), then install it as a service: `sudo ./svc.sh install
  && sudo ./svc.sh start` on Linux; on Windows answer yes to "run as service" in `config.cmd`.
- Persistent directory: set `ROK_CI_HOME` in the runner's `.env` file (next to `config.sh`; restart the
  service), e.g. `ROK_CI_HOME=/var/lib/rok-ci` or `ROK_CI_HOME=D:\rok-ci`. It holds the LLVM prefixes
  (`llvm-<version>-<arch>-<hash>`, marked complete by `.complete`; delete old ones by hand) and one cargo
  target directory per runner (`target/<runner name>`), outside the workspace and instead of the actions
  cache. Runners on one machine share the prefixes; give each its own name.
- Prerequisites: Linux: rustup, clang, curl, xz, make, python3, and for the cross targets `libc6-i386
  libc6-armhf-cross qemu-user-binfmt`, node 18+ for wasm. Windows: rustup, Git for Windows (bash), Visual
  Studio or its Build Tools with `VC.Tools.x86.x64` (the MSVC environment comes from `vswhere`), clang
  (`targets/build.sh`), python3 and make for `dist`. cmake and ninja are needed only to build an LLVM prefix
  (`prefixes.yml`), not for a normal CI run, which downloads it.
- Security: a self-hosted runner runs whatever a workflow gives it. Pull requests from forks never get one:
  the plan job forces GitHub-hosted runners when `github.event.pull_request.head.repo.full_name !=
  github.repository`. On a public repository also keep
  "Require approval for all outside collaborators" (Settings -> Actions -> General) and put the runners in a
  runner group limited to this repository.

## Nightly schedule

The cron schedule runs only where the repository variable `ROK_NIGHTLY` is `on`, so a private copy pays for nightly runs (macOS included) only when it opts in. rok-lang/rok has it on: `-O2` runs there, not in the merge queue.
