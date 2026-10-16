"""Builds the MCP diff cases for one scratch environment.

  make_cases.py TENANT.sqlite3 PRIMARY.sqlite3 ENV_ID OUT.json [--perf]

Every tool with representative arguments (windows, filters, cursors), the
protocol methods (initialize, tools/prompts/resources list/get/read), and
the error paths: unknown app/environment/issue/tool/prompt/resource, bad
scope (no mcp, no read, no write), missing or bad arguments, malformed
JSON, a form content type, no token, a revoked token. Write cases come last
(they change issues later cases would read) and are marked write: true.

--perf: read cases only, for the latency table.
"""
import json, sqlite3, sys

tenant, primary, env_id, out = sys.argv[1], sys.argv[2], int(sys.argv[3]), sys.argv[4]
perf = "--perf" in sys.argv
t = sqlite3.connect(f"file:{tenant}?mode=ro", uri=True)
p = sqlite3.connect(f"file:{primary}?mode=ro", uri=True)

env_name, app_id = p.execute("SELECT name, application_id FROM environments WHERE id = ?", (env_id,)).fetchone()
app_slug, prefix = p.execute("SELECT slug, issue_prefix FROM applications WHERE id = ?", (app_id,)).fetchone()
acct = p.execute("SELECT a.slug FROM accounts a JOIN applications ap ON ap.account_id = a.id WHERE ap.id = ?", (app_id,)).fetchone()[0]
issues = p.execute("SELECT id, number, kind FROM issues WHERE environment_id = ? ORDER BY id", (env_id,)).fetchall()
exc = [i for i in issues if i[2] == "exception"]
perfi = [i for i in issues if i[2] != "exception"]

one = lambda sql: (t.execute(sql).fetchone() or [None])[0]
slowest_req = one("SELECT execution_id FROM executions WHERE kind = 'request' ORDER BY duration DESC LIMIT 1")
job = one("SELECT execution_id FROM executions WHERE kind = 'job_attempt' ORDER BY occurred_at DESC LIMIT 1")
with_children = one("SELECT execution_id FROM queries GROUP BY execution_id ORDER BY COUNT(*) DESC LIMIT 1")
prof_exe = one("SELECT execution_id FROM profiles WHERE execution_id IS NOT NULL ORDER BY occurred_at DESC LIMIT 1")
prof_id = one("SELECT id FROM profiles ORDER BY id LIMIT 1")
explained = one("SELECT group_hash FROM queries WHERE explain IS NOT NULL GROUP BY group_hash ORDER BY COUNT(*) DESC LIMIT 1")
plain = one("SELECT group_hash FROM queries WHERE explain IS NULL AND group_hash NOT IN (SELECT group_hash FROM queries WHERE explain IS NOT NULL) GROUP BY group_hash ORDER BY COUNT(*) DESC LIMIT 1")
route_name, route_hash = t.execute("SELECT name, group_hash FROM rollups WHERE record_type = 'request' GROUP BY group_hash ORDER BY SUM(count) DESC LIMIT 1").fetchone()
np1_route = one("SELECT e.group_hash FROM n_plus_ones n JOIN executions e ON e.execution_id = n.execution_id WHERE e.kind = 'request' GROUP BY e.group_hash ORDER BY COUNT(*) DESC LIMIT 1")
tenant_name = one("SELECT app_tenant FROM executions WHERE app_tenant IS NOT NULL GROUP BY app_tenant ORDER BY COUNT(*) DESC LIMIT 1")
deploy = one("SELECT deploy FROM executions WHERE kind = 'request' AND deploy IS NOT NULL GROUP BY deploy ORDER BY COUNT(*) DESC LIMIT 1")
user = one("SELECT user_ref FROM executions WHERE user_ref IS NOT NULL GROUP BY user_ref ORDER BY COUNT(*) DESC LIMIT 1")
log_word = one("SELECT message FROM logs ORDER BY id LIMIT 1")
log_word = (log_word or "Completed").split()[0]
exc_class = one("SELECT class_name FROM exceptions GROUP BY class_name ORDER BY COUNT(*) DESC LIMIT 1")
job_class = one("SELECT name FROM executions WHERE kind = 'job_attempt' GROUP BY name ORDER BY COUNT(*) DESC LIMIT 1")
route_word = route_name.split("/")[1] if route_name and "/" in route_name else "widgets"

