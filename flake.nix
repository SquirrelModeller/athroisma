{
  description = "System resource aggregator";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs?ref=nixos-unstable";

  outputs = {nixpkgs, ...}: let
    systems = ["x86_64-linux" "aarch64-linux"];
    forEachSystem = nixpkgs.lib.genAttrs systems;
    pkgsFor = system: import nixpkgs {inherit system;};
  in {
    packages = forEachSystem (system: let
      pkgs = pkgsFor system;
    in {
      default = pkgs.rustPlatform.buildRustPackage {
        pname = "athroisma";
        version = "0.1.0";
        src = ./.;
        cargoLock.lockFile = ./Cargo.lock;
        nativeBuildInputs = [pkgs.makeWrapper];
        postInstall = ''
          wrapProgram $out/bin/athroisma \
            --prefix LD_LIBRARY_PATH : /run/opengl-driver/lib
        '';
      };
    });

    devShells = forEachSystem (system: let
      pkgs = pkgsFor system;
    in {
      default = pkgs.mkShell {
        packages = with pkgs; [cargo rustc rust-analyzer rustfmt clippy];
      };
    });
  };
}
