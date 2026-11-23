"""The middleware checks run against every board conftest.py found."""

from __future__ import annotations

import re
import subprocess
import time

import pytest

import boards as B
from boards import Board, Mwctl


def need_info(board: Board) -> dict[str, str]:
    if board.info is None:
        pytest.skip("no sys/info (see test_answers_sys_info)")
    return board.info


def test_answers_sys_info(board: Board):
    if board.info is None:
        where = board.address or board.usb_path
        pytest.fail(f"{where} did not answer sys/info: {board.error} (run `just provision`)")
    info = board.info
    for field in ("chip", "firmware", "ABI", "payload capacity", "payload status"):
        assert field in info, f"sys/info has no {field!r}: {info}"


def test_chip_is_supported_and_matches_the_usb_id(board: Board):
    chip = need_info(board)["chip"]
    supported = B.WASM_CHIPS | B.PROTOCOL_ONLY_CHIPS
    assert chip in supported or chip.startswith("linux-"), f"unknown chip {chip!r}"
    if board.kind is not None:
        assert chip in board.kind.chips, f"{board.kind.family} USB id but sys/info says {chip!r}"


def test_abi_matches_this_middleware(board: Board):
    abi = need_info(board)["ABI"]
    assert "expects" not in abi, f"firmware ABI {abi}: run `just provision`"


def test_node_id(board: Board):
    info = need_info(board)
    node = info.get("node")
    if node is None:
        assert info["chip"] in ("esp8266", "ecp5", "rp2040"), "sys/info carries no node id"
        return
    assert re.fullmatch(r"[0-9a-f]{16}", node), node
    if board.node:
        assert node == board.node, f"asked node {board.node}, {node} answered"


def test_runtime_matches_the_chip(board: Board):
    info = need_info(board)
    runs = info["runtime"].startswith("WASM")
    chip = info["chip"]
    if chip in B.WASM_CHIPS or chip.startswith("linux-"):
        assert runs, f"{chip} should run payloads: {info['runtime']}"
    elif chip in B.PROTOCOL_ONLY_CHIPS:
        assert not runs, f"{chip} has no runtime but claims {info['runtime']}"


def test_payload_capacity(board: Board):
    info = need_info(board)
    cap = info["payload capacity"]
    m = re.match(r"(\d+) bytes", cap)
    if m and int(m.group(1)) == 0 and info["chip"] in B.PROTOCOL_ONLY_CHIPS:
        # A serial-only base firmware (micro:bit v2) has no payload runtime.
        pytest.skip(f"{info['chip']} is protocol-only: no payload runtime")
    assert m and int(m.group(1)) > 0, cap


def test_trust_info(board: Board, mwctl: Mwctl):
    need_info(board)
    r = mwctl.run(board, "trust", "info")
    if r.returncode != 0 and "built without `trust`" in r.stderr:
        # The firmware reports it was built without signed payloads.
        pytest.skip("firmware built without `trust`")
    assert r.returncode == 0, r.stderr
    assert re.search(r"trust:\s+(unclaimed|claimed|pinned)", r.stdout), r.stdout


def test_manifest(board: Board, mwctl: Mwctl):
    need_info(board)
    r = mwctl.run(board, "manifest")
    assert r.returncode == 0, r.stderr
    assert "groups:" in r.stdout, r.stdout


def test_board_description(board: Board, mwctl: Mwctl):
    info = need_info(board)
    if info["chip"] == "ecp5" or info["chip"].startswith("linux-"):
        pytest.skip(f"{info['chip']} has no pin board description")
    r = mwctl.run(board, "board")
    assert r.returncode == 0, r.stderr
    assert r.stdout.strip(), "empty sys/board"


def test_firmware_update_state(board: Board, mwctl: Mwctl):
    chip = need_info(board)["chip"]
    if chip not in B.A_B_CHIPS:
        pytest.skip(f"{chip} has no A/B firmware updates")
    r = mwctl.run(board, "fw", "info")
    assert r.returncode == 0, f"sys/fw/info: {r.stderr.strip()}"
    assert "slot" in r.stdout.lower(), r.stdout


