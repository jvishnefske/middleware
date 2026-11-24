#!/usr/bin/env bash
# End to end over the mutual-TLS mesh, runnable in CI (.github/workflows/ci.yml,
# job zenoh-mtls) and on a developer machine with docker and SoftHSM2:
#
#   root of trust   scripts/zenoh-pki.sh: a CA key on a SoftHSM token (mw-pki)
#   router          eclipse/zenoh:1.10.1 in docker, deploy/zenoh/mtls/zenohd.json5
#                   (mTLS, default-deny ACL by client CN)
#   device          mw-posix-node (kd240-ci), with an A/B root on files
#   operator        mwctl (tool-ci) over --zenoh tls/... --zenoh-tls-dir
#
# Checks: the operator reaches the node with its certificate and reads its
# state; a certificate from another CA and no certificate are refused; a root
# is written over the network, rebooted into on trial and confirmed; and a
# root that is not confirmed is rolled back by the next power cycle.
#
# Not covered here (unit tests cover them): a claimed node's signed control
# requests and the cert check on commit (posix-node os::tests,
# mwctl_installs_a_root_on_the_node_and_confirms_the_trial).
set -uo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
W=${1:-$(mktemp -d)}
PORT=${ZENOH_E2E_PORT:-7447}
EP="tls/127.0.0.1:$PORT"
NODE_ID=0a1b2c3d4e5f6071
CONTAINER=mw-zenoh-e2e-router
FAILS=0
NODE_PID=""

say() { printf '\n== %s\n' "$*"; }
pass() { printf 'PASS %s\n' "$*"; }
fail() { printf 'FAIL %s\n' "$*"; FAILS=$((FAILS + 1)); }

cleanup() {
  stop_node
  docker rm -f "$CONTAINER" >/dev/null 2>&1 || true
}

command -v docker >/dev/null || { echo "needs docker" >&2; exit 2; }
command -v openssl >/dev/null || { echo "needs openssl" >&2; exit 2; }

say "build"
cd "$ROOT" || exit 2
cargo build -q -p mw-posix-node -p mw-pki || exit 2
cargo build -q -p mwctl --features zenoh || exit 2
TARGET=${CARGO_TARGET_DIR:-$ROOT/target}/debug
MWCTL=$TARGET/mwctl
NODE_BIN=$TARGET/mw-posix-node
export MW_PKI_BIN=$TARGET/mw-pki

say "root of trust (SoftHSM token, mw-pki)"
scripts/zenoh-pki.sh token "$W/pki" >/dev/null || exit 2
scripts/zenoh-pki.sh ca "$W/pki" >/dev/null || exit 2
scripts/zenoh-pki.sh server "$W/pki" router localhost 127.0.0.1 >/dev/null || exit 2
scripts/zenoh-pki.sh csr "$W/pki" kd240-ci >/dev/null
scripts/zenoh-pki.sh client "$W/pki" kd240-ci kd240-ci >/dev/null || exit 2
scripts/zenoh-pki.sh csr "$W/pki" tool-ci >/dev/null
scripts/zenoh-pki.sh client "$W/pki" tool-ci tool-ci >/dev/null || exit 2
pass "CA on the token; router, kd240-ci and tool-ci certificates issued from CSRs"

# A second, unrelated CA: its tool certificate has the right CN and the wrong root.
mkdir -p "$W/rogue/tool-ci"
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes -days 1 \
  -subj /CN=rogue-ca -keyout "$W/rogue/ca.key" -out "$W/rogue/ca.crt" 2>/dev/null
openssl ecparam -name prime256v1 -genkey -noout -out "$W/rogue/tool-ci/key"
openssl req -new -key "$W/rogue/tool-ci/key" -subj /CN=tool-ci -out "$W/rogue/req.csr"
printf 'extendedKeyUsage=clientAuth\n' > "$W/rogue/ext"
openssl x509 -req -in "$W/rogue/req.csr" -CA "$W/rogue/ca.crt" -CAkey "$W/rogue/ca.key" \
  -CAcreateserial -days 1 -extfile "$W/rogue/ext" -out "$W/rogue/tool-ci/cert" 2>/dev/null
cp "$W/rogue/ca.crt" "$W/rogue/tool-ci/ca"
cp "$W/rogue/tool-ci/key" "$W/rogue/tool-ci/key.tmp" && mv "$W/rogue/tool-ci/key.tmp" "$W/rogue/tool-ci/key"

say "the router (docker, mTLS)"
docker rm -f "$CONTAINER" >/dev/null 2>&1 || true
docker run -d --name "$CONTAINER" -p "127.0.0.1:$PORT:7447" \
  -v "$ROOT/deploy/zenoh/mtls/zenohd.json5:/etc/mesh/conf/zenohd.json5:ro" \
  -v "$W/pki/router:/etc/mesh/tls:ro" \
  eclipse/zenoh:1.10.1 -c /etc/mesh/conf/zenohd.json5 >/dev/null || { echo "router did not start" >&2; exit 2; }
