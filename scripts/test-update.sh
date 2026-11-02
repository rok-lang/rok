#!/bin/sh
# Update a 0.1.x install to a staged release set: the
# archives dist-native.sh / dist-linux-gnu.sh left in <dist>, served
# over http in the GitHub releases' layout. Three updates, each on a
# fresh ROK_HOME holding the 0.1.x release:
#
#   - its `rok toolchain update`, then the staged rok builds and runs a
#     project the 0.1.x rok made and locked (a git dependency), and
#     leaves its rok.lock in lock format 2;
#   - its install.sh --archive with the staged archive;
#   - the staged archive's own install.sh.
#
#   scripts/test-update.sh <dist>
#
# ROK_UPDATE_FROM names the 0.1.x release (default 0.1.6), downloaded
# from GitHub; ROK_UPDATE_PORT the local port (default 8765). Unix only;
# Windows runs scripts/test-update.ps1.
set -eu

[ $# -eq 1 ] || {
  echo "usage: scripts/test-update.sh <dist>" >&2
  exit 2
}
dist=$(cd "$1" && pwd)
v=$(cat "$dist/rok-version")
from=${ROK_UPDATE_FROM:-0.1.6}
port=${ROK_UPDATE_PORT:-8765}
gh=https://github.com/rok-lang/rok

fail() {
  echo "test-update.sh: $*" >&2
  exit 1
}
step() { printf '\n== %s\n' "$*"; }

name=""
for a in "$dist/rok-$v"-*.tar.gz; do
  [ -f "$a" ] && name=${a##*/} && name=${name%.tar.gz}
done
[ -n "$name" ] || fail "no rok-$v-<triple>.tar.gz in $dist"
triple=${name#"rok-$v-"}
# Before the version bump the staged set is the newest release itself,
# which `rok toolchain update` takes as up to date.
if [ "$v" = "$from" ]; then
  from=0.1.5
  echo "test-update.sh: the staged set is $v, the release to update from; updating from $from"
fi
case $v in
  0.*) series=$(echo "$v" | cut -d. -f1,2) ;;
  *) series=${v%%.*} ;;
esac

tmp=$(mktemp -d "${TMPDIR:-/tmp}/rok-test-update.XXXXXX")
server=""
cleanup() {
  [ -z "$server" ] || kill "$server" 2> /dev/null || true
  rm -rf "$tmp"
}
trap cleanup EXIT INT TERM
export HOME="$tmp/user" ROK_NO_MODIFY_PATH=1 NO_COLOR=1
mkdir -p "$HOME"
unset ROK_TOOLCHAIN ROK_HOME

step "rok $from ($triple) from $gh"
base=$tmp/base
mkdir -p "$base"
curl -fsSL -o "$base/install.sh" "https://raw.githubusercontent.com/rok-lang/rok/v$from/install.sh"
curl -fsSL -o "$base/rok-$from-$triple.tar.gz" "$gh/releases/download/v$from/rok-$from-$triple.tar.gz"
curl -fsSL -o "$base/rok-$from-$triple.tar.gz.sha256" "$gh/releases/download/v$from/rok-$from-$triple.tar.gz.sha256"

# A ROK_HOME $1 with rok $from as the default, installed by its install.sh.
install_from() {
  ROK_HOME=$1 sh "$base/install.sh" -q --archive "$base/rok-$from-$triple.tar.gz"
  check_default "$1" "$from"
}

# The proxies in $1 run rok $2 by default, and the toolchain list has it.
check_default() {
  got=$(cd "$tmp" && ROK_HOME=$1 "$1/bin/rokc" --version | head -n 1)
  case $got in
    "rokc $2" | "rokc $2 "*) ;;
    *) fail "$1/bin/rokc --version says '$got', not rokc $2" ;;
  esac
  ROK_HOME=$1 "$1/bin/rok" toolchain list | grep -q "$2-$triple (default)" ||
    fail "$1: $2-$triple is not the default: $(ROK_HOME=$1 "$1/bin/rok" toolchain list)"
}

