# middleware

A library for embedded Linux, microcontroller and cloud messaging: pub/sub and
request/response behind one set of traits, with pluggable transports.

| feature    | what                                              | builds on                    |
|------------|---------------------------------------------------|------------------------------|
| `ble`      | BLE GATT and BLE advertising                      | `no_std`, no heap (RP2040)   |
| `serial`   | COBS-framed link over any byte stream             | `no_std`, no heap            |
| `espnow`   | ESP-NOW broadcast pub/sub                         | `no_std`, no heap            |
| `zenoh`    | Zenoh pub/sub, get/queryable, liveliness discovery | `std` + tokio               |
| `grpc`     | gRPC unary calls, a bus service, health           | `std` + tokio + tonic        |
| `istio`    | Istio mesh helpers and `grpc::mesh` for `grpc`    | `std`                        |
| `mdns`     | mDNS browse (`MdnsSocket`) and announce (`MdnsAnnouncer`) | `std` + tokio        |
| `dns-sd`   | unicast DNS-SD browse against cluster DNS         | `std` + tokio                |
| `memory`   | in-process backend for tests and examples         | `std` + tokio                |
| `trust`    | signed payload checks (SHA-256, Ed25519 verify)   | `no_std`, no heap            |
| `postcard` | serde codec                                       | `no_std`                     |
| `prost`    | protobuf codec                                    | `no_std` + `alloc`           |

None are on by default. Firmware enables `ble,postcard` or `serial,espnow`;
services enable `zenoh` and/or `grpc,istio,prost`, with `mdns` or `dns-sd`
for discovery. The embedded and host sets share no dependencies.

## What each backend does

The traits are in `transport` (`Publisher`, `Subscriber`, `Requester`,
`Gather`, `Responder`) and `discovery` (`Announce`, `Browse`); every
`Subscriber` declares its `DELIVERY` (what happens when it falls behind).
The rustdoc of each backend is the authority; this is a summary of it.

| backend | pub/sub | `DELIVERY` | `request_with` timeout | `Target::Node` | `Gather` | caller identity (sender / principal) | `Announce` | `Browse` | gaps |
|---|---|---|---|---|---|---|---|---|---|
| `transport::memory` | yes | drop oldest, per-sender order | honoured (tokio timer) | yes | yes | node id / set per node | `MemoryDiscovery` (beacons) | `MemoryDiscovery` | reported |
| `zenoh` | yes | block (back-pressure), per-sender order | query timeout, default 5 s | yes (per-node queryables) | yes | session id / none | `ZenohDiscovery` (liveliness) | `ZenohDiscovery` | reported, per key |
| `grpc` (`GrpcTransport`, `GrpcResponder`) | yes (bus service) | drop oldest, per-sender order | `grpc-timeout` plus local deadline | no (`UnsupportedTarget`) | no | `mw-client` id / SPIFFE ID from the sidecar's XFCC (`istio`, opt-in) | `GrpcAnnouncer` (health `SERVING`) | `MeshBrowse` (wraps a DNS-SD browser, checks health) | reported |
| `grpc::mesh::MeshClient` (`istio`) | no | — | `grpc-timeout`, default 5 s | yes, if the directory record names the node | yes | client id / as above | — | — | — |
| `serial::link` | yes | drop newest (inbox), per-sender order | no clock: caller bounds the call | ignored (one peer) | no | configured peer / none | `PubSubDiscovery` beacons | `PubSubDiscovery` | none |
| `espnow` | yes | drop newest, per-sender order | — (no request/response) | — | no | MAC / none | `PubSubDiscovery` beacons | `PubSubDiscovery` | none |
| `ble` GATT | server notifies, client subscribes | drop newest, per-sender order | no clock: caller bounds the call | ignored (one peer) | no | server sees the link's peer / none | — | — | none |
| `ble::advert` | yes | drop newest, no per-sender order | — | — | no | advertiser address / none | `PubSubDiscovery` beacons | `PubSubDiscovery` | none |
| `mdns` | — | — | — | — | — | — | `MdnsAnnouncer` | `MdnsSocket` (exact kinds) | — |
| `dns::unicast` (`dns-sd`) | — | — | — | — | — | — | — (the Kubernetes Service is the announcement) | `UnicastDnsSd` (exact or prefix) | — |

Neither DNS-SD browser reports a node id, so `Target::Node` through a
`MeshClient` works only for directory records that came with one.

`examples/discover_and_call.rs` puts it together on loopback: one service
kind announced over Zenoh, mDNS and a stub cluster DNS, every event fed
into one `discovery::Directory`, then `request_with` and `Gather` over
Zenoh and the gRPC mesh client
(`cargo run --example discover_and_call --features zenoh,grpc,istio,dns-sd,mdns`).

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
