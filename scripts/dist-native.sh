#!/bin/sh
# Build a release toolchain archive:
#
#   dist/rok-<version>-<triple>.tar.gz     (Windows: .zip)
#   dist/rok-<version>-<triple>.tar.xz     the same files, about half the
#                                          size (not on Windows); install.sh
#                                          takes it where xz is at hand
#     rok-<version>-<triple>/
#       bin/{rok,rokc,rokls}                (Windows: .exe)
#       lib/librokr.a                       (Windows: rokr.lib)
#       lib/rok/{core,std,bare}/src/      the standard library sources
#       lib/rok/lldb/rok_lldb.py    lldb formatters for -g programs (rokc --print lldbinit)
#       lib/rok/targets/<triple>/   what --target links (targets/build.sh)
#       lib/libclang.{so,dylib,dll}, lib/clang/<major>/include
#                                   what `rokc bindgen` loads (llvm/build-libclang.sh;
#                                   not with a static, musl rokc)
#       install.sh install.ps1 README.md CHANGELOG.md LICENSE-MIT LICENSE-APACHE
#       COPYING.mingw-w64 (Windows module definitions embedded in rokc)
#
# plus their .sha256 and dist/rok-version; every file is a release asset.
#
#   scripts/dist-native.sh [--out <dir>]    (Windows: Git Bash in a Developer prompt)
#
# LLVM comes from ROK_LLVM_PREFIX, libclang from ROK_LIBCLANG_PREFIX,
# else llvm/build*.sh's (.ps1 on Windows), built here when missing.
set -eu

cd "$(dirname "$0")/.."
out=dist
case "${1-}" in
  --out) out=$2 ;;
  "") ;;
  *)
    echo "usage: scripts/dist-native.sh [--out <dir>]" >&2
    exit 2
    ;;
esac

# A release's --version names its commit: ROK_COMMIT_HASH and
# ROK_COMMIT_DATE (dist-linux-gnu.sh passes the host's), else git. A
# checkout without .git (`git archive`) has neither.
if [ -z "${ROK_COMMIT_HASH-}" ] || [ -z "${ROK_COMMIT_DATE-}" ]; then
  if h=$(git rev-parse --short=9 HEAD 2> /dev/null) &&
    d=$(git log -1 --format=%cs 2> /dev/null); then
    ROK_COMMIT_HASH=${ROK_COMMIT_HASH:-$h}
    ROK_COMMIT_DATE=${ROK_COMMIT_DATE:-$d}
  else
    echo "dist-native.sh: no commit for --version: run from a git checkout, or set ROK_COMMIT_HASH and" \
      "ROK_COMMIT_DATE (\`git rev-parse --short=9 HEAD\` and \`git log -1 --format=%cs\`" \
      "in the checkout this was made from)" >&2
    exit 1
  fi
fi
export ROK_COMMIT_HASH ROK_COMMIT_DATE

# The host's executable suffix, runtime archive and archive format.
case $(uname -s) in
  MINGW* | MSYS* | CYGWIN*)
    exe=.exe rt=rokr.lib ext=zip libclang=libclang.dll
    build_llvm="powershell -NoProfile -ExecutionPolicy Bypass -File llvm/build.ps1"
    build_libclang="powershell -NoProfile -ExecutionPolicy Bypass -File llvm/build-libclang.ps1"
    cache=$(cygpath -u "${ROK_LLVM_CACHE:-$LOCALAPPDATA/rok}")
    # MSYS's /usr/bin/link (coreutils) would be rustc's link.exe.
    [ -n "${VCToolsInstallDir-}" ] || {
      echo "dist-native.sh: run from a Developer prompt for the MSVC build tools" >&2
      exit 1
    }
    PATH=$(cygpath -u "${VCToolsInstallDir}bin/Host$VSCMD_ARG_HOST_ARCH/$VSCMD_ARG_TGT_ARCH"):$PATH
    # GNU tar reads `C:\..` as a remote host (CI's $RUNNER_TEMP is one).
    out=$(cygpath -u "$out")
    ;;
  *)
    exe="" rt=librokr.a ext=tar.gz build_llvm=./llvm/build.sh build_libclang=./llvm/build-libclang.sh
    cache=${ROK_LLVM_CACHE:-${XDG_CACHE_HOME:-$HOME/.cache}/rok}
    libclang=libclang.so
    [ "$(uname -s)" != Darwin ] || libclang=libclang.dylib
    ;;
esac

# A musl rokc is static: it loads nothing at run time, so it ships no
# libclang (`rokc bindgen` says it cannot load one).
static=""
case $(rustc -vV | sed -n 's/^host: //p') in *-musl) static=1 ;; esac
[ -n "${ROK_LLVM_PREFIX-}" ] || $build_llvm
if [ -n "${ROK_LIBCLANG_PREFIX-}" ]; then
  lc=$ROK_LIBCLANG_PREFIX
