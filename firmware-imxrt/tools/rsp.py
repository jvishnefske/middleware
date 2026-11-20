"""Minimal GDB remote serial protocol client for probe-rs's GDB server.

Only what loading a RAM image and driving the flasher need: stop reason,
read/write memory, set registers, continue, interrupt. It starts one
`probe-rs gdb` server on 127.0.0.1 and talks to it (the system gdb has no
Arm support). Notes from bring-up on the MIMXRT1050-EVKB:

* the server halts the core when it attaches, so `?` (not ^C) gives the
  stop reason;
* replies use run-length encoding (`c*n`);
* the target description repeats msp/psp/xpsr in a second feature: the
  first numbering wins (sp 13, pc 15, msp 16, xpsr 18, `extra` 19 =
  PRIMASK, BASEPRI, FAULTMASK, CONTROL, one byte each);
* `D` (detach) leaves the core halted: use `c`, and resume explicitly
  after closing (see ramload.py).

The probe is $IMXRT_PROBE (default: the EVKB's onboard OpenSDA J-Link).
"""
import os, re, socket, subprocess, time

import fusesafe

# GDB memory writes: M<addr>,<len>:<hex> and X<addr>,<len>:<binary>.
_MEM_WRITE = re.compile(r"^[MX]([0-9a-fA-F]+),([0-9a-fA-F]+):")

# IMXRT_GDB=jlink uses SEGGER's JLinkGDBServerCLExe instead: the EVKB's
# OpenSDA J-Link drops off USB when probe-rs opens it after the USB
# firmware has run (2026-11-13); SEGGER's server does not.
JLINK = os.environ.get("IMXRT_GDB") == "jlink"
JLINK_SN = os.environ.get("IMXRT_JLINK_SN", "000621000000")

PROBE = ["--probe", os.environ.get("IMXRT_PROBE", "1366:0105:000621000000"),
         "--speed", "1000", "--chip", "MIMXRT1050_hyperflash"]


def _unrle(p: str) -> str:
    """Expand GDB run-length encoding: `c*n` repeats c (ord(n) - 29) more times."""
    out, i = [], 0
    while i < len(p):
        if p[i] == "*" and out and i + 1 < len(p):
            out.append(out[-1] * (ord(p[i + 1]) - 29))
            i += 2
        else:
            out.append(p[i])
            i += 1
    return "".join(out)


class Rsp:
    def __init__(self, port=1337):
        if JLINK:
            # SEGGER's server: `-noir` keeps the target's state, the core
            # is halted on attach as with probe-rs.
            argv = ["JLinkGDBServerCLExe", "-device", "MIMXRT1052xxx6B", "-if", "SWD", "-speed", "1000",
                    "-select", f"USB={JLINK_SN}", "-port", str(port), "-nogui", "-singlerun", "-noir",
                    "-nolocalhostonly" if False else "-localhostonly"]
        else:
            argv = ["probe-rs", "gdb", *PROBE, "--gdb-connection-string", f"127.0.0.1:{port}", "--non-interactive"]
        # SEGGER's server logs every command: to a file, since an unread pipe
        # fills and blocks it mid-transfer.
        out = open(os.path.join(os.environ.get("TMPDIR", "/tmp"), "jlink-gdb.log"), "wb") if JLINK else subprocess.PIPE
        self.proc = subprocess.Popen(argv, stdout=out, stderr=subprocess.STDOUT)
        try:
            self._connect(port)
        except BaseException:
            # A server left running would hold the port for the next try.
            self.proc.kill()
            self.proc.wait(timeout=10)
            raise

    def _connect(self, port):
        deadline = time.time() + 30
        while True:
            try:
                self.s = socket.create_connection(("127.0.0.1", port), timeout=30 if JLINK else 10)
                break
            except OSError:
                if time.time() > deadline or self.proc.poll() is not None:
                    out = self.proc.stdout.read().decode(errors="replace") if self.proc.stdout and self.proc.poll() is not None else ""
                    raise SystemExit(f"gdb server did not come up:\n{out[-1500:]}")
                time.sleep(0.3)
        self.buf = b""
        self.cmd("qSupported:swbreak+;hwbreak+")
        self.regs = self._reg_numbers()

    def close(self):
        try:
            self.s.close()
        finally:
            self.proc.terminate()
            self.proc.wait(timeout=10)

    def _send(self, data: str):
        cs = sum(data.encode()) & 0xFF
        self.s.sendall(f"${data}#{cs:02x}".encode())

    def _recv(self) -> str:
        while True:
            m = re.search(rb"\$([^#]*)#[0-9a-fA-F]{2}", self.buf)
            if m:
                self.buf = self.buf[m.end():]
                self.s.sendall(b"+")
                return _unrle(m.group(1).decode(errors="replace"))
            chunk = self.s.recv(65536)
            if not chunk:
                raise SystemExit("gdb server closed the connection")
            self.buf += chunk

    def cmd(self, data: str) -> str:
        # Every packet passes here: a memory write that touches the fuse
        # controller never leaves (fusesafe.py).
        m = _MEM_WRITE.match(data)
        if m:
            fusesafe.check(int(m.group(1), 16), int(m.group(2), 16), "gdb " + data[:24])
        self._send(data)
        return self._recv()

    def _reg_numbers(self):
        xml, off = "", 0
        while True:
            r = self.cmd(f"qXfer:features:read:target.xml:{off:x},800")
            xml += r[1:]
            off += len(r) - 1
            if not r or r[0] != "m":
                break
        # Fill sequential numbering where regnum is implied.
        n, out = 0, {}
        for m in re.finditer(r"<reg name=['\"](\w+)['\"]([^>]*)/>", xml):
            rn = re.search(r"regnum=['\"](\d+)['\"]", m.group(2))
            n = int(rn.group(1)) if rn else n
            out.setdefault(m.group(1), n)  # first feature wins (later ones repeat names)
            n += 1
        return out

    def halt(self):
        # The server halts the core on attach; ask for the stop reason.
        return self.cmd("?")

    def write(self, addr: int, data: bytes):
        for i in range(0, len(data), 512):
            c = data[i:i + 512]
            r = self.cmd(f"M{addr + i:x},{len(c):x}:{c.hex()}")
            if r != "OK":
                raise SystemExit(f"write at {addr + i:#x} failed: {r}")

    def read(self, addr: int, n: int) -> bytes:
        out = b""
        while len(out) < n:
            k = min(1024, n - len(out))
            r = self.cmd(f"m{addr + len(out):x},{k:x}")
            if r.startswith("E") or not r:
                raise SystemExit(f"read at {addr + len(out):#x} failed: {r}")
            out += bytes.fromhex(r)
        return out

    def word(self, addr: int) -> int:
        return int.from_bytes(self.read(addr, 4), "little")

    def set_word(self, addr: int, value: int):
        self.write(addr, value.to_bytes(4, "little"))

    def set_reg(self, name: str, value: int):
        r = self.cmd(f"P{self.regs[name]:x}={value.to_bytes(4, 'little').hex()}")
        if r != "OK":
            raise SystemExit(f"set {name} failed: {r}")

    def get_reg(self, name: str) -> int:
        return int.from_bytes(bytes.fromhex(self.cmd(f"p{self.regs[name]:x}")), "little")

    def cont(self):
        """Resumes the core; the stop reply comes only after `interrupt`."""
        self._send("c")

    def interrupt(self):
        """Halts a running core (^C) and returns its stop reply."""
        self.s.sendall(b"\x03")
        return self._recv()

    def detach(self):
        try:
            self._send("D")
        except OSError:
            pass
