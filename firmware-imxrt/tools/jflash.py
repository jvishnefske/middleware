"""flash.py's job through SEGGER's JLinkExe instead of probe-rs.

The EVKB's OpenSDA J-Link drops off USB when probe-rs opens it after a
board with the USB firmware has run (2026-11-13); SEGGER's own commander
does not. This drives the same RAM-loaded imxrt-flasher and mailbox:

1. `r` (reset, core halted at the boot ROM's entry: nothing of a previous
   image runs, so no DMA master writes into the TCM), set the FlexRAM
   banks the flasher was linked for, `loadbin` the flasher at its vector
   table, set MSP/PC/VTOR, `g`;
2. for each OFFSET:FILE: erase its 256 KiB sectors, then per 128 KiB chunk
   `loadbin` into BUFFER and command a program + read-back;
3. read every image back through the FlexSPI window (`savebin`) and compare;
4. reset into the bootloader (`r`, `g`).

usage: python3 -I jflash.py [--flasher ELF] OFFSET:FILE [...]
"""
import os, re, subprocess, sys, tempfile, time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import ramload  # noqa: E402  (symbols, section addresses, objcopy)
import fusesafe  # noqa: E402

HERE = os.path.dirname(os.path.abspath(__file__))
DEFAULT_FLASHER = os.path.join(HERE, "..", "flasher", "target", "thumbv7em-none-eabihf", "release", "imxrt-flasher")
SERIAL = os.environ.get("IMXRT_JLINK_SN", "000621000000")
SECTOR = 256 * 1024
CHUNK = 128 * 1024
FLASH_BASE = 0x6000_0000
GPR16, GPR17, VTOR = 0x400AC040, 0x400AC044, 0xE000ED08


def jlink(cmds, timeout=120, resume=True):
    """Runs JLinkExe on `cmds` (one string per line); returns its output.
    Connecting halts the core; `resume` lets it run again before quitting
    (the flasher must keep running between commands)."""
    fusesafe.check_jlink(cmds)  # never a fuse write (fusesafe.py)
    tail = ["g"] if resume else []
    with tempfile.NamedTemporaryFile("w", suffix=".jlink", delete=False) as f:
        f.write("\n".join(cmds + tail + ["qc"]) + "\n")
        path = f.name
    try:
        r = subprocess.run(
            ["JLinkExe", "-NoGui", "1", "-device", "MIMXRT1052xxx6B", "-if", "SWD", "-speed", "1000",
             "-autoconnect", "1", "-SelectEmuBySN", SERIAL, "-CommanderScript", path],
            capture_output=True, text=True, timeout=timeout)
    finally:
        os.unlink(path)
    out = r.stdout + r.stderr
    if "Cannot connect" in out or "Could not connect" in out:
        sys.exit(f"JLinkExe could not connect:\n{out[-800:]}")
    return out


def words(out, addr, n):
    """The `mem32 addr n` words in JLinkExe's output."""
    vals = []
    for line in out.splitlines():
        m = re.match(r"\s*([0-9A-Fa-f]{8}) = ((?:[0-9A-Fa-f]{8} ?)+)", line)
        if m and int(m.group(1), 16) <= addr + 4 * len(vals) < int(m.group(1), 16) + 64:
            vals += [int(w, 16) for w in m.group(2).split()]
    return vals[:n]


def mem32(addr, n=1):
    return words(jlink([f"mem32 {addr:#x} {n}"]), addr, n)


