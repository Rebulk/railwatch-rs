#!/usr/bin/env python3
"""Batches for the props-diff tenant: a realistic mix of every record type
the dashboard pages read, spread over --spread-days ending at --now, so the
1h / 24h / 7d windows and both raw (sub-hour) and rollup series have data.

    gen_tenant.py FIXTURES OUT_DIR --now EPOCH [--spread-days 9] [--executions 6000] [--seed 11]
                  [--shapes 60] [--tenants 0] [--uniform] [--by-time] [--few-logs]

Writes OUT_DIR/NNNN.ndjson.gz and OUT_DIR/manifest.json (posted with
bench/diff/post.py), plus OUT_DIR/deploys.json (posted to /ingest/deploys).

Differs from bench/diff/gen_batches.py on purpose: that one hunts ingest
edge cases inside one hour; this one builds a tenant that looks like an
app (few routes, many requests, tenants, users, failing jobs, n+1s, LLM
calls, sessions per deploy) across days.

The defaults are the props-diff tenant. For a rollup-heavy tenant like a
busy production one (docs/PERF.md, "Rollup reads"): --shapes 12000 (query
groups; 40% of queries go to the first 200), --tenants 300 (app tenants plus
"transloadx", a small one), --uniform (times spread evenly, not denser near
--now) and --by-time (batches in time order, so each batch touches an hour
or two of rollups instead of all of them, which makes ingest 20x faster),
--few-logs (one log line for a quarter of executions, not two for each).

--security: requests carry what a security reader filters on (ip, path, user
agent, Referer/X-Forwarded-For headers, rate_limited, halted_callback,
unpermitted_parameters) with ~2% planted attack traffic: scanners hitting
probe paths, 401 bursts on a sign-in route (docs/PERF.md, "Security filters").

--prod-shape (implies --security): requests shaped like rebulk-system's
production: 60% are one hot IoT ingest route (POST /api/v1/ingest/scale_live
from a few gateways, one query each, no logs or views); the probes split
between `unmatched` 404s and a catch-all `GET /:org_slug` route that answers
them 302 to /sign_in (halted by `authenticate`), with a rare probe served
200; request headers as big as a browser's (docs/PERF.md, "Security reads").
"""
import argparse, base64, copy, gzip, hashlib, json, os, random, uuid

ap = argparse.ArgumentParser()
ap.add_argument("fixtures")
ap.add_argument("out")
ap.add_argument("--now", type=float, required=True)
ap.add_argument("--spread-days", type=float, default=9)
ap.add_argument("--executions", type=int, default=6000)
ap.add_argument("--seed", type=int, default=11)
ap.add_argument("--records-per-batch", type=int, default=500)
ap.add_argument("--shapes", type=int, default=60)
ap.add_argument("--tenants", type=int, default=0)
ap.add_argument("--uniform", action="store_true")
ap.add_argument("--by-time", action="store_true")
ap.add_argument("--few-logs", action="store_true")
ap.add_argument("--security", action="store_true")
ap.add_argument("--prod-shape", action="store_true")
a = ap.parse_args()
a.security = a.security or a.prod_shape

fixtures = json.load(open(a.fixtures))
rng = random.Random(a.seed)
os.makedirs(a.out, exist_ok=True)
SPAN = a.spread_days * 86400

def md5(*parts):
    return hashlib.md5(",".join(parts).encode()).hexdigest()

def uid():
    return str(uuid.UUID(int=rng.getrandbits(128), version=4))

TABLES = ["widgets", "gadgets", "users", "orders", "line_items", "accounts"]
SHAPES = []
for i in range(a.shapes):
    t = TABLES[i % len(TABLES)]
    sql = f'SELECT "{t}".* FROM "{t}" WHERE "{t}"."id" = ? AND "{t}"."c{i}" = ? LIMIT ?'
    conn = "primary" if i % 5 else "replica"
    SHAPES.append((conn, sql, md5(conn, sql)))
ROUTES = [("GET", f"/widgets/{i}(.:format)") for i in range(1, 25)] + [("POST", "/orders(.:format)"), ("DELETE", "/sessions/:id(.:format)")]
JOBS = ["WidgetJob", "ReportJob", "MailerJob", "CleanupJob", "SyncJob"]
TENANTS = [None, None, "acme", "globex", "initech", "umbrella.co"]
USERS = [None, None, "1", "2", "42", "acme:7", "globex:9"]
if a.tenants:
    TENANTS = [None, None] + [f"tenant{i}" for i in range(a.tenants)] + ["transloadx"]
    USERS = [None, None] + [str(i) for i in range(2000)]
