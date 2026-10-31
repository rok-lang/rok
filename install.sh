#!/bin/sh
# Install a rok toolchain into $ROK_HOME (default ~/.rok, spec 9.2.19):
# $ROK_HOME/toolchains/<version>-<triple>/, made the default, with the
# proxies $ROK_HOME/bin/{rok,rokc,rokls}.
#
#   curl -fsSL https://rok-lang.org/install.sh | sh
#   sh install.sh --archive rok-<version>-<triple>.tar.xz   (or .tar.gz)
#   rok-<version>-<triple>/install.sh          (from an unpacked archive)
#   sh install.sh --version 0.1                (the newest 0.1.x)
#
# Environment: ROK_HOME, ROK_VERSION (default below), ROK_DIST (where
# releases are downloaded from), ROK_NO_MODIFY_PATH, NO_COLOR,
# CLICOLOR_FORCE.
#
# Like rustup, it writes $ROK_HOME/env (and env.fish), which put
# $ROK_HOME/bin first on PATH, sources it from the shell startup files
# (~/.profile, ~/.bashrc, ~/.bash_profile, ~/.zshenv, fish's conf.d) and
# installs the shell completions. --no-modify-path (or
# ROK_NO_MODIFY_PATH=1) touches none of them and says what to add.
#
# Output goes to stderr in the style of `rok toolchain update`: status
# words right-aligned, coloured as rok colours them (spec 9.1.8): on a
# terminal or with CLICOLOR_FORCE, never with NO_COLOR or TERM=dumb.
# -q prints errors only, -v adds notes (URLs, paths).
set -eu

version="${ROK_VERSION:-0.1.6}"
dist="${ROK_DIST:-https://github.com/rok-lang/rok/releases}"
home="${ROK_HOME:-$HOME/.rok}"
archive=""
quiet=""
verbose=""
case ${ROK_NO_MODIFY_PATH-} in
  "" | 0) modify_path=1 ;;
  *) modify_path="" ;;
esac

# stderr is the test, so `curl .. | sh` colours too.
if [ -n "${NO_COLOR-}" ]; then
  color=""
elif [ -n "${CLICOLOR_FORCE-}" ] && [ "$CLICOLOR_FORCE" != 0 ]; then
  color=1
elif [ "${TERM-}" = dumb ]; then
  color=""
elif [ -t 2 ]; then
  color=1
else
  color=""
fi
sky="" red="" dim_sky="" reset=""
if [ -n "$color" ]; then
  e=$(printf '\033')
  case ${COLORTERM-}:${TERM-} in
    truecolor:* | 24bit:*)
      sky="${e}[1;38;2;56;189;248m" red="${e}[1;38;2;255;95;95m" dim_sky="${e}[38;2;56;189;248m"
      ;;
    *:*256color*)
      sky="${e}[1;38;5;81m" red="${e}[1;38;5;203m" dim_sky="${e}[38;5;81m"
      ;;
    *)
      sky="${e}[1;96m" red="${e}[1;91m" dim_sky="${e}[96m"
      ;;
  esac
  reset="${e}[0m"
fi

say() { printf '%s\n' "$*"; }
# A status line: $1 right-aligned, then $2.
status() { [ -n "$quiet" ] || printf '%s%12s%s %s\n' "$sky" "$1" "$reset" "$2" >&2; }
note() { [ -z "$verbose" ] || printf '%s%12s:%s %s\n' "$dim_sky" note "$reset" "$*" >&2; }
info() { [ -n "$quiet" ] || printf '%s\n' "$*" >&2; }
err() { printf 'install.sh: %serror:%s %s\n' "$red" "$reset" "$*" >&2; }
die() {
  err "$*"
  exit 1
}
usage() {
  say "usage: install.sh [-q | --quiet] [-v | --verbose] [--no-modify-path] [--archive <rok-*.tar.xz | rok-*.tar.gz>] [--version <version | series>]"
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
    --no-modify-path)
      modify_path=""
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

# The series of version $1: 0.MINOR, or MAJOR from 1.0 on.
series_of() {
  case $1 in
    0.*) printf '%s\n' "$1" | cut -d. -f1,2 ;;
    *) printf '%s\n' "${1%%.*}" ;;
  esac
}

# The newest release of series $1: its channel file, or before that
# exists rok-version when it names a release of the series.
latest_in() {
  channel="$dist/download/channels/rok-version-$1"
  legacy="$dist/latest/download/rok-version"
  if download "$channel" "$tmp/rok-version"; then
    v=$(tr -d ' \r\n' < "$tmp/rok-version")
  elif download "$legacy" "$tmp/rok-version"; then
    v=$(tr -d ' \r\n' < "$tmp/rok-version")
    [ "$(series_of "$v")" = "$1" ] ||
      die "cannot tell the newest rok $1 release: $channel does not exist and $legacy names $v; give a full version (--version <version>)"
  else
    die "no rok release is published at $channel or $legacy; give a full version (--version <version>)"
  fi
  case $v in
    [0-9]*.*.*) printf '%s\n' "$v" ;;
    *) die "$channel does not name a version" ;;
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
    # A series (--version 0.1) is its newest release.
    case $version in
      *.*.*) ;;
      [0-9]*) version=$(latest_in "$version") || exit 1 ;;
      *) die "--version takes a version (0.1.6) or a series (0.1), not $version" ;;
    esac
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

