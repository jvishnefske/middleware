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
