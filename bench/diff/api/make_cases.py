"""Builds the API diff cases for one scratch environment.

  make_cases.py TENANT.sqlite3 PRIMARY.sqlite3 ENV_ID MEMBER_ID OUT.json [--perf]

Every /api/v1 endpoint with representative params (windows, sorts,
filters, pages, cursors), and the error paths: no / garbage / lowercase
bearer / revoked / expired tokens, tokens without api or read, an
application the token is restricted from, unknown ids (404), malformed
ids, bad params (type, step, cursor, page arrays), pagination edges,
writes without write scope, unrouted paths and verbs, malformed bodies,
the exceptions app's JSON/HTML negotiation, ETag/If-None-Match, HEAD and
the rate limit. Write cases come last (they change rows later cases read)
and are marked write: true.

--perf: read cases only, for the latency table.
"""
import json, sqlite3, sys

tenant, primary, env_id, member_id, out = sys.argv[1], sys.argv[2], int(sys.argv[3]), int(sys.argv[4]), sys.argv[5]
perf = "--perf" in sys.argv
t = sqlite3.connect(f"file:{tenant}?mode=ro", uri=True)
p = sqlite3.connect(f"file:{primary}?mode=ro", uri=True)

app_id = p.execute("SELECT application_id FROM environments WHERE id = ?", (env_id,)).fetchone()[0]
other_app = p.execute("SELECT id FROM applications WHERE slug = 'mcp-diff-other'").fetchone()[0]
issues = p.execute("SELECT id, kind, status FROM issues WHERE environment_id = ? ORDER BY id", (env_id,)).fetchall()
exc = [i[0] for i in issues if i[1] == "exception" and i[2] != "merged"]
perfi = [i[0] for i in issues if i[1] != "exception"]
merged = [i[0] for i in issues if i[2] == "merged"]
user_id = p.execute("SELECT id FROM users WHERE email = 'props-diff@example.com'").fetchone()[0]

one = lambda sql: (t.execute(sql).fetchone() or [None])[0]
route_hash = one("SELECT group_hash FROM rollups WHERE record_type = 'request' GROUP BY group_hash ORDER BY SUM(count) DESC LIMIT 1")
route_name = one(f"SELECT name FROM rollups WHERE group_hash = '{route_hash}' LIMIT 1") if route_hash else None
route_word = route_name.split("/")[1] if route_name and "/" in route_name else "x"
slowest_req = one("SELECT execution_id FROM executions WHERE kind = 'request' ORDER BY duration DESC LIMIT 1")
job = one("SELECT execution_id FROM executions WHERE kind = 'job_attempt' ORDER BY occurred_at DESC LIMIT 1")
traced = one("SELECT execution_id FROM executions WHERE trace_id IN (SELECT trace_id FROM executions WHERE trace_id IS NOT NULL GROUP BY trace_id HAVING COUNT(*) > 1) LIMIT 1")
with_exc = one("SELECT execution_id FROM exceptions WHERE execution_id IS NOT NULL ORDER BY occurred_at DESC LIMIT 1")
query_hash = one("SELECT group_hash FROM rollups WHERE record_type = 'query' GROUP BY group_hash ORDER BY SUM(count) DESC LIMIT 1")
job_class = one("SELECT name FROM executions WHERE kind = 'job_attempt' GROUP BY name ORDER BY COUNT(*) DESC LIMIT 1")
exc_class = one("SELECT class_name FROM exceptions GROUP BY class_name ORDER BY COUNT(*) DESC LIMIT 1")
tenant_name = one("SELECT app_tenant FROM executions WHERE app_tenant IS NOT NULL GROUP BY app_tenant ORDER BY COUNT(*) DESC LIMIT 1")
span_word = (one("SELECT name FROM spans GROUP BY name ORDER BY COUNT(*) DESC LIMIT 1") or "x")[:4]
log_word = (one("SELECT message FROM logs ORDER BY id LIMIT 1") or "Completed").split()[0]

E = f"/api/v1/applications/{app_id}/environments/{env_id}"
cases = []


def case(name, path, method="GET", token="full", write=False, **extra):
    c = {"name": name, "method": method, "path": path, "token": token, "write": write}
    c.update(extra)
    cases.append(c)


