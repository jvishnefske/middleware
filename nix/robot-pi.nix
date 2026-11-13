# The Pi robot's role (design.md MS50-MS54): nix/robot.nix plus the MCU
# gateway (nix/gateway.nix), the Linux payload host (nix/pi-node.nix, once
# its crate exists), runtime networking and SSH, and the memory budget of
# a 512 MiB Raspberry Pi Zero 2 W. Nothing here is Pi hardware: the SD
# image (nix/robot-pi-sd.nix) adds that, and checks.robot-pi-boots runs
# this module unchanged in an x86_64 VM.
#
# Runtime files, all in /var/lib/mw-robot (README "Raspberry Pi image"):
#   join-token, bundle.spiffe, robot.env   as nix/robot.nix; robot.env also
#                        carries MW_GATEWAY_*, the pi-node's variables and
#                        the static address below
#   wpa_supplicant.conf  Wi-Fi networks; mw-wifi runs wpa_supplicant on
#                        wlan0 only when it exists
#   authorized_keys      SSH public keys for root; sshd runs only when it
#                        exists, and never accepts passwords
# mw-provision copies any of these five names from the FAT boot partition's
# mw-robot/ directory into /var/lib/mw-robot at boot and deletes them
# there, so a card can be provisioned from any computer.
#
# Addressing: DHCP on eth0 and wlan0. With MW_NET_ADDRESS (CIDR) in
# robot.env, mw-net-config writes a networkd file for MW_NET_INTERFACE
# (default wlan0) with that address, MW_NET_GATEWAY and MW_NET_DNS
# (space-separated) into /run/systemd/network before networkd starts; it
# sorts before the DHCP file, so it wins for that interface. A malformed
# address is logged and ignored (DHCP stays).
#
# Envoy is nixpkgs' envoy-bin (nix/robot.nix's default: the upstream
# release binary, cached for x86_64 and aarch64).
#
# Memory (512 MiB): Envoy with one worker (--concurrency 1), Go's soft
# limit on spire-agent and spiffe-helper (GOMEMLIMIT), a MemoryMax on every
# service of ours, a zstd zram swap of half the RAM, and a small journal.
{ robotPackage, gatewayPackage, piNodePackage ? null }:
{ config, lib, pkgs, ... }:
let
  stateDir = "/var/lib/mw-robot";
  envFile = "${stateDir}/robot.env";
  provisioned = [ "join-token" "bundle.spiffe" "robot.env" "wpa_supplicant.conf" "authorized_keys" ];

  # Copies the allowlisted files from the boot partition, then removes
  # them there. Mounts the partition itself (it is noauto) when it exists.
  provision = pkgs.writeShellScript "mw-provision" ''
    set -u
    export PATH=${lib.makeBinPath [ pkgs.coreutils pkgs.util-linux ]}
    dev=/dev/disk/by-label/FIRMWARE
    [ -e "$dev" ] || exit 0
    mnt=/run/mw-provision
    mkdir -p "$mnt"
    mount -t vfat -o rw,nosuid,nodev,noexec,umask=0077 "$dev" "$mnt" || exit 0
    src="$mnt/mw-robot"
    if [ -d "$src" ]; then
      for f in ${lib.concatStringsSep " " provisioned}; do
        [ -f "$src/$f" ] || continue
        case "$f" in
          bundle.spiffe) mode=0644 ;;
          *) mode=0600 ;;
        esac
        install -m "$mode" -o root -g root "$src/$f" "${stateDir}/.$f.new" \
          && mv -f "${stateDir}/.$f.new" "${stateDir}/$f" \
          && rm -f "$src/$f" \
          && echo "mw-provision: installed $f"
      done
    fi
    umount "$mnt"
  '';

  # robot.env's MW_NET_* (already in the environment) to a networkd file.
  netConfig = pkgs.writeShellScript "mw-net-config" ''
    set -u
    export PATH=${lib.makeBinPath [ pkgs.coreutils pkgs.gnugrep ]}
    out=/run/systemd/network/40-mw-static.network
    rm -f "$out"
    addr=''${MW_NET_ADDRESS:-}
    [ -n "$addr" ] || exit 0
    iface=''${MW_NET_INTERFACE:-wlan0}
    cidr='^[0-9]{1,3}(\.[0-9]{1,3}){3}/[0-9]{1,2}$'
    ip4='^[0-9]{1,3}(\.[0-9]{1,3}){3}$'
    if ! echo "$addr" | grep -Eq "$cidr" || ! echo "$iface" | grep -Eq '^[a-z0-9]+$'; then
      echo "mw-net-config: ignoring MW_NET_ADDRESS=$addr MW_NET_INTERFACE=$iface (want a.b.c.d/n and an interface name); DHCP stays" >&2
      exit 0
    fi
    mkdir -p /run/systemd/network
    {
      printf '[Match]\nName=%s\n\n[Network]\nAddress=%s\n' "$iface" "$addr"
      gw=''${MW_NET_GATEWAY:-}
      if [ -n "$gw" ]; then
        echo "$gw" | grep -Eq "$ip4" && printf 'Gateway=%s\n' "$gw" \
          || echo "mw-net-config: ignoring MW_NET_GATEWAY=$gw" >&2
      fi
      for d in ''${MW_NET_DNS:-}; do
        echo "$d" | grep -Eq "$ip4" && printf 'DNS=%s\n' "$d" \
          || echo "mw-net-config: ignoring DNS server $d" >&2
      done
    } > "$out.new"
    mv -f "$out.new" "$out"
    echo "mw-net-config: $iface static $addr"
  '';
