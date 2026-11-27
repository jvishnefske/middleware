# The middleware's x86_64 base system, as a NixOS module: the Linux node
# (mw-posix-node, nix/posix-node.nix) on a generic x86_64 machine, with its LAN
# side and its enrolment side. Nothing here is machine-specific, and nothing
# here is an orchestrator: a host that wants one (k3s, say) adds it on top
# (the nixos repo's rainier does).
#
# Router: mw-posix-node publishes through a Zenoh router (MW_ZENOH_ROUTER is
# required, and the node exits when it cannot connect). The robot role gets one
# from its Envoy; here a zenohd runs on loopback only (tcp/127.0.0.1:17447, the
# module's default), as the lathe's KD240 does. Set mw.x86.localRouter = false
# and services.mw-posix-node.zenohRouter to use another one.
#
# LAN: the node listens for Zenoh peers on tcp/0.0.0.0:7447 and announces
# itself by DNS-SD (_mw-node._tcp), so `mwctl scout`, `just lan` and tests/hil
# find it and `mwctl --zenoh tcp/<ip>:7447 --node <id>` reaches it. Any
# Ethernet NIC takes DHCP, and the firewall opens the listener (TCP 7447) and
# DNS-SD (UDP 5353). SSH is on; the host brings the keys.
#
# Provision: the node's identity is a certificate request it makes itself
# (`sys/cert/csr`), signed by the mesh CA (tools/mw-pki) and installed back
# (`sys/cert/install`, `just provision`); the key never leaves the node. With
# mw.x86.tpm the key lives in the TPM 2.0: a tpm2-pkcs11 token the node makes
# on first start, which its certificate store signs through (posix-node/src/
# cert.rs `Chip::Pkcs11`, the SecureChip trait). Without it the key is a file in
# the node's state directory.
#
# The node id is stable per machine: the hardware UUID (DMI product_uuid), or
# the first NIC's MAC when the firmware has none, written as a machine-id file
# the node hashes (a live system's /etc/machine-id is new at every boot).
#
# Persistence: on a live (tmpfs) root the state directory (the trust record,
# the certificate store, the TPM token's key blobs) is gone at reboot.
# mw.x86.stateLabel names an ext4 filesystem to mount there instead.
{ config, lib, pkgs, ... }:
let
  cfg = config.mw.x86;
  state = "/var/lib/mw-posix-node";
  tpm = cfg.tpm.enable;
  pkcs11 = config.security.tpm2.pkcs11.package;
  store = "${state}/tpm2-pkcs11";
  # No plugins, no scouting: a router on loopback and nothing else.
  zenohdConfig = pkgs.writeText "zenohd.json5" (builtins.toJSON {
    mode = "router";
    listen.endpoints = [ config.services.mw-posix-node.zenohRouter ];
    scouting.multicast.enabled = false;
    scouting.gossip.enabled = false;
    plugins_loading.enabled = false;
  });
  tpmEnv = {
    TPM2_PKCS11_STORE = store;
    TPM2_PKCS11_TCTI = "device:/dev/tpmrm0";
    TPM2TOOLS_TCTI = "device:/dev/tpmrm0";
    TPM2_PKCS11_BACKEND = "esysdb"; # no FAPI probing
    TPM2_PKCS11_LOG_LEVEL = "0";
  };
