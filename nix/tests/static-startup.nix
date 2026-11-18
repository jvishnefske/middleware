# MS57: the static musl service binaries get past their Zenoh
# configuration at start.
#
# On the main thread of a static musl binary the stack cannot be measured,
# and the Zenoh configuration parse (pest, through json5) refused to run
# there ("stack limit reached"), so mw-gateway and mw-posix-node exited at
# once. Each binary here is started with a router on loopback that nothing
# listens on, in a fresh state directory, for a few seconds: it must not
# report a configuration error. What it does next (wait for the router,
# for the serial device) is not this check's business.
{ pkgs, binaries }:
let
  run = name: env: ''
    echo "== ${name}"
    state=$(mktemp -d)
    set +e
    env -i ${env} MW_ZENOH_ROUTER=tcp/127.0.0.1:7 \
      timeout 5 ${binaries.${name}}/bin/${name} > "$state/log" 2>&1
    set -e
    cat "$state/log"
    if grep -qE 'zenoh config|stack limit reached' "$state/log"; then
      echo "${name}: the Zenoh configuration did not parse" >&2
      exit 1
    fi
  '';
in
pkgs.runCommand "static-startup" { nativeBuildInputs = [ pkgs.coreutils pkgs.gnugrep ]; } (''
  set -eu
  ${run "mw-gateway" ''MW_GATEWAY_SERIAL=/dev/null MW_GATEWAY_STATE="$state"''}
'' + pkgs.lib.optionalString (binaries ? mw-posix-node) ''
  ${run "mw-posix-node" ''MW_POSIX_NODE_STATE="$state" MW_POSIX_NODE_NODE_ID=0102030405060708''}
'' + ''
  touch $out
'')
