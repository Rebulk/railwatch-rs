#!/usr/bin/env python3
"""A stand-in Google for comparing Rails and Rust sign-in on the same flows.

Serves both shapes:
- Rust (RAILWATCH_GOOGLE_ISSUER=<base>): /authorize, /token, /certs, ID token
  with iss=<base>;
- Rails (omniauth-google-oauth2, client_options pointed here by rails_flows.rb):
  /authorize, /token, /userinfo, /tokeninfo (Rails reads the identity from
  userinfo).

POST /next sets what the next authorization produces:
  {"email", "sub", "email_verified", "claims": {...overrides, null deletes},
   "sign_with": "k1" | "attacker"}
Codes are single use. PKCE is checked when the request phase sent a
challenge (Rails' config does not). Test-only keys from
crates/server/tests/fixtures/oidc.
"""
import base64, hashlib, json, sys, threading, time, urllib.parse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from cryptography.hazmat.primitives import hashes, serialization
from cryptography.hazmat.primitives.asymmetric import padding

FIXTURES = Path(__file__).resolve().parents[3] / "crates/server/tests/fixtures/oidc"
KEYS = {name: serialization.load_der_private_key((FIXTURES / f"{name}.pk8").read_bytes(), None) for name in ["google-k1", "attacker"]}
CLIENT_ID = "railwatch-test.apps.googleusercontent.com"
CLIENT_SECRET = "test-secret"

lock = threading.Lock()
state = {"next": None, "grants": {}, "tokens": {}, "log": []}


def b64(b):
    return base64.urlsafe_b64encode(b).rstrip(b"=").decode()


def sign(kid, key, claims):
    head = b64(json.dumps({"alg": "RS256", "kid": kid, "typ": "JWT"}).encode())
    body = b64(json.dumps(claims).encode())
    sig = key.sign(f"{head}.{body}".encode(), padding.PKCS1v15(), hashes.SHA256())
    return f"{head}.{body}.{b64(sig)}"


def jwk(kid, key):
    n = key.public_key().public_numbers()
    to_b = lambda i: i.to_bytes((i.bit_length() + 7) // 8, "big")
    return {"kty": "RSA", "alg": "RS256", "use": "sig", "kid": kid, "n": b64(to_b(n.n)), "e": b64(to_b(n.e))}


class H(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def reply(self, code, body=b"", ctype="application/json", location=None):
        self.send_response(code)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        if location:
            self.send_header("Location", location)
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        u = urllib.parse.urlsplit(self.path)
        q = dict(urllib.parse.parse_qsl(u.query))
        base = f"http://{self.headers['Host']}"
        if u.path == "/authorize":
            with lock:
                nxt, state["next"] = state["next"], None
                state["log"].append({"authorize": q})
                if not nxt or q.get("client_id") != CLIENT_ID:
                    return self.reply(400, b"bad authorization request", "text/plain")
                code = f"code-{len(state['grants']) + 1}-{int(time.time() * 1000)}"
                state["grants"][code] = {"q": q, "next": nxt}
            loc = f"{q['redirect_uri']}?code={code}&state={urllib.parse.quote(q.get('state', ''))}"
            return self.reply(302, location=loc)
        if u.path == "/certs":
            return self.reply(200, json.dumps({"keys": [jwk("k1", KEYS["google-k1"])]}).encode())
        if u.path in ("/userinfo", "/oauth2/v3/userinfo"):
            token = (self.headers.get("Authorization") or "").removeprefix("Bearer ")
            with lock:
                info = state["tokens"].get(token)
            return self.reply(200, json.dumps(info).encode()) if info else self.reply(401, b'{"error":"invalid_token"}')
        if u.path == "/state":
            with lock:
                return self.reply(200, json.dumps({"log": state["log"][-20:]}).encode())
        return self.reply(404, b"not found", "text/plain")

    def do_POST(self):
        u = urllib.parse.urlsplit(self.path)
        raw = self.rfile.read(int(self.headers.get("Content-Length") or 0))
        base = f"http://{self.headers['Host']}"
        if u.path == "/next":
            with lock:
                state["next"] = json.loads(raw)
            return self.reply(204)
        if u.path == "/token":
            f = dict(urllib.parse.parse_qsl(raw.decode()))
            auth = self.headers.get("Authorization") or ""
            creds = base64.b64decode(auth[6:]).decode() if auth.startswith("Basic ") else ""
            client_ok = creds in (f"{CLIENT_ID}:{CLIENT_SECRET}", f"{urllib.parse.quote(CLIENT_ID)}:{urllib.parse.quote(CLIENT_SECRET)}") or (
                f.get("client_id") == CLIENT_ID and f.get("client_secret") == CLIENT_SECRET)
            with lock:
                g = state["grants"].pop(f.get("code"), None)
                q = g and g["q"]
                pkce_ok = g is not None and ("code_challenge" not in q or b64(hashlib.sha256(f.get("code_verifier", "").encode()).digest()) == q["code_challenge"])
                ok = g is not None and client_ok and pkce_ok and f.get("redirect_uri") == q["redirect_uri"]
                state["log"].append({"token": "ok" if ok else ("used/unknown code" if g is None else "refused")})
                if not ok:
                    return self.reply(400, json.dumps({"error": "invalid_grant", "error_description": "Bad Request"}).encode())
                n = g["next"]
                now = int(time.time())
                claims = {"iss": base, "aud": CLIENT_ID, "azp": CLIENT_ID, "sub": n["sub"], "email": n["email"],
                          "email_verified": n.get("email_verified", True), "name": "Ada Lovelace", "iat": now, "exp": now + 3600}
                if "nonce" in q:
                    claims["nonce"] = q["nonce"]
                for k, v in (n.get("claims") or {}).items():
                    if v is None:
                        claims.pop(k, None)
                    else:
                        claims[k] = v
                key = n.get("sign_with", "k1")
                id_token = sign("k1", KEYS["google-k1" if key == "k1" else "attacker"], claims)
                access = f"ya29.{b64(hashlib.sha256(id_token.encode()).digest())[:20]}"
                # Rails reads userinfo; it carries the same identity.
                state["tokens"][access] = {k: claims[k] for k in ("sub", "email", "email_verified", "name") if k in claims}
            return self.reply(200, json.dumps({"access_token": access, "token_type": "Bearer", "expires_in": 3599,
                                               "scope": "openid email profile", "id_token": id_token}).encode())
        if u.path == "/tokeninfo":
            f = dict(urllib.parse.parse_qsl(raw.decode()))
            with lock:
                known = f.get("access_token") in state["tokens"]
            body = {"aud": CLIENT_ID, "scope": "openid email profile"} if known else {"error_description": "Invalid Value"}
            return self.reply(200 if known else 400, json.dumps(body).encode())
        return self.reply(404, b"not found", "text/plain")


if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 8798
    ThreadingHTTPServer(("0.0.0.0", port), H).serve_forever()
