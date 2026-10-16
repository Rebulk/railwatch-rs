"""Zero-gap restart: one process holds the listening socket for the whole run and
passes it to each server generation as fd 3 (LISTEN_FDS=1). Swap order: start the
new generation on the same socket, wait 300 ms, then SIGTERM the old one, which
stops accepting and drains in flight. The kernel queue never closes, so no
connection is refused. Both generations briefly write the same tenant file; SQLite's
lock (busy_timeout 5 s, BEGIN IMMEDIATE) serialises them.

  python3 -I handoff.py PORT SWAP_AT_S SWAPS
"""
import os, socket, subprocess, sys, time
port, swap_at, swaps = int(sys.argv[1]), float(sys.argv[2]), int(sys.argv[3])
BIN = os.environ.get("RW_FDSERVE", os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "target", "release", "rw-fdserve"))
s = socket.socket(); s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.bind(("127.0.0.1", port)); s.listen(1024)
def gen():
    def child():
        os.dup2(s.fileno(), 3); os.set_inheritable(3, True)
    env = dict(os.environ, LISTEN_FDS="1")
    return subprocess.Popen(["taskset", "-c", "8-11", BIN], env=env, preexec_fn=child, pass_fds=(3,),
                            stdout=open(os.environ["RAILWATCH_DATA_DIR"] + "/server.log", "a"), stderr=subprocess.STDOUT)
cur = gen()
print("ready", flush=True)
for i in range(swaps):
    time.sleep(swap_at)
    new = gen(); time.sleep(0.3)
    t = time.monotonic(); cur.terminate(); cur.wait()
    print(f"swap {i+1}: old generation drained and exited in {1000*(time.monotonic()-t):.0f} ms", flush=True)
    cur = new
sys.stdin.read()  # hold until the driver closes stdin
cur.terminate(); cur.wait()
