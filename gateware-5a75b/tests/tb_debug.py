"""Debug helper: logs FSM state changes of named internal registers."""
import cocotb
from cocotb.triggers import ReadOnly, RisingEdge


def watch(dut, names):
    async def run():
        prev = {}
        while True:
            await RisingEdge(dut.sys_clk)
            await ReadOnly()
            for n in names:
                try:
                    v = int(getattr(dut, n).value)
                except Exception:
                    continue
                if prev.get(n) != v:
                    dut._log.info("%s = %d", n, v)
                    prev[n] = v
    cocotb.start_soon(run())
