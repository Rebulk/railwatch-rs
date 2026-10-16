"""Per-day partitions: retention = DETACH + unlink; reads = UNION ALL over ATTACHed days.

  python3 -I drop_and_query.py PARTDIR
"""
import sqlite3, os, sys, time, glob
d = sys.argv[1]
files = sorted(glob.glob(f"{d}/day*.sqlite3"))
c = sqlite3.connect(":memory:", isolation_level=None)
for i, f in enumerate(files): c.execute("ATTACH ? AS ?", (f, f"d{i+1}"))
names = [f"d{i+1}" for i in range(len(files))]
def union(cols, where, names):
    return " UNION ALL ".join(f"SELECT {cols} FROM {n}.queries INDEXED BY idx_queries_slowest WHERE {where}" for n in names)
def bench(label, sql, n=5):
    ts = []
    for _ in range(n):
        t = time.monotonic(); c.execute(sql).fetchall(); ts.append(1000 * (time.monotonic() - t))
    print(f"  {label:58s} " + " ".join(f"{x:7.1f}" for x in ts) + " ms")
w = "occurred_at > '2026-10-13'"
# slowest page across partitions: each partition's top 51 by its index, then merge
per = " UNION ALL ".join(f"SELECT * FROM (SELECT id, duration FROM {n}.queries INDEXED BY idx_queries_slowest WHERE {w} ORDER BY duration DESC, id DESC LIMIT 51)" for n in names)
bench("slowest 51 across 6 partitions (top-k per day, merge)", f"SELECT * FROM ({per}) ORDER BY duration DESC, id DESC LIMIT 51")
grp = " UNION ALL ".join(f"SELECT group_hash FROM {n}.queries WHERE occurred_at BETWEEN '2026-10-13 22:24' AND '2026-10-13 22:26'" for n in names)
bench("group counts, 2-minute window, 6 partitions", f"SELECT group_hash, COUNT(*) FROM ({grp}) GROUP BY 1")
one = files[0]
size = os.path.getsize(one); rows = sqlite3.connect(one).execute("SELECT (SELECT COUNT(*) FROM queries)+(SELECT COUNT(*) FROM executions)+(SELECT COUNT(*) FROM logs)+(SELECT COUNT(*) FROM cache_events)+(SELECT COUNT(*) FROM view_renders)").fetchone()[0]
t = time.monotonic(); c.execute("DETACH d1"); os.unlink(one)
for ext in ("-wal", "-shm"):
    if os.path.exists(one + ext): os.unlink(one + ext)
print(f"DROP partition {os.path.basename(one)}: {rows} rows, {size} B returned to the filesystem in {1000*(time.monotonic()-t):.1f} ms")
