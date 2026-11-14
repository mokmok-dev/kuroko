{
  config,
  lib,
  pkgs,
  ...
}:

let
  cfg = config.services.kuroko;

  tomlFormat = pkgs.formats.toml { };

  configPath = "/etc/kuroko/config.toml";

  # The operator's table wins leaf by leaf, so overriding `store` with only a
  # `path` keeps the rest of the rendered defaults.
  settings = lib.recursiveUpdate {
    server.bind = "127.0.0.1:8787";
    store.path = "/var/lib/${cfg.stateDirectory}/kuroko.db";
  } cfg.settings;

  transports = builtins.filter (name: settings.transport ? ${name}) [
    "stdio"
    "websocket"
  ];

  workingDirectory =
    if cfg.workingDirectory != null then cfg.workingDirectory else "/var/lib/${cfg.stateDirectory}";

  secretInSettings = cfg.settings ? linear && cfg.settings.linear ? secret;
in
{
  options.services.kuroko = {
    enable = lib.mkEnableOption "the kuroko Linear webhook dispatcher";

    package = lib.mkOption {
      type = lib.types.package;
      description = "The kuroko package to run.";
    };

    settings = lib.mkOption {
      inherit (tomlFormat) type;
      default = { };
      description = ''
        The `kuroko.toml` contents. `server.bind` defaults to `127.0.0.1:8787` and
        `store.path` to `/var/lib/<stateDirectory>/kuroko.db`; every other key keeps
        the daemon's own default.
      '';
    };

    environmentFile = lib.mkOption {
      type = lib.types.nullOr lib.types.path;
      default = null;
      description = ''
        Environment file read by systemd. Put `KUROKO_LINEAR_SECRET=...` here so the
        webhook secret stays out of the Nix store.
      '';
    };

    user = lib.mkOption {
      type = lib.types.str;
      default = "kuroko";
      description = "User the daemon and a stdio agent run as.";
    };

    group = lib.mkOption {
      type = lib.types.str;
      default = "kuroko";
      description = "Group the daemon and a stdio agent run as.";
    };

    stateDirectory = lib.mkOption {
      type = lib.types.str;
      default = "kuroko";
      description = "Name under `/var/lib` for the database and WAL files.";
    };

    workingDirectory = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      description = ''
        Working directory for the daemon. Defaults to the state directory. A stdio
        agent runs in this directory, so set it to the workspace the agent should
        operate on.
      '';
    };

    logLevel = lib.mkOption {
      type = lib.types.str;
      default = "info";
      description = "Value of `RUST_LOG` for the daemon and its agent.";
    };

    stopTimeout = lib.mkOption {
      type = lib.types.str;
      default = "3900s";
      description = ''
        `TimeoutStopSec` for the unit. A graceful shutdown waits for in-flight
        agents, which run under `transport.*.timeout_secs` (3600 by default), so
        this must outlast that timeout or systemd kills a finishing agent.
      '';
    };

    extraServiceConfig = lib.mkOption {
      type = lib.types.attrs;
      default = { };
      description = ''
        Extra `systemd.service` settings. Hardening belongs here: a stdio agent
        inherits the sandbox, so `ProtectHome` or `ProtectSystem` would cut off the
        files it needs to edit.
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion = builtins.length transports == 1;
        message =
          "services.kuroko.settings must configure exactly one of "
          + "`transport.stdio` or `transport.websocket`, found ${toString (builtins.length transports)}.";
      }
      {
        assertion = cfg.environmentFile != null || secretInSettings;
        message =
          "set services.kuroko.environmentFile with KUROKO_LINEAR_SECRET, "
          + "or services.kuroko.settings.linear.secret";
      }
      {
        assertion = lib.hasPrefix "/" (toString settings.store.path);
        message =
          "services.kuroko.settings.store.path must be absolute; "
          + "a relative path resolves beside /etc/kuroko/config.toml";
      }
    ];

    warnings = lib.optional secretInSettings ''
      services.kuroko.settings.linear.secret writes the webhook secret to the
      world-readable Nix store. Set services.kuroko.environmentFile instead.
    '';

    users.groups.${cfg.group} = { };

    users.users.${cfg.user} = {
      isSystemUser = lib.mkDefault true;
      group = lib.mkDefault cfg.group;
      home = lib.mkDefault "/var/lib/${cfg.stateDirectory}";
    };

    environment.etc."kuroko/config.toml".source = tomlFormat.generate "kuroko.toml" settings;

    systemd.services.kuroko = {
      description = "kuroko Linear webhook dispatcher";
      documentation = [ "https://github.com/mokmok-dev/kuroko" ];
      wantedBy = [ "multi-user.target" ];
      wants = [ "network-online.target" ];
      after = [ "network-online.target" ];
      # The rendered config is a store path, so the unit text stays the same when
      # only a setting changes; restart on the file the daemon actually reads.
      restartTriggers = [ config.environment.etc."kuroko/config.toml".source ];

      environment.RUST_LOG = cfg.logLevel;

      serviceConfig = lib.mkMerge [
        {
          ExecStart = "${lib.getExe cfg.package} --config ${configPath}";
          User = cfg.user;
          Group = cfg.group;
          StateDirectory = cfg.stateDirectory;
          StateDirectoryMode = "0700";
          WorkingDirectory = workingDirectory;
          Restart = "on-failure";
          RestartSec = 5;
          # SIGTERM goes to the daemon only, so it can wait for running agents
          # before the rest of the group is killed at TimeoutStopSec.
          KillMode = "mixed";
          TimeoutStopSec = cfg.stopTimeout;
          UMask = "0077";
        }
        (lib.mkIf (cfg.environmentFile != null) {
          EnvironmentFile = cfg.environmentFile;
        })
        cfg.extraServiceConfig
      ];
    };
  };
}
