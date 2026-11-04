#!/bin/sh
# Dry runs of release.sh --publish against a stubbed gh (and git): an
# -rc.N version is created as a prerelease, a final one is not, the body
# is notes.md, the release stays a draft until it holds every planned
# asset, and a missing notes.md or archive, a tag that names another
# version or sits on another commit, or an archive that breaks the
# installer contract stops the release before gh runs. release-notes.sh
# picks the right CHANGELOG section. Fake archives for every target in
# release-targets.json; nothing is built or uploaded.
#
#   scripts/test-release.sh
set -eu

cd "$(dirname "$0")/.."
root=$(pwd)
tmp=$(mktemp -d "${TMPDIR:-/tmp}/rok-test-release.XXXXXX")
trap 'rm -rf "$tmp"' EXIT INT TERM

# gh records its arguments and serves back the files `release create` got,
# less $GH_DROP. git puts the tag on $TAG_AT (empty: no tag), an annotated
# tag when $TAG_OBJ is set.
head=0123456789abcdef0123456789abcdef01234567
mkdir "$tmp/stub"
cat > "$tmp/stub/gh" << 'EOF'
#!/bin/sh
printf '%s\n' "$*" >> "$GH_LOG"
case "$1 $2" in
  "release create")
    shift 3
    : > "$GH_LOG.assets"
    for a; do
      case $a in --*) break ;; esac
      [ "${a##*/}" = "${GH_DROP-}" ] || echo "${a##*/} $(wc -c < "$a" | tr -d ' ')" >> "$GH_LOG.assets"
    done
    ;;
  "release view") cat "$GH_LOG.assets" ;;
esac
EOF
cat > "$tmp/stub/git" << EOF
#!/bin/sh
case \$1 in
  ls-remote)
    [ -n "\$TAG_AT" ] || exit 0
    if [ -n "\${TAG_OBJ-}" ]; then
      printf '%s\t%s\n' "\$TAG_OBJ" "\$3" "\$TAG_AT" "\$4"
    else
      printf '%s\t%s\n' "\$TAG_AT" "\$3"
    fi
    ;;
  *) echo $head ;;
esac
EOF
chmod +x "$tmp/stub/gh" "$tmp/stub/git"

sum() {
  (cd "$1" && { sha256sum "$2" 2> /dev/null || shasum -a 256 "$2"; } > "$2.sha256")
}

# A release set for version $1 in $tmp/dist-$1; with $2, the toolchain
# archives leave out that file.
release_set() {
  v=$1
  d=$tmp/dist-$v
  rm -rf "$d"
  mkdir -p "$d"
  printf '%s\n' "$v" > "$d/rok-version"
  printf -- '- notes for %s\n' "$v" > "$d/notes.md"
  jq -r '.targets[] | "\(.triple) \(.archive)"' release-targets.json | while read -r t kind; do
    n=rok-$v-$t
    s=$tmp/stage/$n
    rm -rf "$tmp/stage"
    mkdir -p "$s/bin" "$s/lib/rok/core/src"
    case $kind in
      win) files="bin/rok.exe bin/rokc.exe bin/rokls.exe lib/rokr.lib" ;;
      *) files="bin/rok bin/rokc bin/rokls lib/librokr.a" ;;
    esac
    for f in $files lib/rok/core/src/lib.rok; do
      [ "$f" = "${2-}" ] || : > "$s/$f"
    done
    case $kind in
      win)
        (cd "$tmp/stage" && python3 -m zipfile -c "$d/$n.zip" "$n")
        sum "$d" "$n.zip"
        ;;
      *)
        tar -C "$tmp/stage" -czf "$d/$n.tar.gz" "$n"
        tar -C "$tmp/stage" -cJf "$d/$n.tar.xz" "$n"
        sum "$d" "$n.tar.gz"
        sum "$d" "$n.tar.xz"
        ;;
    esac
  done
}

fail() {
  echo "test-release.sh: $*" >&2
  exit 1
}

# release.sh --publish for version $1, as a build of tag $2 if given; its
# gh calls in $tmp/gh.log.
publish() {
  : > "$tmp/gh.log"
  GITHUB_REF_TYPE=${2:+tag} GITHUB_REF_NAME=${2-} GH_LOG=$tmp/gh.log TAG_AT=${TAG_AT-$head} \
    PATH=$tmp/stub:$PATH "$root/scripts/release.sh" --dir "$tmp/dist-$1" --publish > "$tmp/out" 2>&1
}

release_set 0.2.0-rc.1
publish 0.2.0-rc.1 || {
  cat "$tmp/out" >&2
  fail "release.sh --publish 0.2.0-rc.1 failed"
}
grep -q '^release create v0.2.0-rc.1 .* --prerelease$' "$tmp/gh.log" ||
  fail "0.2.0-rc.1 is not created as a prerelease: $(cat "$tmp/gh.log")"
echo "ok        v0.2.0-rc.1 is a prerelease"

release_set 0.2.0
publish 0.2.0 || {
  cat "$tmp/out" >&2
  fail "release.sh --publish 0.2.0 failed"
}
grep -q '^release create v0.2.0 ' "$tmp/gh.log" ||
  fail "0.2.0 is not created: $(cat "$tmp/gh.log")"
! grep -q -- '--prerelease' "$tmp/gh.log" ||
  fail "0.2.0 is created as a prerelease: $(cat "$tmp/gh.log")"
grep -q -- "--notes-file $tmp/dist-0.2.0/notes.md" "$tmp/gh.log" ||
  fail "0.2.0 does not use notes.md: $(cat "$tmp/gh.log")"
