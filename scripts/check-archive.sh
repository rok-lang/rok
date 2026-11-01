#!/bin/sh
# Check toolchain archives against the installer contract (spec 9.10.6),
# which every 0.1.x `rok toolchain update` and install script relies on:
#
#   rok-<version>-<triple>.tar.gz / .tar.xz / .zip, one top directory
#   rok-<version>-<triple>/ holding bin/rok, bin/rokc (.exe on Windows),
#   lib/librokr.a (Windows: lib/rokr.lib) and lib/rok/core/src/lib.rok.
#
#   scripts/check-archive.sh <version> <archive>...
#
# Prints one line per archive and exits 1 if any breaks the contract.
# `rokc --version --verbose` is checked where it runs: dist-native.sh.
# The rok-mlir-dev bundle is not a toolchain and is not checked.
set -eu

[ $# -gt 1 ] || {
  echo "usage: scripts/check-archive.sh <version> <archive>..." >&2
  exit 2
}
version=$1
shift

# The entries of archive $1, one per line, without a leading ./
entries() {
  case $1 in
    *.tar.gz) tar -tzf "$1" ;;
    *.tar.xz) tar -tJf "$1" ;;
    *.zip) unzip -Z1 "$1" ;;
  esac | sed 's|^\./||'
}

bad=0
for a in "$@"; do
  file=${a##*/}
  case $file in
    rok-mlir-dev-*) continue ;;
    rok-*.tar.gz) top=${file%.tar.gz} ;;
    rok-*.tar.xz) top=${file%.tar.xz} ;;
    rok-*.zip) top=${file%.zip} ;;
    *)
      echo "breaks    $file: not named rok-<version>-<triple>.tar.gz, .tar.xz or .zip" >&2
      bad=$((bad + 1))
      continue
      ;;
  esac
  triple=${top#"rok-$version-"}
  case $triple in
    "$top" | *[!a-z0-9_-]* | *-*-*-*-*) triple="" ;;
    *-*-*) ;;
    *) triple="" ;;
  esac
  [ -n "$triple" ] || {
    echo "breaks    $file: not named rok-$version-<triple>" >&2
    bad=$((bad + 1))
    continue
  }
  case $triple in
    *-windows-*) exe=.exe rt=rokr.lib ;;
    *) exe="" rt=librokr.a ;;
  esac
  list=$(entries "$a") || {
    echo "breaks    $file: cannot list it" >&2
    bad=$((bad + 1))
    continue
  }
  why=""
  outside=$(printf '%s\n' "$list" | grep -v "^$top/" | grep -v "^$top\$" | head -n 1 || true)
  [ -z "$outside" ] || why="$outside is outside the top directory $top/"
  for need in "bin/rok$exe" "bin/rokc$exe" "lib/$rt" lib/rok/core/src/lib.rok; do
    [ -n "$why" ] || printf '%s\n' "$list" | grep -qx "$top/$need" || why="no $top/$need"
  done
  if [ -n "$why" ]; then
    echo "breaks    $file: $why" >&2
    bad=$((bad + 1))
  else
    echo "contract  $file"
  fi
done
[ "$bad" -eq 0 ] || {
  echo "check-archive.sh: $bad archive(s) break the installer contract (spec 9.10.6)" >&2
  exit 1
}
