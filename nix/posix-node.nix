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

    listen = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = "tcp/0.0.0.0:7447";
      example = null;
      description = ''
        MW_ZENOH_LISTEN: the Zenoh endpoint the node binds for peers, so
        `mwctl` reaches it over the LAN (zenoh:<endpoint>@<node>). Its port is
        announced by DNS-SD as _mw-node._tcp (scripts/mdns-browse.py, the HIL
        discovery). null: no listener and no announcement. The host opens its
        own firewall (the listen port, and UDP 5353 for DNS-SD).
      '';
    };

    osSlots = lib.mkOption {
      type = lib.types.nullOr (lib.types.listOf lib.types.str);
      default = null;
      example = [ "/dev/disk/by-partuuid/4b443234-02" "/dev/disk/by-partuuid/4b443234-03" ];
      description = ''
        The Linux A/B root's two slots, in slot order (A, B): MW_POSIX_NODE_OS_SLOTS.
        Their block devices are opened read-write by the node (the `disk`
        group; PrivateDevices off). null: no Linux root update (the default).
        design.md, "Linux A/B root (OS)".
      '';
    };

    osState = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      example = "/dev/disk/by-id/mmc-SD_card";
      description = ''
        The device holding the A/B boot state's two copies (the card's gap
        sectors, bootstate.rs COPY_LBA): MW_POSIX_NODE_OS_STATE. Required with
        osSlots.
      '';
    };

    localSerial = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      example = "/dev/ttyPS1";
      description = ''
        MW_POSIX_NODE_LOCAL_SERIAL: a serial port (a UART, a gadget, a PTY)
        the node serves mwctl on at 115200 baud, beside the Zenoh link. The
        host must not run a getty on it.
      '';
    };

    memoryMax = lib.mkOption {
      type = lib.types.str;
      default = "128M";
      description = "The unit's MemoryMax.";
    };
  };

  config = lib.mkIf cfg.enable {
    assertions = [{
      assertion = cfg.osSlots == null || cfg.osState != null;
      message = "services.mw-posix-node.osSlots needs osState (the boot state's device)";
    }];

    users.users.mw-posix-node = { isSystemUser = true; group = "mw-posix-node"; };
    users.groups.mw-posix-node = { };

    # The Linux root update: the node writes the inactive slot and the boot
    # state, and asks logind to reboot (the trial). Only with osSlots.
    security.polkit.extraConfig = lib.mkIf (cfg.osSlots != null) ''
      polkit.addRule(function (action, subject) {
        if (action.id == "org.freedesktop.login1.reboot" && subject.user == "mw-posix-node") {
          return polkit.Result.YES;
        }
      });
    '';

    # The SSH claim's second step, as root on the machine:
    # `sudo mw-posix-node-trust pending` / `accept <fingerprint>`
    # (posix-node/src/provision.rs), against the node's state directory.
    environment.systemPackages = [
      (pkgs.writeShellScriptBin "mw-posix-node-trust" ''
        export MW_POSIX_NODE_STATE=/var/lib/mw-posix-node
        exec ${lib.getExe cfg.package} trust "$@"
      '')
    ];

    systemd.services.mw-posix-node = {
      description = "Linux payload host";
      wantedBy = [ "multi-user.target" ];
      after = [ "mw-envoy.service" "network.target" ];
      environment = {
        MW_ZENOH_ROUTER = cfg.zenohRouter;
        MW_PORT = toString cfg.port;
      } // lib.optionalAttrs (cfg.listen != null) {
        MW_ZENOH_LISTEN = cfg.listen;
      } // lib.optionalAttrs (cfg.localSerial != null) {
        MW_POSIX_NODE_LOCAL_SERIAL = cfg.localSerial;
      } // lib.optionalAttrs (cfg.osSlots != null) {
        MW_POSIX_NODE_OS_SLOTS = lib.concatStringsSep "," cfg.osSlots;
        MW_POSIX_NODE_OS_STATE = cfg.osState;
        MW_POSIX_NODE_OS_REBOOT = "${lib.getExe' pkgs.systemd "systemctl"} --no-block reboot";
      } // cfg.environment;
      unitConfig.StartLimitIntervalSec = 0;
      serviceConfig = {
        # robot.env, then the board's own enrolment (tls/env, written by
        # `sys/cert/install`: the router URL and the TLS directory).
        EnvironmentFile = [ "-${envFile}" "-/var/lib/mw-posix-node/tls/env" ];
        ExecStartPre = "+${migrate}";
        ExecStart = lib.escapeShellArgs ([ (lib.getExe cfg.package) ] ++ cfg.args);
        User = "mw-posix-node";
        Group = "mw-posix-node";
        SupplementaryGroups = cfg.supplementaryGroups
          ++ lib.optional (cfg.osSlots != null) "disk"
          # The UART mwctl talks on (root:dialout 0660 on NixOS).
          ++ lib.optional (cfg.localSerial != null) "dialout";
        # With PrivateDevices off for localSerial, a closed policy still
        # limits /dev to that one port.
        DeviceAllow = lib.optional (cfg.localSerial != null) "${cfg.localSerial} rw";
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
        # A private /dev has no ttys, so localSerial needs the host's /dev
        # (DeviceAllow above keeps it to the one port).
        PrivateDevices = cfg.supplementaryGroups == [ ] && cfg.osSlots == null && cfg.localSerial == null;
        ProtectKernelTunables = true;
        ProtectKernelModules = true;
        ProtectKernelLogs = true;
        ProtectControlGroups = true;
        ProtectClock = true;
        ProtectHostname = true;
        ProtectProc = "invisible";
        # AF_NETLINK: Zenoh lists the network interfaces (getifaddrs) to join
        # its multicast scouting group; without it a listening node is never
        # scouted (`mwctl scout`).
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