elif [ -z "$static" ]; then
  $build_libclang
  lc=$cache/libclang-$(sed -n 's/^VERSION=//p' llvm/build-libclang.sh)
fi
cargo build --release --locked -p rok -p rokc
# The runtime as one object (fat LTO): only its exports stay global
# (runtime/rokr/localize.sh).
CARGO_PROFILE_RELEASE_LTO=fat cargo build --release --locked -p rokr
./targets/build.sh

target=${CARGO_TARGET_DIR:-target}/release
version=$("$target/rokc$exe" --version | awk '{ print $2 }')
triple=$("$target/rokc$exe" --version --verbose | sed -n 's/^host: //p')
if "$target/rokc$exe" --version --verbose | grep -q '^llvm: none'; then
  echo "dist-native.sh: rokc was built without LLVM (see llvm/README.md)" >&2
  exit 1
fi
name=rok-$version-$triple

# install.sh / install.ps1 download this version by default.
grep -q "^version=\"\${ROK_VERSION:-$version}\"" install.sh &&
  grep -q "else { '$version' }" install.ps1 || {
  echo "dist-native.sh: install.sh's or install.ps1's default version is not $version" >&2
  exit 1
}

stage=$out/$name
rm -rf "$stage" "$out/$name".tar.* "$out/$name.$ext" "$out/$name.$ext.sha256"
mkdir -p "$stage/bin" "$stage/lib/rok"
# Without debug info and local symbols.
mb() { echo "$(($(wc -c < "$1") / 1000000)) MB"; }
for t in rok rokc rokls; do
  cp "$target/$t$exe" "$stage/bin/"
  # PE has no symbol table to strip; the linker leaves debug info in the .pdb.
  [ -n "$exe" ] || strip -x "$stage/bin/$t"
  echo "$t: $(mb "$target/$t$exe") -> $(mb "$stage/bin/$t$exe")"
done
runtime/rokr/localize.sh "$target/$rt" "$stage/lib/$rt"
# The library sources.
tar -C library -cf - core/src std/src bare/src lldb | tar -C "$stage/lib/rok" -xf -
cp -R "${CARGO_TARGET_DIR:-target}/lib/rok/targets" "$stage/lib/rok/"
# libclang (the file a Linux .so link names) and its built-in headers.
if [ -z "$static" ]; then
  cp -L "$lc/lib/$libclang" "$stage/lib/"
  cp -R "$lc/lib/clang" "$stage/lib/"
  echo "libclang: $(mb "$stage/lib/$libclang")"
fi

# Linux (glibc): what ships needs at most glibc 2.34, as the -gnu
# targets' programs do. A newer build host's glibc leaks its symbol
# versions into the binaries, and its headers (C23 __isoc23_*) into the
# runtimes, which link against their target's stub libc (glibc 2.36).
if [ "$(uname -s)" = Linux ] && [ -z "$static" ]; then
  rustup component add llvm-tools > /dev/null
  tools=$(rustc --print sysroot)/lib/rustlib/$triple/bin
  readelf() { "$tools/llvm-readobj" --elf-output-style=GNU "$@"; }
  # The lines of stdin whose last GLIBC_ version is above 2.34 (or none).
  newer() {
    awk '{ v = $NF; sub(/.*GLIBC_/, "", v); split(v, n, ".")
      if (v !~ /^[0-9.]+$/ || n[1] > 2 || (n[1] == 2 && n[2] > 34)) print }'
  }
  bad=""
  for f in "$stage"/bin/* "$stage/lib/$libclang"; do
    vs=$(readelf -V "$f" | grep -o 'GLIBC_[0-9][0-9.]*' | sort -u -t. -k2,2n -k3,3n)
    echo "${f##*/}: $(echo "$vs" | tail -n 1)"
    # Every one needs some version: none read is no check.
    v=$(echo "${vs:-?}" | newer)
    [ -z "$v" ] || bad="$bad${f#"$stage"/}: $(echo $v)\n"
  done
  # Runtime $1's undefined symbols as the stubs in $2 bind them (`@?`:
  # they have none), but the linker's and ld.so's.
  rt_check() {
    v=$({
      readelf --dyn-syms --wide "$2/libc.so" "$2/libm.so" |
        awk '$8 ~ /@@/ { split($8, s, "@@"); print "D", s[1], s[2] }'
      "$tools/llvm-nm" -P --defined-only "$2/libc_nonshared.a" | awk 'NF > 2 { print "D", $1, "GLIBC_2.0" }'
      "$tools/llvm-nm" -P "$1" | awk '$2 == "U" && $1 !~ /^(_GLOBAL_OFFSET_TABLE_|_DYNAMIC|_+tls_get_addr)$/ { print "U", $1 }'
    } | awk '$1 == "D" { v[$2] = $3; next } { print $2 "@" ($2 in v ? v[$2] : "?") }' | newer)
    [ -z "$v" ] || bad="$bad${1#"$stage"/}: $(echo $v)\n"
  }
  for d in "$stage"/lib/rok/targets/*-linux-gnu*; do
    rt_check "$d/$rt" "$d"
    [ "${d##*/}" != "$triple" ] || rt_check "$stage/lib/$rt" "$d"
  done
  [ -z "$bad" ] || {
    printf 'dist-native.sh: these need a glibc newer than 2.34:\n%b' "$bad" >&2
    echo "dist-native.sh: build the Linux archives with scripts/dist-linux-gnu.sh (in a container with an old glibc)" >&2
    exit 1
  }
