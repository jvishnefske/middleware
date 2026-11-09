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

Trace context (0.6): a W3C `traceparent` and an `x-request-id` set in
`RequestOptions` reach the handler's `RequestMeta` unchanged over the
memory backend, Zenoh (in the query attachment) and gRPC, including every
call of a `MeshClient` gather (as headers). The serial link, ESP-NOW and
BLE carry neither; their handlers see `None`.

The workspace crate `mw-service` (in `service/`) is the mesh service
runner being built for 0.6: a service is declared with
`Service::builder(name)`, and the same declaration is meant to produce its
Kubernetes manifests. Its API and pure parts are in place; serving and
manifest rendering are not implemented yet (see design.md, "Mesh service
runner").

`examples/discover_and_call.rs` puts it together on loopback: one service
kind announced over Zenoh, mDNS and a stub cluster DNS, every event fed
into one `discovery::Directory`, then `request_with` and `Gather` over
Zenoh and the gRPC mesh client
(`cargo run --example discover_and_call --features zenoh,grpc,istio,dns-sd,mdns`).

Requirements and the tests that cover them are listed in `design.md`.
Feature isolation is checked by `scripts/check-features.sh`.

## Robots and the MCU gateway

Two `mw-service` binaries connect things outside the cluster (design.md,
"Robots and MCU gateway").

`mw-robot` (`robot/`) is the Linux robot application. It never handles
TLS: the robot's spire-agent and spiffe-helper give a local Envoy the SVID
`spiffe://infra.local/robot/<name>`, and the application talks plaintext
to that Envoy on loopback, gRPC (h2c) to `127.0.0.1:15001` and its Zenoh
session to `127.0.0.1:17447`. Every tick it calls `demo/echo` with a fresh
trace and publishes its state on `app/robot/<name>/state`. Off cluster
there is no cluster DNS, so called kinds resolve through
`MW_STATIC_ENDPOINTS` (dial address and `:authority` per kind); without
it the robot defaults to the Envoy listener and the kind's
`<name>.apps.svc.cluster.local:50051` authority. Set `MW_ROBOT_NAME`; the
other variables are in the crate docs.

`mw-gateway` (`gateway/`) bridges one serial-attached device running the
ESP base firmware to the Zenoh fabric. The device's `app/...`
publications go to Zenoh under the same keys (or under `app/<ns>/...`
with `MW_GATEWAY_NAMESPACE`), its `disc/<kind>` beacons become Zenoh
discovery records whose node is the device's MAC, and the cluster reaches
`sys/info` and the device log, and nothing else of `sys/...`, through
the gateway's served kind `gw/sys`.

### Running the gateway against the ESP32-CAM (documented, not yet run)

On a workstation with the ESP32-CAM on `/dev/ttyUSB0`, the gateway needs
a Zenoh router to connect to: `MW_ZENOH_ROUTER` names it, for example
the cluster's zenohd reached through a port-forward or the robot Envoy's
`tcp/127.0.0.1:17447`. The port defaults to `/dev/ttyUSB0` at 115200
baud (`MW_GATEWAY_SERIAL`, `MW_GATEWAY_BAUD`); the gateway opens exactly
that path, never searches for ports, and leaves DTR and RTS deasserted
so opening it does not reset the board. Run `cargo run -p mw-gateway`
with those variables set. Close `mwctl monitor` or any other program on
the port first, since only one program can hold it.

On start the gateway asks the device for `sys/info` and binds to the
node id in the reply (the MAC, zero padded). From then on, a
`disc/cam/jpeg` beacon from the device shows up in the cluster as an
`Up` for instance `cam-xxxxxx` (the MAC's last three bytes) with that
node, and it is withdrawn when the camera route stops or the record
expires. Any `ZenohDiscovery` browser on `cam/jpeg` sees it, including a
`Directory` fed from one. Camera chunks published on `app/cam/jpeg` arrive
on Zenoh unchanged.

One gap is known before the first run. Today's firmware sends its camera
beacon only over ESP-NOW (`firmware/src/shell/radio.rs`), and its serial
port carries `sys/log` and payload publications. So the `Up` appears only
once the firmware also publishes the beacon on its serial link, a
firmware change outside this crate. Until then the gateway shows the
device's `sys/log` lines through `gw/sys`, including the `disc: up ...`
lines of beacons the device hears on the radio.

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
