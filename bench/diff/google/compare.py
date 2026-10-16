#!/usr/bin/env python3
"""Rails vs Rust per case: where the browser lands, the flash it sees, and
the users table afterwards. The failure key is reported but not compared
(Rails puts the JWT error text there; docs/DIFFERENCES.md)."""
import json, sys

def norm(o):
    if isinstance(o, dict):
        o = {k: norm(v) for k, v in o.items() if k != "first_flash"}
        if str(o.get("first", "")).startswith("/auth/failure?message="):
            o["first"] = "/auth/failure?message=<key>"
    return o

def key(o):
    s = json.dumps(o["out"])
    return s.split("message=")[1].split("&")[0] if "message=" in s else ""

rails = [json.loads(l) for l in open(sys.argv[1]) if l.startswith('{"case"')]
rust = [json.loads(l) for l in open(sys.argv[2]) if l.startswith("{")]
same = 0
for r, s in zip(rails, rust):
    ok = norm(r) == norm(s)
    same += ok
    print(f"{'SAME' if ok else 'DIFF'}  {r['case']:38} failure key: rails={key(r)[:48]!r} rust={key(s)!r}")
    if not ok:
        print("      rails:", json.dumps(norm(r)["out"])[:240], norm(r)["users"])
        print("      rust: ", json.dumps(norm(s)["out"])[:240], norm(s)["users"])
print(f"{same}/{len(rails)} identical")
