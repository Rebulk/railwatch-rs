#!/usr/bin/env python3
"""Per-second writer transaction cost and rollup flushes from a server log
run with RAILWATCH_LOG=railwatch_server=debug.  txn_timeline.py server.log"""
import sys, re
T, F, L = [], [], []
for l in open(sys.argv[1]):
    l = re.sub(r"\x1b\[[0-9;]*m", "", l)
    m = re.match(r"\S+T(\d+):(\d+):([\d.]+)Z", l); ms = re.search(r" ms=(\d+)", l)
    if not (m and ms): continue
    t = int(m[1]) * 3600 + int(m[2]) * 60 + float(m[3])
    if "transaction committed" in l:
        T.append((t, int(ms[1])))
        lk = re.search(r"lock_ms=(\d+)", l)
        if lk: L.append(int(lk[1]))
    elif "rollup flush" in l: F.append((t, int(ms[1])))
t0 = T[0][0]; v = sorted(x for _, x in T)
q = lambda p: v[min(len(v) - 1, int(p * len(v)))]
print(f"txns {len(v)}: p50 {q(.5)} p90 {q(.9)} p99 {q(.99)} max {v[-1]} ms")
if L:
    L.sort(); print(f"lock wait: p50 {L[len(L)//2]} p99 {L[int(.99*len(L))]} max {L[-1]} ms, >=10 ms: {sum(x >= 10 for x in L)}")
print("flushes (s, ms):", [(round(t - t0, 1), ms) for t, ms in F])
for s in range(0, int(T[-1][0] - t0) + 1, 5):
    w = [x for t, x in T if s <= t - t0 < s + 5]
    if w: print(f"{s:3d}s avg {sum(w)/len(w):5.1f} max {max(w):3d}")
