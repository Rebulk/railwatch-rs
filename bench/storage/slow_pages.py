#!/usr/bin/env python3
"""Helpers for bench/storage/slow_pages.sh.

    slow_pages.py token CONTROL.sqlite3 OUT_TOKENS.json
    slow_pages.py page-cases BASE_CASES.json NOW OUT.json
    slow_pages.py api-cases TENANT.sqlite3 CONTROL.sqlite3 OUT.json
    slow_pages.py report WORK LABEL [BASE_LABEL]

token: issues one full-scope API token (api-dump reads it from
api-tokens.json). page-cases: every props-diff case under every window
preset (cases make_cases.py gives one window keep it). api-cases: every
/api/v1 environment endpoint under every window it takes, plus the
filtered, sorted and second-page variants. report: the identity gate and
the slowest cases.
"""
import datetime, hashlib, json, os, secrets, sqlite3, statistics, sys

WINDOWS = ["1h", "6h", "24h", "7d", "30d"]


def token(control, out):
    raw = "rwp_" + secrets.token_hex(20)
    db = sqlite3.connect(control)
    now = datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%d %H:%M:%S.%f")
    db.execute(
        "INSERT INTO api_tokens (user_id, name, scopes, token_digest, created_at, updated_at) VALUES (1, 'bench', ?, ?, ?, ?)",
        (json.dumps(["api", "mcp", "read", "write"]), hashlib.sha256(raw.encode()).hexdigest(), now, now),
    )
    db.commit()
    json.dump({"full": raw}, open(out, "w"))