def load_flasher(elf):
    with tempfile.TemporaryDirectory() as d:
        blob = os.path.join(d, "flasher.bin")
        subprocess.run([ramload.llvm_tool("llvm-objcopy"), "-O", "binary", elf, blob], check=True)
        data = open(blob, "rb").read()
        base = ramload.section_address(elf, ".vector_table")
        sp, reset = int.from_bytes(data[0:4], "little"), int.from_bytes(data[4:8], "little")
        flexram = ramload.symbols(elf).get("__flexram_config")
        cmds = ["r", "h"]
        if flexram is not None:
            cmds += [f"w4 {GPR17:#x} {flexram:#x}", f"w4 {GPR16:#x} 0x00200007"]
        # Nothing of the boot ROM may fire into the flasher: SysTick off and
        # its pending bit cleared, every NVIC interrupt disabled and cleared.
        cmds += ["w4 0xE000E010 0", "w4 0xE000ED04 0x02000000"]
        cmds += [f"w4 {0xE000E180 + 4 * i:#x} 0xFFFFFFFF" for i in range(8)]
        cmds += [f"w4 {0xE000E280 + 4 * i:#x} 0xFFFFFFFF" for i in range(8)]
        cmds += [f"loadbin {blob} {base:#x}", f"w4 {VTOR:#x} {base:#x}", "w4 0xE000ED94 0",
                 "wreg XPSR 0x01000000",
                 f"wreg MSP {sp:#x}", f"SetPC {reset & ~1:#x}"]
        out = jlink(cmds)
    if "Downloading file" not in out and "O.K." not in out:
        sys.exit(f"flasher load failed:\n{out[-800:]}")
    print(f"flasher loaded at {base:#x} ({len(data)} bytes)")


def run(mailbox, cmd, offset, length, budget):
    seq = mem32(mailbox)[0] + 1
    jlink([f"w4 {mailbox + 4:#x} {cmd}", f"w4 {mailbox + 8:#x} {offset:#x}",
           f"w4 {mailbox + 12:#x} {length:#x}", f"w4 {mailbox:#x} {seq}"])
    deadline = time.time() + budget
    while True:
        time.sleep(0.5)
        mb = mem32(mailbox, 7)
        if len(mb) == 7 and mb[4] == seq:
            break
        if time.time() > deadline:
            sys.exit(f"flasher: command {cmd} at {offset:#x} did not finish in {budget:.0f} s")
    if mb[5] != 1:
        sys.exit(f"flasher: command {cmd} at {offset:#x} failed: status {mb[5]:#x} detail {mb[6]:#x}")


def main():
    args = sys.argv[1:]
    flasher = DEFAULT_FLASHER
    if "--flasher" in args:
        i = args.index("--flasher")
        flasher = args[i + 1]
        del args[i:i + 2]
    images = []
    for spec in args:
        off, _, path = spec.partition(":")
        data = open(path, "rb").read()
        images.append((int(off, 0), data + b"\xff" * (-len(data) % 4)))
    if not images:
        sys.exit(__doc__)
    syms = ramload.symbols(flasher)
    mailbox, buffer = syms["imxrt_flasher::MAILBOX"], syms["imxrt_flasher::BUFFER"]
    load_flasher(flasher)
    time.sleep(1.0)
    if mem32(mailbox + 20)[0] != 1:
        sys.exit("flasher did not report ready")
    with tempfile.TemporaryDirectory() as d:
        for offset, data in images:
            first = offset - offset % SECTOR
            end = -(-(offset + len(data)) // SECTOR) * SECTOR
            n = (end - first) // SECTOR
            print(f"{offset:#09x}: erasing {n} sector(s)")
            run(mailbox, 1, first, end - first, 5.0 * n + 10)
            for at in range(0, len(data), CHUNK):
                chunk = data[at:at + CHUNK]
                path = os.path.join(d, "chunk.bin")
                open(path, "wb").write(chunk)
                jlink([f"loadbin {path} {buffer:#x}"])
                run(mailbox, 2, offset + at, len(chunk), 14.0)
                print(f"{offset:#09x}: programmed and read back {at + len(chunk)} / {len(data)} bytes")
        ok = True
        for offset, data in images:
            path = os.path.join(d, "back.bin")
            jlink([f"savebin {path} {FLASH_BASE + offset:#x} {len(data):#x}"], timeout=300)
            back = open(path, "rb").read() if os.path.exists(path) else b""
            same = back == data
            ok &= same
            print(f"{offset:#09x}: SWD read-back of {len(data)} bytes {'matches' if same else 'DIFFERS from'} the file")
    jlink(["r"])
    print("reset")
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
