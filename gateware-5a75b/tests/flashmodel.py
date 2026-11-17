"""A behavioural W25Q32 for the cocotb testbenches: SPI mode 0, read
(0x03), page program (0x02, wrapping within the 256-byte page, bits only
cleared), 4 KiB sector erase (0x20), write enable (0x06, cleared by every
write), status (0x05: WIP while busy, WEL). Writes without the write
enable or while busy are ignored. Busy times are in system clock cycles,
short to keep simulations fast.

MISO changes one clock after the falling SCK edge is seen, so the master
needs at least two system cycles per SCK half period (`flash_div=2`).

The memory persists across DUT resets (it is the board's flash), so a
test can reset the design and see it boot from what was stored."""

import cocotb
from cocotb.triggers import ReadOnly, RisingEdge

SIZE = 4 * 1024 * 1024


class FlashModel:
    def __init__(self, dut, prefix="flash_", erase_cycles=3000, program_cycles=300):
        self.dut = dut
        self.p = prefix
        self.mem = bytearray(b"\xff" * SIZE)
        self.erase_cycles = erase_cycles
        self.program_cycles = program_cycles
        self.busy = 0
        self.wel = False
        self.writes = []               # ("program" | "erase", address)
        cocotb.start_soon(self.run())

    def s(self, name):
        return getattr(self.dut, self.p + name)

    def erase_all(self):
        self.mem[:] = b"\xff" * SIZE

    def status(self):
        return int(self.busy > 0) | int(self.wel) << 1

    def begin(self):
        self.nbits = 0
        self.shift = 0
        self.cmd = None
        self.addr_bytes = []
        self.addr = 0
        self.page = {}
        self.pg_n = 0
        self.out_next = 0
        self.out_cur = 0

    def end(self):
        """CS rises: a program or erase takes effect."""
        if self.busy:
            return
        if self.cmd == 0x06:
            self.wel = True
        elif self.cmd == 0x02 and self.wel and len(self.addr_bytes) == 3 and self.page:
            for a, v in self.page.items():
                self.mem[a] &= v
            self.busy, self.wel = self.program_cycles, False
            self.writes.append(("program", self.addr))
        elif self.cmd == 0x20 and self.wel and len(self.addr_bytes) == 3:
            a = self.addr & ~0xFFF
            self.mem[a:a + 4096] = b"\xff" * 4096
            self.busy, self.wel = self.erase_cycles, False
            self.writes.append(("erase", a))

    def byte_in(self, b):
        """A whole byte arrived; returns the next byte to shift out."""
        if self.cmd is None:
            self.cmd = b
            return self.status() if b == 0x05 else 0
        if self.cmd == 0x05:
            return self.status()
        if self.cmd not in (0x03, 0x02, 0x20):
            return 0
        if len(self.addr_bytes) < 3:
            self.addr_bytes.append(b)
            if len(self.addr_bytes) < 3:
                return 0
            self.addr = (self.addr_bytes[0] << 16 | self.addr_bytes[1] << 8
                         | self.addr_bytes[2]) % SIZE
            self.rd = self.addr
        elif self.cmd == 0x02:
            a = (self.addr & ~0xFF) | ((self.addr + self.pg_n) & 0xFF)
            self.page[a] = b
            self.pg_n += 1
            return 0
        if self.cmd == 0x03:
            v = 0xFF if self.busy else self.mem[self.rd % SIZE]
            self.rd += 1
            return v
        return 0

    async def run(self):
        dut = self.dut
        miso = self.s("miso")
        miso.value = 0
        prev_cs, prev_sck = 1, 0
        self.begin()
        pending = None
        while True:
            await RisingEdge(dut.sys_clk)
            if pending is not None:
                miso.value = pending
                pending = None
            await ReadOnly()
            if self.busy:
                self.busy -= 1
            cs = int(self.s("cs_n").value)
            sck = int(self.s("sclk").value)
            mosi = int(self.s("mosi").value)
            if cs == 0 and prev_cs == 1:
                self.begin()
            elif cs == 1 and prev_cs == 0:
                self.end()
            elif cs == 0:
                if sck == 1 and prev_sck == 0:
                    self.shift = (self.shift << 1 | mosi) & 0xFF
                    self.nbits += 1
                    if self.nbits % 8 == 0:
                        self.out_next = self.byte_in(self.shift)
                elif sck == 0 and prev_sck == 1:
                    k = self.nbits % 8
                    if k == 0:
                        self.out_cur = self.out_next
                    pending = (self.out_cur >> (7 - k)) & 1
            prev_cs, prev_sck = cs, sck
