"""The SCALE.md result tables, from steps.jsonl (one table per server).

    python3 -I bench/scale/tables.py bench/results/scale/steps.jsonl

Prints markdown. Each section is the newest run of each (side, apps,
busy_every); steps that did not complete are absent from steps.jsonl, so a
missing row means it did not run (or did not finish), never a guessed value.
"""
import json
import sys

SIDES = [
    ("rust", "Rust, origin/main `ad5a091`"),
    ("rust-memory", "Rust, PR #90 `f4ad31b` (perf/memory)"),
    ("rust-mem-ingest", "Rust, PR #89 `ef4b907` merged onto #90 `f4ad31b` (bench-only build)"),
    ("rust-next", "Rust next (Track H)"),
    ("rails", "Rails, railwatch-cloud `71020d3` (the image production last ran)"),
    ("rails-best", "Rails best case (Track G)"),
]

rows = {}
for line in open(sys.argv[1]):
    r = json.loads(line)
    rows[(r["side"], r["busy_every"], r["apps"])] = r  # later runs replace earlier


def n(v, fmt="{:,.1f}"):
    return "-" if v is None else fmt.format(v)


def breaks(r):
    b = r["breaks"]
    if not b:
        return "-"
    words = {"p99>1s": "p99 > 1 s", "drops": "drops", "behind>60s": "behind > 60 s", "oom": "killed (out of memory)",
             "exited": "server exited", "failed_jobs": "failed jobs", "client_late": "client late (run invalid)"}
    out = []
    for x in b:
        # older rows (before the oom/exit split) carry "oom/exit": name it from oom_kills
        if x == "oom/exit":
            x = "oom" if r.get("oom_kills") else "exited"
        out.append(words.get(x, x))
    return ", ".join(out)


for side, title in SIDES:
    for busy in (0, 10):
        steps = sorted((r for (s, b, _), r in rows.items() if s == side and b == busy), key=lambda r: r["apps"])
        if not steps:
            continue
        rails = side.startswith("rails")
        print(f"### {title}{', 10% busy apps' if busy else ''}\n")
        head = ["apps"] + (["busy apps"] if busy else []) + ["p50 ms", "p99 ms", "not 200", "CPU % of 2 threads",
                "steady / peak private MiB", "RSS MiB", "fds"] + (["oldest ready job s"] if rails else []) + ["rest busy", "load max", "breaks"]
        print("| " + " | ".join(head) + " |")
        print("|" + "|".join("---:" if h not in ("breaks",) else "---" for h in head) + "|")
        for r in steps:
            cells = [str(r["apps"])] + ([str(r["busy_apps"])] if busy else []) + [
                n(r["p50_ms"]), n(r["p99_ms"]), f"{r['not_ok']:,}", n(r["cpu_pct_avg"]),
                f"{r['steady_anon_mib']:,} / {r['peak_anon_mib']:,}", f"{r['steady_rss_mib']:,}", f"{r['steady_fds']:,}"]
            if rails:
                cells.append(n(r["jobs_oldest_ready_max_s"], "{:,}"))
            cells += [f"{r['host_busy_pct_rest']:.1f}%", f"{r['loadavg_max']:.1f}", breaks(r)]
            print("| " + " | ".join(cells) + " |")
        print()
