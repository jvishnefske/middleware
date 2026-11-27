# The end-to-end demo's client (docs/demo.md, `just demo`): the x86_64 base
# (nix/x86-base-image.nix) as a QEMU VM, with nothing else added.
#
# It boots unenrolled, with the node on the VM's own loopback Zenoh router.
# The demo provisions it over the network (`sys/cert/csr`, a signature from the
# enrolment server, `sys/cert/install`); the installed enrolment points the
# node at the mTLS hub and the node restarts into it.
#
#   nix build .#x86-demo-vm
#   QEMU_NET_OPTS=hostfwd=tcp:127.0.0.1:17447-:7447 NIX_DISK_IMAGE=vm.qcow2 \
#     result/bin/run-*-vm
#
# The guest reaches the host at 10.0.2.2 (QEMU's user network).
{ lib, ... }:
{
  # A fixed node id: the demo names it in the hub's access rules and in the
  # certificate's CN (kd240-<id>) before the VM has ever run.
  services.mw-posix-node.environment.MW_POSIX_NODE_NODE_ID = "0d1e2f3a4b5c6d7e";

  # No TPM in the VM, and the node's own mTLS reads its key from a file.
  mw.x86.tpm.enable = lib.mkForce false;

  # The serial console (the demo logs it); a root shell on it for debugging.
  services.getty.autologinUser = "root";
  documentation.enable = false;
  # The node's own log on the console, so the demo's VM log shows what it does.
  services.journald.extraConfig = ''
    ForwardToConsole=yes
    TTYPath=/dev/ttyS0
    MaxLevelConsole=info
  '';
  boot.kernelParams = [ "console=ttyS0" ];

  virtualisation.vmVariant.virtualisation = {
    graphics = false;
    memorySize = 1024;
    cores = 2;
  };
}
