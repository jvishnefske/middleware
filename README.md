# middleware

A library for embedded Linux, microcontroller and cloud messaging: pub/sub and
request/response behind one set of traits, with pluggable transports.

| feature    | transport                         | builds on                    |
|------------|-----------------------------------|------------------------------|
| `ble`      | BLE GATT and BLE advertising      | `no_std`, no heap (RP2040)   |
| `zenoh`    | Zenoh pub/sub, get/queryable      | `std` + tokio                |
| `grpc`     | gRPC unary calls and a bus service | `std` + tokio + tonic       |
| `istio`    | Istio mesh helpers for `grpc`     | `std`                        |
| `postcard` | serde codec                       | `no_std`                     |
| `prost`    | protobuf codec                    | `no_std` + `alloc`           |

None are on by default. Firmware enables `ble,postcard`; services enable
`zenoh` and/or `grpc,istio,prost`. The two sets share no dependencies.

Requirements and the tests that cover them are listed in `design.md`.
Feature isolation is checked by `scripts/check-features.sh`.

## Device platform

| path                 | what                                                         |
|----------------------|--------------------------------------------------------------|
| `firmware/`          | ESP base firmware (ESP32, ESP32-S3, ESP32-C3; ESP-NOW for `app/...` keys and DMA routes on ESP32/ESP32-S3): `./build.sh <chip>` |
| `firmware-esp8266/`  | ESP8266 base firmware, protocol only: `./build.sh`, `./flash.sh` |
| `tools/mwctl/`       | host CLI for one serial port: `cargo run -p mwctl -- --port /dev/ttyUSB0 info` |
| `wasm-host/`         | WASM payload runtime (wasmi) implementing `wasm_abi`          |
| `tools/payload-sim/` | run a payload on the desktop: `cargo run -p payload-sim -- x.wasm` |
| `tools/jpeg-fixtures/` | regenerate (or `--check`) the fake camera's baked JPEG frames in `fixtures/cam/` (8 frames at 240×240 and 120×120, baseline 4:2:0): `cargo run -p jpeg-fixtures` |
| `payloads/`          | Rust payload SDK, examples, WAT and AssemblyScript templates |
| `web/`               | flasher, device console/uploader, editor, Blocks visual editor and simulator (`npm run dev`; `web/check.sh`) |

Flash the base firmware once from the web page (Chrome or Edge, Web Serial),
then load payloads over the same USB port, whether it is a CP2102/CH340
bridge or the chip's native USB-Serial-JTAG. The ESP32-C3 and ESP8266 builds
have no WASM runtime (the C3 lacks the atomics wasmi needs; the ESP8266 lacks
the RAM), so they store payloads but cannot run them.
