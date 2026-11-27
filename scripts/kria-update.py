#!/usr/bin/env python3
"""Update a KD240 A/B card from the files of a kria24 bundle, without a disk image.

    kria-update.py <card> <bundle-dir> [--boot-scr FILE] [--write]

<card> is a block device (or a file in tests) that already has the A/B layout of
kd240/nix/ab.nix: partition 1 a FAT boot partition (a/ and b/ hold each slot's
Image, initrd, dtb and cmdline), partitions 2 and 3 the two raw squashfs slots,
and the two boot-state copies in the gap before partition 1 (bootstate.rs).

What it writes, into the slot that is NOT running:
  * the bundle's rootfs.squashfs, into its raw partition (no mount);
  * Image, initrd, dtb and cmdline, into that slot's directory on the FAT partition
    (mtools, no mount); optionally a new boot.scr at the FAT root;
  * a trial of that slot in the boot state: boot.scr tries it once, and the next
    boot falls back to the committed slot unless the new root confirms.

Without --write it prints the plan and changes nothing. With --write it reads
every written region back and compares it with the bundle's sha256s.

A card that is not in the A/B layout is refused: converting it (its partition
table and the squashfs slots) is a one-time provisioning step, not an update.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import subprocess
import sys
import zlib

SECTOR = 512
COPY_LBA = (4096, 6144)
MAGIC = "emco-ab-1"
FILES = ("Image", "initrd", "dtb", "cmdline")


def die(msg: str) -> None:
    sys.exit(f"kria-update: {msg}")


# ---- boot state (the format of kd240/nix/ab.nix and src/os_ab/bootstate.rs) ----

def enc(kv: dict[str, str]) -> bytes:
    body = "".join(f"{k}={kv[k]}\n" for k in sorted(kv)).encode()
    if len(body) > SECTOR - 4:
        die("boot state too long")
    body = body.ljust(SECTOR - 4, b"\0")
    return zlib.crc32(body).to_bytes(4, "big") + body


def dec(sector: bytes):
    if zlib.crc32(sector[4:]) != int.from_bytes(sector[:4], "big"):
        return None
    text = sector[4:].split(b"\0")[0].decode(errors="replace")
    kv = dict(line.split("=", 1) for line in text.splitlines() if "=" in line)
    if kv.get("ab_magic") != MAGIC:
        return None
    try:
        kv["seq"] = int(kv["ab_seq"], 16)
        kv["tries"] = int(kv["ab_tries"], 16)
    except (KeyError, ValueError):
        return None
    return kv


def read_at(path: str, off: int, n: int) -> bytes:
    with open(path, "rb") as f:
        f.seek(off)
        data = f.read(n)
    if len(data) != n:
        die(f"short read at {off} from {path}")
    return data


def write_at(path: str, off: int, data: bytes) -> None:
    with open(path, "r+b") as f:
        f.seek(off)
        f.write(data)
        f.flush()
        os.fsync(f.fileno())


def boot_copies(card: str):
    return [read_at(card, lba * SECTOR, SECTOR) for lba in COPY_LBA]


def newest(card: str):
    best = None
    for index, sector in enumerate(boot_copies(card)):
        kv = dec(sector)
        if kv and (best is None or kv["seq"] > best[1]["seq"]):
            best = (index, kv)
    return best


# ---- the card's layout ----

def layout(card: str):
    out = subprocess.run(["sfdisk", "-J", card], capture_output=True, text=True)
    if out.returncode != 0:
        die(f"sfdisk cannot read {card}: {out.stderr.strip()}")
    parts = json.loads(out.stdout)["partitiontable"]["partitions"]
    if len(parts) < 4:
        die(f"{card} has {len(parts)} partitions; the A/B layout has four (boot, a, b, data)")
    boot, a, b = parts[0], parts[1], parts[2]
    if boot["start"] < 8 * 1024 * 1024 // SECTOR:
        die("the boot partition starts before 8 MiB: the boot-state copies would not fit")
    if a["size"] != b["size"] or a["type"] != "83" or b["type"] != "83":
        die("partitions 2 and 3 are not two equal raw slots (type 83)")
    return boot, [a, b]


def offsets(boot, slots):
    return {"boot": boot["start"] * SECTOR, "slots": [s["start"] * SECTOR for s in slots],
            "slot_size": slots[0]["size"] * SECTOR}


# ---- the bundle ----

def sha256(path: str) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def load_bundle(d: str):
    try:
        meta = json.load(open(os.path.join(d, "bundle.json")))
    except (OSError, ValueError) as e:
        die(f"no bundle.json in {d}: {e}")
    want = {f["name"]: f for f in meta["files"]}
    for name in ("rootfs.squashfs", *FILES):
        if name not in want:
            die(f"bundle has no {name}")
        path = os.path.join(d, name)
        if not os.path.isfile(path):
            die(f"{path} is missing")
        if os.path.getsize(path) != want[name]["size"]:
            die(f"{name}: size is {os.path.getsize(path)}, the bundle says {want[name]['size']}")
        if sha256(path) != want[name]["sha256"]:
            die(f"{name}: sha256 does not match bundle.json")
    return meta


# ---- mtools on the FAT partition ----

def mtools(card: str, boot_off: int, *args: str, data: bytes | None = None) -> None:
    """Runs one mtools command on the FAT partition at boot_off (no mount)."""
    env = dict(os.environ, MTOOLS_SKIP_CHECK="1")
    r = subprocess.run([args[0], "-i", f"{card}@@{boot_off}", *args[1:]],
                       input=data, capture_output=True, env=env)
    if r.returncode not in (0, 1):  # mmd exits 1 when the directory exists
        die(f"{args[0]}: {r.stderr.decode().strip()}")


def fat_file(card: str, boot_off: int, name: str) -> bytes:
    r = subprocess.run(["mtype", "-i", f"{card}@@{boot_off}", f"::{name}"],
                       capture_output=True)
    if r.returncode != 0:
        die(f"cannot read {name} back: {r.stderr.decode().strip()}")
    return r.stdout


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("card")
    ap.add_argument("bundle")
    ap.add_argument("--boot-scr", help="a new boot.scr to put at the FAT root")
    ap.add_argument("--write", action="store_true", help="write the card (otherwise only the plan)")
    a = ap.parse_args()

    boot, slots = layout(a.card)
    off = offsets(boot, slots)
    state = newest(a.card)
    if state is None:
        die("no valid boot state on the card: it is not an A/B card (or it is damaged)")
    index, kv = state
    if kv["ab_try"] != "-" and kv["tries"] > 0:
        die("a trial is pending: let it finish (or abort it) before updating")
    active = 0 if kv["ab_active"] == "a" else 1
    target = 1 - active
    name = "ab"[target]
    meta = load_bundle(a.bundle)
    squash = os.path.join(a.bundle, "rootfs.squashfs")
    if os.path.getsize(squash) > off["slot_size"]:
        die(f"rootfs.squashfs ({os.path.getsize(squash)}) does not fit slot {name} ({off['slot_size']})")

    print(f"card {a.card}: boot state copy {index}, seq {kv['seq']}, committed slot {'ab'[active]}")
    print(f"update target: slot {name} (partition {2 + target}, offset {off['slots'][target]})")
    print(f"bundle {meta.get('version', '?')}")
    for f in ("rootfs.squashfs", *FILES):
        print(f"  {f:16} {os.path.getsize(os.path.join(a.bundle, f)):>12} bytes")
    if a.boot_scr:
        print(f"  boot.scr        {os.path.getsize(a.boot_scr):>12} bytes (FAT root)")
    if not a.write:
        print("plan only: rerun with --write to update the card")
        return 0

    # 1. the squashfs, into the raw slot partition
    with open(squash, "rb") as f:
        data = f.read()
    write_at(a.card, off["slots"][target], data)
    # 2. the boot files, into the slot's FAT directory
    d = "ab"[target]
    mtools(a.card, off["boot"], "mmd", f"::{d}")
    for name_f in FILES:
        src = os.path.join(a.bundle, name_f)
        with open(src, "rb") as f:
            payload = f.read()
        mtools(a.card, off["boot"], "mcopy", "-o", "-", f"::{d}/{name_f}", data=payload)
    if a.boot_scr:
        with open(a.boot_scr, "rb") as f:
            mtools(a.card, off["boot"], "mcopy", "-o", "-", "::boot.scr", data=f.read())
    # 3. read everything back and check it
    back = read_at(a.card, off["slots"][target], len(data))
    want_squash = next(f["sha256"] for f in meta["files"] if f["name"] == "rootfs.squashfs")
    if hashlib.sha256(back).hexdigest() != want_squash:
        die("the squashfs read back differs from the bundle: the card is not updated")
    for name_f in FILES:
        want = next(f["sha256"] for f in meta["files"] if f["name"] == name_f)
        got = hashlib.sha256(fat_file(a.card, off["boot"], f"{d}/{name_f}")).hexdigest()
        if got != want:
            die(f"{d}/{name_f} read back differs from the bundle")
    # 4. the boot state: the new slot is tried once; the committed one stays as fallback
    seq = kv["seq"] + 1
    trial = {"ab_active": "ab"[active], "ab_magic": MAGIC, "ab_seq": f"{seq:x}",
             "ab_tries": "1", "ab_try": name}
    write_at(a.card, COPY_LBA[1 - index] * SECTOR, enc(trial))
    check = newest(a.card)
    if check is None or check[1]["ab_try"] != name or check[1]["tries"] != 1:
        die("the boot state did not read back as a trial of the new slot")
    print(f"updated: slot {name} is the trial on the next boot; the committed slot is {'ab'[active]}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
