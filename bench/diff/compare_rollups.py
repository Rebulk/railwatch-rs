#!/usr/bin/env python3
"""Compares rollups, release_health and their sqlite_sequence rows in two
tenant files, row by row, every column (digest bytes included).

    compare_rollups.py RAILS.sqlite3 RUST.sqlite3
"""
import sqlite3, sys

def rows(path, sql):
    c = sqlite3.connect(f"file:{path}?mode=ro", uri=True)
    c.text_factory = bytes
    return c.execute(sql).fetchall()

bad = 0
for table, order in [("rollups", "id"), ("release_health", "id")]:
    cols = [r[1].decode() for r in rows(sys.argv[1], f'PRAGMA table_info("{table}")')]
    sel = ", ".join(f'"{c}", typeof("{c}")' for c in cols)
    a = rows(sys.argv[1], f'SELECT {sel} FROM "{table}" ORDER BY {order}')
    b = rows(sys.argv[2], f'SELECT {sel} FROM "{table}" ORDER BY {order}')
    diffs = [(x, y) for x, y in zip(a, b) if x != y]
    ok = len(a) == len(b) and not diffs
    print(f"{table:<16} rails={len(a):>6} rust={len(b):>6}  {'IDENTICAL' if ok else 'DIFFERENT'}")
    for x, y in diffs[:5]:
        for i, c in enumerate(cols):
            if x[2 * i:2 * i + 2] != y[2 * i:2 * i + 2]:
                print(f"  id {x[0]} {c}: rails={x[2*i]!r:.120} rust={y[2*i]!r:.120}")
    bad += not ok
    if table == "rollups":
        dig = sum(1 for x, y in zip(a, b) if x[cols.index("digest") * 2] == y[cols.index("digest") * 2])
        print(f"  digests byte-identical: {dig}/{len(a)}")
seq_sql = "SELECT name, seq FROM sqlite_sequence WHERE name IN ('rollups','release_health') ORDER BY name"
sa, sb = rows(sys.argv[1], seq_sql), rows(sys.argv[2], seq_sql)
print(f"sqlite_sequence  {'IDENTICAL' if sa == sb else 'DIFFERENT'} {sa} {sb if sa != sb else ''}")
bad += sa != sb
sys.exit(1 if bad else 0)
