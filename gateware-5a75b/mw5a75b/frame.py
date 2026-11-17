"""The frame core (CL4): one raw serial frame per UDP datagram
(`middleware::udp`, `src/serial/frame.rs`).

    Request   [1][id][key_len][key][payload][crc16 LE]
    Response  [2][id][status][payload][crc16 LE]
    Publish   [3][0][key_len][key][payload][crc16 LE]

`FrameDecoder` takes a datagram one byte per cycle and writes its body
(everything but the CRC) into a block-RAM buffer while it checks the
CRC-16/CCITT-FALSE and the key. The CRC is little-endian at the end, so the
decoder runs the body through a two-byte delay line: the byte leaving the
line is a body byte; the two left in it at the last byte are the CRC.
At the end it pulses `done` with the verdict, in the order
`frame::decode` checks:

    TOO_LARGE   more than MAX_RAW_FRAME bytes
    MALFORMED   fewer than 2 bytes, a body under 3 bytes, a key past the end
    CRC         checksum mismatch
    KIND        unknown kind
    KEY         key not 1..=128 bytes of [A-Za-z0-9_.-] segments split by '/'

The buffer is then held (`busy`, the sink not ready) until the consumer
pulses `consumed`. Key bytes are also strobed out (`key_stb`, `key_idx`,
`key_byte`) as they pass, for the bridge engine's single-pass match.

`CrcAppender` turns a body stream into a datagram: bytes pass through and
the CRC follows, low byte first.
"""

from migen import Cat, Constant, If, Memory, Module, Mux, Signal

MAX_RAW_FRAME = 1024
MAX_KEY = 128

KIND_REQUEST = 1
KIND_RESPONSE = 2
KIND_PUBLISH = 3

OK = 0
TOO_LARGE = 1
MALFORMED = 2
CRC = 3
KIND = 4
KEY = 5

ERROR_NAMES = {OK: "Ok", TOO_LARGE: "TooLarge", MALFORMED: "Malformed",
               CRC: "Crc", KIND: "UnknownKind", KEY: "InvalidKey"}


def _crc16_step_py(crc, byte):
    crc ^= byte << 8
    for _ in range(8):
        crc = ((crc << 1) ^ 0x1021) & 0xFFFF if crc & 0x8000 else (crc << 1) & 0xFFFF
    return crc


def _crc16_taps():
    """For each output bit, the crc bits and byte bits it is the XOR of.
    The byte step is linear over GF(2), so the unit vectors give it."""
    taps = []
    for out in range(16):
        c = [i for i in range(16) if _crc16_step_py(1 << i, 0) >> out & 1]
        d = [i for i in range(8) if _crc16_step_py(0, 1 << i) >> out & 1]
        taps.append((c, d))
    return taps


_TAPS = _crc16_taps()


def crc16_byte(crc, byte):
    """One byte of CRC-16/CCITT-FALSE (poly 0x1021, MSB first) as an
    expression of a 16-bit `crc` and an 8-bit `byte`: one XOR per output
    bit (an unrolled shift loop would nest exponentially)."""
    bits = []
    for c, d in _TAPS:
        terms = [crc[i] for i in c] + [byte[i] for i in d]
        e = terms[0]
        for t in terms[1:]:
            e = e ^ t
        bits.append(e)
    return Cat(*bits)


def crc16_py(data):
    crc = 0xFFFF
    for b in data:
        crc = _crc16_step_py(crc, b)
    return crc


def key_char_ok(b):
    """`allowed()` from src/key.rs: ASCII alphanumeric, `_`, `-`, `.`."""
    return (((b >= ord("0")) & (b <= ord("9")))
            | ((b >= ord("A")) & (b <= ord("Z")))
            | ((b >= ord("a")) & (b <= ord("z")))
            | (b == ord("_")) | (b == ord("-")) | (b == ord(".")))


