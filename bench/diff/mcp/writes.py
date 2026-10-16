"""Compares the issue_activities and comments the write cases left on each
side (run.sh step 5), in order, value for value.

  writes.py RAILS.writes.json RUST.writes.json
"""
import json, sys

rails, rust = (json.load(open(f)) for f in sys.argv[1:3])
ok = True
for k in ("activities", "comments"):
    a, b = rails[k], rust[k]
    if a == b:
        print(f"{k}: {len(a)}/{len(a)} identical")
        continue
    ok = False
    print(f"{k}: DIFFERENT ({len(a)} rails, {len(b)} rust)")
    for i in range(max(len(a), len(b))):
        x = a[i] if i < len(a) else None
        y = b[i] if i < len(b) else None
        if x != y:
            print(f"    [{i}] rails={json.dumps(x)[:300]}\n         rust ={json.dumps(y)[:300]}")
sys.exit(0 if ok else 1)
