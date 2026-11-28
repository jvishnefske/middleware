"""Finding middleware boards on USB and the LAN, and driving them with mwctl.

Everything here is plain functions over text and sysfs so the parsers can be
tested without hardware (test_parsers.py). The pytest wiring is in
conftest.py; the checks themselves are in test_boards.py.

Supported boards and how each is recognised:

  USB id        product / by-id           board                 install (bootloader)
  2e8a:0009     middleware rp-base        Pico / Pico 2         rp2350: mwctl fw update (A/B)
                                                                rp2040: picotool load -f
  2e8a:000f     RP2350 BOOTSEL            Pico 2 in BOOTSEL     picotool load (factory UF2)
  2e8a:0003     RP2 Boot                  Pico in BOOTSEL       picotool load
  303a:1001     Espressif USB-JTAG        ESP32-S3 / ESP32-C3   espflash (ROM download mode)
  10c4:ea60,    CP210x / CH340 /          ESP32 / ESP8266       espflash / esptool (ROM)
  1a86:7523,    FT232R bridge
  1a86:55d4,
  0403:6001
  1366:0105     J-Link OpenSDA VCOM       MIMXRT1050-EVKB       mwctl fw update (A/B)
  1fc9:0094     middleware imxrt-base     MIMXRT1050-EVKB USB   mwctl fw update (A/B)
  0403:6011     "KD Carrier Card", if 01  Kria KD240 console    (linux-aarch64: deployed by
                                          (ttyPS1, FT4232H ch B)  `just os-update`, not installed)

  0403:6011 is also the Colorlight 5A-75B's FT4232H MiniModule (product
  "FT4232H MiniModule"): only the KD240 carrier's interface 01 matches, by
  product string as well as id.

  LAN: `mwctl scout` (Zenoh scouting) finds Zenoh nodes and the RT1050 behind
  a scouted router; DNS-SD (`_mw-node._tcp`, scripts/mdns-browse.py) finds a
  node that announces itself on the link (posix-node with MW_ZENOH_LISTEN),
  with its address and port; MW_HIL_LAN adds what neither can see:
    udp:<host>[:port]                 Colorlight 5A-75B gateware
    zenoh:<endpoint>@<node16>         a node behind a known router
    zenoh-listen:<endpoint>@<node16>  an RT1050 that connects to us

Never probed: FT2232/FT4232 other than the KD240 console (JTAG adapters such
as the Colorlight's MiniModule), other CMSIS-DAP probes such as the micro:bit,
and anything not in the table.
"""

from __future__ import annotations

import dataclasses
import importlib.util
import os
import re
import subprocess
import time
from pathlib import Path
from types import ModuleType

REPO = Path(__file__).resolve().parents[2]


def _load_mdns() -> ModuleType:
    """scripts/mdns-browse.py (a hyphenated name, so loaded by path)."""
    spec = importlib.util.spec_from_file_location("mdns_browse", REPO / "scripts" / "mdns-browse.py")
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod

# Chips each firmware reports in sys/info.
WASM_CHIPS = {"esp32", "esp32s3", "rp2350", "rt1050", "imxrt1052"}
PROTOCOL_ONLY_CHIPS = {"esp32c3", "esp8266", "rp2040", "ecp5", "nrf52833", "stm32g0b1"}
A_B_CHIPS = {"rp2350", "rt1050", "imxrt1052"}


@dataclasses.dataclass(frozen=True)
class UsbKind:
    family: str  # what the USB id tells us
    chips: frozenset[str]  # chips sys/info may report for it
    bootsel: bool = False  # in the ROM bootloader, no tty to talk to
    # Optional matchers, for a VID:PID shared with a device we must not
    # probe: the USB product string, and the interface number (the tty's).
    product: str | None = None
    interface: int | None = None


