#!/bin/sh
# Install a rok toolchain into $ROK_HOME (default ~/.rok, spec 9.2.19):
# $ROK_HOME/toolchains/<version>-<triple>/, made the default, with the
# proxies $ROK_HOME/bin/{rok,rokc,rokls}.
#
#   curl -fsSL https://rok-lang.org/install.sh | sh
#   sh install.sh --archive rok-<version>-<triple>.tar.xz   (or .tar.gz)
#   rok-<version>-<triple>/install.sh          (from an unpacked archive)
#
# Environment: ROK_HOME, ROK_VERSION (default below), ROK_DIST (where
# releases are downloaded from), NO_COLOR.
#
# Output goes to stderr in the style of `rok toolchain update`: status
# words right-aligned, green and bold on a terminal unless NO_COLOR is
# set. -q prints errors only, -v adds notes (URLs, paths).
set -eu

version="${ROK_VERSION:-0.1.3}"
dist="${ROK_DIST:-https://github.com/rok-lang/rok/releases}"
home="${ROK_HOME:-$HOME/.rok}"
archive=""
quiet=""
verbose=""

if [ -t 2 ] && [ -z "${NO_COLOR-}" ]; then
  green=$(printf '\033[1;32m') red=$(printf '\033[1;31m') reset=$(printf '\033[0m')
else
  green="" red="" reset=""
fi

say() { printf '%s\n' "$*"; }
# A status line: $1 right-aligned, then $2.
status() { [ -n "$quiet" ] || printf '%s%12s%s %s\n' "$green" "$1" "$reset" "$2" >&2; }
note() { [ -z "$verbose" ] || printf '        note: %s\n' "$*" >&2; }
info() { [ -n "$quiet" ] || printf '%s\n' "$*" >&2; }
err() { printf 'install.sh: %serror%s: %s\n' "$red" "$reset" "$*" >&2; }
die() {
  err "$*"
  exit 1
}
usage() {
  say "usage: install.sh [-q | --quiet] [-v | --verbose] [--archive <rok-*.tar.xz | rok-*.tar.gz>] [--version <version>]"
  say "installs into \$ROK_HOME (now $home)"
}

while [ $# -gt 0 ]; do
  case $1 in
    --archive)
      [ $# -ge 2 ] || {
        usage >&2
        exit 2
      }
      archive=$2
      shift 2
      ;;
    --version)
      [ $# -ge 2 ] || {
        usage >&2
        exit 2
      }
      version=$2
      shift 2
      ;;
    -q | --quiet)
      quiet=1
      shift
      ;;
    -v | --verbose)
      verbose=1
      shift
      ;;
    -h | --help)
      usage
      exit 0
      ;;
    *)
      usage >&2
      exit 2
      ;;
  esac
done

host_triple() {
  arch=$(uname -m)
  case $arch in
    arm64 | aarch64) arch=aarch64 ;;
    x86_64 | amd64) arch=x86_64 ;;
    *) die "no rok toolchain for the $arch architecture" ;;
  esac
  case $(uname -s) in
    Darwin) say "$arch-apple-darwin" ;;
    Linux)
      # A musl system has its loader, /lib/ld-musl-<arch>.so.1.
      if ls /lib/ld-musl-* > /dev/null 2>&1; then
        say "$arch-unknown-linux-musl"
      else
        say "$arch-unknown-linux-gnu"
      fi
      ;;
    MINGW* | MSYS* | CYGWIN*) die "on Windows use install.ps1 (PowerShell)" ;;
    *) die "no rok toolchain for $(uname -s)" ;;
  esac
}

# Check archive $1 against the SHA-256 in $2 (a release's .sha256).
check_sum() {
  want=$(awk '{ print tolower($1); exit }' "$2")
  if command -v sha256sum > /dev/null 2>&1; then
    got=$(sha256sum "$1" | awk '{ print $1 }')
  elif command -v shasum > /dev/null 2>&1; then
    got=$(shasum -a 256 "$1" | awk '{ print $1 }')
  else
    die "sha256sum or shasum is needed to check $1"
  fi
  [ "$got" = "$want" ] || die "$1 has SHA-256 $got, but its .sha256 says $want (a damaged or altered download)"
}

supported="aarch64-apple-darwin x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu x86_64-pc-windows-msvc x86_64-unknown-linux-musl aarch64-unknown-linux-musl"

# Download $1 to $2; fail with 1 on a 404, die on anything else.
download() {
  code=$(curl -sSL --proto '=https' -o "$2" -w '%{http_code}' "$1") ||
    die "cannot download $1 (no network, or the host is unreachable?)"
  case $code in
    2??) ;;
    404) return 1 ;;
    *) die "cannot download $1: HTTP $code" ;;
  esac
}

