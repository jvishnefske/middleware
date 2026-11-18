#!/usr/bin/env python3
"""The 5A-75B design (CL8): two Ethernet links, each a LiteEth RGMII PHY
with the minimal UDP/ARP/ICMP endpoint of `mw5a75b/net.py` (or, with
`--net liteeth`, LiteEth's MAC/ARP/ICMP/IP/UDP stack), UDP port 7448 into
the middleware node (frame core, system requests, upload and flash store,
WASM payload runtime, bridge engine). No CPU.

    python soc.py                    # build build/mw5a75b/mw5a75b.bit
    python soc.py --ip0 192.168.1.50 --ip1 192.168.2.50
    python soc.py --only-port 0      # one PHY, for per-port bring-up
    python soc.py --net liteeth      # LiteEth's stacks (does not fit with the node)
    python soc.py --no-icmp          # no ping replies

Load to SRAM with `openFPGALoader -c ft4232 --ftdi-serial FT4YW8B7 -m build/mw5a75b/mw5a75b.bit`.

Clocking: a PLL makes the 50 MHz system clock from the 25 MHz input
(LiteEth's system-side IPv4 checksum and ICMP FIFO close timing at about
60 MHz on the -6 part, as litex-boards' Colorlight targets also run, and
50 MHz leaves margin); the node handles one byte per system cycle, so a
link forwards up to 400 Mbit/s;
each PHY's RGMII receive clock drives its own `eth{n}_rx` domain and the
transmit side runs from it (LiteEth's ECP5 RGMII PHY), with `--tx-delay`
(0..2 ns, per port: the v7.0 port 2 may need tuning) applied in the
output delay elements.

Each link's IP and MAC come from the node (`System.net`): the command
line gives the defaults, a `sys/net/addr` setting kept in flash replaces
them at boot. The payload slots and settings live in the configuration
flash past 1 MiB (`mw5a75b/flash.py`); its clock leaves through
`USRMCLK`. The user LED is the payload's GPIO 0 once it claims it, a
1 Hz heartbeat otherwise; the button is GPIO 1.

The B50612D PHYs need no MDIO set-up for 1000BASE-T autonegotiation, so
the shared MDIO bus (`eth_mdio`) is left idle for now: MDC low, MDIO
released. The PHY reset (P5) is not driven.
"""

import argparse

from migen import ClockDomain, ClockDomainsRenamer, Instance, Module, Mux, Signal, TSTriple
from litex.soc.cores.clock import ECP5PLL
from liteeth.core import LiteEthUDPIPCore
from liteeth.phy.ecp5rgmii import LiteEthPHYRGMII

from mw5a75b.net import UdpCore
from mw5a75b.node import PORT, Node
from mw5a75b.platform import Platform
from mw5a75b import rgmii, wasm

rgmii.install()

SYS_CLK = 50e6


class CRG(Module):
    def __init__(self, platform):
        self.clock_domains.cd_sys = ClockDomain()
        clk25 = platform.request("clk25")
        self.submodules.pll = pll = ECP5PLL()
        pll.register_clkin(clk25, 25e6)
        pll.create_clkout(self.cd_sys, SYS_CLK)


class Top(Module):
    def __init__(self, platform, ips, macs, tx_delay, ports=(0, 1), with_sha256=True,
                 net="minimal", with_icmp=True):
        self.submodules.crg = CRG(platform)
        self.submodules.node = node = Node(macs=macs, ips=ips, with_sha256=with_sha256,
                                           ms_cycles=int(SYS_CLK // 1000))

        for n in ports:
            ethphy = ClockDomainsRenamer({"eth_rx": f"eth{n}_rx", "eth_tx": f"eth{n}_tx"})(
                LiteEthPHYRGMII(
                    clock_pads=platform.request("eth_clocks", n),
                    pads=platform.request("eth", n),
                    with_hw_init_reset=False,
                    tx_delay=tx_delay[n],
                ))
            setattr(self.submodules, f"ethphy{n}", ethphy)
            ip = node.sys.net[n].ip
            mac = node.sys.net[n].mac
            link = node.links[n]
            rename = ClockDomainsRenamer({"eth_rx": f"eth{n}_rx", "eth_tx": f"eth{n}_tx"})
            if net == "minimal":
                core = rename(UdpCore(ip, mac, port=PORT, with_icmp=with_icmp))
                setattr(self.submodules, f"ethcore{n}", core)
                rx, tx = ethphy.source, ethphy.sink
                self.comb += [
                    core.phy_rx_valid.eq(rx.valid),
                    core.phy_rx_data.eq(rx.data),
                    core.phy_rx_last.eq(rx.last),
                    rx.ready.eq(1),
                    tx.valid.eq(core.phy_tx_valid),
                    tx.data.eq(core.phy_tx_data),
                    tx.last.eq(core.phy_tx_last),
                    tx.last_be.eq(core.phy_tx_last),
                ]
                self.comb += [getattr(link, s).eq(getattr(core, s)) for s in
                              ("rx_valid", "rx_data", "rx_last", "rx_ip", "rx_port",
                               "tx_ready")]
                self.comb += [getattr(core, s).eq(getattr(link, s)) for s in
                              ("rx_ready", "tx_valid", "tx_data", "tx_last", "tx_ip",
                               "tx_port", "tx_len")]
                continue
            core = rename(
                LiteEthUDPIPCore(ethphy, mac_address=mac, ip_address=ip, clk_freq=SYS_CLK,
                                 with_icmp=with_icmp))
            setattr(self.submodules, f"ethcore{n}", core)
            port = core.udp.crossbar.get_port(PORT, dw=8)
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

        # Configuration flash: payload slots and settings.
        flash = platform.request("spiflash")
        s = node.sys
        self.comb += flash.cs_n.eq(s.flash_cs_n), flash.mosi.eq(s.flash_mosi)
        self.comb += s.flash_miso.eq(flash.miso)
        self.specials += Instance("USRMCLK", i_USRMCLKI=s.flash_sclk, i_USRMCLKTS=0)

        # User LED (active low): the payload's pin 0, else a 1 Hz heartbeat.
        led_n = platform.request("user_led_n")
        beat = Signal(26)
        self.sync += beat.eq(beat + 1)
        self.comb += led_n.eq(~Mux(node.led_claimed, node.led, beat[25]))
        btn_n = platform.request("user_btn_n")
        self.comb += node.button.eq(~btn_n)


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
    p.add_argument("--net", choices=("minimal", "liteeth"), default="minimal",
                   help="network stack per port: mw5a75b/net.py (default) or LiteEth's")
    p.add_argument("--no-icmp", action="store_true", help="no ping replies")
    p.add_argument("--no-sha256", action="store_true",
                   help="leave out the SHA-256 core (the install log carries no digest)")
    a = p.parse_args()
    wasm.ECP5_PRIMITIVES = True     # linear memory as 8 DP16KD blocks
    mac = lambda s: int(s.replace(":", ""), 16)
    platform = Platform()
    ports = (0, 1) if a.only_port is None else (a.only_port,)
    top = Top(platform, ips=(a.ip0, a.ip1), macs=(mac(a.mac0), mac(a.mac1)),
              tx_delay=(a.tx_delay0, a.tx_delay1), ports=ports, with_sha256=not a.no_sha256,
              net=a.net, with_icmp=not a.no_icmp)
    platform.build(top, build_name="mw5a75b", build_dir=a.build_dir,
                   run=not a.no_compile, seed=a.seed)


if __name__ == "__main__":
    main()
