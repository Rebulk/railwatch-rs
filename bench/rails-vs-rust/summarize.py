"""Median and spread (min-max) per (side, variant, label) from runs.jsonl,
as markdown tables, plus every raw run.

    python3 -I bench/rails-vs-rust/summarize.py [runs.jsonl] [pages.jsonl]
"""
import json, os, statistics, sys
from collections import defaultdict

here = os.path.dirname(os.path.abspath(__file__))
res = os.path.join(here, "..", "results", "rails-vs-rust")
runs_path = sys.argv[1] if len(sys.argv) > 1 else os.path.join(res, "runs.jsonl")
pages_path = sys.argv[2] if len(sys.argv) > 2 else os.path.join(res, "pages.jsonl")

runs = [json.loads(l) for l in open(runs_path) if l.strip()]
groups = defaultdict(list)
for r in runs:
    side = r["side"] if r.get("variant", "stock") == "stock" else f'{r["side"]}+{r["variant"]}'
    groups[(r["label"].removeprefix("cf-"), side)].append(r)


def ms(vals, fmt="{:.0f}"):
    vals = [v for v in vals if v is not None]
    if not vals:
        return "-"
    med = statistics.median(vals)
    return f"{fmt.format(med)} ({fmt.format(min(vals))}-{fmt.format(max(vals))})" if len(vals) > 1 else fmt.format(med)


cols = [
    ("n", lambda rs: str(len(rs))),
    ("events/s", lambda rs: ms([r["events_per_s"] for r in rs])),
    ("p50 ms", lambda rs: ms([r["p50_ms"] for r in rs])),
    ("p99 ms", lambda rs: ms([r["p99_ms"] for r in rs])),
    ("CPU s (incl. tail)", lambda rs: ms([r["cpu_total_s"] for r in rs], "{:.1f}")),
    ("CPU ms / 1k events", lambda rs: ms([1e6 * r["cpu_total_s"] / max(r["events"], 1) for r in rs], "{:.1f}")),
    ("peak anon MiB", lambda rs: ms([r["peak_anon_mib"] for r in rs])),
    ("peak anon+cache MiB", lambda rs: ms([r["peak_current_mib"] for r in rs])),
    ("fsync / batch", lambda rs: ms([r["fsync"] / max(r["posts"], 1) for r in rs if r["fsync"] is not None], "{:.2f}")),
    ("non-200", lambda rs: str(sum(sum(v for k, v in r["status_counts"].items() if k != "200") for r in rs))),
]
order = ["max-1app", "max-10app", "max-50app", "rate-1app", "rate-10app", "rate-50app", "conc1-1app", "conc4-1app", "conc64-1app"]
labels = sorted({k[0] for k in groups}, key=lambda l: order.index(l) if l in order else 99)
# The three main sides first, then any variant (mechanism.sh's sync-off, debug-log).
sides = ["rails", "rails+group-commit", "rust"]
sides += sorted({k[1] for k in groups} - set(sides))
print("| case | side | " + " | ".join(c for c, _ in cols) + " |")
print("|---|---|" + "---:|" * len(cols))
for label in labels:
    for side in sides:
        rs = groups.get((label, side))
        if rs:
            print(f"| {label} | {side} | " + " | ".join(f(rs) for _, f in cols) + " |")

print("\nMemory and disk (median of the runs):\n")
print("| case | side | idle anon MiB | anon after load MiB | telemetry bytes / 1M events | device bytes written / 1M events |")
print("|---|---|---:|---:|---:|---:|")
for label in labels:
    for side in sides:
        rs = groups.get((label, side))
        if rs:
            m = lambda f: statistics.median([f(r) for r in rs])
            print(f"| {label} | {side} | {m(lambda r: r['idle_anon_mib']):.0f} | {m(lambda r: r['after_anon_mib']):.0f} | "
                  f"{m(lambda r: r['telemetry_bytes'] / r['events'] * 1e6) / 1048576:.0f} MiB | {m(lambda r: r['written_bytes'] / r['events'] * 1e6) / 1048576:.0f} MiB |")

gc = [r for r in runs if r.get("group_commit_txns")]
if gc:
    print("\nGroup commit (Rails counterfactual): batches per transaction\n")
    print("| case | run | batches | transactions | batches / txn |")
    print("|---|---|---:|---:|---:|")
    for r in gc:
        print(f"| {r['label']} | {r['run']} | {r['group_commit_batches']} | {r['group_commit_txns']} | {r['group_commit_batches'] / r['group_commit_txns']:.2f} |")

print("\nHost state before each run:\n")
print("| run | load avg (1/5/15) | IO pressure full avg10 % | failed / pending jobs |")
print("|---|---|---:|---|")
for r in runs:
    print(f"| {r['run']} | {r['load_before']} | {r['io_full_avg10_before']} | {r.get('failed_jobs')} / {r.get('pending_jobs')} |")

if os.path.exists(pages_path):
    pages = [json.loads(l) for l in open(pages_path) if l.strip()]
    print(f"\nDashboard pages (window=24h), tenant: {pages[0]['tenant']}, {pages[0]['tenant_bytes'] / 1048576:.0f} MiB\n")
    print("| page | Rails cold ms | Rust cold ms | Rails warm ms | Rust warm ms |")
    print("|---|---:|---:|---:|---:|")
    for name in pages[0]["pages"]:
        cell = {}
        for side in ["rails", "rust"]:
            ps = [p["pages"][name] for p in pages if p["side"] == side]
            cell[side] = (ms([p["cold_ms"] for p in ps], "{:.1f}"), ms([statistics.median(p["warm_ms"]) for p in ps], "{:.1f}"))
        print(f"| {name} | {cell['rails'][0]} | {cell['rust'][0]} | {cell['rails'][1]} | {cell['rust'][1]} |")
