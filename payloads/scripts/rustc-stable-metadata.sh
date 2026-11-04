#!/usr/bin/env bash
# RUSTC_WRAPPER for scripts/build-byte-pages.sh: replaces cargo's
# `-C metadata=<hash>` on wasm32 units with a hash of the rustc command
# line in which every checkout-dependent part is normalized, so the crate
# identities (StableCrateId), and with them the symbol hashes, do not
# depend on where the repository is.
#
# Why: cargo folds the absolute path of a path dependency that lies outside
# the workspace root into that dependency's metadata hash (rust-lang/cargo
# #7645, #15122). `middleware` (the repository root) is such a dependency
# of the SDK, so its hash, and through the dependency hashes those of
# mw-payload and every payload, changed with the checkout path. rustc
# orders the items of a codegen unit that have no local definition (generic
# instantiations such as `Result::map_err::<{closure}>`) by symbol name,
# whose hash includes the crate identity, so moving the checkout could swap
# two functions and the string constants they reference (periph_demo's
# "spi_open" / "i2c_open" log tags). --remap-path-prefix cannot help: it
# rewrites embedded paths, not cargo's hash.
#
# The replacement hashes: the arguments without -C metadata,
# -C extra-filename, -L, --out-dir, --remap-path-prefix, the terminal's
# --diagnostic-width / --color, and with --extern reduced to the crate
# name; the repository root replaced by /mw and the cargo registry by
# /cargo/registry; and CARGO_PKG_VERSION. Features (--cfg), crate type,
# edition, source file and codegen options stay in, so distinct units keep
# distinct identities. Host units (build scripts, proc macros: no
# `--target`) and cargo's queries (`-vV`, `--print`) pass through.
#
# Remove once cargo stops hashing those paths (rust-lang/cargo#17624).
#
#   RUSTC_WRAPPER=scripts/rustc-stable-metadata.sh MW_REPO=<repo> cargo build ...
set -euo pipefail
rustc="$1"
shift
repo="${MW_REPO:?MW_REPO: the repository root}"
registry="${CARGO_HOME:-$HOME/.cargo}/registry"

wasm=0
for a in "$@"; do
  case "$a" in
    wasm32-*|--target=wasm32-*) wasm=1 ;;
  esac
done
if [ "$wasm" = 0 ]; then
  exec "$rustc" "$@"
fi

args=()
key=()
skip_next=""
for a in "$@"; do
  if [ -n "$skip_next" ]; then
    case "$skip_next" in
      codegen)
        case "$a" in
          metadata=*) ;;                         # dropped: replaced below
          extra-filename=*) args+=(-C "$a") ;;   # file name only
          *) args+=(-C "$a"); key+=(-C "$a") ;;
        esac ;;
      extern) args+=(--extern "$a"); key+=(--extern "${a%%=*}") ;;
      path) args+=("$prev" "$a") ;;
    esac
    skip_next=""
    continue
  fi
  case "$a" in
    -C) skip_next=codegen ;;
    -Cmetadata=*) ;;
    -Cextra-filename=*) args+=("$a") ;;
    --extern) skip_next=extern ;;
    -L|--out-dir) prev="$a"; skip_next=path ;;
    -L*|--out-dir=*|--remap-path-prefix=*|--diagnostic-width=*|--color=*) args+=("$a") ;;
    *) args+=("$a"); key+=("$a") ;;
  esac
done

normalized="$(printf '%s\n' "${key[@]}" "version=${CARGO_PKG_VERSION:-}" \
  | sed -e "s|$registry|/cargo/registry|g" -e "s|$repo|/mw|g")"
hash="$(printf '%s' "$normalized" | sha256sum | cut -c1-16)"
exec "$rustc" "${args[@]}" -C "metadata=$hash"
