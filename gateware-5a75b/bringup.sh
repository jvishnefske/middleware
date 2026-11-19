#!/usr/bin/env bash
# Board bring-up for the Colorlight 5A-75B (design.md CL8, CL9): one
# command per step, each failing loudly when the board does not answer as
# the design says.
#
#   ./bringup.sh detect               # JTAG: IDCODE 0x41111043 (LFE5U-25F)
#   ./bringup.sh blinky               # the LED blinker in SRAM
#   ./bringup.sh build PORT TXDELAY   # build build/p<PORT>-<TXDELAY>/ (no board)
#   ./bringup.sh sram PORT TXDELAY    # that build (built if missing) in SRAM
#   ./bringup.sh net-check IP         # ping, then mwctl --udp IP info
#   ./bringup.sh upload IP [SECONDS]  # bridge.wat as an image, start, monitor
#   ./bringup.sh flash [BIT]          # write BIT [build/all/mw5a75b.bit] to flash
#   ./bringup.sh report               # Fmax and utilisation of every build/p*/
#
# PORT is 0 or 1 (`soc.py --only-port`); TXDELAY is the RGMII transmit
# clock delay of that port in ns, 0 to 3 in steps of 0.025 (`soc.py
# --tx-delay<PORT>`, LiteEth's DELAYG taps). One-port builds take the
# default addresses: port 0 is 192.168.1.50, port 1 is 192.168.2.50.
#
# JTAG is the FT4232H-56Q MiniModule, channel A (ADBUS0-3: TCK, TDI, TDO,
# TMS), serial FT4YW8B7. This host also has the KD240 carrier's FT4232H
# (XFL1HM3KDKK1), which must never be driven from here: every
# openFPGALoader call goes through `ofl`, which names the MiniModule.
#
# The toolchain comes from this directory's flake: the script re-runs
# itself under `nix develop` when it is not in that shell. mwctl is
# $MWCTL, or the workspace's release build (built with cargo -j 2).
set -euo pipefail

readonly FTDI_SERIAL=FT4YW8B7
readonly KD240_SERIAL=XFL1HM3KDKK1
readonly CABLE=ft4232
readonly FTDI_CHANNEL=0          # A: ADBUS0-3
readonly IDCODE=0x41111043       # LFE5U-25F
readonly MIN_FREE_GB=8           # one nextpnr of the full design needs about this

HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
ROOT=$(dirname "$HERE")
cd "$HERE"

die() {
    echo "bringup: $*" >&2
    exit 1
}

usage() {
    sed -n '2,/^set -euo/p' "$0" | sed -e '/^set -euo/d' -e 's/^# \{0,1\}//'
    exit 2
}

# The serial is fixed in this file; refuse to run if an edit made it
# empty, the KD240's, or not an FTDI serial at all.
check_serial() {
    [[ $FTDI_SERIAL =~ ^FT[0-9A-Z]{6}$ ]] || die "FTDI_SERIAL '$FTDI_SERIAL' is not the MiniModule's"
    [[ $FTDI_SERIAL != "$KD240_SERIAL" ]] || die "FTDI_SERIAL is the KD240 carrier's FT4232H"
}

# openFPGALoader on the MiniModule only.
ofl() {
    check_serial
    local a
    for a in "$@"; do
        case $a in
            *"$KD240_SERIAL"*) die "refusing an argument naming the KD240 carrier: $a" ;;
            --ftdi-serial* | --usb-serial-num* | -c | --cable* | -b | --board*)
                die "ofl sets the cable and serial itself: $a" ;;
        esac
    done
    echo "+ openFPGALoader -c $CABLE --ftdi-serial $FTDI_SERIAL --ftdi-channel $FTDI_CHANNEL $*" >&2
    openFPGALoader -c "$CABLE" --ftdi-serial "$FTDI_SERIAL" --ftdi-channel "$FTDI_CHANNEL" "$@"
}

need_shell() {
    local t missing=
    for t in yosys nextpnr-ecp5 ecppack openFPGALoader wasm-tools python; do
        command -v "$t" >/dev/null || missing+=" $t"
    done
    [[ -n $missing ]] || return 0
    [[ -z ${BRINGUP_IN_NIX:-} ]] || die "nix develop $HERE gave no$missing"
    export BRINGUP_IN_NIX=1
    exec nix develop "$HERE" -c "$HERE/bringup.sh" "$@"
}

