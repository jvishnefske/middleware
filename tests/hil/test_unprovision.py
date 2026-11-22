"""`unprovision` picks one board by index or name, and never erases without a
per-chip path. Pure: no board is touched."""

from __future__ import annotations

import pytest

import boards as B
import unprovision as U


def usb(address: str, serial: str, chip: str) -> B.Board:
    return B.Board(transport="usb", address=address, serial=serial, info={"chip": chip})


BOARDS = [
    usb("/dev/ttyACM0", "0123ABCD", "imxrt1052"),
    usb("/dev/ttyACM1", "9999FFFF", "nrf52833"),
    B.Board(transport="zenoh", address="tcp/10.0.0.5:7447", node="00ff00ff00ff00ff", info={"chip": "rp2350"}),
]


def test_index_is_one_based():
    assert U.select(BOARDS, "1") is BOARDS[0]
    assert U.select(BOARDS, "3") is BOARDS[2]


def test_index_out_of_range_is_an_error():
    for bad in ("0", "4"):
        with pytest.raises(U.UnprovisionError, match="no board with index"):
            U.select(BOARDS, bad)


def test_name_matches_tty_serial_node_chip_or_id():
    assert U.select(BOARDS, "/dev/ttyACM1") is BOARDS[1]
    assert U.select(BOARDS, "0123abcd") is BOARDS[0]  # USB serial, any case
    assert U.select(BOARDS, "00FF00FF00FF00FF") is BOARDS[2]  # LAN node
    assert U.select(BOARDS, "nrf52833") is BOARDS[1]  # chip
    assert U.select(BOARDS, BOARDS[2].id) is BOARDS[2]


def test_unknown_name_is_an_error_that_says_how_to_list():
    with pytest.raises(U.UnprovisionError, match="just unprovision"):
        U.select(BOARDS, "/dev/ttyUSB9")


def test_ambiguous_name_asks_for_the_index():
    same = [usb("/dev/ttyACM0", "A", "rp2350"), usb("/dev/ttyACM1", "B", "rp2350")]
    with pytest.raises(U.UnprovisionError, match="use the index instead: 1, 2"):
        U.select(same, "rp2350")


def test_listing_numbers_every_board_from_one():
    lines = U.numbered(BOARDS)
    assert [line.split()[0] for line in lines] == ["1", "2", "3"]
    assert "/dev/ttyACM0" in lines[0]


def test_unknown_chip_is_refused():
    odd = B.Board(transport="usb", address="/dev/ttyACM9", info={"chip": "esp8266"})
    with pytest.raises(U.UnprovisionError, match="not implemented yet"):
        U.unpin(odd)


def test_rt1050_erase_command_covers_only_the_trust_record():
    first, length = U.RT1050_TRUST
    assert (first, length) == (0xC0000, 0x80000)
    cmd = U.erase_command(first, length)
    assert cmd[-2:] == ["--erase", "0xc0000:0x80000"]
    assert cmd[2].endswith("firmware-imxrt/tools/flash.py")
    # The update record (0x40000) and the payload store (0x140000) lie outside it.
    assert 0x40000 < first and first + length <= 0x140000


def test_rt1050_unpin_runs_the_erase_and_reports_failure(monkeypatch):
    calls = []

    class Done:
        returncode = 0

    monkeypatch.setattr(U.subprocess, "run", lambda cmd, **kw: calls.append((cmd, kw)) or Done())
    U.unpin(BOARDS[0])
    assert calls and calls[0][0] == U.erase_command(*U.RT1050_TRUST)
    assert calls[0][1]["cwd"] == B.REPO

    class Failed:
        returncode = 1

    monkeypatch.setattr(U.subprocess, "run", lambda cmd, **kw: Failed())
    with pytest.raises(U.UnprovisionError, match="reads back as 0xFF"):
        U.unpin(BOARDS[0])


def test_other_chips_are_still_refused_without_running_anything(monkeypatch):
    def boom(*a, **k):
        raise AssertionError("no subprocess for an unimplemented chip")

    monkeypatch.setattr(U.subprocess, "run", boom)
    with pytest.raises(U.UnprovisionError, match="not implemented yet"):
        U.unpin(BOARDS[1])  # nrf52833


class FakeMw:
    """Answers sys/info after `silent` failed attempts, or never (silent=None)."""

    def __init__(self, silent=0):
        self.silent = silent
        self.calls = 0

    def info(self, board):
        self.calls += 1
        if self.silent is None or self.calls <= self.silent:
            raise RuntimeError("no response")
        return {}


class Clock:
    def __init__(self):
        self.t = 0.0

    def __call__(self):
        return self.t

    def sleep(self, s):
        self.t += s


class Done:
    def __init__(self, code=0):
        self.returncode = code


def test_rt1050_erase_runs_with_the_jlink_backend(monkeypatch):
    seen = {}

    def run(cmd, **kw):
        seen.update(kw)
        return Done()

    monkeypatch.setattr(U.subprocess, "run", run)
    U.unpin(BOARDS[0], FakeMw(silent=0))
    assert seen["env"]["IMXRT_GDB"] == "jlink"


def test_a_silent_board_after_the_erase_is_not_success(monkeypatch, capsys):
    monkeypatch.setattr(U.subprocess, "run", lambda cmd, **kw: Done())
    clock = Clock()
    monkeypatch.setattr(U.time, "monotonic", clock)
    monkeypatch.setattr(U.time, "sleep", clock.sleep)
    with pytest.raises(U.UnprovisionError) as e:
        U.unpin(BOARDS[0], FakeMw(silent=None))
    msg = str(e.value)
    assert "does not answer sys/info" in msg
    assert "./build.sh flash boot a" in msg
    assert "erased" in msg  # the erase happened: said, not hidden
    assert "trust record erased (reads back" not in capsys.readouterr().out


def test_a_board_that_answers_after_a_moment_is_success(monkeypatch, capsys):
    monkeypatch.setattr(U.subprocess, "run", lambda cmd, **kw: Done())
    clock = Clock()
    monkeypatch.setattr(U.time, "monotonic", clock)
    monkeypatch.setattr(U.time, "sleep", clock.sleep)
    U.unpin(BOARDS[0], FakeMw(silent=3))
    out = capsys.readouterr().out
    assert "answers sys/info" in out
    assert "version floor" in out  # the caveat is always printed


def test_the_wait_is_bounded_by_settle_seconds():
    clock = Clock()
    mw = FakeMw(silent=None)
    assert U.wait_for_info(mw, BOARDS[0], sleep=clock.sleep, clock=clock) is False
    assert clock.t >= U.SETTLE_SECONDS
    assert mw.calls < 100


def test_a_failed_erase_does_not_wait_for_the_board(monkeypatch):
    monkeypatch.setattr(U.subprocess, "run", lambda cmd, **kw: Done(1))
    mw = FakeMw(silent=None)
    with pytest.raises(U.UnprovisionError, match="erase over SWD failed"):
        U.unpin(BOARDS[0], mw)
    assert mw.calls == 0
