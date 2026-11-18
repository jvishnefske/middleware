"""CL7: the WASM interpreter against the reference model's vectors."""

from simtools import ecp5_sim_model, run


def test_cl7_wasm_core():
    from mw5a75b.wasm import WasmCore
    c = WasmCore()
    ios = {v for k, v in vars(c).items() if hasattr(v, "nbits") and not k.startswith("_")}
    ios |= set(c.call_args) | set(c.host_args)
    run(c, "wasm_core", "tb_wasm", ios=ios)


def test_cl7_wasm_core_ecp5_memory():
    """The same vectors with the linear memory built from DP16KD blocks, as
    soc.py builds it, against a behavioural DP16KD (tests/dp16kd_x1.v)."""
    from mw5a75b import wasm
    wasm.ECP5_PRIMITIVES = True
    try:
        c = wasm.WasmCore()
    finally:
        wasm.ECP5_PRIMITIVES = False
    ios = {v for k, v in vars(c).items() if hasattr(v, "nbits") and not k.startswith("_")}
    ios |= set(c.call_args) | set(c.host_args)
    run(c, "wasm_core_ecp5", "tb_wasm", ios=ios, sources=[ecp5_sim_model()])
