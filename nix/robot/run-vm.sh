#!@bash@/bin/bash
# Launcher for the robot VM (`nix run .#robot-vm -- [options]`). The flake
# fills its at-sign placeholders: bash, vm (the NixOS run-mw-robot-vm script)
# and labMac. Needs no privileges itself: attaching to the lab takes a tap
# or bridge helper the user set up beforehand (README "Robot VM").
#
#   --state DIR     host directory shared as /var/lib/mw-robot
#                   (default ./robot-state, created if missing)
#   --tap NAME      attach a second NIC to an existing tap device (the lab's
#                   convention: a tap owned by you, enslaved to br-infra)
#   --bridge NAME   attach through qemu-bridge-helper instead (needs the
#                   setuid helper and `allow NAME` in /etc/qemu/bridge.conf)
#   --dry-run       print the QEMU command line instead of running it
#   --              everything after is passed to QEMU unchanged
set -euo pipefail

state=./robot-state
net=()
dry=0
while [ $# -gt 0 ]; do
  case "$1" in
    --state) state=$2; shift 2 ;;
    --tap) net=(-netdev "tap,id=lab,ifname=$2,script=no,downscript=no" -device "virtio-net-pci,netdev=lab,mac=@labMac@"); shift 2 ;;
    --bridge) net=(-netdev "bridge,id=lab,br=$2" -device "virtio-net-pci,netdev=lab,mac=@labMac@"); shift 2 ;;
    --dry-run) dry=1; shift ;;
    --) shift; break ;;
    -h|--help) sed -n '2,14p' "$0"; exit 0 ;;
    *) echo "robot-vm: unknown option $1 (try --help)" >&2; exit 2 ;;
  esac
done

mkdir -p "$state"
MW_ROBOT_STATE=$(cd "$state" && pwd)
export MW_ROBOT_STATE
for f in join-token bundle.spiffe; do
  [ -e "$MW_ROBOT_STATE/$f" ] || echo "robot-vm: note: $MW_ROBOT_STATE/$f missing; spire-agent will not start" >&2
done

if [ "$dry" = 1 ]; then
  echo "MW_ROBOT_STATE=$MW_ROBOT_STATE @vm@ ${net[*]} $*"
  exit 0
fi
exec @vm@ "${net[@]}" "$@"
