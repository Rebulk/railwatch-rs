#!/usr/bin/env bash
# Post one gzipped NDJSON batch (one record of every wire type from the gem's
# wire_fixtures.json, timestamps refreshed to now) to the baseline and show
# the ack plus per-table row counts in that environment's tenant file.
#
#   bench/rails/smoke.sh [token-file]   # default tokens-1.txt, first line
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
FIXTURES=${FIXTURES:?set FIXTURES to the railwatch gem's lib/railwatch/wire_fixtures.json}
TOKEN=$(head -1 "${1:-$HERE/tokens-1.txt}")
URL=${URL:-http://127.0.0.1:8780/ingest}
TMP=$(mktemp -d); trap 'rm -rf "$TMP"' EXIT

python3 -I - "$FIXTURES" > "$TMP/batch.ndjson" <<'PY'
import json, sys, time
now = time.time()
for record in json.load(open(sys.argv[1])).values():
    record["timestamp"] = now
    print(json.dumps(record, separators=(",", ":")))
PY
RECORDS=$(wc -l < "$TMP/batch.ndjson")
gzip -c "$TMP/batch.ndjson" > "$TMP/batch.ndjson.gz"
BATCH_ID=$(python3 -I -c 'import uuid; print(uuid.uuid4())')

echo "POST $RECORDS records ($(stat -c %s "$TMP/batch.ndjson.gz") bytes gzipped), batch $BATCH_ID"
curl -sS -w '\nHTTP %{http_code} in %{time_total}s\n' -X POST "$URL" \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/x-ndjson" \
  -H "Content-Encoding: gzip" \
  -H "X-Railwatch-Version: 0.9.0" \
  -H "X-Railwatch-Batch-Id: $BATCH_ID" \
  --data-binary @"$TMP/batch.ndjson.gz" | tee "$TMP/ack"
python3 -I -c 'import json,sys; a=json.loads(open(sys.argv[1]).read().split("\nHTTP")[0]); n=int(sys.argv[2]); print("accepted", a.get("accepted"), "of", n); sys.exit(0 if a.get("accepted")==n else 1)' "$TMP/ack" "$RECORDS" \
  || echo "WARNING: accepted != records sent"

# Rows per table in the tenant file the token maps to. The image ships the
# sqlite3 CLI; read-only so we never contend with the writer.
SLUG=$(docker exec -i -e TOKEN="$TOKEN" rw-baseline bin/rails runner 'puts "SLUG #{Environment.authenticate(ENV["TOKEN"]).slug}"' | sed -n 's/^SLUG //p')
DB=/rails/storage/telemetry/$SLUG.sqlite3
echo "tenant $DB (non-empty tables):"
docker exec rw-baseline bash -c '
  db=$1
  for t in $(sqlite3 "file:$db?mode=ro" "select name from sqlite_schema where type=\"table\" and name not like \"sqlite_%\" order by name"); do
    c=$(sqlite3 "file:$db?mode=ro" "select count(*) from \"$t\"")
    [ "$c" != 0 ] && printf "  %-32s %s\n" "$t" "$c"
  done' _ "$DB"
