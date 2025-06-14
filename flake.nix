{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    crane.url = "github:ipetkov/crane";
    fenix.url = "github:nix-community/fenix";
    fenix.inputs.nixpkgs.follows = "nixpkgs";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    {
      self,
      nixpkgs,
      crane,
      fenix,
      flake-utils,
      ...
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs {
          inherit system;
        };

        # https://github.com/nix-community/fenix/issues/178
        cargo = fenix.packages.${system}.complete.cargo.overrideAttrs (old: {
          postBuild = pkgs.lib.optionalString pkgs.stdenv.isDarwin ''
            cargo="./cargo/bin/cargo"
            install_name_tool \
              -change "/usr/lib/libcurl.4.dylib" "${pkgs.curl.out}/lib/libcurl.4.dylib" \
              "$cargo"
          '';
        });
        devRustToolchain =
          with fenix.packages.${system};
          combine [
            cargo
            complete.rustc
            complete.clippy
            complete.rustfmt
            rust-analyzer
          ];
        buildRustToolchain =
          with fenix.packages.${system};
          combine [
            cargo
            complete.rustc
          ];

        craneLib = (crane.mkLib pkgs).overrideToolchain buildRustToolchain;

        commonArgs =
          let
            crateFilter = path: type: craneLib.filterCargoSources path type;
          in
          {
            src = pkgs.lib.cleanSourceWith {
              src = craneLib.path ./.;
              filter = crateFilter;
            };
            strictDeps = true;
            doCheck = false;
            cargoCheckCommand = "${pkgs.coreutils}/bin/true";

            buildInputs = with pkgs; lib.optionals stdenv.isDarwin [ libiconv ];

            nativeBuildInputs =
              with pkgs;
              lib.optionals stdenv.isLinux [
                clang
                mold
                patchelf
              ];

            CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER =
              if !pkgs.stdenv.isDarwin then "${pkgs.clang}/bin/clang" else null;
            CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS =
              if !pkgs.stdenv.isDarwin then "-C link-arg=-fuse-ld=${pkgs.mold}/bin/mold" else null;
          };

        gal = craneLib.buildPackage (
          commonArgs
          // {
            cargoArtifacts = craneLib.buildDepsOnly commonArgs;
          }
        );
      in
      {
        checks = {
          inherit gal;
        };

        apps.default = flake-utils.lib.mkApp {
          drv = gal;
        };

        devShells.default = ((crane.mkLib pkgs).overrideToolchain devRustToolchain).devShell {
          checks = self.checks.${system};

          packages = with pkgs; [
            bacon
          ];
        };

        packages.default = gal;
      }
    );
}
