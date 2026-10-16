"""Stands in for systemd's railwatch.socket: holds one listening socket for the
whole run and starts each `railwatch serve` generation with it as fd 3
(LISTEN_FDS=1, LISTEN_PID=<child>), as systemd does.

  python3 -I holder.py BIN PORT SWAP_EVERY_S SWAPS ORDER LOG_DIR

ORDER:
  restart  - SIGTERM the old generation, wait for it to exit, start the new
             one: what `systemctl restart railwatch` does with a .socket unit
             (the socket stays open throughout; connections queue in the
             kernel backlog while no process is accepting).
  overlap  - start the new generation, wait until it accepts, then SIGTERM
             the old one (research/restart/handoff.py's order; needs a
             supervisor that does this, not plain systemd).

Prints one line per swap: how long the old generation took to drain and
exit, and how long until the new one answered /up.
"""
import os, socket, subprocess, sys, time, urllib.request

bin_, port, every, swaps, order, logdir = sys.argv[1], int(sys.argv[2]), float(sys.argv[3]), int(sys.argv[4]), sys.argv[5], sys.argv[6]
s = socket.socket()
s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.bind(("127.0.0.1", port))
s.listen(4096)


def gen():
    def child():
        os.dup2(s.fileno(), 3)
        os.set_inheritable(3, True)
        os.environ["LISTEN_PID"] = str(os.getpid())

    env = dict(os.environ, LISTEN_FDS="1")
    env.pop("RAILWATCH_BIND", None)
    return subprocess.Popen([bin_, "serve"], env=env, preexec_fn=child, pass_fds=(3,),
                            stdout=open(os.path.join(logdir, "server.log"), "a"), stderr=subprocess.STDOUT)


def up_after(t0):
    while True:
        try:
            urllib.request.urlopen(f"http://127.0.0.1:{port}/up", timeout=1).read()
            return 1000 * (time.monotonic() - t0)
        except Exception:
            time.sleep(0.005)


cur = gen()
up_after(time.monotonic())
print("ready", flush=True)
for i in range(swaps):
    time.sleep(every)
    t = time.monotonic()
    if order == "restart":
        cur.terminate(); cur.wait()
        drained = 1000 * (time.monotonic() - t)
        t2 = time.monotonic()
        cur = gen()
        print(f"swap {i+1}: old exited in {drained:.0f} ms, new answered /up {up_after(t2):.0f} ms after start "
              f"(no process accepting for ~{drained + 1000*(time.monotonic()-t2):.0f} ms; socket open throughout)", flush=True)
    else:
        new = gen()
        ready = up_after(t)
        t2 = time.monotonic(); cur.terminate(); cur.wait()
        print(f"swap {i+1}: new answered /up {ready:.0f} ms after start, old drained and exited in {1000*(time.monotonic()-t2):.0f} ms", flush=True)
        cur = new
sys.stdin.read()
cur.terminate(); cur.wait()
