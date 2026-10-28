#!/bin/sh
# Rewrite nix/sources.json for release <version>: the archive URL and
# SHA-256 the flake fetches on each Nix system, read from the release's
# .sha256 assets. Run after the release is published.
#
#   scripts/nix-sources.sh 0.1.2
set -eu

v=${1:?usage: $0 <version>}
base=https://github.com/rok-lang/rok/releases/download/v$v
root=$(cd "$(dirname "$0")/.." && pwd)

entry() {
  sys=$1 triple=$2
  a=rok-$v-$triple.tar.xz
  sum=$(curl -fsSL "$base/$a.sha256" | cut -d' ' -f1)
  [ ${#sum} -eq 64 ] || {
    echo "no sha256 for $a" >&2
    exit 1
  }
  printf '    "%s": { "url": "%s/%s", "sha256": "%s" }' "$sys" "$base" "$a" "$sum"
}

{
  printf '{\n  "version": "%s",\n  "archives": {\n' "$v"
  entry aarch64-darwin aarch64-apple-darwin
  printf ',\n'
  entry aarch64-linux aarch64-unknown-linux-musl
  printf ',\n'
  entry x86_64-linux x86_64-unknown-linux-musl
  printf '\n  }\n}\n'
} > "$root/nix/sources.json"
echo "nix/sources.json: rok $v"
