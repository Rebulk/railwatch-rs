#!/usr/bin/env python3
"""evict.py PATH...: drop every file under each PATH (a directory or a
file) from the page cache, without root: fdatasync, then
posix_fadvise(DONTNEED) (works on clean pages of files the caller can
open). Prints how many pages stay resident, from mincore via `fincore` when
it is installed, so a run can check it was cold. Name a loop device's
backing image too (EVICT_ALSO in dashboard.sh): its pages are a second
cache under the mounted file system's.
"""
import os, subprocess, sys

files = []
for d in sys.argv[1:]:
    if os.path.isfile(d):
        files.append(d)
    for root, _, names in os.walk(d):
        files.extend(os.path.join(root, n) for n in names)
for f in files:
    try:
        fd = os.open(f, os.O_RDWR)
    except OSError:
        continue
    try:
        os.fdatasync(fd)
        os.posix_fadvise(fd, 0, 0, os.POSIX_FADV_DONTNEED)
    finally:
        os.close(fd)
try:
    out = subprocess.run(["fincore", "-b", "-n", "-o", "RES", *files], capture_output=True, text=True).stdout
    res = sum(int(x) for x in out.split() if x.isdigit())
    print(f"evicted {len(files)} files, {res / 1048576:.1f} MiB still resident")
except FileNotFoundError:
    print(f"evicted {len(files)} files (fincore not installed: not checked)")
