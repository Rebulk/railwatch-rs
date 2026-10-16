#!/usr/bin/env bash
# MCP diff: every McpController tool, prompt and resource, plus the
# protocol and auth error paths, as JSON-RPC over POST /mcp to Rails
# (rw-baseline) and to the Rust server (web::mcp::call via mcp-dump) over
# the SAME control rows and tenant file, at the SAME frozen NOW.
#
#   bench/diff/mcp/run.sh PROPS_WORK [warm_runs=0]
#
# PROPS_WORK is a finished bench/diff/props/run.sh work dir (its scratch
# environment's issues are grouped by Rails, its tenant file copied out).
#
# 1. setup_rails.rb issues six tokens (full, read-only, api-only,
#    mcp-without-read, revoked, restricted to another app).
# 2. The Rails primary DB is backed up and imported into a fresh Rust data
#    dir (ids, token digests, issues kept); the tenant file is adopted.
# 3. make_cases.py builds the cases; NOW is the tenant's last execution
#    plus a minute, so the windows hold data.
# 4. Rails: capture_rails.rb POSTs each case through the real middleware
#    under travel_to(NOW). Rust: mcp-dump at NOW. Write cases run on both
#    (each on its own copy of the issues), then their activity entries are
#    dumped from both control databases.
# 5. compare.py diffs the responses; activities.py diffs the activities.
#
# SPLIT=1: the Rust tenant is moved to the per-day layout first
# (`railwatch split`), so the tools read through the union reader.
# PERF=1: read cases only, no write diff (perf.sh times them).
set -euo pipefail
PW=${1:?usage: run.sh PROPS_WORK [warm]} WARM=${2:-0}
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../../.." && pwd)
cargo build -q --release -p railwatch-server --bin mcp-dump --bin railwatch --manifest-path "$ROOT/Cargo.toml"
BIN=$ROOT/target/release
SLUG=$(cat "$PW/slug") ENV_ID=$(cat "$PW/env_id")
W=$PW/mcp; rm -rf "$W"; mkdir -p "$W/telemetry"

# 1. Tokens (and the restricted token's second application) before the backup.
docker cp "$HERE/setup_rails.rb" rw-baseline:/tmp/mcp_setup.rb > /dev/null
docker exec -e SLUG="$SLUG" rw-baseline bin/rails runner /tmp/mcp_setup.rb 2>/dev/null | grep TOKENS
docker cp rw-baseline:/tmp/mcp-tokens.json "$W/tokens.json" > /dev/null

# 2. Rails primary snapshot -> Rust data dir.
docker exec rw-baseline sqlite3 /rails/storage/production.sqlite3 ".backup /tmp/mcp-primary.sqlite3"
docker cp rw-baseline:/tmp/mcp-primary.sqlite3 "$W/primary.sqlite3" > /dev/null
docker exec rw-baseline rm -f /tmp/mcp-primary.sqlite3
cp "$PW/tenant.sqlite3" "$W/telemetry/$SLUG.sqlite3"
export RAILWATCH_DATA_DIR=$W/rust
"$BIN/railwatch" import-rails --primary "$W/primary.sqlite3" --telemetry-dir "$W/telemetry" | grep -E "api_tokens|issues|NOT IMPORTED" || true
if [ -n "${SPLIT:-}" ]; then "$BIN/railwatch" split "$SLUG" > /dev/null; fi
cp "$W/tokens.json" "$W/rust/mcp-tokens.json"

# 3. Cases at NOW.
NOW=$(sqlite3 "$PW/tenant.sqlite3" "SELECT CAST(strftime('%s', MAX(occurred_at)) AS INTEGER) + 60 FROM executions")
echo "$NOW" > "$W/now"
python3 -I "$HERE/make_cases.py" "$PW/tenant.sqlite3" "$W/primary.sqlite3" "$ENV_ID" "$W/cases.json" ${PERF:+--perf}

# 4. Both sides.
docker cp "$W/cases.json" rw-baseline:/tmp/mcp-cases.json > /dev/null
docker cp "$W/tokens.json" rw-baseline:/tmp/mcp-tokens.json > /dev/null
docker cp "$HERE/capture_rails.rb" rw-baseline:/tmp/mcp_capture.rb > /dev/null
docker exec -e NOW="$NOW" -e WARM="$WARM" rw-baseline bin/rails runner /tmp/mcp_capture.rb 2>/dev/null | grep CAPTURED
docker cp rw-baseline:/tmp/mcp-rails.json "$W/rails.json" > /dev/null
docker cp rw-baseline:/tmp/mcp-rails-timings.json "$W/rails.timings.json" > /dev/null
"$BIN/mcp-dump" "$W/rust" "$W/cases.json" "$NOW" "$W/rust.json" "$WARM"

if [ -n "${PERF:-}" ]; then
  python3 -I "$HERE/compare.py" "$W/rails.json" "$W/rust.json" | tee "$W/diff.txt"
  exit 0
fi

# 5. What the writes left: issue_activities and comments above the
#    snapshot's high-water marks (the same on both sides: Rust imported
#    that snapshot), with users by email.
A=$(sqlite3 "$W/primary.sqlite3" "SELECT COALESCE(MAX(id), 0) FROM issue_activities")
C=$(sqlite3 "$W/primary.sqlite3" "SELECT COALESCE(MAX(id), 0) FROM comments")
WRITES="SELECT json_object(
  'activities', (SELECT json_group_array(json_object('issue_id', a.issue_id, 'kind', a.kind, 'user', u.email, 'data', json(a.data)))
                 FROM (SELECT * FROM issue_activities WHERE id > $A ORDER BY id) a LEFT JOIN users u ON u.id = a.user_id),
  'comments', (SELECT json_group_array(json_object('issue_id', c.issue_id, 'body', c.body, 'source', c.source, 'user', u.email))
               FROM (SELECT * FROM comments WHERE id > $C ORDER BY id) c LEFT JOIN users u ON u.id = c.user_id))"
docker exec rw-baseline sqlite3 /rails/storage/production.sqlite3 "$WRITES" > "$W/rails.writes.json"
sqlite3 "$W/rust/control.sqlite3" "$WRITES" > "$W/rust.writes.json"

python3 -I "$HERE/compare.py" "$W/rails.json" "$W/rust.json" ${VERBOSE:+--verbose} | tee "$W/diff.txt"
python3 -I "$HERE/writes.py" "$W/rails.writes.json" "$W/rust.writes.json" | tee -a "$W/diff.txt"
