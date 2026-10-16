#!/usr/bin/env python3
"""Summarise an inferno folded file.
  analyze.py FOLDED [--grep FRAME] [--callers FRAME] [--depth N]
Prints share by thread (first frame, digits stripped), top self frames,
top inclusive frames; with --callers, the Rust/C frames that lead into FRAME."""
import sys, re, collections, argparse
ap = argparse.ArgumentParser()
ap.add_argument("folded"); ap.add_argument("--callers"); ap.add_argument("--depth", type=int, default=6)
ap.add_argument("--top", type=int, default=30); ap.add_argument("--thread")
a = ap.parse_args()
rows = []
for line in open(a.folded):
    s, _, n = line.rstrip().rpartition(" ")
    fr = s.split(";")
    rows.append((fr, int(n)))
if a.thread: rows = [(f, n) for f, n in rows if re.sub(r"[-_]?\d+$", "", f[0]).startswith(a.thread)]
tot = sum(n for _, n in rows)
def short(f):
    f = re.sub(r"::h[0-9a-f]{16}$", "", f)
    f = re.sub(r"<(.*) as (.*)>", r"\2", f)
    return f[:110]
pct = lambda n: f"{100*n/tot:5.1f}%"
print(f"total samples {tot}")
th = collections.Counter()
for f, n in rows: th[re.sub(r"[-_]?\d+$", "", f[0])] += n
print("-- threads"); [print(pct(n), k) for k, n in th.most_common(12)]
if a.callers:
    c = collections.Counter(); sub = 0
    for f, n in rows:
        idx = [i for i, x in enumerate(f) if a.callers in x]
        if not idx: continue
        sub += n
        i = idx[0]
        chain = [short(x) for x in f[max(1, i - a.depth):i] if not x.startswith("[")]
        c[" <- ".join(reversed(chain[-a.depth:]))] += n
    print(f"-- callers of {a.callers}: {pct(sub)}")
    for k, n in c.most_common(a.top): print(pct(n), k)
    sys.exit()
selfc = collections.Counter(); incl = collections.Counter()
for f, n in rows:
    selfc[short(f[-1])] += n
    for x in set(short(x) for x in f): incl[x] += n
print("-- self"); [print(pct(n), k) for k, n in selfc.most_common(a.top)]
print("-- inclusive"); [print(pct(n), k) for k, n in incl.most_common(a.top + 20)]
