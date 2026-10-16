#!/usr/bin/env bash
# Grouping equivalence: one tenant's exceptions through Rails'
# GroupExceptionsJob (Ingest::ExceptionGrouping, Issue.record_occurrence!)
# and the Rust port, same chunking, same deploy marker; the issues and their
# activities compared field by field.
#
#   [RESOLVE_AND_REPEAT=1] bench/diff/grouping_diff.sh RAW_TENANT.sqlite3 [chunk=50] ["deploy at, UTC"]
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
SRC=${1:?usage: grouping_diff.sh RAW_TENANT.sqlite3 [chunk] [deploy_at]}
CHUNK=${2:-50}
DEPLOY_AT=${3:-}
RUN=$(date +%s)
WORK=${WORK:-/tmp/rw-gdiff-$RUN}
mkdir -p "$WORK"
cargo build -q --release -p railwatch-server --bin rw-group --manifest-path "$ROOT/Cargo.toml"
sqlite3 "$SRC" ".backup $WORK/raw.sqlite3"

NAME=gdiff-$RUN
SLUG=$(docker exec -i -e NAME="$NAME" rw-baseline bin/rails runner - <<'RUBY' | sed -n 's/^SLUG //p'
app = Account.find_by!(slug: "bench").applications.find_by!(slug: "bench")
puts "SLUG #{app.environments.create!(name: ENV.fetch("NAME")).slug}"
RUBY
)
DEST=/rails/storage/telemetry/$SLUG.sqlite3
docker exec rw-baseline rm -f "$DEST" "$DEST-wal" "$DEST-shm"
docker cp "$WORK/raw.sqlite3" "rw-baseline:$DEST" >/dev/null
docker exec -u 0 rw-baseline chown rails:rails "$DEST"
docker cp "$HERE/grouping_rails.rb" rw-baseline:/tmp/grouping_rails.rb
docker exec -e SLUG="$SLUG" -e CHUNK="$CHUNK" -e DEPLOY_AT="$DEPLOY_AT" -e RESOLVE_AND_REPEAT="${RESOLVE_AND_REPEAT:-}" rw-baseline bin/rails runner /tmp/grouping_rails.rb 2>&1 | sed -n 's/^RWJSON //p' > "$WORK/rails.json"

ARGS=(--chunk "$CHUNK"); [ -n "${RESOLVE_AND_REPEAT:-}" ] && ARGS+=(--resolve-and-repeat); [ -n "$DEPLOY_AT" ] && ARGS+=(--deploy-at "$DEPLOY_AT")
"$ROOT/target/release/rw-group" "$WORK/raw.sqlite3" "$WORK/control.sqlite3" "${ARGS[@]}" | sed -n 's/^RWJSON //p' > "$WORK/rust.json"

python3 -I - "$WORK/rails.json" "$WORK/rust.json" <<'PY'
import json, sys
a, b = json.load(open(sys.argv[1])), json.load(open(sys.argv[2]))
# The deploy-swap check and regressions aside, alert payloads/keys differ by
# design (no applications yet), so only issue fields and activities compare.
bad = 0
print(f"issues: rails={len(a)} rust={len(b)}")
bad += len(a) != len(b)
for x, y in zip(a, b):
    for k in x:
        if x[k] != y.get(k):
            bad += 1
            print(f"  {x['group_hash']} {k}: rails={json.dumps(x[k])[:200]} rust={json.dumps(y.get(k))[:200]}")
print("IDENTICAL" if not bad else f"{bad} DIFFERENCES")
sys.exit(1 if bad else 0)
PY
echo "work dir $WORK (scratch env $SLUG)"
