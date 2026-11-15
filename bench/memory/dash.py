"""Dashboard readers for bench/memory/run.sh: CLIENTS threads GET the main
pages of every environment, round robin, through the trusted-proxy header
(no sign-in form), for DURATION seconds, with PAUSE seconds between a
client's requests. Prints one JSON line: pages, status counts, latency
percentiles (ms), and the same per page.

  python3 -I dash.py BASE SECRET_FILE ENVS_FILE CLIENTS DURATION PAUSE
"""
import base64, hashlib, hmac, json, sys, threading, time, urllib.request

base, secret_file, envs_file, clients, duration, pause = sys.argv[1:7]
clients, duration, pause = int(clients), float(duration), float(pause)
secret = open(secret_file, "rb").read().strip()
envs = [l.split() for l in open(envs_file) if l.strip()]  # app_id env_id slug

PAGES = [
    ("overview", "", "24h"),
    ("overview_7d", "", "7d"),
    ("requests", "/requests", "24h"),
    ("requests_7d", "/requests", "7d"),
    ("jobs", "/jobs", "24h"),
    ("queries", "/queries", "24h"),
    ("queries_7d", "/queries", "7d"),
    ("logs", "/logs", "24h"),
    ("logs_q", "/logs?q=checkout&", "24h"),
    ("cache", "/cache", "24h"),
    ("views", "/views", "24h"),
    ("exceptions", "/exceptions", "7d"),
    ("users", "/users", "7d"),
    ("tenants", "/tenants", "7d"),
    ("processes", "/processes", "1h"),
]


def b64(b):
    return base64.urlsafe_b64encode(b).rstrip(b"=").decode()


def header():
    claims = b64(json.dumps({"email": "bench@example.com", "name": "Bench", "role": "admin", "exp": int(time.time()) + 120}).encode())
    signed = "v1." + claims
    return signed + "." + b64(hmac.new(secret, signed.encode(), hashlib.sha256).digest())


lock = threading.Lock()
samples = {}  # page -> [ms]
status = {}
stop = time.time() + duration


def client(k):
    i = k * 7
    while time.time() < stop:
        name, path, window = PAGES[i % len(PAGES)]
        app, env, _ = envs[(i // len(PAGES) + k) % len(envs)]
        sep = "&" if "?" in path else "?"
        url = f"{base}/apps/{app}/envs/{env}{path}{'' if path.endswith('&') else sep}window={window}"
        req = urllib.request.Request(url, headers={"x-railwatch-proxy-auth": header()})
        t0 = time.time()
        try:
            with urllib.request.urlopen(req, timeout=120) as r:
                r.read()
                code = r.status
        except urllib.error.HTTPError as e:
            code = e.code
        except Exception:
            code = "err"
        ms = (time.time() - t0) * 1000
        with lock:
            status[str(code)] = status.get(str(code), 0) + 1
            if code == 200:
                samples.setdefault(name, []).append(ms)
        i += 1
        if pause:
            time.sleep(pause)


ts = [threading.Thread(target=client, args=(k,)) for k in range(clients)]
[t.start() for t in ts]
[t.join() for t in ts]


def pct(v, p):
    v = sorted(v)
    return round(v[min(len(v) - 1, int(len(v) * p))], 1) if v else None


every = [x for v in samples.values() for x in v]
print(json.dumps({
    "pages": len(every),
    "status": status,
    "p50": pct(every, 0.5), "p90": pct(every, 0.9), "p99": pct(every, 0.99),
    "per_page": {k: {"n": len(v), "p50": pct(v, 0.5), "p90": pct(v, 0.9)} for k, v in sorted(samples.items())},
}))