# Deploys every ~2 days; each record carries the deploy live at its time.
DEPLOYS = []
t = a.now - SPAN
k = 0
while t < a.now:
    DEPLOYS.append((t, f"{k:02x}" + hashlib.sha1(str(k).encode()).hexdigest()[:10]))
    k += 1
    t += 2 * 86400 * rng.uniform(0.7, 1.3)

def deploy_at(ts):
    live = DEPLOYS[0][1]
    for at, d in DEPLOYS:
        if at <= ts:
            live = d
    return live

def base(t, ts, eid=None, trace=None):
    r = copy.deepcopy(fixtures[t])
    r["timestamp"] = round(ts, 6)
    r["deploy"] = deploy_at(ts)
    r["server"] = rng.choice(["web-1", "web-2", "worker-1"])
    if "execution_id" in r:
        r["execution_id"] = eid
    if "trace_id" in r:
        r["trace_id"] = trace
    return r

def stamp(r, user, tenant):
    if "user" in r:
        r["user"] = user
    if "tenant" in r:
        r["tenant"] = tenant
    return r

UAS = ["Mozilla/5.0 (Macintosh; Intel Mac OS X 14_6) AppleWebKit/605.1.15 Safari/605.1.15",
       "Mozilla/5.0 (X11; Linux x86_64; rv:131.0) Gecko/20100101 Firefox/131.0",
       "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 Chrome/130.0 Safari/537.36"]
SCANNERS = [f"198.51.100.{i}" for i in range(1, 40)]
PROBES = ["/.env", "/.git/config", "/wp-login.php", "/wp-admin/setup-config.php", "/phpmyadmin/", "/cgi-bin/luci", "/static/../../etc/passwd", "/xmlrpc.php"]

BROWSER_HEADERS = {"Accept-Language": "en-US,en;q=0.9", "Accept-Encoding": "gzip, deflate, br, zstd", "Cf-Ipcountry": "US",
                   "Sec-Ch-Ua": "\"Chromium\";v=\"130\", \"Google Chrome\";v=\"130\", \"Not?A_Brand\";v=\"99\"", "Sec-Fetch-Mode": "navigate",
                   "Sec-Fetch-Site": "same-origin", "Cookie": "[FILTERED]", "X-Request-Id": "6f1c2a9e-0d1b-4c55-9a77-3c2f9b8e4d10"}
GATEWAYS = [f"100.64.{i}.{i * 7 % 250 + 1}" for i in range(6)]

def hot_ingest(p):
    """rebulk-system's busiest route: a scale gateway posting readings."""
    p["method"], p["route"], p["status_code"] = "POST", "/api/v1/ingest/scale_live(.:format)", 200
    p["_group"] = md5("POST", "/api/v1/ingest/scale_live(.:format)")
    p["ip"], p["path"], p["user_agent"] = rng.choice(GATEWAYS), "/api/v1/ingest/scale_live", "ESP32HTTPClient"
    p["url"] = "https://shop.example/api/v1/ingest/scale_live"
    p["headers"] = {"Host": "shop.example", "Content-Type": "application/json", "X-Forwarded-For": f"{p['ip']}, 10.0.0.1", "Cf-Ipcountry": "US"}
    p["response_size"], p["inertia"], p["exception_preview"] = 2, None, None
    p["duration"] = int(rng.lognormvariate(8, 0.5))
    p["user"], p["tenant"] = None, None

