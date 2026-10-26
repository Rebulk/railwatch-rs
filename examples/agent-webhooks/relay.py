"""Railwatch webhook -> GitHub repository_dispatch, with the signature checked.

Railwatch signs each webhook; GitHub's dispatch API wants a token and its own
body shape. This relay sits between them: it verifies X-Railwatch-Signature-256
over "<timestamp>.<body>" (rejecting timestamps more than 5 minutes off),
drops repeats of an event id it has already relayed, and POSTs
{"event_type": "railwatch-<event>", "client_payload": {...}} to
/repos/OWNER/REPO/dispatches. Standard library only.

  RAILWATCH_WEBHOOK_SECRET=...   the integration's signing secret
  GITHUB_TOKEN=...               fine-grained, this repository, Contents: read and write
  GITHUB_REPOSITORY=owner/repo
  RELAY_EVENTS=new_issue,regressed_issue   (default: new_issue)
  PORT=8080
  python3 -I relay.py

Run it behind HTTPS (Railwatch only delivers to https:// URLs): a reverse
proxy, a tunnel, or a platform that terminates TLS. GITHUB_API_URL points it
at GitHub Enterprise (or a test double).
"""
import hashlib
import hmac
import http.server
import json
import os
import threading
import time
import urllib.request

SECRET = os.environ["RAILWATCH_WEBHOOK_SECRET"].encode()
TOKEN = os.environ["GITHUB_TOKEN"]
REPO = os.environ["GITHUB_REPOSITORY"]
API = os.environ.get("GITHUB_API_URL", "https://api.github.com").rstrip("/")
EVENTS = set(os.environ.get("RELAY_EVENTS", "new_issue").split(","))
TOLERANCE = 300
MAX_BODY = 1 << 20
seen = {}  # event id -> when relayed; a delivery retried after a timeout is not dispatched twice
LOCK = threading.Lock()  # ThreadingHTTPServer: one thread per request share `seen`


def verify(headers, raw):
    """True when the body is signed with SECRET within the last 5 minutes."""
    ts = headers.get("X-Railwatch-Timestamp", "")
    got = headers.get("X-Railwatch-Signature-256", "")
    if not ts.isdigit() or abs(time.time() - int(ts)) > TOLERANCE:
        return False
    want = "sha256=" + hmac.new(SECRET, ts.encode() + b"." + raw, hashlib.sha256).hexdigest()
    return hmac.compare_digest(want, got)


def dispatch(body):
    issue = body.get("issue") or {}
    payload = {
        "event_id": body.get("alert_id"),
        "event": body.get("event"),
        "issue_key": issue.get("canonical_key") or body.get("issue_key"),
        "title": issue.get("title"),
        "url": body.get("url"),
        "mcp": body.get("mcp"),
        # Under 64 KB: Railwatch caps the summary at 16 KB.
        "railwatch": body,
    }
    req = urllib.request.Request(
        f"{API}/repos/{REPO}/dispatches",
        data=json.dumps({"event_type": f"railwatch-{body.get('event')}", "client_payload": payload}).encode(),
        method="POST",
        headers={
            "Authorization": f"Bearer {TOKEN}",
            "Accept": "application/vnd.github+json",
            "X-GitHub-Api-Version": "2022-11-28",
            "Content-Type": "application/json",
        },
    )
    with urllib.request.urlopen(req, timeout=10) as res:
        return res.status


class Relay(http.server.BaseHTTPRequestHandler):
    # Reached before the signature is checked, by anyone: a slow or silent
    # client must not hold a thread.
    timeout = 15

    def do_POST(self):
        raw_length = self.headers.get("Content-Length") or "0"
        if not (raw_length.isascii() and raw_length.isdigit()):
            return self.answer(400)
        length = int(raw_length)
        if length > MAX_BODY:
            return self.answer(413)
        raw = self.rfile.read(length)
        if not verify(self.headers, raw):
            return self.answer(401)
        body = json.loads(raw)
        event_id = self.headers.get("X-Railwatch-Event-Id") or body.get("alert_id")
        if body.get("event") not in EVENTS or body.get("payload", {}).get("test"):
            return self.answer(204)  # not wanted here; 2xx so Railwatch does not retry
        # Reserve the id before dispatching, so two concurrent deliveries of
        # one event cannot both dispatch; release it if the dispatch fails.
        with LOCK:
            now = time.time()
            for k in [k for k, at in seen.items() if now - at > 86400]:
                del seen[k]
            if event_id in seen:
                return self.answer(204)
            seen[event_id] = now
        try:
            dispatch(body)
        except Exception as e:  # GitHub down or the token refused: let Railwatch retry
            with LOCK:
                seen.pop(event_id, None)
            self.log_error("dispatch failed: %s", e)
            return self.answer(502)
        self.answer(204)

    def answer(self, status):
        self.send_response(status)
        self.send_header("Content-Length", "0")
        self.end_headers()


if __name__ == "__main__":
    port = int(os.environ.get("PORT", "8080"))
    print(f"relaying Railwatch {sorted(EVENTS)} to {REPO} on :{port}", flush=True)
    http.server.ThreadingHTTPServer(("0.0.0.0", port), Relay).serve_forever()
