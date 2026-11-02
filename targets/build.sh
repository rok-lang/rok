#!/bin/sh
# Builds what rokc links for a --target into
# <cargo target dir>/lib/rok/targets/<triple>/.
#
#   targets/build.sh [<triple>...]
#
# Default: every shipped target the host can build (Apple ones only on
# macOS, where the host runtime is lib/librokr.a and only its profile
# runtime is built here).
#
# Per target:
#   librokr.a (Windows: rokr.lib)  the runtime, localized by
#       runtime/rokr/localize.sh.
#   -musl   musl's crt objects and libc.a from the pinned Rust toolchain,
#       plus musl's empty libm.a, libpthread.a, ...
#   -gnu    crt objects, libc_nonshared.a, the empty libpthread.a etc. and
#       stub libc.so / libm.so made by glibc_stubs.py from glibc 2.36
#       (Debian 12; riscv64 from debian-ports; packages SHA-256 pinned).
#       Executables need glibc >= 2.34 (32-bit runtimes use time64): the
#       stubs leave out newer symbol versions.
#   wasm32  wasi-libc's crt objects and libc.a from the pinned Rust
#       toolchain (wasm32-wasip2, plus a pinned
#       wasm-component-ld; wasip1-threads; wasip1 for the browser, plus
#       rok.mjs, its JS host).
#   libclang_rt.profile.a  compiler-rt's profile runtime from the pinned
#       LLVM, built against the target's C headers (macOS SDK, Debian 12,
#       musl 1.2.5, mingw-w64 with profile_windows.c, wasi-libc). Not for
#       wasm32-unknown-unknown.
#
# Needs rustup with llvm-tools, curl, make, python3, bash, clang (CC
# overrides; on an arm64 Mac without CC the non-Apple targets use the
# pinned LLVM release's clang, downloaded into the cache, since Apple's
# has no RISC-V target; wasm32-wasip2's profile runtime needs one with the
# WebAssembly target) and the pinned LLVM's ld.lld (ROK_LLVM_PREFIX,
# else llvm/build.sh's). Windows images name the first two
# mingw32-make and python.
set -eu

make=$(command -v make || command -v mingw32-make || echo make)
python=$(command -v python3 || command -v python || echo python3)

