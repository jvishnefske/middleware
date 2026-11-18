"""CL8: the minimal Ethernet/IPv4/UDP/ARP/ICMP endpoint (`mw5a75b/net.py`)."""

from migen import Module, Signal

from simtools import ios_of, run


class _Top(Module):
    def __init__(self, with_icmp=True):
        from mw5a75b.net import UdpCore
        self.ip = Signal(32)
        self.mac = Signal(48)
        self.submodules.core = UdpCore(self.ip, self.mac, arp_gap=2000, with_icmp=with_icmp)


def test_cl8_minimal_udp_endpoint():
    t = _Top()
    run(t, "net", "tb_net", ios=ios_of(t.core) | {t.ip, t.mac})


def test_cl8_minimal_udp_endpoint_without_icmp():
    t = _Top(with_icmp=False)
    run(t, "net_noicmp", "tb_net", ios=ios_of(t.core) | {t.ip, t.mac},
        extra_env={"NET_ICMP": "0"})


class _NodeTop(Module):
    """`soc.py --only-port 0` without the PHY and the pads."""

    def __init__(self):
        from mw5a75b.net import UdpCore
        from mw5a75b.node import Node, PORT
        self.submodules.node = node = Node(flash_div=2, ms_cycles=50)
        net = node.sys.net[0]
        self.submodules.core = core = UdpCore(net.ip, net.mac, port=PORT, arp_gap=2000)
        link = node.links[0]
        self.comb += [getattr(link, s).eq(getattr(core, s)) for s in
                      ("rx_valid", "rx_data", "rx_last", "rx_ip", "rx_port", "tx_ready")]
        self.comb += [getattr(core, s).eq(getattr(link, s)) for s in
                      ("rx_ready", "tx_valid", "tx_data", "tx_last", "tx_ip", "tx_port",
                       "tx_len")]
        self.comb += node.links[1].tx_ready.eq(1)
        s = node.sys
        self.ios = ({core.phy_rx_valid, core.phy_rx_data, core.phy_rx_last, core.phy_tx_valid,
                     core.phy_tx_data, core.phy_tx_last, core.rx_full,
                     s.flash_cs_n, s.flash_sclk, s.flash_mosi, s.flash_miso, node.button,
                     node.status})


def test_cl8_node_over_ethernet():
    t = _NodeTop()
    run(t, "net_node", "tb_net_node", ios=t.ios)
