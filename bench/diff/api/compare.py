"""Compares capture_rails.rb's and api-dump's outputs case by case.

  compare.py RAILS.json RUST.json [--verbose]

Each case is {status, headers, body}. Equal means: the same status; the
same body, value for value with key order (JSON parsed; an HTML error page
compared by its <title>); and the same values for the headers a client
reads (content-type, cache-control, etag, content-length, and the security
headers). Normalized, and only these (docs/DIFFERENCES.md):
- headers Rails adds that this server does not: x-request-id, x-runtime,
  strict-transport-security (behind TLS), content-security-policy-report-only
  (a per-request nonce), vary;
- volatile values: ids a write assigns (comment id, a new deploy's id) and
  the ETag of a body holding one; cursors signed with each server's own key
  (next_cursor: present or null).
"""
import json, re, sys

rails, rust = json.load(open(sys.argv[1])), json.load(open(sys.argv[2]))

# Rails bugs fixed on both sides (2026-10-16) after rw-baseline was built: it
# still runs the old Rails, so these cases may differ from it. A listed case
# that differs prints as "expected vs old baseline" and does not fail the run;
# on a baseline rebuilt from the fixed railwatch-cloud each must be identical.
EXPECTED_DIFF = {
    "issues.q_percent": "LIKE now has ESCAPE '\\': % matches a literal percent (old Rails: a literal backslash)",
    "issues.q_underscore": "LIKE now has ESCAPE '\\': _ matches a literal underscore",
    "issues.page_array": "page[]=1 reads as absent: 200 with page 1 (old Rails: the 500 page)",
    "issues.per_page_array": "per_page[]=1 reads as absent: 200 with 25 per page (old Rails: the 500 page)",
    "logs.limit_array": "limit[]=1 reads as absent (50), cursor page only (old Rails: the 500 page)",
    "logs.level": "per_page only: data/meta alone, no cursor page (old Rails: both)",
    "logs.text": "limit only: results/results_meta alone, no offset page (old Rails: both)",
    "logs.cursor_page2": "cursor+limit only: results/results_meta alone",
    "exceptions.cursor_wrong_resource": "limit only: results/results_meta alone (the cursor error itself is unchanged)",
    "write.deploy.repost": "a re-posted deploy keeps its deployed_at (old Rails: reset to now)",
    "after_writes.deploys": "the re-posted deploy keeps its deployed_at, so its place in the list",
    "after_writes.releases": "the re-posted deploy keeps its deployed_at, so its place in the timeline",
    "series.no_type": "Rust-only: series also takes type=visit, so the 422 lists it",
    "series.bad_type": "Rust-only: series also takes type=visit, so the 422 lists it",
}
verbose = "--verbose" in sys.argv

IGNORED_HEADERS = {"x-request-id", "x-runtime", "strict-transport-security", "content-security-policy-report-only", "vary", "set-cookie"}
VOLATILE_KEYS = {
    ("write.comment", "id"),
    ("write.deploy.new", "id"),
    ("write.deploy.long", "id"),
    ("after_writes.issue", "id"),
    ("after_writes.deploys", "id"),
}


def volatile(case, path):
    # comments[].id and deploys data[].id after the writes are assigned at write time.
    key = path.rsplit(".", 1)[-1]
    if key == "next_cursor":
        return True
    if (case, key) in VOLATILE_KEYS:
        if case == "after_writes.issue":
            return ".comments[" in path
        if case == "after_writes.deploys":
            return ".data[" in path
        return True
    return False


def norm_body(b):
    if isinstance(b, str) and b.lstrip().lower().startswith("<!doctype html"):
        m = re.search(r"<title>(.*?)</title>", b, re.S)
        return {"html_page": m.group(1).strip() if m else "?"}
    return b


def has_volatile(case):
    return any(c == case for c, _ in VOLATILE_KEYS)


def norm_headers(case, h):
    out = {k: v for k, v in h.items() if k not in IGNORED_HEADERS}
    if has_volatile(case) or "next_cursor" in json.dumps(out):
        pass
    return out


def first_diff(case, a, b, path="$"):
    if volatile(case, path):
        if a is None or b is None:
            return None if a is b else f"{path}: presence {a is None} != {b is None}"
        return None if type(a) is type(b) else f"{path}: type {type(a).__name__} != {type(b).__name__}"
    if type(a) is not type(b):
        return f"{path}: {json.dumps(a)[:200]} != {json.dumps(b)[:200]}"
    if isinstance(a, dict):
        if list(a) != list(b):
            return f"{path} keys: only rails={[k for k in a if k not in b]} only rust={[k for k in b if k not in a]} order_same={sorted(a) == sorted(b)}"
        for k in a:
            d = first_diff(case, a[k], b[k], f"{path}.{k}")
            if d:
                return d
        return None
    if isinstance(a, list):
        if len(a) != len(b):
            return f"{path}: length {len(a)} != {len(b)}"
        for i, (x, y) in enumerate(zip(a, b)):
            d = first_diff(case, x, y, f"{path}[{i}]")
            if d:
                return d
        return None
    return None if a == b else f"{path}: {json.dumps(a)[:200]} != {json.dumps(b)[:200]}"


def body_has_volatile(case, body):
    """The ETag and length hash the body: skip them where it holds a volatile value."""
    text = json.dumps(body)
    return has_volatile(case) or '"next_cursor": "' in text


same = expected = 0
for name, r in rails.items():
    u = rust.get(name)
    if u is None:
        print(f"MISSING    {name}")
        continue
    ra, ua = norm_headers(name, r["headers"]), norm_headers(name, u["headers"])
    if body_has_volatile(name, r["body"]) or body_has_volatile(name, u["body"]):
        for h in (ra, ua):
            for k in ("etag", "content-length"):
                if k in h:
                    h[k] = "<volatile>"
    a = {"status": r["status"], "headers": dict(sorted(ra.items())), "body": norm_body(r["body"])}
    b = {"status": u["status"], "headers": dict(sorted(ua.items())), "body": norm_body(u["body"])}
    d = first_diff(name, a, b)
    if d is not None and name in EXPECTED_DIFF:
        expected += 1
        print(f"EXPECTED   {name}  (expected vs old baseline: {EXPECTED_DIFF[name]})\n    {d}")
        continue
    if d is None:
        same += 1
        if verbose:
            print(f"identical  {name}")
    else:
        print(f"DIFFERENT  {name}\n    {d}")
print(f"{same}/{len(rails) - expected} cases identical" + (f"; {expected} expected vs old baseline (not counted)" if expected else ""))
sys.exit(0 if same == len(rails) - expected else 1)
