#!/usr/bin/env bash
# Requirement E6: builds release firmware for every chip and writes merged
# flash images for the web flasher.
#
#   ./build-images.sh                  # all chips: esp32 esp32s3 esp32c3 esp8266
#   ./build-images.sh esp32c3 esp8266  # only these chips
#
# Writes ../web/public/firmware/<chip>.bin (bootloader + partition table +
# app merged, flashed at offset 0x0, not padded to the flash size) and
# ../web/public/firmware/manifest.json:
#
#   { "version": "0.1.0",
#     "images": { "esp32c3": { "file": "esp32c3.bin", "offset": 0, "wasm": false }, ... } }
#
# The ESP32 family is built here (./build.sh + espflash save-image). The
# ESP8266 is a separate crate: ../firmware-esp8266/build.sh builds it and
# ../firmware-esp8266/image.sh writes its merged image (also flashed at 0x0).
#
# `wasm` tells the page whether that image includes the WASM payload runtime.
# ESP32-C3 (see Cargo.toml feature `wasm`) and ESP8266 build without it: they
# serve the protocol and store payloads but do not run them.
#
# The manifest lists only the chips built by this run, so a partial build
# writes a partial manifest.
set -euo pipefail
cd "$(dirname "$0")"

# Absolute, because ../firmware-esp8266/image.sh may change directory.
out="$(cd .. && pwd)/web/public/firmware"
mkdir -p "$out"
version="$(sed -n 's/^version *= *"\(.*\)"/\1/p' Cargo.toml | head -n1)"

declare -A triple=(
  [esp32]=xtensa-esp32-none-elf
  [esp32s3]=xtensa-esp32s3-none-elf
  [esp32c3]=riscv32imc-unknown-none-elf
)
declare -A wasm=([esp32]=true [esp32s3]=true [esp32c3]=false [esp8266]=false)
all_chips=(esp32 esp32s3 esp32c3 esp8266)

if [[ $# -gt 0 ]]; then
  chips=("$@")
else
  chips=("${all_chips[@]}")
fi
for chip in "${chips[@]}"; do
  if [[ -z "${wasm[$chip]+set}" ]]; then
    echo "unknown chip '$chip' (expected one of: ${all_chips[*]})" >&2
    exit 2
  fi
done

for chip in "${chips[@]}"; do
  echo "== $chip"
  if [[ "$chip" == esp8266 ]]; then
    ../firmware-esp8266/build.sh
    ../firmware-esp8266/image.sh "$out/esp8266.bin"
  else
    ./build.sh "$chip"
    espflash save-image --merge --skip-padding --skip-update-check \
      --chip "$chip" --flash-size 4mb --partition-table partitions.csv \
      "target/${triple[$chip]}/release/esp-base" "$out/$chip.bin"
  fi
done

{
  printf '{\n  "version": "%s",\n  "images": {\n' "$version"
  for i in "${!chips[@]}"; do
    chip="${chips[$i]}"
    sep=","
    [[ $i -eq $((${#chips[@]} - 1)) ]] && sep=""
    printf '    "%s": { "file": "%s.bin", "offset": 0, "wasm": %s }%s\n' \
      "$chip" "$chip" "${wasm[$chip]}" "$sep"
  done
  printf '  }\n}\n'
} > "$out/manifest.json"

echo "wrote $out/manifest.json:"
cat "$out/manifest.json"