TARGETS="x86_64-apple-darwin aarch64-apple-darwin x86_64-unknown-linux-gnu
aarch64-unknown-linux-gnu x86_64-unknown-linux-musl aarch64-unknown-linux-musl
x86_64-pc-windows-msvc aarch64-pc-windows-msvc i686-unknown-linux-gnu
i686-pc-windows-msvc armv7-unknown-linux-gnueabihf riscv64gc-unknown-linux-gnu
wasm32-wasip2 wasm32-wasip1-threads wasm32-unknown-unknown"
GLIBC=2.36-9+deb12u14
GLIBC_URL=https://snapshot.debian.org/archive/debian/20260504T025922Z/pool/main/g/glibc
LINUX=6.1.159-1
LINUX_URL=https://snapshot.debian.org/archive/debian/20260504T025922Z/pool/main/l/linux
# <package>_<arch> <sha256>
DEB_SHA256="
libc6_amd64 ba4f88f73dbc3ae9055f3c20f4523bfdbaf1ad13ff95e258924f77d20b4fbedf
libc6_arm64 01f4330719fd4f65580e16ea5a0527f372fca750e8f588d26deaf09f2d3b1cf4
libc6-dev_amd64 0218fc2befcd784c1b0c6292c0a137ce89fad054efaa579ad083bee0f2c01aae
libc6-dev_arm64 8bc395bf2ee9939f1a6ae395f974cffdcf541d98f99adfe89ce7e4bef36d159e
libc6_i386 76b12e06be66ec3fc2c1791d7d8cf34c2b828ed210e3913feeeb8edc9688f820
libc6-dev_i386 c02c0adacabf53b8f34364940c7c4fe5aedaf921765e915742cc31ffc7a9d1ad
libc6_armhf 758c68b92654747025b48476a45c315bf16df1321224abdd9be672bfd120be45
libc6-dev_armhf ca9486dc7f2dfb25c61c0c93d561c01f8abdf9866bd4c157f0ccd518295bbbb7
linux-libc-dev_amd64 117349f98e0d9c4522bf1b8f33c6239ab9c4c5904c56f6379be3eb790a97e7dc
linux-libc-dev_arm64 6da0f62a7a63524364bc0f4da96d11181cc15010051ff2f4c0dbd4a0ca5e085c
linux-libc-dev_i386 72e870c8b56344008990a7939edd44f71ae728152b5473c0fcc95a191a04d05f
linux-libc-dev_armhf 536d83afebd9a89e35dd104a4f3529b34833fd5fe279c4df18f2c6e9174f0e47
libc6_riscv64 fd7b16d34b67f9ca97dd048636c229fa0a0e2dbcc927ade3731885f33284910f
libc6-dev_riscv64 293cde3b863a8502d461aa232ff5e28fb1821384e7380b855e9f892d05b5e2c3
linux-libc-dev_riscv64 52234ad2798c2e58f0aebadf925d4c51d07fbf525e7721a36780c12abd4d250b"
# riscv64, not in Debian 12: debian-ports' glibc 2.36-9 (bookworm's base
# release) and linux 6.1.20, from one snapshot.
PORTS_GLIBC=2.36-9
PORTS_LINUX=6.1.20-2
PORTS_URL=https://snapshot.debian.org/archive/debian-ports/20230414T140955Z/pool-riscv64/main
MUSL=1.2.5
MUSL_SHA256=a9a118bbe84d8764da0ea0d28b3ab3fae8477fc7e4085d90102b8596fc7c75e4
# The wasi-libc of wasi-sdk 25, whose libc.a the pinned Rust ships for
# wasm32-wasip1 (its headers; its emulated mmap and getpid, which Rust
# does not ship).
WASI_LIBC=574b88da481569b65a237cb80daf9a2d5aeaf82d
WASI_LIBC_SHA256=7d11a801570972e7f32639eed3f8f7d0e997f276b3c85b7aa03283e1265e4b8e
# The commit of the module definitions rokc carries (gen.sh).
MINGW_SHA256=94469ea48f974f9a4f7d8b7050cb7eceaa92c1f941ed3b6b8137591ba19491de

here=$(cd "$(dirname "$0")" && pwd)
# The host's executable suffix.
exe() { case $(uname -s) in MINGW* | MSYS* | CYGWIN*) echo .exe ;; esac }
cd "$here/.."
case $(uname -s) in
  # MSYS paths: GNU tar takes `C:/..` for a remote host.
  MINGW* | MSYS* | CYGWIN*)
    cache=$(cygpath -u "${ROK_LLVM_CACHE:-$LOCALAPPDATA/rok}")
    prefix=$(cygpath -u "${ROK_LLVM_PREFIX:-$cache/llvm-22.1.8}")
    # MSYS's /usr/bin/link (coreutils) would be rustc's link.exe.
    [ -z "${VCToolsInstallDir-}" ] ||
      PATH=$(cygpath -u "${VCToolsInstallDir}bin/Host$VSCMD_ARG_HOST_ARCH/$VSCMD_ARG_TGT_ARCH"):$PATH
    ;;
  *)
    cache=${ROK_LLVM_CACHE:-${XDG_CACHE_HOME:-$HOME/.cache}/rok}
    prefix=${ROK_LLVM_PREFIX:-$cache/llvm-22.1.8}
    ;;
