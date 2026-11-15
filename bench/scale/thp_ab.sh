#!/usr/bin/env bash
# Does transparent huge pages explain Rust's per-tenant memory floor?
#
#   bench/scale/thp_ab.sh BINARY APPS [SECONDS]
#
# Runs the SAME binary twice on a fresh data dir in /dev/shm, once with THP as
# the host has it and once with MIMALLOC_ALLOW_THP=0 (what #89's
# prctl(PR_SET_THP_DISABLE) amounts to for mimalloc), each under the same
# rw-load profile. Reports the server's Anonymous and AnonHugePages
# (/proc/<pid>/smaps_rollup) and its THP_enabled flag. Same binary, same load:
# the difference is THP.
#
# Run it BETWEEN sweeps, never beside one, with reserve.sh on: the server goes
# into bench-server.slice and rw-load into bench-load.slice (each a transient
# scope, so each gets its own slice's CPUs: 11,23 and 6-10,18-22). It refuses
# to start while anything else is in bench.slice (found by cgroup, not name).
# A side check, not a result row: it measures memory only.
set -euo pipefail
BIN=$1 APPS=$2 SECS=${3:-120}
HERE=$(cd "$(dirname "$0")" && pwd)
SERVER_CPUS=${SERVER_CPUS:-11,23}
LOAD_CPUS=${LOAD_CPUS:-6-10,18-22}
busy=$(find /sys/fs/cgroup/bench.slice -name cgroup.procs -exec cat {} + 2>/dev/null | wc -l)
[ "$busy" = 0 ] || { echo "thp_ab: bench.slice has $busy processes (a sweep is running); run between sweeps" >&2; exit 1; }
PORT=${THP_PORT:-18890}
LOAD=${LOAD_BIN:-$HOME/.cache/rw-scale/bin/rw-load}
root=$(mktemp -d /dev/shm/rw-thp.XXXX)
srv=
cleanup() {
  if [ -n "$srv" ] && kill -0 "$srv" 2>/dev/null; then
    ps -o pid,pgid,comm,args -p "$srv" | tail -1 | cut -c1-80 >&2
    kill "$srv"; wait "$srv" 2>/dev/null || true
  fi
  rm -rf "$root"
}
trap cleanup EXIT

# the server's PID: the one process named after the binary in bench-server.slice
pgrep_cg() {
  local p
  for p in $(find /sys/fs/cgroup/bench.slice/bench-server.slice -name cgroup.procs -exec cat {} + 2>/dev/null); do
    [ "$(readlink -f "/proc/$p/exe" 2>/dev/null)" = "$(readlink -f "$BIN")" ] && { echo "$p"; return 0; }
  done
  return 1
}

run() {  # $1 = label, rest = env for the server
  local label=$1; shift
  local d=$root/$label; mkdir -p "$d"
  for i in $(seq 1 "$APPS"); do RAILWATCH_DATA_DIR=$d/data "$BIN" env create "e$i" | grep -o 'rw_[A-Za-z0-9]*' >> "$d/tokens"; done
  # the server's settings go in a 0600 env file read by `env -S`-free means:
  # a small launcher script in the run dir, so no value (the generated secret
  # included) appears on a command line other processes can read
  ( umask 077
    { echo "export RAILWATCH_DATA_DIR='$d/data' PORT='$PORT' RAILWATCH_TRUST_PROXY=1 RAILWATCH_RATE_LIMIT_ENV=0"
      echo "export RAILWATCH_SECRET_KEY_BASE='$(openssl rand -hex 64)'"
      for kv in "$@"; do echo "export $kv"; done
      echo "exec '$BIN' serve"; } > "$d/launch.sh" )
  sudo systemd-run --scope --quiet --slice=bench-server.slice --uid="$(id -u)" --gid="$(id -g)" -- bash "$d/launch.sh" > "$d/server.log" 2>&1 &
  # the server is the scope's child; take its PID from the scope's own cgroup
  srv=
  for _ in $(seq 1 100); do
    srv=$(pgrep_cg) && [ -n "$srv" ] && break; sleep 0.1
  done
  [ -n "$srv" ] || { echo "thp_ab: server did not start" >&2; cat "$d/server.log" >&2; return 1; }
  for _ in $(seq 1 150); do curl -fsS --max-time 2 -o /dev/null "http://127.0.0.1:$PORT/up" 2>/dev/null && break; sleep 0.2; done
  local idle; idle=$(awk '/^Anonymous:/{print $2}' "/proc/$srv/smaps_rollup")
  sudo systemd-run --scope --quiet --slice=bench-load.slice --uid="$(id -u)" --gid="$(id -g)" -- "$LOAD" --profile "$HERE/profiles.json" --url "http://127.0.0.1:$PORT" --tokens "$d/tokens" \
    --duration "$SECS" --warmup 10 --out "$d/load.json" > "$d/load.txt" 2>&1
  python3 -I - "$label" "$APPS" "$idle" "/proc/$srv/smaps_rollup" "/proc/$srv/status" "$d/load.json" <<'PY'
import json, sys
label, apps, idle, smaps, status, load = sys.argv[1:]
v = {l.split(":")[0]: int(l.split()[1]) for l in open(smaps) if l.split()[0].rstrip(":") in ("Anonymous", "AnonHugePages", "Rss")}
thp = next((l.split()[1] for l in open(status) if l.startswith("THP_enabled")), "?")
r = json.load(open(load))
apps = int(apps); a = v["Anonymous"] / 1024; h = v["AnonHugePages"] / 1024
print(json.dumps({"label": label, "apps": apps, "thp_enabled": thp, "idle_anon_mib": round(int(idle) / 1024, 1),
  "anon_mib": round(a, 1), "anon_hugepages_mib": round(h, 1), "huge_share": round(h / a, 3) if a else None,
  "anon_per_app_mib": round((a - int(idle) / 1024) / apps, 2), "records_ok": r["records_ok"], "not_ok": r["not_ok"]}))
PY
  ps -o pid,pgid,comm,args -p "$srv" | tail -1 | cut -c1-80 >&2
  kill "$srv"; wait "$srv" 2>/dev/null || true; srv=
}

run thp_host
run thp_off MIMALLOC_ALLOW_THP=0