USB_KINDS: dict[tuple[int, int], UsbKind] = {
    (0x2E8A, 0x0009): UsbKind("rp-base", frozenset({"rp2040", "rp2350"})),
    (0x2E8A, 0x000F): UsbKind("rp2350-bootsel", frozenset({"rp2350"}), bootsel=True),
    (0x2E8A, 0x0003): UsbKind("rp2040-bootsel", frozenset({"rp2040"}), bootsel=True),
    (0x303A, 0x1001): UsbKind("esp-usb-jtag", frozenset({"esp32s3", "esp32c3"})),
    (0x10C4, 0xEA60): UsbKind("uart-bridge", frozenset({"esp32", "esp8266", "esp32s3", "esp32c3"})),
    (0x1A86, 0x7523): UsbKind("uart-bridge", frozenset({"esp32", "esp8266", "esp32s3", "esp32c3"})),
    (0x1A86, 0x55D4): UsbKind("uart-bridge", frozenset({"esp32", "esp8266", "esp32s3", "esp32c3"})),
    (0x0403, 0x6001): UsbKind("uart-bridge", frozenset({"esp32", "esp8266", "esp32s3", "esp32c3"})),
    (0x1366, 0x0105): UsbKind("rt1050-opensda", frozenset({"rt1050", "imxrt1052"})),
    (0x1FC9, 0x0094): UsbKind("rt1050-usb", frozenset({"rt1050", "imxrt1052"})),
    # micro:bit v2: the DAPLink's CMSIS-DAP probe and VCOM. The VCOM is the
    # nRF52833's UART (P0.06 TX, P1.08 RX); native USB is not wired on v2.
    (0x0D28, 0x0204): UsbKind("microbit-daplink", frozenset({"nrf52833"})),
}

# VID:PID entries that also need the product and interface (0403:6011 is
# the Colorlight's FT4232H too). Checked before USB_KINDS.
# How long the KD240 console probe waits for its Linux posix-node to answer
# (a cold boot of the board takes about this long).
KD240_CONSOLE_WAIT_S = 45

USB_KINDS_MATCHED: list[tuple[int, int, UsbKind]] = [
    (0x0403, 0x6011, UsbKind("kd240-console", frozenset({"linux-aarch64"}),
                             product="KD Carrier Card", interface=1)),
    # stm32-base (BTT EBB42, STM32G0B1) on pid.codes' shared test PID: only
    # its own product string, on the CDC-ACM control interface.
    (0x1209, 0x0001, UsbKind("stm32-base", frozenset({"stm32g0b1"}),
                             product="stm32-base", interface=0)),
]


def usb_kind(vid: int, pid: int, product: str = "", interface: int | None = None) -> UsbKind | None:
    """The kind of a USB device, or None (never probed)."""
    for v, p, kind in USB_KINDS_MATCHED:
        if (v, p) == (vid, pid) and product == kind.product and interface == kind.interface:
            return kind
    if any((v, p) == (vid, pid) for v, p, _ in USB_KINDS_MATCHED):
        return None  # a shared id whose matcher did not match: not ours
    return USB_KINDS.get((vid, pid))


@dataclasses.dataclass
class Board:
    """One reachable board: how to address it and what it said."""

    transport: str  # "usb", "udp", "zenoh", "zenoh-listen"
    address: str  # tty path, host, or endpoint
    kind: UsbKind | None = None
    node: str | None = None  # 16 hex digits, LAN boards
    serial: str | None = None  # USB serial number
    usb_path: str | None = None  # sysfs device, for BOOTSEL boards without a tty
    info: dict[str, str] | None = None
    error: str | None = None
    installed: bool = False

    @property
    def link(self) -> list[str]:
        """mwctl's arguments that select this board."""
        if self.transport == "usb":
            return ["--port", self.address]
        if self.transport == "udp":
            return ["--udp", self.address]
        flag = "--zenoh" if self.transport == "zenoh" else "--zenoh-listen"
        return [flag, self.address, "--node", self.node or ""]

    @property
    def chip(self) -> str | None:
        return (self.info or {}).get("chip")

    @property
    def id(self) -> str:
        name = self.chip or (self.kind.family if self.kind else "unknown")
        tag = (self.info or {}).get("node") or self.node or self.serial or self.address
        return f"{self.transport}-{name}-{re.sub(r'[^0-9A-Za-z]', '', tag)[-8:]}"


# ---------------------------------------------------------------- USB


def _read(p: Path) -> str | None:
    try:
        return p.read_text().strip()
    except OSError:
        return None


