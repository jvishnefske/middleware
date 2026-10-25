#!/usr/bin/env bash
# Convert the release ELF (built by build.sh) into one merged ESP8266 image
# to be written at flash offset 0x0.
#
#   ./image.sh <out.bin>
#
# ESPTOOL overrides the esptool executable.
set -euo pipefail
if [[ $# -ne 1 ]]; then
  echo "usage: $0 <out.bin>" >&2
  exit 2
fi
OUT_BIN="$(realpath -m "$1")"
cd "$(dirname "$0")"
ESPTOOL="${ESPTOOL:-$HOME/.arduino15/packages/esp32/tools/esptool_py/5.3.0/esptool}"
ELF=target/xtensa-esp8266-none-elf/release/firmware-esp8266
WORK=target/xtensa-esp8266-none-elf/release
if [[ ! -f "$ELF" ]]; then
  echo "$ELF not found; run ./build.sh first" >&2
  exit 1
fi
mkdir -p "$(dirname "$OUT_BIN")"
# DIO, not the QIO default: with QIO in the header the ROM loader read
# garbage after the first segment ("csum err") on this board.
# elf2image v1 emits <prefix>0x00000.bin (IRAM/DRAM, loaded by ROM) and
# <prefix>0x10000.bin (IROM, mapped from flash at 0x40210000).
"$ESPTOOL" --chip esp8266 elf2image --version 1 --flash-mode dio --flash-freq 40m --flash-size 4MB \
  -o "$WORK/firmware-" "$ELF"
"$ESPTOOL" --chip esp8266 merge-bin -o "$OUT_BIN" \
  0x00000 "$WORK/firmware-0x00000.bin" \
  0x10000 "$WORK/firmware-0x10000.bin"
