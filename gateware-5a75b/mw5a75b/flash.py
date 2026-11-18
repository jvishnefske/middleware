"""Payload and settings storage in the configuration SPI flash (CL6, CL8).

The 5A-75B's W25Q32 (4 MiB) holds the bitstream from 0; the node uses
the space past 1 MiB:

    0x100000  slot 0 header sector   [len u32][crc32 u32][seq u32][magic u32]
    0x101000  slot 0 image           16 KiB (four sectors)
    0x105000  slot 1 header sector
    0x106000  slot 1 image
    0x10A000  settings sector        [link 0: 16 bytes][link 1: 16 bytes][magic u32]

All integers little-endian. A slot is valid when its magic is present,
`1 <= len <= 16384` and its image's CRC-32 matches; the valid slot with
the higher `seq` is the installed payload. An upload goes into the
other slot: its sectors are erased (at `sys/loader/begin`), the image is
programmed after a successful commit, then its header, magic last, with
`seq + 1`. A write torn by a power cut leaves the new header without its
magic, so the previous slot stays the installed one; the installed slot
is never erased, as `esp_base::store` activates only after the CRC
check. The settings sector likewise has its magic last.

`SpiFlash` is the SPI master (mode 0, `div` system cycles per SCK half
period; the clock leaves through `USRMCLK` in the SoC): read (0x03),
page program (0x02), 4 KiB sector erase (0x20), each write after a write
enable (0x06) and followed by status polling (0x05, WIP). `Store` runs
the jobs above on it.
"""

from migen import Array, Cat, Constant, If, Module, Mux, NextState, NextValue, Signal

from .gotofsm import GotoFSM

SECTOR = 4096
PAGE = 256
SLOT_BASE = (0x100000, 0x105000)
IMG_OFF = 0x1000
SETTINGS = 0x10A000
SLOT_SECTORS = 5
MAGIC = 0x5A75B0A5
SET_MAGIC = 0x5A75B0C5
SETTINGS_LEN = 32

CMD_READ, CMD_PROGRAM, CMD_ERASE, CMD_WREN, CMD_STATUS = 0x03, 0x02, 0x20, 0x06, 0x05
OP_READ, OP_ERASE, OP_PROGRAM = 0, 1, 2


