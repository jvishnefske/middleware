#!/usr/bin/env bash
# The end-to-end demo (docs/demo.md): a root of trust, an mTLS Zenoh hub, an
# OAuth login, and a QEMU client that is provisioned over the network and then
# talks to the hub with its own certificate.
#
#   scripts/demo/demo.sh [all|up|provision|verify|down|status]      (just demo)
#
#   root of trust  scripts/zenoh-pki.sh: the CA key is made on a SoftHSM token
#                  and never leaves it; mw-pki signs through the token
#   hub            eclipse/zenoh in docker: mutual TLS, default-deny ACL by the
#                  client certificate's CN (deploy/zenoh/mtls/zenohd.json5)
#   OAuth          scripts/demo/idp.py (the provider: device login, RFC 8628)
#                  scripts/demo/admin.py (the enrolment server: checks the
#                  bearer token, signs a CSR with the token-held CA key)
#   client         nix/demo-vm.nix: the x86_64 base image as a QEMU VM, the
#                  node (mw-posix-node) unenrolled, no certificate
#   provision      `just provision --env-only`: the node makes its own key and
#                  CSR, the operator's token buys a signature, the node installs
#                  the chain and restarts into the hub
#   verify         the operator reaches the node only through the hub, and
#                  what must be refused is refused
#
# `all` runs everything and tears it down (DEMO_KEEP=1 leaves it up for a look).
# Exit status: 0 when every check passed.
#
# Environment (all optional): DEMO_DIR, DEMO_FLAKE (the flake with
# packages.x86-demo-vm, default .), DEMO_VM_BIN (a built VM's bin/ dir),
# DEMO_HUB_PORT, DEMO_NODE_PORT, DEMO_IDP_PORT, DEMO_ADMIN_PORT, DEMO_KEEP,
# DEMO_NO_BUILD.
set -uo pipefail

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
W=${DEMO_DIR:-$ROOT/target/demo}
ID=0d1e2f3a4b5c6d7e # nix/demo-vm.nix names the same node id
HUB=${DEMO_HUB_PORT:-7457}
NODEP=${DEMO_NODE_PORT:-17447}
IDPP=${DEMO_IDP_PORT:-18080}
ADMP=${DEMO_ADMIN_PORT:-18081}
ISSUER=http://127.0.0.1:$IDPP/realms/demo
HUB_CONTAINER=mw-demo-hub
FLAKE=${DEMO_FLAKE:-.}
TARGET=${CARGO_TARGET_DIR:-$ROOT/target}/release
MWCTL=$TARGET/mwctl
export MW_PKI_BIN=$TARGET/mw-pki
FAILS=0 CHECKS=0

say() { printf '\n== %s\n' "$*"; }
note() { printf '   %s\n' "$*"; }
pass() { CHECKS=$((CHECKS + 1)); printf 'PASS %s\n' "$*"; }
fail() { CHECKS=$((CHECKS + 1)); FAILS=$((FAILS + 1)); printf 'FAIL %s\n' "$*"; }
die() { printf 'demo: %s\n' "$*" >&2; exit 2; }

# check "what" cmd...    passes when cmd succeeds
check() {
  local what=$1; shift
  if "$@" >/dev/null 2>&1; then pass "$what"; else fail "$what"; fi
}
# refused "what" cmd...  passes when cmd fails (it was refused)
refused() {
  local what=$1; shift
  if "$@" >/dev/null 2>&1; then fail "$what (it was allowed)"; else pass "$what"; fi
}
# wait_for seconds "what" cmd...
wait_for() {
  local secs=$1 what=$2; shift 2
  local end=$((SECONDS + secs))
  while ((SECONDS < end)); do
    if "$@" >/dev/null 2>&1; then return 0; fi
    sleep 2
  done
  echo "demo: timed out after ${secs}s waiting for $what" >&2
  return 1
}

