#!/usr/bin/env bash
# Reserve CPUs for one scale run on a shared box, and give them back.
#
#   bench/scale/reserve.sh on       # bench.slice gets RESERVED, everyone else REST
#   bench/scale/reserve.sh off      # every slice touched back to AllowedCPUs= (unset)
#   bench/scale/reserve.sh status   # AllowedCPUs + effective cpus of every slice
#
# Everything is `systemctl set-property --runtime`, so a reboot also undoes it.
# Only AllowedCPUs is changed: no memory limit of another slice, no service or
# container is stopped or restarted. Work already running on the reserved CPUs
# is moved off them by the kernel when its slice's cpuset shrinks.
#
# Inside bench.slice:
#   bench-server.slice  SERVER_CPUS (default 14,30: one physical core + its SMT
#                       sibling, i.e. "2 vCPUs"), MemoryMax 4G, no swap
#   bench-load.slice    LOAD_CPUS (default the other reserved CPUs)
# (Topology on rebulk: CPU n and n+16 are SMT siblings; check `lscpu -e=CPU,CORE`.)
set -euo pipefail
RESERVED=${RESERVED:-12-15,28-31}
REST=${REST:-0-11,16-27}
SERVER_CPUS=${SERVER_CPUS:-14,30}
LOAD_CPUS=${LOAD_CPUS:-12,13,15,28,29,31}
SERVER_MEM=${SERVER_MEM:-4G}

# Every top-level slice (the cgroup root's *.slice children) plus the nested
# system-space.slice (T3 Spaces), named explicitly as the brief asks.
others() {
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
    cg=$(systemctl show -p ControlGroup --value "$s" 2>/dev/null || true)
    eff=$([ -n "$cg" ] && cat "/sys/fs/cgroup$cg/cpuset.cpus.effective" 2>/dev/null || echo -)
    printf '%-28s AllowedCPUs=%-14s effective=%s\n' "$s" "${v:-(unset)}" "$eff"
    case "$s" in bench*) ;; *) [ -n "$v" ] && bad=1 ;; esac
  done
  if [ $bad = 0 ]; then echo "reserve: all other slices unrestricted"; else echo "reserve: RESTRICTED (on)"; fi
}

case "${1:-}" in
on)
  # bench.slice first, so the reserved CPUs have an owner before anyone shrinks.
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
    sudo systemctl set-property --runtime "$s" AllowedCPUs= || true
    sudo systemctl stop "$s" 2>/dev/null || true
  done
  status
  # verify: nothing outside bench may stay restricted
  for s in $(others); do [ -z "$(systemctl show -p AllowedCPUs --value "$s")" ] || { echo "reserve off: $s still restricted" >&2; rc=1; }; done
  exit $rc
  ;;
status) status ;;
*) echo "usage: reserve.sh on|off|status" >&2; exit 2 ;;
esac
