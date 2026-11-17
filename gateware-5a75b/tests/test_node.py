"""CL4-CL8: the whole node (two links, system requests, upload, flash,
payload runtime, bridging) driven only through its link streams and a
flash model."""

import os

from simtools import run


def _node():
    from mw5a75b.node import Node
    n = Node(flash_div=2, ms_cycles=50)
    ios = set()
    for link in n.links:
        ios |= set(link.signals())
    s = n.sys
    ios |= {s.flash_cs_n, s.flash_sclk, s.flash_mosi, s.flash_miso, n.status, n.led,
            n.led_claimed, n.button, n.peers[0][0], n.peers[1][0]}
    return n, ios


def test_cl6_upload_requests_follow_the_loader():
    n, ios = _node()
    run(n, "node", "tb_node_upload", ios=ios)


def test_cl8_full_node():
    n, ios = _node()
    run(n, "node", "tb_node_full", ios=ios)
