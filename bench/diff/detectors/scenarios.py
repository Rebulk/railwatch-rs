"""Writes the detector scenarios both rails_runner.rb and rw-detectors replay.

  python3 -I scenarios.py OUT.json

A scenario is a fresh account (owner Ada, member Grace), one application
"Shop" (issue prefix DET), one environment "production", webhook
integrations, alert rules, thresholds and anomaly rules, then a timeline of
steps, each at a fixed UTC time (both sides freeze their clock there):

  tenant_sql  statements run against the environment's telemetry file
  deploy      a deploys row {deploy, ref, deployed_at}
  usage       ingest_usage_months for the environment {month, events}
  env         environments columns {last_seen_at, paused_at, crash_free_threshold, expected_servers}
  account     accounts columns {monthly_event_quota, additional_events_cap, auto_resolve_after_days}
  scan        one recurring.yml job: performance_scan, anomaly_scan, silent_host_check,
              scheduled_task_check, release_health_check, quota_check, auto_resolve_issues
  group       ExceptionGrouping over exception ids
  issue       {number, action: resolve|ignore|reopen|assign|priority, deploy?, value?}
  deliver     the alert outbox: reconcile + every due delivery / burst summary

Telemetry is written as rows (rollups with hand-built t-digests), so the
detectors read identical bytes on both sides and no RNG is involved.
"""
import hashlib, json, struct, sys

OUT = sys.argv[1]
DAY = "2026-10-14"


def t(hms, day=DAY):
    return f"{day}T{hms}Z"


def db(hms, day=DAY):
    return f"{day} {hms}"


def gh(*parts):
    return hashlib.md5(",".join(parts).encode()).hexdigest()


def lit(v):
    if v is None:
        return "NULL"
    if isinstance(v, bool):
        return "1" if v else "0"
    if isinstance(v, (int, float)):
        return repr(v)
    if isinstance(v, bytes):
        return "X'" + v.hex() + "'"
    if isinstance(v, (dict, list)):
        v = json.dumps(v, separators=(",", ":"))
    return "'" + str(v).replace("'", "''") + "'"


def ins(table, row):
    return f'INSERT INTO "{table}" ({", ".join(chr(34) + k + chr(34) for k in row)}) VALUES ({", ".join(lit(v) for v in row.values())})'


def digest(centroids):
    """tdigest SMALL encoding (format 2): f32 mean deltas, varint weights."""
    out = struct.pack("<IdI", 2, 100.0, len(centroids))
    x = 0.0
    for mean, _ in centroids:
        out += struct.pack("<f", mean - x)
        x = mean
    for _, n in centroids:
        while n > 0x7F:
            out += bytes([0x80 | (n & 0x7F)])
            n >>= 7
        out += bytes([n])
    return out


def rollup(record_type, name, bucket, centroids, errors=0, extra=None, group=None):
    count = sum(n for _, n in centroids)
    return ins("rollups", {
        "record_type": record_type, "name": name, "group_hash": group or gh(name), "bucket": bucket,
        "count": count, "error_count": errors, "client_error_count": 0,
        "duration_sum": int(sum(m * n for m, n in centroids)), "duration_max": int(max(m for m, _ in centroids)),
        "p50": 0, "p95": 0, "p99": 0, "digest": digest(centroids), "extra": extra or {},
    })


EXEC_ID = [0]


def execution(kind, name, at, server="web-1", task_key=None, schedule=None, group=None, duration=1000, deploy=None):
    detail = {"schedule": schedule} if schedule else {}
    EXEC_ID[0] += 1
    return ins("executions", {
        "kind": kind, "name": name, "occurred_at": at, "duration": duration, "server": server, "task_key": task_key,
        "group_hash": group or gh(task_key or name), "detail": detail, "deploy": deploy, "execution_id": f"x-{EXEC_ID[0]}",
        "status": 200 if kind == "request" else None,
    })