@pytest.fixture
def writable(board: Board, mwctl: Mwctl, pytestconfig) -> list[str]:
    """Signing arguments, when this run may replace the board's payload."""
    if not pytestconfig.getoption("--mw-write"):
        pytest.skip("replaces the board's payload: pass --mw-write")
    info = need_info(board)
    if info["chip"] == "ecp5":
        pytest.skip("the 5A-75B WASM core is loaded with the gateware")
    r = mwctl.run(board, "trust", "info")
    claimed = "unclaimed" not in r.stdout
    sign = B.signing_args()
    if (claimed or board.transport != "usb") and not sign:
        pytest.skip("board is claimed (or remote): set MW_HIL_KEY or MW_HIL_PKCS11")
    return sign


def test_payload_upload_start_stop(board: Board, mwctl: Mwctl, writable: list[str], tmp_path):
    wasm = tmp_path / "hil.wasm"
    wasm.write_bytes(B.hil_payload("hil: started"))
    r = mwctl.run(board, "upload", str(wasm), *writable, timeout=180)
    assert r.returncode == 0, f"upload: {r.stderr}"
    if not need_info(board)["runtime"].startswith("WASM"):
        # Protocol-only boards store the payload and never run it.
        assert mwctl.info(board)["payload status"] in ("stopped", "empty")
        return

    control = writable if board.transport != "usb" else []
    if board.transport == "usb":
        # init logs once; start it while monitor holds the port to see it.
        mon = subprocess.Popen(
            [mwctl.exe, *board.link, "monitor", "--seconds", "6"],
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
        )
        time.sleep(1.5)
        r = mwctl.run(board, "start")
        out, err = mon.communicate(timeout=30)
        assert r.returncode == 0, f"start: {r.stderr}"
        assert "[log] hil: started" in out, f"payload log not seen:\n{out}\n{err}"
    else:
        r = mwctl.run(board, "start", *control)
        assert r.returncode == 0, f"start: {r.stderr}"
    assert mwctl.info(board)["payload status"] == "running"

    r = mwctl.run(board, "stop", *control)
    assert r.returncode == 0, f"stop: {r.stderr}"
    assert mwctl.info(board)["payload status"] == "stopped"


def test_open_reliability(board: Board, mwctl: Mwctl):
    """Every fresh `mwctl info` must get a reply. The micro:bit VCOM glitched the nRF
    RX on a lone 0x00 sync write (design.md, "micro:bit v2 base firmware"); this
    guards the host write shape that fixed it."""
    if need_info(board)["chip"] != "nrf52833":
        pytest.skip("open-reliability regression is for the micro:bit VCOM (nrf52833)")
    misses = []
    for i in range(1, 21):
        r = mwctl.run(board, "info", timeout=30)
        if r.returncode != 0 or "chip:" not in r.stdout:
            misses.append(i)
    assert not misses, f"{len(misses)}/20 fresh opens got no sys/info reply (runs {misses})"


def test_linux_board_reports_its_payload_slot_and_root_state(board: Board, mwctl: Mwctl):
    """A Linux board (posix-node, the KD240) reports its payload slot; with a
    Linux A/B root (design.md, "Linux A/B root (OS)") it answers `os info`."""
    info = need_info(board)
    if not info["chip"].startswith("linux-"):
        pytest.skip("not a Linux board")
    assert info["payload status"] in ("stopped", "empty", "running"), info
    r = mwctl.run(board, "os", "info")
    if r.returncode != 0:
        pytest.skip(f"no Linux root update on this board: {(r.stderr or r.stdout).strip()}")
    fields = B.parse_fields(r.stdout)
    assert fields.get("running") in ("a", "b"), fields
    assert fields.get("state") in ("idle", "receiving", "committed", "trial", "confirmed", "rolled back"), fields
    assert fields.get("state") != "trial", "a root on trial: `just os-update` confirms it, or `os abort`"
