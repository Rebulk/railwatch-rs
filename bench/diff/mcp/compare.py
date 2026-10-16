"""Compares capture_rails.rb's and mcp-dump's outputs case by case.

  compare.py RAILS.json RUST.json [--verbose]

Each case is {status, body}. body is the JSON-RPC response; a tool's
result text (content[0].text) is itself JSON and is parsed before
comparing, so a difference is reported at its path inside the tool's
output. Everything must be equal, key order included, except the volatile
fields listed in VOLATILE (ids and times a write assigns, and a cursor
signed with each server's own key), which are compared for presence and
type only. A Rails HTML error page has body null on both sides.
"""
import json, sys

rails, rust = json.load(open(sys.argv[1])), json.load(open(sys.argv[2]))
verbose = "--verbose" in sys.argv

# (case name prefix, key) pairs whose values are assigned at write time or
# signed with a per-server secret.
VOLATILE = {
    ("write.add_comment", "id"), ("write.add_comment", "created_at"),
    ("search_telemetry", "next_cursor"),
}


def unwrap(body):
    """Parse the tool text so diffs point inside it."""
    try:
        content = body["result"]["content"]
        text = content[0]["text"]
        return {**body, "result": {**body["result"], "content": [{**content[0], "text": json.loads(text)}]}}
    except (KeyError, TypeError, IndexError, ValueError):
        return body


def volatile(case, key):
    return any(case.startswith(p) and key == k for p, k in VOLATILE)


def first_diff(case, a, b, path="$"):
    if type(a) is not type(b):
        return f"{path}: {json.dumps(a)[:200]} != {json.dumps(b)[:200]}"
    if isinstance(a, dict):
        if list(a) != list(b):
            return f"{path} keys: only rails={[k for k in a if k not in b]} only rust={[k for k in b if k not in a]} order_same={sorted(a) == sorted(b)}"
        for k in a:
            if volatile(case, k) and type(a[k]) is type(b[k]):
                continue
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


# Rails bugs fixed on both sides (2026-10-16) after rw-baseline was built: it
# still runs the old Rails, so these may differ from it. A listed case that
# differs prints as "expected vs old baseline" and does not fail the run; on
# a baseline rebuilt from the fixed railwatch-cloud each must be identical.
EXPECTED_DIFF = {
    "parse.form_ctype": "the raw body is JSON whatever the Content-Type: initialize answers (old Rails: 400 -32700)",
    "parse.string_body": "valid JSON, not an object: 400 -32600 (old Rails: String#[] lookups on the payload, a JSON-RPC error with HTTP 200)",
    "parse.array_body": "valid JSON, not an object: 400 -32600 (old Rails: the HTML 500 page)",
    "parse.null_body": "valid JSON, not an object: 400 -32600 (old Rails: the HTML 500 page)",
    "parse.number_body": "valid JSON, not an object: 400 -32600 (old Rails: the HTML 500 page)",
    "prompts/get.params_array": "params not an object: 200 -32602 Invalid params (old Rails: the HTML 500 page)",
    "tools/call.params_array": "params not an object: 200 -32602 Invalid params (old Rails: -32603)",
    "initialize.params_string": "params not an object: 200 -32602 Invalid params (old Rails: ignored)",
    "tools/list.params_number": "params not an object: 200 -32602 Invalid params (old Rails: ignored)",
}

same = expected = 0
for name, r in rails.items():
    u = rust.get(name)
    if u is None:
        print(f"MISSING    {name}")
        continue
    a = {"status": r["status"], "body": unwrap(r["body"])}
    b = {"status": u["status"], "body": unwrap(u["body"])}
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
