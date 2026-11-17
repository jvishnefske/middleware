"""CL7: the WASM interpreter against the reference model's vectors."""

from simtools import run


def test_cl7_wasm_core():
    from mw5a75b.wasm import WasmCore
    c = WasmCore()
    ios = {v for k, v in vars(c).items() if hasattr(v, "nbits") and not k.startswith("_")}
    ios |= set(c.call_args) | set(c.host_args)
    run(c, "wasm_core", "tb_wasm", ios=ios)
