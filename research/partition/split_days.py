"""Split a tenant file into per-day partition files with the IDENTICAL gem schema.

  python3 -I split_days.py MASTER OUTDIR DAYSTART_FILES...

Each partition is a full tenant file (same DDL, every index) holding only the rows
whose occurred_at falls in [day_k, day_k+1). Small/global tables (query_shapes,
rollups, receipts, people) stay in the master-of-record and are not counted here.
"""
import sqlite3, sys, os, datetime, time
master, out, starts = sys.argv[1], sys.argv[2], sys.argv[3:]
def ts(f): return datetime.datetime.fromtimestamp(float(open(f).read()), datetime.timezone.utc).strftime("%Y-%m-%d %H:%M:%S.%f")
bounds = [ts(f) for f in starts] + ["9999-12-31"]
bounds[0] = "0000-01-01"
os.makedirs(out, exist_ok=True)
src = sqlite3.connect(master)
ddl = [r[0] for r in src.execute("SELECT sql FROM sqlite_master WHERE sql IS NOT NULL AND name NOT LIKE 'sqlite_%' AND name NOT LIKE 'logs_fts_%' ORDER BY rowid")]
TABLES = [r[0] for r in src.execute("SELECT DISTINCT m.name FROM sqlite_master m, pragma_table_info(m.name) p WHERE m.type='table' AND p.name='occurred_at' AND m.name NOT LIKE 'logs_fts%'")]
src.close()
for k in range(len(bounds) - 1):
    p = os.path.join(out, f"day{k+1}.sqlite3")
    if os.path.exists(p): os.remove(p)
    t = time.monotonic()
    d = sqlite3.connect(p, isolation_level=None)
    d.execute("PRAGMA journal_mode=wal"); d.execute("PRAGMA synchronous=normal")
    d.execute("BEGIN")
    for s in ddl: d.execute(s)
    d.execute("ATTACH ? AS m", (master,))
    n = 0
    for tb in TABLES:
        n += d.execute(f'INSERT INTO main."{tb}" SELECT * FROM m."{tb}" WHERE occurred_at >= ? AND occurred_at < ?', (bounds[k], bounds[k+1])).rowcount
    d.execute("INSERT INTO main.logs_fts(rowid, message) SELECT id, message FROM main.logs")
    d.execute("COMMIT"); d.execute("DETACH m"); d.execute("PRAGMA wal_checkpoint(TRUNCATE)"); d.close()
    print(f"day{k+1}: {n} rows, {os.path.getsize(p)} B, built in {time.monotonic()-t:.1f}s")
