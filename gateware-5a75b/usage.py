#!/usr/bin/env python3
"""Per-submodule ECP5 resource use: builds each unit of the design on its
own, synthesizes it with `synth_ecp5`, and prints LUT4 / FF / block RAM
counts, so a fit problem points at a unit instead of one flat netlist.

    nix develop . -c python usage.py              # every unit, 4 at a time
    nix develop . -c python usage.py wasm_core loader
    JOBS=2 nix develop . -c python usage.py

Every public Signal anywhere in a unit's module tree becomes a port, so
nothing is pruned for lack of a reader; the numbers are an upper bound
for the unit inside the SoC (shared signals are counted once per unit).
Logs and netlists go to build/usage/<unit>/."""

import os
import re
import subprocess
import sys
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

from migen import Module, Signal
from migen.fhdl.verilog import convert

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))


def _units():
    from mw5a75b.bridge import BridgeTable
    from mw5a75b.flash import Store
    from mw5a75b.frame import CrcAppender, FrameDecoder
    from mw5a75b.imports import ImportUnit
    from mw5a75b.loader import Loader
    from mw5a75b.node import Node
    from mw5a75b.sha256 import Sha256
    from mw5a75b.upload import UploadState
    from mw5a75b.wasm import WasmCore
    from mw5a75b.system import CAPACITY

    # A unit that drives another unit's ports (the loader and the import unit
    # drive the core's) is built with that unit outside its tree: the core's
    # signals become ports and the core's logic is not counted twice.
    return {
        "wasm_core": lambda: WasmCore(),
        "import_unit": lambda: ImportUnit(WasmCore(), None),
        "loader": lambda: Loader(WasmCore()),
        "bridge": lambda: BridgeTable(2),
        "store": lambda: Store(2),
        "upload": lambda: UploadState(CAPACITY),
        "sha256": lambda: Sha256(),
        "frame_decoder": lambda: FrameDecoder(),
        "crc_appender": lambda: CrcAppender(),
        "node": lambda: Node(),
    }


def _signals(module, seen):
    """Public Signals of `module` and its submodules (lists/Arrays included)."""
    out = set()
    if id(module) in seen:
        return out
    seen.add(id(module))
    for k, v in vars(module).items():
        if k.startswith("_"):
            continue
        vals = v if isinstance(v, (list, tuple)) else [v]
        out |= {x for x in vals if isinstance(x, Signal)}
    for _, sub in module._submodules:
        out |= _signals(sub, seen)
    return out


CELLS = ["LUT4", "TRELLIS_FF", "CCU2C", "TRELLIS_DPR16X4", "DP16KD", "PDPW16KD", "MULT18X18D"]


def write(name, build):
    d = HERE / "build" / "usage" / name
    d.mkdir(parents=True, exist_ok=True)
    m = build()
    out = convert(m, ios=_signals(m, set()), name=name)
    out.write(str(d / f"{name}.v"))
    for fn, data in out.data_files.items():
        (d / fn).write_text(data)


def synth(name):
    d = HERE / "build" / "usage" / name
    log = d / "yosys.log"
    r = subprocess.run(["yosys", "-q", "-l", str(log), "-p",
                        f"read_verilog {name}.v; synth_ecp5 -top {name}; tee -o stat.txt stat"],
                       cwd=d, stdout=subprocess.DEVNULL, stderr=subprocess.STDOUT)
    if r.returncode != 0:
        return name, None
    text = (d / "stat.txt").read_text()
    counts = {}
    for c in CELLS:
        hit = re.findall(rf"^\s+(\d+)\s+{c}\s*$", text, re.M)
        counts[c] = int(hit[-1]) if hit else 0
    return name, counts


def main():
    units = _units()
    names = sys.argv[1:] or list(units)
    jobs = int(os.environ.get("JOBS", "4"))
    for n in names:             # Migen conversion is not thread-safe
        write(n, units[n])
    with ThreadPoolExecutor(jobs) as ex:
        results = list(ex.map(synth, names))
    short = ["LUT4", "FF", "CCU2C", "DPR16X4", "DP16KD", "PDPW16KD", "MULT"]
    print(f"{'unit':<14}" + "".join(f"{s:>9}" for s in short))
    for name, counts in results:
        if counts is None:
            print(f"{name:<14} failed: build/usage/{name}/yosys.log")
            continue
        print(f"{name:<14}" + "".join(f"{counts[c]:>9}" for c in CELLS))
    print("LFE5U-25F:      24288    24288        -        -       56 (DP16KD+PDPW16KD)   28")


if __name__ == "__main__":
    main()
