#!/bin/sh
# Check the changelog: block of a PR description (pr-lint.yml).
#
#   scripts/pr-lint.sh <file>
#   PR_BODY=... scripts/pr-lint.sh
set -eu

cd "$(dirname "$0")/.."
# shellcheck source=scripts/changelog-lib.sh
. "$(pwd)/scripts/changelog-lib.sh"

if [ $# -ge 1 ]; then
  body=$1
else
  body=$(mktemp "${TMPDIR:-/tmp}/rok-pr-lint.XXXXXX")
  trap 'rm -f "$body"' EXIT INT TERM
  printf '%s\n' "${PR_BODY-}" > "$body"
fi

if out=$(changelog_entries "$body" 2>&1 > /dev/null); then
  changelog_entries "$body" | sed 's/^/pr-lint.sh: /'
else
  echo "pr-lint.sh: the PR description's changelog: block:" >&2
  echo "$out" | sed 's/^/  /' >&2
  echo "edit the description; one line per change, kind $changelog_kinds, as in .github/PULL_REQUEST_TEMPLATE.md" >&2
  exit 1
fi
