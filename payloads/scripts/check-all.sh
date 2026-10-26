#!/usr/bin/env bash
# Builds the Rust examples, runs the host unit tests, compiles the
# AssemblyScript templates (if node is available) and validates every payload
# against middleware::wasm_abi. The AssemblyScript step runs before the
# abi-check tests so they can validate its output too.
set -euo pipefail
cd "$(dirname "$0")/.."
HOST=x86_64-unknown-linux-gnu

EXAMPLES=(blink echo button spi_loopback uart_echo dimmer)
TEMPLATES=(blink echo button timer)   # .wat
AS_TEMPLATES=(blink echo button spi)  # .ts

cargo build --release
# blink and echo again with 1-byte pages (16 KiB fixed memory), for hosts
# that cap linear memory below one 64 KiB page. check-reproducible.sh runs
# build-byte-pages.sh here and in a copy at another path, and fails unless
# both builds are byte-identical.
BYTE_PAGE_EXAMPLES=(blink echo)
scripts/check-reproducible.sh "${BYTE_PAGE_EXAMPLES[@]}"
cargo test -p mw-payload -p button -p spi_loopback -p uart_echo -p dimmer --target "$HOST"
cargo clippy --release -- -D warnings
cargo clippy -p mw-payload -p abi-check --target "$HOST" --all-targets -- -D warnings
# The examples are no_std cdylibs: on the host only their tests build.
cargo clippy -p button -p spi_loopback -p uart_echo -p dimmer --target "$HOST" --tests -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc -p mw-payload --no-deps
RUSTDOCFLAGS="-D warnings" cargo doc -p mw-payload --no-deps --document-private-items --target "$HOST"

files=()
for e in "${EXAMPLES[@]}"; do
  files+=("target/wasm32-unknown-unknown/release/$e.wasm")
done
for e in "${BYTE_PAGE_EXAMPLES[@]}"; do
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
