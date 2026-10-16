#!/usr/bin/env bash
# Remove the baseline containers and their storage volume (all tenants, all
# tokens). The image and the /tmp/rw-baseline-wt worktree are kept; pass
# --image to remove the image too.
set -euo pipefail
docker rm -f rw-baseline rw-baseline-jobs 2>/dev/null || true
docker volume rm rw-baseline-storage rw-baseline-storage-ram 2>/dev/null || true
rm -f "$(cd "$(dirname "$0")" && pwd)"/tokens-*.txt
if [ "${1:-}" = "--image" ]; then
  docker image rm railwatch-cloud-baseline:464f0fd 2>/dev/null || true
fi
