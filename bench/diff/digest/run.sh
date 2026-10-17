#!/usr/bin/env bash
# Weekly digest diff: WeeklyDigestJob + DigestMailer#weekly rendered by Rails
# (rw-baseline, capture_rails.rb under travel_to(NOW)) and by Rust
# (jobs::weekly_digest via digest-dump at NOW) over the SAME primary rows
# and tenant files, then subject, recipients, text and HTML compared. The
# only normalisation is the server's base URL (Rails' default_url_options
# vs the Rust `url`), replaced by the same placeholder on both sides.
#
#   bench/diff/digest/run.sh WORK_DIR [NOW_EPOCH]
#
# 1. setup_rails.rb adds edge-case fixtures (setup_rails.rb's header).
# 2. Rails renders every digest a run at NOW would send.
# 3. The Rails primary and every tenant file are copied out and imported
#    into a fresh Rust data dir (`railwatch import-rails`); Rust renders.
# 4. compare.py diffs the two lists.
#
# SPLIT=1: tenants are moved to the per-day layout first (`railwatch split
# --all`), so the rollups are read through the union reader.
set -euo pipefail
W=${1:?usage: run.sh WORK_DIR [NOW_EPOCH]}
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../../.." && pwd)
URL=https://railwatch.example.com
cargo build -q --release -p railwatch-server --bin digest-dump --bin railwatch --manifest-path "$ROOT/Cargo.toml"
BIN=$ROOT/target/release
rm -rf "$W"; mkdir -p "$W/telemetry"

# NOW: a Monday 08:00 UTC after the newest rollup, unless given.
SRC_SLUG=$(docker exec -i rw-baseline bin/rails runner - 2>/dev/null <<'RUBY' | tail -1
puts Environment.order(:id).first.slug
RUBY
)
NOW=${2:-$(docker exec -i -e SLUG="$SRC_SLUG" rw-baseline bin/rails runner - 2>/dev/null <<'RUBY' | tail -1
t = Environment.order(:id).first.with_telemetry { Railwatch::Telemetry::Rollup.maximum(:bucket) } || Time.current
puts (t.utc.to_date + 1).next_occurring(:monday).in_time_zone("UTC").change(hour: 8).to_i
RUBY
)}
echo "$NOW" > "$W/now"
echo "NOW=$NOW ($(date -u -d "@$NOW"))"

docker cp "$HERE/setup_rails.rb" rw-baseline:/tmp/digest_setup.rb > /dev/null
docker exec -e NOW="$NOW" -e SRC_SLUG="$SRC_SLUG" rw-baseline bin/rails runner /tmp/digest_setup.rb 2>/dev/null | grep SETUP

docker cp "$HERE/capture_rails.rb" rw-baseline:/tmp/digest_capture.rb > /dev/null
docker exec -e NOW="$NOW" -e URL_PLACEHOLDER="$URL" rw-baseline bin/rails runner /tmp/digest_capture.rb 2>/dev/null | grep CAPTURED
docker cp rw-baseline:/tmp/digest-rails.json "$W/rails.json" > /dev/null

# Primary + tenants, checkpointed, -> Rust. (-L: some tenants are symlinks.)
docker exec -i rw-baseline bin/rails runner - >/dev/null 2>&1 <<'RUBY'
Environment.find_each { |e| e.with_telemetry { Railwatch::TelemetryRecord.connection.execute("PRAGMA wal_checkpoint(TRUNCATE)") } if e.telemetry_exists? }
RUBY
docker exec rw-baseline sqlite3 /rails/storage/production.sqlite3 ".backup /tmp/digest-primary.sqlite3"
docker cp rw-baseline:/tmp/digest-primary.sqlite3 "$W/primary.sqlite3" > /dev/null
docker exec rw-baseline rm -f /tmp/digest-primary.sqlite3
for slug in $(sqlite3 "$W/primary.sqlite3" "SELECT slug FROM environments"); do
  docker cp -L "rw-baseline:/rails/storage/telemetry/$slug.sqlite3" "$W/telemetry/$slug.sqlite3" > /dev/null 2>&1 || true
done
export RAILWATCH_DATA_DIR=$W/rust
"$BIN/railwatch" import-rails --primary "$W/primary.sqlite3" --telemetry-dir "$W/telemetry" | grep -E "users|issues|deploys|NOT IMPORTED" || true
if [ -n "${SPLIT:-}" ]; then "$BIN/railwatch" split --all > /dev/null; fi
"$BIN/digest-dump" "$W/rust" "$NOW" "$URL" "$W/rust.json"

python3 -I "$HERE/compare.py" "$W/rails.json" "$W/rust.json" | tee "$W/diff.txt"