def usb_ttys(sys_root: Path = Path("/sys")) -> list[dict]:
    """Every tty with a USB parent: path, vid, pid, product, serial, interface."""
    out = []
    for tty in sorted((sys_root / "class/tty").glob("tty*")):
        dev = tty / "device"
        if not dev.exists():
            continue
        # .../usbX/X-Y/X-Y:1.0/ttyACM0 or .../X-Y:1.0/ttyUSB0/tty/ttyUSB0
        p = dev.resolve()
        while p != p.parent and not (p / "idVendor").exists():
            p = p.parent
        vid = _read(p / "idVendor")
        if vid is None:
            continue
        # The interface (X-Y:1.N) holds bInterfaceNumber. The tty's device link
        # points at the usb-serial port node, a child of the interface on
        # current kernels, or at the interface itself on older ones: search up.
        iface = None
        q = dev.resolve()
        for _ in range(3):
            iface = _read(q / "bInterfaceNumber")
            if iface is not None or q == q.parent:
                break
            q = q.parent
        out.append(
            {
                "path": f"/dev/{tty.name}",
                "vid": int(vid, 16),
                "pid": int(_read(p / "idProduct") or "0", 16),
                "product": _read(p / "product") or "",
                "serial": _read(p / "serial") or "",
                "usb": str(p),
                "interface": int(iface, 16) if iface else None,
            }
        )
    return out


def usb_devices(sys_root: Path = Path("/sys")) -> list[dict]:
    """Every USB device (for BOOTSEL boards, which have no tty)."""
    out = []
    for d in sorted((sys_root / "bus/usb/devices").glob("*")):
        vid = _read(d / "idVendor")
        if vid is None:
            continue
        out.append(
            {
                "vid": int(vid, 16),
                "pid": int(_read(d / "idProduct") or "0", 16),
                "product": _read(d / "product") or "",
                "serial": _read(d / "serial") or "",
                "usb": str(d.resolve()),
            }
        )
    return out


def usb_candidates(ttys: list[dict], devices: list[dict]) -> list[Board]:
    """The USB ports and BOOTSEL devices worth asking (see the table above)."""
    boards = []
    for t in ttys:
        kind = usb_kind(t["vid"], t["pid"], t.get("product", ""), t.get("interface"))
        if kind is None or kind.bootsel:
            continue
        boards.append(Board("usb", t["path"], kind, serial=t["serial"], usb_path=t["usb"]))
    for d in devices:
        kind = usb_kind(d["vid"], d["pid"], d.get("product", ""))
        if kind is not None and kind.bootsel:
            boards.append(Board("usb", "", kind, serial=d["serial"], usb_path=d["usb"]))
    return boards


# ---------------------------------------------------------------- LAN


def parse_scout(text: str) -> list[Board]:
    """Boards in `mwctl scout`'s table, each with a router endpoint to reach it.

    Routers and peers are reached at their own address; a client (the
    RT1050) through the router named in VIA (a zid prefix).
    """
    lines = [ln for ln in text.splitlines() if ln.strip()]
    try:
        head = next(i for i, ln in enumerate(lines) if ln.startswith("KIND"))
    except StopIteration:
        return []
    rows = []
    for ln in lines[head + 1 :]:
        cells = re.split(r"\s{2,}", ln.strip())
        if len(cells) < 7:
            continue
        kind, zid, addr, via, node, chip, fw = cells[:7]
        rows.append(dict(kind=kind, zid=zid, addr=addr, via=via, node=node, chip=chip, fw=fw))

    def tcp(addrs: str) -> str | None:
        for a in addrs.split(","):
            if a.startswith("tcp/"):
                return a
        return None

    boards = []
    for r in rows:
        if r["node"] == "-":
            continue
        ep = None
        if r["kind"] in ("router", "peer"):
            ep = tcp(r["addr"])
        if ep is None:
            for v in r["via"].split(","):
                for other in rows:
                    if other["kind"] == "router" and other["zid"].startswith(v.rstrip("…")):
                        ep = ep or tcp(other["addr"])
        if ep is None:
            continue
        boards.append(Board("zenoh", ep, node=r["node"]))
    return boards


