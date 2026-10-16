#!/usr/bin/env bash
# API diff: every /api/v1 endpoint, plus the auth, scope, routing,
# pagination and write error paths, as HTTP requests to Rails (rw-baseline,
# the real middleware and Api::V1 controllers) and to the Rust server
# (web::api::call via api-dump) over the SAME control rows and tenant file,
# at the SAME frozen NOW.
#
#   bench/diff/api/run.sh PROPS_WORK [warm_runs=0]
#
# PROPS_WORK is a finished bench/diff/props/run.sh work dir (its scratch
# environment's issues are grouped by Rails). As bench/diff/mcp/run.sh:
# 1. NOW is the tenant's last execution plus a minute.
# 2. setup_rails.rb issues eight tokens (full, read-only, mcp-only,
#    no-read, revoked, expired, restricted to another app, restricted to
#    the account) and the fixtures the props dir lacks (a second member,
#    named deploys, a merged issue, a Linear comment).
# 3. The tenant file is copied out of Rails afresh (checkpointed), the
#    Rails primary backed up and imported into a fresh Rust data dir
#    (ids, token digests, issues kept).
# 4. make_cases.py builds the cases; Rails: capture_rails.rb under
#    travel_to(NOW); Rust: api-dump at NOW. Write cases run on both (each on
#    its own copy), then their activities, comments and deploys are dumped
#    from both control databases.
# 5. compare.py diffs the responses (status, headers, body); writes.py the
#    rows the writes left.
#
# SPLIT=1: the Rust tenant is moved to the per-day layout first
# (`railwatch split`), so every endpoint reads through the union reader.
# PERF=1: read cases only, no write diff (perf.sh times them).
set -euo pipefail
PW=${1:?usage: run.sh PROPS_WORK [warm]} WARM=${2:-0}
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../../.." && pwd)
cargo build -q --release -p railwatch-server --bin api-dump --bin railwatch --manifest-path "$ROOT/Cargo.toml"
BIN=$ROOT/target/release
SLUG=$(cat "$PW/slug") ENV_ID=$(cat "$PW/env_id")
W=$PW/api${SPLIT:+-split}; rm -rf "$W"; mkdir -p "$W/telemetry"

# 1-2. The tenant as Rails has it now, NOW from it, then tokens and fixtures.
docker exec -i -e SLUG="$SLUG" rw-baseline bin/rails runner - >/dev/null 2>&1 <<'RUBY'
Environment.find_by!(slug: ENV["SLUG"]).with_telemetry { Railwatch::TelemetryRecord.connection.execute("PRAGMA wal_checkpoint(TRUNCATE)") }
RUBY
# (-L: perf.sh's large tenant is a symlink into /rails/tmp.)
docker cp -L "rw-baseline:/rails/storage/telemetry/$SLUG.sqlite3" "$W/telemetry/$SLUG.sqlite3" > /dev/null
NOW=$(sqlite3 "$W/telemetry/$SLUG.sqlite3" "SELECT CAST(strftime('%s', MAX(occurred_at)) AS INTEGER) + 60 FROM executions")
echo "$NOW" > "$W/now"
docker cp "$HERE/setup_rails.rb" rw-baseline:/tmp/api_setup.rb > /dev/null
MEMBER=$(docker exec -e SLUG="$SLUG" -e NOW="$NOW" rw-baseline bin/rails runner /tmp/api_setup.rb 2>/dev/null | sed -n 's/^TOKENS .* MEMBER //p')
docker cp rw-baseline:/tmp/api-tokens.json "$W/tokens.json" > /dev/null

# 3. Rails primary snapshot -> Rust data dir.
docker exec rw-baseline sqlite3 /rails/storage/production.sqlite3 ".backup /tmp/api-primary.sqlite3"
docker cp rw-baseline:/tmp/api-primary.sqlite3 "$W/primary.sqlite3" > /dev/null
docker exec rw-baseline rm -f /tmp/api-primary.sqlite3
export RAILWATCH_DATA_DIR=$W/rust
"$BIN/railwatch" import-rails --primary "$W/primary.sqlite3" --telemetry-dir "$W/telemetry" | grep -E "api_tokens|issues|deploys|NOT IMPORTED" || true
if [ -n "${SPLIT:-}" ]; then "$BIN/railwatch" split "$SLUG" > /dev/null; fi
cp "$W/tokens.json" "$W/rust/api-tokens.json"

