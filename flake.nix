{
    description = "Ledger Bot";

    inputs = {
        nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
        crane.url = "github:ipetkov/crane";
    };

    outputs =
        {
            self,
            nixpkgs,
            crane,
        }:
        let
            systems = [
                "x86_64-linux"
                "aarch64-linux"
            ];
            forAllSystems = nixpkgs.lib.genAttrs systems;
        in
        {
            packages = forAllSystems (
                system:
                let
                    pkgs = import nixpkgs { inherit system; };
                    craneLib = crane.mkLib pkgs;

                    commonArgs = {
                        src = craneLib.cleanCargoSource ./.;
                        strictDeps = true;

                        nativeBuildInputs = [ pkgs.pkg-config ];
                        buildInputs = [
                            pkgs.openssl
                            pkgs.sqlite
                        ];
                    };

                    cargoArtifacts = craneLib.buildDepsOnly commonArgs;

                    ledger-bot = craneLib.buildPackage (
                        commonArgs
                        // {
                            inherit cargoArtifacts;

                            meta = {
                                description = "Matrix ledger bot backed by PostgreSQL";
                                license = pkgs.lib.licenses.gpl3Plus;
                                mainProgram = "ledger-bot";
                            };
                        }
                    );
                in
                {
                    inherit ledger-bot;
                    default = self.packages.${system}.ledger-bot;
                }
            );

            devShells = forAllSystems (
                system:
                let
                    pkgs = import nixpkgs { inherit system; };
                in
                {
                    default = pkgs.mkShell {
                        packages = with pkgs; [
                            cargo
                            clippy
                            pkg-config
                            rustc
                            rustfmt
                            openssl
                            postgresql
                            sqlite
                        ];

                        RUST_BACKTRACE = "1";
                    };
                }
            );

            nixosModules.default =
                {
                    config,
                    lib,
                    pkgs,
                    ...
                }:
                let
                    cfg = config.services.ledger-bot;
                    settingsFormat = pkgs.formats.toml { };
                in
                {
                    options.services.ledger-bot = {
                        enable = lib.mkEnableOption "Ledger Bot";

                        package = lib.mkOption {
                            type = lib.types.package;
                            default = self.packages.${pkgs.stdenv.hostPlatform.system}.ledger-bot;
                            description = "ledger-bot package to run.";
                        };

                        settings = lib.mkOption {
                            type = lib.types.submodule {
                                freeformType = settingsFormat.type;

                                options.data_dir = lib.mkOption {
                                    type = lib.types.str;
                                    default = cfg.dataDir;
                                    defaultText = lib.literalExpression "config.services.ledger-bot.dataDir";
                                    description = "Directory for Matrix SDK store and Matrix session state.";
                                };
                            };
                            default = { };
                            example = lib.literalExpression ''
                                {
                                  matrix = {
                                    homeserver_url = "https://matrix.example";
                                    user_id = "@ledger-bot:example";
                                    device_id = "REPLACEDEVICE";
                                    allowed_inviter_mxids = [ "@admin:example" ];
                                    allowed_room_ids = [ "!room:example" ];
                                    access_token.file = "/run/secrets/ledger-bot/matrix-access-token";
                                  };
                                  postgres = {
                                    max_connections = 5;
                                    database_url.file = "/run/secrets/ledger-bot/database-url";
                                  };
                                }
                            '';
                            description = ''
                                ledger-bot configuration, rendered to TOML. See config.example.toml
                                for the available fields. Secrets must be referenced by file path or
                                passed through `environmentFile`, not inlined. Ignored when
                                `configFile` is set explicitly.
                            '';
                        };

                        configFile = lib.mkOption {
                            type = lib.types.path;
                            default = settingsFormat.generate "ledger-bot-config.toml" cfg.settings;
                            defaultText = lib.literalExpression "generated from config.services.ledger-bot.settings";
                            description = "Path to the ledger-bot TOML config file. Defaults to a file generated from `settings`.";
                        };

                        environmentFile = lib.mkOption {
                            type = lib.types.nullOr lib.types.path;
                            default = null;
                            description = "Optional systemd EnvironmentFile containing LEDGER__ secret variables.";
                        };

                        dataDir = lib.mkOption {
                            type = lib.types.str;
                            default = "/var/lib/ledger-bot";
                            description = "Directory for Matrix SDK store and Matrix session state.";
                        };

                        user = lib.mkOption {
                            type = lib.types.str;
                            default = "ledger-bot";
                            description = "User that runs ledger-bot.";
                        };

                        group = lib.mkOption {
                            type = lib.types.str;
                            default = "ledger-bot";
                            description = "Group that runs ledger-bot.";
                        };
                    };

                    config = lib.mkIf cfg.enable {
                        users.groups.${cfg.group} = { };

                        users.users.${cfg.user} = {
                            isSystemUser = true;
                            group = cfg.group;
                            home = cfg.dataDir;
                        };

                        systemd.tmpfiles.rules = [
                            "d '${cfg.dataDir}' 0750 ${cfg.user} ${cfg.group} - -"
                        ];

                        systemd.services.ledger-bot = {
                            description = "Matrix ledger bot";
                            wantedBy = [ "multi-user.target" ];
                            wants = [ "network-online.target" ];
                            after = [
                                "network-online.target"
                                "postgresql.service"
                            ];

                            serviceConfig = {
                                ExecStart = "${lib.getExe cfg.package} ${cfg.configFile}";
                                User = cfg.user;
                                Group = cfg.group;
                                WorkingDirectory = cfg.dataDir;
                                Restart = "on-failure";
                                RestartSec = "10s";
                                StateDirectory = "ledger-bot";
                                StateDirectoryMode = "0750";
                                NoNewPrivileges = true;
                                PrivateTmp = true;
                                ProtectHome = true;
                                ProtectSystem = "strict";
                                ReadWritePaths = [ cfg.dataDir ];
                            }
                            // lib.optionalAttrs (cfg.environmentFile != null) {
                                EnvironmentFile = cfg.environmentFile;
                            };
                        };
                    };
                };
        };
}