mwctl() {
    local bin=${MWCTL:-}
    if [[ -z $bin ]]; then
        cargo build -q --release -j 2 -p mwctl --manifest-path "$ROOT/Cargo.toml"
        bin=$ROOT/target/release/mwctl
    fi
    echo "+ mwctl $*" >&2
    "$bin" "$@"
}

check_port() {
    [[ $1 == 0 || $1 == 1 ]] || die "PORT must be 0 or 1, not '$1'"
}

check_delay() {
    [[ $1 =~ ^[0-9]+(\.[0-9]+)?$ ]] || die "TXDELAY is in ns (0, 1, 2, 1.5 ...), not '$1'"
    awk -v d="$1" 'BEGIN { exit !(d < 3.2) }' || die "TXDELAY $1 ns is past the 127 taps of DELAYG"
}

check_ip() {
    [[ $1 =~ ^[0-9]{1,3}(\.[0-9]{1,3}){3}$ ]] || die "IP must be A.B.C.D, not '$1'"
}

wait_for_memory() {
    local avail
    while :; do
        avail=$(free -g | awk '/^Mem:/ { print $7 }')
        ((avail >= MIN_FREE_GB)) && return
        echo "bringup: ${avail} GB available, waiting for ${MIN_FREE_GB} GB" >&2
        sleep 30
    done
}

# Fmax per clock and nextpnr's utilisation from a build log.
summary() {
    local log=$1
    # nextpnr reports Fmax after placement and again after routing: keep
    # each clock's last value.
    grep -E "^(Info|Warning): Max frequency for clock" "$log" |
        sed -E "s/.*clock +'([^']+)': ([0-9.]+) MHz \((PASS|FAIL) at ([0-9.]+) MHz\).*/\1 \2 \3 \4/" |
        awk '{ f[$1] = $2 " MHz (" $3 " at " $4 ")"; if (!($1 in o)) o[$1] = n++ }
             END { for (c in o) k[o[c]] = c; for (i = 0; i < n; i++) print "  " k[i] ": " f[k[i]] }'
    grep -E "^Info:[[:space:]]+(TRELLIS_COMB|TRELLIS_FF|DP16KD):" "$log" | tail -n 3 |
        sed -E 's/^Info:[[:space:]]+/  /'
}

build_dir() {
    echo "build/p$1-$2"
}

# nextpnr's placement seed per one-port build: the first seed whose
# placement meets 50 MHz and that port's 125 MHz RGMII clock (README
# "Bring-up builds"). $SEED overrides it.
seed_for() {
    local seeds=(1 2)
    echo "${SEED:-${seeds[$1]}}"
}

# Whether a build log's routed Fmax met every clock.
timing_met() {
    ! summary "$1" | grep -q "FAIL at"
}

build() {
    local port=$1 delay=$2 dir seed
    check_port "$port"
    check_delay "$delay"
    dir=$(build_dir "$port" "$delay")
    if [[ -f $dir/mw5a75b.bit ]]; then
        echo "bringup: $dir/mw5a75b.bit exists" >&2
        return
    fi
    seed=$(seed_for "$port")
    [[ $seed =~ ^[0-9]+$ ]] || die "SEED must be a number, not '$seed'"
    wait_for_memory
    mkdir -p "$dir"
    local args=(--no-sha256 --only-port "$port" "--tx-delay$port" "${delay}e-9"
        --seed "$seed" --build-dir "$dir")
    echo "+ python soc.py ${args[*]}" | tee "$dir/build.log" >&2
    python soc.py "${args[@]}" >>"$dir/build.log" 2>&1 ||
        die "build failed, see $dir/build.log"
    [[ -f $dir/mw5a75b.bit ]] || die "no $dir/mw5a75b.bit, see $dir/build.log"
    summary "$dir/build.log"
    timing_met "$dir/build.log" ||
        echo "bringup: $dir misses timing; try another SEED (rm -r $dir first)" >&2
}

cmd_detect() {
    local out
    out=$(ofl --detect 2>&1) || { echo "$out"; die "openFPGALoader --detect failed"; }
    echo "$out"
    grep -qiE "idcode[[:space:]:]+$IDCODE\b" <<<"$out" ||
        die "expected IDCODE $IDCODE (LFE5U-25F)"
    echo "bringup: IDCODE $IDCODE, LFE5U-25F"
}

