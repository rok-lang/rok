#!/bin/sh
# Check a collected set of 0.1.0 release archives and, only with
# --publish, cut the GitHub release (spec 9.2.19, docs/releasing.md).
#
#   scripts/release.sh [--dir <dir>] [--publish]
#
# Build every host's archives first (docs/releasing.md) and gather them,
# with their .sha256 sidecars and the rok-version file, into one <dir>
# (default dist). Without --publish this only checks that the full set is
# present and every checksum verifies, so a release never ships with a
# missing archive (a dead download link). --publish then creates the tag
# and uploads the assets with the gh CLI.
#
# --publish is the release itself: run it only once the release is meant
# to go out. It does not deploy the sites or publish the stones; those
# are separate steps in docs/releasing.md.
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

# <triple> <kind>. unix: .tar.gz + .tar.xz + the rok-mlir-dev .tar.gz.
# win: .zip + the rok-mlir-dev .zip. musl: .tar.gz + .tar.xz, static, no
# dev bundle. These six are what 0.1.0 ships; Intel Mac and Windows on
# arm64 have no archive (build from source).
targets="
aarch64-apple-darwin unix
x86_64-unknown-linux-gnu unix
aarch64-unknown-linux-gnu unix
x86_64-pc-windows-msvc win
x86_64-unknown-linux-musl musl
aarch64-unknown-linux-musl musl
"

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
      want "rok-mlir-dev-$version-$t.tar.gz"
      ;;
    win)
      want "rok-$version-$t.zip"
      want "rok-mlir-dev-$version-$t.zip"
      ;;
    musl)
      want "rok-$version-$t.tar.gz"
      want "rok-$version-$t.tar.xz"
      ;;
  esac
done > "$dir/.release-check"
cat "$dir/.release-check"
# `while` ran in a subshell (the pipe), so count from its output.
problems=$(grep -c '^\(missing\|no sha256\|bad sha\)' "$dir/.release-check" || true)
rm -f "$dir/.release-check"

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
# shellcheck disable=SC2086
gh release create "$tag" "$dir"/rok-* "$dir/rok-version" \
  --target "$(git rev-parse HEAD)" --title "rok $version" $notes
echo "release.sh: created $tag"
