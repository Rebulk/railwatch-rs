#!/usr/bin/env bash
# Issues props diff: /issues (index with filters), /issues/:id (show) and
# /issues/:id/merge_candidates from Rails (rw-baseline) and from the Rust
# server over the SAME control rows and tenant file.
#
#   bench/diff/issues/run.sh PROPS_WORK [port=8799]
#
# PROPS_WORK is a finished bench/diff/props/run.sh work dir: its scratch
# environment has issues grouped by Rails (settle_rails.rb) and its tenant
# file copied out. Rust side: `railwatch import-rails` of a backup of
# rw-baseline's primary DB (ids preserved), that one tenant file adopted,
# the props-diff user's password set to a known one, then the same GETs.
# Shared props (auth, account(s), flash, applications, errors) are left out;
# everything else is compared value for value. Both sides use the real clock
# and are fetched a few seconds apart (show's 30-day `daily` is day-bucketed).
#
# SPLIT=1: after import-rails, `railwatch split` moves the tenant to the
# per-day layout, so the Rust side reads it through the union reader.
set -euo pipefail
PW=${1:?usage: run.sh PROPS_WORK [port]} PORT=${2:-8799}
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../../.." && pwd)
BIN=$ROOT/target/release/railwatch
SLUG=$(cat "$PW/slug") ENV_ID=$(cat "$PW/env_id")
W=$PW/issues; rm -rf "$W"; mkdir -p "$W/telemetry"

# 1. Rails primary snapshot + the scratch tenant.
docker exec rw-baseline sqlite3 /rails/storage/production.sqlite3 ".backup /tmp/issues-primary.sqlite3"
docker cp rw-baseline:/tmp/issues-primary.sqlite3 "$W/primary.sqlite3" > /dev/null
docker exec rw-baseline rm -f /tmp/issues-primary.sqlite3
cp "$PW/tenant.sqlite3" "$W/telemetry/$SLUG.sqlite3"

# 2. Paths: index variants, then show + merge_candidates for this env's issues.
python3 -I - "$W/primary.sqlite3" "$ENV_ID" "$W/paths.json" <<'PY'
import json, sqlite3, sys, urllib.parse
c = sqlite3.connect(f"file:{sys.argv[1]}?mode=ro", uri=True)
env = int(sys.argv[2])
rows = c.execute("SELECT id, kind, status, title FROM issues WHERE environment_id = ? ORDER BY id", (env,)).fetchall()
app = c.execute("SELECT application_id FROM environments WHERE id = ?", (env,)).fetchone()[0]
paths = ["/issues", "/issues?status=open", "/issues?status=resolved", "/issues?kind=exception", f"/issues?environment_id={env}",
         f"/issues?application_id={app}", "/issues?q=Error", "/issues?q=source:application", "/issues?mine=1"]
for (i, kind, status, title) in rows[:12]:
    paths += [f"/issues/{i}", f"/issues/{i}/merge_candidates", f"/issues/{i}/merge_candidates?q={urllib.parse.quote(title.split(':')[0][:12])}"]
json.dump(paths, open(sys.argv[3], "w"))
print(f"{len(rows)} issues in env {env} ({', '.join(sorted({r[1] for r in rows}))}); {len(paths)} paths")
PY

# 3. Rust: import, password, serve.
export RAILWATCH_DATA_DIR=$W/rust RAILWATCH_BIND=127.0.0.1:$PORT RAILWATCH_URL=http://127.0.0.1:$PORT
"$BIN" import-rails --primary "$W/primary.sqlite3" --telemetry-dir "$W/telemetry" | grep -E "issues|comments|MISSING|NOT IMPORTED" || true
if [ -n "${SPLIT:-}" ]; then
  # import-rails symlinks the file; split reads through the link.
  "$BIN" split "$SLUG"
fi
DIGEST=$(docker exec rw-baseline bin/rails runner 'print BCrypt::Password.create("issues diff password", cost: 4)' 2>/dev/null | tail -1)
sqlite3 "$W/rust/control.sqlite3" "UPDATE users SET password_digest = '$DIGEST' WHERE email = 'props-diff@example.com'"
EMAIL=props-diff@example.com
"$BIN" serve > "$W/rust.log" 2>&1 &
RP=$!; trap 'kill $RP 2>/dev/null || true' EXIT
until curl -sf "127.0.0.1:$PORT/up" > /dev/null; do sleep 0.1; done

# 4. Capture both, back to back.
docker cp "$W/paths.json" rw-baseline:/tmp/issues-paths.json > /dev/null
docker cp "$HERE/capture_rails.rb" rw-baseline:/tmp/issues_capture.rb > /dev/null
docker exec rw-baseline bin/rails runner /tmp/issues_capture.rb 2>/dev/null | grep CAPTURED
docker cp rw-baseline:/tmp/issues-rails.json "$W/rails.json" > /dev/null
python3 -I "$HERE/capture_rust.py" "http://127.0.0.1:$PORT" "$EMAIL" "issues diff password" "$W/paths.json" "$W/rust.json"
python3 -I "$HERE/compare.py" "$W/rails.json" "$W/rust.json" | tee "$W/diff.txt"
