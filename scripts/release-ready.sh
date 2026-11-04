#!/bin/sh
# The GitHub side of release-ready.yml: runs that must pass on the exact
# commit of a release before it is tagged.
#
#   scripts/release-ready.sh ci <sha> [<branch>]
#   scripts/release-ready.sh dry-run <sha> [<branch>]
#   scripts/release-ready.sh verify <sha>
#
# ci: a ci run with every host and job (jobs=all, hosts=all) passed on
# <sha>. dry-run: a manual release.yml run (build and verify, no publish)
# passed on <sha>. Either reuses a passing run of <sha> if there is one,
# else dispatches one on <branch>, which must point at <sha>, and waits.
# verify: the release-ready status on <sha> is success and was set by a
# release-ready run whose `ready (<sha>)` job passed. Needs gh with
# GH_REPO and GH_TOKEN.
set -eu

usage() {
  echo "usage: scripts/release-ready.sh ci|dry-run <sha> [<branch>] | verify <sha>" >&2
  exit 2
}
[ $# -ge 2 ] || usage
cmd=$1 sha=$2 branch=${3-}
: "${GH_REPO:?release-ready.sh: set GH_REPO (owner/repo)}"

die() {
  echo "release-ready.sh: $*" >&2
  exit 1
}

# The jobs a full ci run has, by name (ci.yml plan with hosts=all jobs=all).
full_ci_jobs() {
  for h in linux-x64 linux-arm64 macos-arm64 windows-x64; do
    echo "check ($h)"
    for p in 1 2 3; do echo "test ($h $p/3)"; done
    for o in -O0 -O2; do
      for s in 1 2 3 4; do echo "corpus ($h $o $s/4)"; done
    done
  done
  for t in i686-unknown-linux-gnu armv7-unknown-linux-gnueabihf x86_64-unknown-linux-musl \
    aarch64-unknown-linux-musl i686-pc-windows-msvc aarch64-pc-windows-msvc; do
    for o in -O0 -O2; do echo "cross ($t $o)"; done
  done
  for t in wasm32-wasip2 wasm32-wasip1-threads wasm32-unknown-unknown; do echo "wasm ($t)"; done
  for h in linux-x64 windows-x64; do
    echo "bindgen ($h)"
    echo "dist ($h)"
  done
  echo cross-link
  echo ci-gate
}

# Run $1 passed every job a full ci run has; prints what it lacks.
full_ci() {
  gh api --paginate "repos/$GH_REPO/actions/runs/$1/jobs?per_page=100" \
    --jq '.jobs[] | select(.conclusion == "success") | .name' > "$tmp/passed"
  full_ci_jobs | while IFS= read -r j; do
    grep -qxF "$j" "$tmp/passed" || echo "  $j"
  done > "$tmp/lacks"
  [ ! -s "$tmp/lacks" ]
}

# Passing runs of workflow $1 on $sha by a manual run or the schedule.
passed_runs() {
  gh api "repos/$GH_REPO/actions/workflows/$1/runs?head_sha=$sha&status=success&per_page=100" \
    --jq '.workflow_runs[] | select(.event == "workflow_dispatch" or .event == "schedule") | .id'
}

# Dispatch workflow $1 on $branch with the -f fields after it and wait for
# its run of $sha; prints the run id once it completed.
dispatch() {
  wf=$1
  shift
  [ -n "$branch" ] || die "no passing $wf run on $sha and no branch at $sha to dispatch one; run release-ready with ref set to that branch"
  at=$(git ls-remote origin "refs/heads/$branch" | cut -f1)
  [ "$at" = "$sha" ] || die "branch $branch is on ${at:-nothing}, not $sha; dispatch on a branch whose head is the release commit"
  since=$(date -u +%Y-%m-%dT%H:%M:%SZ)
  gh workflow run "$wf" --ref "$branch" "$@" >&2
  q="repos/$GH_REPO/actions/workflows/$wf/runs?head_sha=$sha&event=workflow_dispatch&created=%3E%3D$since"
  id=""
  i=0
  while [ -z "$id" ]; do
    i=$((i + 1))
    [ "$i" -le 30 ] || die "the dispatched $wf run did not show up"
    sleep 20
    id=$(gh api "$q" --jq '.workflow_runs[0].id // empty')
  done
  echo "release-ready.sh: waiting for $wf run $id" >&2
  while [ "$(gh api "repos/$GH_REPO/actions/runs/$id" --jq .status)" != completed ]; do
    sleep 60
  done
  echo "$id"
}

tmp=$(mktemp -d "${TMPDIR:-/tmp}/rok-release-ready.XXXXXX")
trap 'rm -rf "$tmp"' EXIT INT TERM

case $cmd in
  ci)
    for id in $(passed_runs ci.yml); do
      if full_ci "$id"; then
        echo "release-ready.sh: full ci passed on $sha in run $id"
        exit 0
      fi
    done
    id=$(dispatch ci.yml -f hosts=all -f jobs=all -f filter=)
    full_ci "$id" || {
      cat "$tmp/lacks" >&2
      die "ci run $id on $sha did not pass these jobs"
    }
    echo "release-ready.sh: full ci passed on $sha in run $id"
    ;;
  dry-run)
    id=$(passed_runs release.yml | head -n 1)
    if [ -z "$id" ]; then
      id=$(dispatch release.yml)
      c=$(gh api "repos/$GH_REPO/actions/runs/$id" --jq .conclusion)
      [ "$c" = success ] || die "release.yml dry run $id on $sha: $c"
    fi
    echo "release-ready.sh: release.yml dry run passed on $sha in run $id"
    ;;
  verify)
    url=$(gh api "repos/$GH_REPO/commits/$sha/statuses?per_page=100" \
      --jq '[.[] | select(.context == "release-ready")][0] | select(.state == "success") | .target_url')
    id=${url##*/runs/}
    id=${id%%/*}
    case $id in
      '' | *[!0-9]*) die "$sha is not release-ready; run the release-ready workflow on it before tagging (docs/releasing.md)" ;;
    esac
    path=$(gh api "repos/$GH_REPO/actions/runs/$id" --jq .path)
    [ "$path" = .github/workflows/release-ready.yml ] ||
      die "the release-ready status on $sha points at run $id of $path, not release-ready.yml"
    ok=$(gh api --paginate "repos/$GH_REPO/actions/runs/$id/jobs?per_page=100" \
      --jq ".jobs[] | select(.name == \"ready ($sha)\" and .conclusion == \"success\") | .id")
    [ -n "$ok" ] || die "release-ready run $id did not pass its ready ($sha) job"
    echo "release-ready.sh: $sha is release-ready (run $id)"
    ;;
  *) usage ;;
esac
