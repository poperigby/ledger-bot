{
  description = "Ledger Bot";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    naersk = {
      url = "github:nix-community/naersk";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    { self, nixpkgs, naersk }:
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
          naersk' = pkgs.callPackage naersk { };
          ledger-bot = naersk'.buildPackage {
            pname = "ledger-bot";
            version = "0.1.0";

            src = ./.;

            nativeBuildInputs = [ pkgs.pkg-config ];
            buildInputs = [
              pkgs.openssl
              pkgs.sqlite
            ];

            meta = {
              description = "Matrix ledger bot backed by PostgreSQL";
              license = pkgs.lib.licenses.gpl3Plus;
              mainProgram = "ledger-bot";
            };
          };
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
        in
        {
          options.services.ledger-bot = {
            enable = lib.mkEnableOption "Ledger Bot";

            package = lib.mkOption {
              type = lib.types.package;
              default = self.packages.${pkgs.stdenv.hostPlatform.system}.ledger-bot;
              description = "ledger-bot package to run.";
            };

            configFile = lib.mkOption {
              type = lib.types.path;
              default = "/etc/ledger-bot/config.toml";
              description = "Path to the ledger-bot TOML config file.";
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

              serviceConfig =
                {
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
