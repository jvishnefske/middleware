{
  description = "middleware: static service binaries, OCI images, the robot VM and their checks";

  # nixos-26.05 rather than the lab repo's nixos-24.11: 24.11 has neither
  # spiffe-helper nor zenoh, and its spire (1.11) is four minors behind the
  # lab's SPIRE server (chart spire-0.30.3, SPIRE 1.15.3); 26.05 ships spire
  # 1.15.1, spiffe-helper 0.11 and envoy 1.36.
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
    crane.url = "github:ipetkov/crane";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, crane, rust-overlay }:
    let
      system = "x86_64-linux";
      armSystem = "aarch64-linux";
      pkgsFor = s: import nixpkgs {
        system = s;
        overlays = [ rust-overlay.overlays.default ];
      };
      pkgs = pkgsFor system;
      # The native aarch64 package set: built natively on aarch64 (CI), and
      # on x86_64 only a source of cache.nixos.org substitutes (the Pi
      # kernel and Envoy, nix/robot-pi-sd.nix).
      pkgsArm = pkgsFor armSystem;
      inherit (pkgs) lib;

      # The cargo workspace without the parts that are not in it (the
      # firmware and payload workspaces, the web app) or that do not affect
      # the build (nix files, prose), so editing them rebuilds nothing.
      # payloads/jpeg and payloads/sdk stay: tools/payload-sim depends on
      # them by path. esp-base's library (firmware/Cargo.toml, build.rs and
      # src) stays: posix-node depends on it by path; its build script only
      # adds linker arguments for ESP targets.
      src = lib.fileset.toSource {
        root = ./.;
        fileset = lib.fileset.difference ./. (lib.fileset.unions [
          (lib.fileset.difference ./firmware (lib.fileset.unions [
            ./firmware/Cargo.toml
            ./firmware/build.rs
            ./firmware/src
          ]))
          ./firmware-esp8266
          (lib.fileset.difference ./payloads (lib.fileset.unions [ ./payloads/jpeg ./payloads/sdk ]))
          ./nix
          ./flake.nix
          ./flake.lock
          ./scripts
          ./README.md
          ./CHANGELOG.md
          ./design.md
          (lib.fileset.difference ./web ./web/protocol-wasm)
          (lib.fileset.maybeMissing ./target)
        ]);
      };

      # nix/binaries.nix entries whose crate directory exists (mw-posix-node
      # joins once posix-node/ lands).
      binaryList = lib.filterAttrs
        (_: b: builtins.pathExists (./. + "/${b.dir}/Cargo.toml"))
        (import ./nix/binaries.nix);

      rust = import ./nix/rust.nix {
        inherit pkgs lib crane src;
        binaries = binaryList;
      };
      # The same binaries for aarch64 (design.md MS49): cross compiled on
      # x86_64, native on aarch64.
      armTarget = "aarch64-unknown-linux-musl";
      rustArmCross = import ./nix/rust.nix {
        inherit pkgs lib crane src;
        binaries = binaryList;
        target = armTarget;
      };
      rustArmNative = import ./nix/rust.nix {
        pkgs = pkgsArm;
        inherit lib crane src;
        binaries = binaryList;
        target = armTarget;
      };

      # Image tag and labels: the commit for a clean tree, `dev` otherwise.
      tag = self.shortRev or "dev";
      images = import ./nix/images.nix {
        inherit pkgs lib tag;
        source = "https://github.com/jvishnefske/middleware";
        revision = self.rev or "dirty";
      };

      bins = rust.binaries // { zenohd = rust.zenohd; };
      # The aarch64 cross builds, zenohd among them (packages.zenohd-aarch64
      # for an aarch64 host that runs its own router).
      armCrossBins = rustArmCross.binaries // { zenohd = rustArmCross.zenohd; };
      imagePackages = lib.mapAttrs' (name: exe:
        lib.nameValuePair "image-${name}" (images.mkImage name exe)) bins;

      # The robot app (nix/binaries.nix lists mw-robot).
      robotPackage = rust.binaries.mw-robot;
      labMac = "52:54:00:66:00:21";
      labAddress = "192.168.66.21/24";

      robotVm = nixpkgs.lib.nixosSystem {
        inherit system;
        modules = [ (import ./nix/robot-vm.nix { inherit robotPackage labMac labAddress; }) ];
      };
      vm = robotVm.config.virtualisation.vmVariant.system.build.vm;

      # The Pi robot (nix/robot-pi.nix on nix/robot-pi-sd.nix): `native`
      # evaluates for an aarch64 build machine, `cross` for x86_64.
      piPackages = bs: {
        robotPackage = bs.mw-robot;
        gatewayPackage = bs.mw-gateway;
        posixNodePackage = bs.mw-posix-node or null;
      };
      robotPi = { native }: nixpkgs.lib.nixosSystem {
        modules = [
          (import ./nix/robot-pi.nix
            (piPackages (if native then rustArmNative.binaries else rustArmCross.binaries)))
          (import ./nix/robot-pi-sd.nix (if native then { } else {
            buildPlatform = system;
            nativePkgs = pkgsArm;
          }))
        ];
      };
      robotPiNative = robotPi { native = true; };
      robotPiCross = robotPi { native = false; };
      sdImageOf = c: c.config.system.build.sdImage;

      # The services the Pi must have (checks.robot-pi-eval, robot-pi-image).
      piServices = [ "mw-robot" "mw-envoy" "spire-agent" "spiffe-helper" "mw-gateway" "mw-provision" "mw-net-config" "mw-wifi" "sshd" ]
        ++ lib.optional (binaryList ? mw-posix-node) "mw-posix-node";

      launcher = pkgs.replaceVarsWith {
        name = "robot-vm";
        src = ./nix/robot/run-vm.sh;
        dir = "bin";
        isExecutable = true;
        replacements = {
          inherit (pkgs) bash;
          inherit labMac;
          vm = "${vm}/bin/run-mw-robot-vm";
        };
      };

      e2e = pkgs.replaceVarsWith {
        name = "mw-e2e";
        src = ./nix/e2e.sh;
        dir = "bin";
        isExecutable = true;
        replacements = {
          inherit (pkgs) bash;
          robotVm = "${launcher}/bin/robot-vm";
          images = lib.concatStringsSep "\n"
            (lib.mapAttrsToList (n: p: "${n} ${p}") imagePackages);
        };
      };

      e2eGreps = lib.concatMapStringsSep "\n"
        (n: "echo \"$plan\" | grep -qF \"${n} ${imagePackages.${n}}\"")
        (lib.attrNames imagePackages);

      # Every image is a static, non-root, shell-less docker archive.
      imageChecks = lib.mapAttrs' (name: image:
        lib.nameValuePair "${name}-contents"
          (pkgs.callPackage ./nix/tests/image-contents.nix {
            inherit image;
            name = lib.removePrefix "image-" name;
          }))
        imagePackages;
    in
    {
      packages.${system} = bins // imagePackages
        // lib.mapAttrs' (n: p: lib.nameValuePair "${n}-aarch64" p) armCrossBins
        // {
        robot-vm = launcher;
        inherit e2e;
        default = rust.binaries.hello;
        sd-image-robot-pi = sdImageOf robotPiCross;
      };
      packages.${armSystem} = rustArmNative.binaries // {
        zenohd = rustArmNative.zenohd;
        sd-image-robot-pi = sdImageOf robotPiNative;
      };

      apps.${system} = {
        e2e = { type = "app"; program = "${e2e}/bin/mw-e2e"; };
        robot-vm = { type = "app"; program = "${launcher}/bin/robot-vm"; };
      };

      nixosModules.robot = ./nix/robot.nix;
      nixosModules.robot-pi-gateway = ./nix/gateway.nix;
      nixosModules.posix-node = ./nix/posix-node.nix;
      nixosModules.robot-posix-node = ./nix/posix-node.nix;
      # The module's name before posix-node hosted more than Pis.
      nixosModules.robot-pi-node = ./nix/posix-node.nix;
      nixosConfigurations.robot-vm = robotVm;
      nixosConfigurations.robot-pi = robotPiNative;
      nixosConfigurations.robot-pi-cross = robotPiCross;

      checks.${system} = imageChecks // {
        clippy = rust.clippy;
        robot-boots = import ./nix/tests/robot-boots.nix { inherit pkgs robotPackage; };
        robot-vm = vm;

        # The Pi system evaluates for both build machines with every
        # service, the udev rule and no baked-in runtime file (MS50-MS54).
        robot-pi-eval = import ./nix/tests/robot-pi-eval.nix {
          inherit pkgs lib piServices;
          configs = { native = robotPiNative; cross = robotPiCross; };
        };
        # The cross-built image: size, boot files, device trees, closure,
        # no secrets (MS53, MS54).
        robot-pi-image = pkgs.callPackage ./nix/tests/robot-pi-image.nix {
          inherit piServices;
          image = sdImageOf robotPiCross;
          toplevel = robotPiCross.config.system.build.toplevel;
        };
        # nix/robot-pi.nix in an x86_64 VM with no Pico attached (MS51, MS52).
        robot-pi-boots = import ./nix/tests/robot-pi-boots.nix {
          inherit pkgs;
          piModule = import ./nix/robot-pi.nix (piPackages rust.binaries);
        };
        # The aarch64 binaries are static aarch64 executables (MS49).
        static-startup = import ./nix/tests/static-startup.nix {
          inherit pkgs;
          inherit (rust) binaries;
        };

        aarch64-static = import ./nix/tests/aarch64-static.nix {
          inherit pkgs lib;
          binaries = armCrossBins;
        };

        # The launcher maps --tap/--bridge/--state onto QEMU and the share.
        robot-vm-launcher = pkgs.runCommand "robot-vm-launcher" { } ''
          set -eu
          cd "$TMPDIR"
          tap=$(${launcher}/bin/robot-vm --dry-run --tap tap-mwrobot --state s1)
          echo "$tap" | grep -qF "MW_ROBOT_STATE=$TMPDIR/s1 "
          echo "$tap" | grep -qF -- "-netdev tap,id=lab,ifname=tap-mwrobot,script=no,downscript=no"
          echo "$tap" | grep -qF -- "-device virtio-net-pci,netdev=lab,mac=${labMac}"
          test -d s1
          br=$(${launcher}/bin/robot-vm --dry-run --bridge br-infra --state s2 -- -m 2048)
          echo "$br" | grep -qF -- "-netdev bridge,id=lab,br=br-infra"
          echo "$br" | grep -qF -- " -m 2048"
          if ${launcher}/bin/robot-vm --bogus 2>/dev/null; then exit 1; fi
          touch $out
        '';

        # The e2e skeleton lists every image and runs nothing.
        e2e-plan = pkgs.runCommand "e2e-plan" { } ''
          set -eu
          plan=$(${e2e}/bin/mw-e2e)
          ${e2eGreps}
          echo "$plan" | grep -qF "would: mw load"
          touch $out
        '';
      };

      # Built on an aarch64 machine (the release workflow's arm runner).
      checks.${armSystem} = {
        robot-pi-image = pkgsArm.callPackage ./nix/tests/robot-pi-image.nix {
          inherit piServices;
          image = sdImageOf robotPiNative;
          toplevel = robotPiNative.config.system.build.toplevel;
        };
      };

      formatter.${system} = pkgs.nixpkgs-fmt;
    };
}
