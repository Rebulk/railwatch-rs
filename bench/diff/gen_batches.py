#!/usr/bin/env python3
"""Builds the differential test's batches: N gzip NDJSON files of M records
each, a realistic mix (distinct execution ids, queries from a shape pool, of
which most digest to their group so query_shapes dedupe applies and some do
not), plus deliberately invalid records, oversize fields, control
characters, unicode, invalid UTF-8, health detail strings, profiles and
attachments with gzip blobs (valid and broken), users, sessions...

    gen_batches.py FIXTURES OUT_DIR [batches=20] [records=200] [seed=7]

Timestamps are "now" at build time (seconds back, up to an hour) so every
record is inside both servers' windows. Writes OUT_DIR/NNNN.ndjson.gz and
OUT_DIR/manifest.json (per-batch headers: direct or receipted).
"""
import base64, copy, gzip, hashlib, json, os, random, sys, time, uuid

fixtures = json.load(open(sys.argv[1]))
out_dir = sys.argv[2]
n_batches = int(sys.argv[3]) if len(sys.argv) > 3 else 20
n_records = int(sys.argv[4]) if len(sys.argv) > 4 else 200
rng = random.Random(int(sys.argv[5]) if len(sys.argv) > 5 else 7)
os.makedirs(out_dir, exist_ok=True)
NOW = time.time()

def md5(*parts):
    return hashlib.md5(",".join(parts).encode()).hexdigest()

TABLES = ["widgets", "gadgets", "users", "orders", "line_items", "accounts", "sessions", "events"]
SHAPES = []
for i in range(200):
    t = TABLES[i % len(TABLES)]
    sql = f'SELECT "{t}".* FROM "{t}" WHERE "{t}"."id" = ? AND "{t}"."c{i}" = ? LIMIT ?'
    conn = "primary" if i % 5 else "replica"
    SHAPES.append((conn, sql, md5(conn, sql)))

def uid():
    return str(uuid.UUID(int=rng.getrandbits(128), version=4))

def ts():
    # A float with sub-microsecond noise, as Process.clock_gettime gives.
    return round(NOW - rng.uniform(0, 3500), 7)

def base(t, eid, trace):
    r = copy.deepcopy(fixtures[t])
    r["timestamp"] = ts()
    if "execution_id" in r:
        r["execution_id"] = eid
    if "trace_id" in r:
        r["trace_id"] = trace
    return r

def gz_b64(b):
    return base64.b64encode(gzip.compress(b, mtime=0)).decode()

UNICODE = ["héllo wörld", "日本語のログ", "emoji 😀 🚂", "rtl עברית", "zero​width", "line sep", "</script><b>&amp;"]

def execution_family():
    eid, trace = uid(), uid()
    out = []
    parent = "request" if rng.random() < 0.8 else rng.choice(["job_attempt", "job_attempt", "scheduled_task", "command", "channel_action"])
    p = base(parent, eid, trace)
    p["duration"] = rng.randint(1, 500_000)
    if parent == "request":
        p["route"] = f"/widgets/{rng.randint(1, 30)}(.:format)"
        p["status_code"] = rng.choice([200, 200, 200, 302, 404, 500])
        p["user"] = rng.choice([None, "1", "42", "tenant:7"])
        if rng.random() < 0.2:
            p["inertia"] = {"component": "Widgets/Show", "partial_only": rng.choice([[], ["a"], None])}
    out.append(p)
    for _ in range(rng.randint(5, 11)):
        q = base("query", eid, trace)
        conn, sql, group = rng.choice(SHAPES)
        q["connection"] = conn
        q["duration"] = rng.randint(1, 50_000)
        q["row_count"] = rng.randint(0, 100)
        roll = rng.random()
        if roll < 0.85:
            q["sql"], q["_group"] = sql, group
        elif roll < 0.95:
            q["sql"], q["_group"] = sql.replace("?", str(rng.randint(1, 9))), group  # captured values: stays on the row
        else:
            q["sql"], q["_group"] = sql, None
        out.append(q)
    for _ in range(2):
        c = base("cache_event", eid, trace)
        c["key"] = f"views/widget/{rng.randint(1, 1000)}"
        c["hits"] = rng.choice([None, 0, 1])
        out.append(c)
    v = base("view_render", eid, trace)
    v["duration"] = rng.randint(1, 9000)
    out.append(v)
    for _ in range(2):
        l = base("log", eid, trace)
        l["message"] = rng.choice(["Completed 200 OK", "Started GET", rng.choice(UNICODE), "x" * rng.choice([10, 70_000])])
        l["tags"] = rng.choice([[], ["req-1"], ["a", "b"]])
        out.append(l)
    if rng.random() < 0.2:
        o = base("outgoing_request", eid, trace)
        o["status_code"] = rng.choice([200, 503])
        o["response_body"] = rng.choice([None, "ok", "y" * 70_000])
        out.append(o)
    if rng.random() < 0.05:
        e = base("exception", eid, trace)
        e["message"] = rng.choice(["kaboom", rng.choice(UNICODE), "m" * 70_000])
        e["locals"] = rng.choice([None, {"id": 1, "name": "x"}])
        out.append(e)
    return out

ODD_KIND = [0]