fi
# The installer contract: 0.1.x `rok toolchain update` reads
# the version from `rokc <version>` and the host from `host: <triple>`.
v=$("$stage/bin/rokc$exe" --version --verbose)
printf '%s\n' "$v" | head -n 1 | grep -Eq "^rokc $version( |\$)" &&
  printf '%s\n' "$v" | grep -qx "host: $triple" || {
  printf 'dist-native.sh: rokc --version --verbose breaks the installer contract:\n%s\n' "$v" >&2
  exit 1
}
cp install.sh install.ps1 README.md CHANGELOG.md LICENSE-MIT LICENSE-APACHE \
  compiler/rokc_driver/windows/COPYING.mingw-w64 "$stage/"

# The toolchain runs from its new place, with no LLVM on PATH: its rokc
# finds its library and builds with its own LLVM and lld.
# Its programs need glibc 2.34: an older host (dist-linux-gnu.sh's) only
# links them, and dist-linux-gnu.sh runs them on 2.34.
run=1
case $(getconf GNU_LIBC_VERSION 2> /dev/null) in "glibc 2."[0-9] | "glibc 2."[12][0-9] | "glibc 2.3"[0-3]) run="" ;; esac
printf 'fn main() -> i32 {\n    println("ok");\n    0\n}\n' > "$stage/check.rok"
for o in -O0 -O2; do
  PATH=/usr/bin:/bin "$stage/bin/rokc$exe" -q build "$stage/check.rok" $o -o "$stage/check$exe"
  [ -z "$run" ] || [ "$("$stage/check$exe")" = ok ]
  # Every shipped target links (it runs only where the machine can).
  # shellcheck disable=SC2045 # the target dirs are controlled triples: no spaces or globs
  for t in $(ls "$stage/lib/rok/targets"); do
    PATH=/usr/bin:/bin "$stage/bin/rokc$exe" -q build "$stage/check.rok" $o --target "$t" -o "$stage/check$exe"
  done
done
# bindgen loads the toolchain's libclang, which finds its own headers.
if [ -z "$static" ]; then
  printf '#include <stddef.h>\nsize_t check_len(const char *s);\n' > "$stage/check.h"
  PATH=/usr/bin:/bin "$stage/bin/rokc$exe" bindgen "$stage/check.h" |
    grep -q 'pub fn check_len(s: CPtr<c_char>) -> usize;'
fi
rm -rf "$stage/check.h" "$stage/check.rok" "$stage/check$exe" "$stage/.check$exe.fp" "$stage/check.pdb" \
  "$stage/check$exe.dSYM" "$stage/lib/rok-cache"

sum=$(command -v sha256sum || echo "shasum -a 256")
if [ "$ext" != zip ] && ! command -v xz > /dev/null 2>&1; then
  echo "dist-native.sh: xz is needed for the .tar.xz archive (Debian / Ubuntu: apt install xz-utils; macOS: brew install xz)" >&2
  exit 1
fi
exts=$ext
case $ext in
  # cmake (needed for LLVM anyway) writes zip; Git Bash's tar does not.
  zip) (cd "$out" && cmake -E tar cf "$name.zip" --format=zip "$name") ;;
  *)
    tar -C "$out" -czf "$out/$name.$ext" "$name"
    # rok toolchain install reads the .tar.gz; install.sh prefers this.
    tar -C "$out" -cf - "$name" | xz -9 -T0 > "$out/$name.tar.xz"
    exts="$ext tar.xz"
    ;;
esac
for e in $exts; do
  (cd "$out" && $sum "$name.$e" > "$name.$e.sha256")
  echo "archive: $(mb "$out/$name.$e")"
  echo "$out/$name.$e"
done
rm -rf "${out:?}/$name"
# Listing a .zip takes unzip; release.sh checks every archive again.
if [ "$ext" != zip ]; then
  scripts/check-archive.sh "$version" "$out/$name.tar.gz" "$out/$name.tar.xz"
elif command -v unzip > /dev/null 2>&1; then
  scripts/check-archive.sh "$version" "$out/$name.zip"
fi
# The release's version: `rok toolchain update` reads the newest release's
# from releases/latest/download/rok-version.
printf '%s\n' "$version" > "$out/rok-version"
echo "$out/rok-version"
