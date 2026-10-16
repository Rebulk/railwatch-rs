#!/usr/bin/env python3
"""Diffs Rails' page props against Rust's, case by case.

    compare.py RAILS.json RUST.json [--verbose]

Normalized before comparing (and only these; docs/DIFFERENCES.md lists why):
- shared props p3-control owns, not this module: auth, account, accounts,
  flash, google_oauth, applications, environment, saved_views, errors,
  deferred/merge bookkeeping;
- `pagination.next_cursor`: signed by each server with its own key
  (reduced to present/absent);
- `trace_url`, `links`: URL helpers (p3-control builds them).

Prints one line per case (OK / DIFF with the first few differing paths) and
exits 1 on any difference.
"""
import json, sys

SHARED = {"auth", "account", "accounts", "flash", "google_oauth", "applications", "environment", "saved_views", "errors"}

def norm(case_props):
    p = {k: v for k, v in case_props.items() if k not in SHARED}
    if isinstance(p.get("pagination"), dict) and "next_cursor" in p["pagination"]:
        p["pagination"] = dict(p["pagination"], next_cursor="<present>" if p["pagination"]["next_cursor"] else None)
    for k in ("trace_url", "links"):
        if k in p:
            p[k] = "<url>" if p[k] else p[k]
    return p

def diff(a, b, path, out, limit=8):
    if len(out) >= limit:
        return
    if type(a) != type(b) and not (isinstance(a, (int, float)) and isinstance(b, (int, float)) and type(a) != bool and type(b) != bool):
        out.append(f"{path}: rails={json.dumps(a)[:120]} rust={json.dumps(b)[:120]}")
    elif isinstance(a, dict):
        if list(a.keys()) != list(b.keys()):
            ka, kb = list(a.keys()), list(b.keys())
            if set(ka) != set(kb):
                out.append(f"{path}: keys rails-only={sorted(set(ka)-set(kb))} rust-only={sorted(set(kb)-set(ka))}")
            else:
                out.append(f"{path}: key order rails={ka[:12]} rust={kb[:12]}")
        for k in a:
            if k in b:
                diff(a[k], b[k], f"{path}.{k}", out, limit)
    elif isinstance(a, list):
        if len(a) != len(b):
            out.append(f"{path}: len rails={len(a)} rust={len(b)}")
        for i, (x, y) in enumerate(zip(a, b)):
            diff(x, y, f"{path}[{i}]", out, limit)
    elif a != b or (isinstance(a, float) != isinstance(b, float)):
        # 1 and 1.0 are different JSON from Ruby (Integer vs Float).
        out.append(f"{path}: rails={json.dumps(a)[:120]} rust={json.dumps(b)[:120]}")

def main():
    rails = json.load(open(sys.argv[1]))
    rust = json.load(open(sys.argv[2]))
    verbose = "--verbose" in sys.argv
    bad = 0
    for name in rails:
        r, s = rails[name], rust.get(name)
        if s is None:
            print(f"MISSING {name}")
            bad += 1
            continue
        if "status" in r or "status" in s:
            same = r.get("status") == s.get("status")
            print(f"{'OK  ' if same else 'DIFF'} {name}: status rails={r.get('status', 200)} rust={s.get('status', 200)} {'' if same else s.get('body', '')[:200]}")
            bad += not same
            continue
        out = []
        if r["component"] != s["component"]:
            out.append(f"component rails={r['component']} rust={s['component']}")
        diff(norm(r["props"]), norm(s["props"]), "props", out, limit=40 if verbose else 6)
        if out:
            bad += 1
            print(f"DIFF {name}")
            for line in out:
                print(f"     {line}")
        else:
            print(f"OK   {name}")
    print(f"\n{len(rails) - bad}/{len(rails)} cases identical")
    sys.exit(1 if bad else 0)

main()
