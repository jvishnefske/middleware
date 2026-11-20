#!/usr/bin/env bash
# The updates channel end to end on this machine (design.md UP), with the
# images the cluster runs:
#
#   zenohd (image)     the updates router: rendered config, usrpwd, ACL
#   mw-updates (image) the release store, holding only the root public key
#   zenohd (image)     a plain router for the posix-node's own mesh session
#   mw-posix-node      a Linux payload host, its local link a socat PTY
#   mw-updater         pulls releases into the node over that PTY
#   mw-release         signs with the root and publishes
#
# It claims the node with a throwaway root (or ROOT_SEED=<file>), publishes
# a payload, waits for the updater to install it, then publishes a second
# one and shows the updater installing it at once (the `latest` put wakes
# it; its poll is 10 min). Everything lives in a temporary directory and is
# torn down on exit; KEEP=1 leaves it running.
#
# Needs: podman (or docker), socat, nix (to build the images when they are
# not loaded), cargo. Ports: 127.0.0.1:17447/17448 (updates router),
# 17450 (mesh router).
#
#   scripts/updates-demo.sh [payload.wasm payload2.wasm]
set -euo pipefail
cd "$(dirname "$0")/.."

oci=$(command -v podman || command -v docker) || { echo "needs podman or docker" >&2; exit 1; }
command -v socat >/dev/null || { echo "needs socat" >&2; exit 1; }
w=$(mktemp -d -t mw-updates-demo.XXXXXX)
say() { printf '\n\033[1m== %s\033[0m\n' "$*"; }
cleanup() {
  [ "${KEEP:-}" = 1 ] && { echo "left running in $w"; return; }
  kill "${pids[@]}" 2>/dev/null || true
  "$oci" rm -f mwdemo-z mwdemo-u mwdemo-mesh >/dev/null 2>&1 || true
  "$oci" network rm mwdemo >/dev/null 2>&1 || true
  # The server's files belong to the container's user.
  if [ "$(basename "$oci")" = podman ]; then podman unshare rm -rf "$w"; else rm -rf "$w"; fi
}
pids=()
trap cleanup EXIT

payload1=${1:-}
payload2=${2:-}
say "build"
cargo build -q -p mw-updates -p mw-posix-node -p mwctl
bin=target/debug
if [ -z "$payload1" ]; then
  # Two payloads: the posix-node fixture, and the same with another value.
  cargo run -q -p mw-updates --example wat2wasm -- posix-node/tests/fixtures/pi_demo.wat "$w/p1.wasm"
  sed 's/pi: started/pi: v2 run!/' posix-node/tests/fixtures/pi_demo.wat > "$w/p2.wat"
  cargo run -q -p mw-updates --example wat2wasm -- "$w/p2.wat" "$w/p2.wasm"
  payload1=$w/p1.wasm payload2=$w/p2.wasm
fi
for img in zenohd mw-updates; do
  if ! "$oci" image exists "localhost/mw/$img:dev" 2>/dev/null &&
     ! "$oci" image inspect "localhost/mw/$img:dev" >/dev/null 2>&1; then
    say "image $img (nix build .#image-$img)"
    "$oci" load -q -i "$(nix build --no-link --print-out-paths ".#image-$img")"
  fi
done
ztag=$("$oci" images --format '{{.Repository}}:{{.Tag}}' | grep '^localhost/mw/zenohd:' | head -1)
utag=$("$oci" images --format '{{.Repository}}:{{.Tag}}' | grep '^localhost/mw/mw-updates:' | head -1)

say "keys and router config"
# Keys and the clients' credentials stay private (0700); only what the
# containers read is opened up below.
mkdir -m 0700 "$w/keys"
( umask 077
  if [ -n "${ROOT_SEED:-}" ]; then cp "$ROOT_SEED" "$w/keys/root.seed"; else head -c 32 /dev/urandom > "$w/keys/root.seed"; fi )
root=$($bin/mwctl key pub "$w/keys/root.seed")
echo "root public key: $root"
pw() { head -c 18 /dev/urandom | base64 | tr -d '/+='; }
mkdir -p "$w/auth" "$w/data"
{ echo "mw-updates:$(pw)"; echo "release:$(pw)"; echo "demo-host:$(pw)"; } > "$w/auth/dictionary"
grep "^mw-updates:" "$w/auth/dictionary" > "$w/mw-updates.cred"
for u in release demo-host; do (umask 077; grep "^$u:" "$w/auth/dictionary" > "$w/keys/$u.cred"); done
cargo run -q -p mw-updates --bin mw-updates -- router-config \
  --server mw-updates --publishers release --updaters demo-host > "$w/config.json5"
