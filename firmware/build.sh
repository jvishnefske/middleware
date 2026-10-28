#!/usr/bin/env bash
# Builds the ESP base firmware (release) for one chip.
#
#   ./build.sh esp32|esp32s3|esp32c3 [extra cargo args...]
#
# ESP32-C3 (RISC-V) builds with the stable toolchain pinned in
# rust-toolchain.toml. ESP32 and ESP32-S3 (Xtensa) need the espup
# toolchain; esp-hal 1.2 requires rustc >= 1.95, so the default is the
# `esp-new` toolchain (override with ESP_TOOLCHAIN=...). The Xtensa GCC
# linker is put on PATH from that toolchain (or ~/export-esp.sh).
#
# Output: target/<triple>/release/esp-base (ELF). Flash with e.g.
#   espflash flash --chip esp32c3 --partition-table partitions.csv \
#       target/riscv32imc-unknown-none-elf/release/esp-base
#
# ESP32 builds accept silicon from v1.0 (ESP_HAL_CONFIG_MIN_CHIP_REVISION,
# default 100 here; esp-hal's own default is 300).
set -euo pipefail
cd "$(dirname "$0")"

chip="${1:-}"
shift || true
case "$chip" in
  esp32c3)
    exec cargo build-esp32c3 "$@"
    ;;
  esp32|esp32s3)
    toolchain="${ESP_TOOLCHAIN:-esp-new}"
    if [[ -f "$HOME/export-esp.sh" ]]; then
      # shellcheck disable=SC1091
      source "$HOME/export-esp.sh"
    fi
    for bin in "$HOME/.rustup/toolchains/$toolchain"/xtensa-esp-elf/*/xtensa-esp-elf/bin; do
      [[ -d "$bin" ]] && PATH="$bin:$PATH"
    done
    export PATH
    if [[ "$chip" == esp32 ]]; then
      # esp-hal refuses ESP32 silicon older than v3.0 by default; boards
      # such as the ESP32-CAM ship v1.x.
      export ESP_HAL_CONFIG_MIN_CHIP_REVISION="${ESP_HAL_CONFIG_MIN_CHIP_REVISION:-100}"
    fi
    exec cargo "+$toolchain" "build-$chip" "$@"
    ;;
  *)
    echo "usage: $0 esp32|esp32s3|esp32c3 [cargo args...]" >&2
    exit 2
    ;;
esac