# -- account level ---------------------------------------------------------------
case("me", "/api/v1/me")
case("me.json_format", "/api/v1/me.json")
case("me.trailing_slash", "/api/v1/me/")
case("me.restricted", "/api/v1/me", token="other_app")
case("me.account_only", "/api/v1/me", token="account_only")
case("accounts", "/api/v1/accounts")
case("accounts.restricted", "/api/v1/accounts", token="other_app")
case("applications", "/api/v1/applications")
case("applications.page2", "/api/v1/applications?per_page=1&page=2")
case("applications.restricted", "/api/v1/applications", token="other_app")
case("application", f"/api/v1/applications/{app_id}")
case("application.restricted_from", f"/api/v1/applications/{app_id}", token="other_app")
case("application.other", f"/api/v1/applications/{other_app}", token="other_app")
case("application.unknown", "/api/v1/applications/99999999")
case("application.abc", "/api/v1/applications/abc")
case("application.1abc", f"/api/v1/applications/{app_id}abc")
case("application.huge", "/api/v1/applications/99999999999999999999")
case("environment", f"/api/v1/applications/{app_id}/environments/{env_id}")
case("environment.unknown", f"/api/v1/applications/{app_id}/environments/99999999")
case("environment.wrong_app", f"/api/v1/applications/{other_app}/environments/{env_id}")
case("environment.restricted_from", f"/api/v1/applications/{app_id}/environments/{env_id}", token="other_app")

# -- environment telemetry ---------------------------------------------------------
for w in ["1h", "24h", "7d", "30d"]:
    case(f"summary@{w}", f"{E}/summary?window={w}")
case("summary.bad_window", f"{E}/summary?window=2w")
case("summary.restricted_from", f"{E}/summary", token="other_app")
case("summary.unknown_env", f"/api/v1/applications/{app_id}/environments/99999999/summary")
for w in ["24h", "7d", "30d"]:
    case(f"requests@{w}", f"{E}/requests?window={w}")
case("requests.sort_p95_asc", f"{E}/requests?window=30d&sort=p95&dir=asc")
case("requests.sort_bad", f"{E}/requests?window=30d&sort=nope")
case("requests.q", f"{E}/requests?window=30d&q=method:get+route:{route_word}")
case("requests.q_5xx", f"{E}/requests?window=30d&q=status:5xx")
case("requests.page2", f"{E}/requests?window=30d&per_page=2&page=2")
case("requests.page_past_end", f"{E}/requests?window=30d&page=999")
case("requests.per_page_float", f"{E}/requests?window=30d&per_page=3.9")
if route_hash:
    case("request_group@7d", f"{E}/requests/{route_hash}?window=7d")
    case("request_group@30d.page2", f"{E}/requests/{route_hash}?window=30d&per_page=5&page=2")
case("request_group.unknown", f"{E}/requests/deadbeef?window=30d")
for label, eid in [("slowest", slowest_req), ("job", job), ("traced", traced), ("with_exception", with_exc)]:
    if eid:
        case(f"execution.{label}", f"{E}/executions/{eid}")
case("execution.unknown", f"{E}/executions/00000000-0000-0000-0000-000000000000")
for w in ["24h", "30d"]:
    case(f"jobs@{w}", f"{E}/jobs?window={w}")
case("jobs.limit", f"{E}/jobs?window=30d&limit=5")
case("jobs.cursor_page2", f"{E}/jobs?window=30d&limit=5", cursor_from="jobs.limit")
case("jobs.failed", f"{E}/jobs?window=30d&q=outcome:failed")
if job_class:
    case("jobs.class", f"{E}/jobs?window=30d&q=class:{job_class}&limit=3&sort=max")
case("jobs.bad_cursor", f"{E}/jobs?window=30d&cursor=forged--0000")
for w in ["24h", "30d"]:
    case(f"queries@{w}", f"{E}/queries?window={w}")
case("queries.limit", f"{E}/queries?window=30d&limit=5&per_page=10&sort=p99")
case("queries.cursor_page2", f"{E}/queries?window=30d&limit=5&per_page=10&sort=p99", cursor_from="queries.limit")
case("queries.adapter", f"{E}/queries?window=30d&q=adapter:sqlite&limit=3")
case("queries.limit_negative", f"{E}/queries?window=7d&limit=-5")
for w in ["24h", "30d"]:
    case(f"exceptions@{w}", f"{E}/exceptions?window={w}")
