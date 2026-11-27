#!/usr/bin/env bash
# Builds wasm-host with each interpreter backend for every architecture the
# middleware's platforms use, and checks the outcome against what is
# expected: both backends everywhere, except wasmi on the two targets
# without CAS atomics (it needs `alloc::sync::Arc`), where only `micro`
# builds.
#
#   scripts/check-wasm-backends.sh            every target
#   scripts/check-wasm-backends.sh --no-xtensa   skip ESP32 / ESP32-S3
#
# The Xtensa targets need the espup toolchain (ESP_TOOLCHAIN, default
# `esp-new`), as firmware/build.sh does. Exits non-zero on any surprise.
set -uo pipefail
cd "$(dirname "$0")/.."

xtensa=1
[[ "${1:-}" == "--no-xtensa" ]] && xtensa=0
esp="${ESP_TOOLCHAIN:-esp-new}"

# target | platforms | toolchain args | wasmi expected to build
rows=(
  "x86_64-unknown-linux-gnu|posix-node, tools|+stable|yes"
  "aarch64-unknown-linux-gnu|KD240, Raspberry Pi|+stable|yes"
  "wasm32-unknown-unknown|browser simulator|+stable|yes"
  "thumbv8m.main-none-eabihf|RP2350|+stable|yes"
  "thumbv7em-none-eabihf|i.MX RT1050, nRF52833|+stable|yes"
  "thumbv6m-none-eabi|RP2040|+stable|no"
  "riscv32imc-unknown-none-elf|ESP32-C3|+stable|no"
)
if (( xtensa )); then
  rows+=(
    "xtensa-esp32-none-elf|ESP32|+$esp -Zbuild-std=core,alloc|yes"
    "xtensa-esp32s3-none-elf|ESP32-S3|+$esp -Zbuild-std=core,alloc|yes"
  )
fi

bad=0
printf '%-30s %-22s %-6s %-6s %s\n' target platforms wasmi micro micro-float
for row in "${rows[@]}"; do
  IFS='|' read -r target platforms tool wasmi_ok <<<"$row"
  read -r -a tool <<<"$tool"
  cells=()
  for backend in wasmi micro micro-float; do
    features=(--no-default-features --features "$backend")
    if cargo "${tool[0]}" build --quiet -p wasm-host --target "$target" "${tool[@]:1}" "${features[@]}" >/dev/null 2>&1; then
      got=yes
    else
      got=no
    fi
    want=yes
    [[ "$backend" == wasmi ]] && want="$wasmi_ok"
    if [[ "$got" == "$want" ]]; then
      cells+=("$got")
    else
      cells+=("$got!")
      bad=1
    fi
  done
  printf '%-30s %-22s %-6s %-6s %s\n' "$target" "$platforms" "${cells[@]}"
done
if (( bad )); then
  echo "wasm backends: an entry marked ! is not what is expected" >&2
  exit 1
fi
echo "wasm backends: as expected"
