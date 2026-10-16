#!/usr/bin/env python3
"""Markdown table of median page times, Rails vs Rust.

    timings.py RAILS.timings.json RUST.timings.json

Rails' time is the whole integration request (routing, auth, controller,
Inertia JSON render) in one Puma-less process; Rust's is the page function
alone. The JSON render Rust skips is measured separately (see
docs/DIFFERENCES.md); it is small next to the queries.
"""
import json, statistics, sys

rails = json.load(open(sys.argv[1]))
rust = json.load(open(sys.argv[2]))
print("| page | Rails ms | Rust ms | speedup |")
print("|---|---:|---:|---:|")
tot_r = tot_s = 0.0
for name in rails:
    if name not in rust or not rails[name] or not rust[name]:
        continue
    r, s = statistics.median(rails[name]), statistics.median(rust[name])
    tot_r += r
    tot_s += s
    print(f"| {name} | {r:.1f} | {s:.1f} | {r / s if s else float('inf'):.0f}x |")
print(f"| **sum** | **{tot_r:.0f}** | **{tot_s:.0f}** | **{tot_r / tot_s:.0f}x** |")
