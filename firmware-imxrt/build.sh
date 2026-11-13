#!/usr/bin/env bash
# Builds and flashes the i.MX RT base firmware (MIMXRT1050-EVKB).
#
#   ./build.sh boot                      the bootloader ELF and its block-0 binary
#   ./build.sh app [--version M.m]       the firmware ELF and its slot image (with
#                                        the payload runtime; APP_FEATURES=app for
#                                        a protocol-only build)
#   ./build.sh image                     boot + app
#   ./build.sh flash [boot] [a|b] [--version M.m]
#                                        builds, then programs and reads back over
#                                        SWD (tools/flash.py): the bootloader at 0x0,
#                                        the slot image at slot A (0x400000) or B
#                                        (0x800000)
#
# Outputs in target/thumbv7em-none-eabihf/release/ (or $CARGO_TARGET_DIR):
#   imxrt-boot.bin            block 0 contents (FCB, IVT, DCD, bootloader)
#   imxrt-base-V.slot         slot image (header + SDRAM image), also what
#                             `mwctl fw update` sends
# The version defaults to the crate's major.minor.
set -euo pipefail
cd "$(dirname "$0")"

target=thumbv7em-none-eabihf
out="${CARGO_TARGET_DIR:-target}/$target/release"
objcopy="$(command -v llvm-objcopy || command -v rust-objcopy)"
crate_version="$(sed -n 's/^version = "\([0-9]*\)\.\([0-9]*\).*/\1.\2/p' Cargo.toml | head -1)"
version="$crate_version"

build_boot() {
  cargo build --release -j 2 --target "$target" --features boot --bin imxrt-boot
  "$objcopy" -O binary "$out/imxrt-boot" "$out/imxrt-boot.bin"
  ls -l "$out/imxrt-boot.bin"
}

build_app() {
  cargo build --release -j 2 --target "$target" --features "${APP_FEATURES:-wasm,usb,eth}" --bin imxrt-base
  "$objcopy" -O binary "$out/imxrt-base" "$out/imxrt-base.bin"
  cargo run --quiet --release --features image-tool --bin imxrt-image -- \
    "$out/imxrt-base.bin" "$version" "$out/imxrt-base-$version.slot"
}

cmd="${1:-}"
shift || true
args=()
while [[ $# -gt 0 ]]; do
  case "$1" in
    --version) version="${2:?--version needs MAJOR.MINOR}"; shift 2 ;;
    *) args+=("$1"); shift ;;
  esac
done

case "$cmd" in
  boot) build_boot ;;
  app) build_app ;;
  image) build_boot; build_app ;;
  flash)
    images=()
    for a in "${args[@]}"; do
      case "$a" in
        boot) build_boot; images+=("0x0:$out/imxrt-boot.bin") ;;
        a) build_app; images+=("0x400000:$out/imxrt-base-$version.slot") ;;
        b) build_app; images+=("0x800000:$out/imxrt-base-$version.slot") ;;
        *) echo "flash: unknown image $a (boot, a, b)" >&2; exit 2 ;;
      esac
    done
    [[ ${#images[@]} -gt 0 ]] || { echo "flash: name at least one of boot, a, b" >&2; exit 2; }
    python3 -I tools/flash.py "${images[@]}"
    ;;
  *)
    echo "usage: $0 boot | app [--version M.m] | image | flash [boot] [a|b] [--version M.m]" >&2
    exit 2
    ;;
esac
