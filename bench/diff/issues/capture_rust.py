"""Signs in to the Rust server and GETs each path the way capture_rails.rb
does: Inertia pages as {component, props}, merge_candidates as {json}.

  python3 -I capture_rust.py BASE EMAIL PASSWORD PATHS.json OUT.json
"""
import http.cookiejar, json, re, sys, urllib.error, urllib.parse, urllib.request

base, email, password, paths_file, out_file = sys.argv[1:6]
cj = http.cookiejar.CookieJar()


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *a, **k):
        return None


op = urllib.request.build_opener(urllib.request.HTTPCookieProcessor(cj), NoRedirect)


def req(method, path, data=None, headers=None):
    r = urllib.request.Request(base + path, method=method, data=json.dumps(data).encode() if data is not None else None)
    r.add_header("Origin", base)
    for k, v in (headers or {}).items():
        r.add_header(k, v)
    if data is not None:
        r.add_header("Content-Type", "application/json")
        r.add_header("X-XSRF-TOKEN", next((urllib.parse.unquote(c.value) for c in cj if c.name == "XSRF-TOKEN"), ""))
    try:
        resp = op.open(r)
    except urllib.error.HTTPError as e:
        resp = e
    return resp.status, resp.read().decode("utf-8", "replace")


_, html = req("GET", "/sign_in")
version = json.loads(re.search(r'<script data-page="app" type="application/json">(.*?)</script>', html, re.S).group(1))["version"]
status, _ = req("POST", "/sign_in", {"email": email, "password": password})
assert status == 302, f"sign-in failed: {status}"
out = {}
for path in json.load(open(paths_file)):
    if "merge_candidates" in path:
        status, body = req("GET", path, headers={"Accept": "application/json"})
        out[path] = {"json": json.loads(body)} if status == 200 else {"status": status, "body": body[:300]}
    else:
        status, body = req("GET", path, headers={"X-Inertia": "true", "X-Inertia-Version": version, "Accept": "text/html, application/xhtml+xml"})
        if status == 200:
            page = json.loads(body)
            out[path] = {"component": page["component"], "props": page["props"]}
        else:
            out[path] = {"status": status, "body": body[:300]}
json.dump(out, open(out_file, "w"))
print(f"captured {len(out)} from Rust ({sum('status' in v for v in out.values())} non-200)")
