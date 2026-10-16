#!/usr/bin/env python3
"""Posts every batch in a manifest, in order, to one server and records the
(status, JSON ack) of each. Volatile ack fields (committed_at) are reduced to
their presence so two servers' acks can be compared.

    post.py URL TOKEN BATCH_DIR OUT.json [xff]
"""
import http.client, json, os, sys, urllib.parse

url, token, batch_dir, out = sys.argv[1:5]
xff = sys.argv[5] if len(sys.argv) > 5 else None
u = urllib.parse.urlparse(url)
manifest = json.load(open(os.path.join(batch_dir, "manifest.json")))
conn = http.client.HTTPConnection(u.hostname, u.port, timeout=60)
results = []
for i, m in enumerate(manifest):
    body = open(os.path.join(batch_dir, m["file"]), "rb").read()
    headers = {"Authorization": f"Bearer {token}", "Content-Type": "application/x-ndjson", "Content-Encoding": "gzip",
               "User-Agent": "railwatch-ruby/0.9.0", **m["headers"]}
    if xff:
        headers["X-Forwarded-For"] = xff
    conn.request("POST", u.path or "/ingest", body=body, headers=headers)
    r = conn.getresponse()
    raw = r.read()
    try:
        ack = json.loads(raw)
    except ValueError:
        ack = {"_raw": raw[:200].decode("utf-8", "replace")}
    if isinstance(ack, dict) and "committed_at" in ack:
        ack["committed_at"] = "<present>" if ack["committed_at"] else None
    results.append({"post": i, "file": m["file"], "replay": m.get("replay", False), "status": r.status, "ack": ack,
                    "retry_after": r.getheader("Retry-After")})
json.dump(results, open(out, "w"), indent=1, ensure_ascii=False)
print(f"{len(results)} posts -> {out}: " + ", ".join(f"{s}x{sum(1 for x in results if x['status']==s)}" for s in sorted({x['status'] for x in results})))
