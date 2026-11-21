# The Pico on USB networking (CDC-NCM)

A build with `--features usb-ncm` (`./build.sh rp2350 --features usb-ncm`,
or `rp2040`) is a composite USB device:

| interfaces | function | host driver | what it is |
|---|---|---|---|
| 0, 1 | CDC-ACM | `cdc_acm` | the serial link, still `/dev/serial/by-id/usb-middleware_rp-base_<id>-if00` (mwctl, the gateway) |
| 2, 3 | CDC-NCM | `cdc_ncm` | an Ethernet link to the board |

The NCM link needs no configuration on the board. Everything follows from
the node id (`rp_base::netid`):

- **The host's MAC** is `06:4d:xx:xx:xx:xx`. Linux names the interface
  `enx064d<8 hex digits>`, and udev may leave it as `usb0`.
- **The board's MAC** is `02:4d:` followed by the same four bytes.
- **The board's address** is `169.254.a.b/16`. The boot log (`sys/log`)
  shows it: `net: usb ncm at 169.254.a.b/16, host interface enx064d...`.
- **The mDNS name** is `rp-<node id>.local`, with service
  `rp-<node id>._mw-node._tcp.local`. The TXT records are `chip`,
  `board=pico-usb` or `pico2-usb`, and `fw`.

The host needs a link-local IPv4 address on that interface and mDNS.

## Linux: NetworkManager

Match on the MAC prefix, so the profile catches only these boards:

```
nmcli connection add type ethernet con-name mw-pico ifname '*' \
  match.interface-name 'enx064d*' \
  ipv4.method link-local ipv6.method ignore \
  connection.mdns yes connection.autoconnect yes
```

- `link-local` picks a random 169.254 address and ARP-probes it. This
  coexists with the board's address. If a probe finds the board's address
  taken, the host moves, not the board, because the board does not probe.
- `match.driver cdc_ncm` also works, but it also catches phone tethering.
- If the interface stays `usb0`, match on that instead, or add a udev rule
  that renames it.

### NixOS

```nix
networking.networkmanager.ensureProfiles.profiles.mw-pico = {
  connection = { id = "mw-pico"; type = "ethernet"; autoconnect = true; mdns = 2; };
  match = { interface-name = "enx064d*"; };
  ipv4 = { method = "link-local"; };
  ipv6 = { method = "ignore"; };
};
```

## Linux: systemd-networkd

```
# /etc/systemd/network/50-mw-pico.network
[Match]
Name=enx064d*

[Network]
LinkLocalAddressing=ipv4
MulticastDNS=yes
```

## Finding the board

```
avahi-browse -rt _mw-node._tcp           # lists rp-<id> with its TXT and address
avahi-resolve -n rp-<id>.local
arping -I enx064d... 169.254.a.b         # no ICMP echo: ping does not answer
```

- If Avahi has `allow-interfaces` set in `avahi-daemon.conf`, check that
  it includes the interface.
- With two boards on one host, both are in 169.254/16 on different
  interfaces. mDNS works per interface. For unicast, name the interface:
  `169.254.a.b%enx064d...`.

## Behaviour

- The record is announced only while the host has the NCM interface up.
  Unplugging the board, or downing the interface, withdraws the record.
  When the link comes back, the board probes again (about 750 ms) and
  re-announces.
- The serial link is not affected: mwctl works over the ACM port whether
  or not the NCM interface is up.

## Other hosts

- **macOS:** no profile is needed, because it falls back to 169.254 by
  itself. CDC-NCM support there is not yet checked on hardware.
- **Windows:** NCM works on Windows 11 only.
- **Android:** a locally administered MAC breaks Ethernet tethering, so
  Android is not a target.
