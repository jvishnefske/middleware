#!/usr/bin/env bash
# Build the ESP8266 firmware (release). Extra args are passed to cargo.
# LX106_GCC_DIR overrides where xtensa-lx106-elf-gcc (the linker) lives.
set -euo pipefail
cd "$(dirname "$0")"
LX106_GCC_DIR="${LX106_GCC_DIR:-$HOME/.arduino15/packages/esp8266/tools/xtensa-lx106-elf-gcc/3.1.0-gcc10.3-e5f9fec/bin}"
if [[ ! -x "$LX106_GCC_DIR/xtensa-lx106-elf-gcc" ]]; then
  echo "xtensa-lx106-elf-gcc not found in $LX106_GCC_DIR" >&2
  echo "install: arduino-cli core install esp8266:esp8266@3.1.2 --additional-urls https://arduino.esp8266.com/stable/package_esp8266com_index.json" >&2
  exit 1
fi
export PATH="$LX106_GCC_DIR:$PATH"
cargo +esp-new build --release "$@"
xtensa-lx106-elf-size target/xtensa-esp8266-none-elf/release/firmware-esp8266
