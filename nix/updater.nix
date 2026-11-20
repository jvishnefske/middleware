# NixOS module: `mw-updater`, the updates channel's client (design.md
# UP6/UP7). It pulls root-signed releases from `wss://<host>/zenoh` and
# installs them into the serial devices it is given: MCU boards on USB, and
# (with `posixNodeLink`) the local mw-posix-node through a PTY pair.
#
#   service   mw-updater as the system user mw-updater (groups dialout and,
#             with the link, mw-node-link), StateDirectory mw-updater, the
#             Zenoh credentials through LoadCredential (never in the store).
#             Restarts always, delays growing from 2 s to 1 min.
#   link      with posixNodeLink: mw-node-link runs socat with two PTYs,
#             /run/mw-node-link/node (mw-posix-node's
#             MW_POSIX_NODE_LOCAL_SERIAL) and /run/mw-node-link/updater (one
#             of the updater's devices), group mw-node-link, mode 0660.
#
# The root key is public (it only verifies), so it is an option; the
# credentials file holds one line `<user>:<password>` (the router's
# dictionary entry for this host).
{ config, lib, pkgs, ... }:
let
  cfg = config.services.mw-updater;
  linkDir = "/run/mw-node-link";
  devices = cfg.devices ++ lib.optional cfg.posixNodeLink "${linkDir}/updater";
in
{
  options.services.mw-updater = {
    enable = lib.mkEnableOption "the updates channel client";

    package = lib.mkOption {
      type = lib.types.package;
      description = "mw-updates; its `bin/mw-updater` is run.";
    };

    host = lib.mkOption {
      type = lib.types.str;
      example = "updates.example.com";
      description = "MW_UPDATES_HOST: the channel is wss://<host>/zenoh.";
    };

    addr = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      example = "192.168.1.185:443";
      description = "MW_UPDATES_ADDR: dial this instead of resolving the host (a LAN without hairpin NAT). The TLS name stays the host.";
    };

    channel = lib.mkOption {
      type = lib.types.str;
      default = "stable";
      description = "MW_UPDATES_CHANNEL.";
    };

    rootKey = lib.mkOption {
      type = lib.types.str;
      description = "MW_UPDATES_ROOT_KEY: the root public key, hex (`mwctl key pub`). Devices must be claimed with it.";
    };

    credentialsFile = lib.mkOption {
      type = lib.types.nullOr lib.types.path;
      default = null;
      description = "A file with one line `<user>:<password>` for the updates router; read through LoadCredential.";
    };

    devices = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
      example = [ "/dev/serial/by-id/usb-Raspberry_Pi_Pico_E6614103E7-if00" ];
      description = "MW_UPDATER_DEVICES: the boards' serial ports (by-id paths survive re-enumeration after a firmware reboot).";
    };

    posixNodeLink = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = "Link the updater to the local mw-posix-node through a PTY pair (sets its MW_POSIX_NODE_LOCAL_SERIAL).";
    };

    poll = lib.mkOption {
      type = lib.types.ints.between 10 604800;
      default = 3600;
      description = "MW_UPDATES_POLL: seconds between passes (a publish also starts one).";
    };

    node = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      description = "MW_UPDATER_NODE: the status name (default: the host name).";
    };
  };

  config = lib.mkIf cfg.enable {
    assertions = [{
      assertion = devices != [ ];
      message = "services.mw-updater: set devices or posixNodeLink";
    }];

    users.users.mw-updater = {
      isSystemUser = true;
      group = "mw-updater";
      extraGroups = [ "dialout" ] ++ lib.optional cfg.posixNodeLink "mw-node-link";
    };
    users.groups.mw-updater = { };
    users.groups.mw-node-link = lib.mkIf cfg.posixNodeLink { };
    users.users.mw-posix-node.extraGroups = lib.mkIf cfg.posixNodeLink [ "mw-node-link" ];

    systemd.services.mw-node-link = lib.mkIf cfg.posixNodeLink {
      description = "PTY pair between mw-posix-node and mw-updater";
      wantedBy = [ "multi-user.target" ];
      before = [ "mw-posix-node.service" "mw-updater.service" ];
      serviceConfig = {
        RuntimeDirectory = "mw-node-link";
        RuntimeDirectoryMode = "0750";
        Group = "mw-node-link";
        ExecStart = lib.escapeShellArgs [
          "${pkgs.socat}/bin/socat"
          "pty,raw,echo=0,link=${linkDir}/node,group=mw-node-link,mode=660"
          "pty,raw,echo=0,link=${linkDir}/updater,group=mw-node-link,mode=660"
        ];
        Restart = "always";
        RestartSec = "1s";
      };
    };
    services.mw-posix-node.environment = lib.mkIf cfg.posixNodeLink {
      MW_POSIX_NODE_LOCAL_SERIAL = "${linkDir}/node";
    };
    systemd.services.mw-posix-node = lib.mkIf cfg.posixNodeLink {
      requires = [ "mw-node-link.service" ];
      after = [ "mw-node-link.service" ];
    };

    systemd.services.mw-updater = {
      description = "Updates channel client";
      wantedBy = [ "multi-user.target" ];
      wants = [ "network-online.target" ];
      after = [ "network-online.target" ] ++ lib.optional cfg.posixNodeLink "mw-node-link.service";
      requires = lib.optional cfg.posixNodeLink "mw-node-link.service";
      environment = {
        MW_UPDATES_HOST = cfg.host;
        MW_UPDATES_CHANNEL = cfg.channel;
        MW_UPDATES_ROOT_KEY = cfg.rootKey;
        MW_UPDATER_DEVICES = lib.concatStringsSep "," devices;
        MW_UPDATES_POLL = toString cfg.poll;
      }
      // lib.optionalAttrs (cfg.addr != null) { MW_UPDATES_ADDR = cfg.addr; }
      // lib.optionalAttrs (cfg.node != null) { MW_UPDATER_NODE = cfg.node; }
      // lib.optionalAttrs (cfg.credentialsFile != null) {
        MW_ZENOH_CREDENTIALS = "%d/zenoh";
      };
      unitConfig.StartLimitIntervalSec = 0;
      serviceConfig = {
        ExecStart = "${cfg.package}/bin/mw-updater";
        LoadCredential = lib.optional (cfg.credentialsFile != null) "zenoh:${cfg.credentialsFile}";
        User = "mw-updater";
        Group = "mw-updater";
        StateDirectory = "mw-updater";
        StateDirectoryMode = "0700";
        Restart = "always";
        RestartSec = "2s";
        RestartSteps = 6;
        RestartMaxDelaySec = "60s";
        CapabilityBoundingSet = "";
        AmbientCapabilities = "";
        NoNewPrivileges = true;
        ProtectSystem = "strict";
        ProtectHome = true;
        PrivateTmp = true;
        ProtectKernelTunables = true;
        ProtectKernelModules = true;
        ProtectKernelLogs = true;
        ProtectControlGroups = true;
        ProtectClock = true;
        ProtectHostname = true;
        ProtectProc = "invisible";
        RestrictAddressFamilies = [ "AF_INET" "AF_INET6" "AF_UNIX" "AF_NETLINK" ];
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
