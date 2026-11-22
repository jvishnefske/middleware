"""Unprovision one board: remove its pinned key (the trust record).

    just unprovision                 # list the boards with index numbers
    just unprovision 2               # the board with index 2
    just unprovision /dev/ttyACM0    # or by name: tty, host, USB serial, node, chip

A target is required: with none, the boards are listed with their index
numbers and nothing else happens. A name or index that matches no board, or
more than one, is an error. The erase itself is per chip (the trust record
sits at a different flash address on each); it is refused for every chip
until that path is written and approved.
"""

from __future__ import annotations

import argparse
import os
import subprocess
import sys
import time

import boards as B
import provision


class UnprovisionError(Exception):
    pass


def numbered(boards: list[B.Board]) -> list[str]:
    """One line per board, its index (1-based) first."""
    return [f"{i:>3}  {provision.row(b).strip()}" for i, b in enumerate(boards, 1)]


def select(boards: list[B.Board], target: str) -> B.Board:
    """The board `target` names: a 1-based index, or a name matching the
    board's tty or host, USB serial, LAN node, chip or id (case-insensitive).
    Raises UnprovisionError for no match or an ambiguous name."""
    if target.isdigit():
        i = int(target)
        if not 1 <= i <= len(boards):
            raise UnprovisionError(f"no board with index {i} (there are {len(boards)})")
        return boards[i - 1]
    t = target.lower()
    hits = [
        b
        for b in boards
        if t in {
            (b.address or "").lower(),
            (b.serial or "").lower(),
            (b.node or "").lower(),
            (b.chip or "").lower(),
            b.id.lower(),
        }
    ]
    if not hits:
        raise UnprovisionError(f"no board named {target!r}; list them with: just unprovision")
    if len(hits) > 1:
        raise UnprovisionError(
            f"{target!r} names {len(hits)} boards; use the index instead: "
            + ", ".join(str(boards.index(b) + 1) for b in hits)
        )
    return hits[0]


#: The RT1050's trust record: two 256 KiB HyperFlash blocks at 0xC0000
#: (firmware-imxrt/src/layout.rs). The update record below it and the payload
#: store above it are outside this range.
RT1050_TRUST = (0xC0000, 0x80000)
FLASH_PY = B.REPO / "firmware-imxrt" / "tools" / "flash.py"


def erase_command(first: int, length: int) -> list[str]:
    """The SWD erase-only run of the HyperFlash programmer (flash.py)."""
    return [sys.executable, "-I", str(FLASH_PY), "--erase", f"{first:#x}:{length:#x}"]


#: flash.py's SWD backend. The default probe-rs backend failed to connect to
#: this board's J-Link (the erase never started), so the erase always uses the
#: SEGGER server.
ERASE_ENV = {"IMXRT_GDB": "jlink"}
#: How long the board may take to answer sys/info after the erase.
SETTLE_SECONDS = 60
RECOVERY = (
    "the board does not answer sys/info after the erase. Re-flash the bootloader and slot A "
    "from middleware/firmware-imxrt with `./build.sh flash boot a` (flash.py reads each image "
    "back and verifies it), then check with `mwctl info`."
)
#: The update record's version floor (0x40000) is not part of the trust record,
#: so an unpin does not lower it.
FLOOR_CAVEAT = (
    "note: the update record at 0x40000 keeps the version floor. An unpin does not clear it, "
    "so an image below the floor is still refused after the erase."
)


def wait_for_info(mw, board: B.Board, seconds: float = SETTLE_SECONDS, sleep=None, clock=None) -> bool:
    """True once the board answers sys/info, False if it stays silent for `seconds`."""
    # The clock functions are looked up at call time, so a test can replace them.
    sleep = sleep or time.sleep
    clock = clock or time.monotonic
    deadline = clock() + seconds
    while True:
        try:
            mw.info(board)
            return True
        except RuntimeError:
            if clock() >= deadline:
                return False
            sleep(2)


def unpin(board: B.Board, mw=None) -> None:
    """Erase the board's trust record, so it reads Unclaimed on the next boot.
    Only the RT1050 has a path so far: its record is erased over SWD by
    firmware-imxrt/tools/flash.py (the board's serial port is not used).
    With `mw`, the board must then answer sys/info within SETTLE_SECONDS;
    if it does not, the erase is reported as not successful and the recovery
    step is printed."""
    chip = board.chip or (board.kind.family if board.kind else "unknown")
    if chip not in ("rt1050", "imxrt1052"):
        raise UnprovisionError(
            f"erasing the trust record on {chip} is not implemented yet: "
            "the record's flash address and erase path are per chip (see "
            "middleware/firmware-imxrt/src/layout.rs and firmware-rp/src/layout.rs)"
        )
    env = {**os.environ, **ERASE_ENV}
    r = subprocess.run(erase_command(*RT1050_TRUST), cwd=B.REPO, timeout=900, env=env)
    if r.returncode != 0:
        raise UnprovisionError(f"erase over SWD failed (flash.py exit {r.returncode}); see its output above. The record is erased only when flash.py reports the range reads back as 0xFF")
    if mw is not None and not wait_for_info(mw, board):
        raise UnprovisionError(f"trust record erased, but {RECOVERY}")
    answered = " and the board answers sys/info" if mw is not None else ""
    print(f"trust record erased (reads back as 0xFF){answered}")
    print(FLOOR_CAVEAT)


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("target", nargs="?", help="board index (from the list) or name")
    ap.add_argument("--no-lan", action="store_true")
    a = ap.parse_args(argv)
    sys.stdout.reconfigure(line_buffering=True)
    mw = B.build_mwctl()
    boards = B.discover(mw, lan=not a.no_lan)
    if a.target is None:
        print(f"{len(boards)} board(s) found; pick one: just unprovision <index|name>")
        for line in numbered(boards):
            print(line)
        return 2
    try:
        board = select(boards, a.target)
        print(f"unprovisioning {board.id} ({board.address or board.serial})")
        unpin(board, mw)
    except UnprovisionError as e:
        print(f"unprovision: {e}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
