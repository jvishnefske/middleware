#!/usr/bin/env bash
# Builds payloads with 1-byte wasm pages (the custom-page-sizes proposal) for
# devices that cannot spare a 64 KiB page, e.g. the conagotchi badge.
#
#   scripts/build-byte-pages.sh -p blink -p echo
#
# Arguments are passed to `cargo build --release`. Output goes to
# target/byte-pages/wasm32-unknown-unknown/release/, next to (not over) the
# normal 64 KiB-page build. The linear memory is fixed (initial = maximum)
# at MEMORY_BYTES with a STACK_BYTES shadow stack; both must leave room for
# the payload's static data, or the link fails. They come from, in order:
# the environment; the package's `[package.metadata.byte-pages]` `memory`
# and `stack` when exactly one package is built and its manifest
# (examples/<package>/Cargo.toml) has them (cam_jpeg_view does: its memory
# is sized to its data, .bss and stack); else 16384 and 4096. The host's
# wasm-host `Limits::bytes` cap must be at least MEMORY_BYTES; its heap
# bound charges MEMORY_BYTES, the module's declared maximum.
#
# RUSTFLAGS replaces (not extends) the rustflags of .cargo/config.toml, and
# only for the wasm32 target, so the host build scripts are unaffected.
#
# The build is reproducible: --remap-path-prefix rewrites the absolute paths
# rustc would embed (panic locations, debug info) so the module does not
# depend on where the repository, the cargo registry or the home directory
# are. Later flags win when several match, so the most specific comes last:
# $HOME -> /home, the cargo registry -> /cargo/registry, the middleware
# repository (the payloads' parent, which holds the `middleware` crate the
# SDK depends on) -> /mw. scripts/check-reproducible.sh verifies that two
# builds from different directories are byte-identical.
set -euo pipefail
cd "$(dirname "$0")/.."
# The packages named by -p/--package, to find their byte-page metadata.
packages=()
want=0
for a in "$@"; do
  if [ "$want" = 1 ]; then packages+=("$a"); want=0; continue; fi
  case "$a" in
    -p|--package) want=1 ;;
    --package=*) packages+=("${a#--package=}") ;;
    -p*) packages+=("${a#-p}") ;;
  esac
done
# `key = N` from the [package.metadata.byte-pages] table of a manifest.
metadata() {
  awk -v key="$2" '
    /^\[/ { inside = ($0 == "[package.metadata.byte-pages]") ; next }
    inside && $1 == key && $2 == "=" { print $3; exit }
  ' "$1"
}
if [ ${#packages[@]} -eq 1 ] && [ -f "examples/${packages[0]}/Cargo.toml" ]; then
  manifest="examples/${packages[0]}/Cargo.toml"
  MEMORY_BYTES="${MEMORY_BYTES:-$(metadata "$manifest" memory)}"
  STACK_BYTES="${STACK_BYTES:-$(metadata "$manifest" stack)}"
fi
MEMORY_BYTES="${MEMORY_BYTES:-16384}"
STACK_BYTES="${STACK_BYTES:-4096}"
echo "byte pages: ${packages[*]:-default members}: memory $MEMORY_BYTES, stack $STACK_BYTES" >&2
REPO="$(cd .. && pwd -P)"
CARGO_HOME_DIR="${CARGO_HOME:-$HOME/.cargo}"
RUSTFLAGS="-C link-arg=--page-size=1 \
-C link-arg=-zstack-size=$STACK_BYTES \
-C link-arg=--initial-memory=$MEMORY_BYTES \
-C link-arg=--max-memory=$MEMORY_BYTES \
--remap-path-prefix=$HOME=/home \
--remap-path-prefix=$CARGO_HOME_DIR/registry=/cargo/registry \
--remap-path-prefix=$REPO=/mw" \
  cargo build --release --target-dir target/byte-pages "$@"
