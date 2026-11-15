"""Before/after table from bench/memory/run.sh run directories: per label
prefix (the part before the last '-N'), the min-max over its runs of each
phase's anon RSS and open .sqlite3 fds at the end, peak anon, and ingest /
dashboard numbers.

  python3 -I compare.py RUN_DIR...
"""
import collections, json, re, sys

runs = collections.defaultdict(list)
for d in sys.argv[1:]:
    label = re.sub(r"-\d+\.\w+$", "", d.rstrip("/").split("/")[-1])
    try:
        runs[label].append(json.load(open(f"{d}/summary.txt")))
    except (OSError, ValueError):
        pass


def get(f, s):
    """f(s), or None when a run is missing that phase or metric (null in summary.txt)."""
    try:
        return f(s)
    except (KeyError, TypeError, IndexError):
        return None


def rng(vals):
    vals = [v for v in vals if v is not None]
    if not vals:
        return "-"
    lo, hi = min(vals), max(vals)
    fmt = (lambda x: f"{x:g}") if any(isinstance(v, float) for v in vals) else str
    return fmt(lo) if lo == hi else f"{fmt(lo)}-{fmt(hi)}"


rows = [
    ("anon MiB, idle at start (end of idle0)", lambda s: s["idle0"]["anon_end"]),
    ("anon MiB, peak during mixed", lambda s: s["mixed"]["anon_peak"]),
    ("anon MiB, end of mixed (15 min)", lambda s: s["mixed"]["anon_end"]),
    ("anon MiB, idle after mixed", lambda s: s["idle1"]["anon_end"]),
    ("anon MiB, peak during burst", lambda s: s["burst"]["anon_peak"]),
    ("anon MiB, idle after burst", lambda s: s["idle2"]["anon_end"]),
    (".sqlite3 fds, end of mixed", lambda s: s["mixed"]["sqlite_fds_end"]),
    (".sqlite3 fds, idle after burst", lambda s: s["idle2"]["sqlite_fds_end"]),
    ("threads, idle after burst", lambda s: s["idle2"]["threads_end"]),
    ("mixed: ingest batches/s (open loop)", lambda s: s["mixed"]["ingest"]["batches_s"]),
    ("mixed: ingest p50 / p99 ms", lambda s: None),
    ("mixed: dashboard pages", lambda s: s["mixed"]["dash"]["pages"]),
    ("mixed: dashboard p50 / p90 ms", lambda s: None),
    ("burst: ingest batches/s (closed loop)", lambda s: s["burst"]["ingest"]["batches_s"]),
    ("burst: dashboard pages (12 clients)", lambda s: s["burst"]["dash"]["pages"]),
    ("burst: dashboard p50 / p90 ms", lambda s: None),
]
labels = sorted(runs)
print("| | " + " | ".join(f"{l} (n={len(runs[l])})" for l in labels) + " |")
print("|---|" + "---:|" * len(labels))
for name, f in rows:
    cells = []
    for l in labels:
        if "ingest p50" in name:
            a = rng([get(lambda s: s["mixed"]["ingest"]["lat_ms_p50_p90_p99_max"][0], s) for s in runs[l]])
            b = rng([get(lambda s: s["mixed"]["ingest"]["lat_ms_p50_p90_p99_max"][2], s) for s in runs[l]])
            cells.append(f"{a} / {b}")
        elif "mixed: dashboard p50" in name:
            cells.append(f"{rng([get(lambda s: s['mixed']['dash']['p50'], s) for s in runs[l]])} / {rng([get(lambda s: s['mixed']['dash']['p90'], s) for s in runs[l]])}")
        elif "burst: dashboard p50" in name:
            cells.append(f"{rng([get(lambda s: s['burst']['dash']['p50'], s) for s in runs[l]])} / {rng([get(lambda s: s['burst']['dash']['p90'], s) for s in runs[l]])}")
        else:
            cells.append(rng([get(f, s) for s in runs[l]]))
    print(f"| {name} | " + " | ".join(cells) + " |")
