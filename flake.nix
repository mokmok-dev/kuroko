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

                # A second stub with the OAuth settings, so the check proves the
                # rendered table and the credential loading rather than the path
                # without them.
                oauthStub = nixosSystem {
                  inherit system;
                  modules = [
                    kurokoModule
                    {
                      system.stateVersion = "24.11";
                      services.kuroko = {
                        enable = true;
                        package = pkgs.writeShellScriptBin "kuroko" "exec true";
                        settings = {
                          linear.secret = "whsec_test";
                          # Written here on purpose: the module must replace this
                          # table rather than publish it, and the secret below must
                          # not reach the rendered config.
                          linear.oauth = {
                            client_secret = "client-secret-in-settings";
                            install_secret = "install-secret-in-settings";
                          };
                          transport.stdio.command = "cat";
                        };
                        oauth = {
                          clientId = "client-id-from-linear";
                          redirectUri = "https://kuroko.example/linear/oauth/callback";
                          installSecretFile = "/run/secrets/kuroko-install-secret";
                          clientSecretFile = "/run/secrets/kuroko-client-secret";
                        };
                      };
                    }
                  ];
                };

                # Only the assertions are read, so this needs no root filesystem.
                partialMessage =
                  let
                    failures =
                      builtins.filter (entry: !entry.assertion)
                        (nixosSystem {
                          inherit system;
                          modules = [
                            kurokoModule
                            {
                              system.stateVersion = "24.11";
                              services.kuroko = {
                                enable = true;
                                package = pkgs.writeShellScriptBin "kuroko" "exec true";
                                settings = {
                                  linear.secret = "whsec_test";
                                  transport.stdio.command = "cat";
                                };
                                oauth.clientId = "client-id-from-linear";
                              };
                            }
                          ];
                        }).config.assertions;
                  in
                  builtins.concatStringsSep "\n" (map (entry: entry.message) failures);

                unit = stub.config.systemd.units."kuroko.service".text;
                generated = stub.config.environment.etc."kuroko/config.toml".source;
                oauthUnit = oauthStub.config.systemd.units."kuroko.service".text;
                oauthGenerated = oauthStub.config.environment.etc."kuroko/config.toml".source;
                unitFile = pkgs.writeText "kuroko.service" unit;
                oauthUnitFile = pkgs.writeText "kuroko-oauth.service" oauthUnit;
                partialFile = pkgs.writeText "kuroko-partial-assertion" partialMessage;
              in
              assert defaultPackage.meta.mainProgram == "kuroko";
              pkgs.runCommand "kuroko-module-check" { } ''
                # `! grep` cannot fail under `set -e`, so an absent match needs an
                # explicit exit or the assertion silently passes.
                absent() {
                  if grep -q -- "$2" "$1"; then
                    echo "unexpected '$2' in $1" >&2
                    exit 1
                  fi
                }

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

                # An unset OAuth must leave the unit and the config as they were.
                absent ${unitFile} 'LoadCredential'
                absent ${unitFile} '_SECRET_FILE'
                absent ${generated} 'linear.oauth'

                # The OAuth settings add the non-secret table and the two secrets.
                grep -q -- '^\[linear.oauth\]' ${oauthGenerated}
                grep -q -- 'client_id = "client-id-from-linear"' ${oauthGenerated}
                grep -q -- 'redirect_uri = "https://kuroko.example/linear/oauth/callback"' ${oauthGenerated}
                grep -q -- 'LoadCredential=client_secret:/run/secrets/kuroko-client-secret' ${oauthUnitFile}
                grep -q -- 'LoadCredential=install_secret:/run/secrets/kuroko-install-secret' ${oauthUnitFile}
                grep -q -- 'KUROKO_LINEAR_CLIENT_SECRET_FILE=%d/client_secret' ${oauthUnitFile}
                grep -q -- 'KUROKO_LINEAR_INSTALL_SECRET_FILE=%d/install_secret' ${oauthUnitFile}

                # Neither secret value may reach the world-readable config, and the
                # secret keys written into `settings.linear.oauth` must be dropped.
                absent ${oauthGenerated} 'client-secret-in-settings'
                absent ${oauthGenerated} 'install-secret-in-settings'
                absent ${oauthGenerated} 'client_secret'
                absent ${oauthGenerated} 'install_secret'

                # A partial oauth block fails evaluation and names what is missing.
                grep -q -- 'services.kuroko.oauth.clientSecretFile' ${partialFile}
                grep -q -- 'services.kuroko.oauth.installSecretFile' ${partialFile}
                grep -q -- 'services.kuroko.oauth.redirectUri' ${partialFile}
                absent ${partialFile} 'services.kuroko.oauth.clientId'

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
