#!/bin/sh
# The dist-* artifacts of a release commit.
#
#   scripts/release-dist.sh check <dir> <sha>
#       every target's commit-<name> file names <sha>; the files are removed
#   scripts/release-dist.sh find <sha>
#       print the newest run of release-build.yml or release.yml on <sha>
#       that still holds a dist-* artifact for every target, or nothing
#   scripts/release-dist.sh assets <dir>
#       the assets as JSON lines {name, size, sha256}
#
# find needs gh (GH_REPO, GH_TOKEN with actions read).
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)

die() {
  echo "release-dist.sh: $*" >&2
  exit 1
}

sha256_of() {
  { sha256sum "$1" 2> /dev/null || shasum -a 256 "$1"; } | awk '{ print $1 }'
}

check() {
  dir=$1 sha=$2
  want=$(jq '.targets | length' "$root/release-targets.json")
  n=$(find "$dir" -maxdepth 1 -name 'commit-*' | wc -l | tr -d ' ')
  got=$(cat "$dir"/commit-* 2> /dev/null | sort -u)
  if [ "$n" -ne "$want" ] || [ "$got" != "$sha" ]; then
    die "$n of $want builds report a commit ($(echo "$got" | tr '\n' ' ')); the release commit is $sha"
  fi
  rm "$dir"/commit-*
  echo "release-dist.sh: all $want builds are of $sha"
}

find_run() {
  sha=$1
  : "${GH_REPO:?release-dist.sh: set GH_REPO}"
  names=$(jq -r '.targets[] | "dist-\(.name)"' "$root/release-targets.json" | sort)
  for wf in release-build.yml release.yml; do
    gh api "repos/$GH_REPO/actions/workflows/$wf/runs?head_sha=$sha&per_page=20" \
      --jq '.workflow_runs[] | select(.status == "completed") | .id' 2> /dev/null || true
  done | grep -E '^[0-9]+$' | sort -rn | while read -r id; do
    have=$(gh api "repos/$GH_REPO/actions/runs/$id/artifacts?per_page=100" \
      --jq '.artifacts[] | select(.expired | not) | select(.name | startswith("dist-")) | .name' | sort -u)
    if [ "$have" = "$names" ]; then
      echo "$id"
      return 0
    fi
  done
}

assets() {
  for f in "$1"/*; do
    [ -f "$f" ] || continue
    jq -nc --arg n "${f##*/}" --argjson s "$(wc -c < "$f" | tr -d ' ')" --arg h "$(sha256_of "$f")" \
      '{name: $n, size: $s, sha256: $h}'
  done
}

cmd=${1:-}
[ $# -eq 0 ] || shift
case $cmd in
  check)
    [ $# -eq 2 ] || die "usage: check <dir> <sha>"
    check "$1" "$2"
    ;;
  find)
    [ $# -eq 1 ] || die "usage: find <sha>"
    find_run "$1"
    ;;
  assets)
    [ $# -eq 1 ] || die "usage: assets <dir>"
    assets "$1"
    ;;
  *) die "usage: release-dist.sh check|find|assets ..." ;;
esac
