{
  description = "diald";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.11";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    {
      self,
      nixpkgs,
      rust-overlay,
      flake-utils,
      ...
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs {
          inherit system overlays;
        };
        rustToolchain = pkgs.rust-bin.selectLatestNightlyWith (
          toolchain:
          toolchain.default.override {
            extensions = [
              "rust-src"
              "rust-analyzer"
            ];
          }
        );
        rustPlatform = pkgs.makeRustPlatform {
          cargo = rustToolchain;
          rustc = rustToolchain;
        };
      in
      with pkgs;
      {
        packages.default = rustPlatform.buildRustPackage {
          pname = "diald";
          version = "0.1.0";
          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;
        };

        devShells.default = mkShell rec {
          buildInputs =
            [
              cacert
              cargo
              rustfmt
              rustToolchain
            ];
          shellHook = ''
            export CARGO_TARGET_DIR="$PWD/.cargo/target"
            echo "Welcome to diald"
          '';
          LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath buildInputs;
        };
      }
    )
    // {
      nixosModules.default =
        { config, lib, pkgs, ... }:
        let
          cfg = config.services.diald;
        in
        {
          options.services.diald = {
            enable = lib.mkEnableOption "Surface Dial event daemon";
            package = lib.mkOption {
              type = lib.types.package;
              default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
              description = "diald package to run.";
            };
            device = lib.mkOption {
              type = lib.types.nullOr lib.types.str;
              default = null;
              description = "Input device path (e.g. /dev/input/event2).";
            };
          };

          config = lib.mkIf cfg.enable {
            assertions = [
              {
                assertion = cfg.device != null;
                message = "services.diald.device must be set to a valid input device path.";
              }
            ];

            systemd.services.diald = {
              description = "Surface Dial event daemon";
              wantedBy = [ "multi-user.target" ];
              after = [ "systemd-udev-settle.service" ];
              serviceConfig = {
                ExecStart = "${cfg.package}/bin/diald --device ${cfg.device}";
                Restart = "on-failure";
                DynamicUser = true;
                SupplementaryGroups = [ "input" ];
              };
            };

            services.udev.extraRules = ''
              # Allow diald (input group) to access Surface Dial haptics on hidraw.
              SUBSYSTEM=="hidraw", ATTRS{idVendor}=="045e", ATTRS{idProduct}=="091b", MODE="0660", GROUP="input"
            '';
          };
        };
    };
  # based on https://github.com/hiveboardgame/hive/blob/50b3804378012ee4ecf62f6e47ca348454eb066b/flake.nix
}
