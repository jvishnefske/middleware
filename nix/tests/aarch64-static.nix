# checks.aarch64-static (design.md MS49): every nix/binaries.nix entry cross
# compiled for aarch64-unknown-linux-musl is an AArch64 ELF executable with
# no program interpreter and no shared-library dependencies (readelf reads
# any ELF machine, so this runs on the x86_64 build machine).
{ pkgs, lib, binaries }:
pkgs.runCommand "aarch64-static" { nativeBuildInputs = [ pkgs.binutils ]; } ''
  set -euo pipefail
  check() {
    exe=$1
    [ -x "$exe" ] || { echo "$exe missing" >&2; exit 1; }
    readelf -hW "$exe" | grep -Eq 'Machine:[[:space:]]+AArch64' \
      || { echo "$exe: not AArch64" >&2; exit 1; }
    if readelf -lW "$exe" | grep -q 'Requesting program interpreter'; then
      echo "$exe: has an ELF interpreter" >&2; exit 1
    fi
    if readelf -dW "$exe" | grep -q '(NEEDED)'; then
      echo "$exe: needs shared libraries" >&2; exit 1
    fi
    echo "$exe: static AArch64"
  }
  ${lib.concatMapStringsSep "\n" (b: "check ${b}/bin/${b.exe}") (lib.attrValues binaries)}
  touch $out
''