# The archive for $triple is missing: no such host, or no such version.
no_archive() {
  case " $supported " in
    *" $triple "*)
      die "rok $version has no release archive at $url; check the version (--version, ROK_VERSION)"
      ;;
  esac
  err "there is no rok $version release archive for $triple"
  printf '  release archives are built for: %s\n' "$supported" >&2
  printf '  on this host, build rok from source: see "Building from source" in\n' >&2
  printf '  https://github.com/rok-lang/rok#building-from-source\n' >&2
  exit 1
}

tmp=$(mktemp -d "${TMPDIR:-/tmp}/rok-install.XXXXXX")
trap 'rm -rf "$tmp"' EXIT INT TERM

# An unpacked toolchain around this script installs itself.
here=$(cd "$(dirname "$0")" 2> /dev/null && pwd) || here=""
if [ -z "$archive" ] && [ -n "$here" ] && [ -x "$here/bin/rok" ] && [ -x "$here/bin/rokc" ]; then
  src=$here
else
  if [ -z "$archive" ]; then
    triple=$(host_triple)
    base="$dist/download/v$version/rok-$version-$triple"
    status Downloading "rok $version ($triple)"
    # The .tar.xz is about half the .tar.gz; tar needs xz for it on Linux
    # (macOS's tar reads it itself). A release without one gets the .tar.gz.
    url=""
    if [ "$(uname -s)" = Darwin ] || command -v xz > /dev/null 2>&1; then
      url="$base.tar.xz"
      archive="$tmp/rok.tar.xz"
      download "$url" "$archive" || url=""
    fi
    if [ -z "$url" ]; then
      url="$base.tar.gz"
      archive="$tmp/rok.tar.gz"
      download "$url" "$archive" || no_archive
    fi
    note "from $url"
    download "$url.sha256" "$archive.sha256" || die "$url.sha256 is missing (HTTP 404), so the archive cannot be checked; report it at https://github.com/rok-lang/rok/issues"
  fi
  [ -f "$archive" ] || die "$archive does not exist"
  # A download is checked against its release's .sha256, an archive
  # file against the one beside it, if any.
  if [ -f "$archive.sha256" ]; then
    check_sum "$archive" "$archive.sha256"
    note "SHA-256 matches $archive.sha256"
  fi
  mkdir "$tmp/files"
  case $archive in
    *.tar.xz | *.txz) tar -xJf "$archive" -C "$tmp/files" || die "cannot unpack $archive (tar needs xz for a .tar.xz: install xz, or use the .tar.gz)" ;;
    *) tar -xzf "$archive" -C "$tmp/files" || die "cannot unpack $archive" ;;
  esac
  src=$(find "$tmp/files" -mindepth 2 -maxdepth 2 -name bin -type d | head -n 1)
  [ -n "$src" ] || die "$archive is not a rok toolchain (no bin/)"
  src=$(dirname "$src")
fi

full=$("$src/bin/rokc" --version | awk 'NR == 1 { print $2 }') || die "cannot run $src/bin/rokc"
# The toolchain's own rok installs it (and checks it).
ROK_HOME=$home "$src/bin/rok" -q toolchain install "$full" --from "$src"
ROK_HOME=$home "$src/bin/rok" -q toolchain default "$full"
"$home/bin/rok" --version > /dev/null || die "the installed proxy $home/bin/rok does not run"

# $home as ~/.. when under $HOME, for display only.
shown=$home
case ${HOME:+$home} in "${HOME-}"/*)
  tilde='~'
  shown=$tilde${home#"$HOME"}
  ;;
esac
status Installed "rok $full to $shown"
note "toolchain in $home/toolchains, proxies in $home/bin"

info ""
case ":${PATH-}:" in
  *":$home/bin:"*) ;;
  *)
    info "Add $shown/bin to PATH (then open a new shell):"
    info "  bash / zsh:  echo 'export PATH=\"$home/bin:\$PATH\"' >> ~/.zshrc   (or ~/.bashrc)"
    info "  fish:        fish_add_path $home/bin"
    info ""
    ;;
esac
# rokc links its own LLVM and linker, and on Linux its own C library
# files (lib/rok/targets); on macOS the link needs the SDK of Xcode's
# command line tools.
if [ "$(uname -s)" = Darwin ] && ! xcrun --show-sdk-path > /dev/null 2>&1; then
  info "rok needs the macOS SDK to link programs: xcode-select --install"
  info ""
fi
info "Next steps:"
info "  rok --version"
info "  rok new hello && cd hello && rok run"
