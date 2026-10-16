"""Median ms per API case, Rails vs Rust, grouped by tool.

  timings.py RAILS.timings.json RUST.timings.json
"""
import json, statistics, sys

rails, rust = (json.load(open(f)) for f in sys.argv[1:3])
print("| case | Rails ms | Rust ms | speedup |")
print("|---|---:|---:|---:|")
tr = tu = 0.0
for name, r in rails.items():
    u = rust.get(name)
    if not u:
        continue
    a, b = statistics.median(r), statistics.median(u)
    tr += a
    tu += b
    print(f"| {name} | {a:.1f} | {b:.2f} | {a / b if b else float('inf'):.0f}x |")
print(f"| **sum** | **{tr:.0f}** | **{tu:.0f}** | **{tr / tu:.0f}x** |")