def boards_from_dnssd(services) -> list[Board]:
    """DNS-SD `_mw-node._tcp` instances (scripts/mdns-browse.py) as Zenoh
    boards: the instance label is the node id; the endpoint is the first
    address (IPv4 first) and the announced port."""
    boards = []
    for s in services:
        if not re.fullmatch(r"[0-9a-fA-F]{16}", s.label) or s.port is None:
            continue
        addrs = sorted(s.addresses, key=lambda a: ":" in a)  # IPv4 before IPv6
        host = addrs[0] if addrs else s.host
        if ":" in host:
            host = f"[{host}]"
        boards.append(Board("zenoh", f"tcp/{host}:{s.port}", node=s.label.lower()))
    return boards


def parse_lan_env(value: str) -> list[Board]:
    """MW_HIL_LAN: space- or comma-separated udp:/zenoh:/zenoh-listen: entries."""
    boards = []
    for item in re.split(r"[\s,]+", value.strip()):
        if not item:
            continue
        scheme, _, rest = item.partition(":")
        if scheme == "udp" and rest:
            boards.append(Board("udp", rest))
        elif scheme in ("zenoh", "zenoh-listen") and "@" in rest:
            ep, _, node = rest.rpartition("@")
            if not re.fullmatch(r"[0-9a-fA-F]{16}", node):
                raise ValueError(f"MW_HIL_LAN {item!r}: node must be 16 hex digits")
            boards.append(Board(scheme, ep, node=node.lower()))
        else:
            raise ValueError(
                f"MW_HIL_LAN {item!r}: expected udp:<host>, zenoh:<ep>@<node> or zenoh-listen:<ep>@<node>"
            )
    return boards


# ---------------------------------------------------------------- mwctl


def parse_fields(text: str) -> dict[str, str]:
    """`key:   value` lines (mwctl info, trust info, fw info) as a dict."""
    out = {}
    for ln in text.splitlines():
        m = re.match(r"^([A-Za-z][A-Za-z0-9 /_-]*?):\s+(.*)$", ln)
        if m and not ln.startswith("["):
            out.setdefault(m.group(1).strip(), m.group(2).strip())
    return out


@dataclasses.dataclass
class Mwctl:
    exe: str
    timeout_ms: int = 1500

    def run(self, board: Board, *args: str, timeout: float = 60, timeout_ms: int | None = None) -> subprocess.CompletedProcess:
        cmd = [self.exe, *board.link]
        if board.transport != "zenoh-listen":
            cmd += ["--timeout", str(timeout_ms or self.timeout_ms)]
        cmd += list(args)
        return subprocess.run(cmd, capture_output=True, text=True, timeout=timeout)

    def info(self, board: Board) -> dict[str, str]:
        # A Linux console answers only if a posix-node serves it, and a board
        # that is still booting (or stuck in U-Boot) does not: a short probe,
        # retried until a deadline, so a slow Linux boot is waited for without
        # lengthening the probe for every other board.
        if board.kind is None or board.kind.family != "kd240-console":
            r = self.run(board, "info", timeout=30)
        else:
            deadline = time.monotonic() + KD240_CONSOLE_WAIT_S
            while True:
                r = self.run(board, "info", timeout=30, timeout_ms=300)
                if r.returncode == 0 or time.monotonic() >= deadline:
                    break
                time.sleep(1)
        if r.returncode != 0:
            raise RuntimeError((r.stderr or r.stdout).strip())
        return parse_fields(r.stdout)

    def scout(self, seconds: int) -> list[Board]:
        r = subprocess.run(
            [self.exe, "scout", "--seconds", str(seconds)],
            capture_output=True,
            text=True,
            timeout=seconds + 60,
        )
        return parse_scout(r.stdout)


def signing_args() -> list[str]:
    """--key/--pkcs11 for a claimed board, from MW_HIL_KEY or MW_HIL_PKCS11."""
    if os.environ.get("MW_HIL_PKCS11"):
        return ["--pkcs11", os.environ["MW_HIL_PKCS11"]]
    if os.environ.get("MW_HIL_KEY"):
        return ["--key", os.environ["MW_HIL_KEY"]]
    return []


# ---------------------------------------------------------------- payload


def _leb(n: int) -> bytes:
    out = bytearray()
    while True:
        b = n & 0x7F
        n >>= 7
        out.append(b | (0x80 if n else 0))
        if not n:
            return bytes(out)


