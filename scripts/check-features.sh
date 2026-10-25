#!/usr/bin/env bash
# Verifies feature isolation (design.md C1, F1-F3):
#   * embedded feature sets build for thumbv6m-none-eabi without std and pull
#     in no cloud dependency;
#   * cloud feature sets never enable an embedded feature;
#   * the host CLI `mwctl` uses only the pure, synchronous core: no
#     middleware feature beyond `default` and no cloud dependency.
set -euo pipefail
cd "$(dirname "$0")/.."

EMBEDDED_TARGET=thumbv6m-none-eabi
CLOUD_CRATES='^(zenoh|tonic|tokio|hyper|h2) '

fail() { echo "FAIL: $*" >&2; exit 1; }

deps() {
    # Normal (non-dev) dependency names for a feature set, one per line.
    cargo tree --edges normal --prefix none --format '{p}' "$@" | sort -u
}

features_of_self() {
    cargo tree --edges features --prefix none -i middleware "$@" \
        | sed -n 's/^middleware feature "\([^"]*\)".*/\1/p' | sort -u
}

echo "== C1: no features, $EMBEDDED_TARGET"
cargo build --quiet --target "$EMBEDDED_TARGET" --no-default-features

for set in "ble" "ble,postcard" "postcard" "serial" "serial,ble,postcard" "espnow" "espnow,postcard"; do
    echo "== F1: [$set] on $EMBEDDED_TARGET"
    cargo build --quiet --target "$EMBEDDED_TARGET" --no-default-features --features "$set"
    if deps --target "$EMBEDDED_TARGET" --no-default-features --features "$set" | grep -Eq "$CLOUD_CRATES"; then
        fail "[$set] pulls in a cloud dependency"
    fi
    if features_of_self --target "$EMBEDDED_TARGET" --no-default-features --features "$set" | grep -qx std; then
        fail "[$set] enables std"
    fi
done

for set in "zenoh" "grpc" "grpc,istio" "zenoh,grpc,istio,prost"; do
    echo "== F2: [$set] on host"
    cargo build --quiet --no-default-features --features "$set"
    if features_of_self --no-default-features --features "$set" | grep -Eqx 'ble|espnow'; then
        fail "[$set] enables an embedded radio feature (ble/espnow)"
    fi
done

echo "== host tool: mwctl uses the pure core only"
cargo build --quiet -p mwctl
if cargo tree --edges normal --prefix none --format '{p}' -p mwctl | sort -u | grep -Eq "$CLOUD_CRATES"; then
    fail "mwctl pulls in a cloud dependency"
fi
extra="$(features_of_self -p mwctl | grep -vx default || true)"
if [ -n "$extra" ]; then
    fail "mwctl enables middleware features: $extra"
fi

echo "OK: feature isolation holds"
