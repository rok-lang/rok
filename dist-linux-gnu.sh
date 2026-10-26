#!/bin/sh
# Build the Linux release archives: dist-native.sh in a Debian 11 container
# (dist-linux-gnu.Containerfile), so rok, rokc, rokls, libclang.so and
# librokr.a need at most glibc 2.34, as the -gnu targets' programs do.
# A newer host's glibc leaks its symbol versions into them.
#
#   ./dist-linux-gnu.sh [--out <dir>]
#
# For the host's architecture (x86_64 or aarch64), with podman, else
# docker. The checkout is mounted read-only; rustup, cargo, LLVM,
# libclang and the target directory live in ROK_DIST_CACHE (default
# ~/.cache/rok/dist-linux), the archives land in <dir> (default dist).
# Then the installed toolchain builds and runs a program on Rocky Linux 9.
set -eu

cd "$(dirname "$0")"
out=dist
case "${1-}" in
  --out) out=$2 ;;
  "") ;;
  *)
    echo "usage: ./dist-linux-gnu.sh [--out <dir>]" >&2
    exit 2
    ;;
esac
# The commit for --version, taken here: the container may not see the
# repository (a worktree's .git points outside the mount).
if [ -z "${ROK_COMMIT_HASH-}" ] || [ -z "${ROK_COMMIT_DATE-}" ]; then
  if h=$(git rev-parse --short=9 HEAD 2> /dev/null) &&
    d=$(git log -1 --format=%cs 2> /dev/null); then
    ROK_COMMIT_HASH=${ROK_COMMIT_HASH:-$h}
    ROK_COMMIT_DATE=${ROK_COMMIT_DATE:-$d}
  else
    echo "dist-linux-gnu.sh: no commit for --version: run from a git checkout, or set" \
      "ROK_COMMIT_HASH and ROK_COMMIT_DATE (\`git rev-parse --short=9 HEAD\` and" \
      "\`git log -1 --format=%cs\` in the checkout this was made from)" >&2
    exit 1
  fi
fi
engine=$(command -v podman || command -v docker) || {
  echo "dist-linux-gnu.sh: needs podman or docker" >&2
  exit 1
}
cache=${ROK_DIST_CACHE:-${XDG_CACHE_HOME:-$HOME/.cache}/rok/dist-linux}
mkdir -p "$out" "$cache"
out=$(cd "$out" && pwd)

# The container's LLVM/libclang: dist-llvm-prefix.sh publishes a prefix built
# in this same container to the rok-lang-artifacts bucket, and the build below
# downloads it instead of building (the image's glibc is older than a CI
# host's, so the native prefix does not fit). A miss falls back to building.
base=${ROK_DIST_LLVM_BASE:-https://rok-lang-artifacts.s3.us-east-1.amazonaws.com}

# The image is named after its definition.
tag=rok-dist-linux:$(sha256sum dist-linux-gnu.Containerfile | cut -c1-12)
$engine image inspect "$tag" > /dev/null 2>&1 ||
  $engine build -t "$tag" - < dist-linux-gnu.Containerfile

# Rootless podman maps the container's root to the caller; docker's is
# root, so it runs as the caller.
user=""
case $engine in */docker) user="--user $(id -u):$(id -g)" ;; esac
# shellcheck disable=SC2086
$engine run --rm $user \
  -v "$PWD:/src:ro" -v "$cache:/cache" -v "$out:/out" -w /src \
  -e HOME=/cache/home -e RUSTUP_HOME=/cache/rustup -e CARGO_HOME=/cache/cargo \
  -e ROK_LLVM_CACHE=/cache/rok -e CARGO_TARGET_DIR=/cache/target -e TMPDIR=/cache/tmp \
  -e ROK_LLVM_JOBS="${ROK_LLVM_JOBS-}" -e ROK_DIST_LLVM_BASE="$base" \
  -e ROK_COMMIT_HASH="$ROK_COMMIT_HASH" -e ROK_COMMIT_DATE="$ROK_COMMIT_DATE" \
  "$tag" sh -euc '
    mkdir -p "$HOME" "$TMPDIR"
    [ -x "$CARGO_HOME/bin/rustup" ] ||
      rustup-init -y -q --no-modify-path --profile minimal --default-toolchain none
    PATH=$CARGO_HOME/bin:$PATH
    rustup toolchain install
    [ -n "$ROK_LLVM_JOBS" ] || unset ROK_LLVM_JOBS
    # Download the prebuilt LLVM/libclang prefix from S3; on any miss
    # dist-native.sh builds it, as before.
    v=$(sed -n "s/^VERSION=//p" llvm/build.sh)
    triple=$(uname -m)-unknown-linux-gnu
    mkdir -p /cache/rok
    for n in llvm libclang; do
      pfx=/cache/rok/$n-$v
      if [ ! -f "$pfx/.complete" ]; then
        u=$ROK_DIST_LLVM_BASE/dist-llvm/$v/$triple/$n.tar.gz
        if curl -fsSL "$u" -o "$TMPDIR/$n.tar.gz" &&
          curl -fsSL "$u.sha256" -o "$TMPDIR/$n.tar.gz.sha256" &&
          ( cd "$TMPDIR" && sha256sum -c "$n.tar.gz.sha256" >/dev/null ) &&
          tar -xzf "$TMPDIR/$n.tar.gz" -C /cache/rok; then
          echo "$n: downloaded $pfx"
        fi
      fi
    done
    if [ -f "/cache/rok/llvm-$v/.complete" ]; then export ROK_LLVM_PREFIX=/cache/rok/llvm-$v; fi
    if [ -f "/cache/rok/libclang-$v/.complete" ]; then export ROK_LIBCLANG_PREFIX=/cache/rok/libclang-$v; fi
    ./dist-native.sh --out /out'

# dist-native.sh's check programs need glibc 2.34, newer than the image's: the
# installed archive builds and runs them on Rocky Linux 9 (glibc 2.34),
# with nothing else there.
name=rok-$(cat "$out/rok-version")-$(uname -m)-unknown-linux-gnu
# shellcheck disable=SC2086
$engine run --rm $user -v "$out:/out:ro" -e HOME=/tmp \
  docker.io/rockylinux/rockylinux:9@sha256:8101994123cf3d0a8fee517bee7f39e555c7d92bd2d9eb3303cc988a0eeed00f \
  sh -euc '
    cd /tmp
    tar -xzf "/out/$1.tar.gz"
    "$1/install.sh" >/dev/null
    printf "fn main() {\n    println(\"ok\");\n}\n" >check.rok
    for o in -O0 -O2; do
      ~/.rok/bin/rokc -q build check.rok $o -o check
      [ "$(./check)" = ok ]
    done
    echo "$1: runs on $(cat /etc/rocky-release)"' sh "$name"
