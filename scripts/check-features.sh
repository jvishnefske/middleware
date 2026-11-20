#!/usr/bin/env bash
# Verifies feature isolation (design.md C1, F1-F3, K13, L11, L15, Y11, Q11,
# CL3, FL4, MS5, RG19, PN9, D19, D26):
#   * the always-built pure core, including `discovery`, `dns::wire`,
#     `zenoh_wire` and `fw_ota`, builds for thumbv6m-none-eabi with no
#     features, and so do the `recipe-core` and `state-table` crates (Q11),
#     and `wasm-hdl-ref` (CL3);
#   * the RP2350 set builds for thumbv8m.main-none-eabihf (Cortex-M33, the
#     Pico 2): the pure core, `fw_ota::service` (`trust`) and the serial
#     link, with no cloud dependency and no host feature;
#   * `fw-sim` (the bootrom model, host only) builds on the host;
#   * the board Zenoh client `zenoh-lite` builds for the i.MX RT target
#     thumbv7em-none-eabihf (and thumbv6m) without std, alloc or a cloud
#     dependency;
#   * embedded feature sets build for thumbv6m-none-eabi without std, pull
#     in no cloud dependency and enable no host feature;
#   * the mDNS core `mdns-core` (D19), with the shared `mw/node` record
#     (D41), builds for thumbv6m and thumbv7em, is
#     gated on `mdns-core` rather than the std feature `mdns`, and is
#     neither embedded nor host (`mdns` enables it); its embassy seam
#     `mdns-embassy` (D26, embedded) builds for thumbv7em and thumbv8m
#     (it needs atomics, so not thumbv6m);
#   * cloud feature sets (including `dns-sd`, `memory` and `udp`) never enable an
#     embedded feature;
#   * the host CLI `mwctl` uses only the pure, synchronous core: no
#     middleware feature beyond `default` and no cloud dependency;
#   * the mesh service runner `mw-service` (design.md MS5) enables exactly
#     the host features grpc, istio, dns-sd and zenoh of middleware, none
#     embedded, and its runner dependencies (tracing, OpenTelemetry, YAML)
#     appear in no embedded set;
#   * the MCU gateway `mw-gateway` and the robot app `mw-robot` (design.md
#     RG19) are host only: no embedded set depends on them, the robot enables
#     no embedded feature, and the gateway enables only the serial link
#     codec (`serial`, the wire it speaks) beside its host features;
#   * the Linux payload host `mw-posix-node` (design.md PN9) is host only: no
#     embedded set depends on it, and of the embedded features it enables
#     only what the device side needs (`serial`, the link it serves;
#     `trust`, signed commits; `espnow`, its ESP-NOW gateway's radio over the
#     USB bridge (EG12), which esp-base's pure core also builds) beside
#     `zenoh`;
#   * the bridge framing `radio_bridge` is ungated (EG1), so the no-feature
#     build below and mwctl's feature-free build both compile it.
set -euo pipefail
cd "$(dirname "$0")/.."

EMBEDDED_TARGET=thumbv6m-none-eabi
RP2350_TARGET=thumbv8m.main-none-eabihf
IMXRT_TARGET=thumbv7em-none-eabihf
CLOUD_CRATES='^(zenoh|tonic|tokio|hyper|h2|mw-service|mw-gateway|mw-robot|mw-posix-node|tracing|tracing-subscriber|tracing-opentelemetry|opentelemetry|opentelemetry_sdk|opentelemetry-otlp|serde_norway) '
EMBEDDED_FEATURES='ble|espnow|serial|trust|cam-fixtures|zenoh-lite|mdns-embassy'
HOST_FEATURES='std|zenoh|grpc|istio|mdns|dns-sd|memory|fw-sim|udp'

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

# The S88 batch-control core (design.md Q11) and the state tables it shares
# identifiers with are no_std, no-heap crates for the same target.
echo "== Q11: recipe-core and state-table, $EMBEDDED_TARGET"
cargo build --quiet --target "$EMBEDDED_TARGET" -p recipe-core -p state-table

echo "== CL3: wasm-hdl-ref (the gateware WASM core's model) is no_std, $EMBEDDED_TARGET"
cargo build --quiet --target "$EMBEDDED_TARGET" -p wasm-hdl-ref

# The discovery core and the DNS codec are part of that build: their module
# declarations carry no cfg gate (K13, L11).
ungated() { # file, declaration
    grep -B1 -x "$2" "$1" | head -1 | grep -q '^#\[cfg' && return 1
    grep -qx "$2" "$1"
}
for decl in "src/lib.rs:pub mod discovery;" "src/lib.rs:pub mod dns;" "src/dns.rs:pub mod wire;" \
            "src/lib.rs:pub mod zenoh_wire;" "src/lib.rs:pub mod clock;" "src/lib.rs:pub mod datagram;" \
            "src/lib.rs:pub mod radio_bridge;" \
            "src/lib.rs:pub mod fw_ota;" "src/fw_ota.rs:pub mod machine;" "src/fw_ota.rs:pub mod wire;" \
            "src/fw_ota.rs:pub mod picobin;" "src/fw_ota.rs:pub mod platform;" "src/fw_ota.rs:pub mod record;" \
            "src/discovery.rs:pub mod record;" "src/discovery.rs:pub mod directory;" "src/discovery.rs:pub mod beacon;"; do
    ungated "${decl%%:*}" "${decl#*:}" || fail "${decl#*:} in ${decl%%:*} is missing or cfg-gated"
done

