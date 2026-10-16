"""Diffs one detector-diff work dir: per scenario, the issues (with their
detection snapshots), issue_activities, alerts and webhook deliveries Rails
and Rust left.

  python3 -I compare.py WORK [--verbose]

Both clocks were frozen to the same step times, so timestamps are compared
as they are, to the microsecond. Normalized: row ids become ordinals within
the scenario (ids differ only by the account/user rows each side created
first), delivery_key / alert_id / delivery_lease_id (random uuids) become
presence, a delivery's URL host (each side has its own receiver path) and
the webhook's `sent_at` is compared to the step time it was sent at.
"""
import collections, json, os, sqlite3, sys

# Rails bugs fixed on both sides after rw-baseline was built: rw-baseline still
# runs the old Rails, so these scenarios differ from it until the image is
# rebuilt with the fixed railwatch-cloud. Each is reported as "expected vs old
# baseline" and does not fail the run; it must be identical on a fixed baseline.
EXPECTED_DIFF = {
    "missed_task_still_missed_seen": "CheckScheduledTasksJob now carries the detection snapshot forward on :seen (old Rails drops it)",
    "quota_80_then_100": "QuotaCheckJob now honours rule filters: the environment_ids [999] rule no longer fires (old Rails fires it twice)",
}

W = sys.argv[1]
VERBOSE = "--verbose" in sys.argv
scenarios = json.load(open(os.path.join(W, "scenarios.json")))
received = collections.defaultdict(list)
if os.path.exists(os.path.join(W, "received.jsonl")):
    for line in open(os.path.join(W, "received.jsonl")):
        r = json.loads(line)
        _, side, name, path = r["path"].split("/", 3)
        received[(side, name)].append(r)


def norm_time(v):
    """'2026-10-14 12:00:00' and '2026-10-14 12:00:00.000000' are one instant."""
    if isinstance(v, str) and len(v) >= 19 and v[4] == "-" and v[10] == " ":
        return v.rstrip("0").rstrip(".") if "." in v else v
    return v


def rows(db, sql):
    c = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
    c.row_factory = sqlite3.Row
    out = [dict(r) for r in c.execute(sql)]
    c.close()
    return out


def jparse(v):
    try:
        return json.loads(v) if isinstance(v, str) else v
    except ValueError:
        return v


def snapshot(db):
    ids = {}

    def ordinal(table, i):
        if i is None:
            return None
        m = ids.setdefault(table, {})
        return m.setdefault(i, f"{table}#{len(m) + 1}")

    issues = rows(db, "SELECT * FROM issues ORDER BY id")
    for i in issues:
        ordinal("issue", i["id"])
    alerts = rows(db, "SELECT a.*, n.name AS integration_name FROM alerts a JOIN integrations n ON n.id = a.integration_id ORDER BY a.id")
    for a in alerts:
        ordinal("alert", a["id"])
    rules = {r["id"]: f"rule#{n}" for n, r in enumerate(rows(db, "SELECT id FROM alert_rules ORDER BY id"), 1)}
    users = {r["id"]: r["email"] for r in rows(db, "SELECT id, email FROM users")}
    env_ids = [r["id"] for r in rows(db, "SELECT id FROM environments ORDER BY id")]

    def fix_payload(p):
        p = jparse(p)
        if isinstance(p, dict):
            if "environment_id" in p and p["environment_id"] in env_ids:
                p["environment_id"] = "env#1"
            if "rule_id" in p:
                p["rule_id"] = "rule"
            if "threshold_id" in p:
                p["threshold_id"] = "threshold"
            if "assignee_id" in p and p["assignee_id"] in users:
                p["assignee_id"] = users[p["assignee_id"]]
            if "account" in p:
                p["account"] = "account"
            if "message" in p and isinstance(p["message"], str) and "monthly event quota" in p["message"]:
                # QuotaCheckJob names the account; each side names its own.
                p["message"] = "account" + p["message"][p["message"].index(" reached "):]
        return p

    def fix_sample(s):
        s = jparse(s)
        if isinstance(s, dict):
            for k in ("rule_id", "threshold_id"):
                if k in s:
                    s[k] = k
            d = s.get("detection")
            if isinstance(d, dict) and isinstance(d.get("rule"), dict) and "id" in d["rule"]:
                d["rule"]["id"] = "rule"
        return s

    out = {"issues": [], "activities": [], "alerts": []}
    for i in issues:
        out["issues"].append({
            "id": ordinal("issue", i["id"]), "number": i["number"], "group_hash": i["group_hash"].split(":")[0] + ":" + i["group_hash"].split(":")[-1]
            if i["group_hash"].startswith(("perf:", "anomaly:")) else i["group_hash"],
            **{k: norm_time(i[k]) for k in ("kind", "title", "culprit", "status", "priority", "occurrences", "affected_users", "first_seen_at",
                                            "last_seen_at", "resolved_at", "resolved_in_deploy", "regressed_at", "source", "created_at", "updated_at")},
            "assignee": users.get(i["assignee_id"]), "sample": fix_sample(i["sample"]),
        })
    for a in rows(db, "SELECT * FROM issue_activities ORDER BY id"):
        data = jparse(a["data"])
        if isinstance(data, dict):
            for k in ("from_id", "to_id"):
                if k in data and data[k] in users:
                    data[k] = users[data[k]]
        out["activities"].append({"issue": ordinal("issue", a["issue_id"]), "kind": a["kind"], "data": data,
                                  "user": users.get(a["user_id"]), "created_at": norm_time(a["created_at"])})
    for a in alerts:
        out["alerts"].append({
            "id": ordinal("alert", a["id"]), "rule": rules.get(a["alert_rule_id"]), "integration": a["integration_name"],
            "issue": ordinal("issue", a["issue_id"]), "event": a["event"], "status": a["status"], "payload": fix_payload(a["payload"]),
            "attempts": a["delivery_attempts"], "error": a["error"], "burst_key": a["burst_key"] and a["burst_key"].split("/first-collapsed/")[0].split("/")[0],
            "summary_alert": ordinal("alert", a["summary_alert_id"]), "delivery_key": bool(a["delivery_key"]),
            **{k: norm_time(a[k]) for k in ("created_at", "sent_at", "next_delivery_at")},
        })
    integ = rows(db, "SELECT name, enabled, last_error, last_delivered_at FROM integrations ORDER BY id")
    out["integrations"] = [{**r, "last_delivered_at": norm_time(r["last_delivered_at"])} for r in integ]
    return out, ids


