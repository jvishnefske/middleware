#!/usr/bin/env bash
# The Zenoh mesh's root of trust, provisioned on a PKCS#11 token.
#
# The CA key is generated on a token and never leaves it. In CI and on a
# developer machine the token is SoftHSM2 (created here); in production it is
# an HSM or a TPM, provisioned by the M-of-N ceremony in docs/pki-ceremony.md.
# Certificates are issued by `mw-pki` (tools/mw-pki), which signs through the
# token. A device's key is made on the device: this script makes it only for
# the test fixtures, and never for production.
#
#   zenoh-pki.sh token  <dir>                     # SoftHSM token + CA key (label mw-zenoh-ca), once
#   zenoh-pki.sh ca     <dir>                     # <dir>/ca/ca.crt, signed by the token key
#   zenoh-pki.sh csr    <dir> <name>              # <dir>/<name>/{key,req.csr}: the device's own key
#   zenoh-pki.sh client <dir> <name> <cn>         # kd240-<id> or tool-<id>: <dir>/<name>/{ca,cert,key}
#   zenoh-pki.sh server <dir> <name> <san>...     # the router: <dir>/<name>/{ca,cert,key}
#
# Client rules (MESH.md): the CN is `kd240-<id>` or `tool-<id>` with a
# [a-z0-9-] id; the key never leaves the device; a client directory holds `ca`,
# `cert` and `key` with no extensions (what `mwctl --zenoh-tls-dir` and the
# KD240's MW_ZENOH_TLS_DIR read).
#
# Environment written by `token` to <dir>/env and read by every other command.
set -euo pipefail

die() { echo "zenoh-pki: $*" >&2; exit 1; }

mw_pki() {
  if [ -n "${MW_PKI_BIN:-}" ]; then "$MW_PKI_BIN" "$@"
  elif command -v mw-pki >/dev/null; then mw-pki "$@"
  else cargo run -q -p mw-pki -- "$@"; fi
}

module() {
  for m in /usr/lib/softhsm/libsofthsm2.so /usr/lib/x86_64-linux-gnu/softhsm/libsofthsm2.so; do
    [ -f "$m" ] && { echo "$m"; return; }
  done
  die "SoftHSM2 not found (apt install softhsm2)"
}

load_env() {
  [ -f "$1/env" ] || die "no token in $1 (run: zenoh-pki.sh token $1)"
  # shellcheck disable=SC1090
  set -a; . "$1/env"; set +a
}

cmd_token() {
  local dir=$1
  [ -e "$dir/env" ] && die "$dir already has a token"
  mkdir -p "$dir/tokens"
  umask 077
  local pin; pin=$(openssl rand -hex 8)
  local mod; mod=$(module)
  printf 'directories.tokendir = %s/tokens\nobjectstore.backend = file\nlog.level = ERROR\n' "$(cd "$dir" && pwd)" > "$dir/softhsm2.conf"
  cat > "$dir/env" <<ENV
SOFTHSM2_CONF=$(cd "$dir" && pwd)/softhsm2.conf
PKCS11_MODULE=$mod
PKCS11_TOKEN=mw-zenoh-ca
MW_PKI_PIN=$pin
ENV
  load_env "$dir"
  softhsm2-util --init-token --free --label mw-zenoh-ca --so-pin "$(openssl rand -hex 8)" --pin "$pin" >/dev/null
  pkcs11-tool --module "$mod" --login --pin "$pin" --keypairgen --key-type EC:prime256v1 \
    --label mw-zenoh-ca --id 01 >/dev/null
  echo "token mw-zenoh-ca in $dir (PIN in $dir/env, mode 0600)"
}

cmd_ca() {
  load_env "$1"
  mkdir -p "$1/ca"
  mw_pki ca --key mw-zenoh-ca "$1/ca" mw-zenoh-test-ca
}

cmd_csr() {
  local dir=$1 name=$2
  mkdir -p "$dir/$name"
  umask 077
  openssl ecparam -name prime256v1 -genkey -noout -out "$dir/$name/key"
  openssl req -new -key "$dir/$name/key" -subj "/CN=$name" -out "$dir/$name/req.csr"
  echo "CSR $dir/$name/req.csr (the key stays in $dir/$name/key)"
}

bundle() {
  # The issued certificate, the CA and the key, in the layout the clients read.
  # `cert` is the full chain (leaf, then CA): Zenoh's TLS client presents it to
  # the router, and the router's own chain is the same file (MESH.md).
  local dir=$1 name=$2 cert=$3
  cat "$cert" "$dir/ca/ca.crt" > "$dir/$name/cert"
  cp "$dir/ca/ca.crt" "$dir/$name/ca"
  chmod 0600 "$dir/$name/key"
  rm -f "$dir/$name/req.csr"
}

cmd_client() {
  local dir=$1 name=$2 cn=$3
  load_env "$dir"
  [ -f "$dir/$name/req.csr" ] || die "no CSR for $name (run csr first)"
  mw_pki client --key mw-zenoh-ca "$dir/ca/ca.crt" "$dir/$name/req.csr" "$cn" "$dir/$name/cert.new"
  bundle "$dir" "$name" "$dir/$name/cert.new"
  rm -f "$dir/$name/cert.new"
  echo "client $cn: $dir/$name/{ca,cert,key}"
}

cmd_server() {
  local dir=$1 name=$2; shift 2
  load_env "$dir"
  mkdir -p "$dir/$name"
  umask 077
  openssl ecparam -name prime256v1 -genkey -noout -out "$dir/$name/key"
  openssl req -new -key "$dir/$name/key" -subj "/CN=$1" -out "$dir/$name/req.csr"
  mw_pki server --key mw-zenoh-ca "$dir/ca/ca.crt" "$dir/$name/req.csr" "$dir/$name/cert.new" "$@"
  bundle "$dir" "$name" "$dir/$name/cert.new"
  rm -f "$dir/$name/cert.new"
  echo "router TLS: $dir/$name/{ca,cert,key}"
}

case "${1:-}" in
  token) shift; [ "$#" -eq 1 ] || die "usage: token <dir>"; cmd_token "$1" ;;
  ca) shift; [ "$#" -eq 1 ] || die "usage: ca <dir>"; cmd_ca "$1" ;;
  csr) shift; [ "$#" -eq 2 ] || die "usage: csr <dir> <name>"; cmd_csr "$@" ;;
  client) shift; [ "$#" -eq 3 ] || die "usage: client <dir> <name> <cn>"; cmd_client "$@" ;;
  server) shift; [ "$#" -ge 3 ] || die "usage: server <dir> <name> <san>..."; cmd_server "$@" ;;
  *) sed -n '2,19p' "$0" >&2; exit 2 ;;
esac
