#!/bin/sh
# Copy every source targets/build.sh downloads to the artifacts bucket,
# keyed by SHA-256, where its fetch() looks first. Each file comes from
# its upstream URL and is checked before the upload; one already in the
# bucket is skipped. CI runs it when a pin changes (mirror.yml).
#
#   scripts/mirror-sources.sh [--check]
#
# --check downloads and verifies only. Needs curl and the aws CLI with
# write access to the bucket (ROK_MIRROR_BUCKET, default
# s3://rok-lang-artifacts/mirror).
set -eu

bucket=${ROK_MIRROR_BUCKET:-s3://rok-lang-artifacts/mirror}
check=
case "${1-}" in
  --check) check=1 ;;
  "") ;;
  *)
    echo "usage: $0 [--check]" >&2
    exit 1
    ;;
esac

llvm=https://github.com/llvm/llvm-project/releases/download/llvmorg-22.1.8
wcl=https://github.com/bytecodealliance/wasm-component-ld/releases/download/v0.5.24
deb=https://snapshot.debian.org/archive/debian/20260504T025922Z/pool/main
ports=https://snapshot.debian.org/archive/debian-ports/20230414T140955Z/pool-riscv64/main

# <sha256> <url>, as pinned in targets/build.sh (LLVM's from llvm/build.sh,
# mingw-w64's commit from compiler/rokc_driver/windows/gen.sh).
sources="
f260f4f7c0d430828a81ae8a3826a1d63fc0963ec2459489308cc23b1f7eab4f $llvm/LLVM-22.1.8-macOS-ARM64.tar.xz
d96c2cc1736f4eb7fa43cb9bbdf56d93551a9ae0a9aadb9c99c3c3b2b712a234 $llvm/clang+llvm-22.1.8-x86_64-pc-windows-msvc.tar.xz
922f1817a0df7b1489272d18134ee0087a8b068828f87ac63b9861b1a9965888 $llvm/llvm-project-22.1.8.src.tar.xz
e2e533389aa894765ff1ec618668239b7b11c5e9fb5cd51a4d9f21583db8aef2 $wcl/wasm-component-ld-v0.5.24-aarch64-linux.tar.gz
374cb68f28aa52ff53a38f8c1ef41eac611feeb9ac6c40aed4fdfb2960d1b027 $wcl/wasm-component-ld-v0.5.24-aarch64-macos.tar.gz
b6814f368ead947ed49a6ffbca695a0cbfdd3f3d43347fc4f6550ca824055a1a $wcl/wasm-component-ld-v0.5.24-aarch64-windows.zip
6e5fe349f59c21a4613dce49d043843e00a6ff37d6e5f830e2dc1c72e629bdf7 $wcl/wasm-component-ld-v0.5.24-x86_64-linux.tar.gz
62f6ae99752114fb2d0e6cc32bec90ff347b9add55ca6b55451d401edb5ae630 $wcl/wasm-component-ld-v0.5.24-x86_64-macos.tar.gz
de82098388b7c1e079ed2964e913e156ffe46ad6407a8e7d21c6e58e50d06106 $wcl/wasm-component-ld-v0.5.24-x86_64-windows.zip
ba4f88f73dbc3ae9055f3c20f4523bfdbaf1ad13ff95e258924f77d20b4fbedf $deb/g/glibc/libc6_2.36-9+deb12u14_amd64.deb
01f4330719fd4f65580e16ea5a0527f372fca750e8f588d26deaf09f2d3b1cf4 $deb/g/glibc/libc6_2.36-9+deb12u14_arm64.deb
76b12e06be66ec3fc2c1791d7d8cf34c2b828ed210e3913feeeb8edc9688f820 $deb/g/glibc/libc6_2.36-9+deb12u14_i386.deb
758c68b92654747025b48476a45c315bf16df1321224abdd9be672bfd120be45 $deb/g/glibc/libc6_2.36-9+deb12u14_armhf.deb
0218fc2befcd784c1b0c6292c0a137ce89fad054efaa579ad083bee0f2c01aae $deb/g/glibc/libc6-dev_2.36-9+deb12u14_amd64.deb
8bc395bf2ee9939f1a6ae395f974cffdcf541d98f99adfe89ce7e4bef36d159e $deb/g/glibc/libc6-dev_2.36-9+deb12u14_arm64.deb
c02c0adacabf53b8f34364940c7c4fe5aedaf921765e915742cc31ffc7a9d1ad $deb/g/glibc/libc6-dev_2.36-9+deb12u14_i386.deb
ca9486dc7f2dfb25c61c0c93d561c01f8abdf9866bd4c157f0ccd518295bbbb7 $deb/g/glibc/libc6-dev_2.36-9+deb12u14_armhf.deb
117349f98e0d9c4522bf1b8f33c6239ab9c4c5904c56f6379be3eb790a97e7dc $deb/l/linux/linux-libc-dev_6.1.159-1_amd64.deb
6da0f62a7a63524364bc0f4da96d11181cc15010051ff2f4c0dbd4a0ca5e085c $deb/l/linux/linux-libc-dev_6.1.159-1_arm64.deb
72e870c8b56344008990a7939edd44f71ae728152b5473c0fcc95a191a04d05f $deb/l/linux/linux-libc-dev_6.1.159-1_i386.deb
536d83afebd9a89e35dd104a4f3529b34833fd5fe279c4df18f2c6e9174f0e47 $deb/l/linux/linux-libc-dev_6.1.159-1_armhf.deb
fd7b16d34b67f9ca97dd048636c229fa0a0e2dbcc927ade3731885f33284910f $ports/g/glibc/libc6_2.36-9_riscv64.deb
293cde3b863a8502d461aa232ff5e28fb1821384e7380b855e9f892d05b5e2c3 $ports/g/glibc/libc6-dev_2.36-9_riscv64.deb
52234ad2798c2e58f0aebadf925d4c51d07fbf525e7721a36780c12abd4d250b $ports/l/linux/linux-libc-dev_6.1.20-2_riscv64.deb
a9a118bbe84d8764da0ea0d28b3ab3fae8477fc7e4085d90102b8596fc7c75e4 https://musl.libc.org/releases/musl-1.2.5.tar.gz
94469ea48f974f9a4f7d8b7050cb7eceaa92c1f941ed3b6b8137591ba19491de https://github.com/mingw-w64/mingw-w64/archive/63ebe6cdb3ec4d5c586b7f0f0c03dd502e44c296.tar.gz
7d11a801570972e7f32639eed3f8f7d0e997f276b3c85b7aa03283e1265e4b8e https://github.com/WebAssembly/wasi-libc/archive/574b88da481569b65a237cb80daf9a2d5aeaf82d.tar.gz
"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

echo "$sources" | while read -r sum url; do
  [ -n "$sum" ] || continue
  if [ -z "$check" ] && aws s3 ls "$bucket/$sum" > /dev/null 2>&1; then
    echo "have $sum ${url##*/}"
    continue
  fi
  curl -fsSL --retry 3 -o "$tmp/file" "$url"
  got=$( (shasum -a 256 "$tmp/file" 2> /dev/null || sha256sum "$tmp/file") | cut -d' ' -f1)
  if [ "$got" != "$sum" ]; then
    echo "checksum mismatch for $url: $got" >&2
    exit 1
  fi
  if [ -n "$check" ]; then
    echo "ok   $sum ${url##*/}"
  else
    aws s3 cp --only-show-errors "$tmp/file" "$bucket/$sum"
    echo "put  $sum ${url##*/}"
  fi
  rm -f "$tmp/file"
done