cmd_blinky() {
    if [[ ! -f build/blinky/blinky.bit ]]; then
        python blinky.py
    fi
    ofl -m build/blinky/blinky.bit
    echo "bringup: blinky loaded; the LED should blink at about 1.5 Hz"
}

cmd_sram() {
    local dir
    build "$1" "$2"
    dir=$(build_dir "$1" "$2")
    timing_met "$dir/build.log" || [[ -n ${ALLOW_TIMING_FAIL:-} ]] ||
        die "$dir misses timing (ALLOW_TIMING_FAIL=1 loads it anyway)"
    ofl -m "$dir/mw5a75b.bit"
    echo "bringup: port $1 (tx delay $2 ns) in SRAM; the LED beats at 1 Hz"
}

# What `mwctl info` prints for the 5A-75B (gateware mw5a75b/system.py:
# CHIP, ABI_VERSION, CAPACITY, Caps::BRIDGE; mwctl report::format_info).
# mwctl's own ABI is 7 on this tree, so the line reads
# "ABI: v6 (mwctl expects v7)"; only the board's number is checked.
cmd_net_check() {
    local ip=$1 out bad=0
    check_ip "$ip"
    echo "+ ping -c 5 $ip" >&2
    ping -c 5 "$ip" || die "no ping replies from $ip"
    out=$(mwctl --udp "$ip" info) || { echo "$out"; die "mwctl info over UDP failed"; }
    echo "$out"
    local want
    for want in \
        '^chip: +ecp5$' \
        '^firmware: +0\.1\.0$' \
        '^ABI: +v6( |$)' \
        '^payload capacity: +16384 bytes$' \
        '^links: +bridged by rules'; do
        if ! grep -qE "$want" <<<"$out"; then
            echo "bringup: missing /$want/" >&2
            bad=1
        fi
    done
    ((bad == 0)) || die "sys/info is not the 5A-75B's"
    echo "bringup: $ip answers as the 5A-75B"
}

cmd_upload() {
    local ip=$1 seconds=${2:-15} wasm=build/bridge.wasm
    check_ip "$ip"
    [[ $seconds =~ ^[0-9]+$ ]] || die "SECONDS must be a number, not '$seconds'"
    mkdir -p build
    # bridge.wat declares 1-byte pages (custom-page-sizes); mwctl compiles
    # the module to the 5A-75B's image when the board says chip ecp5.
    wasm-tools parse "$ROOT/payloads/templates/bridge.wat" -o "$wasm"
    mwctl --udp "$ip" upload "$wasm" --start
    mwctl --udp "$ip" monitor --seconds "$seconds"
}

cmd_flash() {
    local bit=${1:-build/all/mw5a75b.bit}
    [[ -f $bit ]] || die "no $bit"
    ofl -f "$bit"
    echo "bringup: $bit written to flash; power-cycle the board to boot it"
}

cmd_report() {
    local log
    for log in build/p*-*/build.log; do
        [[ -f $log ]] || continue
        echo "$(dirname "$log"):"
        summary "$log"
    done
}

check_serial
[[ $# -ge 1 ]] || usage
case $1 in
    detect) [[ $# -eq 1 ]] || usage; need_shell "$@"; cmd_detect ;;
    blinky) [[ $# -eq 1 ]] || usage; need_shell "$@"; cmd_blinky ;;
    build) [[ $# -eq 3 ]] || usage; need_shell "$@"; build "$2" "$3" ;;
    sram) [[ $# -eq 3 ]] || usage; need_shell "$@"; cmd_sram "$2" "$3" ;;
    net-check) [[ $# -eq 2 ]] || usage; cmd_net_check "$2" ;;
    upload) [[ $# -eq 2 || $# -eq 3 ]] || usage; need_shell "$@"; cmd_upload "$2" "${3:-15}" ;;
    flash) [[ $# -le 2 ]] || usage; need_shell "$@"; cmd_flash "${2:-}" ;;
    report) [[ $# -eq 1 ]] || usage; cmd_report ;;
    -h | --help | help) usage ;;
    *) usage ;;
esac