# The mDNS core (D19): the pure table and origin policy, MdnsError and
# the generic shells are gated on `mdns-core`, never on the std feature
# `mdns` (which only adds the tokio seam and builders: the `host` modules).
echo "== D19: the mDNS core is gated on mdns-core"
for f in src/mdns/origin.rs src/mdns/announce/table.rs src/mdns/error.rs src/mdns/race.rs \
         src/mdns/announce.rs src/mdns/socket.rs src/mdns/node.rs; do
    # Every mention of the feature is `#[cfg(feature = "mdns")]` on the
    # line before `mod host;` or `pub use host::...`.
    odd="$(grep -n 'feature = "mdns"' "$f" | grep -vF '#[cfg(feature = "mdns")]' || true)"
    [ -z "$odd" ] || fail "$f uses the std feature mdns outside a host module gate: $odd"
    bad="$(grep -n -A1 -F '#[cfg(feature = "mdns")]' "$f" | grep -vF 'feature = "mdns"' \
        | grep -v -e '^--$' -e '^[0-9]*-mod host;$' -e '^[0-9]*-pub use host::' || true)"
    [ -z "$bad" ] || fail "$f gates more than its host module on the std feature mdns: $bad"
done
for m in announce origin socket error race node; do
    grep -B1 -E "^(pub )?mod $m;" src/mdns.rs | head -1 | grep -qx '#\[cfg(feature = "mdns-core")\]' \
        || fail "mod $m in src/mdns.rs is not gated on mdns-core"
done

for set in "ble" "ble,postcard" "postcard" "serial" "serial,ble,postcard" "espnow" "espnow,postcard" "espnow,cam-fixtures" "trust" "trust,serial" "zenoh-lite" "zenoh-lite,serial,trust" "mdns-core" "mdns-core,espnow"; do
    echo "== F1: [$set] on $EMBEDDED_TARGET"
    cargo build --quiet --target "$EMBEDDED_TARGET" --no-default-features --features "$set"
    if deps --target "$EMBEDDED_TARGET" --no-default-features --features "$set" | grep -Eq "$CLOUD_CRATES"; then
        fail "[$set] pulls in a cloud dependency"
    fi
    if features_of_self --target "$EMBEDDED_TARGET" --no-default-features --features "$set" | grep -Eqx "$HOST_FEATURES"; then
        fail "[$set] enables std or a host feature ($HOST_FEATURES)"
    fi
done

for set in "" "trust" "trust,serial" "trust,serial,postcard" "mdns-core,mdns-embassy"; do
    echo "== Y11: [${set:-no features}] on $RP2350_TARGET"
    cargo build --quiet --target "$RP2350_TARGET" --no-default-features ${set:+--features "$set"}
    if deps --target "$RP2350_TARGET" --no-default-features ${set:+--features "$set"} | grep -Eq "$CLOUD_CRATES"; then
        fail "[$set] on $RP2350_TARGET pulls in a cloud dependency"
    fi
    if features_of_self --target "$RP2350_TARGET" --no-default-features ${set:+--features "$set"} | grep -Eqx "$HOST_FEATURES"; then
        fail "[$set] on $RP2350_TARGET enables std or a host feature ($HOST_FEATURES)"
    fi
done

# The i.MX RT base firmware's set: the Zenoh client with the serial link
# and signed updates, no heap (FL4, ZN1-ZN3: the codec and session are
# pure and `alloc` is never linked).
for set in "zenoh-lite" "zenoh-lite,serial,trust,postcard" "mdns-core" "zenoh-lite,mdns-embassy"; do
    echo "== FL4: [$set] on $IMXRT_TARGET"
    cargo build --quiet --target "$IMXRT_TARGET" --no-default-features --features "$set"
    if deps --target "$IMXRT_TARGET" --no-default-features --features "$set" | grep -Eq "$CLOUD_CRATES"; then
        fail "[$set] on $IMXRT_TARGET pulls in a cloud dependency"
    fi
    if features_of_self --target "$IMXRT_TARGET" --no-default-features --features "$set" | grep -Eqx "$HOST_FEATURES"; then
        fail "[$set] on $IMXRT_TARGET enables std or a host feature ($HOST_FEATURES)"
    fi
done

echo "== Y11: [fw-sim] on host"
cargo build --quiet --no-default-features --features fw-sim

for set in "zenoh" "grpc" "grpc,istio" "mdns" "dns-sd" "memory" "udp" "mdns,dns-sd" "zenoh,grpc,istio,prost,mdns,dns-sd"; do
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

echo "== MS5: mw-service enables host features only"
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

echo "== RG19: mw-gateway and mw-robot are host only"
cargo build --quiet -p mw-gateway -p mw-robot
gw_features="$(features_of_self -p mw-gateway | sort -u)"
for want in zenoh serial; do
    grep -qx "$want" <<<"$gw_features" || fail "mw-gateway does not enable middleware/$want"
done
if grep -Eqx 'ble|espnow|trust|cam-fixtures|zenoh-lite' <<<"$gw_features"; then
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

echo "== PN9: mw-posix-node is host only"
cargo build --quiet -p mw-posix-node
pn_features="$(features_of_self -p mw-posix-node | sort -u)"
for want in zenoh serial trust espnow mdns; do
    grep -qx "$want" <<<"$pn_features" || fail "mw-posix-node does not enable middleware/$want"
done
if grep -Eqx 'ble|cam-fixtures|zenoh-lite' <<<"$pn_features"; then
    fail "mw-posix-node enables an embedded feature other than serial, trust and espnow"
fi
if cargo tree --edges normal --prefix none --format '{p}' -p middleware --target "$EMBEDDED_TARGET" --no-default-features \
        | grep -Eq '^mw-posix-node '; then
    fail "the embedded core depends on mw-posix-node"
fi

echo "OK: feature isolation holds"
