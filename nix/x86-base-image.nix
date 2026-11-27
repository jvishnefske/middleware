# The middleware's x86_64 base *image*: nix/x86-base.nix on a bootable UEFI
# disk (systemd-repart: a VFAT ESP with systemd-boot and a UKI, an ext4 root).
# `packages.x86_64-linux.x86-base-image` builds it, `nixosConfigurations.x86-base`
# is the system.
#
# Every setting here that a downstream system may want to change is a
# `mkDefault` (or a whole-value default), so a host that builds on this base
# (the nixos repo's rainier: a live USB, a tmpfs root, k3s on top) overrides the
# disk layout and boot with ordinary definitions and keeps the node, its LAN and
# its provisioning. A host that wants only the services imports
# nix/x86-base.nix instead.
{ config, lib, pkgs, modulesPath, ... }:
let
  efiArch = pkgs.stdenv.hostPlatform.efiArch; # "x64"
in
{
  imports = [
    ./x86-base.nix
    "${modulesPath}/image/repart.nix"
  ];

  boot.loader.grub.enable = lib.mkDefault false;
  boot.loader.systemd-boot.enable = lib.mkDefault true;
  boot.loader.efi.canTouchEfiVariables = lib.mkDefault false;
  boot.initrd.systemd.enable = lib.mkDefault true;
  boot.initrd.availableKernelModules = [
    "usb_storage" "uas" "sd_mod" "xhci_pci" "ehci_pci" "ahci" "nvme" "sdhci_pci" "mmc_block"
  ];

  fileSystems."/" = lib.mkDefault { device = "/dev/disk/by-label/nixos"; fsType = "ext4"; };
  fileSystems."/boot" = lib.mkDefault { device = "/dev/disk/by-label/ESP"; fsType = "vfat"; };

  hardware.enableRedistributableFirmware = lib.mkDefault true;
  system.stateVersion = lib.mkDefault "26.05";

  image.repart = {
    name = lib.mkDefault "mw-x86-base";
    partitions = lib.mkDefault {
      "10-esp" = {
        contents = {
          "/EFI/BOOT/BOOT${lib.toUpper efiArch}.EFI".source =
            "${pkgs.systemd}/lib/systemd/boot/efi/systemd-boot${efiArch}.efi";
          "/EFI/Linux/${config.system.boot.loader.ukiFile}".source =
            "${config.system.build.uki}/${config.system.boot.loader.ukiFile}";
        };
        repartConfig = {
          Type = "esp";
          Format = "vfat";
          Label = "ESP";
          SizeMinBytes = "256M";
        };
      };
      "20-root" = {
        storePaths = [ config.system.build.toplevel ];
        repartConfig = {
          Type = "root";
          Format = "ext4";
          Label = "nixos";
          Minimize = "guess";
        };
      };
    };
  };
}
