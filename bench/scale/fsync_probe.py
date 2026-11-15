"""fdatasync latency on the bench disk, once a second, for the length of a
step: tells disk stalls (the shared host disk under the loop image) apart from
server stalls. Writes 4 KiB to DIR/.fsync-probe and fdatasyncs it; prints one
"<unix time> <latency ms>" per line until killed.

    python3 -I fsync_probe.py DIR > probe.txt
"""
import os, sys, time
path = os.path.join(sys.argv[1], ".fsync-probe")
fd = os.open(path, os.O_WRONLY | os.O_CREAT, 0o644)
buf = os.urandom(4096)
while True:
    wall, t = time.time(), time.monotonic()
    os.pwrite(fd, buf, 0)
    os.fdatasync(fd)
    print(round(wall, 2), round((time.monotonic() - t) * 1000, 2), flush=True)
    time.sleep(1)
