#!/usr/bin/env bash
# Builds the Rust examples (and mw-jpeg's fuel_probe payload), runs the
# host unit tests (including mw-jpeg's), compiles the
# AssemblyScript templates (if node is available) and validates every payload
# against middleware::wasm_abi. The AssemblyScript step runs before the
# abi-check tests so they can validate its output too.
set -euo pipefail
cd "$(dirname "$0")/.."
HOST=x86_64-unknown-linux-gnu

EXAMPLES=(blink echo button spi_loopback uart_echo dimmer cam_stream cam_view cam_jpeg_stream cam_jpeg_view bench)
TEMPLATES=(blink echo button timer)   # .wat
AS_TEMPLATES=(blink echo button spi)  # .ts

cargo build --release
# mw-jpeg's fuel_probe example is a payload too (see jpeg/src/lib.rs).
cargo build --release --example fuel_probe -p mw-jpeg
# blink, echo and cam_view (the conagotchi badge's camera viewer) again
# with 1-byte pages (16 KiB fixed memory), for hosts that cap linear memory
# below one 64 KiB page, and cam_jpeg_view (the badge's JPEG viewer) with
# the memory it needs. check-reproducible.sh runs
# build-byte-pages.sh here and in a copy at another path, and fails unless
# both builds are byte-identical.
BYTE_PAGE_EXAMPLES=(blink echo cam_view)
scripts/check-reproducible.sh "${BYTE_PAGE_EXAMPLES[@]}"
# cam_jpeg_view's deploy build (the badge): built alone so
# build-byte-pages.sh takes memory and stack from its manifest's
# [package.metadata.byte-pages] (memory sized to data + .bss + stack).
scripts/check-reproducible.sh cam_jpeg_view
# bench (interpreter speed on a board): its deploy build likewise, memory
# and stack from its manifest.
scripts/check-reproducible.sh bench
cargo test -p mw-payload -p button -p spi_loopback -p uart_echo -p dimmer -p cam_stream -p cam_view -p cam_jpeg_stream -p cam_jpeg_view -p bench -p gc9a01 --target "$HOST"
cargo test -p mw-jpeg --target "$HOST"
# Without `half` (as cam_jpeg_view builds it): Scale::Half is refused.
cargo test -p mw-jpeg --no-default-features --target "$HOST"
cargo clippy --release -- -D warnings
cargo clippy -p mw-payload -p abi-check --target "$HOST" --all-targets -- -D warnings
cargo clippy -p mw-jpeg --release --examples -- -D warnings
cargo clippy -p mw-jpeg --target "$HOST" --all-targets -- -D warnings
cargo clippy -p mw-jpeg --no-default-features --target "$HOST" --all-targets -- -D warnings
# The examples are no_std cdylibs: on the host only their tests build.
cargo clippy -p button -p spi_loopback -p uart_echo -p dimmer -p cam_stream -p cam_view -p cam_jpeg_stream -p cam_jpeg_view -p bench --target "$HOST" --tests -- -D warnings
cargo clippy -p gc9a01 --target "$HOST" --all-targets -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc -p gc9a01 -p cam_jpeg_view -p bench --no-deps --document-private-items --target "$HOST"
RUSTDOCFLAGS="-D warnings" cargo doc -p mw-payload --no-deps
RUSTDOCFLAGS="-D warnings" cargo doc -p mw-payload --no-deps --document-private-items --target "$HOST"
RUSTDOCFLAGS="-D warnings" cargo doc -p mw-jpeg --no-deps
RUSTDOCFLAGS="-D warnings" cargo doc -p mw-jpeg --no-deps --document-private-items --target "$HOST"

files=()
for e in "${EXAMPLES[@]}"; do
  files+=("target/wasm32-unknown-unknown/release/$e.wasm")
done
files+=("target/wasm32-unknown-unknown/release/examples/fuel_probe.wasm")
for e in "${BYTE_PAGE_EXAMPLES[@]}" cam_jpeg_view bench; do
  files+=("target/byte-pages/wasm32-unknown-unknown/release/$e.wasm")
done
for t in "${TEMPLATES[@]}"; do
  files+=("templates/$t.wat")
done

if [ -s "$HOME/.nvm/nvm.sh" ]; then
  # shellcheck disable=SC1091
  source "$HOME/.nvm/nvm.sh" >/dev/null
fi
if command -v npx >/dev/null; then
  mkdir -p target/as
  for t in "${AS_TEMPLATES[@]}"; do
    npx -y -p assemblyscript@0.28 asc "templates/$t.ts" \
      --runtime stub --use abort= -O \
      --initialMemory 1 --maximumMemory 1 \
      --outFile "target/as/$t.wasm"
    files+=("target/as/$t.wasm")
  done
else
  echo "npx not found: skipping AssemblyScript templates" >&2
fi

cargo test -p abi-check --target "$HOST"
cargo run -q -p abi-check --target "$HOST" -- "${files[@]}"