case("exceptions.page2", f"{E}/exceptions?window=30d&per_page=10&page=2&limit=10")
case("exceptions.cursor_page2", f"{E}/exceptions?window=30d&per_page=10&page=2&limit=10", cursor_from="exceptions.page2")
if exc_class:
    case("exceptions.class", f"{E}/exceptions?window=30d&q=class:{exc_class}")
case("exceptions.cursor_wrong_resource", f"{E}/exceptions?window=30d&limit=10", cursor_from="jobs.limit")
case("exceptions.cursor_other_query", f"{E}/exceptions?window=30d&q=handled:false&per_page=10&page=2&limit=10", cursor_from="exceptions.page2")
for w in ["1h", "30d"]:
    case(f"logs@{w}", f"{E}/logs?window={w}")
case("logs.level", f"{E}/logs?window=30d&q=level:error&per_page=5")
case("logs.text", f"{E}/logs?window=30d&q={log_word}&limit=7")
case("logs.cursor_page2", f"{E}/logs?window=30d&q={log_word}&limit=7", cursor_from="logs.text")
case("logs.limit_huge", f"{E}/logs?window=7d&limit=5000&per_page=1")
case("deploys", f"{E}/deploys")
case("deploys.page", f"{E}/deploys?per_page=2&page=2")
for w in ["24h", "7d", "30d"]:
    case(f"releases@{w}", f"{E}/releases?window={w}")
for w in ["24h", "30d"]:
    case(f"users@{w}", f"{E}/users?window={w}")
case("users.page", f"{E}/users?window=30d&per_page=2&page=2")
for w in ["24h", "30d"]:
    case(f"tenants@{w}", f"{E}/tenants?window={w}")
if tenant_name:
    case("tenants.q", f"{E}/tenants?window=30d&q={tenant_name[:3]}&sort=errors&dir=asc")
case("tenants.sort", f"{E}/tenants?window=30d&sort=jobs")
for w in ["24h", "30d"]:
    case(f"spans@{w}", f"{E}/spans?window={w}")
case("spans.name", f"{E}/spans?window=30d&q=name:{span_word}")
case("spans.failed", f"{E}/spans?window=30d&q=status:failed&sort=max&dir=asc")
for typ in ["request", "job_attempt", "query"]:
    case(f"series.{typ}@24h", f"{E}/series?type={typ}")
case("series.request@1h", f"{E}/series?type=request&window=1h")
case("series.request@7d.6h", f"{E}/series?type=request&window=7d&step=6h")
case("series.request@30d.1d", f"{E}/series?type=request&window=30d&step=1d")
if route_hash:
    case("series.group@7d", f"{E}/series?type=request&window=7d&group_hash={route_hash}")
if query_hash:
    case("series.query_group@30d", f"{E}/series?type=query&window=30d&group_hash={query_hash}")
case("series.no_type", f"{E}/series")
case("series.bad_type", f"{E}/series?type=span")
case("series.bad_step", f"{E}/series?type=request&window=30d&step=1m")
case("series.blank_step", f"{E}/series?type=request&window=1h&step=")

# -- issues ----------------------------------------------------------------------------
case("issues", "/api/v1/issues")
case("issues.page2", "/api/v1/issues?per_page=3&page=2")
case("issues.app", f"/api/v1/issues?application_id={app_id}&per_page=50")
case("issues.env", f"/api/v1/issues?environment_id={env_id}&status=open&kind=exception")
case("issues.status_all_filters", "/api/v1/issues?status=ignored&kind=exception&priority=normal")
case("issues.merged", "/api/v1/issues?status=merged")
case("issues.bad_status", "/api/v1/issues?status=bogus&kind=nope&priority=meh&per_page=5")
case("issues.q", "/api/v1/issues?q=Error")
case("issues.q_percent", "/api/v1/issues?q=100%25")
case("issues.q_underscore", "/api/v1/issues?q=_")
case("issues.app_abc", "/api/v1/issues?application_id=abc")
case("issues.app_1abc", f"/api/v1/issues?application_id={app_id}abc&per_page=2")
case("issues.app_array", f"/api/v1/issues?application_id[]={app_id}&application_id[]=x&per_page=2")
case("issues.assignee_blank", "/api/v1/issues?assignee_id=&per_page=2")
case("issues.assignee_abc", "/api/v1/issues?assignee_id=abc")
case("issues.restricted", "/api/v1/issues", token="other_app")
case("issues.page_negative", "/api/v1/issues?page=-2&per_page=-1")
case("issues.per_page_big", "/api/v1/issues?per_page=10000")
case("issues.page_array", "/api/v1/issues?page[]=1")
case("issues.per_page_array", "/api/v1/issues?per_page[]=1")
case("logs.limit_array", f"{E}/logs?window=7d&limit[]=1")
for i in exc[:3] + perfi[:2] + merged[:1]:
    case(f"issue.{i}", f"/api/v1/issues/{i}")