E = {"application_slug": app_slug, "environment": env_name}
cases = []
n = [0]


def rpc(name, method, params=None, token="full", write=False, **extra):
    n[0] += 1
    body = {"jsonrpc": "2.0", "id": n[0], "method": method}
    if params is not None:
        body["params"] = params
    c = {"name": name, "token": token, "body": body, "write": write}
    c.update(extra)
    cases.append(c)


def tool(name, tool_name, args=None, **kw):
    params = {"name": tool_name}
    if args is not None:
        params["arguments"] = args
    rpc(name, "tools/call", params, **kw)


# -- protocol ------------------------------------------------------------------
if not perf:
    rpc("initialize", "initialize", {"protocolVersion": "2025-06-18"})
    rpc("tools/list", "tools/list")
    rpc("tools/list.read_only", "tools/list", token="read_only")
    rpc("prompts/list", "prompts/list")
    rpc("resources/list", "resources/list")
    rpc("unknown_method", "nope/nope")
    rpc("notification", "notifications/initialized")

# -- read tools ----------------------------------------------------------------
tool("list_applications", "list_applications")
tool("list_applications.no_args", "list_applications", None)
tool("list_issues", "list_issues")
tool("list_issues.env", "list_issues", {**E, "limit": 5})
tool("list_issues.resolved", "list_issues", {"status": "resolved"})
tool("list_issues.limit_neg", "list_issues", {"application_slug": app_slug, "limit": -3})
tool("list_issues.limit_str", "list_issues", {"application_slug": app_slug, "limit": "7"})
tool("list_issues.by_id", "list_issues", {"application_id": app_id, "account_slug": acct, "environment": env_name})
tool("list_issues.unknown_app", "list_issues", {"application_slug": "no-such-app"})
tool("list_issues.bad_app_id", "list_issues", {"application_id": "abc"})
for w in ["1h", "24h", "7d", "30d"]:
    tool(f"list_slow_routes@{w}", "list_slow_routes", {**E, "window": w})
tool("list_slow_routes.bad_window", "list_slow_routes", {**E, "window": "2w"})
tool("list_slow_routes.by_env_slug", "list_slow_routes", {"application_slug": app_slug, "environment": p.execute("SELECT slug FROM environments WHERE id = ?", (env_id,)).fetchone()[0], "window": "7d"})
tool("list_slow_routes.unknown_env", "list_slow_routes", {"application_slug": app_slug, "environment": "nope"})
tool("list_slow_routes.unknown_app", "list_slow_routes", {"application_slug": "nope", "environment": env_name})
tool("list_slow_routes.missing_args", "list_slow_routes", {})
for w in ["24h", "7d", "30d"]:
    tool(f"get_route.name@{w}", "get_route", {**E, "route": route_name, "window": w})
tool("get_route.hash@30d", "get_route", {**E, "group_hash": route_hash, "window": "30d"})
if np1_route:
    tool("get_route.n_plus_one@30d", "get_route", {**E, "group_hash": np1_route, "window": "30d"})
tool("get_route.unknown_route", "get_route", {**E, "route": "GET /nope"})
tool("get_route.bad_hash", "get_route", {**E, "group_hash": "deadbeef", "window": "30d"})
tool("get_route.no_route", "get_route", {**E})
tool("get_profile.latest", "get_profile", {**E})
tool("get_profile.limit", "get_profile", {**E, "limit": 5})
if prof_id:
    tool("get_profile.id", "get_profile", {**E, "profile_id": prof_id})
    tool("get_profile.id_str", "get_profile", {**E, "profile_id": str(prof_id)})
if prof_exe:
    tool("get_profile.execution", "get_profile", {**E, "execution_id": prof_exe, "limit": 200})
tool("get_profile.unknown", "get_profile", {**E, "profile_id": 99999999})
for w in ["1h", "24h", "7d"]:
    tool(f"search_requests@{w}", "search_requests", {**E, "window": w})
