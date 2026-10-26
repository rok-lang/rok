#!/bin/sh
# Fail if the release target triples disagree across release-targets.json and
# the install scripts. release.yml and release.sh read the manifest directly,
# but install.sh / install.ps1 are standalone (curl | sh) and bake their
# supported-triple list in, so this keeps those two in sync (justfile
# `check-targets`, run in CI lint).
set -eu

cd "$(dirname "$0")/.."

triples() { grep -oE '[a-z0-9_]+(-[a-z0-9_]+){2,3}' | sort -u; }

manifest=$(jq -r '.targets[].triple' release-targets.json | sort -u)
sh_list=$(grep -E '^supported=' install.sh | triples)
ps_list=$(grep -E '^\$supported ' install.ps1 | triples)

fail=0
[ -n "$manifest" ] || {
  echo "check-targets.sh: no targets in release-targets.json" >&2
  fail=1
}
[ "$manifest" = "$sh_list" ] || {
  echo "check-targets.sh: install.sh supported triples differ from release-targets.json" >&2
  fail=1
}
[ "$manifest" = "$ps_list" ] || {
  echo "check-targets.sh: install.ps1 supported triples differ from release-targets.json" >&2
  fail=1
}
[ "$fail" = 0 ] || {
  echo "update the supported-triple list in install.sh and install.ps1 to match release-targets.json" >&2
  exit 1
}

echo "release targets consistent across release-targets.json, install.sh, install.ps1"
