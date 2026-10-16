#!/usr/bin/env python3
"""The props-diff cases: every environment page, each under a few windows,
plus filters, sorts and a second cursor page where the page has them. Ids
(group hashes, execution ids, a trace, a person, a tenant, a deploy, a
profile) are picked from the tenant file so the show pages have data.

    make_cases.py TENANT.sqlite3 CONTROL.json OUT.json [--quick]

A case: {name, page, path, params, path_params}. `path` is the Rails URL
(with {base} for /apps/:a/envs/:e); `page` the Rust function;
`path_params` what Rails reads from the route.
"""
import json, sqlite3, sys

tenant, control_file, out = sys.argv[1:4]
quick = "--quick" in sys.argv
db = sqlite3.connect(f"file:{tenant}?mode=ro", uri=True)
control = json.load(open(control_file)) if control_file != "-" else {"deploys": []}

def one(sql, *args):
    r = db.execute(sql, args).fetchone()
    return r[0] if r else None

WINDOWS = ["1h"] if quick else ["1h", "24h", "7d"]
cases = []

def add(name, page, path, params=None, path_params=None, windows=WINDOWS):
    for w in windows:
        p = dict(params or {})
        if w:
            p["window"] = w
        cases.append({"name": f"{name}@{w}", "page": page, "path": path, "params": p, "path_params": path_params or {}})

route = one("SELECT group_hash FROM rollups WHERE record_type='request' GROUP BY group_hash ORDER BY SUM(count) DESC LIMIT 1")
job = one("SELECT group_hash FROM rollups WHERE record_type='job_attempt' GROUP BY group_hash ORDER BY SUM(count) DESC LIMIT 1")
query = one("SELECT group_hash FROM rollups WHERE record_type='query' GROUP BY group_hash ORDER BY SUM(count) DESC LIMIT 1")
span = one("SELECT group_hash FROM rollups WHERE record_type='span' LIMIT 1")
exe_req = one("SELECT execution_id FROM executions WHERE kind='request' AND exception_preview IS NOT NULL ORDER BY occurred_at DESC LIMIT 1")
exe_job = one("SELECT execution_id FROM executions WHERE kind='job_attempt' ORDER BY occurred_at DESC LIMIT 1")
exe_prof = one("SELECT execution_id FROM profiles ORDER BY occurred_at DESC LIMIT 1")
trace = one("SELECT trace_id FROM executions WHERE trace_id IS NOT NULL ORDER BY occurred_at DESC LIMIT 1")
person = one("SELECT user_ref FROM executions WHERE user_ref IS NOT NULL GROUP BY user_ref ORDER BY COUNT(*) DESC LIMIT 1")
ten = one("SELECT app_tenant FROM executions WHERE app_tenant IS NOT NULL GROUP BY app_tenant ORDER BY COUNT(*) DESC LIMIT 1")
profile = one("SELECT id FROM profiles ORDER BY id DESC LIMIT 1")
deploys = sorted(control.get("deploys", []), key=lambda d: d["deployed_at"])
dep = deploys[-2] if len(deploys) > 1 else (deploys[0] if deploys else None)

B = "{base}"
add("overview", "overview_show", f"{B}")
add("overview.parallel", "overview_show_parallel", f"{B}")
add("requests", "requests_index", f"{B}/requests")
add("requests.sort_p95_asc", "requests_index", f"{B}/requests", {"sort": "p95", "dir": "asc"})
add("requests.q_5xx_get", "requests_index", f"{B}/requests", {"q": "status:5xx method:get"}, windows=["24h"])
add("requests.step_15m", "requests_index", f"{B}/requests", {"step": "15m"}, windows=["24h"])
add("requests.custom", "requests_index", f"{B}/requests", {"from": "CUSTOM_FROM", "to": "CUSTOM_TO"}, windows=[None])
if route:
    add("requests.route", "requests_route", f"{B}/requests/routes/{route}", path_params={"group_hash": route})
add("jobs", "jobs_index", f"{B}/jobs")
add("jobs.q_failed", "jobs_index", f"{B}/jobs", {"q": "status:failed"}, windows=["7d"])
add("jobs.q_class", "jobs_index", f"{B}/jobs", {"q": "class:ReportJob"}, windows=["7d"])
add("jobs.q_text", "jobs_index", f"{B}/jobs", {"q": "Timeout"}, windows=["7d"])
add("jobs.limit_cursor", "jobs_index", f"{B}/jobs", {"limit": "5"}, windows=["7d"])
if job:
    add("jobs.klass", "jobs_klass", f"{B}/jobs/classes/{job}", path_params={"group_hash": job})