# 4. Cases, both sides.
python3 -I "$HERE/make_cases.py" "$W/telemetry/$SLUG.sqlite3" "$W/primary.sqlite3" "$ENV_ID" "$MEMBER" "$W/cases.json" ${PERF:+--perf}
docker cp "$W/cases.json" rw-baseline:/tmp/api-cases.json > /dev/null
docker cp "$W/tokens.json" rw-baseline:/tmp/api-tokens.json > /dev/null
docker cp "$HERE/capture_rails.rb" rw-baseline:/tmp/api_capture.rb > /dev/null
docker exec -e NOW="$NOW" -e WARM="$WARM" rw-baseline bin/rails runner /tmp/api_capture.rb 2>/dev/null | grep CAPTURED
docker cp rw-baseline:/tmp/api-rails.json "$W/rails.json" > /dev/null
docker cp rw-baseline:/tmp/api-rails-timings.json "$W/rails.timings.json" > /dev/null
"$BIN/api-dump" "$W/rust" "$W/cases.json" "$NOW" "$W/rust.json" "$WARM"

if [ -n "${PERF:-}" ]; then
  python3 -I "$HERE/compare.py" "$W/rails.json" "$W/rust.json" | tee "$W/diff.txt"
  exit 0
fi

# 5. What the writes left above the snapshot's high-water marks (the same on
#    both sides: Rust imported that snapshot): activities (users by email),
#    comments, and the environment's deploys.
A=$(sqlite3 "$W/primary.sqlite3" "SELECT COALESCE(MAX(id), 0) FROM issue_activities")
C=$(sqlite3 "$W/primary.sqlite3" "SELECT COALESCE(MAX(id), 0) FROM comments")
WRITES="SELECT json_object(
  'activities', (SELECT json_group_array(json_object('issue_id', a.issue_id, 'kind', a.kind, 'user', u.email, 'data', json(a.data), 'created_at', a.created_at))
                 FROM (SELECT * FROM issue_activities WHERE id > $A ORDER BY id) a LEFT JOIN users u ON u.id = a.user_id),
  'comments', (SELECT json_group_array(json_object('issue_id', c.issue_id, 'body', c.body, 'source', c.source, 'user', u.email, 'created_at', c.created_at))
               FROM (SELECT * FROM comments WHERE id > $C ORDER BY id) c LEFT JOIN users u ON u.id = c.user_id),
  'issues', (SELECT json_group_array(json_object('id', id, 'status', status, 'priority', priority, 'assignee_id', assignee_id,
               'resolved_at', resolved_at, 'merged_into_id', merged_into_id, 'occurrences', occurrences, 'updated_at', updated_at))
             FROM (SELECT * FROM issues WHERE environment_id = $ENV_ID ORDER BY id)),
  'deploys', (SELECT json_group_array(json_object('deploy', deploy, 'ref', ref, 'name', name, 'url', url, 'previous_ref', previous_ref,
               'deployed_at', deployed_at, 'commits', json(commits)))
              FROM (SELECT * FROM deploys WHERE environment_id = $ENV_ID ORDER BY deploy)))"
docker exec rw-baseline sqlite3 /rails/storage/production.sqlite3 "$WRITES" > "$W/rails.writes.json"
sqlite3 "$W/rust/control.sqlite3" "$WRITES" > "$W/rust.writes.json"

python3 -I "$HERE/compare.py" "$W/rails.json" "$W/rust.json" ${VERBOSE:+--verbose} | tee "$W/diff.txt"
python3 -I "$HERE/writes.py" "$W/rails.writes.json" "$W/rust.writes.json" | tee -a "$W/diff.txt"