esac
host_cc=${CC:-clang}
lld=$prefix/bin/ld.lld
out=${CARGO_TARGET_DIR:-target}/lib/rok/targets
host=$(rustc -vV | sed -n 's/^host: //p')
sysroot=$(rustc --print sysroot)
rustup component add llvm-tools > /dev/null
ar=$sysroot/lib/rustlib/$host/bin/llvm-ar
LLVM=$(sed -n 's/^VERSION=//p' llvm/build.sh)
LLVM_SHA256=$(sed -n 's/^SHA256=//p' llvm/build.sh)
MINGW=$(sed -n 's/^COMMIT=//p' compiler/rokc_driver/windows/gen.sh)
# LLVM-$LLVM-macOS-ARM64.tar.xz and clang+llvm-$LLVM-x86_64-pc-windows-msvc.tar.xz
# of the LLVM release.
CLANG_MACOS_SHA256=f260f4f7c0d430828a81ae8a3826a1d63fc0963ec2459489308cc23b1f7eab4f
CLANG_WINDOWS_SHA256=d96c2cc1736f4eb7fa43cb9bbdf56d93551a9ae0a9aadb9c99c3c3b2b712a234

# The artifacts bucket's copy of every download, keyed by SHA-256
# (scripts/mirror-sources.sh fills it): upstream hosts go down.
mirror=${ROK_MIRROR:-https://rok-lang-artifacts.s3.us-east-1.amazonaws.com/mirror}

sha256() { (shasum -a 256 "$1" 2> /dev/null || sha256sum "$1") | cut -d' ' -f1; }

# $1 downloaded into the cache, checked against SHA-256 $3: from the
# mirror, else from $2.
fetch() {
  file=$cache/$1
  if [ ! -f "$file" ]; then
    mkdir -p "$(dirname "$file")"
    if ! curl -fsSL --retry 3 -o "$file.part" "$mirror/$3" 2> /dev/null ||
      [ "$(sha256 "$file.part")" != "$3" ]; then
      curl -fsSL --retry 3 -o "$file.part" "$2"
    fi
    mv "$file.part" "$file"
  fi
  got=$(sha256 "$file")
  if [ "$got" != "$3" ]; then
    echo "checksum mismatch for $file: $got" >&2
    exit 1
  fi
}

# macOS: Apple's clang has no RISC-V backend, so the non-Apple targets
# use the pinned LLVM release's clang, unpacked once into the cache.
other_cc=
if [ "$(uname -s)" = Darwin ] && [ -z "${CC-}" ]; then
  c=$cache/clang-$LLVM
  if [ ! -x "$c/bin/clang" ]; then
    [ "$(uname -m)" = arm64 ] || {
      echo "no pinned clang for $(uname -m) macOS: set CC to a clang with every target" >&2
      exit 1
    }
    n=LLVM-$LLVM-macOS-ARM64
    fetch "$n.tar.xz" "https://github.com/llvm/llvm-project/releases/download/llvmorg-$LLVM/$n.tar.xz" \
      "$CLANG_MACOS_SHA256"
    rm -rf "$c.tmp"
    mkdir -p "$c.tmp"
    tar -xJf "$cache/$n.tar.xz" -C "$c.tmp" "$n/bin/clang" "$n/bin/clang-${LLVM%%.*}" "$n/lib/clang"
    mv "$c.tmp/$n" "$c"
    rmdir "$c.tmp"
    rm -f "$cache/$n.tar.xz"
  fi
  other_cc=$c/bin/clang
fi
# Windows: the runner image's clang is an older LLVM release than the pinned
# one and rejects its newer riscv codegen options, so the non-native targets
# use the pinned clang (statically linked), unpacked once into the cache.
if [ -z "$other_cc" ] && [ -z "${CC-}" ]; then
  case $(uname -s) in
    MINGW* | MSYS* | CYGWIN*)
      c=$cache/clang-$LLVM
      if [ ! -x "$c/bin/clang.exe" ]; then
        n=clang+llvm-$LLVM-x86_64-pc-windows-msvc
        fetch "$n.tar.xz" "https://github.com/llvm/llvm-project/releases/download/llvmorg-$LLVM/$n.tar.xz" \
          "$CLANG_WINDOWS_SHA256"
        rm -rf "$c.tmp"
        mkdir -p "$c.tmp"
        tar -xf "$cache/$n.tar.xz" -C "$c.tmp" "$n/bin/clang.exe" "$n/lib/clang"
        mv "$c.tmp/$n" "$c"
        rmdir "$c.tmp"
        rm -f "$cache/$n.tar.xz"
      fi
      other_cc=$c/bin/clang.exe
      ;;
  esac
