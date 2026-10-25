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
