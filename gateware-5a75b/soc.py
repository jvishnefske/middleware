#!/usr/bin/env python3
"""The 5A-75B design (CL8): two Ethernet links, each with LiteEth's
hardware MAC/ARP/ICMP/IP/UDP stack, UDP port 7448 into the middleware
node (frame core, system requests, bridge engine). No CPU.

    python soc.py                    # build build/mw5a75b/mw5a75b.bit
    python soc.py --ip0 192.168.1.50 --ip1 192.168.2.50
    python soc.py --only-port 0      # one PHY, for per-port bring-up

Load to SRAM with `openFPGALoader -c ft4232 -m build/mw5a75b/mw5a75b.bit`.

Clocking: a PLL makes the 50 MHz system clock from the 25 MHz input
(LiteEth's system-side IPv4 checksum and ICMP FIFO close timing at about
60 MHz on the -6 part, as litex-boards' Colorlight targets also run, and
50 MHz leaves margin); the node handles one byte per system cycle, so a
link forwards up to 400 Mbit/s;
each PHY's RGMII receive clock drives its own `eth{n}_rx` domain and the
transmit side runs from it (LiteEth's ECP5 RGMII PHY), with `--tx-delay`
(0..2 ns, per port: the v7.0 port 2 may need tuning) applied in the
output delay elements.

The B50612D PHYs need no MDIO set-up for 1000BASE-T autonegotiation, so
the shared MDIO bus (`eth_mdio`) is left idle for now: MDC low, MDIO
released. The PHY reset (P5) is not driven.
"""

import argparse

from migen import ClockDomain, ClockDomainsRenamer, Module, Signal, TSTriple
from litex.soc.cores.clock import ECP5PLL
from liteeth.core import LiteEthUDPIPCore
from liteeth.phy.ecp5rgmii import LiteEthPHYRGMII

from mw5a75b.node import PORT, Node
from mw5a75b.platform import Platform
from mw5a75b import rgmii

rgmii.install()

SYS_CLK = 50e6


class CRG(Module):
    def __init__(self, platform):
        self.clock_domains.cd_sys = ClockDomain()
        clk25 = platform.request("clk25")
        self.submodules.pll = pll = ECP5PLL()
        pll.register_clkin(clk25, 25e6)
        pll.create_clkout(self.cd_sys, SYS_CLK)


def _ip(s):
    a = [int(x) for x in s.split(".")]
    return a[0] << 24 | a[1] << 16 | a[2] << 8 | a[3]


class Top(Module):
    def __init__(self, platform, ips, macs, tx_delay, ports=(0, 1)):
        self.submodules.crg = CRG(platform)
        self.submodules.node = node = Node(macs=macs)

        for n in ports:
            ethphy = ClockDomainsRenamer({"eth_rx": f"eth{n}_rx", "eth_tx": f"eth{n}_tx"})(
                LiteEthPHYRGMII(
                    clock_pads=platform.request("eth_clocks", n),
                    pads=platform.request("eth", n),
                    with_hw_init_reset=False,
                    tx_delay=tx_delay[n],
                ))
            setattr(self.submodules, f"ethphy{n}", ethphy)
            ip = Signal(32, reset=_ip(ips[n]), name=f"ip{n}")
            mac = Signal(48, reset=macs[n], name=f"mac{n}")
            core = ClockDomainsRenamer({"eth_rx": f"eth{n}_rx", "eth_tx": f"eth{n}_tx"})(
                LiteEthUDPIPCore(ethphy, mac_address=mac, ip_address=ip, clk_freq=SYS_CLK,
                                 with_icmp=True))
            setattr(self.submodules, f"ethcore{n}", core)
            port = core.udp.crossbar.get_port(PORT, dw=8)
            link = node.links[n]
            rx, tx = port.source, port.sink
            self.comb += [
                link.rx_valid.eq(rx.valid),
                link.rx_data.eq(rx.data),
                link.rx_last.eq(rx.last),
                link.rx_ip.eq(rx.ip_address),
                link.rx_port.eq(rx.src_port),
                rx.ready.eq(link.rx_ready),
                tx.valid.eq(link.tx_valid),
                tx.data.eq(link.tx_data),
                tx.last.eq(link.tx_last),
                tx.last_be.eq(link.tx_last),
                tx.src_port.eq(PORT),
                tx.dst_port.eq(link.tx_port),
                tx.ip_address.eq(link.tx_ip),
                tx.length.eq(link.tx_len),
                link.tx_ready.eq(tx.ready),
            ]
        for n in set((0, 1)) - set(ports):
            # The unused link never receives and drops what it would send.
            link = node.links[n]
            self.comb += link.tx_ready.eq(1)

        # Shared MDIO bus left idle.
        mdio = platform.request("eth_mdio")
        t = TSTriple()
        self.specials += t.get_tristate(mdio.mdio)
        self.comb += mdio.mdc.eq(0), t.oe.eq(0), t.o.eq(0)

        # Heartbeat on the user LED (active low), about 1 Hz.
        led_n = platform.request("user_led_n")
        beat = Signal(27)
        self.sync += beat.eq(beat + 1)
        self.comb += led_n.eq(~beat[26])


def main():
    p = argparse.ArgumentParser(description=__doc__,
                                formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--ip0", default="192.168.1.50")
    p.add_argument("--ip1", default="192.168.2.50")
    p.add_argument("--mac0", default="02:5a:75:b0:00:00")
    p.add_argument("--mac1", default="02:5a:75:b0:00:01")
    p.add_argument("--tx-delay0", type=float, default=2e-9)
    p.add_argument("--tx-delay1", type=float, default=2e-9)
    p.add_argument("--only-port", type=int, choices=(0, 1))
    p.add_argument("--build-dir", default="build/mw5a75b")
    p.add_argument("--no-compile", action="store_true", help="write the Verilog only")
    p.add_argument("--seed", type=int, default=1, help="nextpnr placement seed")
    a = p.parse_args()
    mac = lambda s: int(s.replace(":", ""), 16)
    platform = Platform()
    ports = (0, 1) if a.only_port is None else (a.only_port,)
    top = Top(platform, ips=(a.ip0, a.ip1), macs=(mac(a.mac0), mac(a.mac1)),
              tx_delay=(a.tx_delay0, a.tx_delay1), ports=ports)
    platform.build(top, build_name="mw5a75b", build_dir=a.build_dir,
                   run=not a.no_compile, seed=a.seed)


if __name__ == "__main__":
    main()
