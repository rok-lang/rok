#!/bin/sh
# Make or refresh the release branch of a version: one `release: <version>`
# commit on origin/main that sets the version in Cargo.toml, Cargo.lock,
# install.sh and install.ps1 and puts a CHANGELOG.md section on top,
# generated from the changelog: blocks of the PRs merged since the
# previous tag and grouped by kind.
#
#   scripts/release-prep.sh [--push] [--branch <name>] [--allow-open] <version>
#
# The Summary, Highlights and Migration blocks between their markers are
# written by hand on the branch; a refresh keeps them, rebases onto the
# current main and regenerates the rest, so a PR's line is fixed in its
# description, not on the branch. Nothing is written when the branch is
# already up to date. Fails, listing each, on open issues in the version's
# milestone (only a warning with --allow-open, for a rehearsal) and on
# merged PRs without a valid changelog: block. Without
# --push the commit goes to the local branch only. Needs gh (GH_REPO,
# GH_TOKEN), jq and cargo with the locked crates fetched.
set -eu

usage() {
  echo "usage: scripts/release-prep.sh [--push] [--branch <name>] [--allow-open] <version>" >&2
  exit 2
}
die() {
  echo "release-prep.sh: $*" >&2
  exit 1
}

push=0 branch="" allow_open=0
while [ $# -gt 0 ]; do
  case $1 in
    --push)
      push=1
      shift
      ;;
    --allow-open)
      allow_open=1
      shift
      ;;
    --branch)
      [ $# -ge 2 ] || usage
      branch=$2
      shift 2
      ;;
    -*) usage ;;
    *) break ;;
  esac