def security_fields(p, route):
    """ip, path, user agent and flags on a request; a few are attacks."""
    roll = rng.random()
    path = "/" + route.split("(")[0].strip("/").replace(":id", str(rng.randrange(1, 900)))
    ip, ua = f"10.{rng.randrange(0, 4)}.{rng.randrange(0, 256)}.{rng.randrange(1, 255)}", rng.choice(UAS)
    # The fixture's own redirect_to (null) and response_size (0); a probe
    # below may set its own.
    redirect, size = None, None
    if roll < 0.01:
        ip, ua, path = rng.choice(SCANNERS), rng.choice(["curl/8.4.0", "python-requests/2.31", "Go-http-client/1.1"]), rng.choice(PROBES)
        p["status_code"], p["route"], p["method"] = 404, "unmatched", "GET"
        p["_group"] = md5("GET", "unmatched")
        if a.prod_shape and "/" not in path.strip("/"):
            # A one-segment probe (/.env, /config.php) matches the app's
            # catch-all org slug route, which sends it to sign in; one in
            # 2,000 is served (the real alarm).
            p["route"], p["_group"] = "/:org_slug(.:format)", md5("GET", "/:org_slug(.:format)")
            if rng.random() < 0.0005:
                p["status_code"], size = 200, rng.randrange(400, 4000)
            else:
                p["status_code"], p["halted_callback"] = 302, "authenticate"
                redirect = "https://shop.example/sign_in"
    elif roll < 0.02:
        ip, path = f"203.0.113.{rng.randrange(1, 6)}", "/session"
        p["status_code"], p["route"], p["method"] = 401, "/session(.:format)", "POST"
        p["_group"] = md5("POST", "/session(.:format)")
    p["ip"], p["path"], p["user_agent"] = ip, path, ua
    p["url"] = f"https://shop.example{path}"
    p["headers"] = {"Host": "shop.example", "Referer": "https://shop.example/", "X-Forwarded-For": f"{ip}, 10.0.0.1", "Accept": "text/html"}
    if rng.random() < 0.003:
        p["rate_limited"] = {"name": "default", "count": 61, "to": 60}
        p["status_code"] = 429
    if rng.random() < 0.002:
        p["halted_callback"] = "require_login"
    if rng.random() < 0.002:
        p["unpermitted_parameters"] = ["admin"]
    # Last: the flags above may have changed the status.
    if a.prod_shape:
        p["headers"].update(BROWSER_HEADERS)
        if redirect is None and p["status_code"] == 302:
            redirect = f"https://shop.example/widgets/{rng.randrange(1, 900)}"
        if size is None:
            size = rng.randrange(800, 60000) if p["status_code"] == 200 else 0
        p["redirect_to"], p["response_size"] = redirect, size

