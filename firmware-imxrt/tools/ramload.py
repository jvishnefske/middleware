"""Loads a RAM-linked ELF into the MIMXRT1050-EVKB over SWD and starts it.

probe-rs cannot reset this board (its SYSRESETREQ write gets no SWD
acknowledge and probe-rs treats that as fatal), so nothing here resets. The
image is loaded into whatever the core was doing, and every piece of state a
previous run can leave behind is cleared first. Each step below was needed
on the board:

1. halt (the GDB server halts on attach) and set the FlexRAM banks (IOMUXC
   GPR17, GPR16 bit 2) to the image's `__flexram_config` *before* writing
   it, so its startup does not remap the DTCM it runs from;
2. write the image and read it back;
3. mask and clear every NVIC interrupt and stop SysTick (a previous image's
   PIT keeps firing into the new image's startup);
4. clear PRIMASK, BASEPRI, FAULTMASK and CONTROL (a fault inside a critical
   section leaves PRIMASK set);
5. disable the boot ROM's MPU and set VTOR;
6. leave every active exception (writing xPSR does not end one; the M7
   ignores VECTRESET; returning to thread mode with another still active is
   INVPC): a chain of exception frames, each `bx lr` returning through one
   active exception, the last into the image's Reset;
7. continue, close the GDB server, and resume explicitly (the server's
   shutdown can leave the core halted).

Never read a peripheral whose clock gate is off: the access stalls the bus
until a power cycle.

usage: python3 -I ramload.py ELF [--listen SECONDS]
The probe is $IMXRT_PROBE; the console is $IMXRT_VCOM (default: the
onboard OpenSDA J-Link's VCOM).
"""
import os, re, select, subprocess, sys, tempfile, termios, time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from rsp import PROBE, Rsp  # noqa: E402

VCOM = os.environ.get("IMXRT_VCOM", "/dev/serial/by-id/usb-SEGGER_J-Link_000621000000-if00")
DHCSR, VTOR, MPU_CTRL = 0xE000EDF0, 0xE000ED08, 0xE000ED94
GPR16, GPR17 = 0x400AC040, 0x400AC044


def llvm_tool(name):
    """`name` (llvm-objcopy, llvm-nm) from the active Rust toolchain."""
    sysroot = subprocess.run(["rustc", "--print", "sysroot"], capture_output=True, text=True, check=True).stdout.strip()
    host = subprocess.run(["rustc", "-vV"], capture_output=True, text=True, check=True).stdout
    triple = re.search(r"host: (\S+)", host).group(1)
    path = os.path.join(sysroot, "lib", "rustlib", triple, "bin", name)
    if not os.path.exists(path):
        sys.exit(f"{name} not found; install it with `rustup component add llvm-tools`")
    return path


def symbols(elf):
    """Demangled symbol name -> address."""
    out = subprocess.run([llvm_tool("llvm-nm"), "-C", elf], capture_output=True, text=True, check=True).stdout
    syms = {}
    for line in out.splitlines():
        parts = line.split(None, 2)
        if len(parts) == 3:
            syms[parts[2]] = int(parts[0], 16)
    return syms


def section_address(elf, name):
    """VMA of section `name`."""
    out = subprocess.run([llvm_tool("llvm-objdump"), "-h", elf], capture_output=True, text=True, check=True).stdout
    for line in out.splitlines():
        parts = line.split()
        if len(parts) >= 4 and parts[1] == name:
            return int(parts[3], 16)
    sys.exit(f"{elf}: no {name} section")


def probe_rs(*args):
    r = subprocess.run(["probe-rs", *args, *PROBE], capture_output=True, text=True, timeout=300)
    if r.returncode != 0:
        sys.exit(f"probe-rs {' '.join(args[:3])} failed:\n{r.stderr[-800:]}")
    return r.stdout


