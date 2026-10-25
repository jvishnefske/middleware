#!/usr/bin/env bash
# Builds payloads with 1-byte wasm pages (the custom-page-sizes proposal) for
# devices that cannot spare a 64 KiB page, e.g. the conagotchi badge.
#
#   scripts/build-byte-pages.sh -p blink -p echo
#
# Arguments are passed to `cargo build --release`. Output goes to
# target/byte-pages/wasm32-unknown-unknown/release/, next to (not over) the
# normal 64 KiB-page build. The linear memory is fixed (initial = maximum)
# at MEMORY_BYTES (default 16384) with a STACK_BYTES (default 4096) shadow
# stack; both must leave room for the payload's static data, or the link
# fails. The host's wasm-host `Limits::bytes` cap must be at least
# MEMORY_BYTES.
#
# RUSTFLAGS replaces (not extends) the rustflags of .cargo/config.toml, and
# only for the wasm32 target, so the host build scripts are unaffected.
set -euo pipefail
cd "$(dirname "$0")/.."
MEMORY_BYTES="${MEMORY_BYTES:-16384}"
STACK_BYTES="${STACK_BYTES:-4096}"
RUSTFLAGS="-C link-arg=--page-size=1 \
-C link-arg=-zstack-size=$STACK_BYTES \
-C link-arg=--initial-memory=$MEMORY_BYTES \
-C link-arg=--max-memory=$MEMORY_BYTES" \
  cargo build --release --target-dir target/byte-pages "$@"
