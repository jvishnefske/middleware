"""Reset an ESP into run mode (EN pulse, IO0 released) and report its boot
mode from the ROM banner. Exit 1 when the ROM stays in download mode: IO0
(the BOOT button or its strap) is held low on the board, which no host-side
reset can override.

    python3 tests/hil/esp_boot.py /dev/ttyACM1
"""

import array
import fcntl
import os
import re
import sys
import time

TIOCMBIS, TIOCMBIC, DTR, RTS = 0x5416, 0x5417, 0x002, 0x004


def boot_banner(port: str, seconds: float = 3.0) -> bytes:
    fd = os.open(port, os.O_RDWR | os.O_NOCTTY | os.O_NONBLOCK)
    try:
        bits = lambda req, b: fcntl.ioctl(fd, req, array.array("i", [b]))  # noqa: E731
        bits(TIOCMBIC, DTR)
        bits(TIOCMBIS, RTS)
        time.sleep(0.1)
        bits(TIOCMBIC, RTS)
        end, buf = time.time() + seconds, b""
        while time.time() < end:
            try:
                buf += os.read(fd, 4096)
            except BlockingIOError:
                time.sleep(0.05)
        return buf
    finally:
        os.close(fd)


def main() -> int:
    port = sys.argv[1]
    m = re.search(rb"boot:(0x[0-9a-f]+) \(([^)]*)", boot_banner(port))
    if m is None:
        print(f"{port}: no ROM banner (booted, or the console is elsewhere)")
        return 0
    mode = m.group(2).decode(errors="replace")
    print(f"{port}: ROM boot mode {m.group(1).decode()} ({mode})")
    if mode.startswith("DOWNLOAD"):
        print(f"{port}: IO0 is held low at reset (BOOT button or strap): release it and power-cycle",
              file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
