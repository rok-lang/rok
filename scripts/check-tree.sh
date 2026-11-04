#!/bin/sh
# Fail if the index tracks a gitlink (mode 160000) that .gitmodules does
# not list, or anything under .claude/. Both come from `git add -A` in a
# checkout with agent worktrees in it.
set -eu

cd "$(dirname "$0")/.."

listed=""
[ ! -f .gitmodules ] ||
  listed=$(git config -f .gitmodules --get-regexp '^submodule\..*\.path$' | cut -d' ' -f2-) || true

bad=$(git ls-files --stage | awk -F '\t' '{ split($1, m, " ") } m[1] == "160000" { print $2 }' |
  while IFS= read -r p; do
    printf '%s\n' "$listed" | grep -qxF -- "$p" || echo "gitlink not in .gitmodules: $p"
  done)
claude=$(git ls-files -- .claude | sed 's/^/tracked under .claude\/: /')

if [ -n "$bad$claude" ]; then
  printf '%s\n' "$bad" "$claude" | sed '/^$/d' >&2
  echo "check-tree.sh: drop them with 'git rm --cached <path>'; add files by path, never 'git add -A'" >&2
  exit 1
fi
echo "no stray gitlinks, nothing under .claude/"
