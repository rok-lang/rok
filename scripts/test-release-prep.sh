#!/bin/sh
# release-prep.sh, changelog-lint.sh, pr-lint.sh and the expected-jobs
# check of release-ready.sh against fixtures: a fake history in a local
# origin, PRs and milestones served by a stubbed gh, a stubbed cargo.
# Nothing leaves $TMPDIR.
#
#   scripts/test-release-prep.sh
set -eu

cd "$(dirname "$0")/.."
root=$(pwd)
tmp=$(mktemp -d "${TMPDIR:-/tmp}/rok-test-release-prep.XXXXXX")
trap 'rm -rf "$tmp"' EXIT INT TERM

export HOME="$tmp/home" GIT_CONFIG_NOSYSTEM=1 GIT_AUTHOR_NAME=t GIT_AUTHOR_EMAIL=t@example.com \
  GIT_COMMITTER_NAME=t GIT_COMMITTER_EMAIL=t@example.com GH_REPO=o/r FIX="$tmp/fix"
mkdir -p "$HOME" "$FIX/api/repos/o/r/pulls" "$FIX/api/repos/o/r/actions/workflows/ci.yml" \
  "$FIX/api/repos/o/r/actions/runs/7" "$FIX/runs" "$tmp/stub"
today=$(date -u +%Y-%m-%d)

fail() {
  echo "test-release-prep.sh: $*" >&2
  exit 1
}
ok() { echo "ok        $*"; }

