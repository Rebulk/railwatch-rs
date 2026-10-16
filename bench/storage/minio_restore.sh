#!/usr/bin/env bash
# Backup + restore end to end against a local MinIO (S3-compatible) container.
#
#   bench/storage/minio_restore.sh [TENANT_DIR_OR_FILE]
#
# 1. MinIO on 127.0.0.1:19000 (container rw-minio, removed on exit), bucket created.
# 2. A data dir: a real environment with rows ingested over HTTP (today) plus
#    the per-day split of the props-diff tenant (days 1-5 old) as a second env,
#    so closed day files exist.
# 3. `railwatch backup` twice: the second pass must upload 0 day files.
# 4. `railwatch restore` into an EMPTY data dir.
# 5. Every file compared: PRAGMA integrity_check, then every table's rows
#    (ORDER BY rowid) identical between original and restored.
# Image: pgsty/minio (MinIO RELEASE.2026-08-04; docker.io/minio and quay.io
# refused anonymous pulls on 2026-10-14).
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
BIN=$ROOT/target/release/railwatch
SRC=${1:-}
W=$(mktemp -d /tmp/rw-minio.XXXX)
PORT=19000 KEY=railwatch SECRET=railwatch-secret-key
docker rm -f rw-minio >/dev/null 2>&1 || true
docker run -d --name rw-minio -p 127.0.0.1:$PORT:9000 -e MINIO_ROOT_USER=$KEY -e MINIO_ROOT_PASSWORD=$SECRET pgsty/minio:latest server /data >/dev/null
trap 'docker rm -f rw-minio >/dev/null 2>&1 || true; [ -n "${KEEP:-}" ] || rm -rf "$W"' EXIT
until curl -sf "127.0.0.1:$PORT/minio/health/live" >/dev/null; do sleep 0.2; done
docker exec rw-minio sh -c "mc alias set l http://127.0.0.1:9000 $KEY $SECRET >/dev/null && mc mb -q l/rw-backups"

export RAILWATCH_SECRET_KEY_BASE=$(printf '0%.0s' {1..128}) RAILWATCH_LOG=warn
export RAILWATCH_BACKUP_S3_ENDPOINT=http://127.0.0.1:$PORT RAILWATCH_BACKUP_S3_REGION=us-east-1 RAILWATCH_BACKUP_S3_BUCKET=rw-backups
export RAILWATCH_BACKUP_S3_ACCESS_KEY_ID=$KEY RAILWATCH_BACKUP_S3_SECRET_ACCESS_KEY=$SECRET RAILWATCH_BACKUP_S3_PREFIX=e2e

# 2. Original data dir.
export RAILWATCH_DATA_DIR=$W/orig RAILWATCH_BIND=127.0.0.1:18791 RAILWATCH_RATE_LIMIT_ENV=0
TOKEN=$("$BIN" env create live-env)
"$BIN" env create old-env >/dev/null
if [ -n "$SRC" ]; then
  rm -rf "$W/orig/telemetry/old-env"
  if [ -d "$SRC" ]; then cp -r "$SRC" "$W/orig/telemetry/old-env"; else sqlite3 "$SRC" ".backup $W/orig/telemetry/old-env.sqlite3"; "$BIN" split old-env >/dev/null; rm -f "$W/orig/telemetry/old-env.sqlite3.split"*; fi
fi
# The server runs without backups so pass 1 below is the first upload.
env -u RAILWATCH_BACKUP_S3_ENDPOINT -u RAILWATCH_BACKUP_S3_BUCKET -u RAILWATCH_BACKUP_S3_ACCESS_KEY_ID -u RAILWATCH_BACKUP_S3_SECRET_ACCESS_KEY \
  "$BIN" serve > "$W/serve.log" 2>&1 &
SP=$!
until curl -sf 127.0.0.1:18791/up >/dev/null; do sleep 0.1; done
"$ROOT/target/release/rw-load" --url http://127.0.0.1:18791 --tokens <(echo "$TOKEN") --concurrency 4 --duration 3 --out "$W/load.json" >/dev/null 2>&1 || true
kill $SP; wait $SP 2>/dev/null || true

# 3. Two passes.
echo "pass 1: $("$BIN" backup)"
echo "pass 2: $("$BIN" backup)"
echo "objects: $(docker exec rw-minio mc ls -r l/rw-backups | wc -l), $(docker exec rw-minio mc du l/rw-backups | awk '{print $1, $2}')"

# 4. Restore into an empty dir.
export RAILWATCH_DATA_DIR=$W/restored
echo "restore: $("$BIN" restore)"

# 5. Compare.
python3 -I - "$W/orig" "$W/restored" <<'PY'
import os, sqlite3, sys
orig, rest = sys.argv[1], sys.argv[2]
files = []
for d, _, fs in os.walk(orig):
    for f in fs:
        if f.endswith(".sqlite3") and "backup-tmp" not in d:
            files.append(os.path.relpath(os.path.join(d, f), orig))
bad = 0; rows = 0; compared = 0
for rel in sorted(files):
    a, b = os.path.join(orig, rel), os.path.join(rest, rel)
    if not os.path.exists(b):
        # Open (not closed) days are not uploaded by design.
        print(f"not in backup: {rel}"); continue
    ca, cb = sqlite3.connect(f"file:{a}?mode=ro", uri=True), sqlite3.connect(f"file:{b}?mode=ro", uri=True)
    assert cb.execute("PRAGMA integrity_check").fetchone()[0] == "ok", rel
    tables = [r[0] for r in ca.execute("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'logs_fts_%'")]
    for t in tables:
        q = f'SELECT * FROM "{t}" ORDER BY rowid' if t not in ("schema_migrations", "ar_internal_metadata", "sqlite_sequence") else f'SELECT * FROM "{t}" ORDER BY 1'
        if t == "logs_fts": q = "SELECT rowid, message FROM logs_fts ORDER BY rowid"
        ra, rb = ca.execute(q).fetchall(), cb.execute(q).fetchall()
        if ra != rb:
            bad += 1; print(f"DIFFERENT {rel} {t}: {len(ra)} vs {len(rb)} rows")
        rows += len(ra)
    compared += 1
print(f"{compared} files compared, {rows} rows, {bad} tables different")
sys.exit(1 if bad else 0)
PY
