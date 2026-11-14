"""Dashboard read latency over real HTTP, as a browser's Inertia navigation
makes it: GET with X-Inertia, signed in with a real session cookie.

    python3 -I pages.py BASE_URL EMAIL PASSWORD ENV_PATH OUT.json [warm_runs=10]

BASE_URL e.g. http://127.0.0.1:8781; ENV_PATH /apps/<id>/envs/<id>. Signs in
through the app's own /sign_in form, then for each page (window=24h):
- cold: the first request to that page after the server started (the
  caller restarts the server and drops the page cache before calling this
  with WARM=0);
- warm: WARM more requests, median and spread reported.
Every response must be 200 with a JSON page object; the script fails
otherwise, so a redirect to /sign_in can never be timed as a fast page.
"""
import http.client, json, re, sys, time, urllib.parse

base, email, password, env_path, out = sys.argv[1:6]
warm = int(sys.argv[6]) if len(sys.argv) > 6 else 10
PAGES = [
    ("overview", ""),
    ("requests", "/requests"),
    ("jobs", "/jobs"),
    ("queries", "/queries"),
    ("exceptions", "/exceptions"),
]
WINDOW = "24h"
UA = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0 Safari/537.36"
u = urllib.parse.urlparse(base)
conn = http.client.HTTPConnection(u.hostname, u.port, timeout=300)  # one keep-alive connection, like a browser tab
cookies = {}  # by hand: Rails marks its cookies Secure (force_ssl), which a cookie jar would not send over http


def req(path, data=None, headers=None, method=None):
    h = {"User-Agent": UA, "Accept": "text/html, application/xhtml+xml", "Host": f"{u.hostname}:{u.port}"}
    if cookies:
        h["Cookie"] = "; ".join(f"{k}={v}" for k, v in cookies.items())
    h.update(headers or {})
    conn.request(method or ("POST" if data is not None else "GET"), path, body=data, headers=h)
    r = conn.getresponse()
    body = r.read()
    for k, v in r.getheaders():
        if k.lower() == "set-cookie":
            name, _, rest = v.partition("=")
            cookies[name.strip()] = rest.split(";", 1)[0]
    return r.status, body


# Sign in through the app's own form: GET for the CSRF token (meta tag and
# XSRF-TOKEN cookie), POST email + password, expect a redirect and a
# session_token cookie.
status, html = req("/sign_in")
m = re.search(rb'name="csrf-token" content="([^"]+)"', html)
token = m.group(1).decode() if m else None
xsrf = urllib.parse.unquote(cookies["XSRF-TOKEN"]) if "XSRF-TOKEN" in cookies else None
form = {"email": email, "password": password}
if token:
    form["authenticity_token"] = token
# Origin as a browser sends it in production: Rails sets assume_ssl (it sits
# behind TLS), so it expects https://; ORIGIN overrides.
import os
hdr = {"Content-Type": "application/x-www-form-urlencoded", "Origin": os.environ.get("ORIGIN", base)}
if xsrf:
    hdr["X-XSRF-TOKEN"] = xsrf
if token:
    hdr["X-CSRF-Token"] = token
status, _ = req("/sign_in", urllib.parse.urlencode(form).encode(), hdr)
if status not in (302, 303) or "session_token" not in cookies:
    sys.exit(f"sign-in failed: HTTP {status}, cookies {sorted(cookies)}")

# The Inertia asset version: the first full HTML page carries it.
status, html = req(env_path)
if status != 200:
    sys.exit(f"{env_path}: HTTP {status}")
html = html.decode()
m = re.search(r'"version":"([^"]*)"', html) or re.search(r'&quot;version&quot;:&quot;([^&]*)&quot;', html)
version = m.group(1) if m else ""

results = {}
for name, suffix in PAGES:
    path = f"{env_path}{suffix}?window={WINDOW}"
    samples = []
    for i in range(warm + 1):
        t = time.perf_counter()
        status, body = req(path, headers={"X-Inertia": "true", "X-Inertia-Version": version, "X-Requested-With": "XMLHttpRequest"})
        ms = (time.perf_counter() - t) * 1000
        if status != 200:
            sys.exit(f"{path}: HTTP {status}")
        page = json.loads(body)
        if "component" not in page:
            sys.exit(f"{path}: not an Inertia page")
        samples.append(ms)
    results[name] = {"component": page["component"], "bytes": len(body), "cold_ms": samples[0], "warm_ms": sorted(samples[1:])}
    print(f"{name:11} cold {samples[0]:8.1f} ms   warm median {sorted(samples[1:])[len(samples[1:]) // 2] if warm else 0:8.1f} ms  ({len(body)} bytes)", flush=True)
json.dump(results, open(out, "w"), indent=1)
