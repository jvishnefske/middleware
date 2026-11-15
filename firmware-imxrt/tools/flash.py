"""Programs images into the MIMXRT1050-EVKB's HyperFlash and verifies them.

probe-rs cannot program this board (it cannot reset it), so this loads the
RAM-linked `imxrt-flasher` (firmware-imxrt/flasher) with ramload.py and
drives it over SWD: for each OFFSET:FILE it erases the 256 KiB sectors the
image covers, then writes the image into the flasher's BUFFER 128 KiB at a
time and has it program and read back each chunk (the firmware's own
HyperFlash driver, `imxrt_base::shell::flexspi`). Last, independently of the
flasher's own check, it reads every image back over SWD through FlexSPI's
memory-mapped window (0x6000_0000 + offset) and compares it byte for byte
with the file.

usage: python3 -I flash.py [--flasher ELF] [--erase OFFSET:LEN ...] OFFSET:FILE [...]
       python3 -I flash.py --verify-only OFFSET:FILE [...]
       python3 -I flash.py --reset
`--erase` erases whole 256 KiB sectors (e.g. the record, trust and payload
regions before a first flashing) before the images are programmed.
Offsets are flash offsets (0x0 for the bootloader, 0x400000 slot A, ...).
The board may be in the boot ROM's serial downloader mode or running
anything; the flasher is loaded over it.

After programming (or with `--reset` alone) the board is reset with
WDOG1's software reset. With the J-Link backend the reset is then checked:
after BOOT_SECONDS VTOR must point into SDRAM, where the bootloader starts
the firmware; still in HyperFlash or the boot ROM means the board stayed in
the bootloader (seen after some resets), and a second WDOG1 reset is
issued (at most RESETS in all).
"""
import os, re, subprocess, sys, time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import ramload  # noqa: E402
from rsp import Rsp  # noqa: E402

HERE = os.path.dirname(os.path.abspath(__file__))
DEFAULT_FLASHER = os.path.join(HERE, "..", "flasher", "target", "thumbv7em-none-eabihf", "release", "imxrt-flasher")
SECTOR = 256 * 1024
CHUNK = 128 * 1024       # the flasher's BUFFER
FLASH_BASE = 0x6000_0000
ERASE_SECONDS = 5.0     # per sector: CFI says 1 s typical, 4 s at most
PROGRAM_SECONDS = 4.0   # per 256 KiB: 512 buffers of 2 ms at most, plus read-back


def run(g, mailbox, cmd, offset, length, budget):
    """Issues one flasher command and waits (bounded) for its completion."""
    seq = g.word(mailbox) + 1
    g.set_word(mailbox + 4, cmd)
    g.set_word(mailbox + 8, offset)
    g.set_word(mailbox + 12, length)
    g.set_word(mailbox, seq)
    deadline = time.time() + budget
    while True:
        g.cont()
        time.sleep(0.3)
        g.interrupt()
        if g.word(mailbox + 16) == seq:
            break
        if time.time() > deadline:
            sys.exit(f"flasher: command {cmd} at {offset:#x} did not finish in {budget:.0f} s")
    status, detail = g.word(mailbox + 20), g.word(mailbox + 24)
    if status != 1:
        what = f"read-back mismatch at +{detail & 0x7FFF_FFFF:#x}" if detail & 0x8000_0000 else f"error {detail:#x}"
        sys.exit(f"flasher: command {cmd} at {offset:#x} len {length:#x} failed: {what}")


