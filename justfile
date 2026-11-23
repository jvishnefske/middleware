set positional-arguments

# Developer shortcuts. `just` lists them.

# List the recipes.
default:
    @just --list

# mwctl scout (H4): Zenoh multicast scouting (UDP 224.0.0.224:7446) for
# `seconds`, then each router's and peer's sessions from its admin space,
# then every node's sys/info.
# Ethernet and Wi-Fi nodes on this subnet appear when they run full Zenoh
# (zenohd, peers with scouting on) or are connected to a router that does.
# Not seen:
#   - ESP32/ESP8266 nodes on ESP-NOW (not IP): ask a gateway's `mwctl ... peers`;
#   - an RT1050 (zenoh_lite client, no scouting) unless its sys/zenoh/endpoint
#     is a router this scan finds;
#   - a zenohd bound to loopback with scouting off, like the KD240's (lathe
#     repo kd240/nix/mesh.nix);
#   - other subnets (multicast stays on the link) and firewalled UDP 7446.
[doc("List the middleware nodes on the local network (Zenoh scouting)")]
lan seconds="3":
    cargo run --quiet --release -p mwctl --features zenoh -- scout --seconds {{seconds}}

# Zenoh scouting finds the nodes that speak Zenoh; DNS-SD finds the ones that
# announce themselves on the link (posix-node: _mw-node._tcp, with its port).
[doc("List middleware devices on the network: Zenoh scouting and DNS-SD (_mw-node._tcp)")]
devices seconds="3":
    just lan {{seconds}}
    python3 -I scripts/mdns-browse.py --seconds {{seconds}}
    just provision --list --no-lan --no-mdns

# The updates channel end to end on this machine (design.md UP): the
# zenohd and mw-updates images (podman), a posix-node over a PTY, the
# updater and the publisher; publish, install, re-publish, refuse a forgery.
[doc("Demo the updates channel locally (podman, socat)")]
updates-demo:
    scripts/updates-demo.sh

# Hardware-in-the-loop (tests/hil): every supported board found on USB
# (rp-base, ESP USB-JTAG/UART bridges, the EVKB's OpenSDA VCOM, BOOTSEL
# Picos) and the LAN (Zenoh scouting, MW_HIL_LAN), checked against this
# middleware: sys/info, the ABI, trust, manifest, board, fw info.
# --mw-write adds a payload upload/start/stop; --mw-flash [--mw-build]
# first installs the current firmware through each board's bootloader.
[doc("Check the middleware against every attached board (pytest)")]
hil *args:
    python3 -m pytest tests/hil {{args}}

# ---- provisioning: the current base firmware through each board's
# bootloader (ESP ROM, RP BOOTSEL or A/B partitions, the RT1050's
# HyperFlash bootloader). Never fuses or OTP (design.md NF1-NF4).
# Claimed boards sign with MW_HIL_KEY (key file) or MW_HIL_PKCS11 (label).

[doc("Provision every attached board that needs the current firmware")]
provision *args:
    python3 tests/hil/provision.py {{args}}

# Remove a board's pinned key. Needs a target: an index from the list (run
# with none to print it) or a name (tty, host, USB serial, node, chip).
[doc("Unprovision one board (by index or name; no target lists the boards)")]
unprovision *target:
    python3 tests/hil/unprovision.py {{target}}

[doc("Build the base firmware for one chip")]
fw-build chip:
    #!/usr/bin/env bash
    set -euo pipefail
    case "{{chip}}" in
      esp32|esp32s3|esp32c3) firmware/build.sh {{chip}} ;;
      esp8266) firmware-esp8266/build.sh ;;
      rp2040|rp2350) firmware-rp/build.sh {{chip}} ;;
      rt1050|imxrt1052) firmware-imxrt/build.sh app ;;
      nrf52833) (cd firmware-nrf52 && cargo build --release) ;;
      *) echo "fw-build: unknown chip {{chip}}" >&2; exit 2 ;;
    esac

[doc("Flash an ESP32/ESP32-S3/ESP32-C3 through its ROM (espflash)")]
provision-esp chip port:
    #!/usr/bin/env bash
    set -euo pipefail
    case "{{chip}}" in
      esp32) t=xtensa-esp32-none-elf ;;
      esp32s3) t=xtensa-esp32s3-none-elf ;;
      esp32c3) t=riscv32imc-unknown-none-elf ;;
      *) echo "provision-esp: not an ESP32 chip: {{chip}}" >&2; exit 2 ;;
    esac
    espflash flash --chip {{chip}} --port {{port}} --non-interactive --skip-update-check \
      --partition-table firmware/partitions.csv firmware/target/$t/release/esp-base
    # The ROM prints its boot mode after the reset; DOWNLOAD means IO0 was low.
    python3 tests/hil/esp_boot.py {{port}}

