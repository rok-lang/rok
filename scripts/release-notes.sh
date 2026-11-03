#!/bin/sh
# Print the CHANGELOG.md section of a version, the GitHub release body.
#
#   scripts/release-notes.sh <version> [<changelog>]
#
# The section is `## <version>` or `## <version> (<date>)`. An -rc.N
# takes its own `## <version>-rc.N` section if there is one, else the final
# version's. Fails if the section is missing or empty.
set -eu

[ $# -ge 1 ] || {
  echo "usage: scripts/release-notes.sh <version> [<changelog>]" >&2
  exit 2
}
version=$1
changelog=${2:-$(dirname "$0")/../CHANGELOG.md}

# The body under `## <v>` or `## <v> (<date>)`, blank edges trimmed.
section() {
  awk -v v="$1" '
    /^## / {
      if (found) exit
      if ($0 == "## " v || index($0, "## " v " (") == 1) found = 1
      next
    }
    found {
      if ($0 ~ /^[[:space:]]*$/) { blank++; next }
      if (seen) while (blank-- > 0) print ""
      blank = 0; seen = 1; print
    }
  ' "$changelog"
}

notes=$(section "$version")
case $version in
  *-rc.*) [ -n "$notes" ] || notes=$(section "${version%-rc.*}") ;;
esac
[ -n "$notes" ] || {
  echo "release-notes.sh: no non-empty '## $version' section in $changelog" >&2
  exit 1
}
printf '%s\n' "$notes"
