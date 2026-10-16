#!/usr/bin/env bash
# Rollup equivalence: the same raw tenant data through Rails' RollupJob /
# ReleaseHealthRollupJob (rw-baseline) and the Rust port, then the rollups
# and release_health tables compared row by row, ids and digest bytes
# included.
#
#   bench/diff/rollup_diff.sh RAW_TENANT.sqlite3 [seed=42]
#
# RAW_TENANT is any tenant file with raw rows (e.g. rails.sqlite3 from
# run.sh, or one filled by rw-load). Its rollups/release_health are emptied
# first; both sides then recompute every hour that holds raw rows, ascending,
# from one Ruby MT stream seeded with `srand(seed)` (Rust: rw-rollup --seed).
# Rails runs in a scratch environment `rdiff-<run>` whose tenant file is the
# snapshot; nothing ingests into it, so the jobs container never touches it.
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
SRC=${1:?usage: rollup_diff.sh RAW_TENANT.sqlite3 [seed]}
SEED=${2:-42}
RUN=$(date +%s)
WORK=${WORK:-/tmp/rw-rdiff-$RUN}
mkdir -p "$WORK"
cargo build -q --release -p railwatch-server --bin rw-rollup --manifest-path "$ROOT/Cargo.toml"

sqlite3 "$SRC" ".backup $WORK/raw.sqlite3"
sqlite3 "$WORK/raw.sqlite3" "DELETE FROM rollups; DELETE FROM release_health; PRAGMA wal_checkpoint(TRUNCATE);"
HOURS=$(sqlite3 "$WORK/raw.sqlite3" "
  SELECT DISTINCT CAST(strftime('%s', strftime('%Y-%m-%d %H:00:00', occurred_at)) AS INTEGER) AS h FROM (
    SELECT occurred_at FROM executions UNION ALL SELECT occurred_at FROM queries UNION ALL SELECT occurred_at FROM outgoing_requests
    UNION ALL SELECT occurred_at FROM cache_events UNION ALL SELECT occurred_at FROM mails UNION ALL SELECT occurred_at FROM visits
    UNION ALL SELECT occurred_at FROM spans UNION ALL SELECT occurred_at FROM notifications UNION ALL SELECT occurred_at FROM view_renders
    UNION ALL SELECT occurred_at FROM transactions UNION ALL SELECT occurred_at FROM llm_calls UNION ALL SELECT occurred_at FROM sessions)
  ORDER BY h" | tr '\n' ' ')
echo "hours: $HOURS"

# Rails
NAME=rdiff-$RUN
SLUG=$(docker exec -i -e NAME="$NAME" rw-baseline bin/rails runner - <<'RUBY' | sed -n 's/^SLUG //p'
app = Account.find_by!(slug: "bench").applications.find_by!(slug: "bench")
puts "SLUG #{app.environments.create!(name: ENV.fetch("NAME")).slug}"
RUBY
)
DEST=/rails/storage/telemetry/$SLUG.sqlite3
docker exec rw-baseline rm -f "$DEST" "$DEST-wal" "$DEST-shm"
docker cp "$WORK/raw.sqlite3" "rw-baseline:$DEST" >/dev/null
docker exec -u 0 rw-baseline chown rails:rails "$DEST"
docker cp "$HERE/rollup_rails.rb" rw-baseline:/tmp/rollup_rails.rb
docker exec -e SLUG="$SLUG" -e SEED="$SEED" -e HOURS="$HOURS" rw-baseline bin/rails runner /tmp/rollup_rails.rb 2>&1 | grep '^RW ' | tee "$WORK/rails-timing.txt"
docker exec rw-baseline sqlite3 "$DEST" ".backup /tmp/$SLUG.copy.sqlite3"
docker cp "rw-baseline:/tmp/$SLUG.copy.sqlite3" "$WORK/rails.sqlite3" >/dev/null
docker exec rw-baseline rm -f "/tmp/$SLUG.copy.sqlite3"

# Rust
cp "$WORK/raw.sqlite3" "$WORK/rust.sqlite3"
# Two passes, as the Rails side runs (its first is a warm-up), so the
# AUTOINCREMENT ids line up too; the second pass is the one timed.
"$ROOT/target/release/rw-rollup" "$WORK/rust.sqlite3" $HOURS --seed "$SEED" >/dev/null 2>&1
"$ROOT/target/release/rw-rollup" "$WORK/rust.sqlite3" $HOURS --seed "$SEED" 2> "$WORK/rust-timing.txt" | tee -a "$WORK/rust-timing.txt"

python3 -I "$HERE/compare_rollups.py" "$WORK/rails.sqlite3" "$WORK/rust.sqlite3"
echo "work dir $WORK (scratch env $SLUG)"
