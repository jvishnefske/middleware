# NixOS module: the MCU gateway (`mw-gateway`) bound to one USB-attached
# board running the RP base firmware (design.md MS51).
#
#   udev     70-mw-pico.rules: a tty whose USB device has the RP base
#            firmware's identity (vendor 2e8a, product 000a on the RP2040 or
#            0009 on the RP2350, manufacturer `middleware`, product
#            `rp-base`; firmware-rp/src/usb.rs) gets the symlink
#            /dev/mw-pico, the systemd tag and SYSTEMD_WANTS=mw-gateway.
#            The strings keep the rule off other Pico SDK boards, which
#            share the ids. One such board per host: a second one would
#            fight over the symlink.
#   service  mw-gateway as the system user mw-gateway (supplementary group
#            dialout, the tty's group; nothing else has it), with
#            MW_GATEWAY_SERIAL=/dev/mw-pico set on its command line (after
#            robot.env, so no runtime file changes it): the gateway opens
#            exactly that path and never searches. The unit is BindsTo/After the device
#            unit and has no wantedBy: udev starts it when the board
#            appears (at boot too) and systemd stops it when the board goes.
#            Without a board it stays inactive, never failed. A crash or a
#            closed link restarts it with growing delays (2 s up to 1 min)
#            and no start limit, so it retries forever without spinning.
#
# Environment: `environment` below, then the shared robot.env
# (/var/lib/mw-robot/robot.env) on top, so a site sets MW_GATEWAY_* there.
# New gateway variables (for example MW_GATEWAY_UPDATERS for signed-update
# forwarding) go into `environment` or robot.env; the unit needs no change.
# A writable /var/lib/mw-gateway (StateDirectory) is there for state the
# gateway keeps across restarts.
{ config, lib, pkgs, utils, ... }:
let
  cfg = config.services.mw-gateway;
  envFile = "/var/lib/mw-robot/robot.env";
  deviceUnit = "${utils.escapeSystemdPath cfg.device}.device";
  link = lib.removePrefix "/dev/" cfg.device;

  rule = id: lib.concatStringsSep ", " [
    ''SUBSYSTEM=="tty"''
    ''KERNEL=="ttyACM[0-9]*"''
    ''ATTRS{idVendor}=="${id.vendor}"''
    ''ATTRS{idProduct}=="${id.product}"''
    ''ATTRS{manufacturer}=="${cfg.usb.manufacturer}"''
    ''ATTRS{product}=="${cfg.usb.product}"''
    ''SYMLINK+="${link}"''
    ''GROUP="dialout"''
    ''MODE="0660"''
    ''TAG+="systemd"''
    ''ENV{SYSTEMD_WANTS}+="mw-gateway.service"''
  ];
  rules = pkgs.writeTextDir "lib/udev/rules.d/70-mw-pico.rules" ''
    # The RP base firmware's CDC-ACM port (nix/gateway.nix, firmware-rp/src/usb.rs).
    ${lib.concatMapStringsSep "\n" rule cfg.usb.ids}
  '';
in
{
  options.services.mw-gateway = {
    enable = lib.mkEnableOption "the MCU gateway on the USB-attached RP base firmware board";

    package = lib.mkOption {
      type = lib.types.package;
      description = "mw-gateway; its `meta.mainProgram` is run.";
    };

    device = lib.mkOption {
      type = lib.types.strMatching "/dev/[A-Za-z0-9_-]+";
      default = "/dev/mw-pico";
      description = "The stable symlink udev gives the board, and the path the gateway opens.";
    };

    usb = {
      ids = lib.mkOption {
        type = lib.types.listOf (lib.types.submodule {
          options = {
            vendor = lib.mkOption { type = lib.types.strMatching "[0-9a-f]{4}"; description = "idVendor, lower-case hex."; };
            product = lib.mkOption { type = lib.types.strMatching "[0-9a-f]{4}"; description = "idProduct, lower-case hex."; };
          };
        });
        # firmware-rp/src/usb.rs: VID 0x2E8A; pid(Rp2040) 0x000A, pid(Rp2350) 0x0009.
        default = [
          { vendor = "2e8a"; product = "000a"; }
          { vendor = "2e8a"; product = "0009"; }
        ];
        description = "USB ids of the RP base firmware (firmware-rp/src/usb.rs `VID`, `pid`).";
      };
      manufacturer = lib.mkOption { type = lib.types.str; default = "middleware"; description = "iManufacturer (usb.rs `MANUFACTURER`)."; };
      product = lib.mkOption { type = lib.types.str; default = "rp-base"; description = "iProduct (usb.rs `PRODUCT`)."; };
    };

    environment = lib.mkOption {
      type = lib.types.attrsOf lib.types.str;
      default = { };
      example = { MW_GATEWAY_NAMESPACE = "pico"; MW_GATEWAY_UPDATERS = "spiffe://infra.local/ns/ops/sa/updater"; };
      description = ''
        Extra MW_* variables for the gateway (the crate's `config` docs list
        them), set before robot.env, which overrides them.
      '';
    };

    zenohRouter = lib.mkOption {
      type = lib.types.str;
      default = "tcp/127.0.0.1:17447";
      description = "MW_ZENOH_ROUTER: the robot Envoy's Zenoh listener.";
    };

    port = lib.mkOption {
      type = lib.types.port;
      default = 50052;
      description = "MW_PORT of the gateway's runner (off the robot app's default 50051).";
    };

    memoryMax = lib.mkOption {
      type = lib.types.str;
      default = "64M";
      description = "The unit's MemoryMax.";
    };
  };

  config = lib.mkIf cfg.enable {
    users.users.mw-gateway = {
      isSystemUser = true;
      group = "mw-gateway";
      extraGroups = [ "dialout" ];
    };
    users.groups.mw-gateway = { };

    services.udev.packages = [ rules ];

    systemd.services.mw-gateway = {
      description = "MCU gateway: the RP base firmware board on ${cfg.device} to Zenoh";
      bindsTo = [ deviceUnit ];
      after = [ deviceUnit "mw-envoy.service" "network.target" ];
      environment = {
        MW_ZENOH_ROUTER = cfg.zenohRouter;
        MW_PORT = toString cfg.port;
      } // cfg.environment;
      unitConfig.StartLimitIntervalSec = 0;
      serviceConfig = {
        EnvironmentFile = "-${envFile}";
        # Set here, after robot.env is read, so no runtime file can point
        # the gateway at another port.
        ExecStart = "${pkgs.coreutils}/bin/env MW_GATEWAY_SERIAL=${cfg.device} ${lib.getExe cfg.package}";
        User = "mw-gateway";
        Group = "mw-gateway";
        SupplementaryGroups = [ "dialout" ];
        StateDirectory = "mw-gateway";
        StateDirectoryMode = "0700";
        Restart = "always";
        RestartSec = "2s";
        RestartSteps = 6;
        RestartMaxDelaySec = "60s";
        MemoryMax = cfg.memoryMax;
        # Only the board's tty among the devices.
        DevicePolicy = "closed";
        DeviceAllow = [ "char-ttyACM rw" ];
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
        RestrictAddressFamilies = [ "AF_INET" "AF_INET6" "AF_UNIX" ];
        RestrictNamespaces = true;
        RestrictRealtime = true;
        RestrictSUIDSGID = true;
        LockPersonality = true;
        MemoryDenyWriteExecute = true;
        SystemCallArchitectures = "native";
        SystemCallFilter = [ "@system-service" "~@privileged" ];
        UMask = "0077";
      };
    };
  };
}
