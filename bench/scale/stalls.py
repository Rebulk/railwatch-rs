"""Do a step's slow ingest windows line up with slow fsyncs on the bench disk?

    python3 -I bench/scale/stalls.py bench/results/scale/raw/<run>

reads <run>.load.json (rw-load's 10 s timeline), <run>.fsync (one
"<unix time> <ms>" per second from fsync_probe.py) and <run>.t0 (the load's
start, unix time). Prints, per 10 s window, the ingest p99 and max and the
worst fdatasync in that window, then a verdict: of the windows whose ingest
max exceeded SLOW_MS (1000), how many also had an fsync over FSYNC_MS (500)?
"""
import json
import sys

SLOW_MS, FSYNC_MS = 1000.0, 500.0
base = sys.argv[1]
load = json.load(open(base + ".load.json"))
t0 = float(open(base + ".t0").read().strip())
probe = [tuple(map(float, l.split())) for l in open(base + ".fsync") if len(l.split()) == 2]

rows, slow, both = [], 0, 0
for w in load["timeline_10s"]:
    lo, hi = t0 + w["t"], t0 + w["t"] + 10
    f = [ms for at, ms in probe if lo <= at < hi]
    worst = max(f, default=0.0)
    is_slow = w["max_ms"] > SLOW_MS
    slow += is_slow
    both += is_slow and worst > FSYNC_MS
    rows.append((w["t"], round(w["p99_ms"]), round(w["max_ms"]), round(worst), "SLOW" if is_slow else ""))

allf = sorted(ms for _, ms in probe)
q = lambda p: allf[min(len(allf) - 1, int(p * len(allf)))] if allf else None
print(f"{'t':>5} {'p99':>7} {'max':>7} {'fsync':>7}")
for r in rows:
    if r[4] or r[3] > FSYNC_MS:
        print(f"{r[0]:>5} {r[1]:>7} {r[2]:>7} {r[3]:>7} {r[4]}")
print(f"fsync probe: n={len(allf)} p50={q(0.5)} p99={q(0.99)} max={allf[-1] if allf else None} ms, "
      f"{sum(1 for x in allf if x > FSYNC_MS)} over {FSYNC_MS:.0f} ms")
print(f"slow windows (ingest max > {SLOW_MS:.0f} ms): {slow}; with an fsync > {FSYNC_MS:.0f} ms in the same window: {both}")
print(json.dumps({"slow_windows": slow, "slow_with_slow_fsync": both,
                  "lines_up": bool(slow) and both / slow >= 0.8}))