def listen(seconds):
    """Bytes from the console during `seconds`."""
    fd = os.open(VCOM, os.O_RDWR | os.O_NOCTTY | os.O_NONBLOCK)
    a = termios.tcgetattr(fd)
    a[0] = 0; a[1] = 0; a[2] = termios.CS8 | termios.CREAD | termios.CLOCAL; a[3] = 0
    a[4] = a[5] = termios.B115200
    termios.tcsetattr(fd, termios.TCSANOW, a)
    buf, end = b"", time.time() + seconds
    while time.time() < end:
        if select.select([fd], [], [], 0.2)[0]:
            try:
                buf += os.read(fd, 256)
            except BlockingIOError:
                pass
    os.close(fd)
    return buf


def active_exceptions(g):
    """Active exception numbers, the current one (VECTACTIVE) first."""
    current = g.word(0xE000ED04) & 0x1FF
    if not current:
        return []
    found = []
    shcsr = g.word(0xE000ED24)
    for bit, exc in ((0, 4), (1, 5), (3, 6), (7, 11), (8, 12), (10, 14), (11, 15)):
        if shcsr & (1 << bit):
            found.append(exc)
    for i in range(8):
        iabr = g.word(0xE000E300 + 4 * i)
        for b in range(32):
            if iabr & (1 << b):
                found.append(16 + 32 * i + b)
    return [current] + [e for e in found if e != current]


CCR, ICIALLU = 0xE000ED14, 0xE000EF50
CCGR5 = 0x400FC07C
DMA0_ERQ, DMAMUX_CHCFG0 = 0x400E800C, 0x400EC000
CCGR1, CCGR6 = 0x400FC06C, 0x400FC080
USB1_USBCMD = 0x402E0140   # RST is bit 1
ENET_ECR = 0x402D8024      # RESET is bit 0


def quiesce(g):
    """Leaves nothing of the halted image running under the new one: the
    caches off (the firmware enables them; SDRAM contents need not survive)
    and, if eDMA is clocked (CCGR5 CG3; never touch a gated block), every
    channel's request disabled (ERQ = 0) and DMAMUX channels 0 and 1 (the
    firmware's LPUART1 and SAI1 receive rings, which write into DTCM) off;
    the USB controller and the
    ENET MAC reset (they too DMA into DTCM: a USB queue head overwrote a
    freshly loaded flasher, 2026-11-13)."""
    g.set_word(CCR, g.word(CCR) & ~((1 << 16) | (1 << 17)))
    g.set_word(ICIALLU, 0)
    if (g.word(CCGR5) >> 6) & 3:
        g.set_word(DMA0_ERQ, 0)
        for ch in range(2):
            g.set_word(DMAMUX_CHCFG0 + 4 * ch, 0)
    # Bus masters that keep writing into DTCM while the core is halted
    # (the firmware's USB queue heads and ENET rings live there): reset the
    # USB controller (USBCMD.RST) and the ENET MAC (ECR.RESET), each only
    # if its clock gate is on (CCGR6 CG0, CCGR1 CG5).
    if g.word(CCGR6) & 3:
        g.set_word(USB1_USBCMD, 1 << 1)
    if (g.word(CCGR1) >> 10) & 3:
        g.set_word(ENET_ECR, 1)


