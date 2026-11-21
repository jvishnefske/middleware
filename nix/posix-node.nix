# NixOS module: `mw-posix-node`, the Linux payload host (design.md MS52),
# on the Pi and on other Linux boards (the Kria KD240). It was `mw-pi-node`
# until it hosted more than Pis; a node set up under the old name keeps its
# state (see the migration below) and its node id (posix-node/src/node_id.rs).
#
#   service  mw-posix-node as the system user mw-posix-node with its own writable
#            /var/lib/mw-posix-node (StateDirectory), MW_ZENOH_ROUTER on the
#            robot Envoy's Zenoh listener and MW_PORT off the robot app's
#            and the gateway's, then robot.env on top. No supplementary
#            groups unless `supplementaryGroups` names them (gpio, i2c,
#            spi, ... when a payload needs a bus). Restarts like the
#            gateway: always, delays growing from 2 s to 1 min, no start
#            limit. MemoryDenyWriteExecute is off: a wasm payload runtime
#            may compile code at run time.
#   zenoh    the node serves its own sys keys at @mw/node/<sender id>/sys/**
#            (MW_POSIX_NODE_ZENOH_SYS, on by default; the signed rules of
#            posix-node/src/mesh_gate.rs). No option: set
#            `environment.MW_POSIX_NODE_ZENOH_SYS = "off"` to turn it off.
#            The session is a Zenoh client of MW_ZENOH_ROUTER with an id
#            derived from the node id, so the sender id survives restarts.
#   mdns     off by default (MW_POSIX_NODE_MDNS); set
#            `environment.MW_POSIX_NODE_MDNS = "on"` (or a unicast IPv4
#            address to pin) to announce the node as posix-<node id> on
#            _mw-node._tcp (posix-node/src/mdns_node.rs). It binds UDP 5353
#            beside avahi; with avahi off, open UDP 5353 in the firewall.
{ config, lib, pkgs, ... }:
let
  cfg = config.services.mw-posix-node;
  envFile = "/var/lib/mw-robot/robot.env";
  # Once, as root before the start: a state directory left by mw-pi-node
  # moves into the new one (payload, trust record, gateway counter), unless
  # the new one already holds something. The old one is kept, renamed.
  migrate = pkgs.writeShellScript "mw-posix-node-migrate" ''
    old=/var/lib/mw-pi-node new=/var/lib/mw-posix-node
    if [ -d "$old" ] && [ -z "$(ls -A "$new")" ]; then
      echo "moving the state of mw-pi-node from $old to $new"
      cp -a "$old"/. "$new"/
      chown -R mw-posix-node:mw-posix-node "$new"
      mv "$old" "$old.migrated"
    fi
  '';
in
{
  options.services.mw-posix-node = {
    enable = lib.mkEnableOption "the Linux payload host";

    package = lib.mkOption {
      type = lib.types.package;
      description = "mw-posix-node; its `meta.mainProgram` is run.";
    };

    args = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
      description = "Arguments to mw-posix-node.";
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
    users.users.mw-posix-node = { isSystemUser = true; group = "mw-posix-node"; };
    users.groups.mw-posix-node = { };

    systemd.services.mw-posix-node = {
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
        ExecStartPre = "+${migrate}";
        ExecStart = lib.escapeShellArgs ([ (lib.getExe cfg.package) ] ++ cfg.args);
        User = "mw-posix-node";
        Group = "mw-posix-node";
        SupplementaryGroups = cfg.supplementaryGroups;
        StateDirectory = "mw-posix-node";
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