# ------------------------------------------------------------------ the pieces
mw_hub() { timeout 40 "$MWCTL" --zenoh "tls/127.0.0.1:$HUB" --zenoh-tls-dir "$1" --node "$ID" "${@:2}"; }
mw_direct() { timeout 40 "$MWCTL" --zenoh "tcp/127.0.0.1:$NODEP" --node "$ID" "$@"; }

stop_pid() {
  local f=$W/run/$1.pid
  [ -f "$f" ] || return 0
  local p; p=$(cat "$f")
  kill "$p" 2>/dev/null
  for _ in $(seq 20); do kill -0 "$p" 2>/dev/null || break; sleep 0.5; done
  kill -9 "$p" 2>/dev/null
  rm -f "$f"
}

down() {
  say "tear down"
  stop_pid vm
  stop_pid admin
  stop_pid idp
  docker rm -f "$HUB_CONTAINER" >/dev/null 2>&1
  note "stopped; logs and certificates stay in $W"
}

need() { command -v "$1" >/dev/null || die "needs $1 ($2)"; }

# login <user> <password> <store-dir>
login_as() {
  local user=$1 pass=$2 dir=$3
  mkdir -p "$dir" && chmod 700 "$dir"
  local out=$dir/login.out
  : > "$out"
  MW_AUTH_ISSUER=$ISSUER MW_AUTH_STORE=$dir/auth.json "$MWCTL" auth login > "$out" 2>&1 &
  local pid=$!
  local code=""
  for _ in $(seq 20); do
    code=$(sed -n 's/.*confirm the code \([A-Z0-9-]*\).*/\1/p' "$out")
    [ -n "$code" ] && break
    sleep 0.5
  done
  [ -n "$code" ] || { kill "$pid" 2>/dev/null; return 1; }
  # What a browser would do: open the verification page and sign in.
  curl -fsS -X POST "$ISSUER/device" --data-urlencode "user_code=$code" \
    --data-urlencode "username=$user" --data-urlencode "password=$pass" >/dev/null || true
  wait "$pid"
}

enroll_status() { # <store-dir> -> prints the HTTP status of an enrolment request
  local dir=$1 token cn=kd240-$ID
  token=$(MW_AUTH_ISSUER=$ISSUER MW_AUTH_STORE=$dir/auth.json "$MWCTL" auth token 2>/dev/null) || token=""
  python3 -I - "$ADMP" "${token:--}" "$cn" "$W/pki/probe.csr" <<'EOF'
import json, sys, urllib.request, urllib.error
port, tok, cn, csr = sys.argv[1], sys.argv[2], sys.argv[3], open(sys.argv[4]).read()
h = {"Content-Type": "application/json"}
if tok != "-":
    h["Authorization"] = "Bearer " + tok
r = urllib.request.Request(f"http://127.0.0.1:{port}/api/enroll", json.dumps({"cn": cn, "csr": csr}).encode(), h)
try:
    print(urllib.request.urlopen(r, timeout=30).status)
except urllib.error.HTTPError as e:
    print(e.code)
EOF
}

# ------------------------------------------------------------------------- up
build() {
  say "build"
  if [ -n "${DEMO_NO_BUILD:-}" ]; then note "DEMO_NO_BUILD: using what is built"; else
    (cd "$ROOT" && cargo build --release -q -p mwctl --features zenoh,auth -p mw-pki) || die "cargo build failed"
  fi
  [ -x "$MWCTL" ] && [ -x "$MW_PKI_BIN" ] || die "mwctl and mw-pki are not built"
  if [ -n "${DEMO_VM_BIN:-}" ]; then
    VMBIN=$DEMO_VM_BIN
  else
    need nix "to build the VM; or set DEMO_VM_BIN to a built VM's bin/ directory"
    (cd "$ROOT" && nix build "$FLAKE#x86-demo-vm" -o "$W/vm" --no-write-lock-file) || die "nix build of the VM failed"
    VMBIN=$W/vm/bin
  fi
  VMRUN=$(ls "$VMBIN"/run-*-vm 2>/dev/null | head -1)
  [ -x "$VMRUN" ] || die "no run-*-vm in $VMBIN"
  pass "mwctl, mw-pki and the client VM are built"
}

