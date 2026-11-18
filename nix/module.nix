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
  baseSettings = lib.recursiveUpdate {
    server.bind = "127.0.0.1:8787";
    store.path = "/var/lib/${cfg.stateDirectory}/kuroko.db";
  } cfg.settings;

  # Only the non-secret keys reach the rendered table. `//` replaces the whole
  # `oauth` value, so a `client_secret` or `install_secret` written into
  # `settings.linear.oauth` is dropped rather than published in the store.
  settings =
    baseSettings
    // lib.optionalAttrs oauthConfigured {
      linear = (baseSettings.linear or { }) // {
        oauth = {
          client_id = cfg.oauth.clientId;
          redirect_uri = cfg.oauth.redirectUri;
        };
      };
    };

  transports = builtins.filter (name: settings.transport ? ${name}) [
    "stdio"
    "websocket"
  ];

  workingDirectory =
    if cfg.workingDirectory != null then cfg.workingDirectory else "/var/lib/${cfg.stateDirectory}";

  secretInSettings = cfg.settings ? linear && cfg.settings.linear ? secret;

  # Spelled out rather than derived, so the assertion message names the option
  # the operator has to set.
  oauthKeys = {
    inherit (cfg.oauth)
      clientId
      redirectUri
      installSecretFile
      clientSecretFile
      ;
  };

  missingOauthKeys = builtins.attrNames (lib.filterAttrs (_name: value: value == null) oauthKeys);

  oauthConfigured = missingOauthKeys == [ ];

  # An operator who sets none of the four keeps the module exactly as it was, so
  # "all unset" must satisfy the same assertion that "all set" does.
  oauthUnset = lib.filterAttrs (_name: value: value != null) oauthKeys == { };

  oauthConsistent = oauthConfigured || oauthUnset;

  oauthTableInSettings = cfg.settings ? linear && cfg.settings.linear ? oauth;

  credentials = lib.optionalAttrs oauthConfigured {
    LoadCredential = [
      "client_secret:${toString cfg.oauth.clientSecretFile}"
      "install_secret:${toString cfg.oauth.installSecretFile}"
    ];
  };

  credentialEnvironment = lib.optionalAttrs oauthConfigured {
    KUROKO_LINEAR_CLIENT_SECRET_FILE = "%d/client_secret";
    KUROKO_LINEAR_INSTALL_SECRET_FILE = "%d/install_secret";
  };
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

    oauth = {
      clientId = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        example = "0a1b2c3d4e5f";
        description = ''
          The OAuth application's client ID, from Linear's application settings.
          The four `oauth` options are set together or not at all; leaving them
          unset keeps the webhook path working and answers both install routes
          with `404`.
        '';
      };

      redirectUri = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        example = "https://kuroko.example/linear/oauth/callback";
        description = ''
          The callback URL registered on the application. It must reach this
          daemon's `/linear/oauth/callback` route.
        '';
      };

      installSecretFile = lib.mkOption {
        type = lib.types.nullOr lib.types.path;
        default = null;
        description = ''
          File holding the secret that guards `GET /linear/install`. The module
          passes it through systemd's `LoadCredential` and sets
          `KUROKO_LINEAR_INSTALL_SECRET_FILE` to the credential path, so the value
          never reaches the Nix store or the rendered config. The file is read at
          every start, so restart the unit after you change it.
        '';
      };

      clientSecretFile = lib.mkOption {
        type = lib.types.nullOr lib.types.path;
        default = null;
        description = ''
          File holding the OAuth application's client secret. The module passes it
          through systemd's `LoadCredential` and sets
          `KUROKO_LINEAR_CLIENT_SECRET_FILE` to the credential path, which this
          takes over from a `KUROKO_LINEAR_CLIENT_SECRET` value in
          `environmentFile`. The file is read at every start, so restart the unit
          after you change it.
        '';
      };
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
      {
        assertion = oauthConsistent;
        message =
          "services.kuroko.oauth is set partly; missing "
          + lib.concatMapStringsSep ", " (key: "services.kuroko.oauth.${key}") missingOauthKeys
          + ". Set all four oauth options, or none of them.";
      }
    ];

    warnings =
      lib.optional secretInSettings ''
        services.kuroko.settings.linear.secret writes the webhook secret to the
        world-readable Nix store. Set services.kuroko.environmentFile instead.
      ''
      ++ lib.optional (oauthConfigured && oauthTableInSettings) ''
        services.kuroko.settings.linear.oauth is ignored because
        services.kuroko.oauth renders that table. A secret written there would
        reach the world-readable Nix store.
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

      environment = credentialEnvironment // {
        RUST_LOG = cfg.logLevel;
      };

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
        credentials
        cfg.extraServiceConfig
      ];
    };
  };
}
