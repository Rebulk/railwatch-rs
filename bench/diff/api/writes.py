"""Compares what the write cases left on each side (run.sh step 5): the
issue activities and comments added, the environment's issues and deploys
after the writes, in order, value for value.

  writes.py RAILS.writes.json RUST.writes.json

Against the old rw-baseline image (built before the 2026-10-16 Rails fixes)
two of these are expected to differ: `activities` (an API write is now the
token user's, so `user` is that email where old Rails left null) and
`deploys` (a re-posted deploy keeps its deployed_at). Those print as
"EXPECTED" and do not fail the run; on a fixed baseline both must match.
"""
import json, sys

rails, rust = (json.load(open(f)) for f in sys.argv[1:3])
EXPECTED_DIFF = {"activities", "deploys"}
ok = True
for k in ("activities", "comments", "issues", "deploys"):
    a, b = rails[k], rust[k]
    if a == b:
        print(f"{k}: {len(a)}/{len(a)} identical")
        continue
    if k in EXPECTED_DIFF:
        print(f"{k}: EXPECTED vs old baseline ({len(a)} rails, {len(b)} rust)")
    else:
        ok = False
        print(f"{k}: DIFFERENT ({len(a)} rails, {len(b)} rust)")
    for i in range(max(len(a), len(b))):
        x = a[i] if i < len(a) else None
        y = b[i] if i < len(b) else None
        if x != y:
            print(f"    [{i}] rails={json.dumps(x)[:300]}\n         rust ={json.dumps(y)[:300]}")
sys.exit(0 if ok else 1)