pki() {
  say "root of trust (a CA key on a SoftHSM token, signed through mw-pki)"
  rm -rf "$W/pki" "$W/rogue"
  "$ROOT/scripts/zenoh-pki.sh" token "$W/pki" >/dev/null || die "token"
  "$ROOT/scripts/zenoh-pki.sh" ca "$W/pki" >/dev/null || die "ca"
  # The hub's certificate names every address a client reaches it by; the VM
  # reaches the host at 10.0.2.2 (QEMU's user network).
  "$ROOT/scripts/zenoh-pki.sh" server "$W/pki" router localhost 127.0.0.1 10.0.2.2 >/dev/null || die "router cert"
  # The operator's own identity: the CA's custodian issues it from the
  # operator's CSR. (A node's certificate comes from the enrolment server.)
  "$ROOT/scripts/zenoh-pki.sh" csr "$W/pki" tool-demo >/dev/null
  "$ROOT/scripts/zenoh-pki.sh" client "$W/pki" tool-demo tool-demo >/dev/null || die "operator cert"
  # A request the enrolment server is asked to sign in the refusal checks.
  openssl ecparam -name prime256v1 -genkey -noout -out "$W/pki/probe.key" 2>/dev/null
  openssl req -new -key "$W/pki/probe.key" -subj "/CN=kd240-$ID" -out "$W/pki/probe.csr"
  # An unrelated CA that issues a certificate with the operator's CN.
  mkdir -p "$W/rogue/tool-demo"
  openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes -days 1 \
    -subj /CN=rogue-ca -keyout "$W/rogue/ca.key" -out "$W/rogue/ca.crt" 2>/dev/null
  openssl ecparam -name prime256v1 -genkey -noout -out "$W/rogue/tool-demo/key"
  openssl req -new -key "$W/rogue/tool-demo/key" -subj /CN=tool-demo -out "$W/rogue/req.csr"
  printf 'extendedKeyUsage=clientAuth\n' > "$W/rogue/ext"
  openssl x509 -req -in "$W/rogue/req.csr" -CA "$W/rogue/ca.crt" -CAkey "$W/rogue/ca.key" \
    -CAcreateserial -days 1 -extfile "$W/rogue/ext" -out "$W/rogue/tool-demo/cert" 2>/dev/null
  cp "$W/pki/ca/ca.crt" "$W/rogue/tool-demo/ca" # trusts the real CA: the router must refuse the rogue certificate
  pass "CA on the token; the hub's and the operator's certificates issued"
}

hub() {
  say "the hub (zenohd in docker, mutual TLS, access rules by certificate CN)"
  mkdir -p "$W/hub"
  # The shipped test rules, with this demo's node and operator names.
  sed -e "s/\"kd240-ci\"/\"kd240-$ID\"/" -e "s/\"tool-ci\"/\"tool-demo\"/" \
    "$ROOT/deploy/zenoh/mtls/zenohd.json5" > "$W/hub/zenohd.json5"
  docker rm -f "$HUB_CONTAINER" >/dev/null 2>&1
  docker run -d --name "$HUB_CONTAINER" -p "127.0.0.1:$HUB:7447" \
    -v "$W/hub/zenohd.json5:/etc/mesh/conf/zenohd.json5:ro" \
    -v "$W/pki/router:/etc/mesh/tls:ro" \
    eclipse/zenoh:1.10.1 -c /etc/mesh/conf/zenohd.json5 >/dev/null || die "the hub did not start (docker?)"
  wait_for 60 "the hub's port" bash -c "echo > /dev/tcp/127.0.0.1/$HUB" || { docker logs "$HUB_CONTAINER" 2>&1 | tail -5; die "hub"; }
  pass "the hub listens on tls/127.0.0.1:$HUB, client certificates required"
}

