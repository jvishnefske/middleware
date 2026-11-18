# gateware-5a75b

Middleware for the Colorlight 5A-75B v7.0 (Lattice LFE5U-25F) as gateware,
with no soft CPU. Each Ethernet port is its own middleware link (LiteEth's
hardware UDP/IP stack, UDP port 7448, one raw serial frame per datagram).
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
| `mw5a75b/loader.py` | the hardware WASM loader (the model's compiler) |
| `mw5a75b/flash.py` | SPI flash master and the slot/settings store |
| `mw5a75b/rgmii.py` | LiteEth's RGMII transmitter with one more register stage |
| `soc.py` | PLL, two LiteEth stacks, the node |
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
after the commit compiled, header last (its magic is the last word
written), with the next sequence number. At reset the valid slot with the
higher sequence is copied into the image RAM (CRC-32 checked), compiled
and started, so a payload runs without a host. A begin while the previous
image is still being programmed answers `Unavailable` (retry).

## Addresses

`--ip0/--ip1/--mac0/--mac1` are the defaults; `mwctl --udp <ip> net
--link N [--ip A.B.C.D] [--mac M] [--peer A.B.C.D:PORT|learn]` changes a
link (applied after the reply, kept in flash). A link sends publications
to its pinned peer, or else to the last unicast sender of a valid frame.

## Payloads

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

State on 2026-11-18, `soc.py --no-sha256`, both ports: 50/56 DP16KD,
10835/24288 FF, **34186/24288 TRELLIS_COMB (140%)**. It does not fit yet.
Largest units (COMB, carry cells counted twice): the two LiteEth UDP/IP
stacks with the SoC glue about 8.5k, the loader 7.5k, the WASM core 5.8k,
the bridge 3.3k. Timing is unknown until it places.
