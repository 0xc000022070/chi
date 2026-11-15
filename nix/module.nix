self: {
  config,
  lib,
  pkgs,
  ...
}: let
  cfg = config.services.chi;
  inherit (lib) mkOption types;
in {
  options.services.chi = {
    enable = lib.mkEnableOption "chi, the self-hosted issue, error, build and agent suite";
    package = mkOption {
      type = types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      defaultText = lib.literalExpression "chi.packages.\${system}.default";
    };
    url = mkOption {
      type = types.str;
      example = "https://chi.example.com";
      description = "Public url; links in mail, hooks, MCP answers and OAuth use it.";
    };
    listen = mkOption {
      type = types.str;
      default = "127.0.0.1:8080";
    };
    agentSlots = mkOption {
      type = types.ints.positive;
      default = 2;
      description = "Agent CLI runs at once.";
    };
    hooks = mkOption {
      type = types.attrsOf types.path;
      default = {};
      description = "Executables that get every event as JSON on stdin, by name.";
    };
    environmentFile = mkOption {
      type = types.nullOr types.path;
      default = null;
      example = "/run/secrets/chi.env";
      description = ''
        Secrets as environment, such as NIX_CONFIG="access-tokens = github.com=..." so private
        pipeline flakes can be fetched.
      '';
    };
    githubSecretFile = mkOption {
      type = types.nullOr types.path;
      default = null;
      description = "GitHub webhook secret; without it POST /api/github answers 404.";
    };
    credentials = mkOption {
      type = types.attrsOf types.path;
      default = {};
      example = {"acme.app.gdrive-account" = "/run/secrets/gdrive.tar";};
      description = "Pipeline secrets, named <org>.<repo>.<secret>, handed over with systemd LoadCredential.";
    };
    agents = mkOption {
      type = types.listOf types.package;
      default = [pkgs.claude-code pkgs.codex];
      defaultText = lib.literalExpression "[pkgs.claude-code pkgs.codex]";
      description = "Agent CLIs, logged in once with `sudo chi-agent-login claude` or `codex login`; claude-code is unfree.";
    };
    extraPackages = mkOption {
      type = types.listOf types.package;
      default = [];
      description = "More programs on the service's PATH.";
    };
  };

  config = lib.mkIf cfg.enable {
    nix.settings.allowed-users = ["chi"];

    environment.systemPackages = [
      (pkgs.writeShellApplication {
        name = "chi-agent-login";
        text = ''
          exec ${config.systemd.package}/bin/systemd-run --pty --wait --collect --quiet \
            -p DynamicUser=yes -p User=chi -p StateDirectory=chi \
            -E HOME=/var/lib/chi/home -E PATH=${lib.makeBinPath (cfg.agents ++ [pkgs.bashInteractive pkgs.coreutils])} \
            -- "''${@:-bash}"
        '';
      })
    ];

    systemd.services.chi = {
      description = "chi";
      wantedBy = ["multi-user.target"];
      after = ["network-online.target"];
      wants = ["network-online.target"];
      path = [pkgs.bubblewrap config.nix.package pkgs.git] ++ cfg.agents ++ cfg.extraPackages;
      environment = {
        CHI_URL = cfg.url;
        CHI_LISTEN = cfg.listen;
        CHI_DATA = "/var/lib/chi";
        CHI_AGENT_SLOTS = toString cfg.agentSlots;
        HOME = "/var/lib/chi/home";
        CHI_HOOKS = lib.mkIf (cfg.hooks != {}) (toString (pkgs.linkFarm "chi-hooks" cfg.hooks));
      };
      serviceConfig = {
        ExecStartPre = "${pkgs.coreutils}/bin/mkdir -p /var/lib/chi/home";
        ExecStart = "${lib.getExe cfg.package} serve";
        EnvironmentFile = lib.mkIf (cfg.environmentFile != null) cfg.environmentFile;
        LoadCredential =
          lib.mapAttrsToList (name: path: "${name}:${path}") cfg.credentials
          ++ lib.optional (cfg.githubSecretFile != null) "github-secret:${cfg.githubSecretFile}";
        DynamicUser = true;
        StateDirectory = "chi";
        StateDirectoryMode = "0700";
        Restart = "on-failure";
        NoNewPrivileges = true;
        PrivateTmp = true;
        ProtectSystem = "strict";
        ProtectHome = true;
        ProtectKernelModules = true;
        ProtectKernelLogs = true;
        ProtectControlGroups = true;
        ProtectClock = true;
        LockPersonality = true;
        RestrictRealtime = true;
        RestrictAddressFamilies = ["AF_UNIX" "AF_INET" "AF_INET6" "AF_NETLINK"];
        SystemCallArchitectures = "native";
      };
    };
  };
}
