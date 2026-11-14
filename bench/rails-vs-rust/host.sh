#!/usr/bin/env bash
# Host setup for the Rails vs Rust benchmark (needs sudo). Both servers run,
# one at a time, inside the same cgroup with the same limits, on the same
# dedicated disk.
#
#   bench/rails-vs-rust/host.sh up     # slice + bench disk
#   bench/rails-vs-rust/host.sh down   # remove both
#
# The slice: rvr.slice, MemoryMax 3 GiB (no swap), an exclusive cpuset
# partition of CPUs 6,7 and their SMT siblings 14,15. Containers are started
# with --cpuset-cpus 6-7, so a server gets two physical cores whose siblings
# sit idle, and nothing else on the machine can be scheduled on them.
# (Check your topology with `lscpu -e=CPU,CORE`; set SLICE_CPUS/SERVER_CPUS.)
#
# The disk: this workstation's own NVMe is a shared, 94%-full encrypted
# btrfs where fdatasync p99 was 2.4 s when this was written (other agents'
# builds). Neither server could be measured on it. Instead:
# ext4 on zram (RAM), behind dm-delay with a WRITE_DELAY_MS (default 1 ms)
# delay on every write bio. That turns a flush into ~3 ms (fsync_probe.py)
# with almost no variance: a slow-ish cloud NVMe, the same for both sides.
# Reads are not delayed. DISK_GB (default 12) of zram.
set -euo pipefail
SLICE_CPUS=${SLICE_CPUS:-6,7,14,15}
DELAY=${WRITE_DELAY_MS:-1}
MNT=${BENCH_MNT:-/mnt/rvr}
STATE=${XDG_CACHE_HOME:-$HOME/.cache}/rvr-zram-dev

case "${1:-}" in
up)
  sudo systemctl set-property --runtime rvr.slice MemoryMax=3G MemorySwapMax=0 AllowedCPUs="$SLICE_CPUS"
  sudo systemctl start rvr.slice
  echo "$SLICE_CPUS" | sudo tee /sys/fs/cgroup/rvr.slice/cpuset.cpus.exclusive >/dev/null
  echo root | sudo tee /sys/fs/cgroup/rvr.slice/cpuset.cpus.partition >/dev/null
  echo "rvr.slice: cpus $(cat /sys/fs/cgroup/rvr.slice/cpuset.cpus.effective) partition $(cat /sys/fs/cgroup/rvr.slice/cpuset.cpus.partition) mem $(cat /sys/fs/cgroup/rvr.slice/memory.max)"
  if ! mountpoint -q "$MNT"; then
    sudo modprobe zram dm-delay
    n=$(sudo cat /sys/class/zram-control/hot_add)
    echo lz4 | sudo tee "/sys/block/zram$n/comp_algorithm" >/dev/null
    echo "${DISK_GB:-12}G" | sudo tee "/sys/block/zram$n/disksize" >/dev/null
    s=$(sudo blockdev --getsz "/dev/zram$n")
    # dm-delay: <dev> <offset> <read delay> [<dev> <offset> <write delay> [<dev> <offset> <flush delay>]]
    echo "0 $s delay /dev/zram$n 0 0 /dev/zram$n 0 $DELAY" | sudo dmsetup create rvrdisk
    sudo mkfs.ext4 -q -F /dev/mapper/rvrdisk
    sudo mkdir -p "$MNT"
    sudo mount -o noatime,discard /dev/mapper/rvrdisk "$MNT"
    sudo chown "$(id -u):$(id -g)" "$MNT"
    echo "$n" > "$STATE"
  fi
  echo "$MNT: $(sudo dmsetup table rvrdisk)"
  python3 -I "$(dirname "$0")/fsync_probe.py" "$MNT"
  ;;
down)
  docker rm -f rvr-rust rvr-rails rvr-rails-jobs >/dev/null 2>&1 || true
  if mountpoint -q "$MNT"; then sudo umount "$MNT"; fi
  sudo dmsetup remove rvrdisk 2>/dev/null || true
  if [ -f "$STATE" ]; then echo "$(cat "$STATE")" | sudo tee /sys/class/zram-control/hot_remove >/dev/null; rm -f "$STATE"; fi
  echo member | sudo tee /sys/fs/cgroup/rvr.slice/cpuset.cpus.partition >/dev/null 2>&1 || true
  sudo systemctl stop rvr.slice || true
  ;;
*) echo "usage: host.sh up|down" >&2; exit 2 ;;
esac
