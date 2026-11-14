#!/usr/bin/env bash
# The data disk for the scale runs: ext4, no compression, like production,
# on a loop-mounted image file. rebulk's root is btrfs+zstd; the image is
# created NOCOW (chattr +C), which on btrfs also turns compression off for it,
# so SQLite's pages are neither compressed nor copy-on-written underneath ext4.
#
#   bench/scale/disk.sh up     # DISK_GB (40) sparse image at $DIR/disk.img, mounted at $DIR/mnt
#   bench/scale/disk.sh down   # unmount, detach, delete the image
#   bench/scale/disk.sh usage  # bytes the image really occupies on the host
set -euo pipefail
DIR=${SCALE_DIR:-/var/tmp/rw-scale}
IMG=$DIR/disk.img
MNT=$DIR/mnt
case "${1:-}" in
up)
  sudo mkdir -p "$DIR" "$MNT"
  if ! mountpoint -q "$MNT"; then
    sudo rm -f "$IMG"
    sudo touch "$IMG"
    sudo chattr +C "$IMG"
    sudo truncate -s "${DISK_GB:-40}G" "$IMG"
    sudo mkfs.ext4 -q -F -E lazy_itable_init=0,lazy_journal_init=0 "$IMG"
    sudo mount -o loop,noatime "$IMG" "$MNT"
    sudo chown "$(id -u):$(id -g)" "$MNT"
  fi
  findmnt -no SOURCE,FSTYPE,OPTIONS "$MNT"
  lsattr "$IMG"
  ;;
down)
  if mountpoint -q "$MNT"; then sudo umount "$MNT"; fi
  for l in $(losetup -j "$IMG" -O NAME -n 2>/dev/null); do sudo losetup -d "$l"; done
  sudo rm -rf "$DIR"
  [ ! -e "$DIR" ] && echo "disk: $DIR removed"
  ;;
usage) du -B1 "$IMG" | cut -f1 ;;
*) echo "usage: disk.sh up|down|usage" >&2; exit 2 ;;
esac