case("issue.unknown", "/api/v1/issues/99999999")
case("issue.abc", "/api/v1/issues/abc")
if exc:
    case("issue.restricted_from", f"/api/v1/issues/{exc[0]}", token="other_app")
    case("issue.json_format", f"/api/v1/issues/{exc[0]}.json")

if not perf:
    # -- auth ---------------------------------------------------------------------------
    case("auth.none", "/api/v1/me", token=None)
    case("auth.garbage", "/api/v1/me", token=None, authorization="Bearer rwp_nope")
    case("auth.lowercase_bearer", "/api/v1/me", token=None, authorization="bearer rwp_nope")
    case("auth.two_words", "/api/v1/me", token=None, authorization="Bearer a b")
    case("auth.revoked", "/api/v1/accounts", token="revoked")
    case("auth.expired", "/api/v1/accounts", token="expired")
    case("auth.mcp_only", "/api/v1/accounts", token="mcp_only")
    case("auth.no_read", "/api/v1/accounts", token="no_read")
    case("auth.none_on_unknown_env", f"/api/v1/applications/{app_id}/environments/99999999/summary", token=None)
    case("auth.none_on_write", f"/api/v1/issues/{exc[0]}", method="PATCH", token=None, body={"status": "resolved"})
    # -- routing and transport ------------------------------------------------------------
    case("route.unknown", "/api/v1/nope")
    case("route.unknown_json", "/api/v1/nope.json")
    case("route.unknown_accept_json", "/api/v1/nope", headers={"Accept": "application/json"})
    case("route.unknown_browser_accept", "/api/v1/nope.json", headers={"Accept": "text/html, */*"})
    case("route.delete_issue", f"/api/v1/issues/{exc[0]}", method="DELETE")
    case("route.post_accounts", "/api/v1/accounts", method="POST")
    case("route.v2", "/api/v2/me")
    case("route.double_slash", "/api/v1//me")
    case("body.malformed", "/api/v1/accounts", body="{bad")
    case("body.malformed_me", "/api/v1/me", body="{bad")
    case("body.malformed_accept_json", "/api/v1/accounts", body="{bad", headers={"Accept": "application/json"})
    case("body.malformed_no_token", "/api/v1/accounts", token=None, body="{bad")
    case("body.array", "/api/v1/accounts", body="[1,2]")
    case("body.form_page", "/api/v1/issues", body="per_page=1&page=3", content_type="application/x-www-form-urlencoded")
    case("body.query_wins", "/api/v1/issues?per_page=2", body={"per_page": 1})
    case("head.me", "/api/v1/me", method="HEAD")
    case("etag.mismatch", "/api/v1/accounts", headers={"If-None-Match": 'W/"nope"'})
    case("rate_limit", "/api/v1/accounts", burst=600)
    case("rate_limit.under", "/api/v1/me", burst=599)
    # -- writes (last: they change what later cases read) -----------------------------------
    def w(name, path, method, body, token="full", **extra):
        case(f"write.{name}", path, method=method, token=token, write=True, body=body, **extra)

    w("read_only.update", f"/api/v1/issues/{exc[0]}", "PATCH", {"status": "resolved"}, token="read_only")
    w("read_only.comment", f"/api/v1/issues/{exc[0]}/comments", "POST", {"body": "x"}, token="read_only")
    w("read_only.deploy", f"{E}/deploys", "POST", {"deploy": "x"}, token="read_only")
    w("read_only.deploy_unknown_env", f"/api/v1/applications/{app_id}/environments/99999999/deploys", "POST", {"deploy": "x"}, token="read_only")
    w("update.unknown", "/api/v1/issues/99999999", "PATCH", {"status": "resolved"})
    w("update.restricted_from", f"/api/v1/issues/{exc[0]}", "PATCH", {"status": "resolved"}, token="other_app")
    w("update.resolve", f"/api/v1/issues/{exc[0]}", "PATCH", {"status": "resolved", "priority": "high"})
    w("update.resolve_again", f"/api/v1/issues/{exc[0]}", "PUT", {"status": "resolved"})
    w("update.reopen", f"/api/v1/issues/{exc[0]}", "PATCH", {"status": "open"})
    w("update.ignore", f"/api/v1/issues/{exc[1]}", "PATCH", {"status": "ignored", "priority": "low"})
    w("update.assign", f"/api/v1/issues/{exc[1]}", "PATCH", {"assignee_id": member_id})
    w("update.assign_string", f"/api/v1/issues/{exc[1]}", "PATCH", {"assignee_id": str(user_id)})
    w("update.unassign_null", f"/api/v1/issues/{exc[1]}", "PATCH", {"assignee_id": None})
    w("update.assign_outsider", f"/api/v1/issues/{exc[1]}", "PATCH", {"assignee_id": 99999999})
    w("update.bad_status_merged", f"/api/v1/issues/{exc[2]}", "PATCH", {"status": "merged"})
    w("update.bad_status_then_priority", f"/api/v1/issues/{exc[2]}", "PATCH", {"status": "nope", "priority": "urgent"})
    w("update.priority_then_bad_assignee", f"/api/v1/issues/{exc[2]}", "PATCH", {"priority": "urgent", "assignee_id": 99999999})
    w("update.bad_priority", f"/api/v1/issues/{exc[2]}", "PATCH", {"priority": "whenever"})
    w("update.noop", f"/api/v1/issues/{exc[2]}", "PATCH", {})
    w("update.form", f"/api/v1/issues/{exc[2]}", "PATCH", "priority=low", content_type="application/x-www-form-urlencoded")
    if merged:
        w("update.unmerge", f"/api/v1/issues/{merged[0]}", "PATCH", {"status": "open"})
    if perfi:
        w("update.perf_resolve", f"/api/v1/issues/{perfi[0]}", "PATCH", {"status": "resolved"})
    w("comment", f"/api/v1/issues/{exc[0]}/comments", "POST", {"body": "Looks like a <b>N+1</b> & friends"})
    w("comment.blank", f"/api/v1/issues/{exc[0]}/comments", "POST", {"body": "  "})
    w("comment.missing", f"/api/v1/issues/{exc[0]}/comments", "POST", {})
    w("comment.number", f"/api/v1/issues/{exc[0]}/comments", "POST", {"body": 42})
    w("comment.unknown", "/api/v1/issues/99999999/comments", "POST", {"body": "x"})
    w("comment.restricted_from", f"/api/v1/issues/{exc[0]}/comments", "POST", {"body": "x"}, token="other_app")
    w("deploy.new", f"{E}/deploys", "POST", {"deploy": "api-diff-new", "ref": "cccc333", "name": "Release C", "url": "https://ci.example.com/c"})
    w("deploy.repost", f"{E}/deploys", "POST", {"deploy": "api-diff-a", "ref": "dddd444"})
    w("deploy.long", f"{E}/deploys", "POST", {"deploy": "x" * 200, "name": "n" * 300, "ref": "  "})
    w("deploy.missing", f"{E}/deploys", "POST", {"ref": "abc"})
    w("deploy.blank", f"{E}/deploys", "POST", {"deploy": "  "})
    w("deploy.bad_url", f"{E}/deploys", "POST", {"deploy": "api-diff-bad", "url": "javascript:alert(1)"})
    w("deploy.hostless_url", f"{E}/deploys", "POST", {"deploy": "api-diff-bad", "url": "https://?x"})
    w("deploy.restricted_from", f"{E}/deploys", "POST", {"deploy": "x"}, token="other_app")
    # after the writes: the reads that show them
    after = [("issue", f"/api/v1/issues/{exc[0]}"), ("issue_assigned", f"/api/v1/issues/{exc[1]}"),
             ("issues_ignored", "/api/v1/issues?status=ignored"), ("deploys", f"{E}/deploys"),
             ("releases", f"{E}/releases?window=30d")]
    if merged:
        after.append(("unmerged", f"/api/v1/issues/{merged[0]}"))
    for name, path in after:
        case(f"after_writes.{name}", path, write=True)

json.dump(cases, open(out, "w"), indent=1)
print(f"{len(cases)} cases ({sum(1 for c in cases if c.get('write'))} write) for app {app_id} env {env_id}")
