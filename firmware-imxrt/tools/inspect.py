"""Read-only inspection of a MIMXRT1050-EVKB that will not answer on its VCOM.

Reports, over SWD (no flash or RAM is written):

* the core: PC, SP, VTOR, ICSR, and the fault status registers (CFSR, HFSR,
  MMFAR, BFAR), so a hang can be told from a fault loop;
* the boot header (FCB tag at 0x60000400, IVT at 0x60001000), the update
  record blocks (0x40000, 0x80000), the trust record (0xC0000) and the slot A
  header (0x400000), as hex dumps of their first bytes, to be decoded against
  `src/layout.rs` and `ab::record`;
* with --console SECONDS, the raw VCOM output (/dev/ttyACM0) for that long,
  after a WDOG1 software reset, so a panic or log line is visible.

Usage (J-Link backend, as for flash.py):
    IMXRT_GDB=jlink python3 -I tools/inspect.py [--console 10] [--port /dev/ttyACM0]

The reset for --console is the same WDOG1 reset flash.py --reset issues; no
flash sector is erased or programmed.
"""
import os, sys, time, termios, select, struct

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from rsp import Rsp  # noqa: E402

FLASH_BASE = 0x6000_0000
CCGR3 = 0x400F_C074          # clock gate of WDOG1 (flash.py)
WDOG1_WCR = 0x400B_8000
REGIONS = [
    ("boot FCB (tag 'FCFB')", FLASH_BASE + 0x0000_0400, 32),
    ("boot IVT", FLASH_BASE + 0x0000_1000, 32),
    ("update record block 0", FLASH_BASE + 0x0004_0000, 64),
    ("update record block 1", FLASH_BASE + 0x0008_0000, 64),
    ("trust record block 0", FLASH_BASE + 0x000C_0000, 64),
    ("trust record block 1", FLASH_BASE + 0x0010_0000, 64),
    ("slot A header", FLASH_BASE + 0x0040_0000, 64),
]
# ARMv7-M System Control Block.
SCB = {
    "VTOR": 0xE000_ED08,
    "ICSR": 0xE000_ED04,
    "HFSR": 0xE000_ED2C,
    "CFSR": 0xE000_ED28,
    "MMFAR": 0xE000_ED34,
    "BFAR": 0xE000_ED38,
}


def core_state(g):
    print("core:")
    for name in ("pc", "sp", "lr"):
        print(f"  {name:<5} {g.get_reg(name):#010x}")
    stop = g.halt()
    print(f"  stop reply: {stop}")
    for name, addr in SCB.items():
        print(f"  {name:<5} {g.word(addr):#010x}")
    cfsr = g.word(SCB["CFSR"])
    if cfsr:
        flags = {0: "IACCVIOL", 1: "DACCVIOL", 3: "MMARVALID", 8: "IBUSERR",
                 9: "PRECISERR", 10: "IMPRECISERR", 12: "UNDEFINSTR",
                 15: "INVSTATE", 16: "INVPC", 17: "NOCP", 24: "UNALIGNED", 25: "DIVBYZERO"}
        bits = [v for k, v in flags.items() if cfsr & (1 << k)]
        print(f"  CFSR decode: {', '.join(bits) or 'unknown bits'}")
    else:
        print("  CFSR decode: no fault flags set")


def regions(g):
    for label, addr, n in REGIONS:
        data = g.read(addr, n)
        print(f"{label} @ {addr:#010x}:")
        for i in range(0, n, 16):
            chunk = data[i:i + 16]
            hexs = " ".join(f"{b:02x}" for b in chunk)
            print(f"  +{i:02x}  {hexs}")
        if all(b == 0xFF for b in data):
            print("  (erased: all 0xFF)")
        elif all(b == 0x00 for b in data):
            print("  (all 0x00)")
        if label.startswith("boot FCB") and data[:4] == b"FCFB":
            print("  FCB tag present")
        if label.startswith("boot IVT") and data[0] == 0xD1:
            length = struct.unpack_from("<H", data, 1)[0]
            print(f"  IVT header tag 0xD1, length {length}, version {data[3]:#x}")


def console(port, seconds):
    fd = os.open(port, os.O_RDWR | os.O_NOCTTY | os.O_NONBLOCK)
    try:
        attrs = termios.tcgetattr(fd)
        attrs[4] = attrs[5] = termios.B115200
        termios.tcsetattr(fd, termios.TCSANOW, attrs)
        out = bytearray()
        end = time.time() + seconds
        while time.time() < end:
            r, _, _ = select.select([fd], [], [], 0.2)
            if r:
                try:
                    out += os.read(fd, 4096)
                except BlockingIOError:
                    pass
        return bytes(out)
    finally:
        os.close(fd)


def main(argv):
    console_s = 0.0
    port = "/dev/ttyACM0"
    if "--console" in argv:
        console_s = float(argv[argv.index("--console") + 1])
    if "--port" in argv:
        port = argv[argv.index("--port") + 1]

    g = Rsp()
    try:
        core_state(g)
        regions(g)
        if console_s:
            print(f"console {port} @115200 for {console_s:.0f} s after WDOG1 reset (raw):")
            # The same WDOG1 software reset as flash.py's reset_board_rsp.
            if (g.word(CCGR3) >> 16) & 3 == 0:
                sys.exit("WDOG1 clock gate off: not resetting; power-cycle the board")
            g.cmd(f"M{WDOG1_WCR:x},2:2000")
            g.cont()
            raw = console(port, console_s)
            g.interrupt()
            print(raw.decode(errors="replace") or "(no output)")
    finally:
        g.close()


if __name__ == "__main__":
    main(sys.argv[1:])
