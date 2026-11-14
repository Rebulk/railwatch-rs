#!/usr/bin/env bash
# Which day-file indexes the dashboard, MCP and API reads use.
#
#   bench/storage/index_audit.sh [work_dir]
#
# 1. gen_tenant.py: a prod-shaped tenant over 8 days (sessions, security
#    fields, tenants, 2,000 query shapes), posted to a fresh per-day server.
# 2. Control rows and tokens (audit_setup.py, no Rails needed).
# 3. Every props-diff page case (props-dump), every MCP case (mcp-dump) and
#    every API case (api-dump) with RW_SQL_TRACE=1: each statement a union
#    reader runs, binds expanded.
# 4. plan-audit: EXPLAIN QUERY PLAN over each distinct statement through a
#    union reader of the same directory; prints which indexes some plan uses
#    and which no plan does (report.json).
# BIN_DIR (target/release), PORT (8812), KEEP=1 keeps the work dir.
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
BIN=${BIN_DIR:-$ROOT/target/release} PORT=${PORT:-8812}
W=${1:-$(mktemp -d /dev/shm/rwps/audit.XXXX)}
mkdir -p "$W"
export RAILWATCH_DATA_DIR=$W/data RAILWATCH_RATE_LIMIT_ENV=0 PORT
NOW=$(date +%s)
if [ ! -f "$W/batches/manifest.json" ]; then
  python3 -I "$ROOT/bench/diff/props/gen_tenant.py" "$ROOT/crates/server/tests/fixtures/wire_fixtures.json" "$W/batches" --now "$NOW" \
    --spread-days 8 --executions 60000 --seed 5 --records-per-batch 2000 --shapes 2000 --tenants 50 --uniform --by-time --few-logs --prod-shape --sessions
fi
rm -rf "$W/data" "$W/single.sqlite3"
tok=$("$BIN/railwatch" env create e1 | grep -o 'rw_[A-Za-z0-9]*')
"$BIN/railwatch" serve > "$W/server.log" 2>&1 &
pid=$!
trap 'kill $pid 2>/dev/null || true; wait $pid 2>/dev/null || true; [ -n "${KEEP:-}" ] || rm -rf "$W"' EXIT
until curl -sf "127.0.0.1:$PORT/up" >/dev/null; do sleep 0.1; done
python3 -I "$ROOT/bench/diff/post.py" "http://127.0.0.1:$PORT/ingest" "$tok" "$W/batches" "$W/acks.json"
# The deploys too: the release, deploy and compare reads filter by them.
python3 -I - "$W/batches/deploys.json" "$tok" "$PORT" <<'PY'
import json, sys, urllib.request
for d in json.load(open(sys.argv[1])):
    req = urllib.request.Request(f"http://127.0.0.1:{sys.argv[3]}/ingest/deploys", data=json.dumps(d).encode(), method="POST",
        headers={"Authorization": f"Bearer {sys.argv[2]}", "Content-Type": "application/json"})
    urllib.request.urlopen(req).read()
PY
# Let the rollup worker settle (the pages read rollups), then stop.
sleep 20
kill $pid; wait $pid 2>/dev/null || true
T=$W/data/telemetry/e1
MEMBER=$(python3 -I "$ROOT/bench/storage/audit_setup.py" "$W/data")
# A single-file export for the case generators (they read it with Python's sqlite3).
"$BIN/railwatch" export e1 "$W/single.sqlite3" > /dev/null
# props-dump's control (dashboard::control::StaticControl): the deploys as posted.
python3 -I - "$W/data/control.sqlite3" "$W/control.json" <<'PY'
import datetime, json, sqlite3, sys
db = sqlite3.connect(sys.argv[1])
us = lambda s: int(datetime.datetime.fromisoformat(s).replace(tzinfo=datetime.timezone.utc).timestamp() * 1e6)
deploys = [{"id": i, "deploy": d, "ref": r, "name": n, "url": u, "server": s, "deployed_at": us(at), "previous_ref": p, "commits": json.loads(c)}
           for i, d, r, n, u, s, at, p, c in db.execute("SELECT id, deploy, ref, name, url, server, deployed_at, previous_ref, commits FROM deploys ORDER BY id")]
json.dump({"deploys": deploys}, open(sys.argv[2], "w"))
PY
python3 -I "$ROOT/bench/diff/props/make_cases.py" "$W/single.sqlite3" "$W/control.json" "$W/props.json" > /dev/null
python3 -I "$ROOT/bench/diff/mcp/make_cases.py" "$W/single.sqlite3" "$W/data/control.sqlite3" 1 "$W/mcp.json"
python3 -I "$ROOT/bench/diff/api/make_cases.py" "$W/single.sqlite3" "$W/data/control.sqlite3" 1 "$MEMBER" "$W/api.json"
RW_SQL_TRACE=1 "$BIN/props-dump" "$T" "$W/props.json" "$W/control.json" "$NOW" 1 "$W/props.out.json" 0 2> "$W/props.trace" > /dev/null || true
RW_SQL_TRACE=1 "$BIN/mcp-dump" "$W/data" "$W/mcp.json" "$NOW" "$W/mcp.out.json" 0 2> "$W/mcp.trace" > /dev/null || true
RW_SQL_TRACE=1 "$BIN/api-dump" "$W/data" "$W/api.json" "$NOW" "$W/api.out.json" 0 2> "$W/api.trace" > /dev/null || true
for f in props mcp api; do echo "$f: $(grep -c ^SQLTRACE "$W/$f.trace") statements traced, $(python3 -I -c "import json,sys;d=json.load(open(sys.argv[1]));print(len(d), 'cases,', sum(1 for v in d.values() if isinstance(v, dict) and str(v.get('status','200'))[0] not in '2'), 'not 2xx')" "$W/$f.out.json")"; done
"$BIN/plan-audit" "$T" "$W/props.trace" "$W/mcp.trace" "$W/api.trace" > "$W/report.json"
python3 -I - "$W/report.json" <<'PY'
import json, sys
r = json.load(open(sys.argv[1]))
print(f"{r['statements']} distinct statements, {len(r['failed'])} failed to plan")
for f in r["failed"][:5]:
    print("  failed:", f["error"], "|", f["sql"][:120])
print("used:")
for k, v in sorted(r["used"].items(), key=lambda kv: -kv[1]["statements"]):
    print(f"  {k:52s} {v['statements']:5d}")
print("unused (no plan names it):")
for u in r["unused"]:
    print(f"  {u['table']:18s} {u['index']}")
PY
echo "work dir $W"
