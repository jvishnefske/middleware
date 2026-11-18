# gateware-5a75b

Middleware for the Colorlight 5A-75B v7.0 (Lattice LFE5U-25F) as gateware,
with no soft CPU. Each Ethernet port is its own middleware link (a minimal
hardware UDP/ARP/ICMP endpoint over LiteEth's RGMII PHY, UDP port 7448, one
raw serial frame per datagram).
A bridge engine forwards publications between the links by rules that a
WebAssembly payload installs with `route_bridge` (ABI 6). The specification
is design.md, section "Colorlight 5A-75B" (CL1–CL9).

```
nix develop                      # yosys, nextpnr-ecp5, trellis, openFPGALoader,
                                 # Verilator, LiteX/LiteEth, cocotb (flake.nix)
pytest tests                     # every cocotb + Verilator test (golden vectors)
python blinky.py                 # JTAG bring-up: build/blinky/blinky.bit
python soc.py                    # the design: build/mw5a75b/mw5a75b.bit
openFPGALoader -c ft4232 --ftdi-serial FT4YW8B7 --detect          # expect IDCODE 0x41111043
openFPGALoader -c ft4232 --ftdi-serial FT4YW8B7 -m build/mw5a75b/mw5a75b.bit   # load to SRAM
```

| file | what |
|------|------|
| `mw5a75b/platform.py` | v7.0 pins: both `eth` resources, one shared `eth_mdio` |
| `mw5a75b/frame.py` | frame decoder (CRC-16, key check, buffer) and CRC appender |
| `mw5a75b/bridge.py` | the 16-rule bridge table and its matchers |
| `mw5a75b/wasm.py` | the WASM interpreter (the ISA of `wasm-hdl-ref`) |
| `mw5a75b/imports.py` | the import unit and the payload runtime |
| `mw5a75b/node.py` | the two-link node: dispatch, forwarding, deliveries, TX arbitration |
| `mw5a75b/system.py` | system requests, payload lifecycle, events, flash jobs, `sys/log` |
| `mw5a75b/upload.py` | `Loader::step` for begin/chunk/commit, streaming CRC-32 |
| `mw5a75b/sha256.py` | SHA-256 of the uploaded image (one round per cycle) |
| `mw5a75b/installer.py` | the image installer: a host-compiled image into the core's RAMs |
| `mw5a75b/flash.py` | SPI flash master and the slot/settings store |
| `mw5a75b/rgmii.py` | LiteEth's RGMII transmitter with one more register stage |
| `mw5a75b/net.py` | the minimal Ethernet/IPv4/UDP/ARP/ICMP endpoint of a link |
| `soc.py` | PLL, two RGMII PHYs with their endpoints, the node |
| `tests/vectors/` | golden vectors written by `wasm-hdl-ref/tests/vectors.rs` |
| `tests/flashmodel.py` | a behavioural W25Q32 for the testbenches |

Two FT4232Hs are on this host: the FT4232H-56Q MiniModule wired to the
5A-75B (serial `FT4YW8B7`) and the KD240's carrier card (`XFL1HM3KDKK1`),
which must never be driven from here. Every `openFPGALoader` command names
the MiniModule with `--ftdi-serial FT4YW8B7` (`openFPGALoader --scan-usb`
lists both).

Bring-up goes one port at a time (`python soc.py --only-port 0`), with
`--tx-delay0` / `--tx-delay1` (0 to 2 ns) to tune RGMII per port, then both:
ARP and ping at `--ip0` / `--ip1`, then `mwctl --udp <ip> info`.

## Flash layout

The bitstream sits at 0 in the W25Q32 (`openFPGALoader ... -f` writes it
there); the node keeps its state past 1 MiB:

| address | content |
|---------|---------|
| `0x100000` | slot 0 header `[len u32][crc32 u32][seq u32][magic 0x5A75B0A5]` |
| `0x101000` | slot 0 image (16 KiB) |
| `0x105000` | slot 1 header |
| `0x106000` | slot 1 image |
| `0x10A000` | settings: link 0, link 1 (`sys/net/addr` records), magic `0x5A75B0C5` |

An upload erases the slot not in use at `sys/loader/begin`, programs it
after the commit installed, header last (its magic is the last word
written), with the next sequence number. At reset the valid slot with the
higher sequence is copied into the image RAM (CRC-32 checked), installed
and started, so a payload runs without a host. A begin while the previous
image is still being programmed answers `Unavailable` (retry).

## Addresses

`--ip0/--ip1/--mac0/--mac1` are the defaults; `mwctl --udp <ip> net
--link N [--ip A.B.C.D] [--mac M] [--peer A.B.C.D:PORT|learn]` changes a
link (applied after the reply, kept in flash). A link sends publications
to its pinned peer, or else to the last unicast sender of a valid frame.

## Network (`mw5a75b/net.py`)

Each port runs a minimal endpoint instead of LiteEth's MAC/IP stack
(`soc.py --net liteeth` builds that one again). What it does:

* receives frames to the link's MAC or broadcast with a good FCS into a
  2 KiB block-RAM ring (eth_rx, 125 MHz); the system side parses them;
* UDP to port 7448 on the link's IP (IPv4 without options, not
  fragmented) goes to the node with the sender's IP and port;
* answers ARP requests for the link's IP and ICMP echo requests (ping;
  `--no-icmp` leaves that out), pings up to the 1500-byte MTU;
* sends the node's datagrams (TTL 64, DF, UDP checksum 0) from a second
  2 KiB ring, padded, with preamble, FCS and the inter-frame gap.

