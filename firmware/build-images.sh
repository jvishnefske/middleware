#!/usr/bin/env bash
# Requirements E6, E15: builds release firmware for every chip and writes
# merged flash images, board support packages and an ESP Web Tools manifest
# for the web flasher and for Skygraph.
#
#   ./build-images.sh                  # all chips: esp32 esp32s3 esp32c3 esp8266
#   ./build-images.sh esp32c3 esp8266  # only these chips
#
#   OUT=dist ./build-images.sh esp32s3 # write somewhere else (CI)
#
# Writes into $OUT (default ../web/public/firmware):
#
# * <chip>.bin — bootloader + partition table + app merged, flashed at
#   offset 0x0, not padded to the flash size;
# * <chip>.bsp.json — the chip's mw-bsp/1 board support package from
#   `cargo run --example bsp_export -- <chip>` (the same profile the firmware
#   serves on sys/board);
# * manifest.json — an ESP Web Tools manifest with two extra build fields,
#   `wasm` and `bsp`:
#
#   { "name": "esp-base", "version": "0.6.0", "new_install_prompt_erase": false,
#     "builds": [ { "chipFamily": "ESP32-C3",
#                   "parts": [ { "path": "esp32c3.bin", "offset": 0 } ],
#                   "wasm": false, "bsp": "esp32c3.bsp.json" }, ... ] }
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
out="${OUT:-$(cd .. && pwd)/web/public/firmware}"
mkdir -p "$out"
out="$(cd "$out" && pwd)"
version="$(sed -n 's/^version *= *"\(.*\)"/\1/p' Cargo.toml | head -n1)"

declare -A triple=(
  [esp32]=xtensa-esp32-none-elf
  [esp32s3]=xtensa-esp32s3-none-elf
  [esp32c3]=riscv32imc-unknown-none-elf
)
declare -A wasm=([esp32]=true [esp32s3]=true [esp32c3]=false [esp8266]=false)
declare -A family=([esp32]=ESP32 [esp32s3]=ESP32-S3 [esp32c3]=ESP32-C3 [esp8266]=ESP8266)
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
  # Host build of the exporter (stable toolchain from rust-toolchain.toml).
  cargo run -q --example bsp_export -- "$chip" > "$out/$chip.bsp.json"
done

{
  printf '{\n  "name": "esp-base",\n  "version": "%s",\n  "new_install_prompt_erase": false,\n  "builds": [\n' "$version"
  for i in "${!chips[@]}"; do
    chip="${chips[$i]}"
    sep=","
    [[ $i -eq $((${#chips[@]} - 1)) ]] && sep=""
    printf '    { "chipFamily": "%s", "parts": [{ "path": "%s.bin", "offset": 0 }], "wasm": %s, "bsp": "%s.bsp.json" }%s\n' \
      "${family[$chip]}" "$chip" "${wasm[$chip]}" "$chip" "$sep"
  done
  printf '  ]\n}\n'
} > "$out/manifest.json"

echo "wrote $out/manifest.json:"
cat "$out/manifest.json"