def execution(ts):
    eid, trace = uid(), uid()
    user, tenant = rng.choice(USERS), rng.choice(TENANTS)
    out = []
    kind = rng.choices(["request", "job_attempt", "scheduled_task", "command"], [80, 15, 3, 2])[0]
    p = stamp(base(kind, ts, eid, trace), user, tenant)
    # long tail durations so p95/p99 differ from p50
    p["duration"] = int(rng.lognormvariate(10, 1.1))
    if kind == "request":
        m, route = rng.choice(ROUTES)
        p["method"], p["route"] = m, route
        p["_group"] = md5(m, route)
        p["status_code"] = rng.choices([200, 302, 404, 422, 500], [80, 6, 6, 3, 5])[0]
        p["inertia"] = rng.choice([None, {"component": "Widgets/Show", "partial_only": []}])
        if p["status_code"] >= 500:
            p["exception_preview"] = rng.choice(["ArgumentError: kaboom", "NoMethodError: undefined method `x' for nil"])
        if a.prod_shape and rng.random() < 0.6:
            hot_ingest(p)
            q = stamp(base("query", ts + 0.001, eid, trace), None, None)
            q["connection"], q["sql"], q["_group"] = SHAPES[0]
            q["duration"], q["row_count"], q["source"], q["role"] = int(rng.lognormvariate(6, 0.5)), 1, None, "writing"
            return [p, q]
        if a.security:
            security_fields(p, route)
    elif kind == "job_attempt":
        name = rng.choice(JOBS)
        p["name"], p["_group"] = name, md5(name)
        p["queue"] = rng.choice(["default", "mailers", "low"])
        p["status"] = rng.choices(["processed", "failed"], [92, 8])[0]
        p["queue_latency"] = rng.randrange(0, 5_000_000)
        p["job_id"] = uid()
        if p["status"] == "failed":
            p["exception_preview"] = "Timeout::Error: execution expired"
    elif kind == "scheduled_task":
        key = rng.choice(["cleanup_hourly", "digest_daily"])
        p["task_key"], p["_group"] = key, md5(key)
        p["schedule"] = {"cleanup_hourly": "0 * * * *", "digest_daily": "every 5 minutes"}[key]
        p["status"] = rng.choices(["processed", "failed"], [90, 10])[0]
    else:
        p["exit_code"] = rng.choice([0, 0, 0, 1])
    out.append(p)
    off = 0.0
    for _ in range(rng.randrange(3, 9)):
        off += rng.uniform(0.0005, 0.01)
        q = stamp(base("query", ts + off, eid, trace), user, tenant)
        if len(SHAPES) > 200:
            conn, sql, group = SHAPES[rng.randrange(200) if rng.random() < 0.4 else rng.randrange(len(SHAPES))]
        else:
            conn, sql, group = rng.choice(SHAPES)
        q["connection"], q["sql"], q["_group"] = conn, sql, group
        q["duration"] = int(rng.lognormvariate(7, 1.3))
        q["row_count"] = rng.randrange(0, 50)
        q["source"] = rng.choice(["app/models/widget.rb:12", "app/controllers/widgets_controller.rb:8", None])
        q["role"] = rng.choice(["writing", "reading"])
        if rng.random() < 0.02:
            q["explain"] = "SCAN widgets"
        out.append(q)
    if rng.random() < 0.6:
        c = stamp(base("cache_event", ts + off, eid, trace), user, tenant)
        c["key"] = f"views/widgets/{rng.randrange(0, 40)}"
        c["type"] = rng.choice(["hit", "hit", "miss", "write", "generate"])
        c["_group"] = md5(c["store"], c["key"])
        out.append(c)
    if rng.random() < 0.5:
        v = stamp(base("view_render", ts + off, eid, trace), user, tenant)
        ident = rng.choice(["app/views/widgets/show.html.erb", "app/views/widgets/_row.html.erb", "app/views/layouts/app.html.erb"])
        v["identifier"], v["_group"] = ident, md5(ident)
        v["kind"] = "partial" if "/_" in ident else "template"
        v["duration"] = rng.randrange(100, 20000)
        out.append(v)
    msgs = ["Started GET \"/widgets\" for 127.0.0.1", "Completed 200 OK in 12ms (Views: 3.1ms | ActiveRecord: 4.2ms)"]
    if a.few_logs:
        msgs = msgs[1:] if rng.random() < 0.25 else []
    for msg in msgs:
        lg = stamp(base("log", ts + off, eid, trace), user, tenant)
        lg["message"] = msg
        lg["level"] = rng.choices(["info", "warn", "error", "debug"], [85, 7, 4, 4])[0]
        out.append(lg)
    if rng.random() < 0.15:
        o = stamp(base("outgoing_request", ts + off, eid, trace), user, tenant)
        host = rng.choice(["api.stripe.com", "hooks.slack.com"])
        o["host"], o["url"], o["_group"] = host, f"https://{host}/v1/x", md5("GET", host)
        o["status_code"] = rng.choice([200, 200, 201, 503, None])
        o["duration"] = rng.randrange(20_000, 900_000)
        out.append(o)
    if p.get("status_code", 200) >= 500 or p.get("status") == "failed" or rng.random() < 0.02:
        e = stamp(base("exception", ts + off, eid, trace), user, tenant)
        cls = rng.choice(["ArgumentError", "NoMethodError", "Timeout::Error"])
        e["class"], e["message"] = cls, rng.choice(["kaboom", "undefined method `x' for nil", "execution expired"])
        e["handled"] = rng.random() < 0.3
        e["_group"] = md5(cls, e["message"][:20])
        out.append(e)
    if rng.random() < 0.03:
        n = stamp(base("n_plus_one", ts + off, eid, trace), user, tenant)
        n["sql"] = rng.choice(['SELECT "comments".* FROM "comments" WHERE "comments"."post_id" = ?',
                               'SELECT COUNT(*) FROM "likes" WHERE "likes"."post_id" = ?'])
        n["_group"] = md5(n["sql"])
        n["count"] = rng.randrange(3, 40)
        out.append(n)
    if rng.random() < 0.04:
        sp = stamp(base("span", ts + off, eid, trace), user, tenant)
        name = rng.choice(["pdf.render", "geo.lookup"])
        sp["name"], sp["_group"] = name, md5(name)
        sp["attributes"] = {"vendor": rng.choice(["prawn", "wkhtml"]), "pages": rng.randrange(1, 4)}
        sp["status"] = rng.choice(["ok", "ok", "failed"])
        sp["duration"] = rng.randrange(1000, 400000)
        out.append(sp)
    if rng.random() < 0.02:
        tr = stamp(base("transaction", ts + off, eid, trace), user, tenant)
        tr["outcome"] = rng.choice(["commit", "commit", "rollback"])
        tr["_group"] = md5(tr["connection"], tr["outcome"])
        out.append(tr)
    if rng.random() < 0.02:
        lc = stamp(base("llm_call", ts + off, eid, trace), user, tenant)
        model = rng.choice(["claude-opus-5", "gpt-6"])
        lc["model"], lc["response_model"] = model, model
        lc["_group"] = md5(lc["provider"], model, lc["operation"])
        lc["cost_nanos"] = rng.choice([None, rng.randrange(10_000, 900_000)])
        lc["finish_reason"] = rng.choice([None, "stop", "max_tokens"])
        lc["status"] = rng.choice(["ok", "ok", "failed"])
        lc["duration"] = rng.randrange(200_000, 9_000_000)
        out.append(lc)
        if rng.random() < 0.5:
            tool = stamp(base("llm_call", ts + off, eid, trace), user, tenant)
            tool["operation"], tool["tool_name"], tool["model"] = "tool", rng.choice(["lookup", "search"]), None
            tool["_group"] = md5("tool", tool["tool_name"])
            tool["duration"] = rng.randrange(1000, 90000)
            out.append(tool)
    for t, prob in [("mail", 0.01), ("broadcast", 0.01), ("notification", 0.01), ("storage_op", 0.01), ("deprecation", 0.01), ("enqueued_job", 0.03)]:
        if rng.random() < prob:
            out.append(stamp(base(t, ts + off, eid, trace), user, tenant))
    if rng.random() < 0.01:
        pr = stamp(base("profile", ts, eid, trace), user, tenant)
        stacks = b"main;handle;render 7\nmain;handle;query 3\n"
        pr["stacks"] = base64.b64encode(gzip.compress(stacks)).decode()
        pr["stacks_bytes"] = len(stacks)
        pr["samples"] = 10
        out.append(pr)
    if user and rng.random() < 0.05:
        u = base("user", ts, eid, trace)
        u["id"], u["name"], u["email"], u["tenant"] = user, rng.choice(["Ada", "Grace", None]), f"u{user}@example.com", tenant
        out.append(u)
    if rng.random() < 0.02:
        vi = base("visit", ts)
        comp = rng.choice(["Widgets/Index", "Widgets/Show"])
        vi["component"], vi["_group"] = comp, md5(comp)
        vi["lcp"], vi["inp"], vi["ttfb"] = rng.randrange(800, 5000), rng.randrange(50, 700), rng.randrange(100, 2000)
        vi["cls"] = rng.choice([None, 0.05, 0.2, 0.31])
        vi["status"] = rng.choice(["200", "200", "error"])
        vi["user"], vi["tenant"] = user, tenant
        out.append(vi)
    if rng.random() < 0.02:
        se = base("session", ts)
        se["id"] = uid()
        se["status"] = rng.choice(["ok", "ok", "errored", "crashed"])
        se["user"] = user
        out.append(se)
    return out

