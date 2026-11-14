"""Helpers shared by the cocotb testbenches."""

import json

import cocotb
from cocotb.clock import Clock
from cocotb.triggers import RisingEdge, ReadOnly

from simtools import VECTORS


def vectors(rel):
    return json.loads((VECTORS / rel).read_text())


async def start(dut, sigs_zero=()):
    cocotb.start_soon(Clock(dut.sys_clk, 8, unit="ns").start())
    for s in sigs_zero:
        getattr(dut, s).value = 0
    dut.sys_rst.value = 1
    await RisingEdge(dut.sys_clk)
    await RisingEdge(dut.sys_clk)
    dut.sys_rst.value = 0
    await RisingEdge(dut.sys_clk)


async def edge(dut):
    await RisingEdge(dut.sys_clk)