oauth() {
  say "OAuth: the identity provider and the enrolment server"
  mkdir -p "$W/run" "$W/log"
  python3 "$ROOT/scripts/demo/idp.py" --port "$IDPP" > "$W/log/idp.log" 2>&1 &
  echo $! > "$W/run/idp.pid"
  python3 "$ROOT/scripts/demo/admin.py" --port "$ADMP" --idp "$ISSUER" --pki-dir "$W/pki" \
    --mw-pki "$MW_PKI_BIN" --allow operator@demo.local > "$W/log/admin.log" 2>&1 &
  echo $! > "$W/run/admin.pid"
  wait_for 20 "the identity provider" curl -fsS "http://127.0.0.1:$IDPP/healthz" || die "idp"
  wait_for 20 "the enrolment server" curl -fsS "http://127.0.0.1:$ADMP/healthz" || die "admin"
  pass "provider at $ISSUER; enrolment server at http://127.0.0.1:$ADMP (operator@demo.local may enrol)"
}

vm() {
  say "the client (the x86_64 base image as a QEMU VM; the node has no certificate yet)"
  rm -f "$W/vm.qcow2"
  local accel="KVM"
  [ -w /dev/kvm ] || accel="TCG (no /dev/kvm: slower)"
  note "acceleration: $accel; console log: $W/log/vm.log"
  NIX_DISK_IMAGE=$W/vm.qcow2 QEMU_NET_OPTS="hostfwd=tcp:127.0.0.1:$NODEP-:7447" \
    "$VMRUN" > "$W/log/vm.log" 2>&1 &
  echo $! > "$W/run/vm.pid"
  wait_for 420 "the client to boot and answer" mw_direct info || { tail -15 "$W/log/vm.log"; die "the VM did not come up"; }
  pass "the client booted and answers sys/info before enrolment: $(mw_direct info 2>/dev/null | sed -n 's/^chip: *//p')"
}

up() {
  mkdir -p "$W"
  for c in docker openssl softhsm2-util python3 curl; do need "$c" "see docs/demo.md"; done
  docker info >/dev/null 2>&1 || die "docker is not usable by this user"
  build
  pki
  hub
  oauth
  vm
}

# ------------------------------------------------------------------ provision
provision() {
  say "before: the client is not on the hub"
  refused "no certificate yet, so the node is not reachable through the hub" mw_hub "$W/pki/tool-demo" info

  say "the operator signs in (OAuth device login, RFC 8628)"
  login_as operator@demo.local operator-pass "$W/auth/operator" && pass "operator@demo.local signed in; $(MW_AUTH_ISSUER=$ISSUER MW_AUTH_STORE=$W/auth/operator/auth.json "$MWCTL" auth status 2>&1 | cut -c1-110)" || { fail "the operator could not sign in"; return; }

  say "provision the client (just provision: CSR from the node, signature from the enrolment server, install)"
  note "the node is named by MW_HIL_LAN (QEMU's forwarded port), so --enroll-lan; the mesh CA is pinned by MW_PKI_CA"
  local out=$W/log/provision.out
  (
    cd "$ROOT" || exit 2
    MW_HIL_LAN="zenoh:tcp/127.0.0.1:$NODEP@$ID" MW_ZENOH_ROUTER="tls/10.0.2.2:$HUB" \
    MW_ADMIN_URL="http://127.0.0.1:$ADMP" MW_AUTH_ISSUER="$ISSUER" MW_AUTH_STORE="$W/auth/operator/auth.json" \
    MW_PKI_CA="$W/pki/ca/ca.crt" \
      just provision --env-only --enroll-lan --all --no-mdns --mwctl "$MWCTL"
  ) > "$out" 2>&1
  sed 's/^/   /' "$out" | grep -E "found|zenoh|warning|mesh identity|skipped|FAILED|refused" | head -8
  if grep -q "enrolled as kd240-$ID" "$out"; then pass "the node was enrolled as kd240-$ID"; else fail "the node was not enrolled (see $out)"; return; fi

  say "the node restarts with its certificate and joins the hub"
  wait_for 120 "the node on the hub" mw_hub "$W/pki/tool-demo" info && pass "the node is on the hub: it connected with its own certificate" || fail "the node did not appear on the hub (see $W/log/vm.log and the hub log)"
}

