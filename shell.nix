# Development shell for Linux hosts with nix (NixOS has no FHS paths):
# LLVM's build tools (cmake, ninja, python3, ...), clang for the PGO build of the
# pinned LLVM and for targets/build.sh, and the tools the tests run.
#
#   nix-shell            # then llvm/build.sh, cargo test, ...
{ pkgs ? import <nixpkgs> { } }:
(pkgs.mkShell.override { stdenv = pkgs.clangStdenv; }) {
  # The wrapper's hardening flags (fortify, stack protector, ...) would
  # reach C that links against the toolchain's own C libraries
  # (targets/build.sh, the REPL host).
  hardeningDisable = [ "all" ];
  # rokc links with the prefix's ld.lld (compiler/rokc/build.rs), not
  # nix's wrapped linker, which would add the run path of the C++ and
  # unwinder runtimes; nothing else finds them on NixOS. (Staticlib
  # builds for other targets ignore it.)
  RUSTFLAGS = "-C link-arg=-Wl,-rpath,${pkgs.stdenv.cc.cc.lib}/lib";
  packages = with pkgs; [
    cmake
    ninja
    curl
    xz
    gnumake
    python3
    rustup
    git
    nodejs
    zlib
    llvm
  ];
}
