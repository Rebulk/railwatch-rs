#!/usr/bin/env bash
# Dashboard read latency, Rails vs Rust, on the SAME data and the same user.
#
#   bench/rails-vs-rust/pages.sh [BATCHES=1500] [REPS=3]
#
# 1. Fresh Rails (production image, in rvr.slice), one environment; rw-load
#    sends BATCHES x 500 records with timestamps spread over the last 7 days
#    (--spread-days 7); RollupJob for every hour and exception grouping run
#    through the jobs container (settle_rails.rb), a user is created.
# 2. Rails is stopped; the whole storage dir is copied and imported into a
#    fresh Rust data dir with `railwatch import-rails`: same tenant file,
#    same user/password, same ids. Rust then reads the file in Rails' single
#    file layout (import adopts files in place; per-day splitting is a
#    separate, offline `railwatch split`), so this measures the two servers'
#    read paths on one identical SQLite file, not the storage layouts.
# 3. REPS times per side: start the server in the slice, drop the host page
#    cache (cold: the file is read from the device), sign in, time the 5
#    main pages at window=24h, first request = cold, then 10 warm requests.
# Results: bench/results/rails-vs-rust/pages.jsonl
set -euo pipefail
. "$(dirname "$0")/lib.sh"
BATCHES=${1:-1500} REPS=${2:-3}
OUT=$RVR_ROOT/bench/results/rails-vs-rust/pages.jsonl
mkdir -p "$(dirname "$OUT")"
root=$MNT/pages; work=$(mktemp -d /tmp/rvr-pages.XXXX)
EMAIL=bench@example.com PASS=bench-password-123
rvr_stop
if [ -n "${REUSE:-}" ] && [ -d "$root/rust" ]; then
  echo "REUSE: timing the existing $root (seed + import skipped)"
  ENV_PATH=${ENV_PATH:-/apps/1/envs/1}
  rows=$(sqlite3 "file:$root/rails/telemetry/bench-bench-env-0001.sqlite3?mode=ro" "SELECT (SELECT count(*) FROM executions)||' executions, '||(SELECT count(*) FROM queries)||' queries, '||(SELECT count(*) FROM rollups)||' rollups'")
  size=$(stat -c %s "$root/rails/telemetry/bench-bench-env-0001.sqlite3")
  # The pages read window=24h, which ends now: a seed older than that times empty pages.
  recent=$(sqlite3 "file:$root/rails/telemetry/bench-bench-env-0001.sqlite3?mode=ro" "SELECT count(*) FROM executions WHERE occurred_at >= datetime('now', '-1 day')")
  [ "$recent" -gt 0 ] || { echo "REUSE: no executions in the last 24 h; reseed (unset REUSE)" >&2; exit 1; }
  rows="$rows, $recent executions in the last 24 h"
else
sudo rm -rf "$root"; mkdir -p "$root"

# 1. seed through Rails' own ingest
rvr_start_rails "$root/rails"
rvr_seed_rails 1 "$work/tokens"
taskset -c "$LOAD_CPUS" "$LOAD_BIN" --url "http://127.0.0.1:$RAILS_PORT" --tokens "$work/tokens" --concurrency 8 \
  --batches "$BATCHES" --duration 3600 --spread-days 7 --xff-rotate 256 --out "$work/seed.json" | sed -n 2p
docker cp "$RVR_ROOT/bench/diff/props/settle_rails.rb" rvr-rails:/tmp/settle_rails.rb
docker exec -e SLUG=bench-bench-env-0001 rvr-rails bin/rails runner /tmp/settle_rails.rb 2>/dev/null | grep SETTLED
docker exec -i -e EMAIL=$EMAIL -e PASS=$PASS rvr-rails bin/rails runner - 2>/dev/null <<'RUBY' | grep '^PATH' > "$work/path"
env = Environment.find_by!(slug: "bench-bench-env-0001")
user = User.find_or_create_by!(email: ENV["EMAIL"]) { |u| u.name = "Bench"; u.password = ENV["PASS"] }
user.update!(password: ENV["PASS"])
env.application.account.memberships.find_or_create_by!(user: user) { |m| m.role = "owner" }
env.with_telemetry { TelemetryRecord.connection.execute("PRAGMA wal_checkpoint(TRUNCATE)") }
puts "PATH /apps/#{env.application_id}/envs/#{env.id}"
RUBY
ENV_PATH=$(sed -n 's/^PATH //p' "$work/path")
rows=$(sqlite3 "file:$root/rails/telemetry/bench-bench-env-0001.sqlite3?mode=ro" "SELECT (SELECT count(*) FROM executions)||' executions, '||(SELECT count(*) FROM queries)||' queries, '||(SELECT count(*) FROM rollups)||' rollups'")
size=$(stat -c %s "$root/rails/telemetry/bench-bench-env-0001.sqlite3")
echo "tenant: $rows, $size bytes; env $ENV_PATH"
rvr_stop

# 2. the same install, imported into Rust
# import-rails adopts tenant files where they are (a symlink), so it gets a
# copy of its own that stays (mounted at the same path in the Rust
# container so the link resolves): the two servers never share a file.
cp -a "$root/rails" "$root/rails-src"
RAILWATCH_DATA_DIR=$root/rust "$RUST_BIN" import-rails --primary "$root/rails-src/production.sqlite3" --telemetry-dir "$root/rails-src/telemetry" | tail -3
fi

# 3. time (pages.jsonl holds this invocation's runs only)
: > "$OUT"
for rep in $(seq 1 "$REPS"); do
  for side in rails rust; do
    rvr_stop
    if [ "$side" = rails ]; then rvr_start_rails "$root/rails"; url=http://127.0.0.1:$RAILS_PORT
    else
      skb=$(sed -n 's/^RAILWATCH_SECRET_KEY_BASE=//p' "$SECRETS")
      docker run -d --name rvr-rust --cgroup-parent rvr.slice --cpuset-cpus "$SERVER_CPUS" --network host --user 1000:1000 \
        -v "$RUST_BIN:/usr/local/bin/railwatch:ro" -v "$root/rust:/data" -v "$root/rails-src:$root/rails-src" -e RAILWATCH_DATA_DIR=/data -e PORT=$RUST_PORT \
        -e RAILWATCH_SECRET_KEY_BASE="$skb" -e RAILWATCH_URL=http://127.0.0.1:$RUST_PORT debian:13 railwatch serve >/dev/null
      rvr_wait "http://127.0.0.1:$RUST_PORT/up"; url=http://127.0.0.1:$RUST_PORT
    fi
    sleep 10
    sync; echo 3 | sudo tee /proc/sys/vm/drop_caches >/dev/null
    echo "== rep $rep $side (load $(rvr_load))"
    origin=$url; [ "$side" = rails ] && origin=https://127.0.0.1:$RAILS_PORT
    ORIGIN=$origin taskset -c "$LOAD_CPUS" python3 -I "$RVR_HERE/pages.py" "$url" "$EMAIL" "$PASS" "$ENV_PATH" "$work/$side-$rep.json" 10
    python3 -I - "$work/$side-$rep.json" "$side" "$rep" "$rows" "$size" "$(rvr_load)" >> "$OUT" <<'PY'
import json, sys
r = json.load(open(sys.argv[1]))
print(json.dumps({"side": sys.argv[2], "rep": int(sys.argv[3]), "tenant": sys.argv[4], "tenant_bytes": int(sys.argv[5]), "load_after": sys.argv[6], "pages": r}))
PY
  done
done
rvr_stop
