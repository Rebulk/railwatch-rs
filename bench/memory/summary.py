"""Summarizes one bench/memory/run.sh run directory: per phase the anon RSS
(MiB) at the end and at peak, open .sqlite3 fds (end / peak), threads, and
the ingest + dashboard numbers.

  python3 -I summary.py RUN_DIR
"""
import json, re, sys

r = sys.argv[1]
rows = []
for line in open(f"{r}/samples.tsv"):
    f = line.rstrip("\n").split("\t")
    if len(f) < 8 or not f[2]:
        continue
    rows.append({"t": int(f[0]), "phase": f[1], "anon": int(f[2]) / 1024, "file": int(f[3]) / 1024, "thr": int(f[4]), "fds": int(f[5]), "sq": int(f[6]), "files": int(f[7])})
out = {}
for ph in ["idle0", "mixed", "idle1", "burst", "idle2"]:
    v = [x for x in rows if x["phase"] == ph]
    if not v:
        continue
    out[ph] = {
        "anon_end": round(v[-1]["anon"]), "anon_peak": round(max(x["anon"] for x in v)),
        "sqlite_fds_end": v[-1]["sq"], "sqlite_fds_peak": max(x["sq"] for x in v),
        "files_end": v[-1]["files"], "threads_end": v[-1]["thr"], "threads_peak": max(x["thr"] for x in v),
    }


def load(name):
    try:
        s = open(f"{r}/load-{name}.txt").read()
    except OSError:
        return None
    m = re.search(r"batches/s \(200\): ([\d.]+)\s+events/s accepted: (\d+)", s)
    st = re.search(r"^\s+200\s+(\d+)\s+([\d.]+)\s+([\d.]+)\s+([\d.]+)\s+([\d.]+)", s, re.M)
    other = sum(int(x) for x in re.findall(r"^\s+(?!200)\d{3}\s+(\d+)", s, re.M))
    return {"batches_s": float(m.group(1)) if m else None, "events_s": int(m.group(2)) if m else None,
            "lat_ms_p50_p90_p99_max": [float(x) for x in st.groups()[1:]] if st else None, "non200": other}


def dash(name):
    try:
        d = json.load(open(f"{r}/dash-{name}.json"))
    except (OSError, ValueError):
        return None
    return {k: d[k] for k in ("pages", "status", "p50", "p90", "p99")}


for ph in ("mixed", "burst"):
    if ph in out:
        out[ph]["ingest"] = load(ph)
        out[ph]["dash"] = dash(ph)
print(json.dumps(out, indent=1))