Neighbours: the endpoint remembers the IP and MAC of the last UDP/ICMP
sender and, separately, of the last ARP packet for its IP. A datagram to
either IP goes to that MAC; a broadcast or multicast IP goes to the
broadcast MAC. A datagram to any other IP is dropped (`tx_miss`) and an
ARP request for that IP goes out instead (at most one per 2^22 cycles,
about 84 ms); its reply fills the ARP entry and later datagrams are sent,
so a configured `sys/net/addr` peer that has never sent anything loses
the datagrams before its ARP reply. Unicast traffic never goes to the
broadcast MAC. Nothing waits for ARP (LiteEth held the datagram for up to
8 × 100 ms of ARP retries and then dropped it, and learned MACs only from
ARP replies). Frames from a group source MAC (or an ARP sender address
that is one) are dropped and never learned. Otherwise the two entries are
as spoofable as ARP itself: any host on the segment can claim an IP with
a UDP datagram, a ping or an ARP packet, as it could poison LiteEth's ARP
cache.

Left out compared to LiteEth: IPv4 header and UDP checksum checks on
receive (the FCS is checked), IP options, fragments, an ARP table with
timeouts, 10/100 Mbit/s (as before, the PHY runs 1000BASE-T only), and
transmit back-pressure from the PHY. When the node is slow to read
(it handles one byte per 50 MHz cycle), the receive ring fills and whole
frames are dropped (`rx_full`); an ARP or ping reply waits while a long
datagram from the node is being written, so bursts of them can be lost.

## Payloads

The board does not compile WebAssembly: the host does, with the
gateware's reference model (`wasm-hdl-ref`), and uploads the compiled
**image** (`wasm-hdl-ref/src/image.rs`: a 28-byte header, then code
words, function table, table 0, globals and data segments). `mwctl
--udp <ip> upload x.wasm` does this when the board reports chip `ecp5`,
and refuses a module the core cannot run with the reason (imports,
export signatures, the i32 subset, sizes) before sending anything; a
file that is not a `.wasm` is sent as it is. The gateware's installer
only checks the image's structure (counts, memory limits, function
indices, segment bounds, exact length) and refuses a bad one with
`load refused: <kind>` on `sys/log` (design.md CL6). The slot format,
`sys/loader/*` and `sys/payload/*` are unchanged; the slot holds the
image.

Linear memory is 16 KiB: declare 1-byte pages, as
`payloads/templates/bridge.wat` does
(`(memory (export "memory") 4096 4096 (pagesize 1))`); mwctl refuses a
module that cannot fit before uploading it.

```
mwctl --udp 192.168.1.50 upload bridge.wasm --start
mwctl --udp 192.168.1.50 monitor          # sys/log, sys/payload/status, bridge/stats
```

## Fitting the 25F

`python usage.py` synthesizes each unit on its own and prints its LUT4 / FF /
carry / LUT-RAM / block RAM use (`build/usage/<unit>/`). `soc.py` builds the
WASM linear memory from eight DP16KD primitives (`wasm.ECP5_PRIMITIVES`):
as a Migen Memory, yosys first could not map its two write ports to block
RAM (131072 flip-flops), and once the ports were exclusive it merged them
and copied the RAM per read port (16 blocks). `tests/test_wasm.py` runs the
core's vectors against both forms (`tests/dp16kd_x1.v` models the block).

Starting point on 2026-11-18, `soc.py --no-sha256`, both ports, with
LiteEth's stacks and the hardware loader: 50/56 DP16KD, 10835/24288 FF,
**34186/24288 TRELLIS_COMB (140%)** in nextpnr. Largest units (COMB, carry
cells counted twice): the two LiteEth UDP/IP stacks with the SoC glue
about 8.5k, the loader 7.5k, the WASM core 5.8k, the bridge 3.3k.

Yosys `stat` of the whole SoC (`--no-sha256`, both ports; COMB estimated
as LUT4 + 2 × CCU2C + 2 × DPR16X4, which is below nextpnr's count):

| build | LUT4 | CCU2C | DPR16X4 | DP16KD | FF | COMB est. |
|-------|-----:|------:|--------:|-------:|---:|----------:|
| LiteEth + loader | 22946 | 3551 | 361 | 50 | 10835 | 30770 |
| LiteEth + installer (host compiles) | 17344 | 2951 | 184 | 50 | 9630 | 23614 |
| minimal network + loader | 21676 | 3243 | 237 | 52 | 9124 | 28636 |

One endpoint alone (`python usage.py net`): 1664 LUT4, 251 CCU2C,
1325 FF, 2 DP16KD (its two rings). The installer alone (`usage.py
installer`, with its 8-block image RAM): 1103 LUT4, 71 CCU2C, 287 FF.
ICMP echo costs next to nothing (`--no-icmp`).

**It fits.** `soc.py --no-sha256`, both ports, with the minimal network,
the installer (host compiles) and the per-unit trims (bounds checks without
full-width carries, binary-encoded FSMs, LUT-RAM tables, shared decoders
and CRC engine; see the git log), after nextpnr (seed 1):

| resource | used | 25F |
|----------|-----:|----:|
| TRELLIS_COMB | 20874 (85%) | 24288 |
| TRELLIS_FF | 8168 (33%) | 24288 |
| DP16KD | 52 (92%) | 56 |

| clock | Fmax | target |
|-------|-----:|-------:|
| sys (`crg_clkout`) | 63.8 MHz | 50 MHz |
| eth0 RGMII (`eth_clocks0_rx`, also clocks transmit) | 145.0 MHz | 125 MHz |
| eth1 RGMII (`eth_clocks1_rx`) | 134.9 MHz | 125 MHz |

Yosys `stat` of that build: 15906 LUT4, 1698 CCU2C, 60 DPR16X4 (before
the 125 MHz pipelining in `net.py`, which added about 170 COMB). SHA-256
(`--no-sha256` left out) does not fit next to it; the install log carries
no digest.
