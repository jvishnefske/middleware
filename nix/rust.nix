# Static musl builds of the workspace's service binaries with crane, for
# one target triple per import.
#
# The toolchain comes from rust-overlay (prebuilt, running on the build
# machine, with the target's musl standard library); C code in the
# dependency graph (ring, through zenoh's QUIC link) is compiled and linked
# by nixpkgs' static musl C compiler for that target, so every binary is a
# static-pie executable with no interpreter.
#
#   target  `x86_64-unknown-linux-musl` (the default, design.md MS40) or
#           `aarch64-unknown-linux-musl` (MS49). When the target's CPU is
#           the build machine's, the compiler is `pkgsStatic`'s (a native
#           build); otherwise it is the `pkgsCross` set's static compiler
#           (a cross build from x86_64, or to x86_64 from aarch64). Both
#           give the same kind of executable.
{ pkgs, lib, crane, src, binaries, target ? "x86_64-unknown-linux-musl" }:
let
  # Pinned, not `latest`, so the binaries are reproducible from flake.lock.
  toolchain = pkgs.rust-bin.stable."1.95.0".minimal.override {
    targets = [ target ];
    extensions = [ "clippy" ];
  };
  craneLib = (crane.mkLib pkgs).overrideToolchain toolchain;

  # The pkgsCross set whose static variant targets each triple.
  crossSets = {
    "x86_64-unknown-linux-musl" = "gnu64";
    "aarch64-unknown-linux-musl" = "aarch64-multiplatform";
  };
  targetCpu = lib.head (lib.splitString "-" target);
  native = targetCpu == pkgs.stdenv.buildPlatform.parsed.cpu.name;
  muslCC =
    if native then pkgs.pkgsStatic.stdenv.cc
    else pkgs.pkgsCross.${crossSets.${target} or (throw "nix/rust.nix: no cross set for ${target}")}.pkgsStatic.stdenv.cc;
  prefix = "${muslCC}/bin/${muslCC.targetPrefix}";

  # Cargo's and the cc crate's per-target variable names.
  targetUpper = lib.toUpper (lib.replaceStrings [ "-" ] [ "_" ] target);
  targetLower = lib.replaceStrings [ "-" ] [ "_" ] target;

  # Everything every build of this workspace shares.
  common = {
    inherit src;
    pname = "mw-workspace";
    version = "0.0.0";
    strictDeps = true;
    doCheck = false;
    CARGO_BUILD_TARGET = target;
    CARGO_BUILD_RUSTFLAGS = "-C target-feature=+crt-static";
    "CARGO_TARGET_${targetUpper}_LINKER" = "${prefix}cc";
    "CC_${targetLower}" = "${prefix}cc";
    "AR_${targetLower}" = "${prefix}ar";
    # Build scripts run on the build machine and use the ordinary stdenv
    # compiler; the musl compiler above is reached through the variables
    # alone.
  };

  # Dependencies only, built once and shared by every binary and check.
  cargoArtifacts = craneLib.buildDepsOnly (common // {
    cargoExtraArgs = "--locked -p mw-service";
  });

  cargoArg = b:
    if b ? example then "--example ${b.example}"
    else if b ? bin then "--bin ${b.bin}"
    else throw "nix/binaries.nix: give `example` or `bin`";
  outName = b: b.example or b.bin;
  featureArg = b:
    lib.optionalString (b ? features && b.features != [ ])
      "--features ${lib.concatStringsSep "," b.features}";

  version = b: (craneLib.crateNameFromCargoToml {
    cargoToml = src + "/${b.dir}/Cargo.toml";
  }).version;

  buildBinary = name: b: craneLib.buildPackage (common // {
    inherit cargoArtifacts;
    pname = name;
    version = version b;
    cargoExtraArgs = "--locked -p ${b.package} ${cargoArg b} ${featureArg b}";
    # crane's default install only picks up `[[bin]]` targets; examples
    # live under `examples/`, so install the one artifact explicitly.
    installPhaseCommand = ''
      dir=target/${target}/release
      exe=$dir/${outName b}
      [ -x "$exe" ] || exe=$dir/examples/${outName b}
      install -Dm0555 "$exe" $out/bin/${name}
    '';
    passthru = { exe = name; version = version b; inherit target; };
    meta.mainProgram = name;
  });
in
{
  inherit craneLib toolchain target;
  binaries = lib.mapAttrs buildBinary binaries;

  # Clippy over the runner crate and its examples, with the gate's flags.
  clippy = craneLib.cargoClippy (common // {
    inherit cargoArtifacts;
    pname = "mw-service-clippy";
    cargoExtraArgs = "--locked -p mw-service";
    cargoClippyExtraArgs = "--all-targets -- -D warnings";
  });

  # zenohd built from the same zenoh release as Cargo.lock's `zenoh`
  # (nixpkgs packages an older 1.x), static like the rest. Evaluation
  # fails when the lock moves to another zenoh until this pin follows.
  zenohd = let
    zenohVersion = "1.10.1";
    locked = lib.findFirst (p: p.name == "zenoh") null
      (builtins.fromTOML (builtins.readFile (src + "/Cargo.lock"))).package;
    zsrc = pkgs.fetchFromGitHub {
      owner = "eclipse-zenoh";
      repo = "zenoh";
      rev = zenohVersion;
      hash = "sha256-RF/1xvJxx8D4LQWJOUZuxYvWRIpFdGS0BWJAhsiDXho=";
    };
    zcommon = common // {
      src = zsrc;
      pname = "zenohd";
      version = zenohVersion;
      cargoExtraArgs = "--locked -p zenohd";
    };
  in
    assert lib.assertMsg (locked != null && locked.version == zenohVersion)
      "nix/rust.nix: zenohd ${zenohVersion} but Cargo.lock has zenoh ${locked.version or "none"}; update zenohVersion and its hash";
    craneLib.buildPackage (zcommon // {
    cargoArtifacts = craneLib.buildDepsOnly zcommon;
    passthru.exe = "zenohd";
    meta.mainProgram = "zenohd";
  });
}