# $1 as ~/.. when under $HOME, for display only.
shown_path() {
  case ${HOME:+$1} in
    "${HOME-}"/*) printf '~%s' "${1#"$HOME"}" ;;
    *) printf '%s' "$1" ;;
  esac
}
shown=$(shown_path "$home")
status Installed "rok $full to $shown"
note "toolchain in $home/toolchains, proxies in $home/bin"

# $1 as "$HOME/.." when under $HOME, for the shell files below.
sh_path() {
  # shellcheck disable=SC2016 # $HOME is for the shell that reads it
  case ${HOME:+$1} in
    "${HOME-}"/*) printf '$HOME%s' "${1#"$HOME"}" ;;
    *) printf '%s' "$1" ;;
  esac
}

changed=""
mark_changed() {
  changed="${changed:+$changed, }$(shown_path "$1")"
}

# Append $line to startup file $1 unless it is there already.
add_line() {
  if [ -f "$1" ] && grep -qxF "$line" "$1"; then
    return 0
  fi
  mkdir -p "$(dirname "$1")"
  if [ -s "$1" ] && [ -n "$(tail -c 1 "$1")" ]; then
    printf '\n' >> "$1"
  fi
  printf '%s\n' "$line" >> "$1"
  mark_changed "$1"
}

# Write $2 to file $1 if it differs.
put_file() {
  if [ -f "$1" ] && [ "$(cat "$1")" = "$2" ]; then
    return 0
  fi
  mkdir -p "$(dirname "$1")"
  printf '%s\n' "$2" > "$1"
  mark_changed "$1"
}

# $1's completion script for shell $2 into file $3; a toolchain too old
# for `completions` leaves it alone.
completion() {
  mkdir -p "$(dirname "$3")"
  if "$home/bin/$1" completions "$2" > "$3.new" 2> /dev/null; then
    mv "$3.new" "$3"
    note "$2 completions for $1 in $3"
  else
    rm -f "$3.new"
  fi
}

setup_shells() {
  env=$(sh_path "$home")
  cat > "$home/env" << EOF
#!/bin/sh
# rok shell setup: $env/bin first on PATH, once; zsh also
# finds the completions in $env/zsh. Written by install.sh.
case ":\${PATH}:" in
  *:"$env/bin":*) ;;
  *) export PATH="$env/bin:\$PATH" ;;
esac
if [ -n "\${ZSH_VERSION-}" ]; then
  eval 'fpath=("$env/zsh" \${fpath:#"$env/zsh"})'
fi
EOF
  cat > "$home/env.fish" << EOF
# rok shell setup for fish: $env/bin first on PATH, once.
# Written by install.sh.
if not contains -- "$env/bin" \$PATH
    set -gx PATH "$env/bin" \$PATH
end
EOF
  # Guarded: with $ROK_HOME gone the shell still starts.
  line="[ -f \"$env/env\" ] && . \"$env/env\""
  zdot=${ZDOTDIR:-$HOME}
  # Unset in containers and some CI runners.
  login=${SHELL-}
  login=${login##*/}
  for f in "$HOME/.profile" "$HOME/.bashrc" "$HOME/.bash_profile" "$zdot/.zshenv"; do
    if [ -f "$f" ]; then
      add_line "$f"
    fi
  done
  # The login shell's own file, made if missing. A new ~/.bash_profile
  # would hide ~/.profile, so bash gets ~/.profile instead.
  case $login in
    bash)
      add_line "$HOME/.bashrc"
      if [ ! -f "$HOME/.bash_profile" ] && [ ! -f "$HOME/.bash_login" ]; then
        add_line "$HOME/.profile"
      fi
      ;;
    zsh) add_line "$zdot/.zshenv" ;;
    fish) ;;
    *) add_line "$HOME/.profile" ;;
  esac
  fish_dir=${XDG_CONFIG_HOME:-$HOME/.config}/fish
  fish=""
  if [ "$login" = fish ] || [ -d "$fish_dir" ]; then
    fish=1
    put_file "$fish_dir/conf.d/rok.fish" "test -f \"$env/env.fish\"; and source \"$env/env.fish\""
  fi
  bash_dir=${XDG_DATA_HOME:-$HOME/.local/share}/bash-completion/completions
  for t in rok rokc; do
    completion "$t" bash "$bash_dir/$t"
    completion "$t" zsh "$home/zsh/_$t"
    if [ -n "$fish" ]; then
      completion "$t" fish "$fish_dir/completions/$t.fish"
    fi
  done
}

info ""
if [ -n "$modify_path" ]; then
  setup_shells
  if [ -n "$changed" ]; then
    status Modified "$changed (PATH)"
    info ""
  fi
  case ":${PATH-}:" in
    *":$home/bin:"*) ;;
    *)
      info "A new shell finds rok on PATH; to use it in this one, run:"
      info "  . \"$env/env\"            (sh, bash, zsh)"
      info "  source \"$env/env.fish\"  (fish)"
      info ""
      ;;
  esac
else
  case ":${PATH-}:" in
    *":$home/bin:"*) ;;
    *)
      info "Add $shown/bin to PATH (then open a new shell):"
      info "  bash / zsh:  echo 'export PATH=\"$home/bin:\$PATH\"' >> ~/.zshrc   (or ~/.bashrc)"
      info "  fish:        fish_add_path $home/bin"
      info ""
      ;;
  esac
fi
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
