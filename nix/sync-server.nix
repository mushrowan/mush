self: {
  config,
  lib,
  pkgs,
  ...
}: let
  cfg = config.services.mush-sync;
in {
  options.services.mush-sync = {
    enable = lib.mkEnableOption "the mush session sync server";

    package = lib.mkOption {
      type = lib.types.package;
      inherit (self.packages.${pkgs.stdenv.hostPlatform.system}) default;
      defaultText = lib.literalExpression "mush.packages.\${system}.default";
      description = "the mush package whose `mush sync serve` runs the server";
    };

    port = lib.mkOption {
      type = lib.types.port;
      default = 7347;
      description = "tcp port the server listens on";
    };

    address = lib.mkOption {
      type = lib.types.str;
      default = "0.0.0.0";
      description = ''
        address to bind. the firewall only opens the port on
        `firewallInterface`, so binding everywhere keeps it on the tailnet
        without waiting for tailscale to hand out an address at boot
      '';
    };

    firewallInterface = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = "tailscale0";
      description = "interface whose firewall opens the port, null opens nothing";
    };

    tokenHash = lib.mkOption {
      type = lib.types.strMatching "[0-9a-f]{64}";
      example = "3b1f...";
      description = ''
        blake3 hash of the clients' token, printed by `mush sync key token-hash`.
        it is safe to keep in the nix store, the token and the key behind it are not
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    systemd.services.mush-sync = {
      description = "mush session sync server";
      wantedBy = ["multi-user.target"];
      wants = ["network-online.target"];
      after = ["network-online.target" "tailscaled.service"];
      environment = {
        MUSH_SYNC_TOKEN_HASH = cfg.tokenHash;
        RUST_LOG = "warn,mush_sync=info";
      };
      serviceConfig = {
        ExecStart = "${cfg.package}/bin/mush sync serve --listen ${cfg.address}:${toString cfg.port} --root /var/lib/mush-sync";
        DynamicUser = true;
        StateDirectory = "mush-sync";
        StateDirectoryMode = "0700";
        UMask = "0077";
        Restart = "on-failure";
        RestartSec = 5;
        NoNewPrivileges = true;
        CapabilityBoundingSet = "";
        PrivateDevices = true;
        PrivateTmp = true;
        ProtectSystem = "strict";
        ProtectHome = true;
        ProtectKernelTunables = true;
        ProtectKernelModules = true;
        ProtectControlGroups = true;
        RestrictAddressFamilies = ["AF_INET" "AF_INET6" "AF_UNIX"];
        RestrictNamespaces = true;
        LockPersonality = true;
        SystemCallArchitectures = "native";
      };
    };

    networking.firewall.interfaces = lib.mkIf (cfg.firewallInterface != null) {
      ${cfg.firewallInterface}.allowedTCPPorts = [cfg.port];
    };
  };
}