def deliveries(side, name, ids):
    out = []
    for r in received[(side, name)]:
        b = dict(r["body"])
        b["alert_id"] = bool(b.get("alert_id"))
        if b.get("issue"):
            b["issue"] = dict(b["issue"], id="issue")
        if b.get("url"):
            b["url"] = b["url"].rsplit("/", 2)[-2]
        p = b.get("payload")
        if isinstance(p, dict):
            for k in ("environment_id", "rule_id", "threshold_id"):
                if k in p:
                    p[k] = k
            if "account" in p:
                p["account"] = "account"
            if isinstance(p.get("message"), str) and "monthly event quota" in p["message"]:
                p["message"] = "account" + p["message"][p["message"].index(" reached "):]
            if "assignee_id" in p:
                p["assignee_id"] = "user"
        out.append({"path": r["path"].split("/", 3)[-1], "signature_ok": r["signature_ok"], "user_agent": r["user_agent"],
                    "answered": r["answered"], "body": b})
    return out


def first_diff(a, b, path="$"):
    if type(a) is not type(b):
        return f"{path}: {json.dumps(a)[:220]} != {json.dumps(b)[:220]}"
    if isinstance(a, dict):
        if list(a) != list(b):
            return f"{path} keys: only rails={[k for k in a if k not in b]} only rust={[k for k in b if k not in a]} order_same={sorted(a) == sorted(b)}"
        for k in a:
            d = first_diff(a[k], b[k], f"{path}.{k}")
            if d:
                return d
        return None
    if isinstance(a, list):
        for i, (x, y) in enumerate(zip(a, b)):
            d = first_diff(x, y, f"{path}[{i}]")
            if d:
                return d
        if len(a) != len(b):
            return f"{path}: length {len(a)} != {len(b)}; extra: {json.dumps((a[len(b):] or b[len(a):])[:2])[:300]}"
        return None
    return None if a == b else f"{path}: {json.dumps(a)[:220]} != {json.dumps(b)[:220]}"


per = collections.defaultdict(lambda: [0, 0])
facts = collections.Counter()
for sc in scenarios:
    name, det = sc["name"], sc["detector"]
    rails_db = os.path.join(W, "rails", f"{name}.sqlite3")
    rust_db = os.path.join(W, "rust", name, "control.sqlite3")
    if not (os.path.exists(rails_db) and os.path.exists(rust_db)):
        continue
    (ra, rids), (ru, uids) = snapshot(rails_db), snapshot(rust_db)
    ra["deliveries"], ru["deliveries"] = deliveries("rails", name, rids), deliveries("rust", name, uids)
    # IssueDetectionPresenter#as_json per issue (Rust renders Times as as_json).
    ra["presented"] = json.load(open(os.path.join(W, "rails", f"{name}.presented.json")))
    ru["presented"] = json.load(open(os.path.join(W, "rust", name, "presented.json")))
    for side in (ra, ru):
        for v in side["presented"].values():
            if isinstance(v, dict) and isinstance(v.get("rule"), dict) and "id" in v["rule"]:
                v["rule"]["id"] = "rule"
    facts["available"] += sum(1 for v in ra["presented"].values() if isinstance(v, dict) and v.get("available"))
    facts["representatives"] += sum(len(v.get("representative_records", [])) for v in ra["presented"].values() if isinstance(v, dict))
    per[det][1] += 1
    facts["issues"] += len(ra["issues"])
    facts["alerts"] += len(ra["alerts"])
    facts["deliveries"] += len(ra["deliveries"])
    facts["snapshots"] += sum(1 for i in ra["issues"] if isinstance(i["sample"], dict) and "detection" in i["sample"])
    d = first_diff(ra, ru)
    if d is not None and name in EXPECTED_DIFF:
        print(f"EXPECTED   {name}  (expected vs old baseline: {EXPECTED_DIFF[name]})\n    {d}")
        facts["expected"] += 1
        per[det][1] -= 1
        continue
    if d is None:
        per[det][0] += 1
        if VERBOSE:
            print(f"identical  {name}  ({len(ra['issues'])} issues, {len(ra['alerts'])} alerts, {len(ra['deliveries'])} deliveries)")
    else:
        print(f"DIFFERENT  {name}\n    {d}")
total = [sum(v[0] for v in per.values()), sum(v[1] for v in per.values())]
for det, (s, n) in per.items():
    print(f"{det:16} {s}/{n} identical")
print(f"TOTAL {total[0]}/{total[1]} scenarios identical; Rails side: {facts['issues']} issues ({facts['snapshots']} with detection), "
      f"{facts['alerts']} alerts, {facts['deliveries']} deliveries; presenter: {facts['available']} available, {facts['representatives']} representative records"
      + (f"; {facts['expected']} expected vs old baseline (not counted)" if facts["expected"] else ""))
sys.exit(0 if total[0] == total[1] else 1)
