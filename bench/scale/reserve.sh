#!/usr/bin/env bash
# CPU and memory placement for one scale run.
#
#   bench/scale/reserve.sh on|off|status
#
# Inside bench.slice (all --runtime, so a reboot also undoes it):
#   bench-server.slice  SERVER_CPUS: one physical core + its SMT sibling, i.e.
#                       "2 threads", MemoryMax SERVER_MEM (4G), no swap
#   bench-load.slice    LOAD_CPUS: other cores, for rw-load
# Check `lscpu -e=CPU,CORE` and pick siblings: on framework (HX 370) CPU n
# and n+12 share a core, so 11,23 is core 11 and 6-10,18-22 are cores 6-10.
#
# SHARED=1 also confines every other slice to REST (the CPUs outside
# RESERVED), for a box where other people's work runs (rebulk). `off` then
# restores each of them to AllowedCPUs= (unset). Only AllowedCPUs changes;
# no memory limit of another slice, no service or container, is touched.
set -euo pipefail
SERVER_CPUS=${SERVER_CPUS:-11,23}
LOAD_CPUS=${LOAD_CPUS:-6-10,18-22}
SERVER_MEM=${SERVER_MEM:-4G}
SHARED=${SHARED:-0}
RESERVED=${RESERVED:-6-11,18-23}
REST=${REST:-0-5,12-17}

others() {
  [ "$SHARED" = 1 ] || return 0
  for d in /sys/fs/cgroup/*.slice; do
    s=$(basename "$d")
    case "$s" in bench.slice | -.slice) ;; *) echo "$s" ;; esac
  done
  [ -d /sys/fs/cgroup/system.slice/system-space.slice ] && echo system-space.slice
}

status() {
  local bad=0
  for s in $(others) bench.slice bench-server.slice bench-load.slice; do
    v=$(systemctl show -p AllowedCPUs --value "$s" 2>/dev/null || true)
    m=$(systemctl show -p MemoryMax --value "$s" 2>/dev/null || true)
    cg=$(systemctl show -p ControlGroup --value "$s" 2>/dev/null || true)
    eff=$([ -n "$cg" ] && cat "/sys/fs/cgroup$cg/cpuset.cpus.effective" 2>/dev/null || echo -)
    printf '%-24s AllowedCPUs=%-14s MemoryMax=%-12s effective=%s\n' "$s" "${v:-(unset)}" "$m" "$eff"
    case "$s" in bench*) ;; *) [ -n "$v" ] && bad=1 ;; esac
  done
  if [ $bad = 0 ]; then echo "reserve: no slice outside bench.slice restricted"; else echo "reserve: RESTRICTED"; fi
}

case "${1:-}" in
on)
  sudo systemctl set-property --runtime bench.slice AllowedCPUs="$RESERVED"
  sudo systemctl set-property --runtime bench-server.slice AllowedCPUs="$SERVER_CPUS" MemoryMax="$SERVER_MEM" MemorySwapMax=0
  sudo systemctl set-property --runtime bench-load.slice AllowedCPUs="$LOAD_CPUS"
  sudo systemctl start bench-server.slice bench-load.slice
  for s in $(others); do sudo systemctl set-property --runtime "$s" AllowedCPUs="$REST"; done
  status
  ;;
off)
  rc=0
  for s in $(others); do sudo systemctl set-property --runtime "$s" AllowedCPUs= || rc=1; done
  for s in bench-load.slice bench-server.slice bench.slice; do
    sudo systemctl set-property --runtime "$s" AllowedCPUs= MemoryMax=infinity MemorySwapMax=infinity 2>/dev/null || true
    sudo systemctl stop "$s" 2>/dev/null || true
  done
  status
  for s in $(others); do [ -z "$(systemctl show -p AllowedCPUs --value "$s")" ] || { echo "reserve off: $s still restricted" >&2; rc=1; }; done
  exit $rc
  ;;
status) status ;;
*) echo "usage: [SHARED=1] reserve.sh on|off|status" >&2; exit 2 ;;
esac