fi

# wasm-component-ld into $1: the release matching the component model
# (preview 3 async, fixed-size lists) of the wasmtime the tests run under
# (targets/test-wasm.sh), not the Rust toolchain's older copy.
WCL=v0.5.24
component_ld() {
  case $(uname -m) in
    arm64 | aarch64) a=aarch64 ;;
    *) a=x86_64 ;;
  esac
  case $(uname -s) in
    Darwin) o=macos x=tar.gz ;;
    Linux) o=linux x=tar.gz ;;
    *) o=windows x=zip ;;
  esac
  n=wasm-component-ld-$WCL-$a-$o
  # The release's Linux builds need glibc: a musl host builds that
  # version from crates.io (cargo checks the packages' checksums).
  case $host in *-musl)
    if [ ! -x "$cache/$n-musl/bin/wasm-component-ld" ]; then
      cargo install --quiet --locked --root "$cache/$n-musl" --version "${WCL#v}" wasm-component-ld
    fi
    cp "$cache/$n-musl/bin/wasm-component-ld" "$1/"
    return
    ;;
  esac
  case $n in
    *-aarch64-linux) s=e2e533389aa894765ff1ec618668239b7b11c5e9fb5cd51a4d9f21583db8aef2 ;;
    *-aarch64-macos) s=374cb68f28aa52ff53a38f8c1ef41eac611feeb9ac6c40aed4fdfb2960d1b027 ;;
    *-aarch64-windows) s=b6814f368ead947ed49a6ffbca695a0cbfdd3f3d43347fc4f6550ca824055a1a ;;
    *-x86_64-linux) s=6e5fe349f59c21a4613dce49d043843e00a6ff37d6e5f830e2dc1c72e629bdf7 ;;
    *-x86_64-macos) s=62f6ae99752114fb2d0e6cc32bec90ff347b9add55ca6b55451d401edb5ae630 ;;
    *-x86_64-windows) s=de82098388b7c1e079ed2964e913e156ffe46ad6407a8e7d21c6e58e50d06106 ;;
  esac
  fetch "$n.$x" "https://github.com/bytecodealliance/wasm-component-ld/releases/download/$WCL/$n.$x" "$s"
  if [ ! -d "$cache/$n" ]; then
    case $x in
      zip) (cd "$cache" && unzip -q "$n.$x") ;;
      *) tar -xzf "$cache/$n.$x" -C "$cache" ;;
    esac
  fi
  cp "$cache/$n/wasm-component-ld$(exe)" "$1/"
}

# A Debian package's data.tar.xz (the .deb is an ar archive), verified,
# unpacked under $1.
deb() {
  into=$1 pkg=$2 darch=$3
  [ -f "$into/.$pkg" ] && return
  case $darch,$pkg in
    riscv64,linux-*) version=$PORTS_LINUX url=$PORTS_URL/l/linux ;;
    riscv64,*) version=$PORTS_GLIBC url=$PORTS_URL/g/glibc ;;
    *,linux-*) version=$LINUX url=$LINUX_URL ;;
    *) version=$GLIBC url=$GLIBC_URL ;;
  esac
  name=${pkg}_${version}_$darch.deb
  fetch "glibc-$GLIBC/$name" "$url/$name" "$(echo "$DEB_SHA256" | sed -n "s/^${pkg}_$darch //p")"
  mkdir -p "$into"
  # Not the symlinks to /lib/.. (the loader, unversioned .so): unused,
  # and MSYS cannot make them.
  "$ar" p "$cache/glibc-$GLIBC/$name" data.tar.xz |
    tar -xJf - -C "$into" --exclude './lib64' --exclude './lib/ld-linux*' --exclude './usr/lib/*/*.so'
  touch "$into/.$pkg"
}

