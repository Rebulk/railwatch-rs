#!/usr/bin/env python3
"""Compares two telemetry tenant files table by table, row by row, ids
included. Prints a per-table summary; exits 1 on any difference.

    compare.py RAILS.sqlite3 RUST.sqlite3

Volatile columns (time of receipt on each server) are listed in VOLATILE and
compared only for presence. Tables the Rails background jobs write after
ingest (rollups, release_health) are Phase 2 and listed in NOT_YET; the
receipts' followups column is drained by Rails' DrainIngestFollowupsJob,
which Phase 1 does not run, so it is compared as written at commit time
only when Rails has not drained it yet (see docs/DIFFERENCES.md).
"""
import sqlite3, sys

VOLATILE = {
    "ingest_batches": {"received_at"},
    "ingest_receipts": {"committed_at", "retain_until"},
    "ar_internal_metadata": {"created_at", "updated_at"},
}
NOT_YET = {"rollups", "release_health"}
SKIP = {"sqlite_stat1", "sqlite_stat4"}

def open_ro(path):
    c = sqlite3.connect(f"file:{path}?mode=ro", uri=True)
    c.text_factory = bytes
    return c

def tables(c):
    return [r[0].decode() for r in c.execute("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")]

def columns(c, t):
    return [r[1].decode() for r in c.execute(f'PRAGMA table_info("{t}")')]

def rows(c, t, cols):
    order = "rowid" if t not in ("logs_fts_idx", "logs_fts_config", "schema_migrations", "ar_internal_metadata", "query_shapes") else ", ".join(f'"{x}"' for x in cols)
    sel = ", ".join(f'"{x}", typeof("{x}")' for x in cols)
    return c.execute(f'SELECT {sel} FROM "{t}" ORDER BY {order}').fetchall()

def show(v):
    if isinstance(v, bytes):
        s = v.decode("utf-8", "replace")
        return repr(s if len(s) < 120 else s[:117] + "...")
    return repr(v)

rails, rust = open_ro(sys.argv[1]), open_ro(sys.argv[2])
rt, ut = tables(rails), tables(rust)
bad = 0
if rt != ut:
    print(f"TABLE SETS DIFFER: only rails={sorted(set(rt)-set(ut))} only rust={sorted(set(ut)-set(rt))}")
    bad += 1
schema_r = rails.execute("SELECT type, name, tbl_name, sql FROM sqlite_master ORDER BY rowid").fetchall()
schema_u = rust.execute("SELECT type, name, tbl_name, sql FROM sqlite_master ORDER BY rowid").fetchall()
# A new Rust tenant makes these two queries indexes partial (schema.rs
# PARTIAL_INDEXES, docs/DIFFERENCES.md); every other row must be Rails' own.
PARTIAL = {b"idx_queries_tenant_duration_cursor", b"idx_queries_duration_cursor", "idx_queries_tenant_duration_cursor", "idx_queries_duration_cursor"}
CLAUSE = " WHERE app_tenant IS NOT NULL"
def expected_rust(row):
    ty, name, tbl, sql = row
    if name in PARTIAL and sql is not None:
        sql = sql + (CLAUSE.encode() if isinstance(sql, bytes) else CLAUSE)
    return (ty, name, tbl, sql)
if schema_r == schema_u:
    print(f"{'sqlite_master':<26} identical ({len(schema_r)} entries)")
elif [expected_rust(r) for r in schema_r] == schema_u:
    print(f"{'sqlite_master':<26} identical but for the 2 partial queries indexes ({len(schema_r)} entries)")
else:
    print(f"{'sqlite_master':<26} DIFFERENT ({len(schema_r)} entries)")
    bad += 1

print(f"{'table':<26} {'rails':>7} {'rust':>7}  result")
for t in rt:
    if t in SKIP or t not in ut:
        continue
    cols = columns(rails, t)
    if cols != columns(rust, t):
        print(f"{t:<26} column lists differ"); bad += 1; continue
    a, b = rows(rails, t, cols), rows(rust, t, cols)
    if t == "sqlite_sequence":
        # The phase-2 tables' own sequence rows come from Rails' jobs too.
        a = [r for r in a if r[0] not in tuple(x.encode() for x in NOT_YET)]
        b = [r for r in b if r[0] not in tuple(x.encode() for x in NOT_YET)]
        a.sort(key=lambda r: r[0]); b.sort(key=lambda r: r[0])
    if t in NOT_YET:
        print(f"{t:<26} {len(a):>7} {len(b):>7}  not compared (written by Rails jobs; phase 2)")
        continue
    vol = set(VOLATILE.get(t, set()))
    if t == "ingest_receipts" and any(r[cols.index("followups") * 2] is None for r in a):
        # Rails' DrainIngestFollowupsJob (every minute) has drained some;
        # Phase 1 has no drain, so the column can only be compared before it runs.
        vol |= {"followups", "followups_completed_at"}
        print("  (ingest_receipts: Rails drained follow-ups before the copy; followups columns not compared)")
    idx = [i for i, c in enumerate(cols) if c not in vol]
    def key(r):
        return tuple(r[2 * i] for i in idx) + tuple(r[2 * i + 1] for i in idx)
    def present(r):
            return tuple(r[2 * i] is None for i, c in enumerate(cols) if c in vol and c not in ("followups", "followups_completed_at"))
    diffs = []
    for n, (ra, rb) in enumerate(zip(a, b)):
        if key(ra) != key(rb) or present(ra) != present(rb):
            for i, c in enumerate(cols):
                if c in vol:
                    continue
                if (ra[2 * i], ra[2 * i + 1]) != (rb[2 * i], rb[2 * i + 1]):
                    diffs.append(f"    row {n + 1} {c}: rails={show(ra[2*i])}:{ra[2*i+1].decode()} rust={show(rb[2*i])}:{rb[2*i+1].decode()}")
            if len(diffs) > 12:
                break
    ok = not diffs and len(a) == len(b)
    if t == "sqlite_sequence" and ok:
        print(f"{t:<26} {len(a):>7} {len(b):>7}  identical (ignoring the rollups/release_health rows)")
        continue
    note = f"identical (ignoring {', '.join(sorted(vol))})" if vol and ok else ("identical" if ok else "DIFFERENT")
    print(f"{t:<26} {len(a):>7} {len(b):>7}  {note}")
    if not ok:
        bad += 1
        print("\n".join(diffs[:12]))
sys.exit(1 if bad else 0)
