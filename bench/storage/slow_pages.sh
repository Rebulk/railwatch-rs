#!/usr/bin/env bash
# Dashboard page and /api/v1 timings over the SAME rows in both layouts,
# with the identity gate (docs/PERF.md, "Slow dashboard pages").
#
#   bench/storage/slow_pages.sh setup TENANT.sqlite3 WORK
#   bench/storage/slow_pages.sh run WORK LABEL [BIN_DIR=target/release] [WARM=5]
#   bench/storage/slow_pages.sh report WORK LABEL [BASE_LABEL]
#
# setup: WORK/single and WORK/split are two server data dirs with the same
# control DB (one user, one environment, one API token) and secret: the
# tenant file as is, and a `railwatch split` copy of it. NOW is the
# tenant's last execution plus a minute. Cases: every props-diff page case
# (bench/diff/props/make_cases.py) under every window preset (1h, 6h, 24h,
# 7d, 30d), and every /api/v1 environment endpoint (slow_pages.py
# api-cases) under every window it takes. No Rails needed.
#
# run: props-dump and api-dump over both layouts on pinned CPUs (CPUS,
# default 4-7), WARM timed runs per case after one dropped. Writes
# WORK/LABEL-{single,split}-{pages,api}.json and their .timings.json.
# Alternate two binaries' runs (A B A B) for a before/after table.
#
# report: the gate (single == split for LABEL; LABEL == BASE_LABEL per
# layout, byte for byte) and the slowest cases; exits 1 on any difference.
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
HERE=$ROOT/bench/storage
cmd=${1:?usage: slow_pages.sh setup|run|report ...}
shift
case $cmd in
setup)
  SRC=${1:?TENANT.sqlite3} W=${2:?WORK}
  BIN=${BIN_DIR:-$ROOT/target/release}
  [ ! -e "$W" ] || { echo "$W exists" >&2; exit 1; }
  mkdir -p "$W/single"
  # Control DB: one admin, account, app, environment (slug bench-bench-production).
  pw=$(python3 -I -c 'import secrets; print(secrets.token_urlsafe(18))')
  echo "$pw" | RAILWATCH_DATA_DIR=$W/single "$BIN/railwatch" setup --email bench@example.com --password-stdin \
    --account bench --app bench --env production > /dev/null
  SLUG=bench-bench-production
  rm -rf "$W/single/telemetry/$SLUG"
  sqlite3 "$SRC" ".backup $W/single/telemetry/$SLUG.sqlite3"
  python3 -I "$HERE/slow_pages.py" token "$W/single/control.sqlite3" "$W/single/api-tokens.json"
  cp -r "$W/single" "$W/split"
  RAILWATCH_DATA_DIR=$W/split "$BIN/railwatch" split "$SLUG"
  rm -f "$W/split/telemetry/$SLUG.sqlite3.split"*
  T=$W/single/telemetry/$SLUG.sqlite3
  sqlite3 "$T" "SELECT CAST(strftime('%s', MAX(occurred_at)) AS INTEGER) + 60 FROM executions" > "$W/now"
  echo '{"deploys":[]}' > "$W/control.json"
  python3 -I "$ROOT/bench/diff/props/make_cases.py" "$T" "$W/control.json" "$W/pages.base.json" > /dev/null
  python3 -I "$HERE/slow_pages.py" page-cases "$W/pages.base.json" "$(cat "$W/now")" "$W/pages.json"
  python3 -I "$HERE/slow_pages.py" api-cases "$T" "$W/single/control.sqlite3" "$W/api.json"
  echo "$SLUG" > "$W/slug"
  du -sh "$W"
  ;;
run)
  W=${1:?WORK} L=${2:?LABEL} BIN=${3:-$ROOT/target/release} WARM=${4:-5}
  SLUG=$(cat "$W/slug") NOW=$(cat "$W/now")
  for v in ${LAYOUTS:-single split}; do
    T=$W/$v/telemetry/$SLUG.sqlite3; [ "$v" = split ] && T=$W/$v/telemetry/$SLUG
    taskset -c "${CPUS:-4-7}" "$BIN/props-dump" "$T" "$W/pages.json" "$W/control.json" "$NOW" 1 "$W/$L-$v-pages.json" "$WARM" > /dev/null
    taskset -c "${CPUS:-4-7}" "$BIN/api-dump" "$W/$v" "$W/api.json" "$NOW" "$W/$L-$v-api.json" "$WARM" > /dev/null
  done
  ;;
report)
  W=${1:?WORK} L=${2:?LABEL}
  python3 -I "$HERE/slow_pages.py" report "$W" "$L" ${3:+"$3"}
  ;;
*)
  echo "usage: slow_pages.sh setup|run|report ..." >&2
  exit 2
  ;;
esac