add("scheduled_tasks", "scheduled_tasks_index", f"{B}/scheduled_tasks")
add("commands", "commands_index", f"{B}/commands")
for label, e in [("req", exe_req), ("job", exe_job), ("profiled", exe_prof)]:
    if e:
        add(f"executions.{label}", "executions_show", f"{B}/executions/{e}", path_params={"id": e}, windows=["24h"])
if trace:
    add("traces", "traces_show", f"{B}/traces/{trace}", path_params={"trace_id": trace}, windows=["24h"])
add("people", "people_index", f"{B}/users")
if person:
    add("people.show", "people_show", f"{B}/users/{person}", path_params={"id": person})
add("tenants", "tenants_index", f"{B}/tenants")
add("tenants.sort_p95", "tenants_index", f"{B}/tenants", {"sort": "p95", "q": "e"}, windows=["7d"])
if ten:
    add("tenants.show", "tenants_show", f"{B}/tenants/{ten.replace('.', '%2E')}", path_params={"id": ten})
add("queries", "queries_index", f"{B}/queries")
add("queries.parallel", "queries_index_parallel", f"{B}/queries")
add("queries.parallel_cursor", "queries_index_parallel", f"{B}/queries", {"limit": "7", "q": "widgets"}, windows=["7d"])
add("queries.sort_p99", "queries_index", f"{B}/queries", {"sort": "p99"}, windows=["7d"])
add("queries.q", "queries_index", f"{B}/queries", {"q": "widgets role:reading"}, windows=["7d"])
add("queries.limit_cursor", "queries_index", f"{B}/queries", {"limit": "7"}, windows=["7d"])
if query:
    add("queries.show", "queries_show", f"{B}/queries/{query}", path_params={"id": query})
add("exceptions", "exceptions_index", f"{B}/exceptions")
add("exceptions.q", "exceptions_index", f"{B}/exceptions", {"q": "status:unhandled kaboom"}, windows=["7d"])
add("logs", "logs_index", f"{B}/logs")
add("logs.fts", "logs_index", f"{B}/logs", {"q": "complet -started level:info"}, windows=["7d"])
add("logs.limit_cursor", "logs_index", f"{B}/logs", {"limit": "10", "q": "level:warn"}, windows=["7d"])
add("spans", "spans_index", f"{B}/spans")
add("spans.q", "spans_index", f"{B}/spans", {"q": "status:failed name:pdf"}, windows=["7d"])
if span:
    add("spans.show", "spans_show", f"{B}/spans/{span}", path_params={"id": span})
add("profiles", "profiles_index", f"{B}/profiles")
if profile:
    add("profiles.show", "profiles_show", f"{B}/profiles/{profile}", path_params={"id": str(profile)}, windows=["24h"])
add("view_renders", "view_renders_index", f"{B}/views")
add("transactions", "transactions_index", f"{B}/transactions")
add("deprecations", "deprecations_index", f"{B}/deprecations")
add("cache_events", "cache_events_index", f"{B}/cache")
add("cache_events.store", "cache_events_index", f"{B}/cache", {"q": "store:MemoryStore"}, windows=["7d"])
add("mails", "mails_index", f"{B}/mail")
add("notifications", "notifications_index", f"{B}/notifications")
add("broadcasts", "broadcasts_index", f"{B}/broadcasts")
add("outgoing_requests", "outgoing_requests_index", f"{B}/outgoing_requests")
add("outgoing_requests.q", "outgoing_requests_index", f"{B}/outgoing_requests", {"q": "host:stripe status:5xx"}, windows=["7d"])
add("llm_calls", "llm_calls_index", f"{B}/llm")
add("llm_calls.q", "llm_calls_index", f"{B}/llm", {"q": "model:opus attachments:image"}, windows=["7d"])
add("storage_ops", "storage_ops_index", f"{B}/storage")
add("visits", "visits_index", f"{B}/visits")
add("processes", "processes_index", f"{B}/processes")
add("releases", "releases_index", f"{B}/releases")
if dep:
    add("releases.show", "releases_show", f"{B}/releases/{dep['deploy']}", path_params={"id": dep["deploy"]}, windows=["24h"])
    add("deploys.show", "deploys_show", f"{B}/deploys/{dep['id']}", path_params={"id": str(dep["id"])}, windows=["24h"])
add("deploys", "deploys_index", f"{B}/deploys", windows=["24h"])
add("thresholds", "thresholds_index", f"{B}/thresholds", windows=["24h"])
json.dump(cases, open(out, "w"), indent=1)
print(f"{len(cases)} cases")
