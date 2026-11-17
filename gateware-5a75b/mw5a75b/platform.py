"""The Colorlight 5A-75B v7.0 platform with both Ethernet ports.

litex-boards describes each port with its own `eth` resource, but on v7.0
the two B50612D PHYs share one MDIO bus (MDC on P3, MDIO on T2), so asking
for both resources would claim those balls twice. This platform drops
`mdio`/`mdc` from both `eth` resources and adds one `eth_mdio` resource
for the shared bus. The PHY reset (P5) is common too and is left alone:
the PHYs come out of their power-on reset by themselves.

Payload GPIO 0 is the user LED (P11, active low) and GPIO 1 the user
button (M13, active low); see `board-pins` `COLORLIGHT_5A75B`.
"""

from litex.build.generic_platform import IOStandard, Pins, Subsignal
from litex.build.lattice import LatticeECP5Platform
from litex_boards.platforms import colorlight_5a_75b as base

DEVICE = "LFE5U-25F-6BG256C"
IDCODE = 0x41111043


def _io():
    io = []
    for res in base._io_v7_0:
        name, number, *subs = res
        if name == "eth":
            subs = [s for s in subs
                    if not (isinstance(s, Subsignal) and s.name in ("mdio", "mdc"))]
        io.append((name, number, *subs))
    io.append(("eth_mdio", 0,
               Subsignal("mdio", Pins("T2")),
               Subsignal("mdc", Pins("P3")),
               IOStandard("LVCMOS33")))
    return io


class Platform(LatticeECP5Platform):
    default_clk_name = "clk25"
    default_clk_period = 1e9 / 25e6

    def __init__(self, toolchain="trellis"):
        LatticeECP5Platform.__init__(self, DEVICE, _io(),
                                     connectors=base._connectors_v7_0,
                                     toolchain=toolchain)

    def do_finalize(self, fragment):
        LatticeECP5Platform.do_finalize(self, fragment)
        self.add_period_constraint(self.lookup_request("clk25", loose=True), 1e9 / 25e6)
        for n in (0, 1):
            self.add_period_constraint(
                self.lookup_request("eth_clocks:rx", n, loose=True), 1e9 / 125e6)
