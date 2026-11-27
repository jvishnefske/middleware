Check a download against `SHA256SUMS` (`sha256sum -c --ignore-missing SHA256SUMS`).
Nothing here writes fuses or OTP, and nothing is signed: a board that pins a key
takes an update signed at install time (`mwctl fw update --key` or `--pkcs11`).

## Microcontrollers

| Board | File | Install |
|---|---|---|
| Raspberry Pi Pico (RP2040) | `rp-base-rp2040.uf2` | Hold BOOTSEL while plugging in USB, copy the file to the `RPI-RP2` drive; or `picotool load -x -f rp-base-rp2040.uf2` |
| Raspberry Pi Pico 2 (RP2350), first install | `rp-base-rp2350-<V>-factory.uf2` | Through BOOTSEL as above (partition table and slot A); or `picotool load -x` |
| Raspberry Pi Pico 2 (RP2350), update | `rp-base-rp2350-<V>-update.uf2` | `mwctl --port <tty> fw update <file>` on a board already running rp-base |
| MIMXRT1050-EVKB, update | `imxrt-base-<V>.slot` | `mwctl --port <tty> fw update <file>` |
| MIMXRT1050-EVKB, first install | `imxrt-boot.bin` and the `.slot` | Over SWD from a checkout: `python3 firmware-imxrt/tools/flash.py 0x0:imxrt-boot.bin 0x400000:imxrt-base-<V>.slot` |
| BBC micro:bit v2 (nRF52833) | `firmware-nrf52-microbit-v2.hex` | Copy to the `MICROBIT` drive; or `probe-rs download --chip nRF52833_xxAA firmware-nrf52-microbit-v2.elf` |
| ESP32, ESP32-S3, ESP32-C3 | `esp-base-<chip>.bin` | Merged image at offset 0: `espflash write-bin 0x0 esp-base-<chip>.bin` |
| ESP8266 | `esp-base-esp8266.bin` | `esptool --chip esp8266 write_flash 0x0 esp-base-esp8266.bin` |

The `.elf` files are for a debug probe (probe-rs). `esp-base-<chip>.bsp.json`
is the chip's board support package (mw-bsp/1), the same profile the firmware
serves on `sys/board`.

## Linux

| File | What |
|---|---|
| `mwctl-x86_64-linux` | The command-line tool (glibc). `chmod +x` and run. |
| `<name>-x86_64-linux-musl`, `<name>-aarch64-linux-musl` | Static service binaries (`mw-posix-node`, `mw-gateway`, …) and `zenohd`; no dependencies. |
| `mw-x86-base-x86_64.raw.zst` | Bootable UEFI disk of the x86_64 base system: `zstd -dc mw-x86-base-x86_64.raw.zst \| sudo dd of=/dev/sdX bs=4M conv=fsync status=progress` |
| `mw-robot-pi-aarch64.img.zst` | Raspberry Pi Zero 2 W / Pi 3 robot SD image, written the same way. See README, "Raspberry Pi robot image". |
