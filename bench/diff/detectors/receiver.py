"""HTTPS capture webhook for the detector diff. Every POST is appended to
OUT as one JSON line {path, signature_ok, user_agent, idempotency_key, body}.
The last path segment decides the answer: "ok" -> 204, "failN" -> 500 for
the first N requests to that exact path, then 204.

  python3 -I receiver.py BIND PORT CERT KEY SECRET OUT
"""
import collections, hashlib, hmac, http.server, json, ssl, sys

bind, port, cert, key, secret, out = sys.argv[1], int(sys.argv[2]), sys.argv[3], sys.argv[4], sys.argv[5].encode(), sys.argv[6]
seen = collections.Counter()


class H(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        raw = self.rfile.read(int(self.headers.get("Content-Length", 0)))
        want = "sha256=" + hmac.new(secret, raw, hashlib.sha256).hexdigest()
        rec = {"path": self.path, "signature_ok": hmac.compare_digest(want, self.headers.get("X-Railwatch-Signature", "")),
               "user_agent": self.headers.get("User-Agent"), "content_type": self.headers.get("Content-Type"), "body": json.loads(raw)}
        seen[self.path] += 1
        last = self.path.rsplit("/", 1)[-1]
        status = 204
        if last.startswith("fail") and seen[self.path] <= int(last[4:]):
            status = 500
        rec["answered"] = status
        with open(out, "a") as f:
            f.write(json.dumps(rec) + "\n")
        self.send_response(status)
        self.send_header("Content-Length", "0")
        self.end_headers()

    def log_message(self, *a):
        pass


srv = http.server.ThreadingHTTPServer((bind, port), H)
ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
ctx.load_cert_chain(cert, key)
srv.socket = ctx.wrap_socket(srv.socket, server_side=True)
print(f"listening https://{bind}:{port}", flush=True)
srv.serve_forever()