echo "ok        v0.2.0 is a full release with notes.md"

publish 0.2.0 v0.2.0 || {
  cat "$tmp/out" >&2
  fail "release.sh --publish 0.2.0 on tag v0.2.0 failed"
}
echo "ok        tag v0.2.0 publishes 0.2.0"
grep -q '^release create v0.2.0 .* --draft --verify-tag ' "$tmp/gh.log" ||
  fail "0.2.0 is not created as a draft of the pushed tag: $(cat "$tmp/gh.log")"
grep -q '^release view v0.2.0 ' "$tmp/gh.log" ||
  fail "the 0.2.0 draft assets are not checked: $(cat "$tmp/gh.log")"
[ "$(tail -n 1 "$tmp/gh.log")" = "release edit v0.2.0 --draft=false" ] ||
  fail "0.2.0 is not made public last: $(cat "$tmp/gh.log")"
echo "ok        v0.2.0 goes public only after the draft is checked"

(TAG_OBJ=fedcba9876543210fedcba9876543210fedcba98 publish 0.2.0 v0.2.0) || {
  cat "$tmp/out" >&2
  fail "release.sh --publish 0.2.0 on an annotated tag failed"
}
echo "ok        an annotated tag on the built commit publishes"

if (TAG_AT=fedcba9876543210fedcba9876543210fedcba98 publish 0.2.0 v0.2.0); then
  fail "release.sh --publish ran with the tag on another commit"
fi
grep -q "^release.sh: tag v0.2.0 is on fedcba98" "$tmp/out" ||
  fail "release.sh did not name the moved tag: $(cat "$tmp/out")"
[ ! -s "$tmp/gh.log" ] || fail "gh ran with the tag on another commit"
echo "ok        a moved tag stops the release"

if (TAG_AT='' publish 0.2.0 v0.2.0); then
  fail "release.sh --publish ran without the tag on origin"
fi
[ ! -s "$tmp/gh.log" ] || fail "gh ran without the tag on origin"
echo "ok        a tag missing on origin stops the release"

if (GH_DROP=rok-version publish 0.2.0 v0.2.0); then
  fail "release.sh --publish made a release public without rok-version"
fi
grep -q '^release create ' "$tmp/gh.log" || fail "no draft was created: $(cat "$tmp/gh.log")"
! grep -q '^release edit ' "$tmp/gh.log" || fail "a draft without rok-version went public"
echo "ok        a draft missing an asset stays a draft"

rm "$tmp/dist-0.2.0/rok-0.2.0-x86_64-pc-windows-msvc.zip"
if publish 0.2.0; then
  fail "release.sh --publish ran without the windows archive"
fi
grep -q '^missing   rok-0.2.0-x86_64-pc-windows-msvc.zip' "$tmp/out" ||
  fail "release.sh did not name the missing archive: $(cat "$tmp/out")"
[ ! -s "$tmp/gh.log" ] || fail "gh ran without the windows archive"
echo "ok        a missing archive stops the release"
release_set 0.2.0

if publish 0.2.0 v0.2.1; then
  fail "release.sh --publish 0.2.0 ran on tag v0.2.1"
fi
grep -q "^release.sh: tag v0.2.1 but .* is 0.2.0" "$tmp/out" ||
  fail "release.sh did not name the tag mismatch: $(cat "$tmp/out")"
[ ! -s "$tmp/gh.log" ] || fail "gh ran on a mismatched tag"
echo "ok        tag v0.2.1 stops a 0.2.0 release"

rm "$tmp/dist-0.2.0/notes.md"
if publish 0.2.0; then
  fail "release.sh --publish ran without notes.md"
fi
[ ! -s "$tmp/gh.log" ] || fail "gh ran without notes.md"
echo "ok        no notes.md stops the release"

for missing in lib/rok/core/src/lib.rok bin/rokc lib/librokr.a; do
  release_set 0.2.0 "$missing"
  if publish 0.2.0; then
    fail "release.sh --publish took archives without $missing"
  fi
  grep -q "^breaks .*: no rok-0.2.0-[^/]*/$missing" "$tmp/out" ||
    fail "release.sh did not name the missing $missing: $(cat "$tmp/out")"
  [ ! -s "$tmp/gh.log" ] || fail "gh ran for archives without $missing"
  echo "ok        an archive without $missing stops the release"
done

# An rc without its own section takes the final version's.
cat > "$tmp/CHANGELOG.md" << 'EOF'
# Changelog

## 0.2.1-rc.1

- rc only.

## 0.2.1 (2026-01-02)

- patch.

## 0.2.0 (2026-01-01)

- Breaking: one.

- two.

## 0.1.9 (2025-12-01)

## 0.1.8 (2025-11-01)

- old.
EOF
notes() {
  "$root/scripts/release-notes.sh" "$1" "$tmp/CHANGELOG.md"
}
[ "$(notes 0.2.0)" = "$(printf -- '- Breaking: one.\n\n- two.')" ] ||
  fail "0.2.0 notes: $(notes 0.2.0)"
[ "$(notes 0.2.0-rc.2)" = "$(notes 0.2.0)" ] || fail "0.2.0-rc.2 notes: $(notes 0.2.0-rc.2)"
[ "$(notes 0.2.1-rc.1)" = "- rc only." ] || fail "0.2.1-rc.1 notes: $(notes 0.2.1-rc.1)"
for v in 0.1.9 0.3.0 0.2; do
  ! notes "$v" > /dev/null 2>&1 || fail "release-notes.sh took a missing or empty $v section"
done
echo "ok        release-notes.sh picks the version's section"