# Debian's architecture and multiarch directory of a Linux target.
debian() {
  case ${1%%-*} in
    x86_64) deb=amd64 multi=x86_64-linux-gnu ;;
    aarch64) deb=arm64 multi=aarch64-linux-gnu ;;
    i686) deb=i386 multi=i386-linux-gnu ;;
    armv7) deb=armhf multi=arm-linux-gnueabihf ;;
    riscv64gc) deb=riscv64 multi=riscv64-linux-gnu ;;
  esac
  root=$cache/glibc-$GLIBC/$deb
}

# Target $1 as clang names it (LLVM has no `riscv64gc` arch).
clang_target() {
  case $1 in
    riscv64gc-*) echo "riscv64-${1#*-}" ;;
    *) echo "$1" ;;
  esac
}

# compiler-rt's profile runtime for $1 into $2: its sources from the
# pinned LLVM's tarball, the C headers per target.
profile() {
  t=$1 dir=$2
  arch=${t%%-*}
  src=$cache/compiler-rt-$LLVM
  if [ ! -d "$src" ]; then
    fetch "llvm-project-$LLVM.src.tar.xz" \
      "https://github.com/llvm/llvm-project/releases/download/llvmorg-$LLVM/llvm-project-$LLVM.src.tar.xz" \
      "$LLVM_SHA256"
    top=llvm-project-$LLVM.src/compiler-rt
    tar -xJf "$cache/llvm-project-$LLVM.src.tar.xz" -C "$cache" "$top/lib/profile" "$top/include/profile"
    mv "$cache/$top" "$src"
    rmdir "$cache/llvm-project-$LLVM.src"
  fi
  format=gnu
  unix="-fPIC -DCOMPILER_RT_HAS_FCNTL_LCK=1 -DCOMPILER_RT_HAS_FLOCK=1 -DCOMPILER_RT_HAS_UNAME=1"
  lib=libclang_rt.profile.a
  extra=
  case $t in
    *-apple-*)
      format=darwin
      flags="--target=$arch-apple-macosx11.0 -isysroot $(xcrun --show-sdk-path) $unix"
      ;;
    *-linux-musl)
      inc=$cache/musl-$MUSL/$arch
      if [ ! -d "$inc" ]; then
        fetch "musl-$MUSL.tar.gz" "https://musl.libc.org/releases/musl-$MUSL.tar.gz" "$MUSL_SHA256"
        work=$cache/musl-$MUSL/src
        rm -rf "$work"
        mkdir -p "$work"
        tar -xzf "$cache/musl-$MUSL.tar.gz" -C "$work"
        "$make" -s -C "$work/musl-$MUSL" ARCH="$arch" prefix= DESTDIR="$inc" install-headers
        rm -rf "$work"
      fi
      flags="--target=$t -nostdlibinc -isystem $inc/include $unix"
      ;;
    *-linux-gnu | *-linux-gnueabihf)
      debian "$t"
      deb "$root" libc6-dev "$deb"
      deb "$root" linux-libc-dev "$deb"
      # Named, not --sysroot: a wrapping driver (nix's) adds
      # -nostdlibinc, which drops a sysroot's include directories.
      flags="--target=$(clang_target "$t") -nostdlibinc -isystem $root/usr/include/$multi -isystem $root/usr/include $unix"
      case $arch in
        armv7) flags="$flags -mfloat-abi=hard -mfpu=neon" ;;
      esac
      ;;
    *-windows-msvc)
      # mingw-w64's headers expect its own target; the objects are
      # COFF with the same C ABI. Its UCRT headers, version 0xa00.
      inc=$cache/mingw-w64-$MINGW
      if [ ! -d "$inc" ]; then
        fetch "mingw-w64-$MINGW.tar.gz" "https://github.com/mingw-w64/mingw-w64/archive/$MINGW.tar.gz" "$MINGW_SHA256"
        tar -xzf "$cache/mingw-w64-$MINGW.tar.gz" -C "$cache" "mingw-w64-$MINGW/mingw-w64-headers/crt" \
          "mingw-w64-$MINGW/mingw-w64-headers/include"
        rm -rf "$inc.tmp"
        mv "$cache/mingw-w64-$MINGW/mingw-w64-headers" "$inc.tmp"
        rm -rf "$cache/mingw-w64-$MINGW"
        sed -e 's/@DEFAULT_MSVCRT_VERSION@/0xE00/' -e 's/@DEFAULT_WIN32_WINNT@/0xa00/' \
          "$inc.tmp/crt/_mingw.h.in" > "$inc.tmp/crt/_mingw.h"
        mv "$inc.tmp" "$inc"
      fi
      flags="--target=$arch-w64-windows-gnu -nostdlibinc -isystem $inc/crt -isystem $inc/include -D_UCRT"
      lib=clang_rt.profile.lib format=coff
      extra=$here/profile_windows.c
      ;;
    wasm32-wasip2)
      if ! "$cc" --print-targets | grep -q wasm32; then
        echo "$t: no profile runtime ($cc has no WebAssembly target; set CC)" >&2
        return
      fi
      w=$cache/wasi-libc-$WASI_LIBC
      if [ ! -d "$w" ]; then
        fetch "wasi-libc-$WASI_LIBC.tar.gz" "https://github.com/WebAssembly/wasi-libc/archive/$WASI_LIBC.tar.gz" \
          "$WASI_LIBC_SHA256"
        rm -rf "$w.tmp"
        mkdir -p "$w.tmp"
        tar -xzf "$cache/wasi-libc-$WASI_LIBC.tar.gz" -C "$w.tmp"
        src_w=$w.tmp/wasi-libc-$WASI_LIBC
        SYSROOT_INC=$w.tmp/include TARGET_TRIPLE=wasm32-wasi CP=cp RM=rm \
          bash "$src_w/scripts/install-include-headers.sh" > /dev/null 2>&1
        mkdir -p "$w.tmp/src"
        cp "$src_w/libc-bottom-half/mman/mman.c" "$src_w/libc-bottom-half/getpid/getpid.c" \
          "$src_w/LICENSE" "$w.tmp/src/"
        rm -rf "$src_w"
        mv "$w.tmp" "$w"
      fi
      flags="--target=wasm32-wasip1 -nostdlibinc -isystem $w/include -D_WASI_EMULATED_MMAN \
			-D_WASI_EMULATED_GETPID"
      extra="$w/src/mman.c $w/src/getpid.c"
      ;;
    *)
      return
      ;;
  esac
  work=$dir/profile
  mkdir -p "$work"
  for f in "$src"/lib/profile/*.c "$src"/lib/profile/*.cpp $extra; do
    # profile_windows.c registers the runtime instead.
    case $t,${f##*/} in *-windows-*,InstrProfilingRuntime.cpp) continue ;; esac
    # shellcheck disable=SC2086
    "$cc" $flags -O2 -fno-exceptions -fno-rtti -nostdinc++ -w -DCOMPILER_RT_HAS_ATOMICS=1 \
      -I"$src/lib" -I"$src/include" -c "$f" -o "$work/${f##*/}.o"
  done
  rm -f "$dir/$lib"
  "$ar" rcs --format=$format "$dir/$lib" "$work"/*.o
  rm -rf "$work"
}

# shellcheck disable=SC2048 # split the given args, or $TARGETS when none
for t in ${*:-$TARGETS}; do
  dir=$out/$t
  cc=$host_cc
  case $t in *-apple-*) ;; *) [ -z "$other_cc" ] || cc=$other_cc ;; esac
  case $t in *-apple-*)
    # The Mach-O runtime is localized by ld64 and the profile runtime
    # built against the macOS SDK: a macOS host's job.
    if [ "$(uname -s)" != Darwin ]; then
      [ $# -eq 0 ] || {
        echo "$t: needs a macOS host" >&2
        exit 1
      }
      continue
    fi
    if [ "$t" = "$host" ]; then
      rm -rf "$dir"
      mkdir -p "$dir"
      profile "$t" "$dir"
      echo "$dir"
      continue
    fi
    ;;
  esac
  case $t in
    wasm32-unknown-unknown) rust=wasm32-wasip1 ;;
    *) rust=$t ;;
  esac
  rustup target add "$rust" > /dev/null
  # One object (fat LTO), as dist-native.sh builds the host's runtime.
  CARGO_PROFILE_RELEASE_LTO=fat cargo build --release --locked -p rokr --target "$rust"
  rm -rf "$dir"
  mkdir -p "$dir"
  case $t in *-windows-*) rt=rokr.lib ;; *) rt=librokr.a ;; esac
  built=${CARGO_TARGET_DIR:-target}/$rust/release/$rt
  case $t in
    # Localized, as the host's lib/librokr.a (dist-native.sh).
    *-linux-* | *-apple-* | *-windows-*) runtime/rokr/localize.sh "$built" "$dir/$rt" ;;
    *) cp "$built" "$dir/" ;;
  esac
  case $t in
    wasm32-*)
      case $t in
        wasm32-unknown-unknown) crt1="crt1-reactor.o" ;;
        wasm32-wasip2) crt1="crt1-command.o crt1-reactor.o" ;;
        *) crt1="crt1-command.o" ;;
      esac
      for f in $crt1 libc.a; do
        cp "$sysroot/lib/rustlib/$rust/lib/self-contained/$f" "$dir/"
      done
      case $t in
        wasm32-wasip2) component_ld "$dir" ;;
        wasm32-unknown-unknown) cp "$here/rok.mjs" "$dir/" ;;
      esac
      ;;
    *-linux-musl)
      for f in crt1.o crti.o crtn.o libc.a; do
        cp "$sysroot/lib/rustlib/$t/lib/self-contained/$f" "$dir/"
      done
      # libc.a holds them all; musl installs these empty, so
      # `-lm` / `-lpthread` link.
      for lib in m rt pthread crypt util xnet resolv dl; do
        printf '!<arch>\n' > "$dir/lib$lib.a"
      done
      ;;
    *-linux-gnu | *-linux-gnueabihf)
      debian "$t"
      deb "$root" libc6 "$deb"
      deb "$root" libc6-dev "$deb"
      # The empty archives of the libraries glibc 2.34 merged into libc.
      for f in Scrt1.o crti.o crtn.o libc_nonshared.a libpthread.a libdl.a librt.a libutil.a libanl.a; do
        cp "$root/usr/lib/$multi/$f" "$dir/"
      done
      work=$dir/stub
      mkdir -p "$work"
      for lib in c m; do
        "$python" "$here/glibc_stubs.py" "$root/lib/$multi/lib$lib.so.6" "$work/$lib.s" "$work/$lib.map"
        "$cc" --target="$(clang_target "$t")" -c "$work/$lib.s" -o "$work/$lib.o"
        "$lld" -shared -soname "lib$lib.so.6" --version-script "$work/$lib.map" -o "$dir/lib$lib.so" "$work/$lib.o"
      done
      rm -rf "$work"
      ;;
  esac
  profile "$t" "$dir"
  echo "$dir"
done
