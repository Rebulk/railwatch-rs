"""fdatasync latency on a directory's filesystem: 4 KiB pwrite + fdatasync, 200 times.

    python3 -I bench/rails-vs-rust/fsync_probe.py /mnt/rvr
"""
import os, sys, time
d = sys.argv[1] if len(sys.argv) > 1 else "."
p = os.path.join(d, ".fsync.probe")
fd = os.open(p, os.O_WRONLY | os.O_CREAT, 0o600)
lat = []
for i in range(200):
    os.pwrite(fd, b"x" * 4096, i * 4096)
    t = time.perf_counter()
    os.fdatasync(fd)
    lat.append((time.perf_counter() - t) * 1000)
os.close(fd)
os.unlink(p)
lat.sort()
print("fdatasync %s: p50 %.2f ms  p90 %.2f  p99 %.2f  max %.2f" % (d, lat[100], lat[180], lat[198], lat[-1]))
