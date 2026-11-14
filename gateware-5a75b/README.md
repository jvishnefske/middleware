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
| `mw5a75b/node.py` | the two-link node: dispatch, `sys/*` responses, forwarding |
| `mw5a75b/rgmii.py` | LiteEth's RGMII transmitter with one more register stage |
| `soc.py` | PLL, two LiteEth stacks, the node |
| `tests/vectors/` | golden vectors written by `wasm-hdl-ref/tests/vectors.rs` |

Two FT4232Hs are on this host: the FT4232H-56Q MiniModule wired to the
5A-75B (serial `FT4YW8B7`) and the KD240's carrier card (`XFL1HM3KDKK1`),
which must never be driven from here. Every `openFPGALoader` command names
the MiniModule with `--ftdi-serial FT4YW8B7` (`openFPGALoader --scan-usb`
lists both).

Bring-up goes one port at a time (`python soc.py --only-port 0`), with
`--tx-delay0` / `--tx-delay1` (0 to 2 ns) to tune RGMII per port, then both:
ARP and ping at `--ip0` / `--ip1`, then `mwctl --udp <ip> info`.
