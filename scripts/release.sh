#!/bin/sh
# Check a collected set of release archives and, with --publish, cut the
# GitHub release.
#
#   scripts/release.sh [--dir <dir>] [--publish [--commit <sha>]]
#
# Gather every host's archives, their .sha256 sidecars and rok-version into
# one <dir> (default dist). Without --publish it only checks the full set is
# present and verifies, and that every toolchain archive keeps the installer
# contract (scripts/check-archive.sh), so a release never ships a missing
# archive or one a released client cannot install. --publish needs the tag
# v<rok-version> already on origin and still on <sha> (the commit the
# archives were built from, default HEAD). It uploads the planned assets to
# a draft release with <dir>/notes.md (scripts/release-notes.sh) as the
# body, checks the draft holds exactly them, checks the tag again and only
# then makes the release public. A -rc.N version is published as a
# prerelease, so it never becomes latest/ and a bare `rok toolchain update`
# never installs it. A tag build whose tag is not v<rok-version> fails. It
# does not deploy the sites or publish stones.
set -eu

dir=dist
publish=""
commit=""
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
    --commit)
      commit=$2
      shift 2
      ;;
    *)
      echo "usage: scripts/release.sh [--dir <dir>] [--publish [--commit <sha>]]" >&2
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

# A tag build must ship the version it was tagged for.
if [ "${GITHUB_REF_TYPE-}" = tag ] && [ "${GITHUB_REF_NAME-}" != "$tag" ]; then
  echo "release.sh: tag $GITHUB_REF_NAME but $dir/rok-version is $version; tag the commit that sets $version, or fix the version" >&2
  exit 1
fi

# <triple> <archive> per target, from release-targets.json (the single
# source of truth). unix: .tar.gz + .tar.xz. win: .zip. Intel Mac and
# Windows on arm64 have no archive (build from source).
targets=$(jq -r '.targets[] | "\(.triple) \(.archive)"' \
  "$(dirname "$0")/../release-targets.json")
bad=$(echo "$targets" | awk '$2 != "unix" && $2 != "win"')
[ -z "$bad" ] || {
  echo "release.sh: unknown archive in release-targets.json: $bad" >&2
  exit 1
}

# Every asset the release carries, one per line: each archive, its .sha256
# and rok-version.
plan() {
  echo "$targets" | while read -r t kind; do
    [ -n "$t" ] || continue
    case $kind in
      unix) set -- "rok-$version-$t.tar.gz" "rok-$version-$t.tar.xz" ;;
      *) set -- "rok-$version-$t.zip" ;;
    esac
    for a in "$@"; do
      echo "$a"
      echo "$a.sha256"
    done
  done
  echo rok-version
}
planned=$(plan)

want() {
  if [ ! -f "$dir/$1" ]; then
    echo "missing   $1"
    return
  fi
  if [ ! -f "$dir/$1.sha256" ]; then
    echo "no sha256 $1.sha256"
    return
  fi
  if ! (cd "$dir" && { shasum -a 256 -c "$1.sha256" || sha256sum -c "$1.sha256"; } > /dev/null 2>&1); then
    echo "bad sha   $1"
    return
  fi
  echo "ok        $1"
}

for a in $planned; do
  case $a in *.sha256 | rok-version) ;; *) want "$a" ;; esac
done > "$dir/.release-check"
cat "$dir/.release-check"
problems=$(grep -c '^\(missing\|no sha256\|bad sha\)' "$dir/.release-check" || true)
rm -f "$dir/.release-check"

# The toolchain archives keep the contract every released client relies on.
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
[ -s "$dir/notes.md" ] || {
  echo "release.sh: no $dir/notes.md; write it with scripts/release-notes.sh $version > $dir/notes.md" >&2
  exit 1
}
[ -n "$commit" ] || commit=$(git rev-parse HEAD)

# A moved or deleted tag would publish these archives under another
# commit. ^{} is the commit an annotated tag points at.
tag_on_commit() {
  refs=$(git ls-remote origin "refs/tags/$tag" "refs/tags/$tag^{}") || {
    echo "release.sh: git ls-remote origin failed" >&2
    return 1
  }
  at=$(echo "$refs" | awk '$2 ~ /\^\{\}$/ { p = $1 } { s = $1 } END { print p ? p : s }')
  if [ -z "$at" ]; then
    echo "release.sh: tag $tag is not on origin; push it before publishing" >&2
    return 1
  fi
  if [ "$at" != "$commit" ]; then
    echo "release.sh: tag $tag is on $at, but the archives were built from $commit; a tag never moves, ship the fix as the next version" >&2
    return 1
  fi
}
tag_on_commit

# A release candidate is a prerelease: GitHub's latest/ skips it, so the
# rok-version there keeps naming the last final release.
pre=""
case $version in *-rc.*) pre=--prerelease ;; esac
set --
for a in $planned; do
  set -- "$@" "$dir/$a"
done
# A draft is not public and latest/ skips it, so a failed upload never
# leaves a partial release behind.
# shellcheck disable=SC2086
gh release create "$tag" "$@" --draft --verify-tag \
  --title "rok $version" --notes-file "$dir/notes.md" $pre

# The draft holds exactly the planned assets, each at its local size.
for a in $planned; do
  echo "$a $(wc -c < "$dir/$a" | tr -d ' ')"
done | sort > "$dir/.release-want"
gh release view "$tag" --json assets --jq '.assets[] | "\(.name) \(.size)"' | sort > "$dir/.release-got"
n=$(wc -l < "$dir/.release-want" | tr -d ' ')
if ! cmp -s "$dir/.release-want" "$dir/.release-got"; then
  echo "release.sh: draft $tag does not hold the $n planned assets (< planned, > uploaded):" >&2
  diff "$dir/.release-want" "$dir/.release-got" >&2 || true
  rm -f "$dir/.release-want" "$dir/.release-got"
  echo "release.sh: the draft stays unpublished; delete it with 'gh release delete $tag' and rerun" >&2
  exit 1
fi
rm -f "$dir/.release-want" "$dir/.release-got"
echo "release.sh: draft $tag holds all $n planned assets"

tag_on_commit || {
  echo "release.sh: the draft stays unpublished" >&2
  exit 1
}
gh release edit "$tag" --draft=false
echo "release.sh: published $tag${pre:+ (a prerelease)}"
