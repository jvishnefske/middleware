"""CL6 + CL7: the installer streams each vector's image (the host's
compiled module) into the core; what it builds must equal the model's,
and the script must then run as the model says. Refused images fail with
the model's error kind."""

from simtools import run


def test_cl6_installer_and_runtime():
    from migen import If, Module, Mux, Signal
    from mw5a75b.bridge import BridgeTable
    from mw5a75b.imports import Runtime
    from mw5a75b.installer import Installer

    class Dut(Module):
        def __init__(self):
            self.submodules.bridge = BridgeTable(2)
            self.submodules.rt = rt = Runtime(bridge=self.bridge)
            self.submodules.ld = ld = Installer(rt.core)
            self.tb_mem_sel = Signal()
            self.tb_mem_adr = Signal(14)
            self.comb += [
                rt.ld_mem_sel.eq(Mux(ld.busy, ld.mem_sel, self.tb_mem_sel)),
                rt.ld_mem_adr.eq(Mux(ld.busy, ld.mem_adr, self.tb_mem_adr)),
                rt.ld_mem_dat.eq(ld.mem_dat),
                rt.ld_mem_we.eq(ld.busy & ld.mem_we),
                rt.exp_init.eq(ld.exp_init), rt.exp_tick.eq(ld.exp_tick),
                rt.exp_alloc.eq(ld.exp_alloc), rt.exp_on_message.eq(ld.exp_on_message),
                rt.core.ld_mem_len.eq(ld.mem_len),
                rt.core.ld_mem_len_we.eq(ld.done),
            ]

    d = Dut()
    rt, core, imp, b, ld = d.rt, d.rt.core, d.rt.imports, d.bridge, d.ld
    ios = {d.tb_mem_sel, d.tb_mem_adr, rt.mem_rd_dat}
    for name in ["ev_reset", "ev_init", "ev_tick", "ev_deliver", "dlv_slot", "dlv_len",
                 "dlv_base", "dlv_adr", "dlv_dat", "ev_done", "ev_result", "poisoned"]:
        ios.add(getattr(rt, name))
    for name in ["now_ms", "node_id", "peer_ok", "pub_valid", "pub_data", "pub_last",
                 "pub_ready", "pub_len", "button", "sub_count"]:
        ios.add(getattr(imp, name))
    m = imp.sub_match[0]
    ios |= {m.start, m.stb, m.idx, m.byte, m.key_len, m.hit, m.slot}
    ios |= {b.add_start, b.add_end, b.add_stb, b.add_byte, b.add_src, b.add_dst, b.add_prefix,
            b.rm_stb, b.stat_stb, b.stat_dropped, b.handle}
    ios |= {core.executed, core.mem_len, core.glob_rd_adr, core.glob_rd_dat,
            ld.dbg_code_adr, core.dbg_code_dat, core.dbg_func_adr, core.dbg_func_dat,
            core.dbg_table_adr, core.dbg_table_dat, core.fuel_per_call}
    ios |= {ld.img_adr, ld.img_dat, ld.img_we, ld.img_len, ld.start, ld.done, ld.error,
            ld.busy, ld.code_len, ld.nfuncs, ld.table_len, ld.mem_len, ld.mem_max,
            ld.page_log2}
    ios |= set(ld.exp)
    run(d, "system", "tb_system", ios=ios)
