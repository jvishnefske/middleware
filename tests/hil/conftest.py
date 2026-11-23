"""Hardware-in-the-loop: find every supported board on USB and the LAN and
check the middleware against it (test_boards.py).

    pytest tests/hil                     # read-only checks on what is running
    pytest tests/hil --mw-write          # also upload, start and stop a payload
    pytest tests/hil --mw-flash          # first install the current firmware
                                         # through each board's bootloader
    pytest tests/hil --mw-flash --mw-build   # build the firmware before installing
    pytest tests/hil -k rp2350           # one kind of board
    pytest tests/hil --mw-no-mdns        # LAN without DNS-SD
    just hil [args]                      # the same

Environment:
    MW_HIL_LAN      extra LAN boards: udp:<host>[:port], zenoh:<ep>@<node16>,
                    zenoh-listen:<ep>@<node16> (see boards.py)
    MW_HIL_KEY      key file that signs uploads/updates for claimed boards
    MW_HIL_PKCS11   or a token key label (e.g. mw-release; PIN from ~/.env)
    MW_HIL_RT_SLOT  the RT1050 slot image to install [default: newest built]
    MW_HIL_PORTS    only these tty paths (comma-separated)

Installing assumes the bootloader is in place: the ESP/ESP8266 ROM, the RP
BOOTSEL ROM or the rp-base A/B partitions, the RT1050's HyperFlash
bootloader. It never writes fuses or OTP (design.md NF1-NF4).
"""

from __future__ import annotations

import pytest

import boards as B


def pytest_addoption(parser):
    g = parser.getgroup("middleware hardware")
    g.addoption("--mw-flash", action="store_true", help="install the current firmware first")
    g.addoption("--mw-build", action="store_true", help="build the firmware before --mw-flash")
    g.addoption("--mw-write", action="store_true", help="upload/start/stop a test payload")
    g.addoption("--mw-no-usb", action="store_true", help="skip USB discovery")
    g.addoption("--mw-no-lan", action="store_true", help="skip LAN discovery")
    g.addoption("--mw-no-mdns", action="store_true", help="skip DNS-SD (_mw-node._tcp) on the LAN")
    g.addoption("--mw-scout-seconds", type=int, default=3, help="Zenoh scouting time")
    g.addoption("--mw-mwctl", default=None, help="mwctl binary [default: build it]")


def _mwctl(config) -> B.Mwctl:
    exe = config.getoption("--mw-mwctl")
    return B.Mwctl(exe) if exe else B.build_mwctl()


def discover(config) -> list[B.Board]:
    return B.discover(
        config._mw_mwctl,
        usb=not config.getoption("--mw-no-usb"),
        lan=not config.getoption("--mw-no-lan"),
        scout_seconds=config.getoption("--mw-scout-seconds"),
        mdns=not config.getoption("--mw-no-mdns"),
    )


def pytest_configure(config):
    config._mw_mwctl = None
    config._mw_boards = None


def pytest_generate_tests(metafunc):
    if "board" not in metafunc.fixturenames:
        return
    config = metafunc.config
    if config._mw_boards is None:
        config._mw_mwctl = _mwctl(config)
        config._mw_boards = discover(config)
    boards = config._mw_boards
    if not boards:
        metafunc.parametrize(
            "board",
            [pytest.param(None, marks=pytest.mark.skip(reason="no supported board on USB or the LAN"))],
            ids=["no-board"],
        )
        return
    metafunc.parametrize("board", boards, ids=[b.id for b in boards], indirect=True, scope="session")


@pytest.fixture(scope="session")
def mwctl(pytestconfig) -> B.Mwctl:
    return pytestconfig._mw_mwctl


@pytest.fixture(scope="session")
def board(request, mwctl) -> B.Board:
    """A discovered board, with the current firmware installed under --mw-flash."""
    b: B.Board = request.param
    if b is None:
        pytest.skip("no board")
    if request.config.getoption("--mw-flash") and not b.installed:
        b.installed = True
        try:
            B.install(b, mwctl, request.config.getoption("--mw-build"))
        except B.InstallError as e:
            pytest.fail(f"install: {e}")
    return b
