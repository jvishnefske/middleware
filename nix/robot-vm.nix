# The emulated robot: nix/robot.nix on a small NixOS guest, run under QEMU
# (`nix run .#robot-vm`, which wraps `system.build.vm`; see README "Robot VM").
#
# Networking: the default QEMU user-mode NIC (outbound only) plus, when the
# launcher is given `--tap` or `--bridge`, a second NIC with the MAC below,
# statically addressed on the lab subnet (the lab bridge has no DHCP).
# `labMac`/`labAddress` come from flake.nix, which hands the MAC to the
# launcher too.
{ robotPackage, labMac, labAddress }:
{ ... }:
{
  imports = [ ./robot.nix ];

  system.stateVersion = "26.05";
  networking.hostName = "mw-robot";
  networking.useNetworkd = true;
  systemd.network.networks."40-lab" = {
    matchConfig.MACAddress = labMac;
    address = [ labAddress ];
    linkConfig.RequiredForOnline = "no";
  };

  services.mw-robot = {
    enable = true;
    package = robotPackage;
  };

  virtualisation.vmVariant = {
    virtualisation = {
      memorySize = 1024;
      cores = 2;
      graphics = false;
      # The runtime files, from the host directory the launcher names
      # (MW_ROBOT_STATE, default ./robot-state). Read at boot.
      sharedDirectories.mw-robot = {
        source = ''"''${MW_ROBOT_STATE:-$PWD/robot-state}"'';
        target = "/var/lib/mw-robot";
      };
    };
    # A development VM on an isolated lab: root on the serial console.
    services.getty.autologinUser = "root";
  };
}
