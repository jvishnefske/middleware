#!/usr/bin/env bash
# Placement seed sweep for the 5A-75B design: re-runs only nextpnr (the
# command LiteX wrote into build/mw5a75b/build_mw5a75b.sh, after
# `python soc.py` or `python soc.py --no-compile` plus one synthesis) with
# each seed, in parallel, and prints each clock's achieved frequency.
#
#   ./seeds.sh 1 20          # seeds 1..20, $JOBS at a time (default 4)
#   ./seeds.sh 7 7           # one seed; its .config stays in build/seeds/
#
# A seed that meets every clock is reported "met"; keep its
# build/seeds/seed<N>.config and pack it with ecppack (the line after
# nextpnr in the build script), or rebuild with `python soc.py --seed N`.
set -euo pipefail
cd "$(dirname "$0")"
first=${1:-1}
last=${2:-$first}
jobs=${JOBS:-4}
script=build/mw5a75b/build_mw5a75b.sh
[ -f "$script" ] || { echo "no $script: run python soc.py first" >&2; exit 1; }
cmd=$(grep -m1 '^nextpnr-ecp5' "$script")
mkdir -p build/seeds
run() {
    local s=$1
    local c
    c=$(sed -E -e "s#--seed [0-9]+#--seed $s#" \
        -e "s#--textcfg [^ ]+#--textcfg ../seeds/seed$s.config#" <<<"$cmd")
    (cd build/mw5a75b && eval "$c" >"../seeds/seed$s.log" 2>&1) || true
    local fmax
    fmax=$(grep -E "Max frequency for clock" "build/seeds/seed$s.log" | tail -n 4 \
        | sed -E "s/.*clock +'([^']+)': ([0-9.]+) MHz \((PASS|FAIL) at ([0-9.]+) MHz\).*/\1 \2\/\4 \3/" \
        | tr '\n' ';')
    if grep -q "FAIL at" "build/seeds/seed$s.log"; then
        echo "seed $s: $fmax"
    elif [ -n "$fmax" ]; then
        echo "seed $s: met: $fmax"
    else
        echo "seed $s: nextpnr failed (build/seeds/seed$s.log)"
    fi
}
export -f run
export cmd
seq "$first" "$last" | xargs -P "$jobs" -I{} bash -c 'run {}'
