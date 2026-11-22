"""The discovery and parsing helpers, without hardware (run with
`pytest tests/hil/test_parsers.py --mw-no-usb --mw-no-lan`, or always: these
tests take no `board`)."""

import subprocess
from pathlib import Path

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


def _fake_sysfs(root: Path, name: str, vid: str, pid: str, product: str, serial: str) -> None:
    usb = root / "devices/usb1/1-1"
    iface = usb / "1-1:1.0" / name
    iface.mkdir(parents=True)
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
