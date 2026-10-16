"""Rails PruneTelemetryJob, statement for statement, against one tenant file.

  python3 -I prune_day.py FILE CUTOFF_EPOCH

Mirrors app/jobs/prune_telemetry_job.rb @ 464f0fd: unindex logs from FTS, then
for each RAW table `SELECT id ... WHERE occurred_at < ? ORDER BY occurred_at
LIMIT 5000` + `DELETE ... WHERE id IN (...)` until empty (each statement its
own autocommit transaction, as Active Record runs them), the query_shapes
NOT EXISTS sweep, the small tables, then wal_checkpoint(TRUNCATE).
Connection pragmas are database.yml's. Prints per-table seconds and rows.
"""
import sqlite3, sys, time, datetime, os

path, cutoff_epoch = sys.argv[1], float(sys.argv[2])
cutoff = datetime.datetime.fromtimestamp(cutoff_epoch, datetime.timezone.utc).strftime("%Y-%m-%d %H:%M:%S.%f")
c = sqlite3.connect(path, isolation_level=None, timeout=5)
for p in ["journal_mode = wal", "synchronous = normal", "mmap_size = 134217728", "journal_size_limit = 67108864", "cache_size = 2000"]:
    c.execute(f"PRAGMA {p}")
RAW = ["executions", "queries", "exceptions", "cache_events", "mails", "broadcasts", "notifications",
       "outgoing_requests", "storage_ops", "view_renders", "logs", "enqueued_jobs", "transactions",
       "n_plus_ones", "deprecations", "visits", "spans", "llm_calls", "profiles", "attachments"]
t_all = time.monotonic()
stats = []
def timed(label, fn):
    t = time.monotonic(); n = fn(); stats.append((label, time.monotonic() - t, n))

def unindex_logs():
    # PruneTelemetryJob#unindex_logs: one statement over the whole expired range
    return c.execute("INSERT INTO logs_fts(logs_fts, rowid, message) SELECT 'delete', id, message FROM logs WHERE occurred_at < ?", (cutoff,)).rowcount

def prune(table):
    def run():
        n = 0
        while True:
            ids = [r[0] for r in c.execute(f'SELECT "{table}"."id" FROM "{table}" WHERE "{table}"."occurred_at" < ? ORDER BY "{table}"."occurred_at" ASC LIMIT 5000', (cutoff,))]
            if not ids: return n
            c.execute(f'DELETE FROM "{table}" WHERE "{table}"."id" IN ({",".join(map(str, ids))})')
            n += len(ids)
            if len(ids) < 5000: return n
    return run

has_fts = c.execute("SELECT 1 FROM sqlite_master WHERE name='logs_fts'").fetchone()
if has_fts: timed("logs_fts unindex", unindex_logs)
for t in RAW: timed(t, prune(t))
timed("query_shapes sweep", lambda: c.execute("DELETE FROM query_shapes WHERE NOT EXISTS (SELECT 1 FROM queries WHERE queries.group_hash = query_shapes.group_hash)").rowcount)
timed("ingest_batches", lambda: c.execute("DELETE FROM ingest_batches WHERE received_at < ?", (cutoff,)).rowcount)
timed("health_samples", lambda: c.execute("DELETE FROM health_samples WHERE sampled_at < ?", (cutoff,)).rowcount)
timed("wal_checkpoint(TRUNCATE)", lambda: c.execute("PRAGMA wal_checkpoint(TRUNCATE)").fetchone()[0])
total = time.monotonic() - t_all
for label, s, n in stats:
    if n: print(f"  {label:28s} {s:8.2f} s  {n:>10} rows")
pc, fl, ps = (c.execute(f"PRAGMA {p}").fetchone()[0] for p in ("page_count", "freelist_count", "page_size"))
print(f"PRUNE total {total:.2f} s; cutoff {cutoff}; file {os.path.getsize(path)} B; freelist {fl*ps} B ({100*fl/pc:.1f}% of file)")
