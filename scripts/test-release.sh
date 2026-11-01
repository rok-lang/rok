#!/bin/sh
# Dry runs of release.sh --publish against a stubbed gh (and git): an
# -rc.N version is created as a prerelease, a final one is not, and an
# archive that breaks the installer contract stops the release before gh
# runs. Fake archives for every target in release-targets.json; nothing
# is built or uploaded.
#
#   scripts/test-release.sh
set -eu

cd "$(dirname "$0")/.."
root=$(pwd)
tmp=$(mktemp -d "${TMPDIR:-/tmp}/rok-test-release.XXXXXX")
trap 'rm -rf "$tmp"' EXIT INT TERM

# gh records its arguments; git names a fixed commit.
mkdir "$tmp/stub"
cat > "$tmp/stub/gh" << 'EOF'
#!/bin/sh
printf '%s\n' "$*" >> "$GH_LOG"
EOF
cat > "$tmp/stub/git" << 'EOF'
#!/bin/sh
echo 0123456789abcdef0123456789abcdef01234567
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
    m=$tmp/stage/rok-mlir-dev-$v-$t
    mkdir -p "$m/include"
    case $kind in
      win)
        (cd "$tmp/stage" && python3 -m zipfile -c "$d/$n.zip" "$n" &&
          python3 -m zipfile -c "$d/rok-mlir-dev-$v-$t.zip" "rok-mlir-dev-$v-$t")
        sum "$d" "$n.zip"
        sum "$d" "rok-mlir-dev-$v-$t.zip"
        ;;
      *)
        tar -C "$tmp/stage" -czf "$d/$n.tar.gz" "$n"
        tar -C "$tmp/stage" -cJf "$d/$n.tar.xz" "$n"
        sum "$d" "$n.tar.gz"
        sum "$d" "$n.tar.xz"
        [ "$kind" = musl ] || {
          tar -C "$tmp/stage" -czf "$d/rok-mlir-dev-$v-$t.tar.gz" "rok-mlir-dev-$v-$t"
          sum "$d" "rok-mlir-dev-$v-$t.tar.gz"
        }
        ;;
    esac
  done
}

fail() {
  echo "test-release.sh: $*" >&2
  exit 1
}

# release.sh --publish for version $1; its gh calls in $tmp/gh.log.
publish() {
  : > "$tmp/gh.log"
  GH_LOG=$tmp/gh.log PATH=$tmp/stub:$PATH "$root/scripts/release.sh" --dir "$tmp/dist-$1" --publish \
    > "$tmp/out" 2>&1
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
echo "ok        v0.2.0 is a full release"

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
