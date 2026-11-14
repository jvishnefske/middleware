"""The upload path (CL6): `sys/loader/begin|chunk|commit` as
`middleware::loader::Loader::step` handles them, in gateware.

`UploadState` is the transfer state of `Loader` (`Idle` or `Receiving
{len, crc, received, running}`) and its step: `begin` checks the length
against the capacity and resets the transfer; `chunk` acknowledges a
resend of the last chunk without writing it (`offset + n == received`),
refuses one out of order (`InvalidRequest`, transfer abandoned) or past
the declared length (`BufferTooSmall`), and otherwise has the caller
write it while the running CRC-32 follows the bytes; `commit` needs every
byte and a matching CRC-32. Body parsing (`Command::parse`: begin exactly
8 bytes, chunk 1..=512 data bytes, commit empty or 68 bytes) is the
caller's (`system.System`), because a malformed body leaves the transfer
alone.

The CRC-32 (ISO-HDLC, the zlib CRC, `middleware::crc::Crc32`) is one
byte per cycle as XOR taps, the technique of `frame.crc16_byte`.
"""

from migen import Cat, If, Module, Mux, Signal

POLY = 0xEDB88320

# HandlerError codes (src/transport/reqrep.rs).
OK, NOT_FOUND, INVALID_REQUEST, BUFFER_TOO_SMALL, INTERNAL, DENIED, UNAVAILABLE = 0, 1, 2, 3, 4, 5, 6

# PayloadStatus.
EMPTY, STOPPED, RUNNING, TRAPPED, LOADING = 0, 1, 2, 3, 4


def _crc32_step_py(crc, byte):
    crc ^= byte
    for _ in range(8):
        crc = (crc >> 1) ^ POLY if crc & 1 else crc >> 1
    return crc


def crc32_py(data):
    crc = 0xFFFF_FFFF
    for b in data:
        crc = _crc32_step_py(crc, b)
    return crc ^ 0xFFFF_FFFF


def _taps():
    taps = []
    for out in range(32):
        c = [i for i in range(32) if _crc32_step_py(1 << i, 0) >> out & 1]
        d = [i for i in range(8) if _crc32_step_py(0, 1 << i) >> out & 1]
        taps.append((c, d))
    return taps


_TAPS = _taps()


def crc32_byte(crc, byte):
    """One byte of the reflected CRC-32 register update (no final XOR)."""
    bits = []
    for c, d in _TAPS:
        terms = [crc[i] for i in c] + [byte[i] for i in d]
        e = terms[0]
        for t in terms[1:]:
            e = e ^ t
        bits.append(e)
    return Cat(*bits)


class Crc32(Module):
    """`init` restarts; each `stb` adds `byte`; `value` is the CRC so far
    (final XOR applied)."""

    def __init__(self):
        self.init = Signal(name="crc_init")
        self.stb = Signal(name="crc_stb")
        self.din = Signal(8, name="crc_din")
        self.value = Signal(32, name="crc_value")
        reg = Signal(32, reset=0xFFFF_FFFF)
        self.sync += If(self.init, reg.eq(0xFFFF_FFFF)).Elif(self.stb,
                                                             reg.eq(crc32_byte(reg, self.din)))
        self.comb += self.value.eq(~reg)


class UploadState(Module):
    """`Loader` and `Loader::step` for the three transfer commands.

    Pulse one of `begin`, `chunk`, `commit` with the parsed fields
    (`len_in`, `crc_in`; `offset`, `n`); `code` answers in the same cycle
    (combinational) and the state moves at the clock edge. For an
    accepted chunk (`write` high) the caller then streams the `n` bytes
    through `wr_stb`/`wr_byte` (the running CRC follows); for an accepted
    commit `activate` is high."""

    def __init__(self, capacity):
        self.begin = Signal()
        self.chunk = Signal()
        self.commit = Signal()
        self.len_in = Signal(32)
        self.crc_in = Signal(32)
        self.offset = Signal(32)
        self.n = Signal(10)
        self.code = Signal(8)
        self.write = Signal()
        self.activate = Signal()
        self.wr_stb = Signal()
        self.wr_byte = Signal(8)
        self.receiving = Signal()
        self.length = Signal(15)       # declared length while receiving
        self.crc = Signal(32)          # declared CRC

        # # #

        received = Signal(15)
        self.submodules.run = run = Crc32()
        self.comb += run.stb.eq(self.wr_stb), run.din.eq(self.wr_byte)
        end = Signal(33)
        self.comb += end.eq(self.offset + self.n)

        begin_ok = (self.len_in != 0) & (self.len_in <= capacity)
        resend = self.receiving & (end == received)
        in_order = self.offset == received
        fits = end <= self.length
        commit_ok = self.receiving & (received == self.length) & (run.value == self.crc)

        self.comb += [
            If(self.begin,
                self.code.eq(Mux(begin_ok, OK, BUFFER_TOO_SMALL)),
            ).Elif(self.chunk,
                If(~self.receiving, self.code.eq(INVALID_REQUEST)
                ).Elif(resend, self.code.eq(OK)
                ).Elif(~in_order, self.code.eq(INVALID_REQUEST)
                ).Elif(~fits, self.code.eq(BUFFER_TOO_SMALL)
                ).Else(self.code.eq(OK), self.write.eq(1)),
            ).Elif(self.commit,
                If(commit_ok, self.code.eq(OK), self.activate.eq(1)
                ).Else(self.code.eq(INVALID_REQUEST)),
            ),
        ]
        self.sync += [
            If(self.begin,
                self.receiving.eq(begin_ok),
                self.length.eq(self.len_in[:15]),
                self.crc.eq(self.crc_in),
                received.eq(0),
            ).Elif(self.chunk,
                If(self.receiving & ~resend,
                    If(~in_order | ~fits, self.receiving.eq(0)
                    ).Else(received.eq(end[:15])),
                ),
            ).Elif(self.commit,
                self.receiving.eq(0),
            ),
        ]
        self.comb += run.init.eq(self.begin)

