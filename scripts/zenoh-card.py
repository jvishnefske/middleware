#!/usr/bin/env python3
"""The A/B card of the Linux root, as files, for the Zenoh end-to-end test.

  zenoh-card.py init <card> <cmdline> <slot-a> <slot-b>   a fresh card (slot a committed)
  zenoh-card.py boot <card> <cmdline>                    one power cycle, as boot.scr does

The boot-state copies are kd240/nix/ab.nix's format (lib src/os_ab/bootstate.rs):
a CRC-32 (zlib's, big-endian) over the NUL-padded `env export -t` text, at LBA
4096 and 6144 of 512-byte sectors. `boot` is boot.scr's decision: the newest
valid copy; with a try and tries left, count one down, write it, boot the
try; otherwise boot the committed slot. The root's own unit tests cover the
Rust side of the same format (src/os_ab/bootstate.rs).
"""
import sys
import zlib

SECTOR = 512
LBA = (4096, 6144)


def enc(kv):
    body = "".join(f"{k}={kv[k]}\n" for k in sorted(kv)).encode()
    if len(body) > SECTOR - 4:
        raise SystemExit("boot state too long")
    body = body.ljust(SECTOR - 4, b"\0")
    return zlib.crc32(body).to_bytes(4, "big") + body


def dec(sector):
    if zlib.crc32(sector[4:]) != int.from_bytes(sector[:4], "big"):
        return None
    text = sector[4:].split(b"\0")[0].decode()
    kv = dict(line.split("=", 1) for line in text.splitlines() if line)
    if kv.get("ab_magic") != "mw-ab-1":
        return None
    kv["seq"] = int(kv["ab_seq"], 16)
    return kv


def copies(card):
    with open(card, "rb") as f:
        data = f.read()
    return [data[l * SECTOR:(l + 1) * SECTOR] for l in LBA]


def write_copy(card, index, sector):
    with open(card, "r+b") as f:
        f.seek(LBA[index] * SECTOR)
        f.write(sector)


def newest(card):
    best = None
    for i, sec in enumerate(copies(card)):
        kv = dec(sec)
        if kv and (best is None or kv["seq"] > best[1]["seq"]):
            best = (i, kv)
    return best


def state_text(active, try_slot, tries, seq):
    return {"ab_active": active, "ab_magic": "mw-ab-1", "ab_seq": f"{seq:x}",
            "ab_tries": f"{tries:x}", "ab_try": try_slot}


def init(card, cmdline, slot_a, slot_b):
    with open(card, "wb") as f:
        f.truncate(LBA[1] * SECTOR + SECTOR)
    write_copy(card, 0, enc(state_text("a", "-", 0, 1)))
    with open(cmdline, "w") as f:
        f.write("init=/nix/store/init quiet mw.slot=a\n")
    for p in (slot_a, slot_b):
        with open(p, "wb") as f:
            f.truncate(1 << 20)


def boot(card, cmdline):
    best = newest(card)
    if best is None:
        raise SystemExit("no valid boot state")
    i, kv = best
    slot = kv["ab_active"]
    if kv["ab_try"] != "-" and int(kv["ab_tries"], 16) > 0:
        tries = int(kv["ab_tries"], 16) - 1
        slot = kv["ab_try"]
        nxt = dict(kv, ab_tries=f"{tries:x}", ab_seq=f"{kv['seq'] + 1:x}")
        nxt.pop("seq", None)
        write_copy(card, 1 - i, enc({k: v for k, v in nxt.items() if k.startswith("ab_")}))
    with open(cmdline, "w") as f:
        f.write(f"init=/nix/store/init quiet mw.slot={slot}\n")
    print(f"booted slot {slot}")


if __name__ == "__main__":
    a = sys.argv[1:]
    if a[:1] == ["init"] and len(a) == 5:
        init(*a[1:])
    elif a[:1] == ["boot"] and len(a) == 3:
        boot(*a[1:])
    else:
        sys.exit(__doc__)
