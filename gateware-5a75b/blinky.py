#!/usr/bin/env python3
"""JTAG bring-up design: blinks the user LED at about 1.5 Hz from the 25 MHz
clock. Build with `python blinky.py`, load to SRAM with
`openFPGALoader -c ft4232 --ftdi-serial FT4YW8B7 -m build/blinky/blinky.bit`."""

import argparse

from migen import Module, Signal, ClockDomain

from mw5a75b.platform import Platform


class Blinky(Module):
    def __init__(self, platform):
        clk = platform.request("clk25")
        led_n = platform.request("user_led_n")
        self.clock_domains.cd_sys = ClockDomain(reset_less=True)
        self.comb += self.cd_sys.clk.eq(clk)
        counter = Signal(24)
        self.sync += counter.eq(counter + 1)
        self.comb += led_n.eq(~counter[23])


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--build-dir", default="build/blinky")
    args = p.parse_args()
    platform = Platform()
    platform.build(Blinky(platform), build_name="blinky",
                   build_dir=args.build_dir)


if __name__ == "__main__":
    main()