in
{
  imports = [ ./posix-node.nix ];

  options.mw.x86 = {
    listen = lib.mkOption {
      type = lib.types.str;
      default = "tcp/0.0.0.0:7447";
      description = "The Zenoh endpoint the node listens on for LAN peers.";
    };
    provisionOverLan = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = ''
        Serve the enrolment keys `sys/cert/csr` and `sys/cert/install` over the
        LAN (the gateway's MW_GATEWAY_SYS_ALLOW beside `sys/info` and `sys/log`),
        so `just provision` can enrol the node from another machine. The CSR is
        public. An install is refused once the node is claimed (a signed pinned
        key; signed installs are not built yet), so until then the first install
        wins: trust on first use. Turn this off on a network you do not control
        and enrol over the local serial port instead.
      '';
    };
    localRouter = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = "Run a zenohd on loopback for the node (see the router note above).";
    };
    zenohdPackage = lib.mkOption {
      type = lib.types.package;
      description = "zenohd, of the zenoh release the node is built with.";
    };
    tpm = {
      enable = lib.mkEnableOption "the node's key in the TPM 2.0 (a tpm2-pkcs11 token)";
      tokenLabel = lib.mkOption { type = lib.types.str; default = "mw-node"; };
      keyLabel = lib.mkOption {
        type = lib.types.str;
        default = "mw-device";
        description = "The key's label in the token (MW_CERT_PKCS11_LABEL).";
      };
    };
    stateLabel = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      example = "mw-state";
      description = ''
        The label of an ext4 filesystem to mount at ${state}, so the node's
        state survives a reboot of a live system. null: the state lives on
        whatever the root is.
      '';
    };
  };

  config = {
    services.mw-posix-node = {
      enable = true;
      listen = cfg.listen;
      # The node's identity file (below) and, with the TPM, its token's settings.
      environment = {
        MW_POSIX_NODE_MACHINE_ID = "/run/mw-node/machine-id";
        # What the node answers per node over Zenoh (posix-node's sysnode.rs): its
        # info, log and manifest (read-only) and, to enrol it, the CSR and the
        # install. The gateway's allowlist, at most eight keys. (The device
        # itself refuses `sys/trust/info` from the gateway link, so it is not
        # listed.)
        MW_GATEWAY_SYS_ALLOW = lib.concatStringsSep "," (
          [ "sys/info" "sys/log" "sys/manifest" ]
          ++ lib.optionals cfg.provisionOverLan [ "sys/cert/csr" "sys/cert/install" ]
        );
      } // lib.optionalAttrs tpm (tpmEnv // {
        MW_CERT_PKCS11_LABEL = cfg.tpm.keyLabel;
        PKCS11_MODULE = "${pkcs11}/lib/libtpm2_pkcs11.so";
        PKCS11_TOKEN = cfg.tpm.tokenLabel;
      });
      # The TPM device node is group tss; a non-empty list also lifts the
      # module's PrivateDevices.
      supplementaryGroups = lib.optional tpm "tss";
    };

    networking.firewall = {
      allowedTCPPorts = [ 7447 ];
      allowedUDPPorts = [ 5353 7446 ]; # DNS-SD; Zenoh multicast scouting (`mwctl scout`)
    };

    # network-online means an IPv4 address, not just the IPv6 link-local one:
    # the node joins the Zenoh scouting group on the NICs that have one.
    systemd.network.wait-online.extraArgs = [ "--any" "--ipv4" ];

    # DHCP on any Ethernet NIC; a host with a fixed address says so for its NIC
    # (its unit sorts first).
    networking.useNetworkd = lib.mkDefault true;
    systemd.network.networks."90-mw-lan" = lib.mkDefault {
      matchConfig.Name = "en* eth*";
      networkConfig.DHCP = "yes";
    };

    services.openssh = {
      enable = lib.mkDefault true;
      settings.PasswordAuthentication = lib.mkDefault false;
    };

    systemd.services.mw-zenohd = lib.mkIf cfg.localRouter {
      description = "Zenoh router for the middleware node (loopback only)";
      wantedBy = [ "multi-user.target" ];
      before = [ "mw-posix-node.service" ];
      after = [ "network.target" ];
      serviceConfig = {
        ExecStart = "${lib.getExe' cfg.zenohdPackage "zenohd"} --config ${zenohdConfig}";
        DynamicUser = true;
        StateDirectory = "mw-zenohd";
        Restart = "always";
        RestartSec = "2s";
        NoNewPrivileges = true;
        ProtectSystem = "strict";
        ProtectHome = true;
        PrivateTmp = true;
        PrivateDevices = true;
        RestrictAddressFamilies = [ "AF_INET" "AF_INET6" "AF_UNIX" "AF_NETLINK" ];
        MemoryMax = "128M";
      };
    };

    systemd.services.mw-posix-node = {
      # With the network up: the node announces itself by DNS-SD on the NIC.
      after = [ "network-online.target" "mw-node-identity.service" ]
        ++ lib.optional tpm "mw-tpm-provision.service"
        ++ lib.optional cfg.localRouter "mw-zenohd.service";
      requires = [ "mw-node-identity.service" ];
      wants = [ "network-online.target" ] ++ lib.optional tpm "mw-tpm-provision.service"
        ++ lib.optional cfg.localRouter "mw-zenohd.service";
      # The token's PIN, written by mw-tpm-provision (absent without the TPM).
      serviceConfig.EnvironmentFile = lib.mkIf tpm [ "-${state}/tpm.env" ];
    };

    # An installed enrolment (`sys/cert/install` writes tls/env last) takes
    # effect by restarting the node, which then reads its router and TLS
    # directory. The pause lets the install's reply leave first.
    systemd.paths.mw-posix-node-enrolment = {
      wantedBy = [ "multi-user.target" ];
      pathConfig = {
        PathChanged = "${state}/tls/env";
        Unit = "mw-posix-node-restart.service";
      };
    };
    systemd.services.mw-posix-node-restart = {
      description = "Restart the middleware node to use its new enrolment";
      serviceConfig = {
        Type = "oneshot";
        ExecStartPre = "${pkgs.coreutils}/bin/sleep 3";
        ExecStart = "${pkgs.systemd}/bin/systemctl restart mw-posix-node.service";
      };
    };

    # A stable machine id for the node id hash.
    systemd.services.mw-node-identity = {
      description = "Stable machine id for the middleware node";
      wantedBy = [ "multi-user.target" ];
      before = [ "mw-posix-node.service" ];
      serviceConfig = { Type = "oneshot"; RemainAfterExit = true; RuntimeDirectory = "mw-node"; RuntimeDirectoryPreserve = true; };
      path = [ pkgs.coreutils ];
      script = ''
        u=$(tr -d '\n-' < /sys/class/dmi/id/product_uuid 2>/dev/null | tr 'A-F' 'a-f' || true)
        case "$u" in
          ""|00000000000000000000000000000000|ffffffffffffffffffffffffffffffff)
            # no usable UUID: hash the first NIC's MAC
            u=$(cat /sys/class/net/e*/address 2>/dev/null | sort | head -1 | sha256sum | cut -c1-32) ;;
        esac
        printf '%s\n' "$u" > /run/mw-node/machine-id
        chmod 0644 /run/mw-node/machine-id
      '';
    };

    # The token the node signs through, made once (its presence is tpm.env).
    systemd.services.mw-tpm-provision = lib.mkIf tpm {
      description = "Make the middleware node's TPM token (tpm2-pkcs11)";
      before = [ "mw-posix-node.service" ];
      unitConfig.ConditionPathExists = [ "/dev/tpmrm0" "!${state}/tpm.env" ];
      environment = tpmEnv;
      path = [ pkgs.tpm2-pkcs11.bin pkgs.openssl pkgs.coreutils ];
      serviceConfig = {
        Type = "oneshot";
        User = "mw-posix-node";
        Group = "mw-posix-node";
        SupplementaryGroups = [ "tss" ];
        StateDirectory = "mw-posix-node";
        StateDirectoryMode = "0700";
        UMask = "0077";
      };
      script = ''
        set -eu
        pin=$(openssl rand -hex 16)
        sopin=$(openssl rand -hex 16)
        mkdir -p ${store}
        tpm2_ptool init --path ${store} >/dev/null
        tpm2_ptool addtoken --path ${store} --pid=1 --label=${cfg.tpm.tokenLabel} \
          --sopin="$sopin" --userpin="$pin" >/dev/null
        # Last, so a half-made token is made again.
        printf 'PKCS11_PIN=%s\n' "$pin" > ${state}/tpm.env
      '';
    };

    security.tpm2 = lib.mkIf tpm {
      enable = true;
      pkcs11.enable = true;
    };

    fileSystems.${state} = lib.mkIf (cfg.stateLabel != null) {
      device = "/dev/disk/by-label/${cfg.stateLabel}";
      fsType = "ext4";
      options = [ "noatime" "nofail" "x-systemd.device-timeout=10s" ];
    };

    # efibootmgr: a booted node can set the firmware's next boot (a re-test of a
    # live system from a machine whose firmware boots another disk first).
    environment.systemPackages = [ pkgs.efibootmgr ]
      ++ lib.optionals tpm [ pkgs.tpm2-tools pkgs.tpm2-pkcs11.bin ];
  };
}
