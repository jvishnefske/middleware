# checks.robot-pi-eval (design.md MS50-MS54): both Pi systems (native
# aarch64 and cross from x86_64) evaluate, down to the SD image derivation,
# and their configuration says what the image promises. Pure evaluation:
# the check's output is the two image derivation paths with their context
# dropped, so `nix flake check` builds nothing aarch64 for it.
#
#   * every service in `piServices` is defined, mw-gateway bound to the
#     /dev/mw-pico device unit and started by udev (no wantedBy), with the
#     path on its command line and dialout as its only extra group; no other
#     unit of ours gets dialout
#   * the 70-mw-pico.rules package is among the udev packages
#   * every service of ours runs as a non-root user with NoNewPrivileges and
#     a MemoryMax; Envoy runs one worker; zram swap is on
#   * root has no password, sshd takes no passwords, reads keys only from
#     /var/lib/mw-robot/authorized_keys and starts only once it exists
#   * /etc holds none of the runtime files (join-token, bundle.spiffe,
#     robot.env, wpa_supplicant.conf, authorized_keys, SSH host keys) and
#     networking.wireless declares no network
#   * networkd does DHCP on eth0 and wlan0 by default
#   * the image is an `img.zst` with the Pi firmware partition
{ pkgs, lib, configs, piServices }:
let
  ours = [ "mw-robot" "mw-envoy" "mw-gateway" ] ++ lib.optional (lib.elem "mw-posix-node" piServices) "mw-posix-node";
  secretNames = [ "join-token" "bundle.spiffe" "robot.env" "wpa_supplicant.conf" "authorized_keys" ];

  checkOne = name: c:
    let
      cfg = c.config;
      svc = cfg.systemd.services;
      gw = svc.mw-gateway;
      sc = s: svc.${s}.serviceConfig;
      etcNames = lib.attrNames cfg.environment.etc;
      fail = msg: throw "robot-pi-eval (${name}): ${msg}";
      need = cond: msg: if cond then true else fail msg;
      groupsOf = s: lib.toList ((sc s).SupplementaryGroups or [ ]);
      checks = [
        (need (lib.all (s: svc ? ${s}) piServices) "missing one of ${toString piServices}")
        (need (lib.elem "dev-mw\\x2dpico.device" gw.bindsTo) "mw-gateway not bound to dev-mw\\x2dpico.device")
        (need (gw.wantedBy == [ ]) "mw-gateway has a wantedBy (udev starts it)")
        (need (lib.hasInfix "MW_GATEWAY_SERIAL=/dev/mw-pico " (sc "mw-gateway").ExecStart) "mw-gateway does not open /dev/mw-pico")
        (need (groupsOf "mw-gateway" == [ "dialout" ]) "mw-gateway's groups are not exactly dialout")
        (need (lib.all (s: s == "mw-gateway" || !(lib.elem "dialout" (groupsOf s))) ours) "dialout outside mw-gateway")
        (need (lib.any (p: p.name == "70-mw-pico.rules") cfg.services.udev.packages) "no 70-mw-pico.rules")
        (need (lib.all (s: let u = (sc s).User or "root"; in u != "root" && u != "") ours) "a service of ours runs as root")
        (need (lib.all (s: (sc s).NoNewPrivileges or false) ours) "a service of ours lacks NoNewPrivileges")
        (need (lib.all (s: (sc s) ? MemoryMax) (ours ++ [ "spire-agent" "spiffe-helper" ])) "a service lacks MemoryMax")
        (need (lib.hasInfix "--concurrency 1" (sc "mw-envoy").ExecStart) "Envoy not at one worker")
        (need cfg.zramSwap.enable "no zram swap")
        (need (cfg.users.users.root.hashedPassword == "!") "root has a password")
        (need (!cfg.services.openssh.settings.PasswordAuthentication) "sshd accepts passwords")
        (need (cfg.services.openssh.authorizedKeysFiles == [ "/var/lib/mw-robot/authorized_keys" ]) "sshd reads keys elsewhere")
        (need (svc.sshd.unitConfig.ConditionPathExists == "/var/lib/mw-robot/authorized_keys") "sshd starts without a key file")
        (need (!lib.any (n: lib.elem (baseNameOf n) secretNames || lib.hasPrefix "ssh/ssh_host_" n) etcNames) "a runtime file is in /etc")
        (need (cfg.networking.wireless.networks == { }) "a Wi-Fi network is built in")
        (need (cfg.systemd.network.networks."50-mw-dhcp".networkConfig.DHCP == "yes") "no default DHCP")
        (need (cfg.image.extension == "img.zst") "the image is not img.zst")
        (need (lib.hasInfix "bcm2710-rpi-zero-2-w.dtb" cfg.sdImage.populateFirmwareCommands) "no Zero 2 W device tree on the firmware partition")
      ];
    in
    builtins.deepSeq checks
      "${name} ${builtins.unsafeDiscardStringContext cfg.system.build.sdImage.drvPath}";
in
pkgs.writeText "robot-pi-eval" (lib.concatStringsSep "\n" (lib.mapAttrsToList checkOne configs) + "\n")
