#!/bin/sh
# Build the pinned LLVM (and, for gnu, libclang) prefix inside the Linux
# release container and package it for the rok-lang-artifacts bucket, so
# dist-linux-{gnu,musl}.sh download it instead of building it in-container.
# A CI host's glibc/libc is newer than the image's, so the native prefix
# (prefixes.yml) does not fit; this one is built in the same image.
# Published by prefixes.yml at dist-llvm/<version>/<triple>/{llvm,libclang}.tar.gz.
#
#   scripts/dist-llvm-prefix.sh <gnu|musl> [--out <dir>]
#
# Writes <dir>/pkg/{llvm,libclang}.tar.gz and their .sha256 (default <dir>
# dist-llvm). For the host's architecture, with podman, else docker.
set -eu

cd "$(dirname "$0")/.."
kind=${1:-}
case $kind in
  gnu | musl) shift ;;
  *)
    echo "usage: scripts/dist-llvm-prefix.sh <gnu|musl> [--out <dir>]" >&2
    exit 2
    ;;
esac
out=dist-llvm
case "${1-}" in
  --out) out=$2 ;;
  "") ;;
  *)
    echo "usage: scripts/dist-llvm-prefix.sh <gnu|musl> [--out <dir>]" >&2
    exit 2
    ;;
esac

case $kind in
  gnu) cf=dist-linux-gnu.Containerfile image=rok-dist-linux libclang=1 ;;
  musl) cf=dist-linux-musl.Containerfile image=rok-dist-musl libclang="" ;;
esac

v=$(sed -n 's/^VERSION=//p' llvm/build.sh)
mkdir -p "$out"
out=$(cd "$out" && pwd)
build=$out/build
mkdir -p "$build"

engine=$(command -v podman || command -v docker) || {
  echo "dist-llvm-prefix.sh: needs podman or docker" >&2
  exit 1
}
# The image is named after its definition (same tag as dist-linux-*.sh).
tag=$image:$(sha256sum "$cf" | cut -c1-12)
$engine image inspect "$tag" > /dev/null 2>&1 ||
  $engine build -t "$tag" - < "$cf"

# Rootless podman maps the container's root to the caller; docker's is
# root, so it runs as the caller.
user=""
case $engine in */docker) user="--user $(id -u):$(id -g)" ;; esac
# shellcheck disable=SC2086
$engine run --rm $user \
  -v "$PWD:/src:ro" -v "$build:/cache" -w /src \
  -e HOME=/cache/home -e TMPDIR=/cache/tmp -e ROK_LLVM_JOBS="${ROK_LLVM_JOBS-}" \
  -e V="$v" -e LIBCLANG="$libclang" \
  "$tag" sh -euc '
    mkdir -p "$HOME" "$TMPDIR"
    [ -n "$ROK_LLVM_JOBS" ] || unset ROK_LLVM_JOBS
    llvm/build.sh /cache/rok/llvm-$V
    ( cd /cache/rok/llvm-$V &&
      rm -rf lib/objects-Release share/opt-viewer lib/*.dylib lib/*.so lib/*.so.* &&
      touch .complete )
    if [ -n "$LIBCLANG" ]; then
      llvm/build-libclang.sh /cache/rok/libclang-$V
      touch /cache/rok/libclang-$V/.complete
    fi'

triple=$(uname -m)-unknown-linux-$kind
pkg=$out/pkg
mkdir -p "$pkg"
tar -C "$build/rok" -czf "$pkg/llvm.tar.gz" "llvm-$v"
(cd "$pkg" && sha256sum llvm.tar.gz > llvm.tar.gz.sha256)
if [ -n "$libclang" ]; then
  tar -C "$build/rok" -czf "$pkg/libclang.tar.gz" "libclang-$v"
  (cd "$pkg" && sha256sum libclang.tar.gz > libclang.tar.gz.sha256)
fi
echo "triple=$triple"
echo "dist-llvm-prefix.sh: packaged $pkg (triple $triple, v $v)"