def load(elf, listen_seconds=0.0, quiet=False, release=True, session=None):
    """Loads and starts `elf`; returns what the console printed meanwhile.

    `session`: an open `Rsp` to use and leave open (the caller keeps
    driving the image over it). Closing probe-rs's GDB server can reset
    the chip once it has booted from HyperFlash, so flash.py loads the
    flasher and programs over one session.

    `release` ends with a separate probe-rs session that clears C_HALT, for
    when the GDB server left the core halted. Once the board has booted
    from HyperFlash that session resets the chip (SRC_SRSR
    lockup_sysresetreq, seen 2026-11-13), killing the loaded image, so
    callers that check the image came up (flash.py) pass `release=False`.
    """
    say = (lambda *a: None) if quiet else print
    with tempfile.TemporaryDirectory() as d:
        blob_path = os.path.join(d, "image.bin")
        subprocess.run([llvm_tool("llvm-objcopy"), "-O", "binary", elf, blob_path], check=True)
        blob = open(blob_path, "rb").read()
    blob += b"\0" * (-len(blob) % 4)
    syms = symbols(elf)
    # The image starts with its vector table (objcopy -O binary starts at the
    # lowest loadable address). Not the __svector_table symbol: RAM layouts
    # set it to 0 so imxrt-rt's startup copies nothing.
    base = section_address(elf, ".vector_table")
    flexram = syms.get("__flexram_config")
    sp, reset = int.from_bytes(blob[0:4], "little"), int.from_bytes(blob[4:8], "little")
    say(f"image {len(blob)} B at {base:#x}, SP {sp:#x}, Reset {reset:#x}")

    g = session or Rsp()
    try:
        g.halt()
        quiesce(g)
        if flexram is not None and g.word(GPR17) != flexram:
            g.set_word(GPR17, flexram)
            g.set_word(GPR16, g.word(GPR16) | (1 << 2))
            say(f"FlexRAM banks: GPR17 {flexram:#010x}")
        g.write(base, blob)
        back = g.read(base, len(blob))
        if back != blob:
            bad = next(i for i, (a, b) in enumerate(zip(blob, back)) if a != b)
            sys.exit(f"readback mismatch at {base + bad:#x}")
        say(f"readback ok: {len(blob)} bytes match")
        for i in range(8):
            g.set_word(0xE000E180 + 4 * i, 0xFFFF_FFFF)   # NVIC ICER
            g.set_word(0xE000E280 + 4 * i, 0xFFFF_FFFF)   # NVIC ICPR
        g.set_word(0xE000E010, 0)                         # SysTick CTRL
        if "extra" in g.regs:
            g.set_reg("extra", 0)                         # PRIMASK .. CONTROL
        else:
            g.cmd("qRcmd," + b"reg primask = 0".hex())    # J-Link: not in its register list
        g.set_word(MPU_CTRL, 0)
        g.set_word(VTOR, base)
        active = active_exceptions(g)
        if active:
            trampoline = sp - 0x1000
            g.write(trampoline, (0x4770).to_bytes(2, "little") + b"\x00\xbf")   # bx lr; nop
            n = len(active)
            frames = []
            for k in range(n):
                last = k == n - 1
                # Frame k is popped by return k; its LR is the EXC_RETURN of
                # return k + 1 (to thread mode for the last one).
                lr = 0xFFFF_FFFF if last else (0xFFFF_FFF9 if k == n - 2 else 0xFFFF_FFF1)
                pc = (reset & ~1) if last else trampoline
                xpsr = 0x0100_0000 | (0 if last else active[k + 1])
                frames += [0, 0, 0, 0, 0, lr, pc, xpsr]
            frame_sp = sp - 32 * n
            g.write(frame_sp, b"".join(w.to_bytes(4, "little") for w in frames))
            g.set_reg("sp", frame_sp)
            g.set_reg("msp", frame_sp)
            g.set_reg("lr", 0xFFFF_FFF9 if n == 1 else 0xFFFF_FFF1)
            g.set_reg("pc", trampoline)
            say(f"active exceptions {active}: unwinding into Reset")
        else:
            g.set_reg("sp", sp)
            g.set_reg("msp", sp)
            g.set_reg("pc", reset & ~1)
            g.set_reg("xpsr", 0x0100_0000)
        listen(0.2)
        g.cont()
        if session is not None:
            return listen(listen_seconds) if listen_seconds > 0 else b""
        time.sleep(0.5)
    finally:
        if session is None:
            g.close()
    if release:
        probe_rs("write", "b32", hex(DHCSR), hex(0xA05F0001))
    return listen(listen_seconds) if listen_seconds > 0 else b""


def main():
    elf = sys.argv[1]
    secs = float(sys.argv[sys.argv.index("--listen") + 1]) if "--listen" in sys.argv else 3.0
    out = load(elf, secs)
    print("console:", out.decode(errors="replace") if out else "(nothing)")


if __name__ == "__main__":
    main()