for _ in $(seq 60); do
  (echo > "/dev/tcp/127.0.0.1/$PORT") 2>/dev/null && break
  sleep 1
done

# The device: an A/B root on files (the card's gap and the two slots).
mkdir -p "$W/node"
python3 "$ROOT/scripts/zenoh-card.py" init "$W/card" "$W/cmdline" "$W/slot-a" "$W/slot-b" || exit 2

start_node() {
  env MW_ZENOH_ROUTER="$EP" MW_ZENOH_TLS_DIR="$W/pki/kd240-ci" \
      MW_POSIX_NODE_NODE_ID="$NODE_ID" MW_POSIX_NODE_STATE="$W/node" \
      MW_POSIX_NODE_OS_SLOTS="$W/slot-a,$W/slot-b" MW_POSIX_NODE_OS_STATE="$W/card" \
      MW_POSIX_NODE_OS_CMDLINE="$W/cmdline" MW_POSIX_NODE_OS_REBOOT=none \
      "$NODE_BIN" >>"$W/node.log" 2>&1 &
  NODE_PID=$!
}
stop_node() {
  if [ -n "$NODE_PID" ]; then kill "$NODE_PID" 2>/dev/null; wait "$NODE_PID" 2>/dev/null; NODE_PID=""; fi
}
op() { # operator call with its certificate
  timeout 120 "$MWCTL" --zenoh "$EP" --zenoh-tls-dir "$W/pki/tool-ci" --node "$NODE_ID" "$@"
}
trap cleanup EXIT

wait_node() {
  for _ in $(seq 60); do
    op info >/dev/null 2>&1 && return 0
    sleep 1
  done
  return 1
}

say "the device answers the operator over mTLS"
start_node
if wait_node; then pass "tool-ci reaches kd240-ci (sys/info)"; else fail "no answer over mTLS"; tail -20 "$W/node.log"; fi

say "the operator's view"
if op info | grep -q '^chip:'; then pass "info: chip"; else fail "info has no chip"; fi
if op os info | grep -q '^running:      a$'; then pass "os info: running a"; else fail "os info"; fi

say "refused: a certificate from another CA, and no certificate"
if timeout 90 "$MWCTL" --zenoh "$EP" --zenoh-tls-dir "$W/rogue/tool-ci" --node "$NODE_ID" info >/dev/null 2>&1; then
  fail "a rogue CA's certificate was accepted"
else
  pass "rogue CA refused"
fi
if timeout 90 "$MWCTL" --zenoh "$EP" --node "$NODE_ID" info >/dev/null 2>&1; then
  fail "a connection without a certificate was accepted"
else
  pass "no certificate refused"
fi

say "remote update: install, trial, confirm"
head -c $((1 << 20)) /dev/urandom > "$W/root-1.img"
if op os update "$W/root-1.img" --version 65537 | grep -q 'written to slot b'; then pass "root written to slot b over mTLS"; else fail "os update"; fi
if op os reboot >/dev/null; then pass "reboot requested"; else fail "os reboot"; fi
stop_node
python3 "$ROOT/scripts/zenoh-card.py" boot "$W/card" "$W/cmdline" >/dev/null
start_node
if wait_node && op os info | grep -q '^state:        trial$'; then pass "trial boot of slot b"; else fail "no trial"; fi
if op os confirm | grep -q confirmed; then pass "confirmed"; else fail "os confirm"; fi
if op os info | grep -q '^running:      b$' && op os info | grep -q '^state:        confirmed$'; then pass "running b, confirmed"; else fail "after confirm"; fi
stop_node
python3 "$ROOT/scripts/zenoh-card.py" boot "$W/card" "$W/cmdline" >/dev/null
start_node
if wait_node && op os info | grep -q '^running:      b$'; then pass "next power cycle boots the confirmed b"; else fail "b did not stay"; fi

say "remote update, not confirmed: rolled back"
head -c $((1 << 20)) /dev/urandom > "$W/root-2.img"
if op os update "$W/root-2.img" --version 65538 | grep -q 'written to slot a'; then pass "root 2 written to slot a"; else fail "os update 2"; fi
op os reboot >/dev/null || fail "os reboot 2"
stop_node
python3 "$ROOT/scripts/zenoh-card.py" boot "$W/card" "$W/cmdline" >/dev/null   # trial of a
start_node
if wait_node && op os info | grep -q '^running:      a$'; then pass "trial of a booted"; else fail "no trial of a"; fi
stop_node
python3 "$ROOT/scripts/zenoh-card.py" boot "$W/card" "$W/cmdline" >/dev/null   # tries used up: b
start_node
if wait_node && op os info | grep -q '^running:      b$' && op os info | grep -q '^state:        rolled back$'; then
  pass "unconfirmed trial rolled back to b"
else
  fail "no rollback"
fi

say "result"
if [ "$FAILS" -eq 0 ]; then
  echo "all checks passed (state in $W)"
else
  echo "$FAILS check(s) failed; node log: $W/node.log"
  tail -30 "$W/node.log" 2>/dev/null
  docker logs "$CONTAINER" 2>&1 | tail -15
fi
exit "$FAILS"