def _vec(items: list[bytes]) -> bytes:
    return _leb(len(items)) + b"".join(items)


def _name(s: str) -> bytes:
    return _leb(len(s)) + s.encode()


def _section(sid: int, body: bytes) -> bytes:
    return bytes([sid]) + _leb(len(body)) + body


def hil_payload(message: str = "hil: started") -> bytes:
    """A WASM payload whose `init` logs `message` through mw.log, and nothing else.

    Equivalent to:
      (module (import "mw" "log" (func $log (param i32 i32)))
        (memory (export "memory") 1 1)
        (data (i32.const 16) "<message>")
        (func (export "init") (call $log (i32.const 16) (i32.const <len>))))
    """
    msg = message.encode()
    i32 = 0x7F
    types = _section(1, _vec([bytes([0x60]) + _vec([bytes([i32]), bytes([i32])]) + _vec([]),
                              bytes([0x60]) + _vec([]) + _vec([])]))
    imports = _section(2, _vec([_name("mw") + _name("log") + b"\x00" + _leb(0)]))
    funcs = _section(3, _vec([_leb(1)]))
    memory = _section(5, _vec([b"\x01" + _leb(1) + _leb(1)]))
    exports = _section(7, _vec([_name("memory") + b"\x02" + _leb(0),
                                _name("init") + b"\x00" + _leb(1)]))
    code_body = b"\x00" + b"\x41" + _leb(16) + b"\x41" + _leb(len(msg)) + b"\x10" + _leb(0) + b"\x0b"
    code = _section(10, _vec([_leb(len(code_body)) + code_body]))
    data = _section(11, _vec([b"\x00" + b"\x41" + _leb(16) + b"\x0b" + _leb(len(msg)) + msg]))
    return b"\x00asm\x01\x00\x00\x00" + types + imports + funcs + memory + exports + code + data


# ---------------------------------------------------------------- install
#
# The install steps themselves are justfile recipes (`just provision-<chip>`),
# so a person provisions a board with the same commands the tests run.


class InstallError(Exception):
    pass


class NotInstalled(Exception):
    """A board this tool finds but does not install: a Linux host, deployed
    with `just os-update` (design.md, Linux A/B root)."""


def esp_chip_of(port: str) -> str | None:
    """The chip in ROM download mode (espflash board-info), for a silent board."""
    r = subprocess.run(
        ["espflash", "board-info", "--port", port, "--non-interactive", "--skip-update-check"],
        capture_output=True, text=True, timeout=60,
    )
    m = re.search(r"Chip type:\s+(esp\w+)", r.stdout + r.stderr, re.I)
    return m.group(1).lower() if m else None


def chip_to_install(board: Board) -> str | None:
    """Which firmware the board needs: from sys/info, its USB id, or the ESP ROM."""
    if board.chip:
        return board.chip
    if board.kind and (board.kind.bootsel or len(board.kind.chips) == 1):
        return next(iter(board.kind.chips))
    if board.transport == "usb" and board.kind and board.kind.family in ("uart-bridge", "esp-usb-jtag"):
        return esp_chip_of(board.address)
    return None


def install_command(board: Board, chip: str) -> list[str] | None:
    """`just provision-<chip> ...` for this board, or None (not installable here)."""
    port, ser = board.address, board.serial or ""
    if chip.startswith("linux-"):
        return None  # the Linux root is updated by `just os-update`
    if chip in ("esp32", "esp32s3", "esp32c3"):
        return ["just", "provision-esp", chip, port]
    if chip == "esp8266":
        return ["just", "provision-esp8266", port]
    if chip == "rp2040":
        return ["just", "provision-rp2040", ser]
    if chip == "rp2350":
        return ["just", "provision-rp2350", port or "-", ser]
    if chip in ("rt1050", "imxrt1052"):
        return ["just", "provision-rt1050", port]
    if chip == "nrf52833":
        # The probe's serial (the board's identity) selects the micro:bit.
        return ["just", "provision-nrf52", ser]
    if chip == "stm32g0b1":
        # The EBB42 is installed over an SWD probe or its ROM's USB DFU
        # (BOOT held at reset); neither is named by the board's own USB id.
        return None
    return None


