#!/usr/bin/env bash
# Host unit tests and lints for the pure modules (flash, link, controller).
#
# .cargo/config.toml targets xtensa-esp8266-none-elf with build-std, which
# needs the esp-new toolchain. The library is `no_std` except under
# `cfg(test)` and its ESP8266-only dependencies are gated on
# `target_arch = "xtensa"`, so it also builds for the host: use the stable
# toolchain (which ignores the `[unstable]` build-std table) and name the
# host target explicitly. `--lib` skips the firmware binary, which only
# builds for the chip.
set -euo pipefail
cd "$(dirname "$0")"
HOST="${HOST_TARGET:-x86_64-unknown-linux-gnu}"
cargo +stable test --lib --target "$HOST" "$@"
cargo +stable clippy --lib --tests --target "$HOST" -- -D warnings
# Lint the firmware binary as built for the chip (needs the esp-new
# toolchain; clippy does not link, so the lx106 GCC is not required).
cargo +esp-new clippy --release -- -D warnings
