"""A tiny HTTPS webhook receiver for alert-delivery proofs. Verifies
X-Railwatch-Signature (sha256=<hex HMAC-SHA256(secret, raw body)>) and appends
one JSON line per request to OUT: {received_at, path, signature_ok, body}.

  python3 -I receiver.py PORT CERT KEY SECRET OUT
"""
import hashlib, hmac, http.server, json, ssl, sys, time

port, cert, key, secret, out = int(sys.argv[1]), sys.argv[2], sys.argv[3], sys.argv[4].encode(), sys.argv[5]


class H(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        raw = self.rfile.read(int(self.headers.get("Content-Length", 0)))
        want = "sha256=" + hmac.new(secret, raw, hashlib.sha256).hexdigest()
        got = self.headers.get("X-Railwatch-Signature", "")
        rec = {"received_at": time.time(), "path": self.path, "user_agent": self.headers.get("User-Agent"),
               "signature_ok": hmac.compare_digest(want, got), "body": json.loads(raw)}
        with open(out, "a") as f:
            f.write(json.dumps(rec) + "\n")
        self.send_response(204)
        self.end_headers()

    def log_message(self, *a):
        pass


srv = http.server.HTTPServer(("127.0.0.1", port), H)
ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
ctx.load_cert_chain(cert, key)
srv.socket = ctx.wrap_socket(srv.socket, server_side=True)
print(f"listening https://127.0.0.1:{port}", flush=True)
srv.serve_forever()