done
[ $# -eq 1 ] || usage
version=$1
echo "$version" | grep -Eqx '[0-9]+\.[0-9]+\.[0-9]+' ||
  die "'$version' is not a release version; write X.Y.Z, such as 0.2.1"
branch=${branch:-release/$version}
: "${GH_REPO:?release-prep.sh: set GH_REPO (owner/repo)}"

cd "$(dirname "$0")/.."
root=$(pwd)
# shellcheck source=scripts/changelog-lib.sh
. "$root/scripts/changelog-lib.sh"
release_files='Cargo.toml Cargo.lock install.sh install.ps1 CHANGELOG.md'
tab=$(printf '\t')

tmp=$(mktemp -d "${TMPDIR:-/tmp}/rok-release-prep.XXXXXX")
cleanup() {
  git -C "$root" worktree remove --force "$tmp/wt" > /dev/null 2>&1 || true
  rm -rf "$tmp"
}
trap cleanup EXIT INT TERM

git fetch -q --tags origin "+refs/heads/main:refs/remotes/origin/main"
base=$(git rev-parse refs/remotes/origin/main)
[ -z "$(git ls-remote --tags origin "refs/tags/v$version")" ] ||
  die "v$version is already tagged; a tag never moves, so prepare the next version"
prev=$(changelog_prev_tag "$base" "$version") ||
  die "no release tag on main before $version"
p=${prev#v}

# The version must follow the previous one: next patch, next minor or next major.
next_ok=$(echo "$p $version" | awk '{
  split($1, a, "."); split($2, b, ".")
  ok = (b[1] == a[1] && b[2] == a[2] && b[3] == a[3] + 1) ||
    (b[1] == a[1] && b[2] == a[2] + 1 && b[3] == 0) ||
    (b[1] == a[1] + 1 && b[2] == 0 && b[3] == 0)
  print ok ? "yes" : "no"
}')
[ "$next_ok" = yes ] ||
  die "$version does not follow $p; the next is $(echo "$p" | awk -F. '{ print $1 "." $2 "." $3 + 1 }') (patch) or $(echo "$p" | awk -F. '{ print $1 "." $2 + 1 ".0" }') (minor)"

problems=$tmp/problems
: > "$problems"

ms=$(gh api "repos/$GH_REPO/milestones?state=all&per_page=100" \
  --jq ".[] | select(.title == \"$version\") | .number" < /dev/null)
if [ -n "$ms" ]; then
  gh api --paginate "repos/$GH_REPO/issues?milestone=$ms&state=open&per_page=100" \
    --jq '.[] | "  #\(.number) \(.title)"' < /dev/null > "$tmp/open"
  if [ -s "$tmp/open" ] && [ "$allow_open" = 1 ]; then
    echo "release-prep.sh: warning: milestone $version has open issues:" >&2
    cat "$tmp/open" >&2
  elif [ -s "$tmp/open" ]; then
    {
      echo "milestone $version has open issues; close them or move them to a later milestone:"
      cat "$tmp/open"
    } >> "$problems"
  fi
else
  echo "release-prep.sh: no milestone $version, so no open issues to check" >&2
fi

# kind, PR, text per CHANGELOG line; PRs with only silent lines.
: > "$tmp/entries"
: > "$tmp/silent"
changelog_prs "$prev..$base" > "$tmp/prs"
while read -r n; do
  gh api "repos/$GH_REPO/pulls/$n" < /dev/null > "$tmp/pr.json"
  # Dependabot bumps carry no changelog: block and are never user-visible.
  if [ "$(jq -r .user.login "$tmp/pr.json")" = "dependabot[bot]" ]; then
    echo "$n" >> "$tmp/silent"
    continue
  fi
  jq -r '.body // ""' "$tmp/pr.json" > "$tmp/body"
  if changelog_entries "$tmp/body" > "$tmp/e" 2> "$tmp/err"; then
    loud=0
    while IFS=$tab read -r k t; do
      [ "$k" = silent ] && continue
      printf '%s\t%s\t%s\n' "$k" "$n" "$t" >> "$tmp/entries"
      loud=1
    done < "$tmp/e"
    [ "$loud" = 1 ] || echo "$n" >> "$tmp/silent"
  else
    {
      echo "#$n $(jq -r .title "$tmp/pr.json"): fix the changelog: block in its description:"
      sed 's/^/  /' "$tmp/err"
    } >> "$problems"
  fi
done < "$tmp/prs"

if [ -s "$problems" ]; then
  cat "$problems" >&2
  die "nothing prepared; fix the above, then run release-prep again"
fi

# The hand-written blocks come from the branch if it exists, else from a
# section main already has (a release commit that landed but was never
# tagged).
old=$(git ls-remote origin "refs/heads/$branch" | cut -f1)
if [ -n "$old" ]; then
  git fetch -q origin "refs/heads/$branch"
  mb=$(git merge-base "$old" "$base") ||
    die "$branch shares no history with main; delete it and run release-prep again"
  echo "$release_files" | tr ' ' '\n' > "$tmp/release-files"
  extra=$(git diff --name-only "$mb" "$old" | grep -vxF -f "$tmp/release-files" || true)
  [ -z "$extra" ] ||
    die "$branch changes more than the release files ($(echo "$extra" | tr '\n' ' ')); land those through a PR into main, drop them from the branch, then run release-prep again"
  git show "$old:CHANGELOG.md" > "$tmp/src"
else
  git show "$base:CHANGELOG.md" > "$tmp/src"
fi

# Block $1 of the version's section in $tmp/src, markers included. Status
# 1: no section; 2: a section without the block (deleted by hand).
human_block() {
  awk -v v="$version" -v b="$1" '
    /^## / {
      if (insec) exit
      insec = ($0 == "## " v || index($0, "## " v " (") == 1)
      if (insec) sec = 1
      next
    }
    insec && $0 == "<!-- begin " b " -->" { inb = 1 }
    insec && inb { print; got = 1 }
    insec && $0 == "<!-- end " b " -->" { inb = 0 }
    END { exit sec ? (got ? 0 : 2) : 1 }
  ' "$tmp/src"
}

default_block() {
  echo "<!-- begin $1 -->"
  case $1 in
    summary)
      echo "Summary: TODO one line on what $version brings; the roadmap and rok-lang.org show it"
      ;;
    highlights)
      printf '### Highlights\n\n'
      echo "- TODO the changes to read first; delete this block if there are none"
      ;;
    migration)
      printf '### Migration\n\n'
      echo "- TODO what existing code must change, and how; delete this block if nothing does"
      ;;
  esac
  echo "<!-- end $1 -->"
}

