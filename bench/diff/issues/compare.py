"""Compares capture_rails.rb's and capture_rust.py's outputs path by path.
Shared props (auth, account, accounts, flash, google_oauth, applications,
errors, and the deferred/once metadata) are not compared: p3-control owns
them and they differ by design (sessions). Everything else must be equal,
key order included.

  python3 -I compare.py RAILS.json RUST.json [--verbose]
"""
import json, sys

SHARED = {"auth", "account", "accounts", "flash", "google_oauth", "applications", "errors"}
# Rails bugs fixed on both sides (2026-10-16) after rw-baseline was built: it
# still runs the old Rails. A difference only under one of these prop paths
# prints as "expected vs old baseline" and does not fail the run; on a
# baseline rebuilt from the fixed railwatch-cloud it must be identical.
EXPECTED_DIFF_PATHS = {
    "$.props.related_issues": "related_issues LIKE now has ESCAPE '\\' (old Rails: a `_` in the class name matched only `\\_`)",
    "$.props.comments": "a Linear comment on a detection issue falls back to author_name, then \"Linear\" (old Rails: NoMethodError)",
}
rails, rust = json.load(open(sys.argv[1])), json.load(open(sys.argv[2]))
verbose = "--verbose" in sys.argv


def first_diff(a, b, path="$"):
    if type(a) is not type(b):
        return f"{path}: {json.dumps(a)[:160]} != {json.dumps(b)[:160]}"
    if isinstance(a, dict):
        if list(a) != list(b):
            return f"{path} keys: only rails={[k for k in a if k not in b]} only rust={[k for k in b if k not in a]} order_same={sorted(a) == sorted(b)}"
        for k in a:
            d = first_diff(a[k], b[k], f"{path}.{k}")
            if d:
                return d
        return None
    if isinstance(a, list):
        if len(a) != len(b):
            return f"{path}: length {len(a)} != {len(b)}"
        for i, (x, y) in enumerate(zip(a, b)):
            d = first_diff(x, y, f"{path}[{i}]")
            if d:
                return d
        return None
    return None if a == b else f"{path}: {json.dumps(a)[:160]} != {json.dumps(b)[:160]}"


same = expected = 0
for path, r in rails.items():
    u = rust.get(path)
    if "props" in r and u and "props" in u:
        r = {"component": r["component"], "props": {k: v for k, v in r["props"].items() if k not in SHARED}}
        u = {"component": u["component"], "props": {k: v for k, v in u["props"].items() if k not in SHARED}}
    d = first_diff(r, u)
    why = next((w for p, w in EXPECTED_DIFF_PATHS.items() if d and d.startswith(p)), None)
    if why:
        expected += 1
        print(f"EXPECTED   {path}  (expected vs old baseline: {why})\n    {d}")
        continue
    if d is None:
        same += 1
        if verbose:
            print(f"identical  {path}")
    else:
        print(f"DIFFERENT  {path}\n    {d}")
print(f"{same}/{len(rails) - expected} cases identical" + (f"; {expected} expected vs old baseline (not counted)" if expected else ""))
sys.exit(0 if same == len(rails) - expected else 1)
