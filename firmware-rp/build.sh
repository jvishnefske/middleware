#!/usr/bin/env bash
# Builds the RP base firmware (release) for one chip and converts it to UF2.
#
#   ./build.sh rp2040 [extra cargo args...]
#   ./build.sh rp2350 [--version MAJOR.MINOR] [extra cargo args...]
#
# rp2040: Raspberry Pi Pico, thumbv6m-none-eabi, no payload runtime.
#   target/thumbv6m-none-eabi/release/ (or $CARGO_TARGET_DIR):
#     rp-base-rp2040.elf  for probe-rs (SWD)
#     rp-base-rp2040.uf2  for BOOTSEL drag-and-drop or `picotool load -x`
#
# rp2350: Raspberry Pi Pico 2 (Arm cores), thumbv8m.main-none-eabihf, with
#   A/B firmware OTA. The image's IMAGE_DEF version is --version (exported
#   as RP_BASE_FW_VERSION for build.rs), else the crate's major.minor.
#   target/thumbv8m.main-none-eabihf/release/ (or $CARGO_TARGET_DIR):
#     rp-base-rp2350.elf                  for probe-rs (SWD)
#     rp-base-rp2350-V-update.bin / .uf2  for `mwctl fw update` (TBYB image)
#     rp-base-rp2350-V-factory.uf2        for the first BOOTSEL flashing:
#                                         partition table, the image bought
#                                         in slot A (design.md "Factory
#                                         flashing")
#     rp-base-rp2350-V-factory.bin        the same flash contents from 0
#   picotool converts the ELF to a UF2 (family rp2350-arm-s); the host tool
#   rp-image (src/bin/rp-image.rs, feature image-tool) checks it as mwctl
#   does and writes the four files. The byte layouts are rp_base::ota's.
#
# Both print the section sizes (flash = .text + .rodata + .data and the
# boot metadata, static RAM = .data + .bss, which includes the heap arena
# on the RP2350).
set -euo pipefail
cd "$(dirname "$0")"

chip="${1:-}"
shift || true
case "$chip" in
  rp2040)
    target=thumbv6m-none-eabi
    ;;
  rp2350)
    target=thumbv8m.main-none-eabihf
    if [[ "${1:-}" == "--version" ]]; then
      export RP_BASE_FW_VERSION="${2:?--version needs MAJOR.MINOR}"
      shift 2
    fi
    ;;
  *)
    echo "usage: $0 rp2040 [cargo args...] | $0 rp2350 [--version MAJOR.MINOR] [cargo args...]" >&2
    exit 2
    ;;
esac

export CARGO_INCREMENTAL=0
cargo build --release --target "$target" --features "$chip" "$@"

out="${CARGO_TARGET_DIR:-target}/$target/release"
elf="$out/rp-base-$chip.elf"
cp "$out/rp-base" "$elf"

if [[ "$chip" == rp2040 ]]; then
  uf2="$out/rp-base-rp2040.uf2"
  if command -v picotool >/dev/null; then
    picotool uf2 convert --quiet "$elf" -t elf "$uf2" --family rp2040
  elif command -v elf2uf2-rs >/dev/null; then
    elf2uf2-rs "$elf" "$uf2"
  else
    echo "no UF2 tool: install picotool (or elf2uf2-rs)" >&2
    exit 1
  fi
  ls -l "$uf2"
else
  command -v picotool >/dev/null || { echo "picotool is needed for the RP2350 build" >&2; exit 1; }
  linked="$out/rp-base-rp2350-linked.uf2"
  picotool uf2 convert --quiet "$elf" -t elf "$linked" --family rp2350-arm-s
  cargo run --quiet --release --features image-tool --bin rp-image -- "$linked" "$out"
  rm -f "$linked"
fi

size_tool="$(command -v llvm-size || command -v rust-size || command -v size)"
"$size_tool" -A "$elf" | awk '
  $1 == ".text" || $1 == ".rodata" || $1 == ".vector_table" || $1 == ".boot2" || $1 == ".start_block" || $1 == ".sentinel_block" || $1 == ".bi_entries" { flash += $2 }
  $1 == ".data" { flash += $2; ram += $2 }
  $1 == ".bss" || $1 == ".uninit" { ram += $2 }
  END { printf "flash %d B, static RAM %d B\n", flash, ram }'
ls -l "$elf"
