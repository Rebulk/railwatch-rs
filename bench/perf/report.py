#!/usr/bin/env python3
"""report.py RUNS_DIR LABEL [LABEL...]: per case, each label's cold runs
(the first call of a fresh process on evicted files, one per round, with
the MiB it read) and warm median (the median of each round's warm
medians), in ms, plus whether the outputs are identical across labels
(every cold and warm OUT.json; a case whose rounds disagree differs). A
label also takes in the runs of `LABEL-r*` (one run dir per round, to
interleave two binaries).
"""
import glob, json, os, statistics as st, sys

runs, labels = sys.argv[1], sys.argv[2:]

DIFFERS = "<differs between rounds>"


def load(label):
    dirs = [os.path.join(runs, label)] + sorted(glob.glob(os.path.join(runs, label + "-r*")))
    cold, warm, outs = {}, {}, {}
    for f in sorted(f for d in dirs for f in glob.glob(os.path.join(d, "cold-*.json.timings.json"))):
        for k, v in json.load(open(f)).items():
            cold.setdefault(k, []).append((v["first"], v.get("first_read_bytes", 0) / 1048576))
    for f in sorted(f for d in dirs for f in glob.glob(os.path.join(d, "warm-*.json.timings.json"))):
        for k, v in json.load(open(f)).items():
            warm.setdefault(k, []).append(st.median(v["warm"]))
    # Every measured output, cold and warm: each case's value, or DIFFERS
    # when two rounds of this label disagree.
    for f in sorted(f for d in dirs for p in ("warm-*.json", "cold-*.json") for f in glob.glob(os.path.join(d, p))):
        if f.endswith(".timings.json"):
            continue
        for k, v in json.load(open(f)).items():
            # page-replay signs in with a new session row per process.
            if isinstance(v.get("props"), dict) and isinstance(v["props"].get("auth"), dict):
                v["props"]["auth"].pop("session", None)
            if outs.get(k, v) != v:
                outs[k] = DIFFERS
            else:
                outs[k] = v
    return cold, warm, outs


data = {l: load(l) for l in labels}
names = list(data[labels[0]][1])
print("| case | " + " | ".join(f"{l} cold (runs) | {l} cold MiB read | {l} warm" for l in labels) + " |")
print("|---|" + "---:|---:|---:|" * len(labels))
for n in names:
    cells = []
    for l in labels:
        cold, warm, _ = data[l]
        runs_ = cold.get(n, [])
        cells += [" / ".join(f"{t:.0f}" for t, _ in runs_) or "-", f"{st.median(m for _, m in runs_):.1f}" if runs_ else "-",
                  f"{st.median(warm[n]):.1f}" if n in warm else "-"]
    print(f"| {n} | " + " | ".join(cells) + " |")
if len(labels) > 1:
    base = data[labels[0]][2]
    for l in labels[1:]:
        other = data[l][2]
        diff = [k for k in base if base.get(k) != other.get(k) or base.get(k) == DIFFERS]
        print(f"\n{labels[0]} vs {l}: {len(base) - len(diff)}/{len(base)} outputs identical" + (f"; differ: {', '.join(diff)}" if diff else ""))