# gh api [--paginate] <path> [--jq <expr>] serves $FIX/api/<path> without
# its query; gh run download <id> -D <dir> copies $FIX/runs/<id>/.
cat > "$tmp/stub/gh" << 'EOF'
#!/bin/sh
case $1 in
  api)
    shift
    path="" jq=""
    while [ $# -gt 0 ]; do
      case $1 in
        --paginate) shift ;;
        --jq)
          jq=$2
          shift 2
          ;;
        *)
          path=$1
          shift
          ;;
      esac
    done
    f=$FIX/api/${path%%\?*}
    [ -f "$f" ] || {
      echo "gh: HTTP 404: $path" >&2
      exit 1
    }
    if [ -n "$jq" ]; then jq -r "$jq" "$f"; else cat "$f"; fi
    ;;
  run)
    run=$3
    [ -d "$FIX/runs/$run" ] || {
      echo "no valid artifacts found to download" >&2
      exit 1
    }
    while [ $# -gt 0 ] && [ "$1" != -D ]; do shift; done
    mkdir -p "$2"
    cp "$FIX/runs/$run"/* "$2/"
    ;;
  *)
    echo "gh stub: $*" >&2
    exit 1
    ;;
esac
EOF
cat > "$tmp/stub/cargo" << 'EOF'
#!/bin/sh
echo "$*" >> "$CARGO_LOG"
[ "$*" = "update --workspace --offline" ] || exit 1
v=$(sed -n '/^\[workspace\.package\]/,/^\[/ s/^version = "\(.*\)"/\1/p' Cargo.toml)
awk -v v="$v" '/^name = "rok"$/ { print; getline; $0 = "version = \"" v "\"" } { print }' Cargo.lock > Cargo.lock.new
mv Cargo.lock.new Cargo.lock
EOF
chmod +x "$tmp/stub/gh" "$tmp/stub/cargo"
PATH=$tmp/stub:$PATH
export PATH CARGO_LOG="$tmp/cargo.log"

pr() {
  jq -n --arg n "$1" --arg login "$2" --arg title "$3" --arg body "$4" \
    '{number: ($n | tonumber), user: {login: $login}, title: $title, body: $body}' \
    > "$FIX/api/repos/o/r/pulls/$1"
}
milestone() {
  echo '[{"title": "0.2.1", "number": 1, "state": "open"}]' > "$FIX/api/repos/o/r/milestones"
  printf '%s\n' "$1" > "$FIX/api/repos/o/r/issues"
}

# A repository at 0.2.0 with four PRs merged since its tag.
git init -q --bare "$tmp/origin.git"
git init -q "$tmp/work"
cd "$tmp/work"
git symbolic-ref HEAD refs/heads/main
mkdir scripts
cp "$root/scripts/release-prep.sh" "$root/scripts/changelog-lint.sh" "$root/scripts/changelog-lib.sh" \
  "$root/scripts/check-version.sh" scripts/
printf '[workspace]\nmembers = []\n\n[workspace.package]\nversion = "0.2.0"\nedition = "2024"\n' > Cargo.toml
printf '[[package]]\nname = "rok"\nversion = "0.2.0"\n\n[[package]]\nname = "serde"\nversion = "1.0.0"\n' > Cargo.lock
printf '#!/bin/sh\nversion="${ROK_VERSION:-0.2.0}"\n' > install.sh
printf "if (-not \$Version) { \$Version = if (\$env:ROK_VERSION) { \$env:ROK_VERSION } else { '0.2.0' } }\n" > install.ps1
printf '# Changelog\n\n## 0.2.0 (2026-11-04)\n\n- The old release.\n' > CHANGELOG.md
git add -A
git commit -q -m "release: 0.2.0"
git tag v0.2.0
merge() {
  echo "$1" >> src.txt
  git add src.txt
  git commit -q -m "$1 (#$2)"
}
merge "feat(lang): a" 10
merge "fix: b" 11
merge "ci: c" 12
merge "chore(deps): bump d" 13
git remote add origin "$tmp/origin.git"
git push -q origin main --tags

pr 10 miniex "feat(lang): a" "$(printf -- '- a\n\nchangelog:\n- added: `a` does a thing.\n- silent: tests\n')"
pr 11 miniex "fix: b" "- b, no block"
pr 12 miniex "ci: c" "$(printf 'changelog:\n- Same code, new result: c\n')"
pr 13 'dependabot[bot]' "chore(deps): bump d" "Bumps d."
milestone '[{"number": 5, "title": "open bug"}]'

prep() {
  "$tmp/work/scripts/release-prep.sh" "$@" > "$tmp/out" 2>&1
}
at() { git ls-remote "$tmp/origin.git" "refs/heads/$1" | cut -f1; }

# Open issues and bad blocks: a list, and nothing pushed.
if prep --push 0.2.1; then fail "release-prep passed with an open issue and bad blocks"; fi
for want in "#5 open bug" "#11 fix: b" "no changelog: block" "#12 ci: c" "has no kind"; do
  grep -qF "$want" "$tmp/out" || fail "no '$want' in: $(cat "$tmp/out")"
done
grep -q "#13" "$tmp/out" && fail "the dependabot PR was reported: $(cat "$tmp/out")"
[ -z "$(at release/0.2.1)" ] || fail "a failed prep pushed a branch"
ok "open issues and PRs without a valid changelog: block are listed, nothing pushed"

milestone '[]'
pr 11 miniex "fix: b" "$(printf 'did b\r\n\r\nchangelog:\r\n- fixed: b no longer\r\n  breaks.\r\n')"
pr 12 miniex "ci: c" "$(printf '<!--\nchangelog:\n- added: not this\n-->\nchangelog:\n- silent: ci only\n\nfooter\n')"
prep --push 0.2.1 || fail "release-prep 0.2.1 failed: $(cat "$tmp/out")"
main=$(git rev-parse main)
b1=$(at release/0.2.1)
[ -n "$b1" ] || fail "no release/0.2.1 on origin: $(cat "$tmp/out")"
git fetch -q origin release/0.2.1
[ "$(git rev-parse "$b1^")" = "$main" ] || fail "release/0.2.1 is not one commit on main"
[ "$(git log -1 --format=%s "$b1")" = "release: 0.2.1" ] || fail "subject: $(git log -1 --format=%s "$b1")"
[ "$(git diff --name-only "$main" "$b1" | tr '\n' ' ')" = "CHANGELOG.md Cargo.lock Cargo.toml install.ps1 install.sh " ] ||
  fail "files: $(git diff --name-only "$main" "$b1")"
git show "$b1:Cargo.toml" | grep -qx 'version = "0.2.1"' || fail "Cargo.toml not bumped"
git show "$b1:Cargo.lock" | grep -A1 -x 'name = "rok"' | grep -qx 'version = "0.2.1"' || fail "Cargo.lock not bumped"
git show "$b1:Cargo.lock" | grep -qx 'version = "1.0.0"' || fail "Cargo.lock lost a dependency"
git show "$b1:install.sh" | grep -qxF 'version="${ROK_VERSION:-0.2.1}"' || fail "install.sh not bumped"
git show "$b1:install.ps1" | grep -qF "else { '0.2.1' }" || fail "install.ps1 not bumped"
grep -qx "update --workspace --offline" "$CARGO_LOG" || fail "cargo: $(cat "$CARGO_LOG")"
cat > "$tmp/want" << EOF
# Changelog

## 0.2.1 ($today)

<!-- begin summary -->
Summary: TODO one line on what 0.2.1 brings; the roadmap and rok-lang.org show it
<!-- end summary -->

<!-- begin highlights -->
### Highlights

- TODO the changes to read first; delete this block if there are none
<!-- end highlights -->

### Added

- \`a\` does a thing (#10)

### Fixed

- B no longer breaks (#11)

<!-- silent: #12 #13 -->

## 0.2.0 (2026-11-04)

- The old release.
EOF
git show "$b1:CHANGELOG.md" > "$tmp/got"
diff -u "$tmp/want" "$tmp/got" || fail "CHANGELOG.md differs"
grep -q "changelog-lint.sh: the 0.2.1 section is well formed (draft" "$tmp/out" || fail "no draft lint: $(cat "$tmp/out")"
ok "release/0.2.1: one release commit, versions bumped, section grouped by kind, silent PRs listed"

prep --push 0.2.1 || fail "second release-prep failed: $(cat "$tmp/out")"
[ "$(at release/0.2.1)" = "$b1" ] || fail "an unchanged second run moved the branch"
grep -q "up to date" "$tmp/out" || fail "second run: $(cat "$tmp/out")"
ok "a second run with nothing new writes nothing"

# The hand-written part, written on the branch in a second commit.
git clone -q -b release/0.2.1 "$tmp/origin.git" "$tmp/edit"
cd "$tmp/edit"
if scripts/changelog-lint.sh 0.2.1 > "$tmp/out" 2>&1; then fail "lint passed a TODO Summary"; fi
grep -q "the summary block still has a TODO" "$tmp/out" || fail "lint: $(cat "$tmp/out")"
awk '
  /^Summary: TODO/ { print "Summary: b stops breaking."; next }
  /^<!-- begin highlights -->$/ { skip = 1 }
  !skip { print }
  /^<!-- end highlights -->$/ { skip = 0; getline }
' CHANGELOG.md > "$tmp/c"
cat "$tmp/c" > CHANGELOG.md
git commit -q -am "docs(changelog): 0.2.1 summary"
git push -q origin release/0.2.1
scripts/changelog-lint.sh 0.2.1 > "$tmp/out" 2>&1 || fail "lint failed a filled section: $(cat "$tmp/out")"
ok "changelog-lint: a TODO Summary fails, a filled one passes"

# main moves on: the refresh rebases, keeps the hand edits, adds the PR.
cd "$tmp/work"
merge "fix: e" 14
git push -q origin main
pr 14 miniex "fix: e" "$(printf 'changelog:\n- fixed: e\n')"
prep --push 0.2.1 || fail "refresh failed: $(cat "$tmp/out")"
b2=$(at release/0.2.1)
git fetch -q origin release/0.2.1
[ "$(git rev-parse "$b2^")" = "$(git rev-parse main)" ] || fail "refresh is not one commit on the new main"
git show "$b2:CHANGELOG.md" > "$tmp/got"
grep -qx "Summary: b stops breaking." "$tmp/got" || fail "refresh lost the Summary"
grep -q "Highlights" "$tmp/got" && fail "refresh brought back the deleted Highlights"
grep -qx -- "- E (#14)" "$tmp/got" || fail "refresh did not add #14"
scripts/changelog-lint.sh --rev "$b2" 0.2.1 > "$tmp/out" 2>&1 || fail "lint after refresh: $(cat "$tmp/out")"
ok "a refresh after main moved: one commit on main, hand edits kept, new PR added"

# A branch that changes more than the release files is refused.
git checkout -q --detach "$b2"
echo x > other.txt
git add other.txt
git commit -q -m "stray"
git push -q origin HEAD:refs/heads/release/0.2.1
git checkout -q main
if prep --push 0.2.1; then fail "prep took a branch with other files"; fi
grep -q "changes more than the release files (other.txt" "$tmp/out" || fail "stray: $(cat "$tmp/out")"
git push -q -f origin "$b2:refs/heads/release/0.2.1"
ok "a branch with other changes is refused"

# Lint failures on hand-made sections.
lint_case() {
  want=$1
  shift
  git checkout -q --detach "$b2"
  "$@"
  git commit -q -am "case"
  r=$(git rev-parse HEAD)
  git checkout -q main
  if scripts/changelog-lint.sh --rev "$r" 0.2.1 > "$tmp/out" 2>&1; then fail "lint passed: $want"; fi
  grep -qF "$want" "$tmp/out" || fail "no '$want' in: $(cat "$tmp/out")"
}
edit() {
  sed "$2" "$1" > "$tmp/c"
  cat "$tmp/c" > "$1"
}
lint_case "#11 (merged since v0.2.0) is not in the section" edit CHANGELOG.md 's/ (#11)$//'
lint_case '"### Fixed" is empty' edit CHANGELOG.md '/^- [BE].* (#1[14])$/d'
lint_case "the first section is '## 0.2.1 (2026-13-01)'" edit CHANGELOG.md "s/^## 0.2.1 .*/## 0.2.1 (2026-13-01)/"
lint_case "Cargo.toml says 0.2.0, not 0.2.1" edit Cargo.toml 's/^version = "0.2.1"/version = "0.2.0"/'
ok "changelog-lint: uncited PR, empty heading, bad date, version fail"

# Other versions.
if prep 0.2.5; then fail "prep took 0.2.5 after 0.2.0"; fi
grep -q "0.2.5 does not follow 0.2.0; the next is 0.2.1 (patch) or 0.3.0 (minor)" "$tmp/out" || fail "0.2.5: $(cat "$tmp/out")"
prep 0.3.0 || fail "prep 0.3.0: $(cat "$tmp/out")"
git show release/0.3.0:CHANGELOG.md | grep -qx "### Migration" || fail "0.3.0 has no Migration block"
[ -z "$(at release/0.3.0)" ] || fail "prep without --push pushed"
git tag v0.2.1 "$b2"
git push -q origin v0.2.1
if prep --push 0.2.1; then fail "prep took a tagged version"; fi
grep -q "v0.2.1 is already tagged" "$tmp/out" || fail "tagged: $(cat "$tmp/out")"
ok "a version that skips one, a series with Migration, a tagged version"

# pr-lint.sh on descriptions.
cd "$root"
pr_lint() {
  printf '%b' "$2" > "$tmp/body"
  if scripts/pr-lint.sh "$tmp/body" > "$tmp/out" 2>&1; then r=pass; else r=fail; fi
  [ "$r" = "$1" ] || fail "pr-lint should $1 on: $2; it said: $(cat "$tmp/out")"
}
pr_lint pass 'x\r\n\r\nchangelog:\r\n- fixed: a\r\n  b\r\n- Silent: c\r\n\r\nfooter\r\n'
grep -q "fixed	a b" "$tmp/out" || fail "pr-lint did not join the continuation: $(cat "$tmp/out")"
pr_lint fail 'no block\n'
pr_lint fail 'changelog:\n- fix: a\n'
pr_lint fail 'changelog:\n\n- added: a\n'
pr_lint fail '<!--\nchangelog:\n- added: a\n-->\n'
pr_lint fail 'changelog:\n- added:\n'
scripts/pr-lint.sh .github/PULL_REQUEST_TEMPLATE.md > "$tmp/out" 2>&1 && fail "pr-lint passed the unfilled template"
ok "pr-lint: kinds, continuations, CRLF, comments, the unfilled template"

# release-ready.sh ci reads the run's expected-jobs.json.
all=$(sed -n "/^ *all='\[/,/\]'/p" .github/workflows/ci.yml | sed "s/^ *all='//; s/'\$//")
cross=$(sed -n "/^ *cross='\[/,/\]'/p" .github/workflows/ci.yml | sed "s/^ *cross='//; s/'\$//")
full() {
  TIER=nightly EVENT=workflow_dispatch JOBS="gate cross wasm bindgen dist" FILTER=${1-} ALL=$all PICKED=$all \
    GATE='["linux-x64", "linux-arm64", "macos-arm64", "windows-x64"]' CROSS=$cross OPTS='["-O0", "-O2"]' \
    WASM='["wasm32-wasip2", "wasm32-wasip1-threads", "wasm32-unknown-unknown"]' \
    BINDGEN='["linux-x64", "windows-x64"]' DIST='["linux-x64", "windows-x64"]' \
    PREFIXES='["linux-arm64", "linux-x64", "macos-arm64", "windows-x64"]' scripts/expected-jobs.sh
}
full > "$tmp/exp.json"
[ "$(jq -r .full "$tmp/exp.json")" = true ] || fail "a full plan is not full"
eval "$(sed -n '/^full_ci_jobs() {/,/^}/p' scripts/release-ready.sh)"
full_ci_jobs | while IFS= read -r j; do
  jq -e --arg j "$j" '.names | index($j)' "$tmp/exp.json" > /dev/null || fail "expected-jobs lacks '$j'"
done
[ "$(jq '.names | length' "$tmp/exp.json")" = 75 ] || fail "expected $(jq '.names | length' "$tmp/exp.json") jobs, not 75"
[ "$(full x | jq -r .full)" = false ] || fail "a filtered plan is full"

echo '{"workflow_runs": [{"id": 7, "event": "workflow_dispatch"}]}' > "$FIX/api/repos/o/r/actions/workflows/ci.yml/runs"
passed() {
  jq '{jobs: [.[] | {name: ., conclusion: "success"}]}' > "$FIX/api/repos/o/r/actions/runs/7/jobs"
}
ready() {
  scripts/release-ready.sh ci 0123456789abcdef0123456789abcdef01234567 > "$tmp/out" 2>&1
}
mkdir -p "$FIX/runs/7"
cp "$tmp/exp.json" "$FIX/runs/7/expected-jobs.json"
jq '.names' "$tmp/exp.json" | passed
ready || fail "release-ready ci failed a full passing run: $(cat "$tmp/out")"
jq '.names - ["wasm (wasm32-wasip2)"]' "$tmp/exp.json" | passed
if ready; then fail "release-ready ci passed a run without wasm (wasm32-wasip2)"; fi
jq '.full = false | .hosts = ["linux-x64"]' "$tmp/exp.json" > "$FIX/runs/7/expected-jobs.json"
jq '.names' "$tmp/exp.json" | passed
if ready; then fail "release-ready ci passed a run that is not full"; fi
rm -rf "$FIX/runs/7"
full_ci_jobs | jq -R . | jq -s . | passed
ready || fail "release-ready ci failed a run without the artifact that passed the built-in list: $(cat "$tmp/out")"
grep -q "no expected-jobs artifact" "$tmp/out" || fail "no fallback notice: $(cat "$tmp/out")"
ok "release-ready: expected-jobs.json of the run, a missing job, a partial run, the built-in list"
