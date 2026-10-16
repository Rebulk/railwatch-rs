#!/usr/bin/env python3
"""The cases of cases.json through a running railwatch-rs (RAILWATCH_GOOGLE_ISSUER
= the mock), printing the same JSON lines as rails_flows.rb.

usage: rust_flows.py <server base> <mock base> <data dir> cases.json
Each case starts from an empty control DB (users, accounts, sessions); the
case's setup users are inserted directly.
"""
import http.cookiejar, json, re, sqlite3, sys, urllib.error, urllib.parse, urllib.request

BASE, MOCK, DATA, CASES = sys.argv[1], sys.argv[2], sys.argv[3], json.load(open(sys.argv[4]))
DB = f"{DATA}/control.sqlite3"
# Never checked: these users only ever sign in with Google here.
DIGEST = "$2b$04$C6ZyUiXu0bE8lj0dvqzYbOlUEpk6r3vSCVMCPMLDu6K5HIp4XsS7K"


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *a, **k):
        return None


class Browser:
    def __init__(self, jar=None):
        self.jar = jar or http.cookiejar.CookieJar()
        self.op = urllib.request.build_opener(urllib.request.HTTPCookieProcessor(self.jar), NoRedirect)

    def req(self, url, data=None):
        headers = {"Origin": BASE} if data is not None else {}
        r = urllib.request.Request(url, data=data.encode() if data is not None else None, headers=headers)
        try:
            res = self.op.open(r)
            return res.status, res.headers.get("Location"), res.read().decode()
        except urllib.error.HTTPError as e:
            return e.code, e.headers.get("Location"), e.read().decode()

    def csrf(self):
        _, _, body = self.req(f"{BASE}/sign_in")
        return re.search(r'name="csrf-token" content="([^"]+)"', body).group(1)

    def click(self):
        s, loc, _ = self.req(f"{BASE}/auth/google_oauth2", "authenticity_token=" + urllib.parse.quote(self.csrf()))
        if s != 302 or not loc.startswith(MOCK):
            return s, loc
        s, loc, _ = self.req(loc)
        return s, loc

    def land(self, path):
        """GET path, then follow our redirects to the page the user sees."""
        s, loc, body = self.req(BASE + path)
        first, final, hops = path_of(loc), path.split("?")[0], 0
        while s == 302 and hops < 5:
            final = path_of(loc).split("?")[0]
            s, loc, body = self.req(BASE + path_of(loc))
            hops += 1
        m = re.search(r'<script data-page="app" type="application/json">(.*?)</script>', body, re.S)
        flash = json.loads(m.group(1).replace("\\/", "/"))["props"]["flash"] if m else {}
        return {"first": first, "final": final, "flash": {k: v for k, v in flash.items() if v is not None}}


def path_of(url):
    if not url:
        return None
    u = urllib.parse.urlsplit(url)
    return u.path + (f"?{u.query}" if u.query else "")


def mock_next(n):
    urllib.request.urlopen(urllib.request.Request(f"{MOCK}/next", json.dumps(n).encode(), {"Content-Type": "application/json"}))


def db():
    return sqlite3.connect(DB, timeout=10)


def setup(users):
    with db() as c:
        for u in users or []:
            cur = c.execute("INSERT INTO users (name, email, password_digest, verified, created_at, updated_at) VALUES (?, ?, ?, ?, datetime('now'), datetime('now'))",
                            (u["email"], u["email"], DIGEST, u["verified"]))
            if u.get("account"):
                uid = cur.lastrowid
                a = c.execute("INSERT INTO accounts (name, slug, plan, created_at, updated_at) VALUES (?, ?, 'self-hosted', datetime('now'), datetime('now'))",
                              (u["email"] + " co", re.sub(r"[^a-z0-9]+", "-", u["email"]))).lastrowid
                c.execute("INSERT INTO memberships (account_id, user_id, role, created_at, updated_at) VALUES (?, ?, 'owner', datetime('now'), datetime('now'))", (a, uid))
                c.execute("INSERT INTO applications (account_id, name, slug, issue_prefix, created_at, updated_at) VALUES (?, 'Shop', 'shop', 'SHOP', datetime('now'), datetime('now'))", (a,))


def users():
    with db() as c:
        return [[e, p, u, bool(v)] for e, p, u, v in c.execute("SELECT email, provider, uid, verified FROM users ORDER BY id")]


def reset():
    with db() as c:
        for t in ["sessions", "applications", "memberships", "invitations", "accounts", "users"]:
            c.execute(f"DELETE FROM {t}")


for case in CASES:
    reset()
    setup(case.get("setup"))
    b = Browser()
    kind = case["kind"]
    if kind == "flow":
        mock_next(case["next"])
        s, back = b.click()
        out = b.land(path_of(back)) if s == 302 else {"provider_status": s}
    elif kind == "state_mismatch":
        mock_next(case["next"])
        _, back = b.click()
        out = b.land(path_of(back).replace("state=", "state=0"))
    elif kind == "replay":
        mock_next(case["next"])
        _, loc, _ = b.req(f"{BASE}/auth/google_oauth2", "authenticity_token=" + urllib.parse.quote(b.csrf()))
        saved = http.cookiejar.CookieJar()
        for c in b.jar:
            if c.name != "session_token":
                saved.set_cookie(c)
        _, back, _ = b.req(loc)
        genuine = b.land(path_of(back))
        out = {"genuine": genuine, "replay": Browser(saved).land(path_of(back))}
    elif kind == "access_denied":
        mock_next(case["next"])
        _, back = b.click()
        st = re.search(r"state=([^&]+)", back).group(1)
        out = b.land(f"/auth/google_oauth2/callback?error=access_denied&state={st}")
    elif kind == "bad_csrf":
        b.csrf()
        s, loc, _ = b.req(f"{BASE}/auth/google_oauth2", "authenticity_token=forged")
        out = dict(b.land(path_of(loc)), request_phase=path_of(loc)) if loc else {"status": s}
    print(json.dumps({"case": case["name"], "out": out, "users": users()}))