def requests(name, n, minutes=(59, 60, 61, 62, 63), deploy="v1"):
    """n requests of `name` spread over `minutes` (of hour 11; 60+ is hour
    12), equal durations in pairs (representative tie order), plus one
    before the window and one after it."""
    out = []
    for i in range(n):
        m = minutes[i % len(minutes)]
        at = db(f"{11 + m // 60}:{m % 60:02d}:{i % 60:02d}.{i:06d}")
        out.append(execution("request", name, at, duration=1_000_000 + (i // 2) * 1000, deploy=deploy if i % 3 else "v0"))
    out.append(execution("request", name, db("11:40:00"), duration=9_000_000, deploy=deploy))
    out.append(execution("request", name, db("12:30:00"), duration=9_000_000, deploy=deploy))
    return out


def health(at, server="web-1", detail=None):
    return ins("health_samples", {"sampled_at": at, "server": server, "role": "web", "detail": detail or {}})


def release(bucket, deploy, sessions, crashed, errored=0):
    return ins("release_health", {"bucket": bucket, "deploy": deploy, "sessions": sessions, "sessions_crashed": crashed,
                                  "sessions_errored": errored, "users": sessions, "users_crashed": crashed,
                                  "duration_sum": sessions * 1000, "duration_count": sessions})


EXC_ID = [0]


def exception(cls, message, at, deploy=None, user=None, source="application", group=None):
    EXC_ID[0] += 1
    return ins("exceptions", {
        "id": EXC_ID[0], "class_name": cls, "message": message, "occurred_at": at, "deploy": deploy, "user_ref": user,
        "source": source, "group_hash": group or gh(cls, message), "handled": False, "fingerprint": [],
        "execution_id": f"exec-{EXC_ID[0]}", "file": "app/models/order.rb", "line": 12,
    }), EXC_ID[0]


SCENARIOS = []


def scenario(name, detector, steps, rules=(), integrations=None, thresholds=(), anomaly_rules=(), env=None, account=None):
    EXC_ID[0] = 0
    SCENARIOS.append({
        "name": name, "detector": detector,
        "account": {"monthly_event_quota": 1_000_000, "additional_events_cap": 0, "auto_resolve_after_days": None, **(account or {})},
        "env": {"last_seen_at": db("11:59:00"), "crash_free_threshold": 99.0, "expected_servers": [], "paused_at": None, **(env or {})},
        "integrations": integrations or [{"name": "hook", "path": "ok", "enabled": True}],
        "rules": [{"event": r[0], "integration": r[1] if len(r) > 1 else 0, "filters": r[2] if len(r) > 2 else {}} for r in rules],
        "thresholds": list(thresholds), "anomaly_rules": list(anomaly_rules), "steps": steps,
    })


def sql(at, *stmts):
    return {"at": at, "do": "tenant_sql", "sql": list(stmts)}


def scan(at, name):
    return {"at": at, "do": "scan", "scan": name}


def deliver(at):
    return {"at": at, "do": "deliver"}


def issue(at, number, action, **kw):
    return {"at": at, "do": "issue", "number": number, "action": action, **kw}


def thr(kind, metric, limit, target="*", window=5):
    return {"target_kind": kind, "metric": metric, "limit": limit, "target": target, "window_minutes": window}


def anom(kind, metric, target="*", deviation=3.0, window=15, days=7, enabled=True):
    return {"target_kind": kind, "metric": metric, "target": target, "deviation": deviation, "window_minutes": window,
            "baseline_days": days, "enabled": enabled}


# ---------------------------------------------------------------- thresholds
SLOW = [(150_000, 90), (2_500_000, 10)]
H12 = db("12:00:00")
scenario("threshold_p95_fires", "threshold", [
    sql(t("12:00:00"), rollup("request", "GET /checkout", H12, SLOW), *requests("GET /checkout", 14)),
    scan(t("12:04:00"), "performance_scan"), deliver(t("12:04:00")),
], rules=[("threshold",)], thresholds=[thr("requests", "p95", 2000)])
scenario("threshold_at_limit_silent", "threshold", [
    sql(t("12:00:00"), rollup("request", "GET /checkout", H12, [(2_000_000, 100)])),
    scan(t("12:04:00"), "performance_scan"), deliver(t("12:04:00")),
], rules=[("threshold",)], thresholds=[thr("requests", "p95", 2000)])
scenario("threshold_below_silent", "threshold", [
    sql(t("12:00:00"), rollup("request", "GET /checkout", H12, [(150_000, 100)])),
    scan(t("12:04:00"), "performance_scan"),
], rules=[("threshold",)], thresholds=[thr("requests", "p95", 2000)])
scenario("threshold_error_rate", "threshold", [
    sql(t("12:00:00"), rollup("request", "GET /pay", H12, [(100_000, 100)], errors=6), rollup("request", "GET /ok", H12, [(100_000, 100)], errors=5)),
    scan(t("12:04:00"), "performance_scan"), deliver(t("12:04:00")),
], rules=[("threshold",)], thresholds=[thr("requests", "error_rate", 5)])
scenario("threshold_lifecycle_dedupe_regress", "threshold", [
    sql(t("12:00:00"), rollup("request", "GET /checkout", H12, SLOW)),
    scan(t("12:04:00"), "performance_scan"), deliver(t("12:04:00")),
    scan(t("12:09:00"), "performance_scan"),
    issue(t("12:10:00"), 1, "resolve"), deliver(t("12:10:00")),
    scan(t("12:14:00"), "performance_scan"), deliver(t("12:14:00")),
    issue(t("12:40:00"), 1, "resolve"),
    scan(t("12:44:00"), "performance_scan"), deliver(t("12:44:00")),
], rules=[("threshold",), ("resolved_issue",)], thresholds=[thr("requests", "p95", 2000)])
scenario("threshold_target_and_jobs_max", "threshold", [
    sql(t("12:00:00"), rollup("request", "GET /a", H12, SLOW), rollup("request", "GET /b", H12, SLOW),
        rollup("job_attempt", "HardJob", H12, [(1_000_000, 50), (3_500_000, 1)]), rollup("job_attempt", "EasyJob", H12, [(3_000_000, 5)])),
    scan(t("12:04:00"), "performance_scan"), deliver(t("12:04:00")),
], rules=[("threshold",)], thresholds=[thr("requests", "p95", 1000, target="GET /a"), thr("jobs", "max", 3000)])
scenario("threshold_llm_spend_tokens_truncation", "threshold", [
    sql(t("12:00:00"), rollup("llm_call", "openai gpt-5", H12, [(900_000, 100)],
                              extra={"cost_nanos": 5_250_000_000, "input_tokens": 900, "output_tokens": 600, "truncated": 3})),
    scan(t("12:04:00"), "performance_scan"), deliver(t("12:04:00")),
], rules=[("threshold",)], thresholds=[thr("llm_calls", "spend", 4.5), thr("llm_calls", "tokens", 1000), thr("llm_calls", "truncation_rate", 2)])
scenario("threshold_rule_filters", "threshold", [
    sql(t("12:00:00"), rollup("request", "GET /checkout", H12, SLOW)),
    scan(t("12:04:00"), "performance_scan"), deliver(t("12:04:00")),
], rules=[("threshold", 0, {"environment_ids": ["999"]}), ("threshold", 0, {"kinds": ["exception"]}),
          ("threshold", 0, {"kinds": ["performance"], "environment_ids": ["", "1"]}), ("threshold", 0, {"min_priority": "high"}),
          ("threshold", 0, {"title_pattern": "/check.ut/"}), ("threshold", 0, {"title_pattern": "CHECKOUT"}),
          ("threshold", 0, {"title_pattern": "/(unclosed/"}), ("threshold", 0, {"min_priority": "bogus"})],
    thresholds=[thr("requests", "p95", 2000)])
scenario("threshold_paused_env_silent", "threshold", [
    sql(t("12:00:00"), rollup("request", "GET /checkout", H12, SLOW)),
    scan(t("12:04:00"), "performance_scan"),
], rules=[("threshold",)], thresholds=[thr("requests", "p95", 2000)], env={"paused_at": db("10:00:00")})
scenario("threshold_window_hour_floor", "threshold", [
    sql(t("12:00:00"), rollup("request", "GET /old", db("10:00:00"), SLOW)),
    scan(t("12:04:00"), "performance_scan"), deliver(t("12:04:00")),
], rules=[("threshold",)], thresholds=[thr("requests", "p95", 2000, window=5), thr("requests", "max", 2000, window=130)])
scenario("threshold_avg", "threshold", [
    sql(t("12:00:00"), rollup("request", "GET /avg", H12, [(1_000_001, 2), (2_000_000, 1)]), rollup("request", "GET /avg2", db("11:00:00"), [(1_500_000, 3)])),
    scan(t("12:04:00"), "performance_scan"), deliver(t("12:04:00")),
], rules=[("threshold",)], thresholds=[thr("requests", "avg", 1200, window=90)])


# ---------------------------------------------------------------- anomalies
def series(record_type, name, hours, per_day, current, day0=14, errors=None, extra=None):
    """Rollups for `hours` on day0 (current) and the 7 days before (per_day[i] = day0-1-i)."""
    out = []
    for i, c in enumerate(per_day):
        if c is None:
            continue
        for h in hours:
            out.append(rollup(record_type, name, f"2026-10-{day0 - 1 - i:02d} {h}:00:00", c, errors=(errors or {}).get(i, 0),
                              extra=(extra or {}).get(i)))
    for h in hours:
        out.append(rollup(record_type, name, f"2026-10-{day0:02d} {h}:00:00", current, errors=(errors or {}).get("cur", 0),
                          extra=(extra or {}).get("cur")))
    return out


def p95_of(ms):
    return [(ms * 1000, 100)]


BASE = [p95_of(v) for v in (200, 210, 190, 205, 195, 200, 210)]
scenario("anomaly_p95_spike", "anomaly", [
    sql(t("12:00:00"), *series("request", "GET /a", ["11", "12"], BASE, p95_of(800)), *requests("GET /a", 25, minutes=(56, 58, 65, 69))),
    scan(t("12:10:00"), "anomaly_scan"), deliver(t("12:10:00")),
], rules=[("anomaly",)], anomaly_rules=[anom("requests", "p95")])
scenario("anomaly_flat_baseline_and_125pct", "anomaly", [
    sql(t("12:00:00"), *series("request", "GET /flat", ["11", "12"], [p95_of(200)] * 7, p95_of(260)),
        *series("request", "GET /meh", ["11", "12"], [p95_of(200)] * 7, p95_of(240))),
    scan(t("12:10:00"), "anomaly_scan"), deliver(t("12:10:00")),
], rules=[("anomaly",)], anomaly_rules=[anom("requests", "p95")])
scenario("anomaly_two_baseline_days_silent", "anomaly", [
    sql(t("12:00:00"), *series("request", "GET /a", ["11", "12"], [p95_of(200), p95_of(210)] + [None] * 5, p95_of(900))),
    scan(t("12:10:00"), "anomaly_scan"),
], rules=[("anomaly",)], anomaly_rules=[anom("requests", "p95")])
scenario("anomaly_count_min_events", "anomaly", [
    sql(t("12:00:00"), *series("request", "GET /tiny", ["12"], [[(1000, 1)]] * 7, [(1000, 19)]),
        *series("request", "GET /busy", ["12"], [[(1000, n)] for n in (30, 32, 28, 31, 29, 30, 30)], [(1000, 200)])),
    scan(t("12:10:00"), "anomaly_scan"), deliver(t("12:10:00")),
], rules=[("anomaly",)], anomaly_rules=[anom("requests", "count")])
scenario("anomaly_cooldown_seen_regress", "anomaly", [
    sql(t("12:00:00"), *series("request", "GET /a", ["11", "12", "13", "14"], BASE, p95_of(800))),
    scan(t("12:10:00"), "anomaly_scan"), deliver(t("12:10:00")),
    scan(t("12:40:00"), "anomaly_scan"),
    scan(t("13:15:00"), "anomaly_scan"), deliver(t("13:15:00")),
    issue(t("13:20:00"), 1, "resolve"),
    scan(t("14:25:00"), "anomaly_scan"), deliver(t("14:25:00")),
], rules=[("anomaly",)], anomaly_rules=[anom("requests", "p95")])
scenario("anomaly_disabled_silent", "anomaly", [
    sql(t("12:00:00"), *series("request", "GET /a", ["11", "12"], BASE, p95_of(800))),
    scan(t("12:10:00"), "anomaly_scan"),
], rules=[("anomaly",)], anomaly_rules=[anom("requests", "p95", enabled=False)])
scenario("anomaly_error_rate", "anomaly", [
    sql(t("12:00:00"), *series("request", "GET /err", ["12"], [[(1000, 100)]] * 7, [(1000, 100)],
                               errors={0: 1, 1: 2, 2: 1, 3: 0, 4: 1, 5: 2, 6: 1, "cur": 20})),
    scan(t("12:10:00"), "anomaly_scan"), deliver(t("12:10:00")),
], rules=[("anomaly",)], anomaly_rules=[anom("requests", "error_rate")])
scenario("anomaly_llm_spend", "anomaly", [
    sql(t("12:00:00"), *series("llm_call", "openai gpt-5", ["12"], [[(1000, 10)]] * 7, [(1000, 10)],
                               extra={**{i: {"cost_nanos": 1_000_000_000 + i * 10_000_000} for i in range(7)}, "cur": {"cost_nanos": 3_333_333_333}})),
    scan(t("12:10:00"), "anomaly_scan"), deliver(t("12:10:00")),
], rules=[("anomaly",)], anomaly_rules=[anom("llm_calls", "spend")])
scenario("anomaly_above_sigma_below_125pct_silent", "anomaly", [
    sql(t("12:00:00"), *series("request", "GET /tight", ["11", "12"], [p95_of(v) for v in (200, 201, 199, 200, 200, 201, 199)], p95_of(240))),
    scan(t("12:10:00"), "anomaly_scan"),
], rules=[("anomaly",)], anomaly_rules=[anom("requests", "p95")])
scenario("anomaly_window_across_hours", "anomaly", [
    sql(t("12:00:00"), *series("request", "GET /a", ["10", "11", "12"], BASE, p95_of(700))),
    scan(t("12:10:00"), "anomaly_scan"), deliver(t("12:10:00")),
], rules=[("anomaly",)], anomaly_rules=[anom("requests", "p95", window=90, deviation=2.5, days=5)])


# ---------------------------------------------------------------- scheduled tasks
def task_run(key, at, schedule="every 5 minutes", rollup_bucket=None):
    out = [execution("scheduled_task", key, at, server="worker-1", task_key=key, schedule=schedule)]
    if rollup_bucket:
        out.append(rollup("scheduled_task", key, rollup_bucket, [(1000, 1)], group=gh(key)))
    return out


scenario("missed_task_fires", "missed", [
    sql(t("12:00:00"), *task_run("cleanup", db("11:43:45.123456"), rollup_bucket=db("11:00:00")),
        *task_run("fresh_task", db("11:58:00"))),
    scan(t("12:00:00"), "scheduled_task_check"), deliver(t("12:00:00")),
], rules=[("threshold",)])
scenario("missed_task_heals_then_boundary", "missed", [
    sql(t("12:00:00"), *task_run("cleanup", db("11:43:45"))),
    scan(t("12:00:00"), "scheduled_task_check"), deliver(t("12:00:00")),
    sql(t("12:08:00"), *task_run("cleanup", db("12:08:00"))),
    scan(t("12:10:00"), "scheduled_task_check"), deliver(t("12:10:00")),
    scan(t("12:15:00"), "scheduled_task_check"), deliver(t("12:15:00")),
    scan(t("12:15:01"), "scheduled_task_check"), deliver(t("12:15:01")),
], rules=[("threshold",), ("resolved_issue",)])
scenario("missed_task_deploy_grace", "missed", [
    sql(t("12:00:00"), *task_run("near_deploy", db("11:40:00")), *task_run("before_grace", db("11:27:00"), schedule="*/10 * * * *")),
    {"at": t("12:00:00"), "do": "deploy", "deploy": "d1", "ref": "abcdef1234567", "deployed_at": db("11:50:00")},
    scan(t("12:00:00"), "scheduled_task_check"), deliver(t("12:00:00")),
], rules=[("threshold",)])
scenario("missed_task_manifest_removal", "missed", [
    sql(t("12:00:00"), *task_run("removed", db("11:40:00")), *task_run("kept", db("11:40:00"))),
    scan(t("12:00:00"), "scheduled_task_check"), deliver(t("12:00:00")),
    sql(t("12:20:00"), health(db("12:19:00"), server="worker-1", detail={"recurring_tasks": {"kept": {"schedule": "every 5 minutes"}}})),
    scan(t("12:20:00"), "scheduled_task_check"), deliver(t("12:20:00")),
], rules=[("threshold",), ("resolved_issue",)])
scenario("missed_task_schedule_forms", "missed", [
    sql(t("12:00:00"), *task_run("cron10", db("11:31:00"), schedule="*/10 * * * *"),
        *task_run("daily3am", db("03:00:05", "2026-10-12"), schedule="every day at 3am"),
        *task_run("utc4", db("04:00:00", "2026-10-13"), schedule="0 4 * * * UTC"),
        *task_run("hourly", db("10:00:00"), schedule="every hour"),
        *task_run("monday8", db("08:00:00", "2026-10-05"), schedule="every monday at 8am"),
        *task_run("garbage", db("09:00:00"), schedule="whenever you like"),
        *task_run("blank", db("09:00:00"), schedule="  ")),
    scan(t("12:00:00"), "scheduled_task_check"), deliver(t("12:00:00")),
], rules=[("threshold",)])
scenario("missed_task_still_missed_seen", "missed", [
    sql(t("12:00:00"), *task_run("cleanup", db("11:40:00"))),
    scan(t("12:00:00"), "scheduled_task_check"), deliver(t("12:00:00")),
    scan(t("12:10:00"), "scheduled_task_check"), deliver(t("12:10:00")),
], rules=[("threshold",)])
scenario("missed_task_stale_env_silent", "missed", [
    sql(t("12:00:00"), *task_run("cleanup", db("11:40:00"))),
    scan(t("12:00:00"), "scheduled_task_check"),
], rules=[("threshold",)], env={"last_seen_at": db("12:00:00", "2026-10-13")})


# ---------------------------------------------------------------- silent hosts
SERVERS = {"expected_servers": ["web-1", "web-2", "worker-1"]}
REPORTING = [health(db("11:58:00"), "web-1"), execution("job_attempt", "SomeJob", db("11:55:00"), server="worker-1")]
scenario("silent_host_fires", "silent_host", [
    sql(t("12:00:00"), *REPORTING), scan(t("12:00:00"), "silent_host_check"), deliver(t("12:00:00")),
], rules=[("silent_host",)], env=SERVERS)
scenario("silent_host_all_reporting", "silent_host", [
    sql(t("12:00:00"), *REPORTING, execution("request", "GET /", db("11:50:00"), server="web-2")),
    scan(t("12:00:00"), "silent_host_check"),
], rules=[("silent_host",)], env=SERVERS)
scenario("silent_host_cooldown_6h", "silent_host", [
    sql(t("12:00:00"), *REPORTING), scan(t("12:00:00"), "silent_host_check"), deliver(t("12:00:00")),
    scan(t("12:05:00"), "silent_host_check"),
    {"at": t("18:00:00"), "do": "env", "last_seen_at": db("17:59:00")},
    scan(t("18:00:00"), "silent_host_check"),
    scan(t("18:00:01"), "silent_host_check"), deliver(t("18:00:01")),
], rules=[("silent_host",)], env=SERVERS)
scenario("silent_host_rule_env_filter", "silent_host", [
    sql(t("12:00:00"), *REPORTING), scan(t("12:00:00"), "silent_host_check"), deliver(t("12:00:00")),
], rules=[("silent_host", 0, {"environment_ids": [2]}), ("silent_host", 0, {"environment_ids": ["1"]})], env=SERVERS)
scenario("silent_host_no_expected_silent", "silent_host", [
    scan(t("12:00:00"), "silent_host_check"),
], rules=[("silent_host",)])
scenario("silent_host_last_seen_24h_boundary", "silent_host", [
    sql(t("12:00:00"), health(db("11:58:00"), "web-1")),
    scan(t("12:00:00"), "silent_host_check"), deliver(t("12:00:00")),
], rules=[("silent_host",)], env={"expected_servers": ["web-1", "web-2"], "last_seen_at": db("12:00:00", "2026-10-13")})


# ---------------------------------------------------------------- release health
def deploys(*ds):
    return [{"at": t("12:00:00"), "do": "deploy", "deploy": d, "ref": r, "deployed_at": at} for d, r, at in ds]


D12 = deploys(("d1", "1111111aaaa", db("08:00:00")), ("d2", "2222222bbbb", db("11:00:00")))
scenario("crash_free_drop_fires", "release_health", [
    *D12, sql(t("12:00:00"), release(db("11:00:00"), "d2", 30, 1), release(db("12:00:00"), "d2", 20, 1), release(db("12:00:00"), "d1", 100, 50)),
    scan(t("12:10:00"), "release_health_check"), deliver(t("12:10:00")),
], rules=[("crash_free_drop",)])
scenario("crash_free_few_sessions_silent", "release_health", [
    *D12, sql(t("12:00:00"), release(db("12:00:00"), "d2", 19, 5)),
    scan(t("12:10:00"), "release_health_check"),
], rules=[("crash_free_drop",)])
scenario("crash_free_at_threshold_silent", "release_health", [
    *D12, sql(t("12:00:00"), release(db("12:00:00"), "d2", 100, 1)),
    scan(t("12:10:00"), "release_health_check"),
], rules=[("crash_free_drop",)])
scenario("crash_free_cooldown_new_deploy", "release_health", [
    *D12, sql(t("12:00:00"), release(db("12:00:00"), "d2", 40, 4)),
    scan(t("12:10:00"), "release_health_check"), deliver(t("12:10:00")),
    scan(t("12:20:00"), "release_health_check"),
    {"at": t("12:30:00"), "do": "deploy", "deploy": "d3", "ref": None, "deployed_at": db("12:30:00")},
    sql(t("12:40:00"), release(db("12:00:00"), "d3", 25, 2)),
    scan(t("12:40:00"), "release_health_check"), deliver(t("12:40:00")),
], rules=[("crash_free_drop",)])
scenario("crash_free_threshold_nil_silent", "release_health", [
    *D12, sql(t("12:00:00"), release(db("12:00:00"), "d2", 40, 20)),
    scan(t("12:10:00"), "release_health_check"),
], rules=[("crash_free_drop",)], env={"crash_free_threshold": None})
scenario("crash_free_rounding_and_old_hour", "release_health", [
    *D12, sql(t("12:00:00"), release(db("10:00:00"), "d2", 500, 500), release(db("11:00:00"), "d2", 30, 1)),
    scan(t("12:10:00"), "release_health_check"), deliver(t("12:10:00")),
], rules=[("crash_free_drop",)], env={"crash_free_threshold": 97.5})


# ---------------------------------------------------------------- quota
def usage(at, month, events):
    return {"at": at, "do": "usage", "month": month, "events": events}


QACC = {"monthly_event_quota": 1000}
scenario("quota_80_then_100", "quota", [
    usage(t("12:00:00"), "2026-10-01", 850), scan(t("12:00:00"), "quota_check"), deliver(t("12:00:00")),
    scan(t("12:15:00"), "quota_check"),
    usage(t("12:30:00"), "2026-10-01", 1000), scan(t("12:30:00"), "quota_check"), deliver(t("12:30:00")),
], rules=[("quota",), ("quota", 0, {"environment_ids": [999]})], account=QACC)
scenario("quota_under_silent", "quota", [
    usage(t("12:00:00"), "2026-10-01", 799), scan(t("12:00:00"), "quota_check"),
], rules=[("quota",)], account=QACC)
scenario("quota_month_rollover", "quota", [
    usage(t("12:00:00"), "2026-10-01", 900), scan(t("12:00:00"), "quota_check"), deliver(t("12:00:00")),
    scan(t("00:10:00", "2026-11-01"), "quota_check"),
    usage(t("00:20:00", "2026-11-01"), "2026-11-01", 820), scan(t("00:20:00", "2026-11-01"), "quota_check"), deliver(t("00:20:00", "2026-11-01")),
], rules=[("quota",)], account=QACC)
scenario("quota_additional_cap", "quota", [
    usage(t("12:00:00"), "2026-10-01", 1300), scan(t("12:00:00"), "quota_check"), deliver(t("12:00:00")),
], rules=[("quota",)], account={"monthly_event_quota": 1000, "additional_events_cap": 500})


# ---------------------------------------------------------------- issue lifecycle events
def exc_steps(at, *specs):
    stmts, ids = [], []
    for s in specs:
        stmt, i = exception(*s)
        stmts.append(stmt)
        ids.append(i)
    return [sql(at, *stmts), {"at": at, "do": "group", "ids": ids}]


def lifecycle():
    EXC_ID[0] = 0
    return [
        *exc_steps(t("12:00:00"), ("Timeout::Error", "execution expired", db("11:59:58"), None, "u1"), ("Timeout::Error", "execution expired", db("11:59:59"), None, "u2")),
        deliver(t("12:00:00")),
        *exc_steps(t("12:05:00"), ("Timeout::Error", "execution expired", db("12:05:00"), None, "u1")),
        issue(t("12:10:00"), 1, "resolve"), deliver(t("12:10:00")),
        *exc_steps(t("12:20:00"), ("Timeout::Error", "execution expired", db("12:20:00"))), deliver(t("12:20:00")),
        issue(t("12:25:00"), 1, "resolve"),
        *exc_steps(t("12:30:00"), ("Timeout::Error", "execution expired", db("12:30:00"))), deliver(t("12:30:00")),
        issue(t("12:40:00"), 1, "resolve", deploy="v2"),
        *exc_steps(t("12:45:00"), ("Timeout::Error", "execution expired", db("12:45:00"), "v2")),
        *exc_steps(t("12:50:00"), ("Timeout::Error", "execution expired", db("12:50:00"), "v3")), deliver(t("12:50:00")),
    ]


scenario("issue_new_seen_resolve_regress_dedupe", "issue_events", lifecycle(),
         rules=[("new_issue",), ("regressed_issue",), ("resolved_issue",)])


def burst():
    EXC_ID[0] = 0
    return [
        *exc_steps(t("12:00:00"), *[(f"Error{i}", f"boom {i}", db("12:00:00")) for i in range(8)]),
        deliver(t("12:00:00")), deliver(t("12:00:59")), deliver(t("12:01:00")), deliver(t("12:01:01")),
    ]


scenario("issue_burst_collapse_summary", "issue_events", burst(), rules=[("new_issue",)])


def ignore_assign():
    EXC_ID[0] = 0
    return [
        *exc_steps(t("12:00:00"), ("NoMethodError", "undefined method `x' for nil", db("12:00:00"))), deliver(t("12:00:00")),
        issue(t("12:01:00"), 1, "ignore"), issue(t("12:02:00"), 1, "reopen"), issue(t("12:03:00"), 1, "assign", value=2),
        issue(t("12:04:00"), 1, "priority", value="high"), issue(t("12:05:00"), 1, "assign", value=None), deliver(t("12:05:00")),
    ]


scenario("issue_ignore_reopen_assign_priority", "issue_events", ignore_assign(),
         rules=[("new_issue",), ("ignored_issue",), ("regressed_issue",), ("assigned_issue",)])


def filtered():
    EXC_ID[0] = 0
    return [
        *exc_steps(t("12:00:00"), ("Timeout::Error", "slow", db("12:00:00")), ("ArgumentError", "(unclosed paren", db("12:00:00")),
                   ("KeyError", "key not found", db("12:00:00"))),
        deliver(t("12:00:00")),
    ]


scenario("issue_rule_filters", "issue_events", filtered(),
         rules=[("new_issue", 0, {"title_pattern": "/timeout/"}), ("new_issue", 0, {"title_pattern": "/(unclosed/"}),
                ("new_issue", 0, {"min_priority": "urgent"}), ("new_issue", 0, {"kinds": "exception"}),
                ("new_issue", 0, {"environment_ids": ["1"], "title_pattern": "key"})])


def deploy_noise():
    EXC_ID[0] = 0
    return [
        {"at": t("12:00:00"), "do": "deploy", "deploy": "v9", "ref": "9999999", "deployed_at": db("12:00:00")},
        *exc_steps(t("12:01:00"), ("HttpNetworkError", "Network Error", db("12:01:00"), "v9", None, "browser")),
        *exc_steps(t("12:05:00"), ("HttpNetworkError", "Network Error", db("12:05:00"), "v9", None, "browser")),
        deliver(t("12:05:00")),
    ]


scenario("issue_browser_deploy_noise", "issue_events", deploy_noise(), rules=[("new_issue",)])


# ---------------------------------------------------------------- auto-resolve
def aged():
    EXC_ID[0] = 0
    return [
        *exc_steps(t("03:00:00", "2026-09-29"), ("OldError", "fifteen days", db("03:00:00", "2026-09-29"))),
        *exc_steps(t("05:00:00", "2026-10-01"), ("NewerError", "thirteen days", db("05:00:00", "2026-10-01"))),
        scan(t("04:00:00"), "auto_resolve_issues"), deliver(t("04:00:00")),
    ]


scenario("auto_resolve_after_days", "auto_resolve", aged(), rules=[("resolved_issue",)], account={"auto_resolve_after_days": 14})
scenario("auto_resolve_off", "auto_resolve", aged(), rules=[("resolved_issue",)], account={"auto_resolve_after_days": None})


# ---------------------------------------------------------------- delivery outbox
def one_issue():
    EXC_ID[0] = 0
    return exc_steps(t("12:00:00"), ("RuntimeError", "kaboom", db("12:00:00")))


scenario("delivery_retry_then_sent", "delivery", [
    *one_issue(), deliver(t("12:00:00")), deliver(t("12:00:59")), deliver(t("12:01:00")), deliver(t("12:06:00")),
], rules=[("new_issue",)], integrations=[{"name": "flaky", "path": "fail2", "enabled": True}])
scenario("delivery_disabled_skipped", "delivery", [
    *one_issue(), deliver(t("12:00:00")),
], rules=[("new_issue",)], integrations=[{"name": "off", "path": "ok", "enabled": False}])
scenario("delivery_exhausts_attempts", "delivery", [
    *one_issue(), deliver(t("12:00:00")), deliver(t("12:01:00")), deliver(t("12:06:00")), deliver(t("12:21:00")), deliver(t("13:21:00")),
    deliver(t("14:30:00")),
], rules=[("new_issue",)], integrations=[{"name": "dead", "path": "fail9", "enabled": True}])

json.dump(SCENARIOS, open(OUT, "w"), indent=1)
by = {}
for s in SCENARIOS:
    by[s["detector"]] = by.get(s["detector"], 0) + 1
print(f"{len(SCENARIOS)} scenarios: " + ", ".join(f"{k} {v}" for k, v in by.items()))