[doc("Flash an ESP8266 through its ROM (esptool)")]
provision-esp8266 port:
    PORT={{port}} firmware-esp8266/flash.sh

[doc("Flash a Pico (RP2040) through BOOTSEL (picotool; forces BOOTSEL)")]
provision-rp2040 serial="":
    #!/usr/bin/env bash
    set -euo pipefail
    serial="$1"
    if [[ -n "$serial" && ! "$serial" =~ ^[0-9A-Za-z]{1,64}$ ]]; then echo "!!! serial $serial: expected 1-64 letters or digits" >&2; exit 1; fi
    if lsusb -d 2e8a:0003 >/dev/null 2>&1; then just _usb-writable 2e8a:0003; fi
    ser=(); [[ -n "$serial" ]] && ser=(--ser "$serial")
    picotool load -x -f "${ser[@]}" firmware-rp/target/thumbv6m-none-eabi/release/rp-base-rp2040.uf2

# A Pico 2 running rp-base with A/B partitions takes the update image over
# the protocol (`mwctl fw update`, port "-" when it sits in BOOTSEL);
# otherwise the factory image (partition table + slot A) goes in through
# BOOTSEL, which picotool enters with the firmware's USB reset request.
[doc("Install the current firmware on a Pico 2 (RP2350)")]
provision-rp2350 port="-" serial="":
    #!/usr/bin/env bash
    set -euo pipefail
    port="$1"; serial="$2"
    if [[ -n "$serial" && ! "$serial" =~ ^[0-9A-Za-z]{1,64}$ ]]; then echo "!!! serial $serial: expected 1-64 letters or digits" >&2; exit 1; fi
    out=firmware-rp/target/thumbv8m.main-none-eabihf/release
    mwctl="${MWCTL:-target/release/mwctl}"
    sign=(); [[ -n "${MW_HIL_PKCS11:-}" ]] && sign=(--pkcs11 "$MW_HIL_PKCS11")
    [[ -z "${MW_HIL_PKCS11:-}" && -n "${MW_HIL_KEY:-}" ]] && sign=(--key "$MW_HIL_KEY")
    ser=(); [[ -n "$serial" ]] && ser=(--ser "$serial")
    update="$(ls -t $out/rp-base-rp2350-*-update.uf2 | head -1)"
    factory="$(ls -t $out/rp-base-rp2350-*-factory.uf2 | head -1)"
    if [[ "$port" != "-" ]] && "$mwctl" --port "$port" fw info >/dev/null 2>&1; then
      exec "$mwctl" --port "$port" fw update "$update" "${sign[@]}"
    fi
    if ! lsusb -d 2e8a:000f >/dev/null 2>&1; then
      echo "no A/B firmware answering on $port: factory image through BOOTSEL"
      picotool load -x -f "${ser[@]}" "$factory" 2>/dev/null && exit 0
      # rp-base has no USB reset interface, so BOOTSEL needs the button.
      echo ">>> hold BOOTSEL on the Pico 2 and replug its USB (waiting ${BOOTSEL_WAIT:-120} s)"
      for _ in $(seq "${BOOTSEL_WAIT:-120}"); do
        lsusb -d 2e8a:000f >/dev/null 2>&1 && break
        sleep 1
      done
      if ! lsusb -d 2e8a:000f >/dev/null 2>&1; then
        echo "!!! no Pico 2 in BOOTSEL appeared: hold BOOTSEL while plugging in USB, then rerun" >&2
        exit 1
      fi
      sleep 1
    fi
    just _usb-writable 2e8a:000f
    picotool load -x "$factory"

[doc("Install the current firmware on the RT1050-EVKB (mwctl fw update)")]
provision-rt1050 port slot="":
    #!/usr/bin/env bash
    set -euo pipefail
    port="$1"; slot="$2"
    mwctl="${MWCTL:-target/release/mwctl}"
    sign=(); [[ -n "${MW_HIL_PKCS11:-}" ]] && sign=(--pkcs11 "$MW_HIL_PKCS11")
    [[ -z "${MW_HIL_PKCS11:-}" && -n "${MW_HIL_KEY:-}" ]] && sign=(--key "$MW_HIL_KEY")
    slot="${slot:-${MW_HIL_RT_SLOT:-$(ls -t firmware-imxrt/target/thumbv7em-none-eabihf/release/imxrt-base-*.slot | head -1)}}"
    "$mwctl" --port "$port" fw update "$slot" "${sign[@]}"

