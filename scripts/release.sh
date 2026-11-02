#!/bin/sh
# Check a collected set of release archives and, with --publish, cut the
# GitHub release.
#
#   scripts/release.sh [--dir <dir>] [--publish]
#
# Gather every host's archives, their .sha256 sidecars and rok-version into
# one <dir> (default dist). Without --publish it only checks the full set is
# present and verifies, and that every toolchain archive keeps the installer
# contract (scripts/check-archive.sh), so a release never ships a missing
# archive or one 0.1.x clients cannot install. --publish creates the tag and
# uploads the assets with gh; run it only when the release is meant to go
# out. A -rc.N version is published as a prerelease, so it never becomes
# latest/ and a bare `rok toolchain update` never installs it. It does not
# deploy the sites or publish stones.
set -eu

dir=dist
publish=""
while [ $# -gt 0 ]; do
  case $1 in
    --dir)
      dir=$2
      shift 2
      ;;
    --publish)
      publish=1
      shift
      ;;
    *)
      echo "usage: scripts/release.sh [--dir <dir>] [--publish]" >&2
      exit 2
      ;;
  esac
done

[ -f "$dir/rok-version" ] || {
  echo "release.sh: no $dir/rok-version; build the archives into $dir first (docs/releasing.md)" >&2
  exit 1
}
version=$(cat "$dir/rok-version")
tag=v$version

# <triple> <archive> per target, from release-targets.json (the single
# source of truth). unix: .tar.gz + .tar.xz. win: .zip. Intel Mac and
# Windows on arm64 have no archive (build from source).
targets=$(jq -r '.targets[] | "\(.triple) \(.archive)"' \
  "$(dirname "$0")/../release-targets.json")

problems=0
want() {
  if [ ! -f "$dir/$1" ]; then
    echo "missing   $1" >&2
    problems=$((problems + 1))
    return
  fi
  if [ ! -f "$dir/$1.sha256" ]; then
    echo "no sha256 $1.sha256" >&2
    problems=$((problems + 1))
    return
  fi
  if ! (cd "$dir" && { shasum -a 256 -c "$1.sha256" || sha256sum -c "$1.sha256"; } > /dev/null 2>&1); then
    echo "bad sha   $1" >&2
    problems=$((problems + 1))
    return
  fi
  echo "ok        $1"
}

echo "$targets" | while read -r t kind; do
  [ -n "$t" ] || continue
  case $kind in
    unix)
      want "rok-$version-$t.tar.gz"
      want "rok-$version-$t.tar.xz"
      ;;
    win)
      want "rok-$version-$t.zip"
      ;;
  esac
done > "$dir/.release-check"
cat "$dir/.release-check"
# `while` ran in a subshell (the pipe), so count from its output.
problems=$(grep -c '^\(missing\|no sha256\|bad sha\)' "$dir/.release-check" || true)
rm -f "$dir/.release-check"

# The toolchain archives keep the contract every 0.1.x client relies on.
if [ "${problems:-0}" -eq 0 ]; then
  set --
  for a in "$dir/rok-$version"-*.tar.gz "$dir/rok-$version"-*.tar.xz "$dir/rok-$version"-*.zip; do
    [ ! -f "$a" ] || set -- "$@" "$a"
  done
  [ $# -eq 0 ] || "$(dirname "$0")/check-archive.sh" "$version" "$@" || problems=1
fi

echo
if [ "${problems:-0}" -ne 0 ]; then
  echo "release.sh: $problems problem(s); $tag is not ready" >&2
  exit 1
fi
echo "release.sh: every archive for $tag is present and verified"

if [ -z "$publish" ]; then
  echo "release.sh: dry run; re-run with --publish to create $tag on GitHub"
  exit 0
fi

command -v gh > /dev/null || {
  echo "release.sh: --publish needs the gh CLI" >&2
  exit 1
}
notes="--generate-notes"
[ -f "$dir/notes.md" ] && notes="--notes-file $dir/notes.md"
# A release candidate is a prerelease: GitHub's latest/ skips it, so the
# rok-version there keeps naming the last final release.
pre=""
case $version in *-rc.*) pre=--prerelease ;; esac
# rok-* already matches rok-version; naming it again uploads it twice (422).
# shellcheck disable=SC2086
gh release create "$tag" "$dir"/rok-* \
  --target "$(git rev-parse HEAD)" --title "rok $version" $notes $pre
echo "release.sh: created $tag${pre:+ (a prerelease)}"
