"""SHA-256 (FIPS 180-4) over a byte stream (CL6): the uploaded image's
digest, `middleware::loader::sha256::ImageHash` in gateware.

Bytes enter one per cycle while `ready`, filling the 16 schedule words
(each byte shifts into word `n / 4`); a full block runs 64 rounds, one
per cycle, the schedule computed in place, the round constants in a
64-word ROM. `finish` (taken at any time after the last byte, also while
a block is being processed) appends the padding: 0x80, zeros, the bit
length, in one or two more blocks. `done` pulses with `digest` (byte 0 of
the digest in bits 255..248, `H0` big-endian first). `start` begins a new
message. Messages up to 2 MiB, more than the 16 KiB image RAM.
"""

from migen import Array, Cat, Constant, FSM, If, Memory, Module, Mux, NextState, NextValue, Signal

K = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
]
H0 = [0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19]


def _rotr(x, n):
    return Cat(x[n:], x[:n])


def _shr(x, n):
    return Cat(x[n:], Constant(0, n))


class Sha256(Module):
    def __init__(self):
        self.start = Signal()
        self.valid = Signal()
        self.data = Signal(8)
        self.ready = Signal()
        self.finish = Signal()
        self.busy = Signal()
        self.done = Signal()
        self.digest = Signal(256)

        # # #

        w = Array(Signal(32, name=f"sha_w{i}") for i in range(16))
        h = [Signal(32, reset=H0[i], name=f"sha_h{i}") for i in range(8)]
        a, b, c, d, e, f, g, hh = (Signal(32, name=f"sha_{n}") for n in "abcdefgh")
        bcount = Signal(6)
        nbytes = Signal(21)
        t = Signal(6)
        fin = Signal()                 # finish seen, padding not started
        first_pad = Signal()           # the next pad byte is 0x80
        lenblk = Signal()              # this padding block takes the length

        kmem = Memory(32, 64, init=K)
        self.specials += kmem
        kr = kmem.get_port()
        self.specials += kr
        kt = kr.dat_r

        s1, ch, s0, maj, t1, t2, sg0, sg1, neww = (Signal(32) for _ in range(9))
        self.comb += [
            s1.eq(_rotr(e, 6) ^ _rotr(e, 11) ^ _rotr(e, 25)),
            ch.eq((e & f) ^ (~e & g)),
            s0.eq(_rotr(a, 2) ^ _rotr(a, 13) ^ _rotr(a, 22)),
            maj.eq((a & b) ^ (a & c) ^ (b & c)),
            t1.eq(hh + s1 + ch + kt + w[0]),
            t2.eq(s0 + maj),
            sg0.eq(_rotr(w[1], 7) ^ _rotr(w[1], 18) ^ _shr(w[1], 3)),
            sg1.eq(_rotr(w[14], 17) ^ _rotr(w[14], 19) ^ _shr(w[14], 10)),
            neww.eq(sg1 + w[9] + sg0 + w[0]),
            self.digest.eq(Cat(*reversed(h))),
        ]

        lenbits = Signal(64)
        self.comb += lenbits.eq(Cat(Constant(0, 3), nbytes))
        pad_byte = Signal(8)
        self.comb += pad_byte.eq(
            Mux(first_pad, 0x80,
                Mux(lenblk & (bcount >= 56),
                    (lenbits >> Cat(Constant(0, 3), ~bcount[:3]))[:8], 0)))

        def push(byte):
            widx = bcount[2:6]
            return [NextValue(w[widx], Cat(byte, w[widx][:24])), NextValue(bcount, bcount + 1)]

        fsm = FSM(reset_state="IDLE")
        self.submodules += fsm
        self.comb += self.busy.eq(~fsm.ongoing("IDLE") | fin)
        fin_clr = Signal()
        self.sync += If(fin_clr, fin.eq(0)).Elif(self.finish, fin.eq(1))
        restart = [NextValue(hi, H0[i]) for i, hi in enumerate(h)] + [
            NextValue(bcount, 0), NextValue(nbytes, 0), fin_clr.eq(1)]

        self.comb += self.ready.eq(fsm.ongoing("IDLE") & ~fin & ~self.start)
        fsm.act("IDLE",
            If(self.start, *restart
            ).Elif(fin,
                fin_clr.eq(1),
                NextValue(first_pad, 1),
                NextValue(lenblk, bcount <= 55),
                NextState("PAD"),
            ).Elif(self.valid,
                *push(self.data),
                NextValue(nbytes, nbytes + 1),
                If(bcount == 63, NextState("LOAD")),
            ),
        )
        fsm.act("PAD",
            *push(pad_byte),
            NextValue(first_pad, 0),
            If(bcount == 63, NextState("LOAD")),
        )
        fsm.act("LOAD",
            kr.adr.eq(0),
            NextValue(a, h[0]), NextValue(b, h[1]), NextValue(c, h[2]), NextValue(d, h[3]),
            NextValue(e, h[4]), NextValue(f, h[5]), NextValue(g, h[6]), NextValue(hh, h[7]),
            NextValue(t, 0),
            NextState("ROUND"),
        )
        fsm.act("ROUND",
            kr.adr.eq(t + 1),
            NextValue(hh, g), NextValue(g, f), NextValue(f, e), NextValue(e, d + t1),
            NextValue(d, c), NextValue(c, b), NextValue(b, a), NextValue(a, t1 + t2),
            *[NextValue(w[i], w[i + 1]) for i in range(15)],
            NextValue(w[15], neww),
            NextValue(t, t + 1),
            If(t == 63, NextState("ADD")),
        )
        padding = Signal()
        self.sync += If(fsm.ongoing("PAD"), padding.eq(1)).Elif(fsm.ongoing("DONE"), padding.eq(0))
        fsm.act("ADD",
            *[NextValue(h[i], h[i] + v) for i, v in enumerate((a, b, c, d, e, f, g, hh))],
            NextValue(bcount, 0),
            If(padding & lenblk, NextState("DONE")
            ).Elif(padding, NextValue(lenblk, 1), NextState("PAD")
            ).Else(NextState("IDLE")),
        )
        fsm.act("DONE",
            self.done.eq(1),
            NextState("IDLE"),
        )