def odd_record():
    # Cycle through every kind so each batch set covers all of them.
    eid, trace = uid(), uid()
    kind = ODD_KIND[0] % 24
    ODD_KIND[0] += 1
    if kind == 0:
        r = base("health", None, None)
        r["detail"] = rng.choice([fixtures["health"]["detail"], "not json", "[1,2]", None, '{"a":1.0e-05,"b":12000000.0}'])
    elif kind == 1:
        r = base("process", None, None)
        r["boot_seconds"] = rng.choice([1, 1.25, "2.5", None])
    elif kind == 2:
        r = base("user", eid, trace)
        r["id"] = rng.choice(["1", "42", "x", "", None])
        r["name"] = rng.choice(["Ada", " ", None, "Grace\u0001"])
    elif kind == 3:
        r = base("visit", None, None)
        r["cls"] = rng.choice([None, 0.1, 1e-7])
    elif kind == 4:
        r = base("span", eid, trace)
        r["attributes"] = {"k": rng.choice([1, 2.5, "v", None, [1, {"n": 1e16}]])}
    elif kind == 5:
        r = base("profile", eid, trace)
        stacks = b"a;b;c 3\nd 1\n"
        r["stacks"], r["stacks_bytes"] = gz_b64(stacks), rng.choice([len(stacks), len(stacks), len(stacks) + 1])
    elif kind == 6:
        r = base("attachment", None, None)
        data = bytes(rng.randrange(256) for _ in range(40))
        r["data"], r["bytes"] = rng.choice([(gz_b64(data), 40), ("!!", 1), (base64.b64encode(b"plain").decode(), 5)])
    elif kind == 7:
        r = base("session", None, None)
        r["id"] = uid()
        r["status"] = rng.choice(["started", "crashed", "ok"])
    elif kind == 8:
        r = base("query", eid, trace)
        r["v"] = 2  # wrong version
    elif kind == 9:
        r = base("query", eid, trace)
        r["timestamp"] = NOW - 40 * 86_400  # out of range
    elif kind == 10:
        r = base("log", eid, trace)
        r["timestamp"] = NOW + 7200
    elif kind == 11:
        r = {"t": "martian", "v": 1, "timestamp": ts()}
    elif kind == 12:
        r = base("query", eid, trace)
        r["execution_id"] = "has space"
    elif kind == 13:
        r = base("exception", eid, trace)
        r["frames"] = [{"line": -1}]
    elif kind == 14:
        r = base("log", eid, trace)
        r["message"] = "ctl\u0000\u0007\u001b end"
        r["server"] = "s\u007fx"
    elif kind == 15:
        r = base("llm_call", eid, trace)
        r["params"] = {"temperature": 0.7, "s": "p" * 17_000}
        r["prompt"] = "q" * 17_000
    elif kind == 16:
        r = base("enqueued_job", eid, trace)
        r["scheduled_at"] = ts()
    elif kind == 17:
        r = base("mail", eid, trace)
        r["subject"] = "s" * 300
    elif kind == 18:
        r = base("n_plus_one", eid, trace)
    elif kind == 19:
        r = base("deprecation", eid, trace)
    elif kind == 20:
        r = base("broadcast", eid, trace)
    elif kind == 21:
        r = base("notification", eid, trace)
    elif kind == 22:
        r = base("storage_op", eid, trace)
    else:
        r = base("transaction", eid, trace)
    return r

RAW_LINES = [
    b'{"t":"query","v":1,"timestamp":TS,"sql":"bad \xff\xfe utf8","duration":1}',
    b'{"t":"log","v":1,"timestamp":TS,"level":"info","message":"lone \\udc00 surrogate","tags":[]}',
    b'"just a string"',
    b'{"t":"query","v":1,"timestamp":TS,"sql":"s","duration":123456789012345678901234567890}',
    b'{"t":"\xff","v":1,"timestamp":TS}',
]

manifest = []
for b in range(n_batches):
    records, raw = [], []
    while len(records) < n_records:
        if rng.random() < 0.15:
            records.append(odd_record())
        else:
            records.extend(execution_family())
    records = records[:n_records]
    lines = [json.dumps(r, separators=(",", ":"), ensure_ascii=rng.random() < 0.5).encode() for r in records]
    if b % 4 == 1:
        lines.insert(rng.randrange(len(lines)), rng.choice(RAW_LINES).replace(b"TS", repr(ts()).encode()))
    body = gzip.compress(b"\n".join(lines) + b"\n", mtime=0)
    name = f"{b:04d}.ndjson.gz"
    open(os.path.join(out_dir, name), "wb").write(body)
    headers = {"X-Railwatch-Version": "0.9.0", "X-Railwatch-Batch-Id": str(uuid.UUID(int=rng.getrandbits(128), version=4))}
    if b % 5 == 3:
        headers["X-Railwatch-Dropped"] = str(rng.randint(1, 50))
        headers["X-Railwatch-Backpressure-Factor"] = rng.choice(["2.5", "abc", "0.5"])
    if b % 3 == 2:
        # Receipted: v7 batch id dated now, producer, digest, policy.
        ms = int(time.time() * 1000)
        v7 = uuid.UUID(int=(ms << 80) | (0x7 << 76) | (rng.getrandbits(12) << 64) | (0b10 << 62) | rng.getrandbits(62))
        headers.update({"X-Railwatch-Producer-Id": "6d1c5f0e-4a2b-4c8d-9e7f-0a1b2c3d4e5f", "X-Railwatch-Batch-Id": str(v7),
                        "X-Railwatch-Body-SHA256": hashlib.sha256(body).hexdigest(), "X-Railwatch-Policy": "everything-v1"})
    manifest.append({"file": name, "headers": headers, "records": len(lines)})
    # Receipted batches are also re-sent once: the second answer must be already_committed.
    if "X-Railwatch-Producer-Id" in headers:
        manifest.append({"file": name, "headers": headers, "records": len(lines), "replay": True})

json.dump(manifest, open(os.path.join(out_dir, "manifest.json"), "w"), indent=1)
print(f"{len(manifest)} posts, {sum(m['records'] for m in manifest if not m.get('replay'))} records -> {out_dir}")
