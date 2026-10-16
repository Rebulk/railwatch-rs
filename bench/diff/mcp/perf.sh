#!/usr/bin/env bash
# Per-tool MCP latency, Rails vs Rust, on one large tenant file.
#
#   bench/diff/mcp/perf.sh TENANT.sqlite3 [warm_runs=3]
#
# TENANT is a multi-day file (rw-load --spread-days 9 against a Rust
# server, then `railwatch export`). As bench/diff/props/perf.sh: a paused
# Rails scratch environment `mcp-perf-<run>` gets the file, settle_rails.rb
# runs Rails' own rollups and grouping for every hour, and the settled file
# is copied back for Rust. Then run.sh's pipeline with --perf cases (reads
# only): both sides time each case WARM+1 times (first dropped). Rails is
# timed through the integration session (routing, middleware, controller,
# JSON) and Rust through web::mcp::call (auth, dispatch, JSON); neither
# includes a socket. The responses are diffed too.
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../../.." && pwd)
SRC=${1:?usage: perf.sh TENANT.sqlite3 [warm]} WARM=${2:-3}
RUN=$(date +%s)
PW=${WORK:-/dev/shm/rw-mcp-perf-$RUN}
mkdir -p "$PW"

docker exec -i -e NAME="mcp-perf-$RUN" rw-baseline bin/rails runner - > "$PW/env.txt" 2>/dev/null <<'RUBY'
account = Account.find_or_initialize_by(slug: "bench")
account.update!(name: "bench", plan: "business", monthly_event_quota: 1_000_000_000_000)
app = account.applications.find_or_create_by!(slug: "bench") { |a| a.name = "bench" }
env = app.environments.create!(name: ENV.fetch("NAME"))
env.update!(paused_at: Time.current)
puts "SLUG #{env.slug}"
puts "ENV_ID #{env.id}"
RUBY
SLUG=$(sed -n 's/^SLUG //p' "$PW/env.txt"); ENV_ID=$(sed -n 's/^ENV_ID //p' "$PW/env.txt")
echo "$SLUG" > "$PW/slug"; echo "$ENV_ID" > "$PW/env_id"
echo "rails env $SLUG (id $ENV_ID)"
# rw-baseline's /rails/storage is an 8 GB tmpfs mostly taken by its own
# nightly backups, so the file lives on the container's disk and is
# symlinked in (SQLite follows the link; -wal/-shm land beside the target).
# The storage volume is shared with rw-baseline-jobs, so the target must
# exist there too: a dangling link gets replaced by an empty tenant. Each
# container gets its own copy; only rw-baseline's is read (and settled).
DEST=/rails/storage/telemetry/$SLUG.sqlite3 REAL=/rails/tmp/mcp-perf/$SLUG.sqlite3
install() {
  for c in rw-baseline rw-baseline-jobs; do
    docker exec -u 0 "$c" sh -c "mkdir -p /rails/tmp/mcp-perf && chown rails:rails /rails/tmp/mcp-perf && rm -f $REAL $REAL-wal $REAL-shm"
    docker cp "$1" "$c:$REAL"
    docker exec -u 0 "$c" chown rails:rails "$REAL"
  done
  docker exec -u 0 rw-baseline sh -c "rm -f $DEST $DEST-wal $DEST-shm && ln -s $REAL $DEST && chown -h rails:rails $DEST"
}
install "$SRC"
docker cp "$ROOT/bench/diff/props/settle_rails.rb" rw-baseline:/tmp/settle_rails.rb
docker exec -e SLUG="$SLUG" rw-baseline bin/rails runner /tmp/settle_rails.rb 2>/dev/null | grep SETTLED
docker exec -i -e SLUG="$SLUG" rw-baseline bin/rails runner - >/dev/null 2>&1 <<'RUBY'
Environment.find_by!(slug: ENV["SLUG"]).with_telemetry { Railwatch::TelemetryRecord.connection.execute("PRAGMA wal_checkpoint(TRUNCATE)") }
RUBY
docker cp "rw-baseline:$REAL" "$PW/tenant.sqlite3"
install "$PW/tenant.sqlite3"

PERF=1 bash "$HERE/run.sh" "$PW" "$WARM"
python3 -I "$HERE/timings.py" "$PW/mcp/rails.timings.json" "$PW/mcp/rust.json.timings.json" | tee "$PW/mcp/timings.md"