# Fails, saying what to run, when the USB device VID:PID is not writable
# by this user (picotool then reports only "unable to connect").
_usb-writable id:
    #!/usr/bin/env bash
    set -euo pipefail
    read -r bus dev < <(lsusb -d {{id}} | awk '{print $2, substr($4,1,3); exit}')
    node=/dev/bus/usb/$bus/$dev
    if [[ ! -w $node ]]; then
      echo "!!! $node ({{id}}) is not writable by $USER: picotool cannot open it." >&2
      echo "!!! fix once: just udev-rules   (sudo; then unplug and replug the board)" >&2
      exit 1
    fi

# A Linux root (design.md, "Linux A/B root (OS)") onto the KD240 over the
# LAN: written into the inactive slot, committed with its Os cert, tried, confirmed.
[doc("Install a Linux root release on the KD240 over the LAN: just os-update <image> <cert> <version>")]
os-update image cert version:
    python3 tests/hil/os_update.py {{image}} {{cert}} {{version}}

# The KD240 (Kria, linux-aarch64) on this machine: its console over USB, then
# the board itself over the LAN, asked for info and board.
[doc("Ask the KD240 (USB console or LAN) for info and board; os info once the root is updatable")]
kd240:
    python3 tests/hil/kd240.py

# udev rule for the RP boards (BOOTSEL 2e8a:0003 RP2040, 2e8a:000f RP2350,
# and rp-base 2e8a:0009) and the ESP USB-JTAG: group plugdev, read-write.
[doc("Install udev rules so picotool/espflash work without sudo (uses sudo)")]
udev-rules:
    printf '%s\n' \
      'SUBSYSTEM=="usb", ATTR{idVendor}=="2e8a", ATTR{idProduct}=="0003", MODE="0660", GROUP="plugdev", TAG+="uaccess"' \
      'SUBSYSTEM=="usb", ATTR{idVendor}=="2e8a", ATTR{idProduct}=="000f", MODE="0660", GROUP="plugdev", TAG+="uaccess"' \
      'SUBSYSTEM=="usb", ATTR{idVendor}=="2e8a", ATTR{idProduct}=="0009", MODE="0660", GROUP="plugdev", TAG+="uaccess"' \
      'SUBSYSTEM=="usb", ATTR{idVendor}=="303a", ATTR{idProduct}=="1001", MODE="0660", GROUP="plugdev", TAG+="uaccess"' \
      'SUBSYSTEM=="usb", ATTR{idVendor}=="0403", ATTR{idProduct}=="6011", ATTR{product}=="KD Carrier Card", MODE="0660", GROUP="plugdev", TAG+="uaccess"' \
      | sudo tee /etc/udev/rules.d/60-middleware-boards.rules
    sudo udevadm control --reload-rules
    sudo udevadm trigger --subsystem-match=usb

[doc("Install the current firmware on a micro:bit v2 (nRF52833) over its SWD probe")]
provision-nrf52 probe_serial:
    #!/usr/bin/env bash
    set -euo pipefail
    probe_serial="$1"
    if [[ ! "$probe_serial" =~ ^[0-9A-Za-z]{1,64}$ ]]; then echo "!!! probe serial $probe_serial: expected 1-64 letters or digits" >&2; exit 1; fi
    elf=firmware-nrf52/target/thumbv7em-none-eabihf/release/firmware-nrf52
    if [[ ! -f "$elf" ]]; then echo "!!! $elf not built: run just fw-build nrf52833" >&2; exit 1; fi
    # Only flash and RAM may be written. A section at 0x1000_0000 or above is
    # UICR/FICR/OTP-class memory: refuse it (design.md NF1-NF4).
    chk='{ for (i=1;i<=NF;i++) if ($i ~ /^[0-9a-fA-F]{8}$/) { if ($i ~ /^1[0-9a-fA-F]{7}$/) bad=1; break } } END { exit !bad }'
    if readelf -SW "$elf" | grep -E '^ *\[' | awk "$chk"; then
      echo "!!! $elf has a section at 0x10000000 or above (UICR/FICR): refusing to flash" >&2; exit 1
    fi
    probe="0d28:0204:$probe_serial"
    probe-rs download --probe "$probe" --chip nRF52833_xxAA --binary-format elf "$elf"
    probe-rs reset --probe "$probe" --chip nRF52833_xxAA
