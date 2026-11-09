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
      pkgs = import nixpkgs {
        inherit system;
        overlays = [ rust-overlay.overlays.default ];
      };
      inherit (pkgs) lib;

      # The cargo workspace without the parts that are not in it (the
      # firmware and payload workspaces, the web app) or that do not affect
      # the build (nix files, prose), so editing them rebuilds nothing.
      # payloads/jpeg and payloads/sdk stay: tools/payload-sim depends on
      # them by path.
      src = lib.fileset.toSource {
        root = ./.;
        fileset = lib.fileset.difference ./. (lib.fileset.unions [
          ./firmware
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

      rust = import ./nix/rust.nix {
        inherit pkgs lib crane src;
        binaries = import ./nix/binaries.nix;
      };

      # Image tag and labels: the commit for a clean tree, `dev` otherwise.
      tag = self.shortRev or "dev";
      images = import ./nix/images.nix {
        inherit pkgs lib tag;
        source = "https://github.com/jvishnefske/middleware";
        revision = self.rev or "dirty";
      };

      bins = rust.binaries // { zenohd = rust.zenohd; };
      imagePackages = lib.mapAttrs' (name: exe:
        lib.nameValuePair "image-${name}" (images.mkImage name exe)) bins;

      # The robot app: agent E's mw-robot once nix/binaries.nix lists it,
      # the `hello` placeholder until then.
      robotPackage = rust.binaries.mw-robot or rust.binaries.hello;
      labMac = "52:54:00:66:00:21";
      labAddress = "192.168.66.21/24";

      robotVm = nixpkgs.lib.nixosSystem {
        inherit system;
        modules = [ (import ./nix/robot-vm.nix { inherit robotPackage labMac labAddress; }) ];
      };
      vm = robotVm.config.virtualisation.vmVariant.system.build.vm;

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
          (pkgs.callPackage ./nix/tests/image-contents.nix { inherit image name; }))
        imagePackages;
    in
    {
      packages.${system} = bins // imagePackages // {
        robot-vm = launcher;
        inherit e2e;
        default = rust.binaries.hello;
      };

      apps.${system} = {
        e2e = { type = "app"; program = "${e2e}/bin/mw-e2e"; };
        robot-vm = { type = "app"; program = "${launcher}/bin/robot-vm"; };
      };

      nixosModules.robot = ./nix/robot.nix;
      nixosConfigurations.robot-vm = robotVm;

      checks.${system} = imageChecks // {
        clippy = rust.clippy;
        robot-boots = import ./nix/tests/robot-boots.nix { inherit pkgs robotPackage; };
        robot-vm = vm;

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

      formatter.${system} = pkgs.nixpkgs-fmt;
    };
}
