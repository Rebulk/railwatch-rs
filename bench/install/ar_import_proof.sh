#!/usr/bin/env bash
# Proves import-rails carries Rails-encrypted integration secrets over.
#
#   bench/install/ar_import_proof.sh [path/to/railwatch]
#
# 1. In rw-baseline (Rails 8.1, its own configured Active Record encryption
#    keys), ar_import_proof.rb saves three integrations in a scratch account,
#    snapshots the primary DB, writes what Rails' Integration#settings returns
#    for each, then deletes the scratch account again.
# 2. `railwatch import-rails` imports the snapshot with those keys.
# 3. What the Rust app reads back (`railwatch integrations --show-secrets`,
#    through the same Integration loader delivery uses) is compared to
#    Rails' JSON: identical, or the script fails.
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
BIN=${1:-$ROOT/target/release/railwatch}
W=$(mktemp -d /tmp/rw-ar-proof.XXXXXX)
trap 'rm -rf "$W"' EXIT

docker cp "$HERE/ar_import_proof.rb" rw-baseline:/tmp/ar_import_proof.rb
docker exec rw-baseline bin/rails runner /tmp/ar_import_proof.rb 2>/dev/null > "$W/rails.out"
key=$(sed -n 's/^AR_PRIMARY_KEY=//p' "$W/rails.out")
salt=$(sed -n 's/^AR_SALT=//p' "$W/rails.out")
snap=$(sed -n 's/^SNAPSHOT=//p' "$W/rails.out")
docker cp "rw-baseline:$snap" "$W/production.sqlite3"
docker cp "rw-baseline:$snap.expected.json" "$W/expected.json"
docker exec rw-baseline rm -f "$snap" "$snap.expected.json"
grep '^RAILS_ROW' "$W/rails.out" | cut -c1-90
mkdir -p "$W/telemetry"

echo "== import without keys"
RAILWATCH_DATA_DIR=$W/nokeys "$BIN" import-rails --primary "$W/production.sqlite3" --telemetry-dir "$W/telemetry" | grep -E 'integrations|UNREADABLE|secrets'
echo "== import with --ar-primary-key/--ar-key-derivation-salt"
RAILWATCH_DATA_DIR=$W/data "$BIN" import-rails --primary "$W/production.sqlite3" --telemetry-dir "$W/telemetry" \
  --ar-primary-key "$key" --ar-key-derivation-salt "$salt" | grep -E 'integrations|UNREADABLE|secrets'
echo "== the Rust app's view of each integration vs Rails' Integration#settings"
RAILWATCH_DATA_DIR=$W/data "$BIN" integrations --show-secrets > "$W/actual.jsonl"
python3 -I - "$W/expected.json" "$W/actual.jsonl" <<'EOF'
import json, sys
exp = {i: s for i, s in json.load(open(sys.argv[1]))}
act = {r["id"]: {"settings": r["settings"], "error": r["error"]} for r in map(json.loads, open(sys.argv[2]))}
ok = True
for i, s in exp.items():
    a = act.get(i)
    same = a == {"settings": s, "error": None}
    ok &= same
    print(f"  integration {i}: {'identical' if same else 'DIFFERENT'} ({len(s)} keys)" + ("" if same else f"\n    rails={s}\n    rust={a}"))
sys.exit(0 if ok else 1)
EOF
echo "PASS: every Rails-encrypted secret decrypts to what Rails reads"