in
{
  imports = [ ./robot.nix ./gateway.nix ./pi-node.nix ];

  system.stateVersion = "26.05";

  services.mw-robot = {
    enable = true;
    package = robotPackage;
    envoy.concurrency = 1;
  };
  services.mw-gateway = {
    enable = true;
    package = gatewayPackage;
  };
  # udev may start the gateway during coldplug, before multi-user.target
  # pulls mw-provision in; bring it in so robot.env is in place first.
  systemd.services.mw-gateway = {
    wants = [ "mw-provision.service" ];
    after = [ "mw-provision.service" ];
  };
  services.mw-pi-node = lib.mkIf (piNodePackage != null) {
    enable = true;
    package = piNodePackage;
  };

  # --- memory ---------------------------------------------------------
  zramSwap = {
    enable = true;
    algorithm = "zstd";
    memoryPercent = 50;
  };
  services.journald.extraConfig = ''
    SystemMaxUse=48M
    RuntimeMaxUse=16M
  '';
  systemd.services.mw-envoy.serviceConfig.MemoryMax = "160M";
  systemd.services.mw-robot.serviceConfig.MemoryMax = "64M";
  systemd.services.spire-agent = {
    environment.GOMEMLIMIT = "48MiB";
    serviceConfig.MemoryMax = "96M";
  };
  systemd.services.spiffe-helper = {
    environment.GOMEMLIMIT = "24MiB";
    serviceConfig.MemoryMax = "48M";
  };
  documentation.enable = false;
  documentation.man.enable = false;
  documentation.nixos.enable = false;
  programs.command-not-found.enable = false;
  # A headless appliance: no fonts, X11 helpers, sudo (no wheel users),
  # containers or the default perl/rsync/strace set.
  fonts.fontconfig.enable = false;
  programs.ssh.setXAuthLocation = false;
  security.sudo.enable = false;
  boot.enableContainers = false;
  environment.defaultPackages = [ ];
  environment.stub-ld.enable = false;
  # Image size: no nixpkgs source in the registry or NIX_PATH (about
  # 200 MiB), no on-device nixos-rebuild and its Python (deploy with
  # `nixos-rebuild --target-host` from another machine, or a new image),
  # only the agent's output of spire (not the 140 MiB server), and
  # userborn instead of the Perl user activation script.
  nixpkgs.flake.setFlakeRegistry = false;
  nixpkgs.flake.setNixPath = false;
  system.disableInstallerTools = true;
  services.spire.agent.package = pkgs.spire.agent;
  services.userborn.enable = true;

  # --- accounts and SSH -----------------------------------------------
  users.mutableUsers = false;
  users.users.root.hashedPassword = "!";
  # Root logs in only with a key from the runtime authorized_keys file,
  # which NixOS cannot see at build time.
  users.allowNoPasswordLogin = true;
  services.openssh = {
    enable = true;
    authorizedKeysInHomedir = false;
    authorizedKeysFiles = lib.mkForce [ "${stateDir}/authorized_keys" ];
    settings = {
      PasswordAuthentication = false;
      KbdInteractiveAuthentication = false;
      PermitRootLogin = "prohibit-password";
      AllowUsers = [ "root" ];
    };
  };
  # SSH is off until the runtime file names a key.
  systemd.services.sshd.unitConfig.ConditionPathExists = "${stateDir}/authorized_keys";

  # --- provisioning from the boot partition ---------------------------
  systemd.services.mw-provision = {
    description = "Copy /var/lib/mw-robot files from the boot partition";
    wantedBy = [ "multi-user.target" ];
    after = [ "local-fs.target" "systemd-tmpfiles-setup.service" ];
    before = [
      "mw-net-config.service" "mw-wifi.service" "systemd-networkd.service"
      "spire-agent.service" "mw-envoy.service" "mw-robot.service"
      "mw-gateway.service" "mw-pi-node.service" "sshd.service"
    ];
    unitConfig.DefaultDependencies = false;
    serviceConfig = {
      Type = "oneshot";
      RemainAfterExit = true;
      ExecStart = provision;
    };
  };

  # --- networking -----------------------------------------------------
  networking.usePredictableInterfaceNames = false;
  networking.useNetworkd = true;
  networking.useDHCP = false;
  networking.wireless.enable = false;
  systemd.network = {
    enable = true;
    networks."50-mw-dhcp" = {
      matchConfig.Name = "eth0 wlan0";
      networkConfig.DHCP = "yes";
    };
    wait-online = {
      anyInterface = true;
      timeout = 30;
    };
  };

  systemd.services.mw-net-config = {
    description = "Static address from robot.env (MW_NET_ADDRESS)";
    wantedBy = [ "systemd-networkd.service" ];
    before = [ "systemd-networkd.service" ];
    after = [ "mw-provision.service" ];
    unitConfig.DefaultDependencies = false;
    serviceConfig = {
      Type = "oneshot";
      RemainAfterExit = true;
      EnvironmentFile = "-${envFile}";
      ExecStart = netConfig;
    };
  };

  systemd.services.mw-wifi = {
    description = "wpa_supplicant on wlan0 with ${stateDir}/wpa_supplicant.conf";
    # Started when wlan0 appears (a board without Wi-Fi never waits for
    # it), with mw-provision in the same transaction so a file dropped on
    # the boot partition is in place before the condition is checked.
    wantedBy = [ "sys-subsystem-net-devices-wlan0.device" ];
    bindsTo = [ "sys-subsystem-net-devices-wlan0.device" ];
    wants = [ "mw-provision.service" "network.target" ];
    after = [ "sys-subsystem-net-devices-wlan0.device" "mw-provision.service" ];
    before = [ "network.target" ];
    unitConfig.ConditionPathExists = "${stateDir}/wpa_supplicant.conf";
    serviceConfig = {
      ExecStart = "${pkgs.wpa_supplicant}/bin/wpa_supplicant -i wlan0 -D nl80211 -c ${stateDir}/wpa_supplicant.conf";
      Restart = "always";
      RestartSec = "5s";
      MemoryMax = "32M";
      CapabilityBoundingSet = [ "CAP_NET_ADMIN" "CAP_NET_RAW" ];
      NoNewPrivileges = true;
      ProtectSystem = "strict";
      ProtectHome = true;
      PrivateTmp = true;
      ReadWritePaths = [ "/run" ];
    };
  };
}