# Rootless containers run as another uid: they read the config, the
# dictionary and the server's credentials, and write the store.
chmod a+rx "$w" "$w/auth"; chmod a+r "$w/config.json5" "$w/auth/dictionary" "$w/mw-updates.cred"
chmod a+rwx "$w/data"

say "updates router and server (containers, read-only root)"
"$oci" network create mwdemo >/dev/null 2>&1 || true
"$oci" run -d --name mwdemo-z --network mwdemo --read-only --tmpfs /tmp \
  -p 127.0.0.1:17447:7447 -p 127.0.0.1:17448:7448 \
  -v "$w/config.json5:/etc/zenoh/config.json5:ro" -v "$w/auth:/etc/zenoh/auth:ro" \
  "$ztag" -c /etc/zenoh/config.json5 >/dev/null
"$oci" run -d --name mwdemo-u --network mwdemo --read-only \
  -e MW_UPDATES_ROOT_KEY="$root" -e MW_ZENOH_ROUTER=tcp/mwdemo-z:7447 \
  -e MW_ZENOH_CREDENTIALS=/etc/mw/credentials \
  -v "$w/mw-updates.cred:/etc/mw/credentials:ro" -v "$w/data:/data" \
  "$utag" serve >/dev/null
"$oci" run -d --name mwdemo-mesh -p 127.0.0.1:17450:7447 "$ztag" >/dev/null

say "posix-node over a PTY"
mkdir -p "$w/pty"
socat "pty,raw,echo=0,link=$w/pty/node" "pty,raw,echo=0,link=$w/pty/updater" 2>"$w/socat.log" &
pids+=($!)
for _ in $(seq 50); do [ -e "$w/pty/updater" ] && break; sleep 0.1; done
MW_POSIX_NODE_STATE=$w/node MW_POSIX_NODE_NODE_ID=00d3e0d3e0d3e001 \
MW_POSIX_NODE_LOCAL_SERIAL=$w/pty/node MW_ZENOH_ROUTER=tcp/127.0.0.1:17450 MW_PORT=50099 \
  $bin/mw-posix-node >"$w/node.log" 2>&1 &
pids+=($!)
for _ in $(seq 50); do $bin/mwctl --port "$w/pty/updater" info >/dev/null 2>&1 && break; sleep 0.2; done
$bin/mwctl --port "$w/pty/updater" trust claim --key "$w/keys/root.seed"

route=(MW_UPDATES_ENDPOINT=ws/127.0.0.1:17448)
publish() {
  env "${route[@]}" MW_ZENOH_CREDENTIALS="$w/keys/release.cred" \
    $bin/mw-release publish --channel demo --target "$chip" --kind payload --key "$w/keys/root.seed" "$1"
}
chip=$($bin/mwctl --port "$w/pty/updater" info | sed -n 's/^chip: *//p')

say "publish payload 1 for $chip"
publish "$payload1"

say "updater"
env "${route[@]}" MW_UPDATES_CHANNEL=demo MW_UPDATES_ROOT_KEY="$root" \
  MW_ZENOH_CREDENTIALS="$w/keys/demo-host.cred" MW_UPDATER_DEVICES="$w/pty/updater" MW_UPDATES_POLL=600 \
  $bin/mw-updater >"$w/updater.log" 2>&1 &
pids+=($!)
wait_for() { for _ in $(seq 100); do grep -q "$1" "$w/updater.log" && return 0; sleep 0.2; done; cat "$w/updater.log"; return 1; }
wait_for "payload .*: installed"
grep "payload" "$w/updater.log" | tail -1

say "publish payload 2: installed on the latest put, not the 10 min poll"
start=$(date +%s.%N)
publish "$payload2"
v2=$(env "${route[@]}" MW_ZENOH_CREDENTIALS="$w/keys/release.cred" $bin/mw-release latest --channel demo --target "$chip" | awk '/^payload/{print $2}')
wait_for "payload $v2: installed"
printf 'installed %s %.1f s after the publish\n' "$v2" "$(echo "$(date +%s.%N) - $start" | bc)"

say "a forged release (another key) is refused by the server"
(umask 077; head -c 32 /dev/urandom > "$w/keys/mallory.seed")
if env "${route[@]}" MW_ZENOH_CREDENTIALS="$w/keys/release.cred" \
   $bin/mw-release publish --channel demo --target "$chip" --kind payload --key "$w/keys/mallory.seed" "$payload1" 2>/dev/null; then
  echo "FAIL: the forged release was stored" >&2; exit 1
fi
"$oci" logs mwdemo-u 2>&1 | grep refused | tail -1

say "server log"
"$oci" logs mwdemo-u 2>&1 | grep -v status | tail -5
echo
echo "OK: published, pulled, installed, re-published and refused a forgery."
