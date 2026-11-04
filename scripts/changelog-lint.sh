#!/bin/sh
# Check the CHANGELOG.md section of a version at a commit (default HEAD):
#
#   scripts/changelog-lint.sh [--draft] [--rev <commit>] <version>
#
# - Cargo.toml sets the version, and the first section is
#   `## <version> (YYYY-MM-DD)`, the only one for it;
# - no `###` heading is empty and every begin/end marker is paired;
# - every PR merged since the previous tag is cited as (#N) or listed in
#   the `<!-- silent: #N ... -->` line;
# - unless --draft (the state release-prep.sh leaves): the Summary line
#   is written and no TODO is left in a hand-written block.
set -eu

usage() {
  echo "usage: scripts/changelog-lint.sh [--draft] [--rev <commit>] <version>" >&2
  exit 2
}

draft=0 rev=HEAD
while [ $# -gt 0 ]; do
  case $1 in
    --draft)
      draft=1
      shift
      ;;
    --rev)
      [ $# -ge 2 ] || usage
      rev=$2
      shift 2
      ;;
    -*) usage ;;
    *) break ;;
  esac
done
[ $# -eq 1 ] || usage
version=$1

cd "$(dirname "$0")/.."
# shellcheck source=scripts/changelog-lib.sh
. "$(pwd)/scripts/changelog-lib.sh"

tmp=$(mktemp -d "${TMPDIR:-/tmp}/rok-changelog-lint.XXXXXX")
trap 'rm -rf "$tmp"' EXIT INT TERM
errs=$tmp/errs
: > "$errs"
err() { echo "  $*" >> "$errs"; }

git show "$rev:Cargo.toml" > "$tmp/cargo"
git show "$rev:CHANGELOG.md" > "$tmp/changelog"

cargo=$(sed -n '/^\[workspace\.package\]/,/^\[/ s/^version = "\(.*\)"/\1/p' "$tmp/cargo")
[ "$cargo" = "$version" ] || err "Cargo.toml says ${cargo:-no version}, not $version"

first=$(grep -m 1 '^## ' "$tmp/changelog" || true)
echo "$first" | grep -Eqx "## $(echo "$version" | sed 's/\./\\./g') \(20[0-9]{2}-(0[1-9]|1[0-2])-(0[1-9]|[12][0-9]|3[01])\)" ||
  err "the first section is '${first:-none}'; it must be '## $version (YYYY-MM-DD)'"
count=$(awk -v v="$version" '$0 == "## " v || index($0, "## " v " (") == 1 { n++ } END { print n + 0 }' "$tmp/changelog")
[ "$count" -le 1 ] || err "there are $count sections for $version; keep one"

awk -v v="$version" '
  /^## / {
    if (insec) exit
    insec = ($0 == "## " v || index($0, "## " v " (") == 1)
    next
  }
  insec { print }
' "$tmp/changelog" > "$tmp/section"

# A heading needs something under it before the next heading or marker.
awk '
  function check() { if (h != "" && !body) print "\"" h "\" is empty; add its lines or delete the heading" }
  /^#+ / { check(); h = $0; body = 0; next }
  /^<!-- (begin|end) [a-z]+ -->$/ { check(); h = ""; next }
  /^[[:space:]]*$/ || /^<!--.*-->$/ { next }
  { body = 1 }
  END { check() }
' "$tmp/section" | while IFS= read -r l; do err "$l"; done

awk '
  /^<!-- begin [a-z]+ -->$/ {
    if (open != "") print "block " open " has no end marker before \"" $0 "\""
    open = $3
  }
  /^<!-- end [a-z]+ -->$/ {
    if (open != $3) print "\"" $0 "\" closes " (open == "" ? "nothing" : open)
    open = ""
  }
  END { if (open != "") print "block " open " has no end marker" }
' "$tmp/section" | while IFS= read -r l; do err "$l"; done

# Every #N in the section counts: an entry's (#N) or the silent line.
grep -o '#[0-9][0-9]*' "$tmp/section" | sort -u > "$tmp/cited" || true
if prev=$(changelog_prev_tag "$rev" "$version"); then
  changelog_prs "$prev..$rev" | while IFS= read -r n; do
    grep -qx "#$n" "$tmp/cited" ||
      err "#$n (merged since $prev) is not in the section; run release-prep again or add it to <!-- silent: -->"
  done
else
  err "no release tag before $version reachable from $rev"
fi

if [ "$draft" = 0 ]; then
  summary=$(awk '/^<!-- begin summary -->$/ { b = 1; next } /^<!-- end summary -->$/ { b = 0 } b && /^Summary:/' "$tmp/section")
  s=$(echo "$summary" | sed 's/^Summary:[[:space:]]*//')
  if [ -z "$summary" ]; then
    err "no Summary: line between <!-- begin summary --> and <!-- end summary -->; write one line on what $version brings"
  elif [ -z "$s" ]; then
    err "the Summary: line is empty; write one line on what $version brings"
  fi
  awk '
    /^<!-- begin [a-z]+ -->$/ { b = $3; next }
    /^<!-- end [a-z]+ -->$/ { b = ""; next }
    b != "" && /TODO/ { print "the " b " block still has a TODO: " $0 }
  ' "$tmp/section" | while IFS= read -r l; do err "$l"; done
fi

if [ -s "$errs" ]; then
  echo "changelog-lint.sh: the $version section of CHANGELOG.md at $rev:" >&2
  cat "$errs" >&2
  exit 1
fi
if [ "$draft" = 1 ]; then
  echo "changelog-lint.sh: the $version section is well formed (draft: Summary and TODOs not checked)"
else
  echo "changelog-lint.sh: the $version section is ready"
fi