tool("search_requests.5xx", "search_requests", {**E, "q": "status:5xx", "window": "30d"})
tool("search_requests.get_404", "search_requests", {**E, "q": "method:get status:404", "window": "30d"})
tool("search_requests.route", "search_requests", {**E, "q": f"route:{route_word} min_ms:250", "window": "7d", "limit": 20})
tool("search_requests.text", "search_requests", {**E, "q": route_word, "window": "7d"})
tool("search_requests.rare_text", "search_requests", {**E, "q": "zzznomatch", "window": "30d"})
if deploy:
    tool("search_requests.deploy", "search_requests", {**E, "q": f"deploy:{deploy}", "window": "30d", "limit": 200})
if tenant_name:
    tool("search_requests.tenant", "search_requests", {**E, "q": f"tenant:{tenant_name}", "window": "30d"})
if user:
    tool("search_requests.user", "search_requests", {**E, "q": f"user:{user}", "window": "30d"})
tool("search_requests.min_ms_float", "search_requests", {**E, "q": "min_ms:1.5e3", "window": "30d", "limit": 500})
if explained:
    tool("explain_query.explained", "explain_query", {**E, "group_hash": explained, "window": "30d"})
if plain:
    tool("explain_query.plain", "explain_query", {**E, "group_hash": plain, "window": "30d"})
tool("explain_query.unknown", "explain_query", {**E, "group_hash": "nope"})
for w in ["24h", "30d"]:
    tool(f"list_tenants@{w}", "list_tenants", {**E, "window": w})
if tenant_name:
    tool("list_tenants.q", "list_tenants", {**E, "window": "30d", "q": tenant_name[:3]})
for label, eid in [("slowest_request", slowest_req), ("job", job), ("many_children", with_children)]:
    if eid:
        tool(f"get_execution.{label}", "get_execution", {**E, "execution_id": eid})
tool("get_execution.unknown", "get_execution", {**E, "execution_id": "00000000-0000-0000-0000-000000000000"})
tool("search_logs", "search_logs", {**E, "q": ""})
tool("search_logs.text", "search_logs", {**E, "q": log_word})
tool("search_logs.level", "search_logs", {**E, "q": "", "level": "error", "limit": 10})
tool("search_logs.unknown_env", "search_logs", {"application_slug": app_slug, "environment": "nope", "q": "x"})
for r in ["jobs", "exceptions", "logs", "queries"]:
    tool(f"search_telemetry.{r}", "search_telemetry", {**E, "resource": r, "window": "7d", "limit": 5})
tool("search_telemetry.jobs_failed", "search_telemetry", {**E, "resource": "jobs", "q": "outcome:failed", "window": "30d"})
if job_class:
    tool("search_telemetry.jobs_class", "search_telemetry", {**E, "resource": "jobs", "q": f"class:{job_class}", "window": "30d", "limit": 3})
if exc_class:
    tool("search_telemetry.exc_class", "search_telemetry", {**E, "resource": "exceptions", "q": f"class:{exc_class}", "window": "30d"})
tool("search_telemetry.queries_adapter", "search_telemetry", {**E, "resource": "queries", "q": "adapter:sqlite", "window": "30d", "limit": 3})
tool("search_telemetry.bad_resource", "search_telemetry", {**E, "resource": "spans"})
tool("search_telemetry.bad_cursor", "search_telemetry", {**E, "resource": "logs", "cursor": "forged--0000"})
tool("search_telemetry.unknown_env", "search_telemetry", {"application_slug": app_slug, "environment": "nope", "resource": "logs"})
for w in ["24h", "7d", "30d"]:
    tool(f"release_health@{w}", "release_health", {**E, "window": w})
tool("recent_deploys", "recent_deploys", {**E})
tool("recent_deploys.unknown_env", "recent_deploys", {"application_slug": app_slug, "environment": "nope"})
tool("list_alerts", "list_alerts")
tool("list_alerts.app", "list_alerts", {"application_slug": app_slug, "status": "sent", "event": "new_issue"})
tool("list_alerts.unknown_app", "list_alerts", {"application_slug": "nope"})
tool("list_alerts.app_only", "list_alerts", {"application_slug": app_slug, "limit": 3})
tool("list_alerts.failed", "list_alerts", {"status": "failed"})
tool("list_alerts.collapsed", "list_alerts", {"status": "collapsed", "event": "threshold"})
tool("list_alerts.bad_filters", "list_alerts", {"status": "nope", "event": "nope", "limit": 0})
for i in exc[:3] + perfi[:2]:
    tool(f"get_issue.{i[2]}.{i[0]}", "get_issue", {"key": f"{prefix}-{i[1]}"})