# The staged set, as GitHub serves a release.
step "staged rok $v on http://127.0.0.1:$port"
serve=$tmp/serve
mkdir -p "$serve/download/v$v" "$serve/latest/download"
cp "$dist/$name".* "$serve/download/v$v/"
cp "$dist/rok-version" "$serve/latest/download/"
python3 -m http.server "$port" --bind 127.0.0.1 --directory "$serve" > "$tmp/server.log" 2>&1 &
server=$!
i=0
until curl -fsS -o /dev/null "http://127.0.0.1:$port/latest/download/rok-version" 2> /dev/null; do
  i=$((i + 1))
  [ "$i" -lt 50 ] || fail "the local server does not answer: $(cat "$tmp/server.log")"
  sleep 0.2
done

step "rok $from: rok toolchain update"
h=$tmp/update
install_from "$h"
# A project rok $from made and locked: a git dependency.
dep=$tmp/greet
mkdir -p "$dep/src"
printf 'pub fn greet() -> i32 {\n    return 1;\n}\n' > "$dep/src/lib.rok"
git -C "$dep" init -q
git -C "$dep" add .
git -C "$dep" -c user.name=rok -c user.email=rok@localhost commit -qm greet
(cd "$tmp" && ROK_HOME=$h "$h/bin/rok" -q new app)
app=$tmp/app
printf 'use greet::greet;\nprintln(f"{greet()}");\n' > "$app/src/main.rok"
(cd "$app" && ROK_HOME=$h "$h/bin/rok" -q add greet --git "$dep")
(cd "$app" && ROK_HOME=$h "$h/bin/rok" -q build)
[ -f "$app/rok.lock" ] || fail "rok $from wrote no rok.lock"
echo "rok.lock after rok $from: version $(sed -n "s/^ *version: *\([0-9]*\).*/\1/p" "$app/rok.lock" | head -n 1)"

(cd "$tmp" && ROK_HOME=$h ROK_DIST=http://127.0.0.1:$port "$h/bin/rok" toolchain update)
check_default "$h" "$v"
ROK_HOME=$h "$h/bin/rok" toolchain list | grep -q "$from-$triple" ||
  fail "rok $from is no longer installed"

# The project moves to the staged series and builds as it was locked.
sed "s/toolchain: \"[^\"]*\"/toolchain: \"$series\"/" "$app/rok.rokon" > "$app/rok.rokon.new"
mv "$app/rok.rokon.new" "$app/rok.rokon"
(cd "$app" && ROK_HOME=$h "$h/bin/rok" build)
out=$(cd "$app" && ROK_HOME=$h "$h/bin/rok" -q run)
[ "$out" = 1 ] || fail "the project prints '$out', not 1"
[ -f "$app/rok.lock" ] || fail "rok $v removed rok.lock"
lockv=$(sed -n "s/^ *version: *\([0-9]*\).*/\1/p" "$app/rok.lock" | head -n 1)
echo "rok.lock after rok $v: version $lockv"
[ "$lockv" = 2 ] || fail "rok $v left rok.lock at version $lockv, not 2"
! grep -q 'vers:' "$app/rok.lock" || fail "rok $v left a 0.1 field (vers) in rok.lock"

step "rok $from: its install.sh --archive $name.tar.xz"
h=$tmp/install-old
install_from "$h"
ROK_HOME=$h sh "$base/install.sh" -q --archive "$dist/$name.tar.xz"
check_default "$h" "$v"

step "rok $from: $name/install.sh"
h=$tmp/install-new
install_from "$h"
mkdir "$tmp/unpacked"
tar -xzf "$dist/$name.tar.gz" -C "$tmp/unpacked"
ROK_HOME=$h sh "$tmp/unpacked/$name/install.sh" -q
check_default "$h" "$v"

printf '\ntest-update.sh: rok %s updates to %s (%s)\n' "$from" "$v" "$triple"
