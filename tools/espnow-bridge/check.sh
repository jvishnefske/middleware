#!/usr/bin/env bash
# Checks the ESP8266 USB<->ESP-NOW bridge without touching any device:
#   1. builds and runs the host test of the C++ frame codec
#      (esp8266/test_bridge_frame.cpp, golden frames from mwctl's bridge.rs);
#   2. compiles the sketch with arduino-cli (skipped with a notice when
#      arduino-cli or the esp8266 core is missing).
# Never uploads. Usage: tools/espnow-bridge/check.sh
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
sketch="$here/esp8266"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

echo "== host test: bridge_frame.h"
g++ -std=c++17 -Wall -Wextra -Werror -I"$sketch" "$sketch/test_bridge_frame.cpp" -o "$tmp/test_bridge_frame"
"$tmp/test_bridge_frame"

echo "== arduino-cli compile: esp8266:esp8266:generic"
cli="${ARDUINO_CLI:-}"
if [[ -z "$cli" ]]; then
  if command -v arduino-cli >/dev/null 2>&1; then
    cli="$(command -v arduino-cli)"
  elif [[ -x "$HOME/.local/bin/arduino-cli" ]]; then
    cli="$HOME/.local/bin/arduino-cli"
  fi
fi
if [[ -z "$cli" ]]; then
  echo "notice: arduino-cli not found; skipping the sketch compile"
  exit 0
fi
if ! "$cli" core list 2>/dev/null | grep -q '^esp8266:esp8266'; then
  echo "notice: esp8266 core not installed; skipping the sketch compile"
  exit 0
fi
"$cli" compile --fqbn esp8266:esp8266:generic --warnings all --build-path "$tmp/build" "$sketch"