class FrameDecoder(Module):
    def __init__(self):
        # Input stream (one datagram, `last` on its final byte).
        self.sink_valid = Signal()
        self.sink_data = Signal(8)
        self.sink_last = Signal()
        self.sink_ready = Signal()

        # Verdict, valid from `done` until `consumed`.
        self.done = Signal()
        self.busy = Signal()
        self.consumed = Signal()
        self.ok = Signal()
        self.error = Signal(3)
        self.kind = Signal(8)
        self.id = Signal(8)
        self.third = Signal(8)         # key_len or status
        self.body_len = Signal(11)     # bytes in the buffer
        self.start = Signal()          # first byte of a datagram

        # Key bytes as they pass (index from 0).
        self.key_stb = Signal()
        self.key_idx = Signal(8)
        self.key_byte = Signal(8)

        # Buffer read port (synchronous: data one cycle after the address).
        self.rd_adr = Signal(10)
        self.rd_dat = Signal(8)

        # # #

        buf = Memory(8, MAX_RAW_FRAME)
        self.specials += buf
        wr = buf.get_port(write_capable=True)
        rd = buf.get_port(has_re=False)
        self.specials += wr, rd
        self.comb += rd.adr.eq(self.rd_adr), self.rd_dat.eq(rd.dat_r)

        count = Signal(11)      # bytes received, saturating past 1024+
        over = Signal()         # more than MAX_RAW_FRAME bytes
        d1 = Signal(8)          # newest delayed byte
        d2 = Signal(8)          # oldest delayed byte
        crc = Signal(16, reset=0xFFFF)
        seg_len = Signal(8)
        bad_char = Signal()
        key_end_ok = Signal()   # last key segment non-empty

        self.comb += self.sink_ready.eq(~self.busy)
        take = Signal()
        self.comb += take.eq(self.sink_valid & self.sink_ready)

        # The body byte leaving the delay line this cycle, at body position
        # `count - 2`.
        body_stb = Signal()
        pos = Signal(11)
        v = Signal(8)
        self.comb += [
            body_stb.eq(take & (count >= 2)),
            pos.eq(count - 2),
            v.eq(d2),
        ]
        crc_next = Signal(16)
        self.comb += crc_next.eq(crc16_byte(crc, v))

        in_key = Signal()
        kidx = Signal(11)
        third_now = Signal(8)
        # `third` is being written this very cycle when pos == 2.
        self.comb += [
            kidx.eq(pos - 3),
            in_key.eq((pos >= 3) & (kidx < self.third)),
            self.key_stb.eq(body_stb & in_key),
            self.key_idx.eq(kidx[:8]),
            self.key_byte.eq(v),
            self.start.eq(take & (count == 0)),
            third_now.eq(self.third),
        ]

        self.comb += [
            wr.adr.eq(pos[:10]),
            wr.dat_w.eq(v),
            wr.we.eq(body_stb & (pos < MAX_RAW_FRAME)),
        ]

        # The verdict at the last byte. n = count + 1 bytes in total.
        n = Signal(11)
        blen = Signal(11)        # body length n - 2
        crc_body = Signal(16)    # CRC over the whole body
        got = Signal(16)
        seg_final = Signal(8)
        bad_final = Signal()
        kend_final = Signal()
        err = Signal(3)
        self.comb += [
            n.eq(count + 1),
            blen.eq(n - 2),
            crc_body.eq(Mux(count >= 2, crc_next, crc)),
            got.eq(Cat(d1, self.sink_data)),
        ]
        # The key state including this cycle's body byte.
        is_slash = Signal()
        self.comb += is_slash.eq(v == ord("/"))
        seg_after = Signal(8)
        bad_after = Signal()
        self.comb += [
            seg_after.eq(Mux(in_key & body_stb,
                             Mux(is_slash, 0, seg_len + 1), seg_len)),
            bad_after.eq(bad_char | (in_key & body_stb &
                                     Mux(is_slash, seg_len == 0, ~key_char_ok(v)))),
        ]
        last_key_byte = Signal()
        self.comb += last_key_byte.eq(in_key & body_stb & (kidx + 1 == self.third))
        self.comb += [
            seg_final.eq(seg_after),
            bad_final.eq(bad_after),
            kend_final.eq(Mux(last_key_byte, seg_after != 0, key_end_ok)),
        ]
        keyed = Signal()
        self.comb += keyed.eq((self.kind == KIND_REQUEST) | (self.kind == KIND_PUBLISH))
        # At the last byte the header registers are final unless the body
        # is shorter than 3 bytes, which is MALFORMED anyway; a header byte
        # written this very cycle (pos == 2 when n == 5) is taken from `v`.
        kind_f = Signal(8)
        third_f = Signal(8)
        self.comb += [
            kind_f.eq(Mux(body_stb & (pos == 0), v, self.kind)),
            third_f.eq(Mux(body_stb & (pos == 2), v, self.third)),
        ]
        keyed_f = Signal()
        self.comb += keyed_f.eq((kind_f == KIND_REQUEST) | (kind_f == KIND_PUBLISH))
        self.comb += If(over | (n > MAX_RAW_FRAME),
                        err.eq(TOO_LARGE)
                     ).Elif(n < 2,
                        err.eq(MALFORMED)
                     ).Elif(crc_body != got,
                        err.eq(CRC)
                     ).Elif(blen < 3,
                        err.eq(MALFORMED)
                     ).Elif(kind_f == KIND_RESPONSE,
                        err.eq(OK)
                     ).Elif(~keyed_f,
                        err.eq(KIND)
                     ).Elif(third_f > blen - 3,
                        err.eq(MALFORMED)
                     ).Elif((third_f == 0) | (third_f > MAX_KEY) | bad_final | ~kend_final,
                        err.eq(KEY)
                     ).Else(
                        err.eq(OK)
                     )

        self.sync += [
            self.done.eq(0),
            If(self.consumed, self.busy.eq(0)),
            If(take,
                d1.eq(self.sink_data),
                d2.eq(d1),
                If(count != (1 << 11) - 1, count.eq(count + 1)),
                If(count >= MAX_RAW_FRAME + 2, over.eq(1)),
                If(body_stb,
                    crc.eq(crc_next),
                    If(pos == 0, self.kind.eq(v)),
                    If(pos == 1, self.id.eq(v)),
                    If(pos == 2, self.third.eq(v)),
                    seg_len.eq(seg_after),
                    bad_char.eq(bad_after),
                    If(last_key_byte, key_end_ok.eq(seg_after != 0)),
                ),
                If(self.sink_last,
                    self.done.eq(1),
                    self.busy.eq(1),
                    self.error.eq(err),
                    self.ok.eq(err == OK),
                    self.kind.eq(kind_f),
                    self.third.eq(third_f),
                    self.body_len.eq(Mux(n > MAX_RAW_FRAME, MAX_RAW_FRAME, blen)),
                    count.eq(0),
                    over.eq(0),
                    crc.eq(0xFFFF),
                    seg_len.eq(0),
                    bad_char.eq(0),
                    key_end_ok.eq(0),
                ),
            ),
        ]