def page_cases(base, now, out):
    now = int(now)
    iso = lambda t: datetime.datetime.fromtimestamp(t, datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    cases, seen = [], set()
    for c in json.load(open(base)):
        stem, _, w = c["name"].rpartition("@")
        # Cases make_cases.py runs under all three windows get every preset.
        windows = WINDOWS if w in ("1h", "24h", "7d") and "window" in c["params"] else [w]
        for win in windows:
            name = f"{stem}@{win}"
            if name in seen:
                continue
            seen.add(name)
            p = dict(c["params"])
            if win != "None":
                p["window"] = win
            for k, v in list(p.items()):
                p[k] = v.replace("CUSTOM_FROM", iso(now - 8 * 3600)).replace("CUSTOM_TO", iso(now - 2 * 3600))
            cases.append(dict(c, name=name, params=p))
    json.dump(cases, open(out, "w"), indent=1)
    print(f"{len(cases)} page cases")


def api_cases(tenant, control, out):
    t = sqlite3.connect(f"file:{tenant}?mode=ro", uri=True)
    c = sqlite3.connect(f"file:{control}?mode=ro", uri=True)
    env_id, app_id = c.execute("SELECT id, application_id FROM environments ORDER BY id LIMIT 1").fetchone()
    one = lambda sql: (t.execute(sql).fetchone() or [None])[0]
    route = one("SELECT group_hash FROM rollups WHERE record_type = 'request' GROUP BY group_hash ORDER BY SUM(count) DESC LIMIT 1")
    qhash = one("SELECT group_hash FROM rollups WHERE record_type = 'query' GROUP BY group_hash ORDER BY SUM(count) DESC LIMIT 1")
    slowest = one("SELECT execution_id FROM executions WHERE kind = 'request' ORDER BY duration DESC LIMIT 1")
    job_class = one("SELECT name FROM executions WHERE kind = 'job_attempt' GROUP BY name ORDER BY COUNT(*) DESC LIMIT 1")
    log_word = (one("SELECT message FROM logs ORDER BY id LIMIT 1") or "Completed").split()[0]
    E = f"/api/v1/applications/{app_id}/environments/{env_id}"
    cases = []

    def case(name, path, **extra):
        cases.append(dict({"name": name, "method": "GET", "path": path, "token": "full", "write": False}, **extra))

    for w in WINDOWS:
        for ep in ["summary", "requests", "jobs", "queries", "exceptions", "logs", "releases", "users", "tenants", "spans"]:
            case(f"api.{ep}@{w}", f"{E}/{ep}?window={w}")
        case(f"api.series.request@{w}", f"{E}/series?type=request&window={w}")
        case(f"api.series.query@{w}", f"{E}/series?type=query&window={w}")
        if route:
            case(f"api.request_group@{w}", f"{E}/requests/{route}?window={w}")
            case(f"api.series.group@{w}", f"{E}/series?type=request&window={w}&group_hash={route}")
        if qhash:
            case(f"api.series.query_group@{w}", f"{E}/series?type=query&window={w}&group_hash={qhash}")
    for w in ["24h", "7d", "30d"]:
        case(f"api.queries.adapter@{w}", f"{E}/queries?window={w}&q=adapter:sqlite&limit=3")
        case(f"api.queries.p99@{w}", f"{E}/queries?window={w}&limit=5&per_page=10&sort=p99")
        case(f"api.queries.page2@{w}", f"{E}/queries?window={w}&limit=5&per_page=10&sort=p99", cursor_from=f"api.queries.p99@{w}")
        case(f"api.logs.text@{w}", f"{E}/logs?window={w}&q={log_word}&limit=7")
        case(f"api.logs.text_page2@{w}", f"{E}/logs?window={w}&q={log_word}&limit=7", cursor_from=f"api.logs.text@{w}")
        case(f"api.logs.level@{w}", f"{E}/logs?window={w}&q=level:error&per_page=5")
        case(f"api.jobs.failed@{w}", f"{E}/jobs?window={w}&q=outcome:failed")
        if job_class:
            case(f"api.jobs.class@{w}", f"{E}/jobs?window={w}&q=class:{job_class}&limit=3&sort=max")
        case(f"api.requests.q_5xx@{w}", f"{E}/requests?window={w}&q=status:5xx")
        case(f"api.exceptions.page2@{w}", f"{E}/exceptions?window={w}&per_page=10&page=2&limit=10")
    if slowest:
        case("api.execution.slowest", f"{E}/executions/{slowest}")
    case("api.deploys", f"{E}/deploys")
    json.dump(cases, open(out, "w"), indent=1)
    print(f"{len(cases)} api cases")


def load(path):
    return json.load(open(path)) if os.path.exists(path) else None


def report(work, label, base=None):
    bad = 0

    def same(a, b, what):
        nonlocal bad
        if a is None or b is None:
            print(f"GATE {what}: missing output")
            bad += 1
            return
        diff = [k for k in a if json.dumps(a[k]) != json.dumps(b.get(k))] + [k for k in b if k not in a]
        errs = [k for k, v in a.items() if isinstance(v, dict) and (v.get("status") in (500, "no such page") or str(v.get("status")).startswith("5"))]
        print(f"GATE {what}: {len(a) - len(diff)}/{len(a)} identical" + (f"; DIFF {diff[:10]}" if diff else "") + (f"; 5xx {errs[:5]}" if errs else ""))
        bad += len(diff) + len(errs)

    for kind in ["pages", "api"]:
        same(load(f"{work}/{label}-single-{kind}.json"), load(f"{work}/{label}-split-{kind}.json"), f"{label} single vs per-day {kind}")
        if base:
            for v in ["single", "split"]:
                same(load(f"{work}/{base}-{v}-{kind}.json"), load(f"{work}/{label}-{v}-{kind}.json"), f"{base} vs {label} {v} {kind}")

    med = lambda path: {k: statistics.median(v) for k, v in (load(path) or {}).items()}
    rows = []
    for kind in ["pages", "api"]:
        s, p = med(f"{work}/{label}-single-{kind}.json.timings.json"), med(f"{work}/{label}-split-{kind}.json.timings.json")
        bs = med(f"{work}/{base}-single-{kind}.json.timings.json") if base else {}
        bp = med(f"{work}/{base}-split-{kind}.json.timings.json") if base else {}
        for k in s:
            rows.append((k, bs.get(k, 0), bp.get(k, 0), s[k], p.get(k, 0)))
    pages = [r for r in rows if not r[0].startswith("api.")]
    print(f"sum of medians: single {sum(r[3] for r in rows):.0f} ms, per-day {sum(r[4] for r in rows):.0f} ms ({len(rows)} cases)")
    at7 = [r for r in pages if r[0].endswith("@7d")]
    print(f"pages @7d over 50 ms: single {[r[0] for r in at7 if r[3] > 50]}, per-day {[r[0] for r in at7 if r[4] > 50]} (of {len(at7)})")
    ratio = sorted((r[4] / max(r[3], 0.01), r[0]) for r in rows if max(r[3], r[4]) >= 2)
    over = [f"{n} {x:.2f}x" for x, n in ratio if x > 1.2]
    print(f"per-day / single (cases >= 2 ms): median {statistics.median(x for x, _ in ratio):.2f}x; {len(over)} over 1.2x, worst: {over[::-1][:12]}")
    top = int(os.environ.get("TOP", "15"))
    print(f"slowest {top} (median ms)" + (f", {base} -> {label}" if base else ""))
    for k, b1, b2, s, p in sorted(rows, key=lambda r: -max(r[1:]))[:top]:
        before = f"single {b1:7.1f} -> {s:7.1f}   per-day {b2:7.1f} -> {p:7.1f}" if base else f"single {s:7.1f}   per-day {p:7.1f}"
        print(f"  {k:34s} {before}   per-day/single {p / max(s, 0.01):4.2f}x")
    return 1 if bad else 0


if __name__ == "__main__":
    cmd, args = sys.argv[1], sys.argv[2:]
    if cmd == "token":
        token(*args)
    elif cmd == "page-cases":
        page_cases(*args)
    elif cmd == "api-cases":
        api_cases(*args)
    elif cmd == "report":
        sys.exit(report(*args))
    else:
        sys.exit(f"unknown command {cmd}")