class SpiFlash(Module):
    def __init__(self, div=2):
        # Pads (sclk through USRMCLK in the SoC).
        self.cs_n = Signal(reset=1, name="flash_cs_n")
        self.sclk = Signal(name="flash_sclk")
        self.mosi = Signal(name="flash_mosi")
        self.miso = Signal(name="flash_miso")
        # Operation.
        self.go = Signal()
        self.op = Signal(2)
        self.addr = Signal(24)
        self.n = Signal(15)            # bytes to read or program (<= 256)
        self.busy = Signal()
        self.done = Signal()
        # Read data, one strobe per byte.
        self.rd_stb = Signal()
        self.rd_byte = Signal(8)
        # Program data, pulled: pg_req with pg_idx until pg_ack with pg_byte.
        self.pg_req = Signal()
        self.pg_idx = Signal(9)
        self.pg_ack = Signal()
        self.pg_byte = Signal(8)

        # # #

        # Byte shifter.
        sh_go = Signal()
        sh_out = Signal(8)
        sh_done = Signal()
        sh_in = Signal(8)
        obuf = Signal(8)
        bits = Signal(4)
        divc = Signal(max=max(2, div + 1))
        phase = Signal()
        active = Signal()
        miso_r = Signal()
        self.comb += self.mosi.eq(obuf[7])
        tick = Signal()
        self.comb += tick.eq(divc == div - 1)
        self.sync += [
            sh_done.eq(0),
            If(sh_go,
                obuf.eq(sh_out), bits.eq(8), phase.eq(0), divc.eq(0), active.eq(1),
                self.sclk.eq(0),
            ).Elif(active,
                divc.eq(Mux(tick, 0, divc + 1)),
                If(tick,
                    If(~phase,
                        self.sclk.eq(1), phase.eq(1),
                        sh_in.eq(Cat(self.miso, sh_in[:7])),
                    ).Else(
                        self.sclk.eq(0), phase.eq(0),
                        obuf.eq(Cat(Constant(0, 1), obuf[:7])),
                        bits.eq(bits - 1),
                        If(bits == 1, active.eq(0), sh_done.eq(1)),
                    ),
                ),
            ),
        ]

        op = Signal(2)
        addr = Signal(24)
        n = Signal(15)
        i = Signal(15)
        abyte = Signal(2)
        fsm = GotoFSM(reset_state="IDLE")
        self.submodules += fsm
        self.comb += self.busy.eq(~fsm.ongoing("IDLE") | self.go)
        cs = Signal(reset=1)
        self.comb += self.cs_n.eq(cs)

        def send(byte, nxt):
            """Shift `byte` out (and a byte in), then go to `nxt`."""
            return [sh_go.eq(1), sh_out.eq(byte), NextState(nxt)]

        def wait(nxt, *then):
            return If(sh_done, NextState(nxt), *then)   # `then` may override nxt

        fsm.act("IDLE",
            If(self.go,
                NextValue(op, self.op), NextValue(addr, self.addr), NextValue(n, self.n),
                NextValue(i, 0),
                If(self.op == OP_READ, NextState("CMD")).Else(NextState("WREN")),
            ),
        )
        # Write enable, its own CS cycle.
        fsm.act("WREN", NextValue(cs, 0), NextState("WREN1"))
        fsm.act("WREN1", *send(CMD_WREN, "WREN2"))
        fsm.act("WREN2", wait("WREN3", NextValue(cs, 1)))
        fsm.act("WREN3", NextState("CMD"))      # CS high for at least a cycle
        fsm.act("CMD",
            NextValue(cs, 0), NextValue(abyte, 0),
            NextState("CMD1"),
        )
        fsm.act("CMD1",
            *send(Mux(op == OP_READ, CMD_READ, Mux(op == OP_ERASE, CMD_ERASE, CMD_PROGRAM)),
                  "CMD2"),
        )
        fsm.act("CMD2", wait("ADDR"))
        abytes = Array([addr[16:24], addr[8:16], addr[0:8]])
        fsm.act("ADDR", *send(abytes[abyte], "ADDR2"))
        fsm.act("ADDR2",
            wait("ADDR",
                NextValue(abyte, abyte + 1),
                If(abyte == 2,
                    If(op == OP_READ, NextState("RD")
                    ).Elif(op == OP_ERASE, NextValue(cs, 1), NextState("POLL")
                    ).Else(NextState("PG")),
                ),
            ),
        )
        # Read n bytes.
        fsm.act("RD",
            If(i == n, NextValue(cs, 1), NextState("FIN")).Else(*send(0, "RD2")),
        )
        fsm.act("RD2",
            wait("RD", NextValue(i, i + 1)),
        )
        self.comb += self.rd_byte.eq(sh_in), self.rd_stb.eq(fsm.ongoing("RD2") & sh_done)
        # Program n bytes, pulled one at a time.
        pg = Signal(8)
        self.comb += self.pg_idx.eq(i[:9])
        fsm.act("PG",
            If(i == n, NextValue(cs, 1), NextState("POLL")).Else(
                self.pg_req.eq(1),
                If(self.pg_ack, NextValue(pg, self.pg_byte), NextState("PG1")),
            ),
        )
        fsm.act("PG1", *send(pg, "PG2"))
        fsm.act("PG2", wait("PG", NextValue(i, i + 1)))
        # Wait for the write to finish: read the status register until WIP
        # is clear, in one CS cycle.
        fsm.act("POLL", NextState("POLL0"))      # CS high between commands
        fsm.act("POLL0", NextValue(cs, 0), NextState("POLL1"))
        fsm.act("POLL1", *send(CMD_STATUS, "POLL2"))
        fsm.act("POLL2", wait("POLL3"))
        fsm.act("POLL3", *send(0, "POLL4"))
        fsm.act("POLL4",
            wait("POLL3",
                If(~sh_in[0], NextValue(cs, 1), NextState("FIN")),
            ),
        )
        fsm.act("FIN",
            self.done.eq(1),
            NextState("IDLE"),
        )


def _le(v, n):
    return [v[8 * k:8 * k + 8] for k in range(n)]


