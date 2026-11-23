"""Installs a Linux root release on the KD240 over the LAN (design.md, "Linux
A/B root (OS)"): the squashfs store is written into the inactive slot over
Zenoh (the serial console would take about 70 hours for a gigabyte), committed
with its root-signed `Os` cert, booted on trial, and confirmed once it runs.

    just os-update <IMAGE> <CERT> <VERSION>

IMAGE is the store (`rootfs.squashfs`), CERT the `mw-release publish --kind os`
output, VERSION the release's version (above the board's floor). The board must
answer on the LAN (its posix-node announces itself, or MW_HIL_LAN names it).

A claimed board takes reboot, confirm and abort over the network only signed by
its pinned key: give MW_HIL_KEY (a key file) or MW_HIL_PKCS11 (a token label).
"""

from __future__ import annotations

import argparse
import sys
import time

import boards as B


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("image")
    ap.add_argument("cert")
    ap.add_argument("version", type=int)
    ap.add_argument("--wait", type=float, default=300, help="seconds for the trial to come up")
    ap.add_argument("--mwctl", help="mwctl binary [default: build it]")
    a = ap.parse_args()

    mw = B.Mwctl(a.mwctl) if a.mwctl else B.build_mwctl()
    lan = [
        b
        for b in B.discover(mw, usb=False)
        if (b.chip or "").startswith("linux-") and b.transport in ("zenoh", "zenoh-listen")
    ]
    if not lan:
        print("no Linux board answers on the LAN: the root goes over Zenoh, not the console", file=sys.stderr)
        return 1
    b = lan[0]
    sign = B.signing_args()
    if not sign:
        print("set MW_HIL_KEY or MW_HIL_PKCS11: reboot and confirm must be signed by the board's key", file=sys.stderr)
        return 1
    print(f"== root {a.image} v{a.version} to {b.id} ({b.address or b.node})")

    r = mw.run(b, "os", "update", a.image, a.cert, "--version", str(a.version), timeout=3600)
    print((r.stdout or r.stderr).strip())
    if r.returncode != 0:
        print("FAILED: os update", file=sys.stderr)
        return 1
    r = mw.run(b, "os", "reboot", *sign)
    if r.returncode != 0:
        print(f"FAILED: os reboot: {(r.stderr or r.stdout).strip()}", file=sys.stderr)
        return 1

    deadline = time.monotonic() + a.wait
    fields: dict[str, str] = {}
    while time.monotonic() < deadline:
        time.sleep(5)
        r = mw.run(b, "os", "info", timeout=20)
        if r.returncode == 0:
            fields = B.parse_fields(r.stdout)
            if fields.get("state") == "trial":
                break
    if fields.get("state") != "trial":
        print(f"FAILED: no trial came up: {fields or 'no answer'}", file=sys.stderr)
        return 1

    r = mw.run(b, "os", "confirm", *sign, timeout=180)
    print((r.stdout or r.stderr).strip())
    return 0 if r.returncode == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
