#!/usr/bin/env bash
# Requirements E6, E15: builds release firmware for every chip and writes
# merged flash images, board support packages and an ESP Web Tools manifest
# for the web flasher and for Skygraph, and (U19) the rp-base UF2s the Start
# tab offers Pico users.
#
#   ./build-images.sh                  # all: esp32 esp32s3 esp32c3 esp8266 rp2040 rp2350
#   ./build-images.sh esp32c3 esp8266  # only these chips
#   ./build-images.sh rp2040 rp2350    # only the Pico UF2s
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
# rp2040 and rp2350 are built by ../firmware-rp/build.sh (needs picotool).
# Their UF2s are copied to ../web/public/firmware/ and listed under "uf2"
# with their SHA-256, size, version and UF2 family id, which the page
# checks a download against (web/src/flash/manifest.ts documents the
# schema):
#
#   "uf2": { "rp2040": { "file": "rp-base-rp2040.uf2", "sha256": "...", "size": 123904,
#                        "version": "0.1.0", "familyId": 3834380118, "factory": false, "wasm": false },
#            "rp2350": { "file": "rp-base-rp2350-0.1-factory.uf2", ..., "version": "0.1",
#                        "familyId": 3834380119, "factory": true, "wasm": true } }
#
# The RP2040 file is rp-base-rp2040.uf2 (family rp2040). The RP2350 file is
# the factory image (partition table plus slot A, family absolute) built
# with --version RP2350_VERSION (default: the firmware-rp crate's
# major.minor): it is what a BOOTSEL drag installs. Its update image is for
# `mwctl fw update` and is not published here. The RP2040 build has no
# payload runtime ("wasm": false).
#
# Like the .bin images, the .uf2 files are build output and are not
# committed (see .gitignore); a deployment runs this script before
# `npm run build` in web/. Without them the Start tab tells Pico users how
# to build the UF2 themselves. Set CARGO_TARGET_DIR to an absolute path to
# build elsewhere (ESP and RP).
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
declare -A wasm=([esp32]=true [esp32s3]=true [esp32c3]=false [esp8266]=false [rp2040]=false [rp2350]=true)
declare -A family=([esp32]=ESP32 [esp32s3]=ESP32-S3 [esp32c3]=ESP32-C3 [esp8266]=ESP8266)
all_chips=(esp32 esp32s3 esp32c3 esp8266 rp2040 rp2350)

rp_dir="$(cd ../firmware-rp && pwd)"
rp_target="${CARGO_TARGET_DIR:-$rp_dir/target}"
rp_crate_version="$(sed -n 's/^version *= *"\(.*\)"/\1/p' "$rp_dir/Cargo.toml" | head -n1)"
rp2350_version="${RP2350_VERSION:-$(echo "$rp_crate_version" | cut -d. -f1,2)}"
# UF2 family ids: rp2040, and absolute for the RP2350 factory image.
declare -A uf2_family=([rp2040]=$((0xe48bff56)) [rp2350]=$((0xe48bff57)))
declare -A uf2_factory=([rp2040]=false [rp2350]=true)
declare -A uf2_file=([rp2040]=rp-base-rp2040.uf2 [rp2350]="rp-base-rp2350-$rp2350_version-factory.uf2")
declare -A uf2_version=([rp2040]="$rp_crate_version" [rp2350]="$rp2350_version")
declare -A uf2_src=(
  [rp2040]="$rp_target/thumbv6m-none-eabi/release/rp-base-rp2040.uf2"
  [rp2350]="$rp_target/thumbv8m.main-none-eabihf/release/rp-base-rp2350-$rp2350_version-factory.uf2"
)

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

esp_chips=()
rp_chips=()
for chip in "${chips[@]}"; do
  echo "== $chip"
  if [[ "$chip" == rp2040 || "$chip" == rp2350 ]]; then
    rp_chips+=("$chip")
    if [[ "$chip" == rp2350 ]]; then
      "$rp_dir/build.sh" rp2350 --version "$rp2350_version"
    else
      "$rp_dir/build.sh" rp2040
    fi
    cp "${uf2_src[$chip]}" "$out/${uf2_file[$chip]}"
    continue
  fi
  esp_chips+=("$chip")
  if [[ "$chip" == esp8266 ]]; then
    ../firmware-esp8266/build.sh
    ../firmware-esp8266/image.sh "$out/esp8266.bin"
  else
    ./build.sh "$chip"
    espflash save-image --merge --skip-padding --skip-update-check \
      --chip "$chip" --flash-size 4mb --partition-table partitions.csv \
      "${CARGO_TARGET_DIR:-target}/${triple[$chip]}/release/esp-base" "$out/$chip.bin"
  fi
  # Host build of the exporter (stable toolchain from rust-toolchain.toml).
  cargo run -q --example bsp_export -- "$chip" > "$out/$chip.bsp.json"
done

{
  printf '{\n  "name": "esp-base",\n  "version": "%s",\n  "new_install_prompt_erase": false,\n  "builds": [\n' "$version"
  for i in "${!esp_chips[@]}"; do
    chip="${esp_chips[$i]}"
    sep=","
    [[ $i -eq $((${#esp_chips[@]} - 1)) ]] && sep=""
    printf '    { "chipFamily": "%s", "parts": [{ "path": "%s.bin", "offset": 0 }], "wasm": %s, "bsp": "%s.bsp.json" }%s\n' \
      "${family[$chip]}" "$chip" "${wasm[$chip]}" "$chip" "$sep"
  done
  printf '  ],\n  "uf2": {\n'
  for i in "${!rp_chips[@]}"; do
    chip="${rp_chips[$i]}"
    f="$out/${uf2_file[$chip]}"
    sep=","
    [[ $i -eq $((${#rp_chips[@]} - 1)) ]] && sep=""
    printf '    "%s": { "file": "%s", "sha256": "%s", "size": %d, "version": "%s", "familyId": %d, "factory": %s, "wasm": %s }%s\n' \
      "$chip" "${uf2_file[$chip]}" "$(sha256sum "$f" | cut -d' ' -f1)" "$(stat -c %s "$f")" \
      "${uf2_version[$chip]}" "${uf2_family[$chip]}" "${uf2_factory[$chip]}" "${wasm[$chip]}" "$sep"
  done
  printf '  }\n}\n'
} > "$out/manifest.json"

echo "wrote $out/manifest.json:"
cat "$out/manifest.json"
