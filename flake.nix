{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-parts.url = "github:hercules-ci/flake-parts";
    flake-parts.inputs.nixpkgs-lib.follows = "nixpkgs";
    git-hooks.url = "github:cachix/git-hooks.nix";
    git-hooks.inputs.nixpkgs.follows = "nixpkgs";
    treefmt-nix.url = "github:numtide/treefmt-nix";
    treefmt-nix.inputs.nixpkgs.follows = "nixpkgs";
    rust-overlay.url = "github:oxalica/rust-overlay";
    rust-overlay.inputs.nixpkgs.follows = "nixpkgs";
    crane.url = "github:ipetkov/crane";
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-parts,
      git-hooks,
      treefmt-nix,
      rust-overlay,
      crane,
      ...
    }@inputs:
    let
      kurokoModule =
        {
          lib,
          pkgs,
          ...
        }:
        {
          imports = [ ./nix/module.nix ];
          services.kuroko.package = lib.mkDefault self.packages.${pkgs.stdenv.hostPlatform.system}.kuroko;
        };
    in
    flake-parts.lib.mkFlake { inherit inputs; } {
      imports = [
        git-hooks.flakeModule
        treefmt-nix.flakeModule
      ];

      flake.nixosModules.default = kurokoModule;

      perSystem =
        {
          config,
          pkgs,
          system,
          ...
        }:
        let
          rustToolchain = pkgs.rust-bin.stable.latest.default;
          craneLib = (crane.mkLib pkgs).overrideToolchain rustToolchain;

          linkerPackages = pkgs.lib.optionals pkgs.stdenv.hostPlatform.isLinux (
            with pkgs;
            [
              clang
              mold
            ]
          );
          linkerEnv = pkgs.lib.optionalString pkgs.stdenv.hostPlatform.isLinux ''
            export RUSTFLAGS="-C link-arg=-fuse-ld=mold"
            export CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER="clang"
            export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER="clang"
          '';

          src = pkgs.lib.cleanSourceWith {
            src = ./.;
            filter =
              path: _type:
              let
                name = baseNameOf path;
              in
              !(builtins.elem name [
                ".direnv"
                "target"
              ])
              && !(pkgs.lib.hasPrefix "result" name);
          };
          cargoCommonArgs = {
            inherit src;
            strictDeps = true;
          };
          cargoArtifacts = craneLib.buildDepsOnly cargoCommonArgs;
          kuroko = craneLib.buildPackage (
            cargoCommonArgs
            // {
              inherit cargoArtifacts;
              meta.mainProgram = "kuroko";
            }
          );
        in
        {
          _module.args = {
            pkgs = import nixpkgs {
              inherit system;
              overlays = [ rust-overlay.overlays.default ];
            };
          };

          checks = {
            clippy = craneLib.cargoClippy (
              cargoCommonArgs
              // {
                inherit cargoArtifacts;
                cargoClippyExtraArgs = "--all-targets -- --deny warnings";
              }
            );
            fmt = craneLib.cargoFmt cargoCommonArgs;
            doctest = craneLib.cargoDocTest (
              cargoCommonArgs
              // {
                inherit cargoArtifacts;
              }
            );
            test = craneLib.cargoNextest (
              cargoCommonArgs
              // {
                inherit cargoArtifacts;
                cargoExtraArgs = "--locked";
                cargoNextestExtraArgs = "--no-tests=pass --no-fail-fast --test-threads $NIX_BUILD_CORES";
              }
            );
          }
          // pkgs.lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
            kuroko-module =
              let
                nixosSystem = nixpkgs.lib.nixosSystem;

                # A stub package keeps the check from pulling the crate build into
                # its closure; the default package is asserted separately.
                stub = nixosSystem {
                  inherit system;
                  modules = [
                    kurokoModule
                    {
                      system.stateVersion = "24.11";
                      services.kuroko = {
                        enable = true;
                        package = pkgs.writeShellScriptBin "kuroko" "exec true";
                        environmentFile = "/run/secrets/kuroko.env";
                        settings = {
                          transport.stdio = {
                            command = "cat";
                            args = [ "-" ];
                          };
                          trigger.label_names = [ "agent" ];
                        };
                      };
                    }
                  ];
                };

                defaultPackage =
                  (nixosSystem {
                    inherit system;
                    modules = [
                      kurokoModule
                      {
                        system.stateVersion = "24.11";
                        services.kuroko = {
                          enable = true;
                          settings = {
                            linear.secret = "whsec_test";
                            transport.websocket.url = "wss://agent.example/ws";
                          };
                        };
                      }
                    ];
                  }).config.services.kuroko.package;

                unit = stub.config.systemd.units."kuroko.service".text;
                generated = stub.config.environment.etc."kuroko/config.toml".source;
                unitFile = pkgs.writeText "kuroko.service" unit;
              in
              assert defaultPackage.meta.mainProgram == "kuroko";
              pkgs.runCommand "kuroko-module-check" { } ''
                grep -q -- '--config /etc/kuroko/config.toml' ${unitFile}
                grep -q -- 'User=kuroko' ${unitFile}
                grep -q -- 'StateDirectory=kuroko' ${unitFile}
                grep -q -- 'StateDirectoryMode=0700' ${unitFile}
                grep -q -- 'EnvironmentFile=/run/secrets/kuroko.env' ${unitFile}
                grep -q -- 'RUST_LOG=info' ${unitFile}
                grep -q -- 'KillMode=mixed' ${unitFile}
                grep -q -- 'TimeoutStopSec=3900s' ${unitFile}
                grep -q -- 'Restart=on-failure' ${unitFile}

                grep -q -- 'path = "/var/lib/kuroko/kuroko.db"' ${generated}
                grep -q -- 'bind = "127.0.0.1:8787"' ${generated}
                grep -q -- 'command = "cat"' ${generated}
                grep -q -- 'agent' ${generated}

                touch $out
              '';
          };

          packages = {
            default = kuroko;
            inherit kuroko;
          };

          apps.kuroko = {
            type = "app";
            program = "${kuroko}/bin/kuroko";
          };

          devShells.default = pkgs.mkShellNoCC {
            inputsFrom = [ config.pre-commit.devShell ];

            packages = with pkgs; [
              cargo-mutants
              cargo-nextest
              linkerPackages
              rustToolchain
              sccache
            ];

            shellHook = ''
              export RUSTC_WRAPPER="${pkgs.sccache}/bin/sccache"
              ${linkerEnv}
            '';
          };

          pre-commit.settings = {
            hooks = {
              deadnix.enable = true;
              statix.enable = true;
            };
          };

          treefmt = {
            projectRootFile = "flake.nix";
            programs = {
              nixfmt.enable = true;
            };
          };
        };

      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
      ];
    };
}