manifest = []

def write_batches(records):
    for b in range(0, len(records), a.records_per_batch):
        chunk = records[b:b + a.records_per_batch]
        name = f"{len(manifest):04d}.ndjson.gz"
        body = "".join(json.dumps(r) + "\n" for r in chunk).encode()
        with open(os.path.join(a.out, name), "wb") as f:
            f.write(gzip.compress(body))
        manifest.append({"file": name, "headers": {}})

# health/process records near now (the processes page reads the last 10 min)
records = []
if a.prod_shape:
    # A production-sized tenant does not fit in memory as one list: times
    # are drawn and sorted up front, and batches written every 20k executions.
    times = sorted(a.now - SPAN * (rng.random() if a.uniform else rng.random() ** 1.6) for _ in range(a.executions))
    for i in range(0, len(times), 20_000):
        chunk = [r for ts in times[i:i + 20_000] for r in execution(ts)]
        chunk.sort(key=lambda r: r["timestamp"])
        write_batches(chunk)
    a.executions = 0
for i in range(a.executions):
    ts = a.now - SPAN * (rng.random() if a.uniform else rng.random() ** 1.6)  # denser near now
    records.extend(execution(ts))
for server, pid in [("web-1", 101), ("web-2", 102), ("worker-1", 201)]:
    for back in [30, 200, 400, 900, 3000]:
        h = base("health", a.now - back)
        h["server"], h["pid"] = server, pid
        h["threads_busy"] = rng.randrange(0, 6)
        records.append(h)
    pr = base("process", a.now - 3600)
    pr["server"], pr["pid"] = server, pid
    records.append(pr)

if a.by_time:
    records.sort(key=lambda r: r["timestamp"])
else:
    rng.shuffle(records)
write_batches(records)
json.dump(manifest, open(os.path.join(a.out, "manifest.json"), "w"))
json.dump([{"deploy": d, "ref": hashlib.sha1(d.encode()).hexdigest(), "name": f"Release {i}", "timestamp": __import__("datetime").datetime.fromtimestamp(at, __import__("datetime").timezone.utc).strftime("%Y-%m-%dT%H:%M:%S.%fZ"), "server": "web-1",
            "performer": "cole", "commits": [{"sha": hashlib.sha1(f"{d}{j}".encode()).hexdigest(), "message": f"commit {j}"} for j in range(3)]}
           for i, (at, d) in enumerate(DEPLOYS)], open(os.path.join(a.out, "deploys.json"), "w"))
print(f"{len(manifest)} batches, {len(DEPLOYS)} deploys")
