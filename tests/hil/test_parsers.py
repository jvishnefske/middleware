"""The discovery and parsing helpers, without hardware (run with
`pytest tests/hil/test_parsers.py --mw-no-usb --mw-no-lan`, or always: these
tests take no `board`)."""

import subprocess
from pathlib import Path

import pytest

import boards as B

SCOUT = """\
scouting Zenoh on the local network for 3 s ...
KIND    ZENOH ID  ADDRESS             VIA  NODE              CHIP       FIRMWARE / PAYLOAD
router  r1        tcp/10.0.0.1:7447   -    -                 -          -
client  c9        tcp/10.0.0.9:50123  r1   310441d265f82484  imxrt1052  0.6.0 / running
peer    p2        tcp/10.0.0.5:7447   -    1111222233334444  linux-aarch64  0.1.0 / empty
2 Zenoh sessions, 1 middleware node answered sys/info
"""


def test_scout_rows_become_boards_with_a_router_to_reach_them():
    got = [(b.transport, b.address, b.node) for b in B.parse_scout(SCOUT)]
    assert got == [
        ("zenoh", "tcp/10.0.0.1:7447", "310441d265f82484"),
        ("zenoh", "tcp/10.0.0.5:7447", "1111222233334444"),
    ]


def test_scout_with_nothing_found():
    assert B.parse_scout("0 Zenoh sessions, 0 middleware nodes answered sys/info\n") == []


def test_lan_env():
    got = B.parse_lan_env("udp:192.168.1.50, zenoh:tcp/127.0.0.1:7447@056E9F229064463F")
    assert [(b.transport, b.address, b.node) for b in got] == [
        ("udp", "192.168.1.50", None),
        ("zenoh", "tcp/127.0.0.1:7447", "056e9f229064463f"),
    ]
    assert got[1].link == ["--zenoh", "tcp/127.0.0.1:7447", "--node", "056e9f229064463f"]


def test_info_fields():
    info = B.parse_fields(
        "chip:             rp2350\nfirmware:         0.1.0\nnode:             056e9f229064463f\n"
        "runtime:          WASM (payloads run)\nABI:              v4 (mwctl expects v9)\n"
        "payload capacity: 65536 bytes\npayload status:   running\n[pub] app/blink/state: 1\n"
    )
    assert info["chip"] == "rp2350"
    assert info["ABI"] == "v4 (mwctl expects v9)"
    assert info["payload capacity"] == "65536 bytes"
    assert "[pub] app/blink/state" not in info


def _fake_sysfs(root: Path, name: str, vid: str, pid: str, product: str, serial: str, iface_no: int = 0) -> None:
    """A tty under a USB device, as sysfs lays it out: class/tty/<name>/device
    links to the interface directory (X-Y:1.N, with bInterfaceNumber)."""
    usb = root / "devices/usb1/1-1"
    iface = usb / f"1-1:1.{iface_no}"
    (iface / name).mkdir(parents=True)
    (iface / "bInterfaceNumber").write_text(f"{iface_no:02x}\n")
    for k, v in (("idVendor", vid), ("idProduct", pid), ("product", product), ("serial", serial)):
        (usb / k).write_text(v + "\n")
    tty = root / "class/tty" / name
    tty.mkdir(parents=True)
    (tty / "device").symlink_to(iface)


def test_usb_ttys_are_matched_by_id_and_others_ignored(tmp_path):
    _fake_sysfs(tmp_path / "a", "ttyACM2", "2e8a", "0009", "rp-base", "056E9F229064463F")
    _fake_sysfs(tmp_path / "b", "ttyACM0", "1234", "5678", "some other device", "0000")
    ttys = B.usb_ttys(tmp_path / "a") + B.usb_ttys(tmp_path / "b")
    assert [t["path"] for t in ttys] == ["/dev/ttyACM2", "/dev/ttyACM0"]
    boards = B.usb_candidates(ttys, [])
    assert [(b.address, b.kind and b.kind.family, b.serial) for b in boards] == [
        ("/dev/ttyACM2", "rp-base", "056E9F229064463F")
    ]


def test_microbit_daplink_vcom_is_a_candidate(tmp_path):
    _fake_sysfs(tmp_path / "m", "ttyACM0", "0d28", "0204", "BBC micro:bit CMSIS-DAP", "9906")
    (b,) = B.usb_candidates(B.usb_ttys(tmp_path / "m"), [])
    assert b.kind is not None and b.kind.family == "microbit-daplink"
    assert b.address == "/dev/ttyACM0" and b.serial == "9906"


def test_bootsel_devices_are_candidates_without_a_tty():
    dev = {"vid": 0x2E8A, "pid": 0x000F, "product": "RP2350 Boot", "serial": "ABC", "usb": "/sys/x"}
    (b,) = B.usb_candidates([], [dev])
    assert b.kind is not None and b.kind.bootsel and b.address == "" and b.serial == "ABC"