def program(images, flasher, erases=()):
    syms = ramload.symbols(flasher)
    mailbox, buffer = syms["imxrt_flasher::MAILBOX"], syms["imxrt_flasher::BUFFER"]
    # One GDB session for the load and the programming: closing probe-rs's
    # GDB server can reset a board that booted from HyperFlash.
    g = Rsp()
    try:
        out = ramload.load(flasher, listen_seconds=1.0, quiet=True, session=g)
        if b"imxrt-flasher: ready" not in out:
            sys.exit(f"flasher did not come up; console: {out.decode(errors='replace')!r}")
        g.interrupt()
        for first, length in erases:
            if first % SECTOR or length % SECTOR:
                sys.exit(f"--erase {first:#x}:{length:#x}: not whole 256 KiB sectors")
            print(f"{first:#09x}: erasing {length // SECTOR} sector(s)")
            run(g, mailbox, 1, first, length, ERASE_SECONDS * (length // SECTOR) + 10)
        for offset, data in images:
            first = offset - offset % SECTOR
            end = -(-(offset + len(data)) // SECTOR) * SECTOR
            n = (end - first) // SECTOR
            print(f"{offset:#09x}: erasing {n} sector(s) {first:#x}..{end:#x}")
            run(g, mailbox, 1, first, end - first, ERASE_SECONDS * n + 10)
            for at in range(0, len(data), CHUNK):
                chunk = data[at : at + CHUNK]
                g.write(buffer, chunk)
                run(g, mailbox, 2, offset + at, len(chunk), PROGRAM_SECONDS + 10)
                print(f"{offset:#09x}: programmed and read back {at + len(chunk)} / {len(data)} bytes")
        g.cont()
        time.sleep(0.2)
    finally:
        g.close()


CCGR3, WDOG1_WCR = 0x400FC074, 0x400B8000
VTOR = 0xE000ED08
SDRAM = range(0x8000_0000, 0x8000_0000 + 32 * 1024 * 1024)
BOOT_SECONDS = 6.0   # the bootloader copies and hashes the slot (about 1 s for 800 KB)
RESETS = 3


def open_rsp(attempts=3):
    """A GDB session, retried: the first J-Link session after the board
    has run often drops or does not answer."""
    for n in range(attempts):
        try:
            return Rsp()
        except (ConnectionResetError, TimeoutError, OSError) as e:
            if n + 1 == attempts:
                raise
            print(f"GDB session failed ({e!r}); retrying")
            time.sleep(3)


def booted(vtor):
    """Whether VTOR says the firmware runs: its vector table is in SDRAM."""
    return vtor in SDRAM


def reset_and_check_rsp():
    """WDOG1 resets until the firmware runs (VTOR in SDRAM), at most
    RESETS; returns whether it does."""
    for n in range(1, RESETS + 1):
        reset_board_rsp()
        time.sleep(BOOT_SECONDS)
        g = open_rsp()
        try:
            g.halt()
            vtor = g.word(VTOR)
            pc = g.get_reg("pc")
            g.cont()
        finally:
            try:
                g.close()
            except Exception:
                pass
        if booted(vtor):
            print(f"firmware running (VTOR {vtor:#010x}, PC {pc:#010x}) after {n} reset(s)")
            return True
        print(f"still in the bootloader or the ROM (VTOR {vtor:#010x}, PC {pc:#010x}); resetting again")
    print(f"the firmware did not start after {RESETS} resets: power-cycle the board")
    return False


def rsp_jlink():
    import rsp
    return rsp.JLINK


def verify_rsp(images):
    """`verify` over one GDB session (the J-Link backend: no probe-rs)."""
    ok = True
    g = Rsp()
    try:
        g.halt()
        for offset, data in images:
            back = g.read(FLASH_BASE + offset, len(data))
            same = back == data
            ok &= same
            print(f"{offset:#09x}: SWD read-back of {len(data)} bytes {'matches' if same else 'DIFFERS from'} the file")
        g.cont()
    finally:
        g.close()
    return ok


def reset_board_rsp():
    """`reset_board` over a GDB session: WDOG1's software reset."""
    g = open_rsp()
    try:
        g.halt()
        if (g.word(CCGR3) >> 16) & 3 == 0:
            print("WDOG1 clock gate off: not resetting; power-cycle the board")
            return
        g.cmd(f"M{WDOG1_WCR:x},2:2000")
        print("reset (WDOG1)")
    finally:
        try:
            g.close()
        except Exception:
            pass


def reset_board():
    """Resets the chip into the boot ROM and the bootloader: WDOG1's
    software reset (WCR = 0x0020, SRS low), after checking its clock gate
    (CCGR3 CG8; a gated block stalls the bus). A probe-rs session that only
    clears C_HALT can leave the core halted or reset it, depending on the
    boot mode; this reset is the one that reliably boots the new images."""
    text = ramload.probe_rs("read", "b32", hex(CCGR3), "1")
    m = re.search(r"[0-9a-f]{8}: ([0-9a-f]{8})", text)
    if not m or (int(m.group(1), 16) >> 16) & 3 == 0:
        print("WDOG1 clock gate off or unreadable: not resetting; power-cycle the board")
        return
    # The chip resets mid-transaction, so probe-rs reports an error: expected.
    subprocess.run(["probe-rs", "write", "b16", hex(WDOG1_WCR), "0x0020", *ramload.PROBE],
                   capture_output=True, timeout=120)
    print("reset (WDOG1)")


def verify(images):
    """Reads each image back over SWD through the memory-mapped window."""
    ok = True
    for offset, data in images:
        words = len(data) // 4
        back = bytearray()
        for at in range(0, words, 16384):
            k = min(16384, words - at)
            text = ramload.probe_rs("read", "b32", hex(FLASH_BASE + offset + 4 * at), str(k))
            for line in text.splitlines():
                m = re.match(r"[0-9a-f]{8}:((?: [0-9a-f]{8})+)", line.strip())
                if m:
                    for w in m.group(1).split():
                        back += int(w, 16).to_bytes(4, "little")
        back = bytes(back[: words * 4])
        if back == data[: words * 4]:
            print(f"{offset:#09x}: SWD read-back of {len(back)} bytes matches the file")
        else:
            bad = next(i for i, (a, b) in enumerate(zip(back, data)) if a != b)
            print(f"{offset:#09x}: SWD read-back DIFFERS at +{bad:#x}")
            ok = False
    return ok


def main():
    args = sys.argv[1:]
    flasher = DEFAULT_FLASHER
    if "--flasher" in args:
        i = args.index("--flasher")
        flasher = args[i + 1]
        del args[i : i + 2]
    erases = []
    while "--erase" in args:
        i = args.index("--erase")
        off, _, length = args[i + 1].partition(":")
        erases.append((int(off, 0), int(length, 0)))
        del args[i : i + 2]
    verify_only = "--verify-only" in args
    reset_only = "--reset" in args
    args = [a for a in args if a not in ("--verify-only", "--reset")]
    if reset_only:
        if rsp_jlink():
            sys.exit(0 if reset_and_check_rsp() else 1)
        reset_board()
        sys.exit(0)
    images = []
    for spec in args:
        off, _, path = spec.partition(":")
        data = open(path, "rb").read()
        # Whole 512-byte packets (0xFF, the erased value): the J-Link GDB
        # server did not answer a short final memory write.
        data += b"\xff" * (-len(data) % 512)
        images.append((int(off, 0), data))
    if not images:
        sys.exit(__doc__)
    if not verify_only:
        # The first J-Link session after the board has run often drops
        # (connection reset); one retry has always been enough.
        try:
            program(images, flasher, erases)
        except (ConnectionResetError, TimeoutError, SystemExit) as e:
            if not rsp_jlink():
                raise
            print(f"first attempt failed ({e!r}); retrying once")
            time.sleep(3)
            program(images, flasher, erases)
    ok = verify_rsp(images) if rsp_jlink() else verify(images)
    if not verify_only:
        if rsp_jlink():
            ok &= reset_and_check_rsp()
        else:
            reset_board()
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
