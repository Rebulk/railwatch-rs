#!/usr/bin/env python3
"""Bytes on disk per table, per index and per row, for one or more tenant
files (a day file, a meta file, a single-file tenant), from SQLite's dbstat.

    bench/storage/bytes.py FILE.sqlite3 [FILE...] [--json OUT.json]

Prints, per file: every table with its own B-tree bytes, the bytes of its
indexes (each named), its rows, and bytes/row with and without indexes;
then, for the big text columns, the share of the table's payload they take
(SUM(length(col)) over the table's payload bytes). FTS5 shadow tables are
summed under `logs_fts`. Opens every file read-only (immutable=1: run it on
a copy or on a closed day file, never on a live production file).
"""
import json, sqlite3, sys

TEXT = {
    "queries": ["sql", "source", "name", "explain", "execution_preview", "execution_id", "trace_id", "group_hash", "deploy", "server", "app_tenant", "user_ref"],
    "logs": ["message", "context", "tags", "source", "execution_preview", "execution_id", "trace_id", "deploy", "server"],
    "cache_events": ["key", "store", "execution_preview", "execution_id", "trace_id", "group_hash", "deploy", "server"],
    "view_renders": ["identifier", "layout", "execution_preview", "execution_id", "trace_id", "group_hash", "deploy", "server"],
    "spans": ["name", "payload", "execution_preview", "execution_id", "trace_id", "group_hash"],
    "executions": ["detail", "counters", "stages", "name", "route", "controller", "action", "execution_preview", "exception_preview",
                   "execution_id", "trace_id", "group_hash", "deploy", "server", "app_tenant", "user_ref", "inertia_component"],
    "outgoing_requests": ["url", "host", "execution_id", "trace_id", "group_hash"],
    "exceptions": ["backtrace", "message", "class_name", "context"],
}


def analyze(path):
    db = sqlite3.connect(f"file:{path}?mode=ro&immutable=1", uri=True)
    page = db.execute("PRAGMA page_size").fetchone()[0]
    pages = db.execute("PRAGMA page_count").fetchone()[0]
    free = db.execute("PRAGMA freelist_count").fetchone()[0]
    owner = {}
    for ty, name, tbl in db.execute("SELECT type, name, tbl_name FROM sqlite_master WHERE type IN ('table', 'index')"):
        if name.startswith("logs_fts"):
            tbl = "logs_fts"
        owner[name] = (ty, tbl)
    sizes = dict(db.execute("SELECT name, SUM(pgsize) FROM dbstat GROUP BY name"))
    payload = dict(db.execute("SELECT name, SUM(payload) FROM dbstat GROUP BY name"))
    tables = {}
    for name, b in sizes.items():
        ty, tbl = owner.get(name, ("table", name))
        if name.startswith("sqlite_autoindex"):
            ty = "index"
        t = tables.setdefault(tbl, {"table": 0, "indexes": {}, "payload": 0})
        if ty == "index":
            t["indexes"][name] = b
        else:
            t["table"] += b
            t["payload"] += payload.get(name) or 0
    for tbl, t in tables.items():
        try:
            t["rows"] = db.execute(f'SELECT COUNT(*) FROM "{tbl}"').fetchone()[0] if tbl != "logs_fts" else None
        except sqlite3.Error:
            t["rows"] = None
        t["text"] = {}
        if t.get("rows") and tbl in TEXT:
            cols = {r[1] for r in db.execute(f'PRAGMA table_info("{tbl}")')}
            for c in TEXT[tbl]:
                if c in cols:
                    t["text"][c] = db.execute(f'SELECT COALESCE(SUM(length(CAST("{c}" AS BLOB))), 0) FROM "{tbl}"').fetchone()[0]
            distinct = {}
            for c in ("sql", "message", "key", "identifier", "name", "route", "execution_preview", "source"):
                if c in cols:
                    distinct[c] = db.execute(f'SELECT COUNT(DISTINCT "{c}") FROM "{tbl}"').fetchone()[0]
            t["distinct"] = distinct
    return {"file": path, "page_size": page, "bytes": page * pages, "free_pages": free, "tables": tables}


def report(r):
    print(f"\n== {r['file']}: {r['bytes'] / 1e6:.1f} MB, page {r['page_size']}, free pages {r['free_pages']}")
    print(f"{'table':22s} {'rows':>10s} {'table MB':>9s} {'idx MB':>8s} {'idx%':>5s} {'B/row':>7s} {'B/row tbl':>9s}")
    rows = sorted(r["tables"].items(), key=lambda kv: -(kv[1]["table"] + sum(kv[1]["indexes"].values())))
    for tbl, t in rows:
        idx = sum(t["indexes"].values())
        tot = t["table"] + idx
        if tot < 64 * 1024:
            continue
        n = t.get("rows") or 0
        print(f"{tbl:22s} {n:10d} {t['table'] / 1e6:9.1f} {idx / 1e6:8.1f} {100 * idx / max(tot, 1):5.0f} "
              f"{(tot / n if n else 0):7.0f} {(t['table'] / n if n else 0):9.0f}")
        for name, b in sorted(t["indexes"].items(), key=lambda kv: -kv[1]):
            if b >= 64 * 1024:
                print(f"    {name:52s} {b / 1e6:7.1f} MB {(b / n if n else 0):5.0f} B/row")
        if t.get("text"):
            p = max(t["payload"], 1)
            parts = ", ".join(f"{c} {100 * v / p:.0f}%" for c, v in sorted(t["text"].items(), key=lambda kv: -kv[1]) if v * 50 >= p)
            print(f"    text share of row payload: {parts}")
            print(f"    distinct: {t.get('distinct')}")


if __name__ == "__main__":
    args = sys.argv[1:]
    out = None
    if "--json" in args:
        i = args.index("--json")
        out = args[i + 1]
        del args[i:i + 2]
    results = [analyze(p) for p in args]
    for r in results:
        report(r)
    if out:
        json.dump(results, open(out, "w"), indent=1)
