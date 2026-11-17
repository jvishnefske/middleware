#!/usr/bin/env python3
"""Worst-case stack depth of an entry symbol: per-function frames from
LLVM's .stack_sizes (-Z emit-stack-sizes), call edges from the
disassembly (bl/blx to symbols; b/b.w tail calls reuse the frame)."""
import re, subprocess, sys
bindir, archive, *entries = sys.argv[1:]
objs = subprocess.run([f"{bindir}/llvm-ar", "t", archive], capture_output=True, text=True).stdout.split()
import tempfile, os
d = tempfile.mkdtemp()
subprocess.run([f"{bindir}/llvm-ar", "x", archive], cwd=d, check=True)
frames, calls, tails, indirect = {}, {}, {}, set()
for o in objs:
    p = os.path.join(d, o)
    rs = subprocess.run([f"{bindir}/llvm-readobj", "--stack-sizes", p], capture_output=True, text=True).stdout
    for fn, sz in re.findall(r"Functions: \[([^\]]+)\]\s+Size: (0x[0-9A-Fa-f]+|\d+)", rs):
        for f in fn.split(","):
            frames[f.strip()] = int(sz, 0)
    dis = subprocess.run([f"{bindir}/llvm-objdump", "-dr", "--no-show-raw-insn", p], capture_output=True, text=True).stdout
    cur = None
    for line in dis.splitlines():
        m = re.match(r"^[0-9a-f]+ <(.+)>:$", line)
        if m:
            cur = m.group(1); calls.setdefault(cur, set()); tails.setdefault(cur, set()); continue
        if cur is None: continue
        m = re.search(r"R_(?:ARM_THM|RISCV)_(CALL_PLT|CALL|JUMP24|JUMP11|JUMP8)\s+(\S+)", line)
        if m:
            tgt = m.group(2)
            (calls if m.group(1).startswith("CALL") else tails)[cur].add(tgt)
        elif re.search(r"\tbl\s+0x[0-9a-f]+ <([^>+]+)>", line):
            tgt = re.search(r"<([^>+]+)", line.split("bl",1)[1]).group(1)
            if tgt != cur: calls[cur].add(tgt)
        elif re.search(r"\tblx\s+r\d", line):
            indirect.add(cur)
memo = {}
def depth(f, stack=()):
    if f in stack: return 0, [f + " (recursion!)"]
    if f in memo: return memo[f]
    own = frames.get(f)
    best, path = 0, []
    for c in calls.get(f, ()):
        dd, pp = depth(c, stack + (f,))
        if dd > best: best, path = dd, pp
    tail_best, tail_path = 0, []
    for c in tails.get(f, ()):
        if c == f: continue
        dd, pp = depth(c, stack + (f,))
        if dd > tail_best: tail_best, tail_path = dd, pp
    total = max((own or 0) + best, tail_best)
    res = (total, [f"{f} [{own if own is not None else '?'}]"] + (path if (own or 0) + best >= tail_best else tail_path))
    memo[f] = res
    return res
for e in entries:
    tot, path = depth(e)
    print(f"{e}: {tot} bytes worst case")
    for p in path[:40]: print("   ", p)
unknown = [f for f in calls if f not in frames and calls[f]]
print("indirect calls in:", sorted(indirect)[:10])
