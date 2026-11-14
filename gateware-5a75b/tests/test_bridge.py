"""CL5: the bridge engine against the BridgeTable golden vectors."""

from simtools import run


def test_cl5_bridge_table():
    from mw5a75b.bridge import BridgeTable
    t = BridgeTable()
    ios = {getattr(t, n) for n in vars(t) if not n.startswith("_")
           and hasattr(getattr(t, n), "nbits")}
    for lst in (t.m_start, t.m_stb, t.m_idx, t.m_byte, t.m_end, t.m_len,
                t.m_valid, t.m_links, t.m_rules):
        ios |= set(lst)
    ios.discard(t.valid)
    run(t, "bridge_table", "tb_bridge", ios=ios)
