"""Find every supported board and install the current firmware on those that
need it (silent, in BOOTSEL, or on another ABI), through `just provision-<chip>`.

    just provision               # list, then provision the boards that need it
    just provision --build       # build each chip's firmware first
    just provision --all         # every board, even ones already current
    just provision --list        # only list
"""

from __future__ import annotations

import argparse
import sys

import boards as B


def row(b: B.Board) -> str:
    i = b.info or {}
    where = b.address or f"BOOTSEL {b.serial or b.usb_path}"
    if b.info is None:
        state = "BOOTSEL" if b.kind and b.kind.bootsel else f"silent ({(b.error or '')[:60]})"
    else:
        state = f"{i.get('firmware')} ABI {i.get('ABI')} payload {i.get('payload status')}"
    return f"  {b.transport:<5} {where:<22} {b.chip or (b.kind.family if b.kind else '?'):<14} {state}"


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--build", action="store_true", help="just fw-build <chip> before installing")
    ap.add_argument("--all", action="store_true", help="also boards already on this ABI")
    ap.add_argument("--list", action="store_true", help="list only")
    ap.add_argument("--no-lan", action="store_true")
    ap.add_argument("--mwctl", help="mwctl binary [default: build it]")
    a = ap.parse_args()
    sys.stdout.reconfigure(line_buffering=True)  # keep our lines in order with the recipes'

    mw = B.Mwctl(a.mwctl) if a.mwctl else B.build_mwctl()
    boards = B.discover(mw, lan=not a.no_lan)
    print(f"{len(boards)} board(s) found:")
    for b in boards:
        print(row(b))
    if a.list:
        return 0
    todo = [b for b in boards if a.all or B.needs_firmware(b)]
    if not todo:
        print("nothing to provision")
        return 0
    failed = 0
    built: set[str] = set()
    for b in todo:
        print(f"\n== provisioning {b.address or b.serial}")
        try:
            chip = B.chip_to_install(b)
            B.install(b, mw, build=a.build and chip not in built)
            built.add(chip or "")
            print(f"ok: {row(b).strip()}")
            if B.needs_firmware(b):
                print("  still not answering on this ABI after install")
                failed += 1
        except B.InstallError as e:
            print(f"FAILED: {e}\n  (the lines marked !!! or >>> above say what to do)")
            failed += 1
    if failed:
        print(f"\n{failed} board(s) not provisioned; after fixing, rerun: just provision")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
