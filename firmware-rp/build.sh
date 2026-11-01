#!/usr/bin/env bash
# Builds the RP base firmware (release) for one chip and converts it to UF2.
#
#   ./build.sh rp2040|rp2350 [extra cargo args...]
#
# rp2040: Raspberry Pi Pico, thumbv6m-none-eabi, no payload runtime.
# rp2350: Raspberry Pi Pico 2 (Arm cores), thumbv8m.main-none-eabihf.
#
# Output in target/<triple>/release/ (or $CARGO_TARGET_DIR):
#   rp-base-<chip>.elf  for probe-rs (SWD):  probe-rs run --chip RP2040 ...
#   rp-base-<chip>.uf2  for BOOTSEL drag-and-drop or `picotool load -x`
# and the section sizes (flash = .text + .rodata + .data, static RAM =
# .data + .bss, which includes the heap arena on the RP2350).
#
# The UF2 is made with picotool (`uf2 convert`, family rp2040 or
# rp2350-arm-s; the RP2350 image gets an absolute block, the RP2350-E10
# workaround, so it lands at 0x10000000 even on a board that holds a
# partition table). elf2uf2-rs is used for the RP2040 if picotool is
# missing.
set -euo pipefail
cd "$(dirname "$0")"

chip="${1:-}"
shift || true
case "$chip" in
  rp2040)
    target=thumbv6m-none-eabi
    family=rp2040
    extra=()
    ;;
  rp2350)
    target=thumbv8m.main-none-eabihf
    family=rp2350-arm-s
    extra=(--abs-block)
    ;;
  *)
    echo "usage: $0 rp2040|rp2350 [cargo args...]" >&2
    exit 2
    ;;
esac

export CARGO_INCREMENTAL=0
cargo build --release --target "$target" --features "$chip" "$@"

out="${CARGO_TARGET_DIR:-target}/$target/release"
elf="$out/rp-base-$chip.elf"
uf2="$out/rp-base-$chip.uf2"
cp "$out/rp-base" "$elf"

if command -v picotool >/dev/null; then
  picotool uf2 convert --quiet "$elf" -t elf "$uf2" --family "$family" "${extra[@]}"
elif [[ "$chip" == rp2040 ]] && command -v elf2uf2-rs >/dev/null; then
  elf2uf2-rs "$elf" "$uf2"
else
  echo "no UF2 tool: install picotool (or elf2uf2-rs for the RP2040)" >&2
  exit 1
fi

size_tool="$(command -v llvm-size || command -v rust-size || command -v size)"
"$size_tool" -A "$elf" | awk '
  $1 == ".text" || $1 == ".rodata" || $1 == ".vector_table" || $1 == ".boot2" || $1 == ".start_block" || $1 == ".end_block" || $1 == ".bi_entries" { flash += $2 }
  $1 == ".data" { flash += $2; ram += $2 }
  $1 == ".bss" || $1 == ".uninit" { ram += $2 }
  END { printf "flash %d B, static RAM %d B\n", flash, ram }'
ls -l "$elf" "$uf2"
