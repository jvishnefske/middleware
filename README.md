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
Feature isolation is checked by `scripts/check-features.sh`. CI
(`.github/workflows/ci.yml`) runs it, the workspace tests, the RP base
firmware build and the web check on every pull request; the ESP firmwares
need the espup toolchain and are built locally. Pushing a `v*` tag runs
`.github/workflows/release.yml`: it builds every chip's image with
`firmware/build-images.sh`, attaches them, the manifest and `SHA256SUMS` to a
GitHub release, and deploys the web app with those images bundled to GitHub
Pages, where the flasher reads them from its own origin.

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
the cluster's ingress gateway, and the robot app (`mw-robot`) running as
uid 1000. The app speaks
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

### Raspberry Pi image

The same robot runs on a Raspberry Pi Zero 2 W or a Pi 3, which is also
the gateway for one Pico running the RP base firmware on its USB port and
the host of `mw-pi-node`, the Linux payload runner (design.md MS49–MS56).
Each release carries `mw-robot-pi-<tag>.img.zst` and its `.sha256`, built
natively on an arm runner. To build it yourself, `nix build
.#sd-image-robot-pi` on an x86_64 machine cross compiles it (the first
build compiles the base system for aarch64 and takes a long time; the
kernel and Envoy come prebuilt from the NixOS cache), and the same
attribute on an aarch64 machine builds it natively. The image is in
`result/sd-image/`.

To flash it, check the download against its `.sha256` with `sha256sum
-c`, then either open Raspberry Pi Imager, choose "Use custom" and pick
the `.img.zst` file (Imager decompresses it, and its own "OS
customisation" settings do not apply to this image, so leave them off),
or decompress it with `zstd -d` and write the `.img` with `dd` to the
whole card device (for example `/dev/sdX`, not a partition), with
`bs=4M conv=fsync status=progress`. Check the device name twice with
`lsblk` first: `dd` overwrites whatever it is given. The root partition
grows to fill the card on first boot.

The image holds no network, name, key or token. After flashing, the
card's small FAT partition (labelled FIRMWARE) is readable from any
computer: make a folder `mw-robot` on it and put in any of these files.
On every boot the Pi moves them into `/var/lib/mw-robot`, readable by
root only, and deletes them from the FAT partition.

- `wpa_supplicant.conf`, the Wi-Fi networks in wpa_supplicant's own
  format (a `network` block with `ssid` and `psk` per network, and a
  `country=` line for your regulatory domain). Without it Wi-Fi stays
  off; a Pi 3 can use its Ethernet port instead.
- `authorized_keys`, OpenSSH public keys allowed to log in as root.
  Without it SSH does not run; it never accepts passwords, and root has
  none.
- `join-token` and `bundle.spiffe`, the robot's SPIRE join token and the
  trust bundle, exactly as for the robot VM above.
- `robot.env`, `KEY=value` lines. Besides the robot VM's settings it
  takes `MW_ROBOT_NAME`, the gateway's `MW_GATEWAY_*` variables (except
  the port, which is fixed) and a static address: `MW_NET_ADDRESS` in
  CIDR form (for example 192.168.66.30/24), with optional
  `MW_NET_INTERFACE` (default wlan0), `MW_NET_GATEWAY` and `MW_NET_DNS`
  (space separated). Without `MW_NET_ADDRESS` both eth0 and wlan0 use
  DHCP.

Files already on the Pi can be replaced the same way, or over SSH in
`/var/lib/mw-robot`, followed by a reboot.

Plug the Pico in at any time. Udev recognises the RP base firmware by its
USB identity (vendor 2e8a, product 000a on an RP2040 or 0009 on an
RP2350, manufacturer "middleware", product "rp-base"), names its port
`/dev/mw-pico` and starts `mw-gateway` on exactly that port; unplugging
it stops the gateway. Other Pico SDK boards share those ids but not the
strings, so they are left alone. Connect one such board per Pi. The
gateway, the robot app and `mw-pi-node` each run as their own
unprivileged user; only the gateway's is in the `dialout` group.

The Zero 2 W has 512 MiB of RAM, so the image runs Envoy with one worker
thread, caps each service's memory, limits the Go heaps of spire-agent and
spiffe-helper, and keeps half the RAM as compressed swap (zram). The Pi
has no clock battery: certificates are only valid once the network time
has been set, so the robot gets its identity a little after the network
comes up.

`nix flake check` covers the Pi without hardware: `robot-pi-eval`
evaluates both system variants and checks their configuration,
`robot-pi-boots` boots the same role in an x86_64 VM with 512 MiB and no
Pico, provisioning it from a FAT disk, `robot-pi-image` takes the cross
built image apart (and so builds it), and `aarch64-static` checks the
cross compiled binaries.

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
| `firmware-rp/`       | RP base firmware over USB CDC-ACM: Raspberry Pi Pico (RP2040, stores payloads only) and Pico 2 (RP2350, runs them): `./build.sh rp2040` or `./build.sh rp2350` (ELF and UF2) |
| `tools/mwctl/`       | host CLI for one serial port: `cargo run -p mwctl -- --port /dev/ttyUSB0 info`; over the air through the bridge with `--espnow <MAC>` (claimed devices, signed uploads) |
| `tools/espnow-bridge/` | ESP8266 USB↔ESP-NOW bridge sketch for `mwctl --espnow` (`check.sh`: codec test and build; upload with `arduino-cli upload --fqbn esp8266:esp8266:generic -p <port>`) |
| `wasm-host/`         | WASM payload runtime (wasmi) implementing `wasm_abi`          |
| `tools/payload-sim/` | run a payload on the desktop: `cargo run -p payload-sim -- x.wasm` |
| `tools/jpeg-fixtures/` | regenerate (or `--check`) the fake camera's baked JPEG frames in `fixtures/cam/` (8 frames at 240×240 and 120×120, baseline 4:2:0): `cargo run -p jpeg-fixtures` |
| `payloads/`          | Rust payload SDK, examples, WAT and AssemblyScript templates |
| `web/`               | Start guide (try Blink in the simulator, per-board setup), flasher, device console/uploader, editor, Blocks visual editor and simulator (`npm run dev`; `web/check.sh`) |

Flash the base firmware once from the web page (Chrome or Edge, Web Serial),
then load payloads over the same USB port, whether it is a CP2102/CH340
bridge or the chip's native USB-Serial-JTAG. A Pico or Pico 2 takes the UF2
from `firmware-rp/build.sh` in BOOTSEL mode (the web page's Start tab offers
it as a checked download when the site was built after
`firmware/build-images.sh`, which builds the ESP images and the Pico UF2s),
then speaks the same protocol on its USB CDC port. The ESP32-C3, ESP8266 and RP2040 builds have no WASM
runtime (the C3 and the RP2040 lack the atomics wasmi needs; the ESP8266
lacks the RAM), so they store payloads but cannot run them.
