"""Refuses any debug-probe write that could program the i.MX RT's fuses.

The RT1050's fuses are programmed only through the OCOTP controller
(0x401F_4000, i.MX RT1050 RM chapter "On-Chip OTP Controller"): set
HW_OCOTP_CTRL's address and unlock key, then write HW_OCOTP_DATA. A burned
fuse (boot configuration, BT_FUSE_SEL, SEC_CONFIG, keys) can never be
cleared and may brick the board, and nothing in this repository ever
needs to write there: the firmware only reads the unique id from the
shadow registers (`src/app.rs`, `src/node.rs`).

Every memory write the tools make goes through a check here: the J-Link
command lists (`jflash.jlink`), the GDB `M` packets (`rsp.Gdb.write`) and
`probe-rs write` (`ramload.probe_rs`, `flash.py`). A write that touches
the OCOTP block, or a J-Link command that writes to an address this module
cannot see, stops the tool before anything reaches the probe.
"""

import os
import re
import sys

# The OCOTP controller's register block, with its whole 16 KiB AIPS slot.
OCOTP = (0x401F_4000, 0x401F_8000)


class FuseWriteRefused(SystemExit):
    pass


def check(addr: int, length: int, what: str = "write") -> None:
    """Refuses a write of `length` bytes at `addr` that touches OCOTP."""
    lo, hi = OCOTP
    if length <= 0:
        length = 1
    if addr < hi and addr + length > lo:
        raise FuseWriteRefused(
            f"refused: {what} at {addr:#010x}..{addr + length:#010x} touches the OCOTP "
            f"fuse controller ({lo:#x}..{hi:#x}); fuses are never written by these tools"
        )


def _int(s: str) -> int:
    return int(s, 0)


# JLinkExe commands that write target memory, and how wide each is.
_JLINK_WRITE = {"w1": 1, "w2": 2, "w4": 4, "write1": 1, "write2": 2, "write4": 4}
# Commands that program fuses or OTP outright, on any device J-Link knows.
_JLINK_FORBIDDEN = re.compile(r"^(exec\s+.*(otp|fuse|efuse)|.*\botp\b|.*efuse)", re.I)


def check_jlink(cmds) -> None:
    """Checks a JLinkExe command list before it is run."""
    for line in cmds:
        c = line.strip()
        if not c:
            continue
        if _JLINK_FORBIDDEN.match(c):
            raise FuseWriteRefused(f"refused J-Link command (fuse/OTP): {c}")
        parts = c.split()
        op = parts[0].lower()
        if op in _JLINK_WRITE:
            check(_int(parts[1]), _JLINK_WRITE[op], c)
        elif op == "loadbin":
            # loadbin <file> <addr>
            path, addr = parts[1], _int(parts[2])
            check(addr, os.path.getsize(path), c)
        elif op in ("loadfile", "loadhex", "mem", "fillmem", "writememory"):
            # Writes whose span this module does not parse: refused, not guessed.
            if op != "mem":
                raise FuseWriteRefused(f"refused J-Link command (unchecked write): {c}")


_PROBE_RS_WIDTH = {"b8": 1, "b16": 2, "b32": 4, "b64": 8}


def check_probe_rs(args) -> None:
    """Checks a `probe-rs` argument list (`write <width> <addr> <values..>`)."""
    if not args or args[0] != "write":
        return
    width = _PROBE_RS_WIDTH.get(args[1])
    if width is None:
        raise FuseWriteRefused(f"refused probe-rs write of unknown width: {args}")
    values = [a for a in args[3:] if not a.startswith("-")]
    check(_int(args[2]), width * max(1, len(values)), "probe-rs " + " ".join(args[:3]))


if __name__ == "__main__":
    # Self-test: python3 fusesafe.py
    for ok in [(0x2020_0000, 4), (0x401F_3FFC, 4), (0x401F_8000, 4)]:
        check(*ok)
    for bad in [(0x401F_4000, 4), (0x401F_3FFE, 4), (0x401F_4010, 1), (0x401F_7FFF, 8)]:
        try:
            check(*bad)
        except FuseWriteRefused:
            continue
        sys.exit(f"not refused: {bad}")
    check_jlink(["w4 0x20200000 1", "r", "mem32 0x401f4410 2"])
    for bad in [["w4 0x401f4000 0x3e770000"], ["exec OTPWrite"], ["loadfile x.bin"]]:
        try:
            check_jlink(bad)
        except FuseWriteRefused:
            continue
        sys.exit(f"not refused: {bad}")
    check_probe_rs(["write", "b16", "0x400B8000", "0x0020"])
    try:
        check_probe_rs(["write", "b32", "0x401F4000", "1"])
        sys.exit("not refused: probe-rs OCOTP write")
    except FuseWriteRefused:
        pass
    print("fusesafe: ok")
