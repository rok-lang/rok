{
  description = "Rok: a statically-typed, compiled language with a rich library";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  # The release archives, wrapped as they are: rokc already links LLVM and
  # ships each target's C library, so there is nothing to build. Linux
  # takes the static musl archives (no interpreter or rpath to patch).
  # nix/sources.json is rewritten for each release (scripts/nix-sources.sh).
  outputs =
    { self, nixpkgs }:
    let
      sources = builtins.fromJSON (builtins.readFile ./nix/sources.json);
      forAll =
        f:
        nixpkgs.lib.genAttrs (builtins.attrNames sources.archives) (
          system: f system nixpkgs.legacyPackages.${system}
        );
    in
    {
      packages = forAll (
        system: pkgs: {
          rok = pkgs.stdenvNoCC.mkDerivation {
            pname = "rok";
            inherit (sources) version;
            src = pkgs.fetchurl { inherit (sources.archives.${system}) url sha256; };
            dontConfigure = true;
            dontBuild = true;
            dontStrip = true;
            dontPatchELF = true;
            installPhase = ''
              mkdir -p $out
              cp -R bin lib $out/
            '';
            meta = {
              description = "Rok toolchain: rok, rokc and rokls";
              homepage = "https://rok-lang.org";
              license = with nixpkgs.lib.licenses; [
                mit
                asl20
              ];
              mainProgram = "rok";
              platforms = [ system ];
            };
          };
          default = self.packages.${system}.rok;
        }
      );

      apps = forAll (
        system: _: {
          default = {
            type = "app";
            program = "${self.packages.${system}.rok}/bin/rok";
          };
        }
      );
    };
}
