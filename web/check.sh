#!/usr/bin/env bash
# Middleware web check: the vitest suite (protocol bindings, upload flow,
# Editor and Blocks compile/simulate tests) and the production build
# (`tsc --noEmit` + `vite build`).
#
# src/pkg (protocol-wasm built by wasm-pack) is gitignored: it is built when
# missing, and rebuilt when REBUILD_WASM=1 (do that after changing the Rust
# protocol, simulator or wasm-host).
#
# Node: uses `node`/`npm` from PATH; NODE_BIN, if set, is prepended.
set -euo pipefail
cd "$(dirname "$0")"
if [[ -n "${NODE_BIN:-}" ]]; then
  export PATH="$NODE_BIN:$PATH"
fi
if [[ ! -d node_modules ]]; then
  npm ci
fi
if [[ "${REBUILD_WASM:-0}" == "1" || ! -f src/pkg/protocol_wasm.js ]]; then
  npm run build:wasm
fi
npm test
npm run build
