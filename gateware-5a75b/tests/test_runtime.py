"""CL7: interpreter + hardware import unit + bridge table against the
reference model's vectors."""

from simtools import run


def test_cl7_runtime():
    from mw5a75b.bridge import BridgeTable
    from mw5a75b.imports import Runtime
    from migen import Module

    class Dut(Module):
        def __init__(self):
            self.submodules.bridge = BridgeTable(2)
            self.submodules.rt = Runtime(bridge=self.bridge)

    d = Dut()
    rt, core, imp, b = d.rt, d.rt.core, d.rt.imports, d.bridge
    ios = set()
    for obj in (rt, core, imp):
        ios |= {v for k, v in vars(obj).items() if hasattr(v, "nbits") and not k.startswith("_")}
    ios |= set(core.call_args) | set(core.host_args)
    m = imp.sub_match[0]
    ios |= {m.start, m.stb, m.idx, m.byte, m.key_len, m.hit, m.slot, imp.sub_count}
    ios |= {b.add_start, b.add_end, b.add_stb, b.add_byte, b.add_src, b.add_dst, b.add_prefix,
            b.add_len, b.rm_stb, b.stat_stb, b.handle, b.cmd_done, b.cmd_result}
    # Inputs driven by the core/imports internally are not top-level inputs.
    for s in (core.call_start, core.call_func, core.call_nargs,
              core.ext_mem_adr, core.ext_mem_dat_w, core.ext_mem_we, *core.call_args,
              b.add_start, b.add_end, b.add_stb, b.add_byte, b.add_src, b.add_dst,
              b.add_prefix, b.add_len, b.rm_stb, b.stat_stb, b.handle,
              imp.has_tick, imp.has_messages, imp.reset_tables):
        ios.discard(s)
    run(d, "runtime", "tb_runtime", ios=ios)
