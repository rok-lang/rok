#!/bin/sh
# Fail if the release version disagrees across the places it is hardcoded:
# the workspace version (the source baked into `rokc --version`) and the
# install scripts' ROK_VERSION defaults. dist-native.sh enforces this at
# release-build time; this catches drift in ordinary CI (justfile
# `version-check`).
set -eu

cd "$(dirname "$0")/.."

cargo=$(sed -n '/^\[workspace\.package\]/,/^\[/ s/^version = "\(.*\)"/\1/p' Cargo.toml)
sh_v=$(sed -n 's/^version="${ROK_VERSION:-\(.*\)}"/\1/p' install.sh)
ps_v=$(sed -n "/ROK_VERSION/ s/.*else { '\([^']*\)' }.*/\1/p" install.ps1)

fail=0
[ -n "$cargo" ] || {
  echo "check-version.sh: no version in Cargo.toml [workspace.package]" >&2
  fail=1
}
[ -n "$sh_v" ] || {
  echo "check-version.sh: no ROK_VERSION default in install.sh" >&2
  fail=1
}
[ -n "$ps_v" ] || {
  echo "check-version.sh: no ROK_VERSION default in install.ps1" >&2
  fail=1
}
[ "$fail" = 0 ] || exit 1

if [ "$cargo" != "$sh_v" ] || [ "$cargo" != "$ps_v" ]; then
  echo "check-version.sh: release version mismatch" >&2
  echo "  Cargo.toml   $cargo" >&2
  echo "  install.sh   $sh_v" >&2
  echo "  install.ps1  $ps_v" >&2
  echo "bump all three together; the install scripts' ROK_VERSION default must equal the workspace version." >&2
  exit 1
fi

echo "version $cargo is consistent across Cargo.toml, install.sh, install.ps1"
