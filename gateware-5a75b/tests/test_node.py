"""CL4/CL5/CL8: the two-link node - sys requests answered, publications
bridged between the links by the rule table."""

from simtools import run


def test_cl8_node():
    from mw5a75b.node import Node
    n = Node()
    ios = set()
    for link in n.links:
        ios |= set(link.signals())
    b = n.bridge
    for name in ["add_start", "add_src", "add_dst", "add_prefix", "add_len", "add_stb",
                 "add_byte", "add_end", "rm_stb", "stat_stb", "stat_dropped", "handle",
                 "cmd_done", "cmd_result"]:
        ios.add(getattr(b, name))
    ios.add(n.status)
    ios |= {n.peers[0][0], n.peers[1][0]}
    run(n, "node", "tb_node", ios=ios)
