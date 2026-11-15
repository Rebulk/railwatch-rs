#!/usr/bin/env python3
"""cases.py DATA_DIR OUT.json: page-replay cases for the main dashboard
pages (overview, requests, queries, jobs, issues) and the most-used MCP
tools (list_slow_routes, get_route, search_requests, list_issues) at 7d
and 30d, over the bench tenant in DATA_DIR (docs/PERF.md, "Cold dashboard
reads"). Adds an MCP token (scopes mcp, read) to the data dir's control DB.
"""
import hashlib, json, secrets, sqlite3, sys

d, out = sys.argv[1], sys.argv[2]
c = sqlite3.connect(d + "/control.sqlite3")
uid = c.execute("SELECT id FROM users ORDER BY id LIMIT 1").fetchone()[0]
acct = c.execute("SELECT account_id FROM memberships WHERE user_id = ? ORDER BY id LIMIT 1", (uid,)).fetchone()[0]
app, env, app_slug, env_name = c.execute(
    "SELECT ap.id, e.id, ap.slug, e.name FROM environments e JOIN applications ap ON ap.id = e.application_id ORDER BY e.id LIMIT 1").fetchone()
raw = "rwp_bench" + secrets.token_hex(16)
c.execute("INSERT INTO api_tokens (user_id, name, scopes, token_digest, created_at, updated_at) VALUES (?, 'perf', ?, ?, datetime('now'), datetime('now'))",
          (uid, json.dumps(["mcp", "read"]), hashlib.sha256(raw.encode()).hexdigest()))
c.commit()

E = f"/apps/{app}/envs/{env}"
cases = []
for w in ["7d", "30d"]:
    for page, path in [("overview", ""), ("requests", "/requests"), ("queries", "/queries"), ("jobs", "/jobs")]:
        cases.append({"name": f"{page}@{w}", "path": f"{E}{path}?window={w}"})
    cases.append({"name": f"issues@{w}", "path": f"/issues?window={w}", "account_id": acct})
    where = {"application_slug": app_slug, "environment": env_name, "window": w}
    for tool, args in [("list_slow_routes", {}), ("get_route", {"route": "POST /api/v1/ingest/scale_live(.:format)"}),
                       ("search_requests", {}), ("search_requests_status", {"q": "status:5xx"}), ("list_issues", {})]:
        name = tool.replace("_status", "")
        body = {"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": name, "arguments": {**where, **args}}}
        if name == "list_issues":
            body["params"]["arguments"] = {"application_slug": app_slug, "environment": env_name}
        cases.append({"name": f"mcp.{tool}@{w}", "path": "/mcp", "body": body, "authorization": f"Bearer {raw}"})
json.dump(cases, open(out, "w"), indent=1)
print(f"{len(cases)} cases; user {uid}")
open(out + ".user", "w").write(str(uid))
