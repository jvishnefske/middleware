"""Builds a Migen module to Verilog and runs a cocotb testbench on it with
Verilator. Each `test_*.py` calls `run()` from pytest; the cocotb test
modules (`tb_*.py`) hold the testbenches."""

import os
import sys
from pathlib import Path

from migen.fhdl.verilog import convert

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent
VECTORS = HERE / "vectors"
sys.path.insert(0, str(ROOT))


def ios_of(module):
    """Every public Signal attribute of `module`: its ports."""
    from migen import Signal
    return {v for k, v in vars(module).items()
            if isinstance(v, Signal) and not k.startswith("_")}


def run(module, name, tb, ios=None, extra_env=None, timeout_s=None, sources=()):
    from cocotb_tools.runner import get_runner
    build = HERE / "sim_build" / name
    build.mkdir(parents=True, exist_ok=True)
    out = convert(module, ios=ios if ios is not None else ios_of(module), name=name)
    out.write(str(build / f"{name}.v"))
    for fn, data in out.data_files.items():   # $readmemh files (ROM contents)
        (build / fn).write_text(data)
    runner = get_runner("verilator")
    runner.build(
        sources=[build / f"{name}.v", *sources],
        includes=sorted({Path(s).parent for s in sources}),
        hdl_toplevel=name,
        build_dir=build,
        always=True,
        build_args=["-Wno-fatal", "-Wno-WIDTH", "-Wno-CASEINCOMPLETE"],
        timescale=("1ns", "1ps"),
    )
    env = {"PYTHONPATH": os.pathsep.join([str(HERE), str(ROOT)])}
    env.update(extra_env or {})
    results = runner.test(
        hdl_toplevel=name,
        test_module=tb,
        build_dir=build,
        test_dir=build,
        extra_env=env,
    )
    from cocotb_tools.check_results import get_results
    total, failed = get_results(results)
    assert failed == 0, f"{failed} of {total} cocotb tests failed ({results})"
    assert total > 0


def ecp5_sim_model():
    """A behavioural DP16KD for a module built with `wasm.ECP5_PRIMITIVES`
    (yosys's cells_sim_ecp5.v only declares it as a blackbox)."""
    return HERE / "dp16kd_x1.v"
