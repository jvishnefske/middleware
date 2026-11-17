"""LiteEth's ECP5 RGMII transmitter with one more register stage.

On the 5A-75B v7.0 the second PHY's pins (T14, R12, R13, R14, R15) are
far from where the MAC's transmit logic lands, and nextpnr reports that
path routing-bound (about 6.6 ns of routing in an 8 ns cycle) on every
placement seed tried. This copy of `liteeth.phy.ecp5rgmii.LiteEthPHYRGMIITX`
(BSD-2-Clause, Enjoy-Digital) registers `tx_ctl`/`tx_data` once more in
the `eth_tx` domain right before the output DDR flip-flops, so the long
hop gets a whole cycle; the frame is one cycle later on the wire, which
RGMII does not notice. `install()` swaps it into LiteEth before the PHYs
are built.
"""

from migen import Cat, Instance, Signal, ClockSignal
from litex.build.io import DDROutput
from litex.gen import LiteXModule
from litex.soc.interconnect import stream
from liteeth.common import eth_phy_description
import liteeth.phy.ecp5rgmii as ecp5rgmii


class RegisteredRGMIITX(LiteXModule):
    def __init__(self, pads, link_state=None, tx_enable=1):
        self.sink = sink = stream.Endpoint(eth_phy_description(8))

        # # #

        tx_ctl_oddrx1f = Signal()
        tx_data_oddrx1f = Signal(4)
        tx_ctl = Signal(2)
        tx_data = Signal(8)
        tx_ctl_r = Signal(2)
        tx_data_r = Signal(8)

        if link_state is None:
            self.comb += [
                sink.ready.eq(1),
                tx_ctl.eq(Cat(sink.valid, sink.valid)),
                tx_data.eq(sink.data),
            ]
        else:
            self.datapath = datapath = ecp5rgmii.LiteEthRGMIITXDatapath(link_state, tx_enable)
            self.comb += [
                sink.connect(datapath.sink),
                tx_ctl.eq(datapath.tx_ctl),
                tx_data.eq(datapath.tx_data),
            ]
        # The extra stage (this module runs in eth_tx).
        self.sync += tx_ctl_r.eq(tx_ctl), tx_data_r.eq(tx_data)

        self.specials += [
            DDROutput(clk=ClockSignal("eth_tx"), i1=tx_ctl_r[0], i2=tx_ctl_r[1],
                      o=tx_ctl_oddrx1f),
            Instance("DELAYG", p_DEL_MODE="SCLK_ALIGNED", p_DEL_VALUE=0,
                     i_A=tx_ctl_oddrx1f, o_Z=pads.tx_ctl),
        ]
        for i in range(4):
            self.specials += [
                DDROutput(clk=ClockSignal("eth_tx"), i1=tx_data_r[i], i2=tx_data_r[4 + i],
                          o=tx_data_oddrx1f[i]),
                Instance("DELAYG", p_DEL_MODE="SCLK_ALIGNED", p_DEL_VALUE=0,
                         i_A=tx_data_oddrx1f[i], o_Z=pads.tx_data[i]),
            ]


def install():
    ecp5rgmii.LiteEthPHYRGMIITX = RegisteredRGMIITX
