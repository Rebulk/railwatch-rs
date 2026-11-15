#!/usr/bin/env bash
# The data disk for the scale runs: ext4, no compression at the filesystem
# level, like production. Two backings (DISK_MODE):
#
#   loop  a sparse image file at $DIR/disk.img on the host's own
#         disk, loop-mounted. On rebulk that is a shared btrfs+zstd NVMe; the
#         image is NOCOW (chattr +C), which also turns btrfs compression off
#         for it. Its fsync latency follows whatever else the host is doing.
#   ram   (default) ext4 on zram (RAM, ZRAM_GB, default 10 GB cap) behind dm-delay with
#         WRITE_DELAY_MS (1) on every write bio: an fdatasync of ~3 ms with
#         almost no variance, a steady "modest cloud NVMe". The same disk
#         Track C used (bench/rails-vs-rust/host.sh). Reads are not delayed.
#         zram compresses the pages it holds (lz4), so the RAM it uses is
#         below the bytes ext4 reports; disk-per-app figures are ext4 bytes.
#
#   bench/scale/disk.sh up     # mounted at $DIR/mnt
#   bench/scale/disk.sh down   # unmount, remove image / dm device / zram
#   bench/scale/disk.sh usage  # bytes it really occupies on the host (file or RAM)
set -euo pipefail
DIR=${SCALE_DIR:-/var/tmp/rw-scale}
IMG=$DIR/disk.img
MNT=$DIR/mnt
MODE=${DISK_MODE:-ram}
DM=rwscale
STATE=$DIR/zram-dev   # zram index, for down

case "${1:-}" in
up)
  sudo mkdir -p "$DIR" "$MNT"
  if ! mountpoint -q "$MNT"; then
    if [ "$MODE" = ram ]; then
      sudo modprobe zram
      sudo modprobe dm-delay
      n=$(sudo cat /sys/class/zram-control/hot_add)
      echo "$n" | sudo tee "$STATE" >/dev/null
      echo lz4 | sudo tee "/sys/block/zram$n/comp_algorithm" >/dev/null
      echo "${ZRAM_GB:-10}G" | sudo tee "/sys/block/zram$n/disksize" >/dev/null
      s=$(sudo blockdev --getsz "/dev/zram$n")
      d=${WRITE_DELAY_MS:-1}
      # dm-delay: <dev> <offset> <read delay> <dev> <offset> <write delay>
      echo "0 $s delay /dev/zram$n 0 0 /dev/zram$n 0 $d" | sudo dmsetup create "$DM"
      sudo mkfs.ext4 -q -F -E lazy_itable_init=0,lazy_journal_init=0 "/dev/mapper/$DM"
      sudo mount -o noatime "/dev/mapper/$DM" "$MNT"
    else
      sudo rm -f "$IMG"
      sudo touch "$IMG"
      sudo chattr +C "$IMG"
      sudo truncate -s "${DISK_GB:-40}G" "$IMG"
      sudo mkfs.ext4 -q -F -E lazy_itable_init=0,lazy_journal_init=0 "$IMG"
      sudo mount -o loop,noatime "$IMG" "$MNT"
    fi
    sudo chown "$(id -u):$(id -g)" "$MNT"
  fi
  findmnt -no SOURCE,FSTYPE,OPTIONS "$MNT"
  [ "$(findmnt -no FSTYPE "$MNT")" = ext4 ] || { echo "disk: $MNT is not the bench ext4" >&2; exit 1; }
  if [ "$MODE" = ram ]; then sudo dmsetup table "$DM"; else lsattr "$IMG"; fi
  ;;
down)
  if mountpoint -q "$MNT"; then sudo umount "$MNT"; fi
  for l in $(losetup -j "$IMG" -O NAME -n 2>/dev/null); do sudo losetup -d "$l"; done
  if sudo dmsetup info "$DM" >/dev/null 2>&1; then sudo dmsetup remove "$DM"; fi
  if [ -f "$STATE" ]; then sudo cat "$STATE" | sudo tee /sys/class/zram-control/hot_remove >/dev/null; fi
  sudo rm -rf "$DIR"
  [ ! -e "$DIR" ] && echo "disk: $DIR removed"
  ;;
usage)
  if [ -f "$STATE" ]; then
    # RAM zram really holds (compressed data + metadata): mm_stat field 3
    awk '{print $3}' "/sys/block/zram$(sudo cat "$STATE")/mm_stat"
  else
    du -B1 "$IMG" | cut -f1
  fi
  ;;
*) echo "usage: [DISK_MODE=loop|ram] disk.sh up|down|usage" >&2; exit 2 ;;
esac
