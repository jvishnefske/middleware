# NixOS module: `mw-pi-node`, the Linux payload host on the Pi (design.md
# MS52). The crate (pi-node/) is written separately; flake.nix enables this
# module only once nix/binaries.nix's mw-pi-node entry builds (its
# directory exists), so until then the Pi image runs without it.
#
#   service  mw-pi-node as the system user mw-pi-node with its own writable
#            /var/lib/mw-pi-node (StateDirectory), MW_ZENOH_ROUTER on the
#            robot Envoy's Zenoh listener and MW_PORT off the robot app's
#            and the gateway's, then robot.env on top. No supplementary
#            groups unless `supplementaryGroups` names them (gpio, i2c,
#            spi, ... when a payload needs a bus). Restarts like the
#            gateway: always, delays growing from 2 s to 1 min, no start
#            limit. MemoryDenyWriteExecute is off: a wasm payload runtime
#            may compile code at run time.
{ config, lib, ... }:
let
  cfg = config.services.mw-pi-node;
  envFile = "/var/lib/mw-robot/robot.env";
in
{
  options.services.mw-pi-node = {
    enable = lib.mkEnableOption "the Linux payload host";

    package = lib.mkOption {
      type = lib.types.package;
      description = "mw-pi-node; its `meta.mainProgram` is run.";
    };

    args = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
      description = "Arguments to mw-pi-node.";
    };

    environment = lib.mkOption {
      type = lib.types.attrsOf lib.types.str;
      default = { };
      description = "Extra variables, set before robot.env, which overrides them.";
    };

    supplementaryGroups = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
      description = "Groups for hardware access (none by default).";
    };

    port = lib.mkOption {
      type = lib.types.port;
      default = 50053;
      description = "MW_PORT of its runner.";
    };

    zenohRouter = lib.mkOption {
      type = lib.types.str;
      default = "tcp/127.0.0.1:17447";
      description = "MW_ZENOH_ROUTER: the robot Envoy's Zenoh listener.";
    };

    memoryMax = lib.mkOption {
      type = lib.types.str;
      default = "128M";
      description = "The unit's MemoryMax.";
    };
  };

  config = lib.mkIf cfg.enable {
    users.users.mw-pi-node = { isSystemUser = true; group = "mw-pi-node"; };
    users.groups.mw-pi-node = { };

    systemd.services.mw-pi-node = {
      description = "Linux payload host";
      wantedBy = [ "multi-user.target" ];
      after = [ "mw-envoy.service" "network.target" ];
      environment = {
        MW_ZENOH_ROUTER = cfg.zenohRouter;
        MW_PORT = toString cfg.port;
      } // cfg.environment;
      unitConfig.StartLimitIntervalSec = 0;
      serviceConfig = {
        EnvironmentFile = "-${envFile}";
        ExecStart = lib.escapeShellArgs ([ (lib.getExe cfg.package) ] ++ cfg.args);
        User = "mw-pi-node";
        Group = "mw-pi-node";
        SupplementaryGroups = cfg.supplementaryGroups;
        StateDirectory = "mw-pi-node";
        StateDirectoryMode = "0700";
        Restart = "always";
        RestartSec = "2s";
        RestartSteps = 6;
        RestartMaxDelaySec = "60s";
        MemoryMax = cfg.memoryMax;
        CapabilityBoundingSet = "";
        AmbientCapabilities = "";
        NoNewPrivileges = true;
        ProtectSystem = "strict";
        ProtectHome = true;
        PrivateTmp = true;
        PrivateDevices = cfg.supplementaryGroups == [ ];
        ProtectKernelTunables = true;
        ProtectKernelModules = true;
        ProtectKernelLogs = true;
        ProtectControlGroups = true;
        ProtectClock = true;
        ProtectHostname = true;
        ProtectProc = "invisible";
        RestrictAddressFamilies = [ "AF_INET" "AF_INET6" "AF_UNIX" ];
        RestrictNamespaces = true;
        RestrictRealtime = true;
        RestrictSUIDSGID = true;
        LockPersonality = true;
        SystemCallArchitectures = "native";
        SystemCallFilter = [ "@system-service" "~@privileged" ];
        UMask = "0077";
      };
    };
  };
}