class Store(Module):
    """The storage jobs on a `SpiFlash`.

    * `erase_go` (`slot`): erase the slot's header and image sectors;
    * `prog_go` (`slot`, `len`, `crc`, `seq`): program the image from the
      source port (`src_adr` -> `src_dat` one cycle later), then the header;
    * `set_go`: erase the settings sector and program `SETTINGS_LEN`
      bytes pulled from `set_idx` -> `set_byte` (combinational), then the
      magic;
    * `rd_go` (`rd_addr`, `rd_n`): read, bytes on `rd_stb`/`rd_byte`.

    `busy` while a job runs, `done` at its end. One job at a time: the
    caller waits for `~busy`."""

    def __init__(self, div=2):
        self.submodules.spi = spi = SpiFlash(div)
        self.erase_go = Signal()
        self.prog_go = Signal()
        self.set_go = Signal()
        self.rd_go = Signal()
        self.slot = Signal()
        self.len = Signal(15)
        self.crc = Signal(32)
        self.seq = Signal(32)
        self.rd_addr = Signal(24)
        self.rd_n = Signal(15)
        self.rd_stb = Signal()
        self.rd_byte = Signal(8)
        self.src_adr = Signal(14)
        self.src_dat = Signal(8)
        self.set_idx = Signal(6)
        self.set_byte = Signal(8)
        self.busy = Signal()
        self.done = Signal()

        # # #

        self.comb += self.rd_stb.eq(spi.rd_stb), self.rd_byte.eq(spi.rd_byte)
        slot = Signal()
        ln = Signal(15)
        crc = Signal(32)
        seq = Signal(32)
        k = Signal(15)                 # sector or page counter / byte offset
        base = Signal(24)
        self.comb += base.eq(Mux(slot, SLOT_BASE[1], SLOT_BASE[0]))

        fsm = GotoFSM(reset_state="IDLE")
        self.submodules += fsm
        self.comb += self.busy.eq(~fsm.ongoing("IDLE"))

        fsm.act("IDLE",
            NextValue(slot, self.slot), NextValue(ln, self.len), NextValue(crc, self.crc),
            NextValue(seq, self.seq), NextValue(k, 0),
            If(self.erase_go, NextState("ERASE")
            ).Elif(self.prog_go, NextState("PROG")
            ).Elif(self.set_go, NextState("SET_ERASE")
            ).Elif(self.rd_go,
                spi.go.eq(1), spi.op.eq(OP_READ), spi.addr.eq(self.rd_addr),
                spi.n.eq(self.rd_n),
                NextState("WAIT"),
            ),
        )
        fsm.act("WAIT", If(spi.done, NextState("DONE")))
        # Erase the five sectors of the slot.
        fsm.act("ERASE",
            If(k == SLOT_SECTORS, NextState("DONE")).Else(
                spi.go.eq(1), spi.op.eq(OP_ERASE), spi.addr.eq(base + (k[:3] << 12)),
                NextState("ERASE_W"),
            ),
        )
        fsm.act("ERASE_W", If(spi.done, NextValue(k, k + 1), NextState("ERASE")))
        # Program the image page by page (k = byte offset), then the header.
        rest = Signal(15)
        self.comb += rest.eq(ln - k)
        fsm.act("PROG",
            If(k >= ln, NextState("HDR")).Else(
                spi.go.eq(1), spi.op.eq(OP_PROGRAM), spi.addr.eq(base + IMG_OFF + k),
                spi.n.eq(Mux(rest > PAGE, PAGE, rest)),
                NextState("PROG_W"),
            ),
        )
        # Image bytes: address the source, take its data a cycle later.
        src_wait = Signal()
        self.comb += self.src_adr.eq(k[:14] + spi.pg_idx)
        fsm.act("PROG_W",
            If(spi.pg_req,
                NextValue(src_wait, ~src_wait),
                If(src_wait, spi.pg_ack.eq(1), spi.pg_byte.eq(self.src_dat)),
            ),
            If(spi.done, NextValue(k, k + PAGE), NextState("PROG")),
        )
        hdr = Array(_le(Cat(ln, Constant(0, 17)), 4) + _le(crc, 4) + _le(seq, 4)
                    + _le(Constant(MAGIC, 32), 4))
        fsm.act("HDR",
            spi.go.eq(1), spi.op.eq(OP_PROGRAM), spi.addr.eq(base), spi.n.eq(16),
            NextState("HDR_W"),
        )
        fsm.act("HDR_W",
            spi.pg_ack.eq(spi.pg_req), spi.pg_byte.eq(hdr[spi.pg_idx[:4]]),
            If(spi.done, NextState("DONE")),
        )
        # Settings.
        smagic = Array(_le(Constant(SET_MAGIC, 32), 4))
        self.comb += self.set_idx.eq(spi.pg_idx[:6])
        fsm.act("SET_ERASE",
            spi.go.eq(1), spi.op.eq(OP_ERASE), spi.addr.eq(SETTINGS),
            NextState("SET_ERASE_W"),
        )
        fsm.act("SET_ERASE_W", If(spi.done, NextState("SET_PROG")))
        fsm.act("SET_PROG",
            spi.go.eq(1), spi.op.eq(OP_PROGRAM), spi.addr.eq(SETTINGS),
            spi.n.eq(SETTINGS_LEN + 4),
            NextState("SET_PROG_W"),
        )
        fsm.act("SET_PROG_W",
            spi.pg_ack.eq(spi.pg_req),
            spi.pg_byte.eq(Mux(spi.pg_idx < SETTINGS_LEN, self.set_byte,
                               smagic[(spi.pg_idx - SETTINGS_LEN)[:2]])),
            If(spi.done, NextState("DONE")),
        )
        fsm.act("DONE", self.done.eq(1), NextState("IDLE"))