class CrcAppender(Module):
    """Body stream in, datagram out: the bytes pass through, then the
    CRC-16 low byte and high byte, the high byte with `last`."""

    def __init__(self):
        self.sink_valid = Signal()
        self.sink_data = Signal(8)
        self.sink_last = Signal()
        self.sink_ready = Signal()
        self.source_valid = Signal()
        self.source_data = Signal(8)
        self.source_last = Signal()
        self.source_ready = Signal()

        # # #

        crc = Signal(16, reset=0xFFFF)
        phase = Signal(2)  # 0 body, 1 crc low, 2 crc high
        self.comb += [
            If(phase == 0,
                self.source_valid.eq(self.sink_valid),
                self.source_data.eq(self.sink_data),
                self.source_last.eq(0),
                self.sink_ready.eq(self.source_ready),
            ).Elif(phase == 1,
                self.source_valid.eq(1),
                self.source_data.eq(crc[:8]),
            ).Else(
                self.source_valid.eq(1),
                self.source_data.eq(crc[8:]),
                self.source_last.eq(1),
            ),
        ]
        self.sync += [
            If(phase == 0,
                If(self.sink_valid & self.source_ready,
                    crc.eq(crc16_byte(crc, self.sink_data)),
                    If(self.sink_last, phase.eq(1)),
                ),
            ).Elif(phase == 1,
                If(self.source_ready, phase.eq(2)),
            ).Else(
                If(self.source_ready,
                    phase.eq(0),
                    crc.eq(0xFFFF),
                ),
            ),
        ]