def reattach(b: Board, mw: Mwctl, seconds: float = 45) -> None:
    """After an install the board may come back on another tty (BOOTSEL ->
    rp-base, a re-enumerated USB CDC): find it by USB serial, then ask it."""
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        if b.transport == "usb" and b.serial:
            for t in usb_ttys():
                kind = usb_kind(t["vid"], t["pid"], t["product"], t["interface"])
                if t["serial"].lower() == b.serial.lower() and kind and not kind.bootsel:
                    b.address, b.kind = t["path"], kind
        if b.address:
            try:
                b.info, b.error = mw.info(b), None
                return
            except Exception as e:  # noqa: BLE001
                b.error = str(e)
        time.sleep(1)


def install(b: Board, mw: Mwctl, build: bool) -> str:
    """Builds (optionally) and installs the current firmware; returns the chip."""
    chip = chip_to_install(b)
    if chip is None:
        raise InstallError(f"cannot tell which firmware {b.address or b.usb_path} needs: {b.error}")
    if chip.startswith("linux-"):
        raise NotInstalled(f"{chip}: deploy with `just os-update` (the Linux root is an A/B update)")
    cmd = install_command(b, chip)
    if cmd is None:
        raise InstallError(f"{chip}: no bootloader install path")
    if build:
        r = subprocess.run(["just", "fw-build", chip], cwd=REPO)
        if r.returncode != 0:
            raise InstallError(f"{chip}: just fw-build {chip} failed")
    env = dict(os.environ, MWCTL=mw.exe)
    r = subprocess.run(cmd, cwd=REPO, env=env, timeout=900)
    if r.returncode != 0:
        raise InstallError(f"{chip}: {' '.join(cmd)} failed (exit {r.returncode})")
    reattach(b, mw)
    return chip


# ---------------------------------------------------------------- discover


def build_mwctl() -> Mwctl:
    subprocess.run(
        # auth: `mwctl auth token` for enrolment through the admin server.
        ["cargo", "build", "--quiet", "--release", "-p", "mwctl", "--features", "zenoh,auth"],
        cwd=REPO, check=True,
    )
    return Mwctl(str(REPO / "target/release/mwctl"))


def dnssd_browse(seconds: float = 3.0) -> list[Board]:
    """`_mw-node._tcp` announcements on the link as boards; none when the
    multicast socket cannot be opened."""
    try:
        return boards_from_dnssd(_load_mdns().browse(seconds, "_mw-node._tcp"))
    except OSError:
        return []


def discover(
    mw: Mwctl,
    usb: bool = True,
    lan: bool = True,
    scout_seconds: int = 3,
    mdns: bool = True,
    scout: bool = True,
) -> list[Board]:
    """Every supported board on USB and the LAN, each asked for sys/info.
    A silent board stays in the list with `error` set."""
    found: list[Board] = []
    if usb:
        only = {p for p in os.environ.get("MW_HIL_PORTS", "").split(",") if p}
        found += [b for b in usb_candidates(usb_ttys(), usb_devices()) if not only or b.address in only]
    if lan:
        # Zenoh scouting and DNS-SD listen at the same time: one window, not two.
        from concurrent.futures import ThreadPoolExecutor

        if scout:
            with ThreadPoolExecutor(max_workers=2) as pool:
                scouted = pool.submit(mw.scout, scout_seconds)
                announced = pool.submit(dnssd_browse, scout_seconds) if mdns else None
                found += scouted.result()
                if announced is not None:
                    found += announced.result()
        found += parse_lan_env(os.environ.get("MW_HIL_LAN", ""))
    seen, out = set(), []
    for b in found:
        if not (b.kind and b.kind.bootsel):
            try:
                b.info = mw.info(b)
            except Exception as e:  # noqa: BLE001 - reported per board
                b.error = str(e)
        key = (b.info or {}).get("node") or (b.transport, b.address, b.serial)
        if key in seen:  # one board over two links (VCOM and USB CDC, or USB and LAN)
            continue
        seen.add(key)
        out.append(b)
    return out


def needs_firmware(b: Board) -> bool:
    """Silent, in BOOTSEL, or on another ABI than this middleware."""
    return b.info is None or "expects" in b.info.get("ABI", "")
