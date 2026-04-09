{
  inputs = {
    naersk.url = "github:nix-community/naersk/master";
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    utils.url = "github:numtide/flake-utils";
  };

  outputs =
    {
      nixpkgs,
      utils,
      naersk,
      ...
    }:
    utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs { inherit system; };
        naersk-lib = pkgs.callPackage naersk { };
        nativeBuildInputs = with pkgs; [
          alsa-lib
          openssl
          pkg-config
        ];
      in
      {
        defaultPackage = naersk-lib.buildPackage {
          inherit nativeBuildInputs;
          src = ./.;
        };
        devShell =
          with pkgs;
          mkShell {
            inherit nativeBuildInputs;
            buildInputs = [
              cargo
              pre-commit
              rust-analyzer
              rustPackages.clippy
              rustc
              rustfmt
            ];
            RUST_SRC_PATH = rustPlatform.rustLibSrc;
          };
      }
    );
}
