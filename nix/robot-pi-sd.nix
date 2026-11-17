# The Pi hardware under nix/robot-pi.nix: nixpkgs' sd-image-aarch64 (U-Boot
# and the Raspberry Pi firmware on the FAT partition, extlinux, the
# mainline kernel, ext4 root grown to the card on first boot) for the
# BCM2710 boards, the Raspberry Pi Zero 2 W and the Pi 3 (design.md MS53).
#
# The image module's `profiles/base.nix` (installer tools, extra file
# systems, ZFS) is left out, so the closure stays small; the Pi Wi-Fi
# firmware (raspberrypiWirelessFirmware, the Zero 2 W's 43436 and the Pi 3's
# 43430/43455) replaces linux-firmware. Device trees: the firmware partition
# carries the Pi firmware's bcm2710-rpi-zero-2-w.dtb and bcm2710-rpi-3-b*.dtb
# (sd-image-aarch64's populateFirmwareCommands), and the kernel's dtbs,
# which U-Boot loads through extlinux, include
# broadcom/bcm2837-rpi-zero-2-w.dtb and bcm2837-rpi-3-b*.dtb;
# checks.robot-pi-image looks for both.
#
# `buildPlatform`: null builds natively (aarch64-linux); "x86_64-linux"
# cross compiles the system. In the cross build the kernel and envoy-bin
# (an upstream binary patched for the native glibc) come from the native
# aarch64 package set, which cache.nixos.org already holds, so the kernel
# is not compiled.
{ buildPlatform ? null, nativePkgs ? null }:
{ config, lib, pkgs, modulesPath, ... }:
{
  imports = [ "${modulesPath}/installer/sd-card/sd-image-aarch64.nix" ];
  disabledModules = [ "${modulesPath}/profiles/base.nix" ];

  networking.hostName = "mw-robot-pi";
  nixpkgs.hostPlatform = "aarch64-linux";
  nixpkgs.buildPlatform = lib.mkIf (buildPlatform != null) buildPlatform;

  boot.kernelPackages = lib.mkIf (nativePkgs != null) nativePkgs.linuxPackages;
  services.mw-robot.envoy.package = lib.mkIf (nativePkgs != null) nativePkgs.envoy-bin;

  hardware.enableRedistributableFirmware = lib.mkForce false;
  hardware.firmware = [ pkgs.raspberrypiWirelessFirmware ];

  boot.consoleLogLevel = 4;

  image.baseName = "mw-robot-pi";
  sdImage = {
    compressImage = true;
    expandOnBoot = true;
  };
}
