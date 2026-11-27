"""The Kria KD240 on this machine: its console (USB, 0403:6011 "KD Carrier
Card" interface 01) or its posix-node on the LAN (DNS-SD _mw-node._tcp, or
MW_HIL_LAN), asked for sys/info, its manifest's board and sys/os.

    just kd240
"""

from __future__ import annotations

import sys

import boards as B


def is_kd240(b: B.Board) -> bool:
    if b.kind is not None and b.kind.family == "kd240-console":
        return True
    return (b.chip or "").startswith("linux-aarch64")


def main() -> int:
    mw = B.build_mwctl()
    found = [b for b in B.discover(mw) if is_kd240(b)]
    if not found:
        print("no KD240 found: its console on USB, or its posix-node on the LAN", file=sys.stderr)
        return 1
    status = 0
    for b in found:
        where = b.address or b.serial or b.usb_path or b.node
        print(f"== {b.transport} {where}")
        if b.error:
            print(f"  error: {b.error}")
            status = 1
            continue
        for k in ("chip", "firmware", "ABI", "node", "payload status"):
            if k in (b.info or {}):
                print(f"  {k}: {b.info[k]}")
        # A posix-node has no chip profile, so `sys/board` is NotFound there by
        # design (posix-node/src/service.rs): its manifest names the board.
        r = mw.run(b, "manifest", timeout=30)
        named = [ln.split(":", 1)[1].strip() for ln in (r.stdout or "").splitlines() if ln.startswith("board:")]
        if r.returncode == 0 and named:
            print(f"  board: {named[0]}")
        else:
            text = (r.stdout or r.stderr).strip()
            print(f"  board: {text.splitlines()[-1] if text else 'no answer'}")
            status = 1
        # `os info` needs the Linux root's update service (an A/B card,
        # design.md): on a single-root card the key is not served.
        r = mw.run(b, "os", "info", timeout=30)
        text = (r.stdout or r.stderr).strip()
        if r.returncode != 0:
            last = text.splitlines()[-1] if text else "no answer"
            print(f"  os info: {'not an A/B root (sys/os is not served)' if 'NotFound' in last else last}")
        else:
            print("  os info:")
            for ln in text.splitlines():
                print(f"    {ln}")
    return status


if __name__ == "__main__":
    sys.exit(main())