if exc:
    i = exc[0]
    tool("get_issue.by_id", "get_issue", {"issue_id": i[0]})
    tool("get_issue.canonical", "get_issue", {"key": f"{acct}/{app_id}/{prefix}-{i[1]}"})
    tool("get_issue.id_key_mismatch", "get_issue", {"issue_id": i[0], "key": f"{prefix}-999999"})
tool("get_issue.unknown", "get_issue", {"key": f"{prefix}-999999"})
tool("get_issue.malformed_key", "get_issue", {"key": "nokey"})
tool("get_issue.trailing_slash_key", "get_issue", {"key": f"{acct}/"})
tool("get_issue.two_part_key", "get_issue", {"key": f"{acct}/{prefix}-{exc[0][1]}" if exc else "x/y"})
tool("get_issue.missing_args", "get_issue", {})

if not perf:
    # -- protocol errors ----------------------------------------------------------
    tool("unknown_tool", "no_such_tool", {})
    rpc("tools/call.no_name", "tools/call", {})
    tool("args_array", "list_issues", [1])
    rpc("prompts/get.triage", "prompts/get", {"name": "triage_issue", "arguments": {"key": f"{prefix}-1"}})
    rpc("prompts/get.slow_route", "prompts/get", {"name": "slow_route", "arguments": {**E, "route": route_name, "window": "7d"}})
    rpc("prompts/get.slow_route_no_window", "prompts/get", {"name": "slow_route", "arguments": {**E, "route": route_name}})
    rpc("prompts/get.daily_summary", "prompts/get", {"name": "daily_summary", "arguments": {**E}})
    rpc("prompts/get.no_args", "prompts/get", {"name": "daily_summary"})
    rpc("prompts/get.unknown", "prompts/get", {"name": "nope"})
    rpc("resources/read.applications", "resources/read", {"uri": "railwatch://applications"})
    rpc("resources/read.summary", "resources/read", {"uri": f"railwatch://applications/{acct}/{app_slug}/environments/{env_name}/summary"})
    rpc("resources/read.summary_short", "resources/read", {"uri": f"railwatch://applications/{app_slug}/environments/{env_name}/summary"})
    rpc("resources/read.summary_unknown", "resources/read", {"uri": f"railwatch://applications/{app_slug}/environments/nope/summary"})
    rpc("resources/read.doc", "resources/read", {"uri": "railwatch://docs/ai-and-mcp"})
    rpc("resources/read.doc_upper", "resources/read", {"uri": "railwatch://docs/README"})
    rpc("resources/read.doc_unknown", "resources/read", {"uri": "railwatch://docs/nope"})
    rpc("resources/read.unknown", "resources/read", {"uri": "https://example.com"})
    # -- auth and transport ---------------------------------------------------------
    cases.append({"name": "auth.none", "token": None, "body": {"jsonrpc": "2.0", "id": 1, "method": "initialize"}})
    cases.append({"name": "auth.garbage", "token": None, "authorization": "Bearer rwp_nope", "body": {"jsonrpc": "2.0", "id": 1, "method": "initialize"}})
    cases.append({"name": "auth.lowercase_bearer", "token": None, "authorization": "bearer x", "body": {"jsonrpc": "2.0", "id": 1, "method": "initialize"}})
    cases.append({"name": "auth.revoked", "token": "revoked", "body": {"jsonrpc": "2.0", "id": 1, "method": "initialize"}})
    cases.append({"name": "auth.api_only", "token": "api_only", "body": {"jsonrpc": "2.0", "id": 1, "method": "initialize"}})
    cases.append({"name": "auth.mcp_no_read", "token": "mcp_no_read", "body": {"jsonrpc": "2.0", "id": 1, "method": "initialize"}})
    cases.append({"name": "parse.malformed", "token": "full", "body": "{bad"})
    cases.append({"name": "parse.empty", "token": "full", "body": ""})
    cases.append({"name": "parse.form_ctype", "token": "full", "content_type": "application/x-www-form-urlencoded", "body": {"jsonrpc": "2.0", "id": 1, "method": "initialize"}})
    cases.append({"name": "parse.text_ctype", "token": "full", "content_type": "text/plain", "body": {"jsonrpc": "2.0", "id": 1, "method": "initialize"}})
    cases.append({"name": "parse.string_body", "token": "full", "body": "\"the id method\""})
    # Valid JSON that is not an object: 400 -32600 (the HTML 500 page before
    # the 2026-10-16 Rails fix).
    cases.append({"name": "parse.array_body", "token": "full", "body": "[1]"})
    cases.append({"name": "parse.null_body", "token": "full", "body": "null"})
    cases.append({"name": "parse.number_body", "token": "full", "body": "7"})
    rpc("prompts/get.params_array", "prompts/get", [1])
    rpc("tools/call.params_array", "tools/call", [1])
    rpc("initialize.params_string", "initialize", "x")
    rpc("tools/list.params_number", "tools/list", 1)
    # -- restricted tokens -------------------------------------------------------------
    tool("restricted_app.list_applications", "list_applications", token="other_app")
    tool("restricted_app.list_issues", "list_issues", {"application_slug": app_slug}, token="other_app")
    tool("restricted_app.slow_routes", "list_slow_routes", {**E}, token="other_app")
    tool("restricted_app.list_alerts", "list_alerts", token="other_app")
    # -- writes ---------------------------------------------------------------------------
    tool("write.read_only.update_issue", "update_issue", {"key": f"{prefix}-{exc[0][1]}", "status": "resolved"}, token="read_only", write=True)
    tool("write.read_only.add_comment", "add_comment", {"key": f"{prefix}-{exc[0][1]}", "body": "x"}, token="read_only", write=True)
    tool("write.update_issue.unknown", "update_issue", {"key": f"{prefix}-999999", "status": "resolved"}, write=True)
    tool("write.update_issue.resolve", "update_issue", {"key": f"{prefix}-{exc[0][1]}", "status": "resolved", "priority": "high", "agent": "diff-bot"}, write=True)
    tool("write.update_issue.reopen", "update_issue", {"issue_id": exc[0][0], "status": "open"}, write=True)
    tool("write.update_issue.ignore", "update_issue", {"key": f"{prefix}-{exc[1][1]}", "status": "ignored", "priority": "low"}, write=True)
    tool("write.update_issue.assign", "update_issue", {"key": f"{prefix}-{exc[1][1]}", "assignee_email": "PROPS-DIFF@example.com"}, write=True)
    tool("write.update_issue.unassign", "update_issue", {"key": f"{prefix}-{exc[1][1]}", "assignee_email": ""}, write=True)
    tool("write.update_issue.bad_assignee", "update_issue", {"key": f"{prefix}-{exc[1][1]}", "assignee_email": "nobody@example.com"}, write=True)
    tool("write.update_issue.bad_priority", "update_issue", {"key": f"{prefix}-{exc[2][1]}", "priority": "whenever"}, write=True)
    tool("write.update_issue.noop", "update_issue", {"key": f"{prefix}-{exc[2][1]}"}, write=True)
    tool("write.add_comment", "add_comment", {"key": f"{prefix}-{exc[0][1]}", "body": "Looks like a <b>N+1</b> & friends", "agent": "diff-bot"}, write=True)
    tool("write.add_comment.blank", "add_comment", {"key": f"{prefix}-{exc[0][1]}", "body": "  "}, write=True)
    tool("write.add_comment.unknown", "add_comment", {"key": f"{prefix}-999999", "body": "x"}, write=True)
    # after the writes: the reads that show them
    tool("after_writes.get_issue", "get_issue", {"key": f"{prefix}-{exc[0][1]}"}, write=True)
    tool("after_writes.list_issues", "list_issues", {**E, "status": "ignored"}, write=True)

json.dump(cases, open(out, "w"), indent=1)
print(f"{len(cases)} cases ({sum(1 for c in cases if c.get('write'))} write) for {app_slug}/{env_name}")