def test_hil_payload_runs_in_payload_sim(tmp_path):
    """The hand-assembled payload loads and logs from init (tools/payload-sim, wasmi)."""
    f = tmp_path / "hil.wasm"
    f.write_bytes(B.hil_payload("hil: started"))
    r = subprocess.run(
        ["cargo", "run", "--quiet", "-p", "payload-sim", "--", str(f)],
        cwd=B.REPO, capture_output=True, text=True, timeout=600,
    )
    assert r.returncode == 0, r.stderr
    assert "log hil: started" in r.stdout + r.stderr


def _mdns():
    return B._load_mdns()


def test_dnssd_instances_become_zenoh_boards():
    M = _mdns()

    svc = [
        M.Service("77b67d8e9011adef", "kd240", 7447, ("192.168.1.134", "fe80::1")),
        M.Service("0123456789abcdef", "pi", 7447, ()),  # no address: the host name
        M.Service("nothex", "x", 7447, ("10.0.0.2",)),  # not a node instance
        M.Service("aaaaaaaaaaaaaaaa", "y", None, ("10.0.0.3",)),  # no port
    ]
    boards = B.boards_from_dnssd(svc)
    assert [(b.transport, b.address, b.node) for b in boards] == [
        ("zenoh", "tcp/192.168.1.134:7447", "77b67d8e9011adef"),
        ("zenoh", "tcp/pi:7447", "0123456789abcdef"),
    ]


def test_dnssd_ipv6_only_address_is_bracketed():
    M = _mdns()

    (b,) = B.boards_from_dnssd([M.Service("77b67d8e9011adef", "h", 7447, ("fe80::1",))])
    assert b.address == "tcp/[fe80::1]:7447"


def test_dnssd_parses_a_wire_answer():
    """A PTR/SRV/A answer for one instance, built as posix-node announces it."""
    import struct

    M = _mdns()

    def name(n):
        return b"".join(bytes([len(x)]) + x.encode() for x in n.split(".")) + b"\x00"

    def rr(owner, rtype, rdata):
        return name(owner) + struct.pack(">HHIH", rtype, 1, 120, len(rdata)) + rdata

    inst = "77b67d8e9011adef._mw-node._tcp.local"
    srv = struct.pack(">HHH", 0, 0, 7447) + name("kd240.local")
    msg = struct.pack(">HHHHHH", 0, 0x8400, 0, 3, 0, 0)
    msg += rr("_mw-node._tcp.local", M.PTR, name(inst))
    msg += rr(inst, M.SRV, srv)
    msg += rr("kd240.local", M.A, bytes([192, 168, 1, 134]))
    (svc,) = M.parse([msg], "_mw-node._tcp.local")
    assert svc == M.Service("77b67d8e9011adef", "kd240.local", 7447, ("192.168.1.134",))
    assert B.boards_from_dnssd([svc])[0].address == "tcp/192.168.1.134:7447"


def test_kd240_console_is_matched_by_product_and_interface_only(tmp_path):
    """0403:6011 is also the Colorlight's FT4232H: only the KD carrier's
    interface 01 is the KD240 console; the Colorlight's ports are never probed."""
    _fake_sysfs(tmp_path / "kd", "ttyUSB1", "0403", "6011", "KD Carrier Card", "XFL1HM3KDKK1", iface_no=1)
    _fake_sysfs(tmp_path / "kd0", "ttyUSB0", "0403", "6011", "KD Carrier Card", "XFL1HM3KDKK1", iface_no=0)
    _fake_sysfs(tmp_path / "col", "ttyUSB1", "0403", "6011", "FT4232H MiniModule", "FT4YW8B7", iface_no=1)
    kd = B.usb_ttys(tmp_path / "kd")  # ttyUSB1, interface 1: the console
    kd0 = B.usb_ttys(tmp_path / "kd0")  # ttyUSB0, interface 0: another FTDI channel
    col = B.usb_ttys(tmp_path / "col")  # the Colorlight's, also ttyUSB1
    assert kd[0]["interface"] == 1 and kd0[0]["interface"] == 0
    kd_boards = B.usb_candidates(kd + kd0, [])
    assert [(b.address, b.kind.family, b.serial) for b in kd_boards] == [
        ("/dev/ttyUSB1", "kd240-console", "XFL1HM3KDKK1")
    ]
    # The Colorlight (same ids, other product, same interface 01) is not a candidate.
    assert B.usb_candidates(col, []) == []
    assert B.usb_kind(0x0403, 0x6011, "KD Carrier Card", 0) is None
    assert B.usb_kind(0x0403, 0x6011) is None  # no product: not ours
    assert B.usb_kind(0x0403, 0x6001) is not None  # the generic FT232R bridge


def test_kd240_console_board_is_a_linux_board_not_installed(tmp_path):
    """The KD240 console is found, and provisioning skips it (deployed by
    `just os-update`) rather than failing."""
    _fake_sysfs(tmp_path, "ttyUSB1", "0403", "6011", "KD Carrier Card", "XFL1HM3KDKK1", iface_no=1)
    (b,) = B.usb_candidates(B.usb_ttys(tmp_path), [])
    assert b.kind.family == "kd240-console" and b.address == "/dev/ttyUSB1"
    assert "linux-aarch64" in b.kind.chips
    with pytest.raises(B.NotInstalled, match="just os-update"):
        B.install(b, None, build=False)  # raises before it needs mwctl
