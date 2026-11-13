#!/usr/bin/env bash
# Verifies feature isolation (design.md C1, F1-F3, K13, L11, L15):
#   * the always-built pure core, including `discovery` and `dns::wire`,
#     builds for thumbv6m-none-eabi with no features;
#   * embedded feature sets build for thumbv6m-none-eabi without std, pull
#     in no cloud dependency and enable no host feature;
#   * cloud feature sets (including `dns-sd` and `memory`) never enable an
#     embedded feature;
#   * the host CLI `mwctl` uses only the pure, synchronous core: no
#     middleware feature beyond `default` and no cloud dependency;
#   * the mesh service runner `mw-service` (design.md M5) enables exactly
#     the host features grpc, istio, dns-sd and zenoh of middleware, none
#     embedded, and its runner dependencies (tracing, OpenTelemetry, YAML)
#     appear in no embedded set;
#   * the MCU gateway `mw-gateway` and the robot app `mw-robot` (design.md
#     V19) are host only: no embedded set depends on them, the robot enables
#     no embedded feature, and the gateway enables only the serial link
#     codec (`serial`, the wire it speaks) beside its host features.
set -euo pipefail
cd "$(dirname "$0")/.."

EMBEDDED_TARGET=thumbv6m-none-eabi
CLOUD_CRATES='^(zenoh|tonic|tokio|hyper|h2|mw-service|mw-gateway|mw-robot|tracing|tracing-subscriber|tracing-opentelemetry|opentelemetry|opentelemetry_sdk|opentelemetry-otlp|serde_norway) '
EMBEDDED_FEATURES='ble|espnow|serial|trust|cam-fixtures'
HOST_FEATURES='std|zenoh|grpc|istio|mdns|dns-sd|memory'

fail() { echo "FAIL: $*" >&2; exit 1; }

deps() {
    # Normal (non-dev) dependency names for a feature set, one per line.
    cargo tree --edges normal --prefix none --format '{p}' "$@" | sort -u
}

features_of_self() {
    # Normal edges only: the crate's self dev-dependency (which turns on
    # `memory` for its own tests) is not part of any build that ships.
    cargo tree --edges features,normal --prefix none -i middleware "$@" \
        | sed -n 's/^middleware feature "\([^"]*\)".*/\1/p' | sort -u
}

echo "== C1: no features, $EMBEDDED_TARGET"
cargo build --quiet --target "$EMBEDDED_TARGET" --no-default-features

# The discovery core and the DNS codec are part of that build: their module
# declarations carry no cfg gate (K13, L11).
ungated() { # file, declaration
    grep -B1 -x "$2" "$1" | head -1 | grep -q '^#\[cfg' && return 1
    grep -qx "$2" "$1"
}
for decl in "src/lib.rs:pub mod discovery;" "src/lib.rs:pub mod dns;" "src/dns.rs:pub mod wire;" \
            "src/discovery.rs:pub mod record;" "src/discovery.rs:pub mod directory;" "src/discovery.rs:pub mod beacon;"; do
    ungated "${decl%%:*}" "${decl#*:}" || fail "${decl#*:} in ${decl%%:*} is missing or cfg-gated"
done

for set in "ble" "ble,postcard" "postcard" "serial" "serial,ble,postcard" "espnow" "espnow,postcard" "espnow,cam-fixtures" "trust" "trust,serial"; do
    echo "== F1: [$set] on $EMBEDDED_TARGET"
    cargo build --quiet --target "$EMBEDDED_TARGET" --no-default-features --features "$set"
    if deps --target "$EMBEDDED_TARGET" --no-default-features --features "$set" | grep -Eq "$CLOUD_CRATES"; then
        fail "[$set] pulls in a cloud dependency"
    fi
    if features_of_self --target "$EMBEDDED_TARGET" --no-default-features --features "$set" | grep -Eqx "$HOST_FEATURES"; then
        fail "[$set] enables std or a host feature ($HOST_FEATURES)"
    fi
done

for set in "zenoh" "grpc" "grpc,istio" "mdns" "dns-sd" "memory" "mdns,dns-sd" "zenoh,grpc,istio,prost,mdns,dns-sd"; do
    echo "== F2: [$set] on host"
    cargo build --quiet --no-default-features --features "$set"
    if features_of_self --no-default-features --features "$set" | grep -Eqx "$EMBEDDED_FEATURES"; then
        fail "[$set] enables an embedded feature ($EMBEDDED_FEATURES)"
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

echo "== M5: mw-service enables host features only"
cargo build --quiet -p mw-service
svc_features="$(features_of_self -p mw-service | sort -u)"
for want in grpc istio dns-sd zenoh; do
    grep -qx "$want" <<<"$svc_features" || fail "mw-service does not enable middleware/$want"
done
if grep -Eqx "$EMBEDDED_FEATURES" <<<"$svc_features"; then
    fail "mw-service enables an embedded feature ($EMBEDDED_FEATURES)"
fi
if cargo tree --edges normal --prefix none --format '{p}' -p middleware --target "$EMBEDDED_TARGET" --no-default-features \
        | grep -Eq '^mw-service '; then
    fail "the embedded core depends on mw-service"
fi

echo "== V19: mw-gateway and mw-robot are host only"
cargo build --quiet -p mw-gateway -p mw-robot
gw_features="$(features_of_self -p mw-gateway | sort -u)"
for want in zenoh serial; do
    grep -qx "$want" <<<"$gw_features" || fail "mw-gateway does not enable middleware/$want"
done
if grep -Eqx 'ble|espnow|trust|cam-fixtures' <<<"$gw_features"; then
    fail "mw-gateway enables an embedded feature other than serial"
fi
robot_features="$(features_of_self -p mw-robot | sort -u)"
if grep -Eqx "$EMBEDDED_FEATURES" <<<"$robot_features"; then
    fail "mw-robot enables an embedded feature ($EMBEDDED_FEATURES)"
fi
if cargo tree --edges normal --prefix none --format '{p}' -p middleware --target "$EMBEDDED_TARGET" --no-default-features \
        | grep -Eq '^mw-(gateway|robot) '; then
    fail "the embedded core depends on mw-gateway or mw-robot"
fi

echo "OK: feature isolation holds"
