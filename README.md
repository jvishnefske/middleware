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

## Containers and the robot VM (nix)

The flake at the top of the repository builds the service binaries as
static musl executables and wraps each in a minimal OCI image. `nix build
.#hello` gives the binary; `nix build .#image-hello` gives `result`, an
uncompressed docker-archive tarball of the image `localhost/mw/hello:<tag>`,
where the tag is the commit's short hash for a clean tree and `dev` for a
dirty one. The image holds the binary, CA certificates and zoneinfo, no
shell, and runs as uid 65532. Load it with `docker load -i result`,
`podman load -i result`, or on a k3s node with `k3s ctr images import
result` (containerd reads docker-archive tarballs), then run it with
`imagePullPolicy: Never`. `nix build .#image-zenohd` is the Zenoh router,
built from the zenoh release that `Cargo.lock` pins.

Every binary is one line in `nix/binaries.nix`: the attribute name (also
the output name, the installed executable and the image name), the cargo
package and its directory, and either `example` or `bin`, optionally with
`features`. Adding that line is all it takes to get `packages.<name>`,
`packages.image-<name>` and its image check.

### Robot VM

`nix/robot.nix` is a NixOS module for a Linux robot: a SPIRE agent with
the join-token attestor, spiffe-helper keeping the robot's SVID in
`/run/spiffe`, a local Envoy that carries the robot's traffic over mTLS to
the cluster's ingress gateway, and the robot app (`mw-robot`, or the
`hello` placeholder until it lands) running as uid 1000. The app speaks
plain h2c to `127.0.0.1:15001` and Zenoh TCP to `127.0.0.1:17447`; Envoy's
admin is on `127.0.0.1:15000`. The h2c listener keeps the request's
`:authority`, which must match the robot VirtualService's host. The Zenoh
listener opens TLS to the gateway with SNI `zenohd.apps.infra.local`
(`MW_ZENOH_SNI`), so the gateway needs a TLS server for that host that
routes TCP to zenohd.

Nothing site specific or secret is built in. The robot reads it at boot
from `/var/lib/mw-robot`: `join-token` (from `just helm::robot-join <name>
unix:uid:1000`), `bundle.spiffe` (the SPIFFE-format trust bundle from
ConfigMap `spire-system/spire-bundle`), and optionally `robot.env`, whose
`KEY=value` lines override `MW_SPIRE_SERVER_ADDRESS` (default
192.168.66.11), `MW_GATEWAY_ADDRESS` (192.168.66.11), `MW_GATEWAY_PORT`
(30443), `MW_GATEWAY_SNI` (`apps.infra.local`), `MW_ZENOH_GATEWAY_PORT`
(30443) and `MW_ZENOH_SNI`. The SPIRE agent starts only once the token and
bundle are there. The rest boots without them.

`nix run .#robot-vm` boots the module as a QEMU guest with the console on
the terminal (root logs in automatically). The host directory given by
`--state` (default `./robot-state`) becomes the guest's `/var/lib/mw-robot`,
so put the three files there before booting. To put the robot on the lab
network, give it a tap device on the lab bridge, the same way the lab's own
nodes are attached. Create the tap once, as root: `ip tuntap add dev
tap-mwrobot mode tap user $USER`, then `ip link set tap-mwrobot master
br-infra up`. After that, `nix run .#robot-vm -- --tap tap-mwrobot` adds a
second NIC with MAC 52:54:00:66:00:21, which the guest addresses statically
as 192.168.66.21/24. `--bridge br-infra` uses QEMU's bridge helper instead,
which needs the setuid helper and an `allow br-infra` line in
`/etc/qemu/bridge.conf`. `--dry-run` prints the QEMU command line without
running it.

`nix flake check` builds every image and checks its contents, runs clippy
on `mw-service`, builds the VM, and runs `checks.robot-boots`. That is a
NixOS test which boots the robot next to a SPIRE server and an
mTLS-checking stand-in gateway inside the nix sandbox. It follows the
robot from attestation through to an accepted request.

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
