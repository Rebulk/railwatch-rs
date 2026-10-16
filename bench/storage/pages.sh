#!/usr/bin/env bash
# Dashboard page timings and props on the SAME rows in both layouts.
#
#   bench/storage/pages.sh SINGLE_TENANT.sqlite3 [warm_runs=5]
#
# Copies the file to /dev/shm, splits the copy with `railwatch split`,
# generates the props-diff cases from it (make_cases.py), and runs
# props-dump over the single file and over the per-day directory on the
# same pinned CPUs. Prints the props comparison (must be all identical)
# and the per-case timing ratio. The 2026-10-14 numbers in docs/STORAGE.md
# come from a 5-day, 291k-row tenant made by
# `rw-load --spread-days 5` against main.
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
SRC=${1:?usage: pages.sh TENANT.sqlite3 [warm]} WARM=${2:-5}
P=$(mktemp -d /dev/shm/rw-pages.XXXX)
trap '[ -n "${KEEP:-}" ] || rm -rf "$P"' EXIT
mkdir -p "$P/telemetry"
sqlite3 "$SRC" ".backup $P/telemetry/t.sqlite3"
RAILWATCH_DATA_DIR=$P "$ROOT/target/release/railwatch" split t
mv "$P/telemetry/t.sqlite3.split" "$P/single.sqlite3"
echo '{"deploys":[]}' > "$P/control.json"
python3 -I "$ROOT/bench/diff/props/make_cases.py" "$P/single.sqlite3" "$P/control.json" "$P/cases.json" > /dev/null
NOW=$(date +%s)
python3 -I - "$P/cases.json" "$NOW" <<'PY'
import json, sys, datetime
p, now = sys.argv[1], int(sys.argv[2])
iso = lambda t: datetime.datetime.fromtimestamp(t, datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
cases = json.load(open(p))
for c in cases:
    for k, v in list(c["params"].items()):
        c["params"][k] = v.replace("CUSTOM_FROM", iso(now - 8 * 3600)).replace("CUSTOM_TO", iso(now - 2 * 3600))
json.dump(cases, open(p, "w"))
PY
for v in single split; do
  T=$P/single.sqlite3; [ $v = split ] && T=$P/telemetry/t
  taskset -c "${CPUS:-4-7}" "$ROOT/target/release/props-dump" "$T" "$P/cases.json" "$P/control.json" "$NOW" 1 "$P/$v.json" "$WARM" > /dev/null
done
python3 -I "$ROOT/bench/diff/props/compare.py" "$P/single.json" "$P/split.json" | tail -1
python3 -I - "$P/single.json.timings.json" "$P/split.json.timings.json" <<'PY'
import json, sys, statistics as st
a, b = json.load(open(sys.argv[1])), json.load(open(sys.argv[2]))
med = lambda v: st.median(v) if isinstance(v, list) else v
rows = [(k, med(a[k]), med(b[k])) for k in a if k in b]
print(f"sum of per-case medians: single {sum(r[1] for r in rows):.1f} ms, per-day {sum(r[2] for r in rows):.1f} ms")
ratios = sorted(r[2] / max(r[1], 0.01) for r in rows)
print(f"per-day/single per case: median {st.median(ratios):.2f}x, p90 {ratios[int(len(ratios) * 0.9)]:.2f}x")
rows.sort(key=lambda r: -r[2])
for k, x, y in rows[:8]:
    print(f"  {k:36s} single {x:7.1f} ms  per-day {y:7.1f} ms  {y / max(x, 0.01):5.2f}x")
PY
