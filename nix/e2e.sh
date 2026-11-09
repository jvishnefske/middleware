#!@bash@/bin/bash
# `nix run .#e2e`: the 0.6 end-to-end run against the lab (design.md M17).
# Phase 2 skeleton: the flake fills @images@ (one "name path" line per built
# image tarball) and @robotVm@; this script prints the plan and touches
# nothing. Phase 3 replaces each `plan` line with the real step (the `mw`
# CLI: mw load, mw apply, mw robot enroll, the assertions, mw logs).
set -euo pipefail

plan() { printf '  would: %s\n' "$*"; }

echo "mw e2e (dry run: Phase 3 implements the steps)"
echo "images built:"
while read -r name path; do
  [ -n "$name" ] || continue
  printf '  %-12s %s\n' "$name" "$path"
done <<'IMAGES'
@images@
IMAGES

echo "steps:"
plan "platform smoke: lab up, Istio ready, sidecar certificates issued by SPIRE, Alloy reachable"
plan "mw load <each image above>   (k3s ctr images import on every node)"
plan "mw apply demo-echo (3 replicas), demo-caller, zenohd, mw-gateway"
plan "boot the robot VM (@robotVm@ --tap tap-mwrobot) and mw robot enroll"
plan "assert identity, gather, deadline, trace, drain, Zenoh ACL, gateway discovery"
plan "collect logs into target/e2e/"