# --------------------------------------------------------------------- verify
verify() {
  say "verify: what works"
  local info
  info=$(mw_hub "$W/pki/tool-demo" info 2>&1)
  if echo "$info" | grep -q "chip: *linux-x86_64"; then pass "the operator reads the client's sys/info through the hub (chip linux-x86_64, node $ID)"; else fail "no sys/info through the hub: $(echo "$info" | tail -1)"; fi

  say "verify: what must be refused"
  refused "the hub refuses a client with no certificate (plain TCP to its TLS port)" \
    timeout 40 "$MWCTL" --zenoh "tcp/127.0.0.1:$HUB" --node "$ID" info
  refused "the hub refuses a certificate from another CA, even with the right CN" mw_hub "$W/rogue/tool-demo" info
  # The node would answer a bad claim with an error of its own; the hub's rule
  # (deny-claim) drops it before it gets there, so there is no reply at all.
  local claim
  claim=$(mw_hub "$W/pki/tool-demo" request sys/trust/claim 0000000000000000000000000000000000 2>&1)
  if echo "$claim" | grep -q "no response"; then pass "the hub's rules drop sys/trust/claim even for the operator (deny-claim): no reply"; else fail "sys/trust/claim was not dropped by the hub: $(echo "$claim" | tail -1)"; fi
  local code
  code=$(enroll_status "$W/auth/nobody")
  [ "$code" = 401 ] && pass "the enrolment server refuses a request with no sign-in (401)" || fail "no-token enrolment answered $code, not 401"
  login_as intruder@demo.local intruder-pass "$W/auth/intruder" >/dev/null 2>&1
  code=$(enroll_status "$W/auth/intruder")
  [ "$code" = 403 ] && pass "a signed-in user who may not enrol is refused (403)" || fail "the intruder's enrolment answered $code, not 403"
  code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "http://127.0.0.1:$IDPP/realms/demo/device" \
    -d "user_code=NOPE-NOPE&username=operator@demo.local&password=wrong")
  [ "$code" = 400 ] || [ "$code" = 401 ] && pass "the provider refuses a bad code or password ($code)" || fail "the provider answered $code"

  say "the story in the logs"
  grep -E "enrolled|refused" "$W/log/admin.log" | sed 's/^/   /' | tail -6
}

summary() {
  say "result"
  if [ "$FAILS" -eq 0 ]; then printf 'DEMO PASSED: %d checks\n' "$CHECKS"; else printf 'DEMO FAILED: %d of %d checks failed\n' "$FAILS" "$CHECKS"; fi
}

status() {
  for p in vm admin idp; do
    if [ -f "$W/run/$p.pid" ] && kill -0 "$(cat "$W/run/$p.pid")" 2>/dev/null; then echo "$p: running"; else echo "$p: stopped"; fi
  done
  docker ps --filter "name=$HUB_CONTAINER" --format 'hub: {{.Status}}' | grep . || echo "hub: stopped"
}

cmd=${1:-all}
case $cmd in
  up) up ;;
  provision) provision ;;
  verify) verify; summary ;;
  down) down ;;
  status) status ;;
  all)
    trap '[ -n "${DEMO_KEEP:-}" ] || down' EXIT
    up
    provision
    verify
    summary
    ;;
  *) die "usage: $0 [all|up|provision|verify|down|status]" ;;
esac
[ "$FAILS" -eq 0 ]