blocks="summary highlights"
case $version in *.0) blocks="$blocks migration" ;; esac
{
  printf '## %s (%s)\n\n' "$version" "$(date -u +%Y-%m-%d)"
  for b in $blocks; do
    st=0
    human_block "$b" > "$tmp/block" || st=$?
    case $st in
      0) cat "$tmp/block" ;;
      1) default_block "$b" ;;
      *) continue ;;
    esac
    echo
  done
  for k in added changed fixed removed; do
    grep "^$k$tab" "$tmp/entries" > "$tmp/kind" || continue
    printf '### %s\n\n' "$(echo "$k" | awk '{ print toupper(substr($0, 1, 1)) substr($0, 2) }')"
    awk -F "$tab" '{
      t = $3
      for (i = 4; i <= NF; i++) t = t "\t" $i
      sub(/\.$/, "", t)
      print "- " toupper(substr(t, 1, 1)) substr(t, 2) " (#" $2 ")"
    }' "$tmp/kind"
    echo
  done
  if [ -s "$tmp/silent" ]; then
    echo "<!-- silent: $(sed 's/^/#/' "$tmp/silent" | tr '\n' ' ' | sed 's/ $//') -->"
    echo
  fi
} > "$tmp/section"

git worktree add -q --detach "$tmp/wt" "$base"
cd "$tmp/wt"

# The section goes on top; an older copy of it is dropped.
awk -v v="$version" -v f="$tmp/section" '
  function put(  l) {
    while ((getline l < f) > 0) print l
    done = 1
  }
  /^## / {
    skip = 0
    if ($0 == "## " v || index($0, "## " v " (") == 1) {
      if (!done) put()
      skip = 1
      next
    }
    if (!done) put()
  }
  !skip { print }
  END { if (!done) put() }
' CHANGELOG.md > "$tmp/out"
cat "$tmp/out" > CHANGELOG.md

awk -v v="$version" '
  /^\[/ { pkg = ($0 == "[workspace.package]") }
  pkg && /^version = "/ { $0 = "version = \"" v "\"" }
  { print }
' Cargo.toml > "$tmp/out"
cat "$tmp/out" > Cargo.toml
sed 's/^version="${ROK_VERSION:-[^}]*}"$/version="${ROK_VERSION:-'"$version"'}"/' install.sh > "$tmp/out"
cat "$tmp/out" > install.sh
sed "/ROK_VERSION/ s/else { '[^']*' }/else { '$version' }/" install.ps1 > "$tmp/out"
cat "$tmp/out" > install.ps1
cargo update --workspace --offline
scripts/check-version.sh > /dev/null

# shellcheck disable=SC2086 # the list is fixed and has no spaces in names
git add -- $release_files
git commit -q -m "release: $version"
new=$(git rev-parse HEAD)
cd "$root"

# The branch is up to date when it is one commit on main with this tree.
if [ -n "$old" ] && [ "$(git rev-parse "$old^")" = "$base" ] &&
  [ "$(git rev-parse "$old^{tree}")" = "$(git rev-parse "$new^{tree}")" ]; then
  echo "release-prep.sh: $branch is up to date at $old ($prev..main, $(wc -l < "$tmp/prs" | tr -d ' ') PRs)"
  new=$old
elif [ "$push" = 1 ]; then
  git push -q --force-with-lease="refs/heads/$branch:$old" origin "$new:refs/heads/$branch"
  echo "release-prep.sh: pushed $branch at $new (${old:+was $old, }$prev..main, $(wc -l < "$tmp/prs" | tr -d ' ') PRs)"
else
  [ "$(git symbolic-ref -q --short HEAD || true)" != "$branch" ] ||
    die "$branch is checked out here; switch away from it or use --push"
  git update-ref "refs/heads/$branch" "$new"
  echo "release-prep.sh: $branch at $new, not pushed ($prev..main, $(wc -l < "$tmp/prs" | tr -d ' ') PRs)"
fi

"$root/scripts/changelog-lint.sh" --draft --rev "$new" "$version"
blocks=$(echo "$blocks" | sed 's/summary //; s/ / and /')
echo "release-prep.sh: next, on $branch: write the Summary line and the $blocks blocks of the $version section in CHANGELOG.md, or delete a block that has nothing to say"
