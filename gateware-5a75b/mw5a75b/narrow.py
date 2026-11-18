"""Comparisons of wide values against narrow bounds, without a carry chain
over the wide value: its bits above the bound's width are ORed, and only
the low bits are compared. Yosys maps `v > c` on a 32-bit `v` to a 32-bit
subtractor (16 carry cells) even when `c` is a small constant."""

from migen import Cat, Constant


def _or(xs):
    out = xs[0]
    for x in xs[1:]:
        out = out | x
    return out


def gt(v, c):
    """v > constant c."""
    w = max(1, c.bit_length())
    return (v > c) if len(v) <= w else ((v[w:] != 0) | (v[:w] > c))


def lt(v, c):
    """v < constant c (c >= 1)."""
    return ~gt(v, c - 1)


def ge(v, s):
    """v >= s for a signal s narrower than v."""
    w = len(s)
    return (v >= s) if len(v) <= w else ((v[w:] != 0) | (v[:w] >= s))


def sum_gt(a, b, lim):
    """a + b > lim (unsigned, without overflow) for a signal lim of width
    w: true when either operand reaches 2**w, else a (w+1)-bit sum."""
    if isinstance(b, int):
        b = Constant(b, max(1, b.bit_length()))
    w = len(lim)
    high = [x[w:] != 0 for x in (a, b) if len(x) > w]
    narrow = Cat(a[:w], Constant(0, 1)) + b[:w] > lim
    return _or(high + [narrow]) if high else narrow
