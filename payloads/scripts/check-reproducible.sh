#!/usr/bin/env bash
# Checks that scripts/build-byte-pages.sh is reproducible: builds the given
# packages (default: blink and echo) in this checkout and in a copy of the
# sources under a different absolute path, and fails unless the .wasm files
# are byte-identical.
#
#   scripts/check-reproducible.sh [package...]
#
# The copy holds only what the payload build reads: the `middleware` crate
# (Cargo.toml, src/) and payloads/ without target/. It gets its own target
# directory, so nothing is shared between the two builds but the cargo
# registry and the toolchain.
set -euo pipefail
cd "$(dirname "$0")/.."
PACKAGES=("$@")
[ ${#PACKAGES[@]} -gt 0 ] || PACKAGES=(blink echo)
OUT=target/byte-pages/wasm32-unknown-unknown/release

copy="$(mktemp -d "${TMPDIR:-/tmp}/mw-repro.XXXXXX")"
trap 'rm -rf "$copy"' EXIT
mkdir -p "$copy/elsewhere/payloads"
cp -R ../Cargo.toml ../src "$copy/elsewhere/"
tar -C . --exclude=./target -cf - . | tar -C "$copy/elsewhere/payloads" -xf -

scripts/build-byte-pages.sh "${PACKAGES[@]/#/--package=}"
"$copy/elsewhere/payloads/scripts/build-byte-pages.sh" "${PACKAGES[@]/#/--package=}"

status=0
for p in "${PACKAGES[@]}"; do
  if cmp -s "$OUT/$p.wasm" "$copy/elsewhere/payloads/$OUT/$p.wasm"; then
    echo "reproducible: $p.wasm"
  else
    echo "NOT reproducible: $p.wasm differs between $(pwd -P) and $copy/elsewhere" >&2
    status=1
  fi
done
exit $status
