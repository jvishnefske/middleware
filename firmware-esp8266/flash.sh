#!/usr/bin/env bash
# Write the release firmware to the board: builds the merged image with
# image.sh, then writes it at offset 0x0. Run ./build.sh first.
#
# PORT (default /dev/ttyUSB0) and ESPTOOL override the port and esptool.
# The payload region (0x100000..0x140000) is not touched, so an installed
# payload survives reflashing the firmware.
set -euo pipefail
cd "$(dirname "$0")"
ESPTOOL="${ESPTOOL:-$HOME/.arduino15/packages/esp32/tools/esptool_py/5.3.0/esptool}"
PORT="${PORT:-/dev/ttyUSB0}"
MERGED=target/xtensa-esp8266-none-elf/release/firmware-merged.bin
ESPTOOL="$ESPTOOL" ./image.sh "$MERGED"
"$ESPTOOL" --chip esp8266 --port "$PORT" --baud 460800 write-flash 0x0 "$MERGED"
